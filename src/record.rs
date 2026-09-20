use crate::{storage::Header, transcript::Transcript};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io::{BufRead, BufReader, BufWriter, Write},
    path::Path,
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc::{Receiver, RecvTimeoutError},
        Arc,
    },
    time::{Duration, Instant},
};

#[derive(Debug)]
pub struct Event {
    pub micros: u64,
    pub kind: char,
    pub data: Vec<u8>,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct CastEvent(pub f64, pub String, pub String);
impl CastEvent {
    pub fn transcript(&self, parser: &mut Transcript, time: u64) -> Result<Vec<String>> {
        Ok(match self.1.as_str() {
            "o" => parser.feed(&self.2, time),
            "r" => {
                let (cols, rows) = parse_size(&self.2)?;
                parser.resize(rows, cols)
            }
            "x" => parser.finish(),
            _ => Vec::new(),
        })
    }
}

#[derive(Default)]
struct Decoder {
    pending: Vec<u8>,
}
impl Decoder {
    fn feed(&mut self, data: &[u8], finish: bool) -> (String, u64) {
        self.pending.extend_from_slice(data);
        let mut result = String::new();
        let mut replaced = 0;
        let mut consumed = 0;
        while consumed < self.pending.len() {
            match std::str::from_utf8(&self.pending[consumed..]) {
                Ok(s) => {
                    result.push_str(s);
                    consumed = self.pending.len();
                }
                Err(e) => {
                    let end = consumed + e.valid_up_to();
                    result.push_str(std::str::from_utf8(&self.pending[consumed..end]).unwrap());
                    consumed = end;
                    match e.error_len() {
                        Some(n) => {
                            result.push('\u{fffd}');
                            replaced += 1;
                            consumed += n;
                        }
                        None if finish => {
                            result.push('\u{fffd}');
                            replaced += 1;
                            consumed = self.pending.len();
                        }
                        None => break,
                    }
                }
            }
        }
        self.pending.drain(..consumed);
        (result, replaced)
    }
}
pub struct Writer {
    cast: BufWriter<File>,
    last: u64,
    output: Decoder,
    input: Decoder,
    replacements: Arc<AtomicU64>,
}
impl Writer {
    pub fn new(cast: File, header: &Header, replacements: Arc<AtomicU64>) -> Result<Self> {
        let mut cast = BufWriter::new(cast);
        serde_json::to_writer(&mut cast, header)?;
        cast.write_all(b"\n")?;
        cast.flush()?;
        Ok(Self {
            cast,
            last: 0,
            output: Decoder::default(),
            input: Decoder::default(),
            replacements,
        })
    }
    fn text_event(&mut self, time: u64, kind: char, data: String) -> Result<()> {
        let time = time.max(self.last);
        let event = CastEvent(
            (time - self.last) as f64 / 1_000_000.0,
            kind.to_string(),
            data,
        );
        serde_json::to_writer(&mut self.cast, &event)?;
        self.cast.write_all(b"\n")?;
        self.last = time;
        Ok(())
    }

    fn event(&mut self, event: Event) -> Result<()> {
        match event.kind {
            'o' | 'i' => {
                let d = if event.kind == 'o' {
                    &mut self.output
                } else {
                    &mut self.input
                };
                let (text, n) = d.feed(&event.data, false);
                self.replacements.fetch_add(n, Ordering::Relaxed);
                if !text.is_empty() {
                    self.text_event(event.micros, event.kind, text)?;
                }
            }
            'x' => {
                for kind in ['o', 'i'] {
                    let d = if kind == 'o' {
                        &mut self.output
                    } else {
                        &mut self.input
                    };
                    let (text, n) = d.feed(&[], true);
                    self.replacements.fetch_add(n, Ordering::Relaxed);
                    if !text.is_empty() {
                        self.text_event(event.micros, kind, text)?;
                    }
                }
                self.text_event(event.micros, 'x', String::from_utf8(event.data)?)?;
            }
            _ => self.text_event(event.micros, event.kind, String::from_utf8(event.data)?)?,
        }
        Ok(())
    }
    fn flush(&mut self) -> Result<()> {
        self.cast.flush()?;
        Ok(())
    }
    pub fn run(mut self, rx: Receiver<Event>, interval: u64) -> Result<()> {
        let duration = Duration::from_millis(interval.max(1));
        let mut deadline = Instant::now() + duration;
        loop {
            match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(e) => {
                    let force = interval == 0 || matches!(e.kind, 'm' | 'x');
                    self.event(e)?;
                    if !force && Instant::now() < deadline {
                        continue;
                    }
                }
                Err(RecvTimeoutError::Timeout) => (),
                Err(RecvTimeoutError::Disconnected) => return self.flush(),
            }
            self.flush()?;
            deadline = Instant::now() + duration;
        }
    }
}

pub fn parse_size(s: &str) -> Result<(u16, u16)> {
    let (a, b) = s
        .split_once('x')
        .ok_or_else(|| anyhow::anyhow!("invalid resize"))?;
    let (cols, rows) = (a.parse()?, b.parse()?);
    anyhow::ensure!(cols > 0 && rows > 0, "empty terminal size");
    Ok((cols, rows))
}
pub struct CastReader {
    reader: BufReader<File>,
    pub header: Header,
    pending: Vec<u8>,
    time: u64,
    exited: bool,
}
impl CastReader {
    pub fn open(path: &Path) -> Result<Self> {
        let mut reader = BufReader::new(File::open(path.join("events.cast"))?);
        let mut line = String::new();
        reader.read_line(&mut line)?;
        let header: Header = serde_json::from_str(&line).context("invalid cast header")?;
        anyhow::ensure!(header.version == 3, "expected asciicast v3");
        anyhow::ensure!(
            header.termlog.transcript_version == crate::transcript::VERSION,
            "unsupported transcript version"
        );
        anyhow::ensure!(
            header.term.rows > 0 && header.term.cols > 0,
            "invalid terminal dimensions"
        );
        Ok(Self {
            reader,
            header,
            pending: Vec::new(),
            time: 0,
            exited: false,
        })
    }
    // At a temporary EOF, keep a partial line until the writer appends its rest.
    pub fn next(&mut self) -> Result<Option<(u64, CastEvent)>> {
        loop {
            if self.reader.read_until(b'\n', &mut self.pending)? == 0
                || !self.pending.ends_with(b"\n")
            {
                return Ok(None);
            }
            let line = std::mem::take(&mut self.pending);
            if line.starts_with(b"#") {
                continue;
            }
            anyhow::ensure!(!self.exited, "cast contains an event after exit");
            let event: CastEvent = serde_json::from_slice(&line).context("invalid cast event")?;
            anyhow::ensure!(
                event.0.is_finite() && event.0 >= 0.0 && event.0 < 1e10,
                "invalid cast interval"
            );
            self.time = self
                .time
                .checked_add((event.0 * 1_000_000.0).round() as u64)
                .context("cast timing overflow")?;
            self.exited = event.1 == "x";
            return Ok(Some((self.time, event)));
        }
    }
    pub fn complete(&self) -> bool {
        self.exited && self.pending.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn decode_split_invalid_and_unfinished_utf8() {
        let mut decoder = Decoder::default();
        assert_eq!(decoder.feed(&[0xe6, 0x97], false), (String::new(), 0));
        assert_eq!(decoder.feed(&[0xa5], false), ("日".into(), 0));
        assert_eq!(decoder.feed(&[255], false), ("�".into(), 1));
        assert_eq!(decoder.feed(&[0xe6], false), (String::new(), 0));
        assert_eq!(decoder.feed(&[], true), ("�".into(), 1));
    }
}
