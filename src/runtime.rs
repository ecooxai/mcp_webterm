//! Native PTY runtime and its small, bounded Unix-socket RPC client.
//!
//! The runtime process owns every PTY handle.  Web, CLI, and TUI processes are
//! deliberately only clients, so restarting one of those clients does not
//! disturb a shell.

use std::{
    collections::{HashMap, HashSet},
    fs::{self, OpenOptions},
    io::{self, BufRead, BufReader, Read, Write},
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    os::unix::{
        fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        mpsc::{self, Receiver, SyncSender, TrySendError},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow, bail};
use portable_pty::{Child, ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;

const DEFAULT_COLS: u16 = 80;
const DEFAULT_ROWS: u16 = 24;
pub const MAX_COLS: u16 = 512;
pub const MAX_ROWS: u16 = 256;
const SCROLLBACK_ROWS: usize = 3_000;
const MAX_SESSION_ID: usize = 128;
const MAX_INPUT: usize = 64 * 1024;
const MAX_CAPTURE_LINES: u16 = 2_000;
const MAX_FRAME: usize = 4 * 1024 * 1024;
const MAX_SNAPSHOT: usize = 1024 * 1024;
const MODEL_REBASE_BYTES: usize = 8 * 1024 * 1024;
const PTY_READ_SIZE: usize = 8 * 1024;
const MAX_SUBSCRIBERS_PER_SESSION: usize = 16;
const SUBSCRIBER_QUEUE: usize = 16;
const MAX_CONNECTIONS: usize = 256;
const RPC_TIMEOUT: Duration = Duration::from_secs(5);
const STOP_GRACE: Duration = Duration::from_millis(150);
const STOP_TIMEOUT: Duration = Duration::from_secs(4);
/// Per-terminal threads only block in poll/waitpid; small stacks keep
/// thousands of terminals cheap in address space and RSS.
const SESSION_THREAD_STACK: usize = 256 * 1024;
const CONNECTION_THREAD_STACK: usize = 512 * 1024;
/// Soft descriptor limit requested at startup; each live PTY uses ~6 fds.
const FD_LIMIT_TARGET: u64 = 65_536;
/// Keep this much space free on a swap filesystem before using it.
const SWAP_RESERVE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_SWEEP_INTERVAL: Duration = Duration::from_secs(60);
const RESUME_NOTICE: &[u8] =
    b"\r\n\x1b[2m[webterm: idle shell was hibernated; started a new shell]\x1b[0m\r\n";

/// Runtime daemon policy for idle terminals.
#[derive(Clone, Debug, Default)]
pub struct RuntimeOptions {
    /// Inactivity after which the screen moves to disk and an idle shell retires.
    pub idle_timeout: Option<Duration>,
    /// Private swap folders, tried in order until one has room.
    pub swap_dirs: Vec<PathBuf>,
}

impl RuntimeOptions {
    pub fn from_config(config: &crate::config::Config) -> Self {
        let uid = unsafe { libc::geteuid() };
        let mut bases = Vec::new();
        if let Some(dir) = &config.runtime_swap_dir {
            bases.push(dir.clone());
        }
        bases.push(std::env::temp_dir().join(format!("webterm-swap-{uid}")));
        bases.push(PathBuf::from(format!("/tmp/webterm-swap-{uid}")));
        bases.push(PathBuf::from(format!("/var/tmp/webterm-swap-{uid}")));
        if let Some(parent) = config.runtime_socket.parent() {
            bases.push(parent.join("swap"));
        }
        if let Some(home) = std::env::var_os("HOME") {
            bases.push(PathBuf::from(home).join(".cache/webterm/swap"));
        }
        // Several runtimes may share one user; never mix their swap files.
        let namespace = format!(
            "rt-{:016x}",
            fnv1a(config.runtime_socket.as_os_str().as_encoded_bytes())
        );
        let mut swap_dirs: Vec<PathBuf> = Vec::new();
        for base in bases {
            let dir = base.join(&namespace);
            if !swap_dirs.contains(&dir) {
                swap_dirs.push(dir);
            }
        }
        Self {
            idle_timeout: (config.runtime_idle_seconds > 0)
                .then(|| Duration::from_secs(config.runtime_idle_seconds)),
            swap_dirs,
        }
    }

    fn sweep_interval(&self) -> Option<Duration> {
        self.idle_timeout
            .map(|idle| (idle / 4).clamp(Duration::from_secs(1), MAX_SWEEP_INTERVAL))
    }
}

/// Aggregate runtime counters, cheap enough to poll.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RuntimeStats {
    pub sessions: usize,
    pub active_shells: usize,
    pub dormant: usize,
    pub swapped_screens: usize,
    pub subscribers: usize,
    pub idle_timeout_seconds: Option<u64>,
}

fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

#[derive(Clone, Debug)]
pub struct RuntimeClient {
    socket: PathBuf,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RuntimeInfo {
    pub pid: Option<u32>,
    pub cols: u16,
    pub rows: u16,
    pub running: bool,
    pub backend: String,
    /// The shell was retired while idle and starts again on next input/viewer.
    #[serde(default)]
    pub hibernated: bool,
    /// The screen and scrollback currently live in a swap file.
    #[serde(default)]
    pub swapped: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum RuntimeEvent {
    Output {
        data: Vec<u8>,
    },
    Resize {
        cols: u16,
        rows: u16,
        snapshot: Vec<u8>,
    },
    Closed,
}

pub struct Subscription {
    pub snapshot: Vec<u8>,
    pub cols: u16,
    pub rows: u16,
    reader: BufReader<UnixStream>,
    stream: UnixStream,
}

impl Subscription {
    pub fn read_event(&mut self) -> Result<Option<RuntimeEvent>> {
        let Some(frame) = read_frame(&mut self.reader, MAX_FRAME)? else {
            return Ok(None);
        };
        serde_json::from_slice(&frame)
            .context("decode runtime subscription event")
            .map(Some)
    }

    /// A duplicate of the event stream, useful for `shutdown()` to wake a
    /// thread blocked in [`Subscription::read_event`].
    pub fn stream(&self) -> &UnixStream {
        &self.stream
    }
}

impl RuntimeClient {
    pub fn new(socket: &Path) -> Result<Self> {
        validate_socket_path(socket)?;
        Ok(Self {
            socket: socket.to_owned(),
        })
    }

    pub fn create(&self, id: &str, cwd: &Path) -> Result<()> {
        validate_session_id(id)?;
        if !cwd.is_absolute() {
            bail!("terminal working directory must be absolute")
        }
        self.rpc_unit(&Request::Create {
            id: id.to_owned(),
            cwd: cwd.to_owned(),
        })
    }

    pub fn has_session(&self, id: &str) -> Result<bool> {
        validate_session_id(id)?;
        self.rpc(&Request::HasSession { id: id.to_owned() })
    }

    /// IDs of every session that is logically alive (including hibernated
    /// shells), in one RPC instead of one connection per terminal.
    pub fn list_sessions(&self) -> Result<HashSet<String>> {
        let ids: Vec<String> = self.rpc(&Request::List)?;
        Ok(ids.into_iter().collect())
    }

    pub fn stats(&self) -> Result<RuntimeStats> {
        self.rpc(&Request::Stats)
    }

    pub fn write(&self, id: &str, data: &str, enter: bool) -> Result<()> {
        let extra = usize::from(enter);
        if data.len().saturating_add(extra) > MAX_INPUT {
            bail!("terminal input exceeds {MAX_INPUT} bytes")
        }
        let mut bytes = data.as_bytes().to_vec();
        if enter {
            bytes.push(b'\r');
        }
        self.write_bytes(id, &bytes)
    }

    pub fn write_bytes(&self, id: &str, data: &[u8]) -> Result<()> {
        validate_session_id(id)?;
        if data.len() > MAX_INPUT {
            bail!("terminal input exceeds {MAX_INPUT} bytes")
        }
        self.rpc_unit(&Request::Write {
            id: id.to_owned(),
            data: data.to_vec(),
        })
    }

    pub fn resize(&self, id: &str, cols: u16, rows: u16) -> Result<()> {
        validate_session_id(id)?;
        validate_dimensions(cols, rows)?;
        self.rpc_unit(&Request::Resize {
            id: id.to_owned(),
            cols,
            rows,
        })
    }

    pub fn stop(&self, id: &str) -> Result<()> {
        validate_session_id(id)?;
        self.rpc_unit(&Request::Stop { id: id.to_owned() })
    }

    pub fn capture(&self, id: &str, lines: u16) -> Result<String> {
        validate_session_id(id)?;
        if lines > MAX_CAPTURE_LINES {
            bail!("capture is limited to {MAX_CAPTURE_LINES} lines")
        }
        self.rpc(&Request::Capture {
            id: id.to_owned(),
            lines,
        })
    }

    pub fn subscribe(&self, id: &str) -> Result<Subscription> {
        validate_session_id(id)?;
        let mut stream = self.connect()?;
        write_json_frame(&mut stream, &Request::Subscribe { id: id.to_owned() })?;
        let mut reader = BufReader::new(stream.try_clone().context("clone runtime socket")?);
        let frame = read_frame(&mut reader, MAX_FRAME)?
            .ok_or_else(|| anyhow!("runtime closed the subscription handshake"))?;
        let response: WireResponse =
            serde_json::from_slice(&frame).context("decode runtime response")?;
        let value = response.into_result()?;
        let hello: SubscribeHello =
            serde_json::from_value(value).context("decode subscription snapshot")?;
        stream
            .set_read_timeout(None)
            .context("clear runtime subscription timeout")?;
        // Keep the BufReader: it may already hold events received in the same
        // read as the handshake. Replacing it would silently discard output.
        reader
            .get_ref()
            .set_read_timeout(None)
            .context("clear runtime event timeout")?;
        Ok(Subscription {
            snapshot: hello.snapshot,
            cols: hello.cols,
            rows: hello.rows,
            reader,
            stream,
        })
    }

    pub fn info(&self, id: &str) -> Result<RuntimeInfo> {
        validate_session_id(id)?;
        self.rpc(&Request::Info { id: id.to_owned() })
    }

    /// Attach the current terminal.  Input is read by this thread (rather
    /// than a detached stdin-copy thread), ensuring that Ctrl-] returns stdin
    /// to a surrounding TUI immediately.
    pub fn attach(&self, id: &str) -> Result<()> {
        use std::io::IsTerminal;
        use std::net::Shutdown;
        use std::os::fd::AsRawFd;

        validate_session_id(id)?;
        if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
            bail!("attach requires a terminal on stdin and stdout")
        }

        if let Ok((cols, rows)) = crossterm::terminal::size() {
            self.resize(id, cols.clamp(2, MAX_COLS), rows.clamp(2, MAX_ROWS))?;
        }
        let mut subscription = self.subscribe(id)?;
        let _terminal = RawTerminalGuard::enter()?;
        {
            let mut stdout = io::stdout().lock();
            stdout.write_all(b"\x1b[?1049h\x1b[2J\x1b[H")?;
            stdout.write_all(&subscription.snapshot)?;
            stdout.flush()?;
        }

        let wake = subscription
            .stream()
            .try_clone()
            .context("clone subscription wake socket")?;
        let event_done = Arc::new(AtomicBool::new(false));
        let event_done_thread = Arc::clone(&event_done);
        let (event_result_tx, event_result_rx) = mpsc::sync_channel(1);
        let event_thread = thread::spawn(move || {
            let result = (|| -> Result<()> {
                let mut stdout = io::stdout().lock();
                while let Some(event) = subscription.read_event()? {
                    match event {
                        RuntimeEvent::Output { data } => stdout.write_all(&data)?,
                        RuntimeEvent::Resize { snapshot, .. } => {
                            stdout.write_all(&snapshot)?;
                        }
                        RuntimeEvent::Closed => break,
                    }
                    stdout.flush()?;
                }
                Ok(())
            })();
            event_done_thread.store(true, Ordering::Release);
            let _ = event_result_tx.send(result);
        });

        let mut last_size = crossterm::terminal::size().ok();
        let stdin_fd = io::stdin().as_raw_fd();
        let mut input = [0_u8; 4096];
        let attach_result = (|| -> Result<()> {
            while !event_done.load(Ordering::Acquire) {
                let mut poll_fd = libc::pollfd {
                    fd: stdin_fd,
                    events: libc::POLLIN,
                    revents: 0,
                };
                let poll_result = unsafe { libc::poll(&mut poll_fd, 1, 200) };
                if poll_result < 0 {
                    let error = io::Error::last_os_error();
                    if error.kind() != io::ErrorKind::Interrupted {
                        return Err(error).context("poll terminal input");
                    }
                } else if poll_result > 0 && poll_fd.revents & libc::POLLIN != 0 {
                    let count = io::stdin().read(&mut input)?;
                    if count == 0 {
                        break;
                    }
                    if let Some(detach) = input[..count].iter().position(|byte| *byte == 0x1d) {
                        if detach > 0 {
                            self.write_bytes(id, &input[..detach])?;
                        }
                        break;
                    }
                    self.write_bytes(id, &input[..count])?;
                }

                let size = crossterm::terminal::size().ok();
                if size != last_size {
                    if let Some((cols, rows)) = size {
                        self.resize(id, cols.clamp(2, MAX_COLS), rows.clamp(2, MAX_ROWS))?;
                        last_size = Some((cols, rows));
                    }
                }
            }
            Ok(())
        })();

        let _ = wake.shutdown(Shutdown::Both);
        let _ = event_thread.join();
        let event_result = event_result_rx.try_recv().unwrap_or(Ok(()));
        attach_result.and(event_result)
    }

    fn connect(&self) -> Result<UnixStream> {
        let stream = UnixStream::connect(&self.socket)
            .with_context(|| format!("connect runtime socket {}", self.socket.display()))?;
        stream
            .set_read_timeout(Some(RPC_TIMEOUT))
            .context("set runtime read timeout")?;
        stream
            .set_write_timeout(Some(RPC_TIMEOUT))
            .context("set runtime write timeout")?;
        Ok(stream)
    }

    fn rpc_unit(&self, request: &Request) -> Result<()> {
        let _: () = self.rpc(request)?;
        Ok(())
    }

    fn rpc<T: DeserializeOwned>(&self, request: &Request) -> Result<T> {
        use std::net::Shutdown;

        let mut stream = self.connect()?;
        write_json_frame(&mut stream, request)?;
        stream.shutdown(Shutdown::Write).ok();
        let mut reader = BufReader::new(stream);
        let frame = read_frame(&mut reader, MAX_FRAME)?
            .ok_or_else(|| anyhow!("runtime closed the RPC connection"))?;
        let response: WireResponse =
            serde_json::from_slice(&frame).context("decode runtime response")?;
        serde_json::from_value(response.into_result()?).context("decode runtime result")
    }
}

struct RawTerminalGuard {
    original: libc::termios,
}

impl RawTerminalGuard {
    fn enter() -> Result<Self> {
        use std::os::fd::AsRawFd;
        let fd = io::stdin().as_raw_fd();
        let mut original = unsafe { std::mem::zeroed::<libc::termios>() };
        if unsafe { libc::tcgetattr(fd, &mut original) } != 0 {
            return Err(io::Error::last_os_error()).context("read terminal attributes");
        }
        let mut raw = original;
        unsafe { libc::cfmakeraw(&mut raw) };
        if unsafe { libc::tcsetattr(fd, libc::TCSAFLUSH, &raw) } != 0 {
            return Err(io::Error::last_os_error()).context("enable raw terminal mode");
        }
        Ok(Self { original })
    }
}

impl Drop for RawTerminalGuard {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;
        let fd = io::stdin().as_raw_fd();
        let _ = unsafe { libc::tcsetattr(fd, libc::TCSAFLUSH, &self.original) };
        let _ = io::stdout().write_all(
            b"\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?2004l\x1b[0m\x1b[?25h\x1b[?1049l",
        );
        let _ = io::stdout().flush();
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum Request {
    Create { id: String, cwd: PathBuf },
    HasSession { id: String },
    Write { id: String, data: Vec<u8> },
    Resize { id: String, cols: u16, rows: u16 },
    Stop { id: String },
    Capture { id: String, lines: u16 },
    Subscribe { id: String },
    Info { id: String },
    List,
    Stats,
}

#[derive(Debug, Serialize, Deserialize)]
struct WireResponse {
    ok: bool,
    #[serde(default)]
    result: Value,
    #[serde(default)]
    error: String,
}

impl WireResponse {
    fn success<T: Serialize>(result: T) -> Result<Self> {
        Ok(Self {
            ok: true,
            result: serde_json::to_value(result).context("encode runtime result")?,
            error: String::new(),
        })
    }

    fn failure(error: impl std::fmt::Display) -> Self {
        let mut error = error.to_string();
        error.truncate(1024);
        Self {
            ok: false,
            result: Value::Null,
            error,
        }
    }

    fn into_result(self) -> Result<Value> {
        if self.ok {
            Ok(self.result)
        } else if self.error.is_empty() {
            bail!("runtime request failed")
        } else {
            bail!("{}", self.error)
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct SubscribeHello {
    snapshot: Vec<u8>,
    cols: u16,
    rows: u16,
}

struct RuntimeServer {
    registry: Mutex<Registry>,
    options: Arc<RuntimeOptions>,
}

#[derive(Default)]
struct Registry {
    sessions: HashMap<String, Arc<Session>>,
    creating: HashSet<String>,
}

impl RuntimeServer {
    fn new(options: Arc<RuntimeOptions>) -> Self {
        Self {
            registry: Mutex::new(Registry::default()),
            options,
        }
    }

    fn sessions(&self) -> Result<Vec<Arc<Session>>> {
        Ok(lock(&self.registry, "runtime registry")?
            .sessions
            .values()
            .cloned()
            .collect())
    }

    fn list(&self) -> Result<Vec<String>> {
        Ok(lock(&self.registry, "runtime registry")?
            .sessions
            .iter()
            .filter(|(_, session)| !session.stopped.load(Ordering::Acquire))
            .map(|(id, _)| id.clone())
            .collect())
    }

    fn stats(&self) -> Result<RuntimeStats> {
        let mut stats = RuntimeStats {
            idle_timeout_seconds: self.options.idle_timeout.map(|idle| idle.as_secs()),
            ..RuntimeStats::default()
        };
        for session in self.sessions()? {
            if session.stopped.load(Ordering::Acquire) {
                continue;
            }
            stats.sessions += 1;
            if *lock(&session.phase, "terminal phase")? == Phase::Dormant {
                stats.dormant += 1;
            }
            let model = lock(&session.model, "terminal model")?;
            stats.active_shells += usize::from(model.running);
            stats.swapped_screens += usize::from(model.swapped.is_some());
            stats.subscribers += model.subscribers.len();
        }
        Ok(stats)
    }

    /// Move idle screens to disk and retire shells idle at a prompt.
    fn sweep(&self) -> Result<usize> {
        let Some(idle) = self.options.idle_timeout else {
            return Ok(0);
        };
        let idle_ms = idle.as_millis() as u64;
        let now = now_ms();
        let candidates = self
            .sessions()?
            .into_iter()
            .filter(|session| {
                !session.stopped.load(Ordering::Acquire)
                    && now.saturating_sub(session.last_activity.load(Ordering::Acquire)) >= idle_ms
            })
            .collect::<Vec<_>>();
        if candidates.is_empty() {
            return Ok(0);
        }
        let census = session_census();
        let changed = candidates
            .iter()
            .filter(|session| session.hibernate_if_idle(census.as_ref(), idle_ms))
            .count();
        if changed > 0 {
            trim_heap();
            tracing::info!(changed, "hibernated idle terminals");
        }
        Ok(changed)
    }

    fn create(&self, id: &str, cwd: &Path) -> Result<()> {
        validate_session_id(id)?;
        validate_working_directory(cwd)?;
        {
            let mut registry = lock(&self.registry, "runtime registry")?;
            if registry.sessions.contains_key(id) || registry.creating.contains(id) {
                bail!("terminal session already exists")
            }
            registry.creating.insert(id.to_owned());
        }

        let created = Session::create(id.to_owned(), cwd.to_owned(), Arc::clone(&self.options));
        let mut registry = lock(&self.registry, "runtime registry")?;
        registry.creating.remove(id);
        match created {
            Ok(session) => {
                registry.sessions.insert(id.to_owned(), session);
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    fn session(&self, id: &str) -> Result<Arc<Session>> {
        validate_session_id(id)?;
        lock(&self.registry, "runtime registry")?
            .sessions
            .get(id)
            .cloned()
            .ok_or_else(|| anyhow!("terminal session is not running"))
    }

    fn has_session(&self, id: &str) -> Result<bool> {
        validate_session_id(id)?;
        Ok(lock(&self.registry, "runtime registry")?
            .sessions
            .get(id)
            .is_some_and(|session| !session.stopped.load(Ordering::Acquire)))
    }

    fn stop(&self, id: &str) -> Result<()> {
        validate_session_id(id)?;
        let session = lock(&self.registry, "runtime registry")?
            .sessions
            .get(id)
            .cloned();
        let Some(session) = session else {
            return Ok(());
        };
        let result = session.stop();
        let mut registry = lock(&self.registry, "runtime registry")?;
        if registry
            .sessions
            .get(id)
            .is_some_and(|registered| Arc::ptr_eq(registered, &session))
        {
            registry.sessions.remove(id);
        }
        result
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Active,
    /// SIGHUP sent to an idle shell; the supervisor parks it once it exits.
    Retiring,
    /// No shell process or threads; screen on disk. Next use starts a shell.
    Dormant,
}

struct Session {
    id: String,
    cwd: PathBuf,
    shell: PathBuf,
    options: Arc<RuntimeOptions>,
    last_activity: AtomicU64,
    phase: Mutex<Phase>,
    resume_cwd: Mutex<Option<PathBuf>>,
    model: Mutex<TerminalModel>,
    io: Mutex<SessionIo>,
    write_lock: Mutex<()>,
    stopped: AtomicBool,
    restart_mutex: Mutex<()>,
    restart_cv: Condvar,
    done: Mutex<bool>,
    done_cv: Condvar,
}

#[derive(Default)]
struct SessionIo {
    master: Option<Box<dyn MasterPty + Send>>,
    writer: Option<Box<dyn Write + Send>>,
    killer: Option<Box<dyn ChildKiller + Send + Sync>>,
    shell_pid: Option<u32>,
    foreground_pgid: Option<i32>,
}

struct TerminalModel {
    parser: vt100::Parser,
    cols: u16,
    rows: u16,
    running: bool,
    pid: Option<u32>,
    last_exit: Option<ExitRecord>,
    subscribers: Vec<Subscriber>,
    next_subscriber: u64,
    bytes_since_rebase: usize,
    swapped: Option<PathBuf>,
}

impl Drop for TerminalModel {
    fn drop(&mut self) {
        if let Some(path) = self.swapped.take() {
            let _ = fs::remove_file(path);
        }
    }
}

#[derive(Clone, Debug)]
struct ExitRecord {
    code: u32,
    signal: Option<String>,
}

struct Subscriber {
    id: u64,
    sender: SyncSender<RuntimeEvent>,
}

struct Spawned {
    child: Box<dyn Child + Send + Sync>,
    reader: Box<dyn Read + Send>,
    poll_fd: OwnedFd,
}

impl Session {
    fn create(id: String, cwd: PathBuf, options: Arc<RuntimeOptions>) -> Result<Arc<Self>> {
        let shell = configured_shell()?;
        Self::create_with_options(id, cwd, shell, options)
    }

    #[cfg(test)]
    fn create_with_shell(id: String, cwd: PathBuf, shell: PathBuf) -> Result<Arc<Self>> {
        Self::create_with_options(id, cwd, shell, Arc::new(RuntimeOptions::default()))
    }

    fn create_with_options(
        id: String,
        cwd: PathBuf,
        shell: PathBuf,
        options: Arc<RuntimeOptions>,
    ) -> Result<Arc<Self>> {
        let session = Arc::new(Self {
            id,
            cwd,
            shell,
            options,
            last_activity: AtomicU64::new(now_ms()),
            phase: Mutex::new(Phase::Active),
            resume_cwd: Mutex::new(None),
            model: Mutex::new(TerminalModel {
                parser: vt100::Parser::new(DEFAULT_ROWS, DEFAULT_COLS, SCROLLBACK_ROWS),
                cols: DEFAULT_COLS,
                rows: DEFAULT_ROWS,
                running: false,
                pid: None,
                last_exit: None,
                subscribers: Vec::new(),
                next_subscriber: 1,
                bytes_since_rebase: 0,
                swapped: None,
            }),
            io: Mutex::new(SessionIo::default()),
            write_lock: Mutex::new(()),
            stopped: AtomicBool::new(false),
            restart_mutex: Mutex::new(()),
            restart_cv: Condvar::new(),
            done: Mutex::new(false),
            done_cv: Condvar::new(),
        });
        let initial = session.spawn_shell()?;
        session.start_supervisor(initial)?;
        Ok(session)
    }

    fn start_supervisor(self: &Arc<Self>, initial: Spawned) -> Result<()> {
        let supervisor = Arc::clone(self);
        let pid = initial.child.process_id();
        let started = thread::Builder::new()
            .name(format!("pty-supervisor-{}", self.id))
            .stack_size(SESSION_THREAD_STACK)
            .spawn(move || supervisor.supervise(initial));
        if let Err(error) = started {
            if let Some(pid) = pid.and_then(|pid| i32::try_from(pid).ok()) {
                unsafe {
                    libc::kill(-pid, libc::SIGKILL);
                    libc::kill(pid, libc::SIGKILL);
                }
            }
            return Err(error).context("start PTY supervisor");
        }
        Ok(())
    }

    fn touch(&self) {
        self.last_activity.store(now_ms(), Ordering::Release);
    }

    /// Start a fresh shell for a hibernated terminal. Cheap no-op otherwise.
    fn wake(self: &Arc<Self>) -> Result<()> {
        let mut phase = lock(&self.phase, "terminal phase")?;
        match *phase {
            Phase::Active => {}
            // The supervisor restarts the shell through its normal path.
            Phase::Retiring => *phase = Phase::Active,
            Phase::Dormant => {
                if self.stopped.load(Ordering::Acquire) {
                    bail!("terminal session is stopped")
                }
                {
                    let mut model = lock(&self.model, "terminal model")?;
                    model.process_output(RESUME_NOTICE);
                }
                let spawned = self.spawn_shell()?;
                self.start_supervisor(spawned)?;
                *phase = Phase::Active;
                tracing::info!(session = %self.id, "resumed hibernated terminal");
            }
        }
        self.touch();
        Ok(())
    }

    /// Called by the sweeper for a session idle past the timeout.
    fn hibernate_if_idle(&self, census: Option<&HashMap<u32, usize>>, idle_ms: u64) -> bool {
        let Ok(mut phase) = self.phase.lock() else {
            return false;
        };
        if self.stopped.load(Ordering::Acquire) {
            return false;
        }
        let Ok(mut model) = self.model.lock() else {
            return false;
        };
        if !model.subscribers.is_empty()
            || now_ms().saturating_sub(self.last_activity.load(Ordering::Acquire)) < idle_ms
        {
            return false;
        }
        match *phase {
            // A capture may have reloaded the screen; put it back on disk.
            Phase::Dormant => {
                return model
                    .hibernate(&self.options.swap_dirs, &self.id)
                    .unwrap_or(false);
            }
            Phase::Retiring => {
                // Still alive a full sweep after SIGHUP.
                if let Some(pid) = model.pid.and_then(|pid| i32::try_from(pid).ok()) {
                    unsafe {
                        libc::kill(-pid, libc::SIGKILL);
                        libc::kill(pid, libc::SIGKILL);
                    }
                }
                return false;
            }
            Phase::Active => {}
        }
        // Only a shell sitting at its prompt is retired: any other process in
        // its terminal session (job, server, sudo, editor) keeps it alive.
        let idle_shell = model.running
            && model.pid.is_some_and(|pid| {
                census.is_some_and(|census| census.get(&pid).copied().unwrap_or(0) <= 1)
            });
        if idle_shell && let Some(pid) = model.pid {
            if let Ok(target) = fs::read_link(format!("/proc/{pid}/cwd"))
                && let Ok(mut resume) = self.resume_cwd.lock()
            {
                *resume = Some(target);
            }
            *phase = Phase::Retiring;
            if let Ok(pid) = i32::try_from(pid) {
                unsafe {
                    libc::kill(-pid, libc::SIGHUP);
                    libc::kill(pid, libc::SIGHUP);
                }
            }
            return true;
        }
        match model.hibernate(&self.options.swap_dirs, &self.id) {
            Ok(changed) => changed,
            Err(error) => {
                tracing::warn!(session = %self.id, %error, "could not move idle screen to disk");
                false
            }
        }
    }

    fn spawn_shell(&self) -> Result<Spawned> {
        if self.stopped.load(Ordering::Acquire) {
            bail!("terminal session was stopped")
        }
        let (cols, rows) = {
            let model = lock(&self.model, "terminal model")?;
            (model.cols, model.rows)
        };
        let pair = native_pty_system()
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("open terminal PTY")?;
        let fd = pair
            .master
            .as_raw_fd()
            .ok_or_else(|| anyhow!("PTY has no Unix descriptor"))?;
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            return Err(io::Error::last_os_error()).context("make PTY I/O nonblocking");
        }
        let copied = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
        if copied < 0 {
            return Err(io::Error::last_os_error()).context("clone PTY poll descriptor");
        }
        let poll_fd = unsafe { OwnedFd::from_raw_fd(copied) };
        let reader = pair.master.try_clone_reader().context("clone PTY reader")?;
        let writer = pair.master.take_writer().context("open PTY writer")?;
        let cwd = lock(&self.resume_cwd, "terminal resume folder")?
            .clone()
            .filter(|path| path.is_dir())
            .unwrap_or_else(|| self.cwd.clone());
        let mut command = CommandBuilder::new(&self.shell);
        command.arg("-l");
        command.cwd(&cwd);
        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");
        for key in [
            "TMUX",
            "TMUX_PANE",
            "WEBTERM_PASSWORD",
            "WEBTERM_AUTH_TOKEN",
            "AUTH_TOKEN",
        ] {
            command.env_remove(key);
        }
        let child = pair
            .slave
            .spawn_command(command)
            .with_context(|| format!("start login shell {}", self.shell.display()))?;
        drop(pair.slave);
        let pid = child.process_id();
        let foreground_pgid = pair.master.process_group_leader();
        let killer = child.clone_killer();
        {
            let mut session_io = lock(&self.io, "terminal PTY")?;
            session_io.master = Some(pair.master);
            session_io.writer = Some(writer);
            session_io.killer = Some(killer);
            session_io.shell_pid = pid;
            session_io.foreground_pgid = foreground_pgid;
        }
        {
            let mut model = lock(&self.model, "terminal model")?;
            model.running = true;
            model.pid = pid;
        }
        if self.stopped.load(Ordering::Acquire) {
            self.signal_owned_processes(libc::SIGKILL);
        }
        Ok(Spawned {
            child,
            reader,
            poll_fd,
        })
    }

    fn supervise(self: Arc<Self>, initial: Spawned) {
        let mut next = Some(initial);
        while let Some(mut spawned) = next.take() {
            let reader_session = Arc::clone(&self);
            let reader_finished = Arc::new(AtomicBool::new(false));
            let reader_finished_thread = reader_finished.clone();
            let (wake_read, wake_write) = match wake_pipe() {
                Ok((read, write)) => (Some(read), Some(write)),
                Err(error) => {
                    tracing::warn!(session = %self.id, %error, "PTY wake pipe unavailable; polling");
                    (None, None)
                }
            };
            let reader = thread::Builder::new()
                .name(format!("pty-reader-{}", self.id))
                .stack_size(SESSION_THREAD_STACK)
                .spawn(move || {
                    reader_session.read_pty(
                        spawned.reader,
                        spawned.poll_fd,
                        wake_read,
                        reader_finished_thread,
                    )
                });
            if reader.is_err() {
                self.signal_owned_processes(libc::SIGKILL);
            }

            let status = spawned.child.wait();
            // Stop the old incarnation's reader even if a background child kept
            // its slave open. Those process groups belong to this terminal only.
            reader_finished.store(true, Ordering::Release);
            if let Some(wake) = &wake_write {
                let _ = unsafe { libc::write(wake.as_raw_fd(), b"x".as_ptr().cast(), 1) };
            }
            let retiring = self
                .phase
                .lock()
                .is_ok_and(|phase| *phase == Phase::Retiring);
            if retiring {
                // The sweeper verified nothing else ran in this session; skip
                // the /proc scans so thousands of shells can retire at once.
                if let Some(pid) = spawned
                    .child
                    .process_id()
                    .and_then(|pid| i32::try_from(pid).ok())
                {
                    let _ = unsafe { libc::kill(-pid, libc::SIGKILL) };
                }
            } else {
                self.signal_owned_processes(libc::SIGHUP);
                self.signal_owned_processes(libc::SIGKILL);
            }
            {
                if let Ok(mut session_io) = self.io.lock() {
                    session_io.writer.take();
                    session_io.master.take();
                    session_io.killer.take();
                    session_io.shell_pid = None;
                    session_io.foreground_pgid = None;
                }
            }
            if let Ok(reader) = reader {
                let _ = reader.join();
            }
            if let Ok(mut model) = self.model.lock() {
                model.running = false;
                model.pid = None;
                model.last_exit = status.ok().map(|status| ExitRecord {
                    code: status.exit_code(),
                    signal: status.signal().map(ToOwned::to_owned),
                });
            }

            {
                let Ok(mut phase) = self.phase.lock() else {
                    break;
                };
                if self.stopped.load(Ordering::Acquire) {
                    break;
                }
                if *phase == Phase::Retiring {
                    if let Ok(mut model) = self.model.lock()
                        && let Err(error) = model.hibernate(&self.options.swap_dirs, &self.id)
                    {
                        tracing::warn!(session = %self.id, %error, "could not move idle screen to disk");
                    }
                    *phase = Phase::Dormant;
                    // No shell and no threads remain until the next viewer/input.
                    return;
                }
            }
            loop {
                let guard = match self.restart_mutex.lock() {
                    Ok(guard) => guard,
                    Err(_) => break,
                };
                let _ = self.restart_cv.wait_timeout(guard, Duration::from_secs(1));
                if self.stopped.load(Ordering::Acquire) {
                    break;
                }
                match self.spawn_shell() {
                    Ok(spawned) => {
                        next = Some(spawned);
                        break;
                    }
                    Err(error) => {
                        tracing::warn!(session = %self.id, %error, "failed to restart login shell");
                    }
                }
            }
        }

        self.finish_closed();
    }

    fn finish_closed(&self) {
        if let Ok(mut model) = self.model.lock() {
            broadcast(&mut model.subscribers, RuntimeEvent::Closed);
            model.subscribers.clear();
            model.running = false;
            model.pid = None;
            if let Some(path) = model.swapped.take() {
                let _ = fs::remove_file(path);
            }
        }
        if let Ok(mut done) = self.done.lock() {
            *done = true;
            self.done_cv.notify_all();
        }
    }

    fn read_pty(
        &self,
        mut reader: Box<dyn Read + Send>,
        poll_fd: OwnedFd,
        wake: Option<OwnedFd>,
        finished: Arc<AtomicBool>,
    ) {
        let mut bytes = [0_u8; PTY_READ_SIZE];
        let mut drain_after_exit = 0usize;
        loop {
            if self.stopped.load(Ordering::Acquire) {
                break;
            }
            if finished.load(Ordering::Acquire) {
                drain_after_exit += PTY_READ_SIZE;
                if drain_after_exit > MAX_INPUT {
                    break;
                }
            }
            match reader.read(&mut bytes) {
                Ok(0) => break,
                Ok(count) => {
                    self.touch();
                    if let Ok(mut model) = self.model.lock() {
                        model.process_output(&bytes[..count]);
                    } else {
                        break;
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    if finished.load(Ordering::Acquire) {
                        break;
                    }
                    // Block without a timer: idle terminals cost no CPU. The
                    // supervisor writes to the wake pipe when the shell exits.
                    let mut polls = [
                        libc::pollfd {
                            fd: poll_fd.as_raw_fd(),
                            events: libc::POLLIN,
                            revents: 0,
                        },
                        libc::pollfd {
                            fd: wake.as_ref().map_or(-1, AsRawFd::as_raw_fd),
                            events: libc::POLLIN,
                            revents: 0,
                        },
                    ];
                    let timeout = if wake.is_some() { -1 } else { 100 };
                    let _ = unsafe { libc::poll(polls.as_mut_ptr(), 2, timeout) };
                }
                // Linux PTY masters commonly report EIO when the final slave closes.
                Err(_) => break,
            }
        }
    }

    fn write(self: &Arc<Self>, data: &[u8]) -> Result<()> {
        if data.len() > MAX_INPUT {
            bail!("terminal input exceeds {MAX_INPUT} bytes")
        }
        self.wake()?;
        let deadline = Instant::now() + Duration::from_secs(2);
        // Serialize one input message without keeping the PTY-control mutex
        // locked while the child is not reading. Stop/resize remain responsive.
        let _serial = loop {
            if self.stopped.load(Ordering::Acquire) {
                bail!("terminal session is stopped")
            }
            match self.write_lock.try_lock() {
                Ok(guard) => break guard,
                Err(std::sync::TryLockError::Poisoned(_)) => {
                    bail!("terminal input lock is poisoned")
                }
                Err(std::sync::TryLockError::WouldBlock) => {}
            }
            if Instant::now() >= deadline {
                bail!("terminal input queue timed out")
            }
            thread::sleep(Duration::from_millis(5));
        };
        let mut written = 0;
        while written < data.len() {
            if self.stopped.load(Ordering::Acquire) {
                bail!("terminal session is stopped")
            }
            let result = {
                let mut io = lock(&self.io, "terminal PTY")?;
                match io.writer.as_mut() {
                    Some(writer) => writer.write(&data[written..]),
                    None => Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        "login shell is restarting",
                    )),
                }
            };
            match result {
                Ok(0) => bail!("terminal input closed after {written} bytes"),
                Ok(n) => written += n,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        bail!("terminal input timed out after {written} bytes: {error}")
                    }
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => return Err(error).context("write terminal input"),
            }
        }
        self.touch();
        Ok(())
    }

    fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        validate_dimensions(cols, rows)?;
        if self.stopped.load(Ordering::Acquire) {
            bail!("terminal session is stopped")
        }
        let io = lock(&self.io, "terminal PTY")?;
        let mut model = lock(&self.model, "terminal model")?;
        if model.cols == cols && model.rows == rows {
            return Ok(());
        }
        self.touch();
        model.ensure_resident();
        // Without a live PTY (restarting or hibernated) the next shell is
        // opened at the model size.
        if let Some(master) = io.master.as_ref() {
            master
                .resize(PtySize {
                    rows,
                    cols,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .context("resize terminal PTY")?;
        }
        model.cols = cols;
        model.rows = rows;
        model.parser.screen_mut().set_size(rows, cols);
        let snapshot = model.snapshot();
        broadcast(
            &mut model.subscribers,
            RuntimeEvent::Resize {
                cols,
                rows,
                snapshot,
            },
        );
        Ok(())
    }

    fn capture(&self, lines: u16) -> Result<String> {
        if lines > MAX_CAPTURE_LINES {
            bail!("capture is limited to {MAX_CAPTURE_LINES} lines")
        }
        let mut model = lock(&self.model, "terminal model")?;
        Ok(model.capture(lines))
    }

    fn subscribe(self: &Arc<Self>) -> Result<(SubscribeHello, u64, Receiver<RuntimeEvent>)> {
        if let Err(error) = self.wake() {
            tracing::warn!(session = %self.id, %error, "could not resume hibernated terminal");
        }
        self.touch();
        let mut model = lock(&self.model, "terminal model")?;
        if model.subscribers.len() >= MAX_SUBSCRIBERS_PER_SESSION {
            bail!("terminal subscriber limit reached")
        }
        let snapshot = model.snapshot();
        let hello = SubscribeHello {
            snapshot,
            cols: model.cols,
            rows: model.rows,
        };
        let (sender, receiver) = mpsc::sync_channel(SUBSCRIBER_QUEUE);
        let id = model.next_subscriber;
        model.next_subscriber = model.next_subscriber.wrapping_add(1).max(1);
        model.subscribers.push(Subscriber { id, sender });
        Ok((hello, id, receiver))
    }

    fn unsubscribe(&self, subscriber_id: u64) {
        self.touch();
        if let Ok(mut model) = self.model.lock() {
            model.subscribers.retain(|item| item.id != subscriber_id);
        }
    }

    fn info(&self) -> Result<RuntimeInfo> {
        let hibernated = *lock(&self.phase, "terminal phase")? == Phase::Dormant;
        let model = lock(&self.model, "terminal model")?;
        if let Some(exit) = &model.last_exit {
            let _ = (exit.code, exit.signal.as_deref());
        }
        Ok(RuntimeInfo {
            pid: model.pid,
            cols: model.cols,
            rows: model.rows,
            running: model.running,
            backend: "native-pty".to_owned(),
            hibernated,
            swapped: model.swapped.is_some(),
        })
    }

    fn stop(&self) -> Result<()> {
        let first = {
            let phase = lock(&self.phase, "terminal phase")?;
            let first = !self.stopped.swap(true, Ordering::AcqRel);
            if *phase == Phase::Dormant {
                // No supervisor exists to report completion.
                self.finish_closed();
                return Ok(());
            }
            first
        };
        if !first {
            return self.wait_done(STOP_TIMEOUT);
        }
        self.restart_cv.notify_all();
        self.signal_owned_processes(libc::SIGHUP);
        if self.wait_done(STOP_GRACE).is_ok() {
            return Ok(());
        }
        self.signal_owned_processes(libc::SIGTERM);
        if self.wait_done(STOP_GRACE).is_ok() {
            return Ok(());
        }
        self.signal_owned_processes(libc::SIGKILL);
        {
            let mut session_io = lock(&self.io, "terminal PTY")?;
            session_io.writer.take();
            session_io.master.take();
        }
        self.wait_done(STOP_TIMEOUT)
    }

    fn signal_owned_processes(&self, signal: i32) {
        let (shell, foreground) = {
            let Ok(io) = self.io.lock() else {
                return;
            };
            (
                io.shell_pid,
                io.master
                    .as_ref()
                    .and_then(|master| master.process_group_leader())
                    .or(io.foreground_pgid),
            )
        };
        let mut groups = HashSet::new();
        if let Some(group) = foreground {
            groups.insert(group);
        }
        if let Some(pid) = shell {
            if let Ok(group) = i32::try_from(pid) {
                groups.insert(group);
            }
            // Interactive shells put background/foreground jobs in distinct
            // process groups, but they remain in this child's terminal session.
            // Never signal any other user's session or unrelated process group.
            #[cfg(target_os = "linux")]
            if let Ok(entries) = fs::read_dir("/proc") {
                for entry in entries.flatten() {
                    if entry.file_name().to_string_lossy().parse::<u32>().is_err() {
                        continue;
                    }
                    let Ok(metadata) = entry.metadata() else {
                        continue;
                    };
                    if metadata.uid() != unsafe { libc::geteuid() } {
                        continue;
                    }
                    let Ok(stat) = fs::read_to_string(entry.path().join("stat")) else {
                        continue;
                    };
                    let Some((_, rest)) = stat.rsplit_once(") ") else {
                        continue;
                    };
                    let fields: Vec<_> = rest.split_whitespace().take(4).collect();
                    if fields.len() == 4 && fields[3].parse::<u32>().ok() == Some(pid) {
                        if let Ok(group) = fields[2].parse::<i32>() {
                            groups.insert(group);
                        }
                    }
                }
            }
        }
        for group in groups {
            if group > 1 {
                let _ = unsafe { libc::kill(-group, signal) };
            }
        }
    }

    fn wait_done(&self, timeout: Duration) -> Result<()> {
        let done = lock(&self.done, "terminal supervisor state")?;
        if *done {
            return Ok(());
        }
        let (done, _) = self
            .done_cv
            .wait_timeout(done, timeout)
            .map_err(|_| anyhow!("terminal supervisor state is poisoned"))?;
        if *done {
            Ok(())
        } else {
            bail!("timed out waiting for terminal process to stop")
        }
    }
}

impl TerminalModel {
    /// Reload a screen that was moved to disk while idle.
    fn ensure_resident(&mut self) {
        let Some(path) = self.swapped.take() else {
            return;
        };
        let mut parser = vt100::Parser::new(self.rows, self.cols, SCROLLBACK_ROWS);
        match fs::read(&path) {
            Ok(bytes) => parser.process(&bytes),
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "hibernated screen unavailable");
                parser.process(b"[webterm: hibernated screen could not be restored]\r\n");
            }
        }
        let _ = fs::remove_file(&path);
        self.parser = parser;
        self.bytes_since_rebase = 0;
    }

    /// Write the canonical screen and scrollback to a private swap file and
    /// release the in-memory grid. Returns false when already on disk.
    fn hibernate(&mut self, dirs: &[PathBuf], id: &str) -> Result<bool> {
        if self.swapped.is_some() {
            return Ok(false);
        }
        let snapshot = self.snapshot();
        let path = write_swap_file(dirs, id, &snapshot)?;
        self.parser = vt100::Parser::new(2, 2, 0);
        self.swapped = Some(path);
        Ok(true)
    }

    fn process_output(&mut self, data: &[u8]) {
        self.ensure_resident();
        self.parser.process(data);
        self.bytes_since_rebase = self.bytes_since_rebase.saturating_add(data.len());
        if self.bytes_since_rebase >= MODEL_REBASE_BYTES {
            let snapshot = self.snapshot();
            self.bytes_since_rebase = 0;
            if snapshot.len() > MAX_SNAPSHOT || self.capture(MAX_CAPTURE_LINES).len() > MAX_SNAPSHOT
            {
                self.parser = vt100::Parser::new(self.rows, self.cols, SCROLLBACK_ROWS);
                self.parser
                    .process(b"[terminal screen reset: state exceeded memory limit]\r\n");
                let snapshot = self.snapshot();
                broadcast(
                    &mut self.subscribers,
                    RuntimeEvent::Resize {
                        cols: self.cols,
                        rows: self.rows,
                        snapshot,
                    },
                );
                return;
            }
        }
        broadcast(
            &mut self.subscribers,
            RuntimeEvent::Output {
                data: data.to_vec(),
            },
        );
    }

    fn snapshot(&mut self) -> Vec<u8> {
        self.ensure_resident();
        canonical_snapshot(&self.parser)
    }

    fn capture(&mut self, lines: u16) -> String {
        if lines == 0 {
            return String::new();
        }
        self.ensure_resident();
        let requested = usize::from(lines);
        let screen = self.parser.screen_mut();
        if screen.alternate_screen() {
            return screen
                .rows(0, self.cols)
                .take(requested)
                .collect::<Vec<_>>()
                .join("\n");
        }

        screen.set_scrollback(usize::MAX);
        let history = screen.scrollback();
        screen.set_scrollback(0);
        let total = history.saturating_add(usize::from(self.rows));
        let mut index = total.saturating_sub(requested);
        let mut captured = Vec::with_capacity(requested.min(total));
        while index < total && captured.len() < requested {
            if index < history {
                screen.set_scrollback(history - index);
                let take = usize::from(self.rows)
                    .min(history - index)
                    .min(requested - captured.len());
                captured.extend(screen.rows(0, self.cols).take(take));
                index += take;
            } else {
                screen.set_scrollback(0);
                let skip = index - history;
                let take = (total - index).min(requested - captured.len());
                captured.extend(screen.rows(0, self.cols).skip(skip).take(take));
                index += take;
            }
        }
        screen.set_scrollback(0);
        captured.join("\n")
    }
}

fn canonical_snapshot(parser: &vt100::Parser) -> Vec<u8> {
    let mut screen = parser.screen().clone();
    let (rows, cols) = screen.size();
    let mut snapshot = Vec::new();
    snapshot.extend_from_slice(b"\x1bc");
    if screen.alternate_screen() {
        snapshot.extend_from_slice(b"\x1b[?1049h");
    } else {
        screen.set_scrollback(usize::MAX);
        let history = screen.scrollback();
        let mut history_rows = std::collections::VecDeque::new();
        let mut bytes = 0usize;
        for index in 0..history {
            screen.set_scrollback(history - index);
            let row = screen.rows(0, cols).next().unwrap_or_default();
            bytes += row.len() + 2;
            history_rows.push_back(row);
            while bytes > MAX_SNAPSHOT / 2 {
                if let Some(row) = history_rows.pop_front() {
                    bytes -= row.len() + 2;
                } else {
                    break;
                }
            }
        }
        if !history_rows.is_empty() {
            for row in history_rows {
                snapshot.extend_from_slice(row.as_bytes());
                snapshot.extend_from_slice(b"\r\n");
            }
            for _ in 1..rows {
                snapshot.extend_from_slice(b"\r\n");
            }
        }
        screen.set_scrollback(0);
    }
    snapshot.extend_from_slice(&screen.state_formatted());
    snapshot
}

fn broadcast(subscribers: &mut Vec<Subscriber>, event: RuntimeEvent) {
    subscribers.retain(
        |subscriber| match subscriber.sender.try_send(event.clone()) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => false,
        },
    );
}

/// Run the native PTY daemon in the foreground.
pub fn serve(socket: &Path, options: RuntimeOptions) -> Result<()> {
    validate_socket_path(socket)?;
    raise_fd_limit();
    let (listener, _socket_guard) = bind_private_socket(socket)?;
    // A new daemon owns no sessions; earlier swap files are unreachable.
    for dir in &options.swap_dirs {
        remove_stale_swap_files(dir);
    }
    let server = Arc::new(RuntimeServer::new(Arc::new(options)));
    if let Some(interval) = server.options.sweep_interval() {
        let sweeper = Arc::clone(&server);
        thread::Builder::new()
            .name("pty-idle-sweeper".to_owned())
            .spawn(move || {
                loop {
                    thread::sleep(interval);
                    if let Err(error) = sweeper.sweep() {
                        tracing::warn!(%error, "idle terminal sweep failed");
                    }
                }
            })
            .context("start idle terminal sweeper")?;
    }
    let active = Arc::new(AtomicUsize::new(0));
    loop {
        let (stream, _) = listener.accept().context("accept runtime connection")?;
        if active
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < MAX_CONNECTIONS).then_some(count + 1)
            })
            .is_err()
        {
            drop(stream);
            continue;
        }
        let server = Arc::clone(&server);
        let active = Arc::clone(&active);
        let spawned = thread::Builder::new()
            .name("runtime-connection".to_owned())
            .stack_size(CONNECTION_THREAD_STACK)
            .spawn(move || {
                let _permit = ConnectionPermit(active);
                if let Err(error) = handle_connection(&server, stream) {
                    tracing::debug!(%error, "runtime client connection ended");
                }
            });
        if let Err(error) = spawned {
            tracing::warn!(%error, "could not start runtime connection thread");
        }
    }
}

struct ConnectionPermit(Arc<AtomicUsize>);

impl Drop for ConnectionPermit {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

fn handle_connection(server: &RuntimeServer, mut stream: UnixStream) -> Result<()> {
    stream.set_read_timeout(Some(RPC_TIMEOUT))?;
    stream.set_write_timeout(Some(RPC_TIMEOUT))?;
    let request_stream = stream.try_clone().context("clone runtime request socket")?;
    let mut reader = BufReader::new(request_stream);
    let request = match read_frame(&mut reader, MAX_FRAME) {
        Ok(Some(frame)) => match serde_json::from_slice::<Request>(&frame) {
            Ok(request) => request,
            Err(error) => {
                write_json_frame(
                    &mut stream,
                    &WireResponse::failure(format!("invalid request: {error}")),
                )?;
                return Ok(());
            }
        },
        Ok(None) => return Ok(()),
        Err(error) => {
            write_json_frame(&mut stream, &WireResponse::failure(error))?;
            return Ok(());
        }
    };

    if let Request::Subscribe { id } = request {
        let result = server.session(&id).and_then(|session| {
            let (hello, subscriber_id, receiver) = session.subscribe()?;
            Ok((session, hello, subscriber_id, receiver))
        });
        match result {
            Ok((session, hello, subscriber_id, receiver)) => {
                if let Err(error) = write_json_frame(&mut stream, &WireResponse::success(hello)?) {
                    session.unsubscribe(subscriber_id);
                    return Err(error);
                }
                loop {
                    match receiver.recv_timeout(Duration::from_millis(200)) {
                        Ok(event) => {
                            if write_json_frame(&mut stream, &event).is_err()
                                || event == RuntimeEvent::Closed
                            {
                                break;
                            }
                        }
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            let mut probe = 0u8;
                            let n = unsafe {
                                libc::recv(
                                    stream.as_raw_fd(),
                                    &mut probe as *mut u8 as *mut libc::c_void,
                                    1,
                                    libc::MSG_PEEK | libc::MSG_DONTWAIT,
                                )
                            };
                            if n >= 0 {
                                break;
                            }
                            let error = io::Error::last_os_error();
                            if !matches!(
                                error.kind(),
                                io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                            ) {
                                break;
                            }
                        }
                    }
                }
                session.unsubscribe(subscriber_id);
                return Ok(());
            }
            Err(error) => {
                write_json_frame(&mut stream, &WireResponse::failure(error))?;
                return Ok(());
            }
        }
    }

    let response = match dispatch(server, request) {
        Ok(value) => WireResponse {
            ok: true,
            result: value,
            error: String::new(),
        },
        Err(error) => WireResponse::failure(error),
    };
    write_json_frame(&mut stream, &response)
}

fn dispatch(server: &RuntimeServer, request: Request) -> Result<Value> {
    match request {
        Request::Create { id, cwd } => {
            server.create(&id, &cwd)?;
            Ok(Value::Null)
        }
        Request::HasSession { id } => {
            serde_json::to_value(server.has_session(&id)?).map_err(Into::into)
        }
        Request::Write { id, data } => {
            if data.len() > MAX_INPUT {
                bail!("terminal input exceeds {MAX_INPUT} bytes")
            }
            server.session(&id)?.write(&data)?;
            Ok(Value::Null)
        }
        Request::Resize { id, cols, rows } => {
            server.session(&id)?.resize(cols, rows)?;
            Ok(Value::Null)
        }
        Request::Stop { id } => {
            server.stop(&id)?;
            Ok(Value::Null)
        }
        Request::Capture { id, lines } => {
            serde_json::to_value(server.session(&id)?.capture(lines)?).map_err(Into::into)
        }
        Request::Info { id } => {
            serde_json::to_value(server.session(&id)?.info()?).map_err(Into::into)
        }
        Request::List => serde_json::to_value(server.list()?).map_err(Into::into),
        Request::Stats => serde_json::to_value(server.stats()?).map_err(Into::into),
        Request::Subscribe { .. } => unreachable!("subscribe handled before dispatch"),
    }
}

fn read_frame(reader: &mut impl BufRead, limit: usize) -> Result<Option<Vec<u8>>> {
    let mut frame = Vec::new();
    loop {
        let available = reader.fill_buf().context("read runtime frame")?;
        if available.is_empty() {
            if frame.is_empty() {
                return Ok(None);
            }
            bail!("unterminated runtime frame")
        }
        if let Some(newline) = available.iter().position(|byte| *byte == b'\n') {
            if frame.len().saturating_add(newline) > limit {
                bail!("runtime frame exceeds {limit} bytes")
            }
            frame.extend_from_slice(&available[..newline]);
            reader.consume(newline + 1);
            return Ok(Some(frame));
        }
        if frame.len().saturating_add(available.len()) > limit {
            bail!("runtime frame exceeds {limit} bytes")
        }
        let count = available.len();
        frame.extend_from_slice(available);
        reader.consume(count);
    }
}

fn write_json_frame(stream: &mut impl Write, value: &impl Serialize) -> Result<()> {
    let encoded = serde_json::to_vec(value).context("encode runtime frame")?;
    if encoded.len() > MAX_FRAME {
        bail!("runtime frame exceeds {MAX_FRAME} bytes")
    }
    stream.write_all(&encoded).context("write runtime frame")?;
    stream.write_all(b"\n").context("finish runtime frame")?;
    stream.flush().context("flush runtime frame")
}

fn validate_session_id(id: &str) -> Result<()> {
    if id.is_empty() || id.len() > MAX_SESSION_ID {
        bail!("terminal session id must contain 1..={MAX_SESSION_ID} bytes")
    }
    if !id
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        bail!("terminal session id contains an invalid character")
    }
    Ok(())
}

fn validate_dimensions(cols: u16, rows: u16) -> Result<()> {
    if !(2..=MAX_COLS).contains(&cols) || !(2..=MAX_ROWS).contains(&rows) {
        bail!("terminal dimensions must be within 2..={MAX_COLS} columns and 2..={MAX_ROWS} rows")
    }
    Ok(())
}

fn validate_working_directory(cwd: &Path) -> Result<()> {
    if !cwd.is_absolute() {
        bail!("terminal working directory must be absolute")
    }
    let metadata = fs::metadata(cwd)
        .with_context(|| format!("inspect terminal working directory {}", cwd.display()))?;
    if !metadata.is_dir() {
        bail!(
            "terminal working directory is not a directory: {}",
            cwd.display()
        )
    }
    Ok(())
}

fn configured_shell() -> Result<PathBuf> {
    let configured = std::env::var_os("SHELL").map(PathBuf::from);
    for shell in configured
        .into_iter()
        .chain([PathBuf::from("/bin/bash"), PathBuf::from("/bin/sh")])
    {
        if shell.is_absolute() {
            if let Ok(metadata) = fs::metadata(&shell) {
                if metadata.is_file() && metadata.permissions().mode() & 0o111 != 0 {
                    return Ok(shell);
                }
            }
        }
    }
    bail!("no valid absolute executable login shell was found")
}

fn validate_socket_path(socket: &Path) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;
    if !socket.is_absolute() {
        bail!("runtime socket path must be absolute")
    }
    if socket.as_os_str().as_bytes().len() >= 100 {
        bail!("runtime Unix socket path is too long: {}", socket.display())
    }
    let parent = socket
        .parent()
        .ok_or_else(|| anyhow!("runtime socket has no parent directory"))?;
    if parent.as_os_str().is_empty() {
        bail!("runtime socket has no parent directory")
    }
    Ok(())
}

fn bind_private_socket(socket: &Path) -> Result<(UnixListener, SocketGuard)> {
    let parent = socket.parent().expect("validated socket has a parent");
    let parent_existed = parent.exists();
    fs::create_dir_all(parent)
        .with_context(|| format!("create runtime socket directory {}", parent.display()))?;
    if !parent_existed {
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("protect runtime socket directory {}", parent.display()))?;
    }

    if let Ok(metadata) = fs::symlink_metadata(socket) {
        if metadata.file_type().is_symlink() {
            bail!(
                "refusing to overwrite runtime socket symlink: {}",
                socket.display()
            )
        }
        if !metadata.file_type().is_socket() {
            bail!(
                "refusing to overwrite non-socket path: {}",
                socket.display()
            )
        }
        match UnixStream::connect(socket) {
            Ok(_) => bail!("runtime socket is already live: {}", socket.display()),
            Err(error) if error.kind() == io::ErrorKind::ConnectionRefused => {
                let current = fs::symlink_metadata(socket).with_context(|| {
                    format!("recheck stale runtime socket {}", socket.display())
                })?;
                if !current.file_type().is_socket()
                    || current.dev() != metadata.dev()
                    || current.ino() != metadata.ino()
                {
                    bail!("runtime socket changed while checking staleness")
                }
                fs::remove_file(socket)
                    .with_context(|| format!("remove stale runtime socket {}", socket.display()))?;
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("probe existing runtime socket {}", socket.display())
                });
            }
        }
    }

    let listener = UnixListener::bind(socket)
        .with_context(|| format!("bind runtime socket {}", socket.display()))?;
    fs::set_permissions(socket, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("protect runtime socket {}", socket.display()))?;
    let metadata = fs::symlink_metadata(socket)?;
    Ok((
        listener,
        SocketGuard {
            path: socket.to_owned(),
            dev: metadata.dev(),
            ino: metadata.ino(),
        },
    ))
}

struct SocketGuard {
    path: PathBuf,
    dev: u64,
    ino: u64,
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        if let Ok(metadata) = fs::symlink_metadata(&self.path) {
            if metadata.file_type().is_socket()
                && metadata.dev() == self.dev
                && metadata.ino() == self.ino
            {
                let _ = fs::remove_file(&self.path);
            }
        }
    }
}

fn wake_pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

fn raise_fd_limit() {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } != 0 {
        return;
    }
    let target = (limit.rlim_max as u64).min(FD_LIMIT_TARGET);
    if (limit.rlim_cur as u64) < target {
        limit.rlim_cur = target as libc::rlim_t;
        if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) } != 0 {
            tracing::warn!(error = %io::Error::last_os_error(), "could not raise descriptor limit");
        }
    }
}

/// Process count per terminal session ID (the shell's PID), from /proc.
#[cfg(target_os = "linux")]
fn session_census() -> Option<HashMap<u32, usize>> {
    let mut counts = HashMap::new();
    for entry in fs::read_dir("/proc").ok()?.flatten() {
        if entry.file_name().to_string_lossy().parse::<u32>().is_err() {
            continue;
        }
        let Ok(stat) = fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        let Some((_, rest)) = stat.rsplit_once(") ") else {
            continue;
        };
        if let Some(session) = rest.split_whitespace().nth(3).and_then(|v| v.parse().ok()) {
            *counts.entry(session).or_insert(0) += 1;
        }
    }
    Some(counts)
}

#[cfg(not(target_os = "linux"))]
fn session_census() -> Option<HashMap<u32, usize>> {
    None
}

fn trim_heap() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    unsafe {
        libc::malloc_trim(0);
    }
}

fn ensure_private_dir(dir: &Path) -> Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("create swap folder {}", dir.display()))?;
    let metadata = fs::symlink_metadata(dir)?;
    if !metadata.is_dir() || metadata.uid() != unsafe { libc::geteuid() } {
        bail!("swap folder {} is not a private directory", dir.display())
    }
    if metadata.mode() & 0o077 != 0 {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn free_bytes(dir: &Path) -> Result<u64> {
    let path = std::ffi::CString::new(dir.as_os_str().as_encoded_bytes())?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(path.as_ptr(), &mut stat) } != 0 {
        return Err(io::Error::last_os_error()).context("statvfs swap folder");
    }
    Ok((stat.f_bavail as u64).saturating_mul(stat.f_frsize as u64))
}

fn try_write_swap_file(dir: &Path, id: &str, data: &[u8]) -> Result<PathBuf> {
    ensure_private_dir(dir)?;
    let needed = data.len() as u64 + SWAP_RESERVE_BYTES;
    if free_bytes(dir)? < needed {
        bail!("swap folder {} is full", dir.display())
    }
    let stem: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect();
    let path = dir.join(format!("{stem}-{}.screen", uuid::Uuid::new_v4().simple()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("create swap file {}", path.display()))?;
    if let Err(error) = file.write_all(data) {
        drop(file);
        let _ = fs::remove_file(&path);
        return Err(error).with_context(|| format!("write swap file {}", path.display()));
    }
    Ok(path)
}

/// Try each swap folder in order (e.g. /tmp, then persistent state) so a
/// full or read-only filesystem does not keep idle screens in RAM.
fn write_swap_file(dirs: &[PathBuf], id: &str, data: &[u8]) -> Result<PathBuf> {
    let mut errors = Vec::new();
    for dir in dirs {
        match try_write_swap_file(dir, id, data) {
            Ok(path) => return Ok(path),
            Err(error) => errors.push(format!("{error:#}")),
        }
    }
    if errors.is_empty() {
        bail!("no swap folder is configured")
    }
    bail!("no swap folder accepted the screen: {}", errors.join("; "))
}

fn remove_stale_swap_files(dir: &Path) {
    let Ok(metadata) = fs::symlink_metadata(dir) else {
        return;
    };
    if !metadata.is_dir() || metadata.uid() != unsafe { libc::geteuid() } {
        return;
    }
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            if entry.path().extension().is_some_and(|ext| ext == "screen") {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
}

fn lock<'a, T>(mutex: &'a Mutex<T>, name: &str) -> Result<std::sync::MutexGuard<'a, T>> {
    mutex.lock().map_err(|_| anyhow!("{name} is poisoned"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Shutdown;
    use std::time::Instant;
    use tempfile::TempDir;

    fn wait_until(timeout: Duration, mut predicate: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if predicate() {
                return true;
            }
            thread::sleep(Duration::from_millis(20));
        }
        predicate()
    }

    fn idle_options(swap: &Path) -> Arc<RuntimeOptions> {
        // The first folder cannot be created, so every test exercises fallback.
        let blocker = swap.join("blocker");
        fs::write(&blocker, b"").unwrap();
        Arc::new(RuntimeOptions {
            idle_timeout: Some(Duration::from_millis(200)),
            swap_dirs: vec![blocker.join("swap"), swap.join("swap")],
        })
    }

    #[test]
    fn more_than_32_sessions_are_allowed() -> Result<()> {
        let temp = TempDir::new()?;
        let server = RuntimeServer::new(Arc::new(RuntimeOptions::default()));
        for index in 0..40 {
            let session = Session::create_with_options(
                format!("pty-many-{index}"),
                temp.path().to_owned(),
                PathBuf::from("/bin/sh"),
                Arc::clone(&server.options),
            )?;
            server
                .registry
                .lock()
                .unwrap()
                .sessions
                .insert(session.id.clone(), session);
        }
        assert_eq!(server.list()?.len(), 40);
        assert_eq!(server.stats()?.sessions, 40);
        for id in server.list()? {
            server.stop(&id)?;
        }
        assert!(server.list()?.is_empty());
        Ok(())
    }

    #[test]
    fn idle_prompt_shell_hibernates_to_disk_and_resumes() -> Result<()> {
        let temp = TempDir::new()?;
        let work = temp.path().join("work");
        fs::create_dir_all(work.join("sub"))?;
        let server = RuntimeServer::new(idle_options(temp.path()));
        let session = Session::create_with_options(
            "pty-idle".to_owned(),
            work.clone(),
            PathBuf::from("/bin/sh"),
            Arc::clone(&server.options),
        )?;
        server
            .registry
            .lock()
            .unwrap()
            .sessions
            .insert(session.id.clone(), Arc::clone(&session));
        session.write(b"cd sub; printf '\\102\\105\\106\\117\\122\\105\\012'\r")?;
        assert!(wait_until(Duration::from_secs(3), || {
            session
                .capture(100)
                .is_ok_and(|text| text.contains("BEFORE"))
        }));
        assert!(wait_until(Duration::from_secs(5), || {
            let _ = server.sweep();
            session
                .info()
                .is_ok_and(|info| info.hibernated && info.swapped && !info.running)
        }));
        let swap = temp.path().join("swap");
        let files = fs::read_dir(&swap)?.count();
        assert_eq!(files, 1);
        assert_eq!(fs::metadata(&swap)?.mode() & 0o777, 0o700);
        assert!(server.list()?.contains(&"pty-idle".to_owned()));
        assert_eq!(server.stats()?.dormant, 1);

        // Reading history does not start a shell.
        assert!(session.capture(100)?.contains("BEFORE"));
        assert!(session.info()?.hibernated);

        session.write(b"pwd; printf '\\101\\106\\124\\105\\122\\012'\r")?;
        assert!(wait_until(Duration::from_secs(3), || {
            session
                .capture(200)
                .is_ok_and(|text| text.contains("AFTER"))
        }));
        let text = session.capture(200)?;
        assert!(text.contains("BEFORE"), "{text}");
        assert!(text.contains("idle shell was hibernated"), "{text}");
        assert!(text.contains("/sub"), "{text}");
        let info = session.info()?;
        assert!(info.running && !info.hibernated && !info.swapped);
        assert_eq!(fs::read_dir(&swap)?.count(), 0);
        server.stop("pty-idle")?;
        Ok(())
    }

    #[test]
    fn busy_idle_terminal_keeps_process_but_moves_screen_to_disk() -> Result<()> {
        let temp = TempDir::new()?;
        let server = RuntimeServer::new(idle_options(temp.path()));
        let session = Session::create_with_options(
            "pty-busy".to_owned(),
            temp.path().to_owned(),
            PathBuf::from("/bin/sh"),
            Arc::clone(&server.options),
        )?;
        server
            .registry
            .lock()
            .unwrap()
            .sessions
            .insert(session.id.clone(), Arc::clone(&session));
        session.write(b"printf '\\123\\124\\101\\122\\124\\012'; sleep 30\r")?;
        assert!(wait_until(Duration::from_secs(3), || {
            session
                .capture(100)
                .is_ok_and(|text| text.contains("START"))
        }));
        let pid = session.info()?.pid;
        assert!(wait_until(Duration::from_secs(5), || {
            let _ = server.sweep();
            session.info().is_ok_and(|info| info.swapped)
        }));
        let info = session.info()?;
        assert!(info.running && !info.hibernated);
        assert_eq!(info.pid, pid);
        let (hello, id, _receiver) = session.subscribe()?;
        assert!(String::from_utf8_lossy(&hello.snapshot).contains("START"));
        session.unsubscribe(id);
        session.stop()?;
        Ok(())
    }

    #[test]
    fn stopping_a_hibernated_terminal_removes_its_swap_file() -> Result<()> {
        let temp = TempDir::new()?;
        let server = RuntimeServer::new(idle_options(temp.path()));
        let session = Session::create_with_options(
            "pty-gone".to_owned(),
            temp.path().to_owned(),
            PathBuf::from("/bin/sh"),
            Arc::clone(&server.options),
        )?;
        server
            .registry
            .lock()
            .unwrap()
            .sessions
            .insert(session.id.clone(), Arc::clone(&session));
        assert!(wait_until(Duration::from_secs(5), || {
            let _ = server.sweep();
            session.info().is_ok_and(|info| info.hibernated)
        }));
        server.stop("pty-gone")?;
        assert!(!server.has_session("pty-gone")?);
        assert_eq!(fs::read_dir(temp.path().join("swap"))?.count(), 0);
        assert!(session.write(b"echo nope\r").is_err());
        Ok(())
    }

    #[test]
    fn swap_falls_back_when_first_folder_is_unusable() -> Result<()> {
        let temp = TempDir::new()?;
        let blocked = temp.path().join("not-a-dir");
        fs::write(&blocked, b"")?;
        let path = write_swap_file(
            &[blocked.join("swap"), temp.path().join("ok")],
            "pty-x",
            b"data",
        )?;
        assert!(path.starts_with(temp.path().join("ok")));
        assert_eq!(fs::read(&path)?, b"data");
        Ok(())
    }

    #[test]
    fn canonical_snapshot_preserves_unicode_alternate_screen_and_modes() {
        let mut parser = vt100::Parser::new(4, 20, 20);
        parser.process(b"normal\r\n\x1b[?1049h\x1b[?2004h\xe7\x95");
        parser.process(b"\x8c\xf0\x9f\x99\x82");
        let snapshot = canonical_snapshot(&parser);
        let mut restored = vt100::Parser::new(4, 20, 20);
        restored.process(&snapshot);
        assert!(restored.screen().alternate_screen());
        assert!(restored.screen().bracketed_paste());
        assert_eq!(restored.screen().contents(), parser.screen().contents());
        assert!(restored.screen().contents().contains("界🙂"));
    }

    #[test]
    fn real_pty_input_output_capture_and_resize() -> Result<()> {
        let temp = TempDir::new()?;
        let session = Session::create_with_shell(
            "pty-real".to_owned(),
            temp.path().to_owned(),
            PathBuf::from("/bin/sh"),
        )?;
        session.resize(91, 37)?;
        session.write(b"stty -echo; printf '\\122\\125\\116\\137\\117\\113\\012'\r")?;
        assert!(wait_until(Duration::from_secs(3), || {
            session
                .capture(100)
                .is_ok_and(|text| text.contains("RUN_OK"))
        }));
        let info = session.info()?;
        assert_eq!((info.cols, info.rows), (91, 37));
        assert!(info.pid.is_some());
        session.stop()?;
        Ok(())
    }

    #[test]
    fn dropping_subscription_detaches_without_stopping_shell() -> Result<()> {
        let temp = TempDir::new()?;
        let session = Session::create_with_shell(
            "pty-detach".to_owned(),
            temp.path().to_owned(),
            PathBuf::from("/bin/sh"),
        )?;
        let (_, subscriber_id, receiver) = session.subscribe()?;
        drop(receiver);
        session.unsubscribe(subscriber_id);
        session.write(b"printf '\\104\\105\\124\\101\\103\\110\\137\\117\\113\\012'\r")?;
        assert!(wait_until(Duration::from_secs(3), || {
            session
                .capture(100)
                .is_ok_and(|text| text.contains("DETACH_OK"))
        }));
        assert!(session.info()?.running);
        session.stop()?;
        Ok(())
    }

    #[test]
    fn explicit_stop_is_permanent() -> Result<()> {
        let temp = TempDir::new()?;
        let session = Session::create_with_shell(
            "pty-stop".to_owned(),
            temp.path().to_owned(),
            PathBuf::from("/bin/sh"),
        )?;
        session.stop()?;
        thread::sleep(Duration::from_millis(1200));
        assert!(!session.info()?.running);
        assert!(session.write(b"echo nope\r").is_err());
        assert!(session.stopped.load(Ordering::Acquire));
        Ok(())
    }

    #[test]
    fn malformed_and_oversized_requests_are_rejected() -> Result<()> {
        let server = Arc::new(RuntimeServer::new(Arc::new(RuntimeOptions::default())));
        let (mut client, daemon) = UnixStream::pair()?;
        let server_thread = Arc::clone(&server);
        let handle = thread::spawn(move || handle_connection(&server_thread, daemon));
        client.write_all(b"{not json}\n")?;
        client.shutdown(Shutdown::Write)?;
        let frame = read_frame(&mut BufReader::new(client), MAX_FRAME)?.unwrap();
        let response: WireResponse = serde_json::from_slice(&frame)?;
        assert!(!response.ok);
        handle.join().unwrap()?;

        let (mut client, daemon) = UnixStream::pair()?;
        let handle = thread::spawn(move || handle_connection(&server, daemon));
        // The daemon may reject and close before the whole oversized frame
        // is written; a broken pipe here is the expected rejection race.
        let sent = client
            .write_all(&vec![b'x'; MAX_FRAME + 1])
            .and_then(|()| client.write_all(b"\n"));
        if let Err(error) = sent {
            assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        }
        let _ = client.shutdown(Shutdown::Write);
        let frame = read_frame(&mut BufReader::new(client), MAX_FRAME)?.unwrap();
        let response: WireResponse = serde_json::from_slice(&frame)?;
        assert!(!response.ok);
        handle.join().unwrap()?;
        Ok(())
    }

    #[test]
    fn slow_subscriber_is_dropped_without_blocking_output() {
        let mut model = TerminalModel {
            parser: vt100::Parser::new(24, 80, SCROLLBACK_ROWS),
            cols: 80,
            rows: 24,
            running: true,
            pid: Some(1),
            last_exit: None,
            subscribers: Vec::new(),
            next_subscriber: 2,
            bytes_since_rebase: 0,
            swapped: None,
        };
        let (sender, _receiver) = mpsc::sync_channel(SUBSCRIBER_QUEUE);
        model.subscribers.push(Subscriber { id: 1, sender });
        for _ in 0..=SUBSCRIBER_QUEUE {
            model.process_output(b"output\r\n");
        }
        assert!(model.subscribers.is_empty());
    }
    #[test]
    fn canonical_snapshot_preserves_scrollback_and_visible_screen() {
        let mut parser = vt100::Parser::new(4, 30, SCROLLBACK_ROWS);
        for line in 0..18 {
            parser.process(format!("history-{line:02}\r\n").as_bytes());
        }
        let snapshot = canonical_snapshot(&parser);
        let mut restored = vt100::Parser::new(4, 30, SCROLLBACK_ROWS);
        restored.process(&snapshot);
        assert_eq!(restored.screen().contents(), parser.screen().contents());
        parser.screen_mut().set_scrollback(usize::MAX);
        restored.screen_mut().set_scrollback(usize::MAX);
        assert_eq!(restored.screen().scrollback(), parser.screen().scrollback());
        assert_eq!(restored.screen().contents(), parser.screen().contents());
        assert!(restored.screen().contents().contains("history-00"));
    }

    #[test]
    fn blocked_input_does_not_block_explicit_stop() -> Result<()> {
        let temp = TempDir::new()?;
        let session = Session::create_with_shell(
            "pty-backpressure".into(),
            temp.path().into(),
            "/bin/sh".into(),
        )?;
        session.write(b"stty -icanon -echo; printf READY > ready; sleep 30\r")?;
        assert!(wait_until(Duration::from_secs(3), || fs::read_to_string(
            temp.path().join("ready")
        )
        .is_ok_and(|v| v == "READY")));
        let input_session = session.clone();
        let writer = thread::spawn(move || input_session.write(&vec![b'x'; MAX_INPUT]));
        thread::sleep(Duration::from_millis(150));
        let start = Instant::now();
        session.stop()?;
        assert!(start.elapsed() < Duration::from_secs(5));
        assert!(writer.join().unwrap().is_err());
        assert!(!session.info()?.running);
        Ok(())
    }

    #[test]
    fn idle_disconnected_subscriber_releases_its_slot() -> Result<()> {
        let temp = TempDir::new()?;
        let server = Arc::new(RuntimeServer::new(Arc::new(RuntimeOptions::default())));
        server.create("pty-idle", temp.path())?;
        for _ in 0..(MAX_SUBSCRIBERS_PER_SESSION + 2) {
            let (mut client, daemon) = UnixStream::pair()?;
            client.set_read_timeout(Some(Duration::from_secs(3)))?;
            let server_thread = server.clone();
            let handler = thread::spawn(move || handle_connection(&server_thread, daemon));
            write_json_frame(
                &mut client,
                &Request::Subscribe {
                    id: "pty-idle".into(),
                },
            )?;
            let mut reader = BufReader::new(client);
            let hello = read_frame(&mut reader, MAX_FRAME)?.expect("subscription hello");
            let response: WireResponse = serde_json::from_slice(&hello)?;
            assert!(response.ok, "{}", response.error);
            reader.get_ref().shutdown(Shutdown::Both)?;
            handler.join().unwrap()?;
            assert!(
                server
                    .session("pty-idle")?
                    .model
                    .lock()
                    .unwrap()
                    .subscribers
                    .is_empty()
            );
        }
        server.stop("pty-idle")?;
        Ok(())
    }

    #[test]
    fn subscription_keeps_events_coalesced_with_handshake() -> Result<()> {
        let temp = TempDir::new()?;
        let socket = temp.path().join("runtime.sock");
        let listener = UnixListener::bind(&socket)?;
        let server = thread::spawn(move || -> Result<()> {
            let (stream, _) = listener.accept()?;
            let mut reader = BufReader::new(stream);
            let _ = read_frame(&mut reader, MAX_FRAME)?.unwrap();
            let hello = WireResponse::success(SubscribeHello {
                cols: 80,
                rows: 24,
                snapshot: b"snapshot".to_vec(),
            })?;
            let output = RuntimeEvent::Output {
                data: b"first-event".to_vec(),
            };
            let mut bytes = serde_json::to_vec(&hello)?;
            bytes.push(b'\n');
            bytes.extend(serde_json::to_vec(&output)?);
            bytes.push(b'\n');
            reader.get_mut().write_all(&bytes)?;
            Ok(())
        });
        let mut sub = RuntimeClient::new(&socket)?.subscribe("pty-test")?;
        assert_eq!(sub.snapshot, b"snapshot");
        assert_eq!(
            sub.read_event()?,
            Some(RuntimeEvent::Output {
                data: b"first-event".to_vec()
            })
        );
        server.join().unwrap()?;
        Ok(())
    }
}
