use crate::{
    record::CastReader,
    storage::{self, Metadata},
    textlog::{Format, Reader},
    transcript::Transcript,
};
use anyhow::{bail, Context, Result};
use std::{
    collections::VecDeque,
    fs::{self, File},
    io::{BufReader, BufWriter, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

fn metadata(path: &Path) -> Option<Metadata> {
    let file = path.join("metadata.json");
    let result: Result<Metadata> = (|| {
        let metadata: Metadata = serde_json::from_slice(&fs::read(&file)?)?;
        anyhow::ensure!(
            metadata.session_id == storage::session_id(path)?,
            "metadata session ID differs from directory"
        );
        Ok(metadata)
    })();
    match result {
        Ok(metadata) => Some(metadata),
        Err(e) => {
            eprintln!(
                "WARNING: cannot read {}: {e:#}; recording state is unknown",
                file.display()
            );
            None
        }
    }
}
struct Session {
    path: PathBuf,
    id: String,
    metadata: Option<Metadata>,
    started_at: Option<chrono::DateTime<chrono::FixedOffset>>,
}
impl Session {
    fn open(path: &Path) -> Result<Self> {
        let id = storage::session_id(path)?.to_owned();
        let metadata = metadata(path);
        let started_at = metadata.as_ref().map(|m| m.started_at).or_else(|| {
            CastReader::open(path)
                .ok()
                .map(|cast| cast.header.termlog.started_at)
        });
        Ok(Self {
            path: path.into(),
            id,
            metadata,
            started_at,
        })
    }
    fn complete(&self) -> bool {
        self.metadata
            .as_ref()
            .is_some_and(|m| m.transcript_complete)
    }
}
fn sessions(root: &Path) -> Result<Vec<Session>> {
    let mut sessions = storage::sessions(root)?
        .into_iter()
        .map(|path| Session::open(&path))
        .collect::<Result<Vec<_>>>()?;
    sessions.sort_by(|a, b| {
        b.started_at
            .cmp(&a.started_at)
            .then_with(|| a.id.cmp(&b.id))
    });
    Ok(sessions)
}
fn warn_incomplete(id: &str) {
    eprintln!("WARNING: transcript is incomplete or unverified for session {id}\nRun: termlog rebuild {id}");
}
pub fn list(root: &Path) -> Result<()> {
    for session in sessions(root)? {
        let Some(m) = session.metadata else {
            println!(
                "{}  {}  unknown  unknown  recording=unknown transcript=unknown",
                session
                    .started_at
                    .map(|time| time.format("%Y-%m-%d %H:%M:%S").to_string())
                    .unwrap_or_else(|| "unknown".into()),
                session.id
            );
            continue;
        };
        let state = |complete| if complete { "complete" } else { "incomplete" };
        println!(
            "{}  {}  {}s  {}  recording={} transcript={}",
            m.started_at.format("%Y-%m-%d %H:%M:%S"),
            m.session_id,
            m.duration_ms.unwrap_or(0) / 1000,
            m.shell,
            state(m.recording_complete),
            state(m.transcript_complete)
        );
    }
    Ok(())
}
fn summary(out: &mut impl Write, session: &Session) -> Result<()> {
    let Some(m) = &session.metadata else {
        writeln!(
            out,
            "{} · {}\nInput: unknown · Exit: unknown · recording=unknown transcript=unknown\n",
            session
                .started_at
                .map(|time| time.to_rfc3339())
                .unwrap_or_else(|| "unknown start time".into()),
            session.id
        )?;
        return Ok(());
    };
    let shell = Path::new(&m.shell)
        .file_name()
        .unwrap_or_default()
        .to_string_lossy();
    let state = |complete| if complete { "complete" } else { "incomplete" };
    let exit = m
        .exit_code
        .map(|code| code.to_string())
        .unwrap_or_else(|| "unknown".into());
    writeln!(
        out,
        "{} · {} · {}",
        shell.escape_debug(),
        m.started_at.format("%Y-%m-%d %H:%M:%S %:z"),
        m.session_id.escape_debug()
    )?;
    writeln!(
        out,
        "Input: {} · Exit: {exit} · recording={} transcript={}\n",
        if m.capture_input {
            "captured"
        } else {
            "not captured"
        },
        state(m.recording_complete),
        state(m.transcript_complete)
    )?;
    Ok(())
}
pub fn show(path: &Path) -> Result<()> {
    let session = Session::open(path)?;
    if !session.complete() {
        warn_incomplete(&session.id);
    }
    let file = File::open(path.join("transcript.log"))
        .context("transcript is unavailable; use rebuild")?;
    let mut reader = Reader::new(BufReader::new(file));
    let mut out = std::io::stdout().lock();
    summary(&mut out, &session)?;
    let mut format = Format::default();
    while let Some((_, line)) = reader.next()? {
        out.write_all(format.line(&line).as_bytes())?;
    }
    Ok(())
}
pub fn search(root: &Path, pattern: &str, context: usize, fixed: bool, plain: bool) -> Result<u32> {
    let regex = regex::Regex::new(&if fixed {
        regex::escape(pattern)
    } else {
        pattern.into()
    })?;
    anyhow::ensure!(context <= 10000, "context exceeds 10000 lines");
    let mut found = false;
    let mut incomplete = false;
    let mut out = std::io::stdout().lock();
    let sessions = match sessions(root) {
        Ok(sessions) => sessions,
        Err(e) => {
            eprintln!("ERROR: search completeness cannot be determined: {e:#}");
            return Ok(2);
        }
    };
    for session in sessions {
        if !session.complete() {
            incomplete = true;
            warn_incomplete(&session.id);
        }
        let source = File::open(session.path.join("transcript.log"))
            .map(|file| Reader::new(BufReader::new(file)));
        let mut reader = match source {
            Ok(reader) => reader,
            Err(e) => {
                incomplete = true;
                eprintln!("WARNING: cannot read transcript for {}: {e}", session.id);
                if session.complete() {
                    warn_incomplete(&session.id);
                }
                continue;
            }
        };
        let mut before = VecDeque::new();
        let mut remaining = 0;
        let mut last = None;
        let mut format = Format::default();
        let mut emit = |index, line: &crate::textlog::Line| -> Result<()> {
            if last.is_some_and(|n| index <= n) {
                return Ok(());
            }
            if plain {
                let text = format!(
                    "{}\t{}\n",
                    line.time
                        .to_rfc3339_opts(chrono::SecondsFormat::Millis, false),
                    line.text
                );
                write!(out, "{}:{index}:{text}", session.id)?;
            } else {
                if last.is_none() {
                    summary(&mut out, &session)?;
                }
                if last.is_some_and(|n| index > n + 1) {
                    writeln!(out, "...")?;
                }
                out.write_all(format.line(line).as_bytes())?;
            }
            last = Some(index);
            Ok(())
        };
        loop {
            let (index, line) = match reader.next() {
                Ok(Some(line)) => line,
                Ok(None) => break,
                Err(e) => {
                    incomplete = true;
                    eprintln!("WARNING: cannot read transcript for {}: {e}", session.id);
                    warn_incomplete(&session.id);
                    break;
                }
            };
            let hit = regex.is_match(&line.text);
            if hit {
                found = true;
                for (i, line) in &before {
                    emit(*i, line)?;
                }
                remaining = context;
            }
            if hit || remaining > 0 {
                emit(index, &line)?;
                if !hit {
                    remaining -= 1;
                }
            }
            if context > 0 {
                before.push_back((index, line));
                if before.len() > context {
                    before.pop_front();
                }
            }
        }
        if last.is_some() && !plain {
            writeln!(out)?;
        }
    }
    Ok(if incomplete {
        2
    } else if found {
        0
    } else {
        1
    })
}
pub fn rebuild(path: &Path) -> Result<()> {
    let id = storage::session_id(path)?;
    if crate::platform::session_running(id)? {
        bail!("cannot rebuild a running session")
    }
    let mut metadata = metadata(path);
    let mut cast = CastReader::open(path)?;
    let mut parser = Transcript::new(
        cast.header.term.rows,
        cast.header.term.cols,
        cast.header.termlog.started_at,
    );
    let mut format = Format::default();
    let target = path.join("transcript.log");
    let temp = path.join(format!("transcript.tmp-{}", uuid::Uuid::new_v4()));
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut file = BufWriter::new(storage::new_file(&temp)?);
        while let Some((time, e)) = cast.next()? {
            let lines = e.transcript(&mut parser, time)?;
            for line in lines {
                file.write_all(format.line(&line).as_bytes())?;
            }
        }
        for line in parser.finish() {
            file.write_all(format.line(&line).as_bytes())?;
        }
        file.flush()?;
        drop(file);
        fs::rename(&temp, &target)?;
        Ok(cast.complete())
    }))
    .unwrap_or_else(|_| Err(anyhow::anyhow!("transcript parser panicked")));
    let error = match &result {
        Ok(true) => None,
        Ok(false) => Some("cast is truncated or has no exit event".into()),
        Err(e) => Some(format!("{e:#}")),
    };
    if let Some(error) = &error {
        eprintln!("WARNING: transcript incomplete: {error}");
    }
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    if let Some(metadata) = &mut metadata {
        metadata.transcript_complete = matches!(result, Ok(true));
        metadata.transcript_error = error;
        if matches!(result, Ok(false)) {
            metadata.recording_complete = false;
        }
        storage::write_metadata(path, metadata)?;
    } else if result.is_ok() {
        eprintln!("WARNING: transcript rebuilt for {id}; metadata is still unavailable and recording state remains unknown");
    }
    result.map(|_| ())
}

pub fn replay(path: &Path) -> Result<()> {
    let mut cast = CastReader::open(path)?;
    let _raw = crate::platform::RawTerminal::enter()?;
    let signals = crate::platform::Signals::new(&[libc::SIGINT, libc::SIGTERM, libc::SIGHUP])?;
    let mut out = crate::replay::Restore(std::io::stdout());
    let mut filter = crate::replay::Filter::default();
    let start = Instant::now();
    while let Some((micros, e)) = cast.next()? {
        let due = Duration::from_micros(micros);
        while start.elapsed() < due {
            if crate::platform::replay_interrupted(&signals)? {
                return Ok(());
            }
            std::thread::sleep(
                due.saturating_sub(start.elapsed())
                    .min(Duration::from_millis(20)),
            );
        }
        if crate::platform::replay_interrupted(&signals)? {
            return Ok(());
        }
        if e.1 == "o" {
            out.0.write_all(&filter.feed(&e.2))?;
        }
        out.0.flush()?;
    }
    Ok(())
}
