use crate::{
    record::CastReader,
    storage::{self, Metadata},
    transcript::Transcript,
};
use anyhow::{bail, Context, Result};
use std::{
    collections::VecDeque,
    fs::{self, File},
    io::{BufRead, BufReader, BufWriter, Write},
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
pub fn show(path: &Path) -> Result<()> {
    let m = metadata(path)?;
    if !m.transcript_complete {
        warn_incomplete(&m.session_id);
    }
    std::io::copy(
        &mut File::open(path.join("transcript.log"))
            .context("transcript is unavailable; use rebuild")?,
        &mut std::io::stdout(),
    )?;
    Ok(())
}
pub fn search(root: &Path, pattern: &str, context: usize, fixed: bool) -> Result<u32> {
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
    for (path, metadata) in sessions {
        if !metadata.transcript_complete {
            incomplete = true;
            warn_incomplete(&metadata.session_id);
        }
        let file = match File::open(path.join("transcript.log")) {
            Ok(f) => f,
            Err(e) => {
                incomplete = true;
                eprintln!(
                    "WARNING: cannot read transcript for {}: {e}",
                    metadata.session_id
                );
                if metadata.transcript_complete {
                    warn_incomplete(&metadata.session_id);
                }
                continue;
            }
        };
        let id = path.file_name().unwrap().to_string_lossy();
        let mut before = VecDeque::new();
        let mut remaining = 0;
        let mut last = None;
        for (index, line) in BufReader::new(file).lines().enumerate() {
            let line = match line {
                Ok(line) => line,
                Err(e) => {
                    incomplete = true;
                    eprintln!("WARNING: cannot read transcript for {id}: {e}");
                    warn_incomplete(&id);
                    break;
                }
            };
            let hit = regex.is_match(&line);
            if hit {
                found = true;
                for (i, s) in &before {
                    if last.is_none_or(|n| *i > n) {
                        writeln!(out, "{id}:{}:{s}", i + 1)?;
                        last = Some(*i);
                    }
                }
                remaining = context;
            }
            if hit || remaining > 0 {
                if last.is_none_or(|n| index > n) {
                    writeln!(out, "{id}:{}:{line}", index + 1)?;
                    last = Some(index);
                }
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
    let target = path.join("transcript.log");
    let temp = path.join(format!("transcript.tmp-{}", uuid::Uuid::new_v4()));
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut file = BufWriter::new(storage::new_file(&temp)?);
        while let Some((time, e)) = cast.next()? {
            let lines = e.transcript(&mut parser, time)?;
            for line in lines {
                file.write_all(line.as_bytes())?;
            }
        }
        for line in parser.finish() {
            file.write_all(line.as_bytes())?;
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
