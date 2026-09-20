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
    path::Path,
    time::{Duration, Instant},
};

pub fn list(root: &Path) -> Result<()> {
    let mut sessions = Vec::new();
    for path in storage::sessions(root)? {
        match fs::read(path.join("metadata.json"))
            .map_err(anyhow::Error::from)
            .and_then(|bytes| Ok(serde_json::from_slice::<Metadata>(&bytes)?))
        {
            Ok(metadata) => sessions.push(metadata),
            Err(e) => eprintln!("WARNING: cannot read {}: {e}", path.display()),
        }
    }
    sessions.sort_by(|a, b| {
        b.started_at
            .cmp(&a.started_at)
            .then_with(|| a.session_id.cmp(&b.session_id))
    });
    for m in sessions {
        println!(
            "{}  {}  {}s  {}  {}",
            m.started_at.format("%Y-%m-%d %H:%M:%S"),
            m.session_id,
            m.duration_ms.unwrap_or(0) / 1000,
            m.shell,
            if m.recording_complete {
                "complete"
            } else {
                "incomplete"
            }
        );
    }
    Ok(())
}
pub fn show(path: &Path) -> Result<()> {
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
    let mut out = std::io::stdout().lock();
    for path in storage::sessions(root)? {
        let file = match File::open(path.join("transcript.log")) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        };
        let id = path.file_name().unwrap().to_string_lossy();
        let mut before = VecDeque::new();
        let mut remaining = 0;
        let mut last = None;
        for (index, line) in BufReader::new(file).lines().enumerate() {
            let line = line?;
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
    Ok(if found { 0 } else { 1 })
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
    let mut out = std::io::stdout();
    let mut filter = crate::replay::Filter::default();
    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            let _ = std::io::stdout().write_all(b"\x1b[0m\x1b[?25h\x1b[?1049l");
        }
    }
    let _restore = Restore;
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
            out.write_all(&filter.feed(&e.2))?;
        }
        out.flush()?;
    }
    Ok(())
}
