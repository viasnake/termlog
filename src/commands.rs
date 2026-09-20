use crate::{
    record::{parse_size, CastEvent},
    storage::{self, Header, Metadata},
    transcript::{self, Transcript},
};
use anyhow::{bail, Context, Result};
use std::{
    collections::VecDeque,
    fs::{self, File},
    io::{BufRead, BufReader, BufWriter, Read, Write},
    path::Path,
    time::{Duration, Instant},
};

pub fn list(root: &Path) -> Result<()> {
    for path in storage::sessions(root)? {
        match fs::read(path.join("metadata.json"))
            .map_err(anyhow::Error::from)
            .and_then(|b| Ok(serde_json::from_slice::<Metadata>(&b)?))
        {
            Ok(m) => println!(
                "{}  {}  {}s  {}  {}",
                m.started_at.format("%Y-%m-%d %H:%M:%S"),
                m.session_id,
                m.duration_ms.unwrap_or(0) / 1000,
                m.shell,
                if m.complete { "complete" } else { "incomplete" }
            ),
            Err(e) => eprintln!("WARNING: cannot read {}: {e}", path.display()),
        }
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
struct CastReader {
    reader: BufReader<File>,
    header: Header,
    time: u64,
    line: usize,
}
impl CastReader {
    fn open(path: &Path) -> Result<Self> {
        let mut reader = BufReader::new(File::open(path.join("events.cast"))?);
        let mut line = String::new();
        reader.read_line(&mut line)?;
        let header: Header = serde_json::from_str(&line).context("invalid cast header")?;
        anyhow::ensure!(header.version == 3, "expected asciicast v3");
        anyhow::ensure!(
            header.termlog.transcript_version == transcript::VERSION,
            "unsupported transcript version"
        );
        anyhow::ensure!(
            header.term.rows > 0 && header.term.cols > 0,
            "invalid terminal dimensions"
        );
        Ok(Self {
            reader,
            header,
            time: 0,
            line: 1,
        })
    }
    fn next(&mut self) -> Result<Option<(u64, CastEvent)>> {
        loop {
            let mut line = String::new();
            if self.reader.read_line(&mut line)? == 0 {
                return Ok(None);
            }
            self.line += 1;
            if line.starts_with('#') {
                continue;
            }
            let parsed = serde_json::from_str::<CastEvent>(&line);
            let e = match parsed {
                Ok(e) => e,
                Err(_) if !line.ends_with('\n') => {
                    eprintln!("WARNING: ignoring truncated final cast line {}", self.line);
                    return Ok(None);
                }
                Err(e) => {
                    return Err(e).with_context(|| format!("invalid cast line {}", self.line))
                }
            };
            anyhow::ensure!(
                e.0.is_finite() && e.0 >= 0.0 && e.0 < 1e10,
                "invalid cast interval on line {}",
                self.line
            );
            self.time = self
                .time
                .checked_add((e.0 * 1_000_000.0).round() as u64)
                .context("cast timing overflow")?;
            return Ok(Some((self.time, e)));
        }
    }
}
pub fn rebuild(path: &Path) -> Result<()> {
    let id = path.file_name().unwrap().to_string_lossy();
    if crate::platform::session_running(&id)? {
        bail!("cannot rebuild a running session")
    }
    let mut cast = CastReader::open(path)?;
    let mut parser = Transcript::new(
        cast.header.term.rows,
        cast.header.term.cols,
        cast.header.termlog.started_at,
    );
    let target = path.join("transcript.log");
    let temp = path.join(format!("transcript.tmp-{}", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut file = BufWriter::new(storage::new_file(&temp)?);
        let mut ended = false;
        while let Some((time, e)) = cast.next()? {
            ended |= e.1 == "x";
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
        if !ended {
            eprintln!("WARNING: rebuilt an incomplete recording (no exit event)");
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
}
pub fn replay(path: &Path) -> Result<()> {
    let mut cast = CastReader::open(path)?;
    let _raw = crate::platform::RawTerminal::enter()?;
    let signals = crate::platform::Signals::new(&[libc::SIGINT, libc::SIGTERM, libc::SIGHUP])?;
    let mut out = std::io::stdout();
    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            let _ = std::io::stdout().write_all(b"\x1b[0m\x1b[?25h\x1b[?1049l");
        }
    }
    let _restore = Restore;
    write!(
        out,
        "\x1b[8;{};{}t",
        cast.header.term.rows, cast.header.term.cols
    )?;
    out.flush()?;
    let start = Instant::now();
    while let Some((micros, e)) = cast.next()? {
        let due = Duration::from_micros(micros);
        while start.elapsed() < due {
            if replay_interrupted(&signals)? {
                return Ok(());
            }
            std::thread::sleep(
                due.saturating_sub(start.elapsed())
                    .min(Duration::from_millis(20)),
            );
        }
        if replay_interrupted(&signals)? {
            return Ok(());
        }
        match e.1.as_str() {
            "o" => out.write_all(e.2.as_bytes())?,
            "r" => {
                let (cols, rows) = parse_size(&e.2)?;
                write!(out, "\x1b[8;{rows};{cols}t")?;
            }
            _ => (),
        }
        out.flush()?;
    }
    Ok(())
}
fn replay_interrupted(signals: &crate::platform::Signals) -> Result<bool> {
    if signals.take().is_some() {
        return Ok(true);
    }
    let mut p = libc::pollfd {
        fd: 0,
        events: libc::POLLIN,
        revents: 0,
    };
    if unsafe { libc::poll(&mut p, 1, 0) } > 0 && p.revents & libc::POLLIN != 0 {
        let mut b = [0u8; 64];
        let n = std::io::stdin().read(&mut b)?;
        if b[..n].iter().any(|b| matches!(b, 3 | b'q')) {
            return Ok(true);
        }
    }
    Ok(false)
}
