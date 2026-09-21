use crate::{record::CastReader, storage, transcript::Transcript};
use anyhow::{Context, Result};
use std::{
    io::{BufWriter, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::Receiver,
        Arc,
    },
    thread::{self, JoinHandle},
};

pub struct Worker {
    handle: JoinHandle<Result<bool>>,
    pub failed: Arc<AtomicBool>,
}
impl Worker {
    pub fn start(path: &Path, updates: Receiver<()>) -> Self {
        let target = path.join("transcript.log");
        Self::spawn(path.to_owned(), updates, move || storage::new_file(&target))
    }
    fn spawn<W: Write + Send + 'static>(
        path: PathBuf,
        updates: Receiver<()>,
        output: impl FnOnce() -> Result<W> + Send + 'static,
    ) -> Self {
        let failed = Arc::new(AtomicBool::new(false));
        let flag = failed.clone();
        let handle = thread::spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                generate(&path, output()?, updates)
            }))
            .unwrap_or_else(|_| Err(anyhow::anyhow!("transcript worker panicked")));
            if !matches!(result, Ok(true)) {
                flag.store(true, Ordering::Release);
            }
            result
        });
        Self { handle, failed }
    }
    pub fn finish(self) -> Result<bool> {
        self.handle
            .join()
            .map_err(|_| anyhow::anyhow!("transcript worker panicked"))?
    }
}

// The cast file is the spool: there is no transcript queue to block the writer.
fn generate(path: &Path, output: impl Write, updates: Receiver<()>) -> Result<bool> {
    let mut cast = CastReader::open(path)?;
    let mut parser = Transcript::new(
        cast.header.term.rows,
        cast.header.term.cols,
        cast.header.termlog.started_at,
    );
    let mut output = BufWriter::new(output);
    let mut format = crate::textlog::Format::new(cast.header.termlog.transcript_version);
    loop {
        // Sender belongs only to CastWriter. Disconnect means its final flush
        // (including BufWriter's drop on failure) has finished. Drain once more.
        let finished = updates.recv().is_err();
        while let Some((time, event)) = cast.next()? {
            for line in event.transcript(&mut parser, time)? {
                output
                    .write_all(format.line(&line).as_bytes())
                    .context("writing transcript")?;
            }
        }
        output.flush().context("flushing transcript")?;
        if finished {
            break;
        }
    }
    for line in parser.finish() {
        output.write_all(format.line(&line).as_bytes())?;
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
        mpsc::{self, Receiver, Sender, SyncSender},
    };
    use std::time::{Duration, Instant};

    fn recorder() -> (
        tempfile::TempDir,
        SyncSender<Event>,
        Receiver<Result<()>>,
        Receiver<()>,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let header = serde_json::from_value(serde_json::json!({
            "version":3,"term":{"cols":80,"rows":24,"type":"xterm"},
            "timestamp":0,"command":"test","env":{},
            "termlog":{"started_at":"1970-01-01T00:00:00Z","transcript_version":1}
        }))
        .unwrap();
        let (notify, updates) = mpsc::sync_channel(1);
        let writer = Writer::new(
            storage::new_file(&dir.path().join("events.cast")).unwrap(),
            &header,
            Arc::new(AtomicU64::new(0)),
            notify,
        )
        .unwrap();
        let (tx, rx) = mpsc::sync_channel(8);
        let (finished, result) = mpsc::channel();
        thread::spawn(move || {
            let _ = finished.send(writer.run(rx, 0));
        });
        (dir, tx, result, updates)
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
        let (dir, tx, done, updates) = recorder();
        let worker = Worker::spawn(dir.path().to_owned(), updates, || Ok(std::io::sink()));
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
        let (dir, tx, done, updates) = recorder();
        let worker = Worker::spawn(dir.path().to_owned(), updates, || Ok(PanickingOutput));
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
        let (dir, tx, done, updates) = recorder();
        let (entered, waiting) = mpsc::channel();
        let (resume, paused) = mpsc::channel();
        let worker = Worker::spawn(dir.path().to_owned(), updates, || {
            Ok(PausedOutput {
                entered,
                resume: paused,
            })
        });
        event(&tx, 'o', "text\r\n");
        waiting.recv_timeout(Duration::from_secs(5)).unwrap();
        finish(tx, done, dir.path());
        assert!(!worker.handle.is_finished());
        // The blocked batch and final batch each require an output write.
        resume.send(()).unwrap();
        waiting.recv_timeout(Duration::from_secs(5)).unwrap();
        resume.send(()).unwrap();
        assert!(worker.finish().unwrap());
    }
    struct FlushObserver(Sender<()>);
    impl Write for FlushObserver {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            self.0.send(()).unwrap();
            Ok(())
        }
    }
    #[test]
    fn idle_worker_waits_for_cast_notification() {
        let (dir, tx, done, updates) = recorder();
        let (flushed, observed) = mpsc::channel();
        let worker = Worker::spawn(dir.path().to_owned(), updates, || {
            Ok(FlushObserver(flushed))
        });
        event(&tx, 'o', "ready\r\n");
        observed.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(matches!(
            observed.recv_timeout(Duration::from_millis(120)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        finish(tx, done, dir.path());
        assert!(worker.finish().unwrap());
    }
    #[test]
    fn coalesced_notifications_drain_final_cast_data() {
        let (dir, tx, done, updates) = recorder();
        for _ in 0..1000 {
            event(&tx, 'o', "line\r\n");
        }
        finish(tx, done, dir.path());
        // All flushes fit in a single pending wake. Even after consuming it,
        // disconnect must cause a final drain before the worker exits.
        assert_eq!(updates.try_recv(), Ok(()));
        assert_eq!(updates.try_recv(), Err(mpsc::TryRecvError::Disconnected));
        let worker = Worker::start(dir.path(), updates);
        assert!(worker.finish().unwrap());
        let text = std::fs::read_to_string(dir.path().join("transcript.log")).unwrap();
        assert_eq!(text.lines().count(), 1001);
        assert!(text.ends_with("tail\n"));
    }
}
