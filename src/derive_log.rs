use crate::{record::CastReader, storage, transcript::Transcript};
use anyhow::{Context, Result};
use std::{
    io::{BufWriter, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
        Arc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

pub struct Worker {
    done: Sender<()>,
    handle: JoinHandle<Result<bool>>,
    pub failed: Arc<AtomicBool>,
}
impl Worker {
    pub fn start(path: &Path, interval: u64) -> Self {
        let target = path.join("transcript.log");
        Self::spawn(path.to_owned(), interval, move || {
            storage::new_file(&target)
        })
    }
    fn spawn<W: Write + Send + 'static>(
        path: PathBuf,
        interval: u64,
        output: impl FnOnce() -> Result<W> + Send + 'static,
    ) -> Self {
        let (done, rx) = mpsc::channel();
        let failed = Arc::new(AtomicBool::new(false));
        let flag = failed.clone();
        let handle = thread::spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                generate(&path, output()?, interval, rx)
            }))
            .unwrap_or_else(|_| Err(anyhow::anyhow!("transcript worker panicked")));
            if !matches!(result, Ok(true)) {
                flag.store(true, Ordering::Release);
            }
            result
        });
        Self {
            done,
            handle,
            failed,
        }
    }
    pub fn finish(self) -> Result<bool> {
        drop(self.done);
        self.handle
            .join()
            .map_err(|_| anyhow::anyhow!("transcript worker panicked"))?
    }
}

// The cast file is the spool: there is no transcript queue to block the writer.
fn generate(path: &Path, output: impl Write, interval: u64, done: Receiver<()>) -> Result<bool> {
    let mut cast = CastReader::open(path)?;
    let mut parser = Transcript::new(
        cast.header.term.rows,
        cast.header.term.cols,
        cast.header.termlog.started_at,
    );
    let mut output = BufWriter::new(output);
    let mut last_flush = Instant::now();
    let mut finished = false;
    loop {
        while let Some((time, event)) = cast.next()? {
            for line in event.transcript(&mut parser, time)? {
                output
                    .write_all(line.as_bytes())
                    .context("writing transcript")?;
            }
            if interval == 0
                || matches!(event.1.as_str(), "m" | "x")
                || last_flush.elapsed() >= Duration::from_millis(interval)
            {
                output.flush().context("flushing transcript")?;
                last_flush = Instant::now();
            }
        }
        output.flush().context("flushing transcript")?;
        if finished {
            break;
        }
        if !matches!(
            done.recv_timeout(Duration::from_millis(20)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ) {
            // Re-read after observing completion, since the last flush can race EOF.
            finished = true;
        }
    }
    for line in parser.finish() {
        output.write_all(line.as_bytes())?;
    }
    output.flush()?;
    Ok(cast.complete())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::{Event, Writer};
    use std::sync::{
        atomic::AtomicU64,
        mpsc::{Receiver, SyncSender},
    };

    fn recorder() -> (tempfile::TempDir, SyncSender<Event>, Receiver<Result<()>>) {
        let dir = tempfile::tempdir().unwrap();
        let header = serde_json::from_value(serde_json::json!({
            "version":3,"term":{"cols":80,"rows":24,"type":"xterm"},
            "timestamp":0,"command":"test","env":{},
            "termlog":{"started_at":"1970-01-01T00:00:00Z","transcript_version":1}
        }))
        .unwrap();
        let writer = Writer::new(
            storage::new_file(&dir.path().join("events.cast")).unwrap(),
            &header,
            Arc::new(AtomicU64::new(0)),
        )
        .unwrap();
        let (tx, rx) = mpsc::sync_channel(8);
        let (finished, result) = mpsc::channel();
        thread::spawn(move || {
            let _ = finished.send(writer.run(rx, 0));
        });
        (dir, tx, result)
    }
    fn event(tx: &SyncSender<Event>, kind: char, data: &str) {
        tx.send(Event {
            micros: 1,
            kind,
            data: data.as_bytes().to_vec(),
        })
        .unwrap();
    }
    fn finish(tx: SyncSender<Event>, done: Receiver<Result<()>>, path: &Path) {
        event(&tx, 'o', "tail\r\n");
        event(&tx, 'x', "0");
        drop(tx);
        done.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
        let mut reader = CastReader::open(path).unwrap();
        let mut found_tail = false;
        while let Some((_, event)) = reader.next().unwrap() {
            found_tail |= event.2 == "tail\r\n";
        }
        assert!(found_tail && reader.complete());
    }
    fn wait_failure(worker: &Worker) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !worker.failed.load(Ordering::Acquire) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
        assert!(worker.failed.load(Ordering::Acquire));
    }
    #[test]
    fn parser_error_does_not_stop_cast() {
        let (dir, tx, done) = recorder();
        let worker = Worker::spawn(dir.path().to_owned(), 0, || Ok(std::io::sink()));
        event(&tx, 'r', "invalid-size");
        wait_failure(&worker);
        finish(tx, done, dir.path());
        assert!(worker
            .finish()
            .unwrap_err()
            .to_string()
            .contains("invalid resize"));
    }
    struct PanickingOutput;
    impl Write for PanickingOutput {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            panic!("injected transcript panic")
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    #[test]
    fn transcript_panic_does_not_stop_cast() {
        let (dir, tx, done) = recorder();
        let worker = Worker::spawn(dir.path().to_owned(), 0, || Ok(PanickingOutput));
        event(&tx, 'o', "text\r\n");
        wait_failure(&worker);
        finish(tx, done, dir.path());
        assert!(worker
            .finish()
            .unwrap_err()
            .to_string()
            .contains("panicked"));
    }
    struct PausedOutput {
        entered: Sender<()>,
        resume: Receiver<()>,
    }
    impl Write for PausedOutput {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.entered.send(()).unwrap();
            self.resume.recv().unwrap();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    #[test]
    fn blocked_transcript_does_not_backpressure_cast() {
        let (dir, tx, done) = recorder();
        let (entered, waiting) = mpsc::channel();
        let (resume, paused) = mpsc::channel();
        let worker = Worker::spawn(dir.path().to_owned(), 0, || {
            Ok(PausedOutput {
                entered,
                resume: paused,
            })
        });
        event(&tx, 'o', "text\r\n");
        waiting.recv_timeout(Duration::from_secs(5)).unwrap();
        finish(tx, done, dir.path());
        assert!(!worker.handle.is_finished());
        // Two logical lines require two output writes with interval=0.
        resume.send(()).unwrap();
        waiting.recv_timeout(Duration::from_secs(5)).unwrap();
        resume.send(()).unwrap();
        assert!(worker.finish().unwrap());
    }
}
