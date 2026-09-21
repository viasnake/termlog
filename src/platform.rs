use crate::{
    config::Config,
    derive_log,
    record::{Event, Writer},
    storage::{self, Extension, Header, Metadata, Term},
    transcript,
};
use anyhow::{bail, Context, Result};
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use serde::{Deserialize, Serialize};
use std::{
    env, fs,
    io::{Read, Write},
    os::unix::{
        fs::PermissionsExt,
        net::{UnixListener, UnixStream},
        process::CommandExt,
    },
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, SyncSender},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

pub struct RawTerminal {
    original: libc::termios,
}
impl RawTerminal {
    pub fn enter() -> Result<Self> {
        unsafe {
            if libc::isatty(0) != 1 || libc::isatty(1) != 1 {
                bail!("recording requires a terminal on stdin and stdout")
            }
            let mut original = std::mem::zeroed();
            if libc::tcgetattr(0, &mut original) != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            let mut raw = original;
            libc::cfmakeraw(&mut raw);
            if libc::tcsetattr(0, libc::TCSANOW, &raw) != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            Ok(Self { original })
        }
    }
}
impl Drop for RawTerminal {
    fn drop(&mut self) {
        unsafe {
            libc::tcsetattr(0, libc::TCSANOW, &self.original);
        }
    }
}
pub fn size() -> PtySize {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    unsafe {
        libc::ioctl(0, libc::TIOCGWINSZ, &mut ws);
    }
    PtySize {
        rows: ws.ws_row.max(1),
        cols: ws.ws_col.max(1),
        pixel_width: ws.ws_xpixel,
        pixel_height: ws.ws_ypixel,
    }
}
fn socket_path(id: &str) -> Result<PathBuf> {
    uuid::Uuid::parse_str(id).context("invalid TERMLOG_SESSION_ID")?;
    let dir = PathBuf::from("/tmp").join(format!("termlog-{}", unsafe { libc::geteuid() }));
    storage::private_dir(&dir)?;
    Ok(dir.join(format!("{id}.sock")))
}
#[derive(Serialize, Deserialize)]
pub struct Status {
    pub session: String,
    pub active: bool,
    pub input: bool,
    pub path: PathBuf,
}
#[derive(Serialize, Deserialize)]
struct Request {
    session: String,
    marker: Option<String>,
}
struct SocketGuard(PathBuf);
impl Drop for SocketGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}
pub fn session_running(id: &str) -> Result<bool> {
    match UnixStream::connect(socket_path(id)?) {
        Ok(_) => Ok(true),
        Err(e)
            if matches!(
                e.raw_os_error(),
                Some(libc::ENOENT) | Some(libc::ECONNREFUSED)
            ) =>
        {
            Ok(false)
        }
        Err(e) => Err(e).context("cannot determine whether recorder is active"),
    }
}
pub fn request(marker: Option<String>) -> Result<Status> {
    let id = env::var("TERMLOG_SESSION_ID").context("this terminal is not being recorded")?;
    request_id(&id, marker)
}
fn request_id(id: &str, marker: Option<String>) -> Result<Status> {
    let mut stream = UnixStream::connect(socket_path(id)?).context("recorder is not reachable")?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    let mut msg = serde_json::to_vec(&Request {
        session: id.to_owned(),
        marker,
    })?;
    anyhow::ensure!(msg.len() <= 8192, "marker is too long");
    msg.push(b'\n');
    stream.write_all(&msg)?;
    stream.shutdown(std::net::Shutdown::Write)?;
    let mut bytes = Vec::new();
    stream.take(16384).read_to_end(&mut bytes)?;
    let status: Status = serde_json::from_slice(&bytes)?;
    anyhow::ensure!(status.session == id, "recorder session mismatch");
    Ok(status)
}
pub struct Signals(Vec<(signal_hook::SigId, i32, Arc<AtomicBool>)>);
impl Signals {
    pub fn new(numbers: &[i32]) -> Result<Self> {
        let mut signals = Self(Vec::new());
        for &number in numbers {
            let flag = Arc::new(AtomicBool::new(false));
            let id = signal_hook::flag::register(number, flag.clone())?;
            signals.0.push((id, number, flag));
        }
        Ok(signals)
    }
    pub fn take(&self) -> Option<i32> {
        self.0
            .iter()
            .find_map(|(_, number, flag)| flag.swap(false, Ordering::Relaxed).then_some(*number))
    }
}
impl Drop for Signals {
    fn drop(&mut self) {
        for (id, _, _) in &self.0 {
            signal_hook::low_level::unregister(*id);
        }
    }
}
pub fn status() -> Result<u32> {
    match request(None) {
        Ok(s) => {
            println!(
                "Recording: {}\nSession:   {}\nInput:     {}\nPath:      {}",
                if s.active { "active" } else { "failed" },
                s.session,
                if s.input { "enabled" } else { "disabled" },
                s.path.display()
            );
            Ok(u32::from(!s.active))
        }
        Err(e) => {
            println!("Recording: inactive\nReason:    {e:#}");
            Ok(1)
        }
    }
}
fn hostname() -> String {
    let mut bytes = [0u8; 256];
    unsafe {
        libc::gethostname(bytes.as_mut_ptr().cast(), bytes.len());
    }
    String::from_utf8_lossy(&bytes[..bytes.iter().position(|v| *v == 0).unwrap_or(bytes.len())])
        .into_owned()
}
pub fn run(config: &Config, command: Vec<String>, capture: bool) -> Result<u32> {
    anyhow::ensure!(!command.is_empty(), "missing command");
    if env::var("TERMLOG_ACTIVE").as_deref() == Ok("1") {
        let status = request(None).context("inherited recorder cannot be verified")?;
        anyhow::ensure!(status.active, "outer recorder has failed");
        let error = std::process::Command::new(&command[0])
            .args(&command[1..])
            .exec();
        return Err(error.into());
    }
    let (notify, updates) = mpsc::sync_channel(1);
    let transcript_version = if config.transcript.timestamps == "rfc3339" {
        1
    } else {
        transcript::VERSION
    };
    let prepared = (|| {
        unsafe {
            if libc::isatty(0) != 1 || libc::isatty(1) != 1 {
                bail!("recording requires terminal stdin/stdout")
            }
        }
        let (id, path, started, start) = storage::create_session(&config.state_dir()?)?;
        let socket = socket_path(&id)?;
        let listener = UnixListener::bind(&socket)?;
        let guard = SocketGuard(socket.clone());
        fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;
        let term = env::var("TERM").unwrap_or_else(|_| "xterm-256color".into());
        let sz = size();
        let meta = Metadata {
            schema_version: 2,
            session_id: id.clone(),
            started_at: started,
            ended_at: None,
            duration_ms: None,
            platform: env::consts::OS.into(),
            arch: env::consts::ARCH.into(),
            hostname: hostname(),
            shell: command[0].clone(),
            initial_cwd: env::current_dir()?,
            terminal: term.clone(),
            capture_input: capture,
            exit_code: None,
            exit_signal: None,
            recording_complete: false,
            transcript_complete: false,
            transcript_error: None,
            recording_error: None,
            utf8_replacements: 0,
            transcript_version,
        };
        storage::write_metadata(&path, &meta)?;
        let mut environment = std::collections::BTreeMap::new();
        if let Ok(s) = env::var("SHELL") {
            environment.insert("SHELL".into(), s);
        }
        let header = Header {
            version: 3,
            term: Term {
                cols: sz.cols,
                rows: sz.rows,
                kind: term,
            },
            timestamp: started.timestamp(),
            command: command.join(" "),
            env: environment,
            termlog: Extension {
                started_at: started,
                transcript_version,
            },
        };
        let replacements = Arc::new(AtomicU64::new(0));
        let writer = Writer::new(
            storage::new_file(&path.join("events.cast"))?,
            &header,
            replacements.clone(),
            notify,
        )?;
        Ok((
            id,
            path,
            meta,
            writer,
            replacements,
            listener,
            guard,
            sz,
            start,
        ))
    })();
    let (id, path, mut meta, writer, replacements, listener, _socket_guard, sz, start) =
        match prepared {
            Ok(p) => p,
            Err(e) if !config.recording_required => {
                eprintln!(
                    "WARNING: recording could not be started: {e:#}. Starting WITHOUT recording."
                );
                let err = std::process::Command::new(&command[0])
                    .args(&command[1..])
                    .exec();
                return Err(err.into());
            }
            Err(e) => {
                return Err(e)
                    .context("terminal recording could not be started; shell was not launched")
            }
        };
    let pty = native_pty_system().openpty(sz)?;
    let mut reader = pty.master.try_clone_reader()?;
    let mut input = pty.master.take_writer()?;
    let fd = pty
        .master
        .as_raw_fd()
        .context("PTY backend has no polling descriptor")?;
    let mut cmd = CommandBuilder::new(&command[0]);
    cmd.args(&command[1..]);
    cmd.env("TERMLOG_ACTIVE", "1");
    cmd.env("TERMLOG_SESSION_ID", &id);
    let signals = Signals::new(&[libc::SIGWINCH, libc::SIGTERM, libc::SIGHUP])?;
    let raw = RawTerminal::enter()?;
    let mut child = pty.slave.spawn_command(cmd)?;
    drop(pty.slave);
    let pid = child.process_id();
    let failed = Arc::new(AtomicBool::new(false));
    let flag = failed.clone();
    // 256 x 16 KiB chunks bounds queued terminal data to about 4 MiB.
    let (tx, rx) = mpsc::sync_channel(256);
    let interval = config.flush_interval_ms;
    let worker = thread::spawn(move || {
        let result =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| writer.run(rx, interval)))
                .unwrap_or_else(|_| Err(anyhow::anyhow!("recording writer panicked")));
        if result.is_err() {
            flag.store(true, Ordering::Release);
        }
        result
    });
    let derived = config
        .transcript
        .enabled
        .then(|| derive_log::Worker::start(&path, updates));
    let mut transcript_warning = false;
    let mut sender = Some(tx);
    let mut warning = false;
    let mut encoding_warning = false;
    let mut output = std::io::stdout();
    let mut buffer = [0u8; 16384];
    let mut status = None;
    let mut exited_at = None;
    let mut eof = false;
    let relay: Result<()> = (|| {
        loop {
            if derived
                .as_ref()
                .is_some_and(|d| d.failed.load(Ordering::Acquire))
                && !transcript_warning
            {
                transcript_warning = true;
                output.write_all(b"\r\nWARNING: transcript generation failed; cast recording continues. Use termlog rebuild after exit.\r\n")?;
                output.flush()?;
            }
            if failed.load(Ordering::Acquire) && !warning {
                warning = true;
                sender.take();
                output.write_all(b"\r\nWARNING: terminal recording has failed.\r\nThis session is no longer being fully recorded.\r\n")?;
                output.flush()?;
            }
            if replacements.load(Ordering::Relaxed) > 0 && !encoding_warning {
                encoding_warning = true;
                output
                    .write_all(b"\r\nWARNING: invalid UTF-8 was replaced in the recording.\r\n")?;
                output.flush()?;
            }
            while let Some(signal) = signals.take() {
                if signal == libc::SIGWINCH {
                    let s = size();
                    pty.master.resize(s)?;
                    send(
                        &mut sender,
                        start,
                        'r',
                        format!("{}x{}", s.cols, s.rows).into_bytes(),
                        &failed,
                    );
                } else if let Some(pid) = pid {
                    unsafe {
                        libc::kill(-(pid as i32), signal);
                    }
                }
            }
            for _ in 0..8 {
                match listener.accept() {
                    Ok((mut conn, _)) => {
                        // A liveness probe may close before we configure the socket.
                        // Failure of this client must not terminate recording.
                        let timeout = Some(Duration::from_millis(20));
                        if conn
                            .set_read_timeout(timeout)
                            .and_then(|()| conn.set_write_timeout(timeout))
                            .is_err()
                        {
                            continue;
                        }
                        let mut bytes = vec![];
                        if (&mut conn).take(8194).read_to_end(&mut bytes).is_ok()
                            && bytes.len() <= 8193
                        {
                            if let Ok(req) = serde_json::from_slice::<Request>(&bytes) {
                                if req.session == id {
                                    if let Some(label) = req.marker {
                                        send(&mut sender, start, 'm', label.into_bytes(), &failed);
                                    }
                                    let reply = Status {
                                        session: id.clone(),
                                        active: !failed.load(Ordering::Acquire),
                                        input: capture,
                                        path: path.clone(),
                                    };
                                    let _ = serde_json::to_writer(&mut conn, &reply);
                                }
                            }
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(e) => return Err(e.into()),
                }
            }
            if status.is_none() {
                if let Some(s) = child.try_wait()? {
                    status = Some(s);
                    exited_at = Some(Instant::now());
                }
            }
            if eof && status.is_some() {
                break;
            }
            if exited_at.is_some_and(|t: Instant| t.elapsed() > Duration::from_secs(2)) {
                bail!("PTY remained open after child exit; recording ended before descendant EOF")
            }
            let mut polls = [
                libc::pollfd {
                    fd: if eof { -1 } else { fd },
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: if eof { -1 } else { 0 },
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            let n = unsafe { libc::poll(polls.as_mut_ptr(), 2, 50) };
            if n < 0 {
                let e = std::io::Error::last_os_error();
                if e.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e.into());
            }
            if polls[0].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
                match reader.read(&mut buffer) {
                    Ok(0) => eof = true,
                    Ok(n) => {
                        send(&mut sender, start, 'o', buffer[..n].to_vec(), &failed);
                        output.write_all(&buffer[..n])?;
                        output.flush()?;
                        if status.is_some() {
                            exited_at = Some(Instant::now());
                        }
                    }
                    Err(e) if e.raw_os_error() == Some(libc::EIO) => eof = true,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => (),
                    Err(e) => return Err(e.into()),
                }
            }
            if polls[1].revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
                bail!("parent terminal disconnected")
            }
            if polls[1].revents & libc::POLLIN != 0 && !eof {
                let n = std::io::stdin().read(&mut buffer)?;
                if n > 0 {
                    if capture {
                        send(&mut sender, start, 'i', buffer[..n].to_vec(), &failed);
                    }
                    input.write_all(&buffer[..n])?;
                    input.flush()?;
                }
            }
        }
        Ok(())
    })();
    if relay.is_err() {
        let _ = child.kill();
    }
    let status = status.or_else(|| child.wait().ok());
    meta.exit_signal = status.as_ref().and_then(|s| s.signal().map(str::to_owned));
    let exit = status.as_ref().map(exit_code).unwrap_or(1);
    send(
        &mut sender,
        start,
        'x',
        exit.to_string().into_bytes(),
        &failed,
    );
    drop(sender);
    let result = worker
        .join()
        .map_err(|_| anyhow::anyhow!("recording writer panicked"))
        .and_then(|r| r);
    drop(raw);
    let duration = start.elapsed();
    if let Some(derived) = derived {
        match derived.finish() {
            Ok(true) => meta.transcript_complete = true,
            Ok(false) => meta.transcript_error = Some("cast has no complete exit event".into()),
            Err(e) => meta.transcript_error = Some(format!("{e:#}")),
        }
    }
    if let Some(error) = &meta.transcript_error {
        eprintln!("WARNING: transcript incomplete: {error}; run termlog rebuild {id}");
    }
    meta.ended_at = Some(meta.started_at + chrono::Duration::from_std(duration)?);
    meta.duration_ms = Some(duration.as_millis() as u64);
    meta.exit_code = Some(exit);
    meta.utf8_replacements = replacements.load(Ordering::Relaxed);
    meta.recording_error = relay
        .as_ref()
        .err()
        .or(result.as_ref().err())
        .map(|e| format!("{e:#}"))
        .or_else(|| {
            (meta.utf8_replacements > 0)
                .then(|| "invalid UTF-8 was replaced in the recording".into())
        });
    meta.recording_complete = relay.is_ok()
        && result.is_ok()
        && !failed.load(Ordering::Acquire)
        && meta.utf8_replacements == 0;
    if !meta.recording_complete {
        eprintln!(
            "WARNING: session {} is incomplete{}",
            id,
            meta.recording_error
                .as_ref()
                .map(|s| format!(": {s}"))
                .unwrap_or_default()
        );
    }
    if let Err(e) = storage::write_metadata(&path, &meta) {
        eprintln!("WARNING: metadata could not be finalized: {e:#}");
    }
    relay?;
    Ok(exit)
}
fn send(
    sender: &mut Option<SyncSender<Event>>,
    start: Instant,
    kind: char,
    data: Vec<u8>,
    failed: &AtomicBool,
) {
    if let Some(tx) = sender {
        if tx
            .send(Event {
                micros: start.elapsed().as_micros() as u64,
                kind,
                data,
            })
            .is_err()
        {
            failed.store(true, Ordering::Release);
            sender.take();
        }
    }
}

// portable-pty exposes Unix signal descriptions but loses the signal number.
// Recover the conventional shell status using the same libc descriptions.
fn exit_code(status: &portable_pty::ExitStatus) -> u32 {
    if let Some(description) = status.signal() {
        for number in 1..=64 {
            let ptr = unsafe { libc::strsignal(number) };
            if !ptr.is_null()
                && unsafe { std::ffi::CStr::from_ptr(ptr) }.to_string_lossy() == description
            {
                return 128 + number as u32;
            }
        }
    }
    status.exit_code()
}

pub fn replay_interrupted(signals: &Signals) -> Result<bool> {
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
