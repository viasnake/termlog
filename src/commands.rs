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

fn metadata(path: &Path) -> Result<Metadata> {
    let file = path.join("metadata.json");
    serde_json::from_slice(&fs::read(&file).with_context(|| format!("reading {}", file.display()))?)
        .with_context(|| format!("parsing {}", file.display()))
}
fn sessions_with_metadata(root: &Path) -> Result<Vec<(PathBuf, Metadata)>> {
    let mut sessions = storage::sessions(root)?
        .into_iter()
        .map(|path| Ok((path.clone(), metadata(&path)?)))
        .collect::<Result<Vec<_>>>()?;
    sessions.sort_by(|(_, a), (_, b)| {
        b.started_at
            .cmp(&a.started_at)
            .then_with(|| a.session_id.cmp(&b.session_id))
    });
    Ok(sessions)
}
fn warn_incomplete(id: &str) {
    eprintln!("WARNING: transcript is incomplete for session {id}\nRun: termlog rebuild {id}");
}
pub fn list(root: &Path) -> Result<()> {
    for (_, m) in sessions_with_metadata(root)? {
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
fn summary(out: &mut impl Write, m: &Metadata) -> Result<()> {
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
    let m = metadata(path)?;
    if !m.transcript_complete {
        warn_incomplete(&m.session_id);
    }
    let file = File::open(path.join("transcript.log"))
        .context("transcript is unavailable; use rebuild")?;
    let mut reader = Reader::new(BufReader::new(file), m.transcript_version)?;
    let mut out = std::io::stdout().lock();
    summary(&mut out, &m)?;
    let mut format = Format::new(2);
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
    let sessions = match sessions_with_metadata(root) {
        Ok(sessions) => sessions,
        Err(e) => {
            eprintln!("ERROR: search completeness cannot be determined: {e:#}");
            return Ok(2);
        }
    };
    for (path, m) in sessions {
        if !m.transcript_complete {
            incomplete = true;
            warn_incomplete(&m.session_id);
        }
        let source = File::open(path.join("transcript.log"))
            .map_err(anyhow::Error::from)
            .and_then(|file| Reader::new(BufReader::new(file), m.transcript_version));
        let mut reader = match source {
            Ok(reader) => reader,
            Err(e) => {
                incomplete = true;
                eprintln!("WARNING: cannot read transcript for {}: {e}", m.session_id);
                if m.transcript_complete {
                    warn_incomplete(&m.session_id);
                }
                continue;
            }
        };
        let mut before = VecDeque::new();
        let mut remaining = 0;
        let mut last = None;
        let mut format = Format::new(2);
        let mut emit = |index, line: &crate::textlog::Line| -> Result<()> {
            if last.is_some_and(|n| index <= n) {
                return Ok(());
            }
            if plain {
                let text = Format::new(1).line(line);
                write!(out, "{}:{index}:{text}", m.session_id)?;
            } else {
                if last.is_none() {
                    summary(&mut out, &m)?;
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
                    eprintln!("WARNING: cannot read transcript for {}: {e}", m.session_id);
                    warn_incomplete(&m.session_id);
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
    let id = path.file_name().unwrap().to_string_lossy();
    if crate::platform::session_running(&id)? {
        bail!("cannot rebuild a running session")
    }
    let mut metadata: Metadata = serde_json::from_slice(&fs::read(path.join("metadata.json"))?)?;
    let mut cast = CastReader::open(path)?;
    let mut parser = Transcript::new(
        cast.header.term.rows,
        cast.header.term.cols,
        cast.header.termlog.started_at,
    );
    let mut format = crate::textlog::Format::new(cast.header.termlog.transcript_version);
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
    metadata.transcript_complete = matches!(result, Ok(true));
    metadata.transcript_error = match &result {
        Ok(true) => None,
        Ok(false) => Some("cast is truncated or has no exit event".into()),
        Err(e) => Some(format!("{e:#}")),
    };
    if matches!(result, Ok(false)) {
        metadata.recording_complete = false;
    }
    if let Some(error) = &metadata.transcript_error {
        eprintln!("WARNING: transcript incomplete: {error}");
    }
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    storage::write_metadata(path, &metadata)?;
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
