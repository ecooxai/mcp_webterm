//! Delayed, non-recursive script supervisor. Owns only explicitly recorded terminals.
//! The independent PTY daemon owns processes; stopping this watcher leaves apps alive.
use crate::{
    config::Config,
    db::{Database, Terminal, canonical_workspace_path},
    terminal::TerminalManager,
};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const POLL: Duration = Duration::from_millis(500);
const SETTLE: Duration = Duration::from_millis(750);
const RETRY: Duration = Duration::from_secs(5);
const TEMPLATE: &str = include_str!("../examples/autoboot-template.sh");
const MAX_SCRIPT: u64 = 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Fingerprint {
    bytes: u64,
    hash: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Owned {
    terminal_id: i64,
    session_id: String,
    fingerprint: Option<Fingerprint>,
}
#[derive(Default, Serialize, Deserialize)]
struct State {
    version: u32,
    directory: PathBuf,
    seeded: bool,
    entries: BTreeMap<String, Owned>,
}
struct Pending {
    fingerprint: Option<Fingerprint>,
    since: Instant,
}

pub struct Watcher {
    stop: Arc<(Mutex<bool>, Condvar)>,
    worker: Option<JoinHandle<()>>,
}
impl Drop for Watcher {
    fn drop(&mut self) {
        let (flag, wake) = &*self.stop;
        *flag.lock().unwrap_or_else(|e| e.into_inner()) = true;
        wake.notify_all();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
fn waiting(stop: &Arc<(Mutex<bool>, Condvar)>, duration: Duration) -> bool {
    let (flag, wake) = &**stop;
    let guard = flag.lock().unwrap_or_else(|e| e.into_inner());
    let (guard, _) = wake
        .wait_timeout_while(guard, duration, |v| !*v)
        .unwrap_or_else(|e| e.into_inner());
    *guard
}

/// Start only after HTTP listeners have been successfully bound. No runtime on discovery.
pub fn start(config: Config) -> Result<Option<Watcher>> {
    if !config.autoboot_enabled {
        return Ok(None);
    }
    let Some(directory) = config
        .autoboot_dir
        .clone()
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join("project/autoboot")))
    else {
        return Ok(None);
    };
    // Isolated/test services must not create or execute another workspace's scripts.
    if let Err(error) = permitted_directory(&config, &directory) {
        tracing::warn!(directory = %directory.display(), error = %error, "autoboot disabled for directory outside workspace roots");
        return Ok(None);
    }
    let stop = Arc::new((Mutex::new(false), Condvar::new()));
    let signal = stop.clone();
    let worker = thread::Builder::new().name("webterm-autoboot".into()).spawn(move || {
        if waiting(&signal, Duration::from_secs(config.autoboot_delay_seconds)) { return; }
        let mut last_error = String::new();
        // Recover after temporary runtime/permission failures. One live owner per database.
        loop {
            match Manager::open(config.clone(), directory.clone()) {
                Ok(Some(mut manager)) => {
                    tracing::info!(directory = %manager.directory.display(), "autoboot watcher started");
                    loop {
                        if waiting(&signal, POLL) { return; }
                        match manager.tick() {
                            Ok(()) => last_error.clear(),
                            Err(error) => {
                                let message = format!("{error:#}");
                                if message != last_error { tracing::warn!(error = %message, "autoboot scan deferred; existing terminals preserved"); last_error = message; }
                            }
                        }
                    }
                }
                Ok(None) => (), // Another frontend owns the lock; do not duplicate execution.
                Err(error) => {
                    let message = format!("{error:#}");
                    if message != last_error { tracing::warn!(error = %message, "autoboot initialization deferred"); last_error = message; }
                }
            }
            if waiting(&signal, RETRY) { return; }
        }
    }).context("start autoboot watcher")?;
    Ok(Some(Watcher {
        stop,
        worker: Some(worker),
    }))
}

/// Check the nearest existing ancestor before mkdir; never create outside allowed roots.
fn permitted_directory(config: &Config, directory: &Path) -> Result<()> {
    if !directory.is_absolute()
        || directory
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        bail!("autoboot directory must be absolute without .. components");
    }
    let mut ancestor = directory;
    while !ancestor.exists() {
        ancestor = ancestor.parent().context("no existing autoboot ancestor")?;
    }
    canonical_workspace_path(config, ancestor)?;
    Ok(())
}
fn regular_directory(config: &Config, directory: &Path) -> Result<()> {
    if !fs::symlink_metadata(directory)?.file_type().is_dir() {
        bail!("autoboot directory must not be a symlink");
    }
    if canonical_workspace_path(config, directory)? != directory {
        bail!("autoboot directory identity changed");
    }
    Ok(())
}
fn fingerprint(path: &Path) -> Result<Fingerprint> {
    // Open without following a last-component symlink on Unix. Files are never shell-expanded.
    use std::os::unix::fs::OpenOptionsExt;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(target_os = "linux")]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    let mut file = options.open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() || meta.len() > MAX_SCRIPT {
        bail!("autoboot script must be a regular file of at most 1 MiB");
    }
    let mut hash = 0xcbf29ce484222325_u64;
    let mut bytes = 0;
    let mut buffer = [0_u8; 8192];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        bytes += n as u64;
        if bytes > MAX_SCRIPT {
            bail!("autoboot script grew beyond 1 MiB");
        }
        for byte in &buffer[..n] {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
    }
    if file.metadata()?.len() != meta.len() || bytes != meta.len() {
        bail!("autoboot script changed during read");
    }
    Ok(Fingerprint { bytes, hash })
}
#[derive(Default)]
struct Snapshot {
    scripts: BTreeMap<String, Fingerprint>,
    errors: BTreeMap<String, String>,
}
fn snapshot(directory: &Path) -> Result<Snapshot> {
    let mut scan = Snapshot::default();
    for item in fs::read_dir(directory)? {
        let item = item?;
        if item.path().extension().and_then(|s| s.to_str()) != Some("sh") {
            continue;
        }
        let Some(name) = item.file_name().to_str().map(str::to_owned) else {
            tracing::warn!("autoboot skipped a non-UTF-8 script filename");
            continue;
        };
        let kind = match item.file_type() {
            Ok(kind) => kind,
            Err(error) => {
                scan.errors.insert(name, error.to_string());
                continue;
            }
        };
        if !kind.is_file() {
            continue;
        }
        match fingerprint(&item.path()) {
            Ok(value) => {
                scan.scripts.insert(name, value);
            }
            Err(error) => {
                scan.errors.insert(name, format!("{error:#}"));
            }
        }
    }
    Ok(scan)
}
fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

struct Manager {
    config: Config,
    directory: PathBuf,
    state_path: PathBuf,
    state: State,
    _lock: File,
    pending: HashMap<String, Pending>,
    retries: HashMap<String, (Instant, String)>,
}
impl Manager {
    fn open(config: Config, directory: PathBuf) -> Result<Option<Self>> {
        use std::os::unix::fs::OpenOptionsExt;
        permitted_directory(&config, &directory)?;
        config.ensure_state_dirs()?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(config.database_path.with_extension("autoboot.lock"))?;
        match lock.try_lock() {
            Ok(()) => (),
            Err(std::fs::TryLockError::WouldBlock) => return Ok(None),
            Err(error) => return Err(error.into()),
        }
        fs::create_dir_all(&directory)?;
        let directory = canonical_workspace_path(&config, &directory)?;
        regular_directory(&config, &directory)?;
        let state_path = config.database_path.with_extension("autoboot.json");
        let mut state: State = match fs::read(&state_path) {
            Ok(bytes) => {
                if bytes.len() > 1024 * 1024 {
                    bail!("autoboot state too large");
                }
                serde_json::from_slice(&bytes)
                    .context("read autoboot ownership state; refusing to adopt unknown terminals")?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => State {
                version: 1,
                directory: directory.clone(),
                ..State::default()
            },
            Err(e) => return Err(e.into()),
        };
        if state.version != 1 || state.directory != directory {
            bail!(
                "autoboot state directory/version differs; preserve its state and restore the configured directory"
            );
        }
        Database::open_config(&config)?.ensure_workspace(&directory)?;
        if !state.seeded {
            // Never overwrite an existing template, including a user-created symlink.
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o700)
                .open(directory.join("template.sh"))
            {
                Ok(mut file) => {
                    file.write_all(TEMPLATE.as_bytes())?;
                    file.sync_all()?;
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => (),
                Err(e) => return Err(e.into()),
            }
            state.seeded = true;
        }
        let result = Self {
            config,
            directory,
            state_path,
            state,
            _lock: lock,
            pending: HashMap::new(),
            retries: HashMap::new(),
        };
        result.save()?;
        Ok(Some(result))
    }
    fn save(&self) -> Result<()> {
        use std::os::unix::fs::OpenOptionsExt;
        let temp = self
            .state_path
            .with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
        let result = (|| -> Result<()> {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temp)?;
            file.write_all(&serde_json::to_vec_pretty(&self.state)?)?;
            file.sync_all()?;
            fs::rename(&temp, &self.state_path)?;
            File::open(self.state_path.parent().context("state parent")?)?.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(temp);
        }
        result
    }
    fn owned(&self, name: &str, db: &Database) -> Result<Option<Terminal>> {
        let Some(entry) = self.state.entries.get(name) else {
            return Ok(None);
        };
        // Numeric IDs can be reused by SQLite after deletion. Check session AND workspace.
        let candidate = db
            .list_terminals(None)?
            .into_iter()
            .find(|t| t.id == entry.terminal_id);
        let Some(terminal) = candidate else {
            return Ok(None);
        };
        if terminal.session_id() != entry.session_id
            || db.workspace_by_id(terminal.workspace_id)?.path != self.directory
        {
            bail!("autoboot terminal identity changed; refusing to touch an unrelated terminal");
        }
        Ok(Some(terminal))
    }
    fn remove(&mut self, name: &str) -> Result<()> {
        let db = Database::open_config(&self.config)?;
        if let Some(terminal) = self.owned(name, &db)? {
            TerminalManager::new(&self.config)?.stop(terminal.session_id())?;
            db.delete_owned_terminal(terminal.id, terminal.session_id())?;
            tracing::info!(
                script = name,
                terminal_id = terminal.id,
                "autoboot terminal removed"
            );
        }
        self.state.entries.remove(name);
        self.save()
    }
    fn launch(&mut self, name: &str, expected: &Fingerprint) -> Result<()> {
        let db = Database::open_config(&self.config)?;
        let terminals = TerminalManager::new(&self.config)?;
        let workspace = db.ensure_workspace(&self.directory)?;
        let script = self.directory.join(name);
        if !fs::symlink_metadata(&script)?.file_type().is_file()
            || fingerprint(&script)? != *expected
        {
            bail!("script changed before launch; waiting for a stable save");
        }
        let terminal = match self.owned(name, &db)? {
            Some(old) => {
                // Resume exactly once after a frontend restart, but not after a daemon loss.
                if self.state.entries[name].fingerprint.as_ref() == Some(expected)
                    && terminals.has_session(old.session_id())?
                {
                    return Ok(());
                }
                terminals.stop(old.session_id())?;
                db.replace_owned_terminal_session(old.id, old.session_id())?
            }
            None => db.reserve_terminal(workspace.id, name)?, // Name conflicts fail; no user takeover.
        };
        self.state.entries.insert(
            name.to_owned(),
            Owned {
                terminal_id: terminal.id,
                session_id: terminal.session_id().to_owned(),
                fingerprint: None,
            },
        );
        self.save()?; // Persist ownership BEFORE any process side effect.
        let started = (|| -> Result<()> {
            terminals.create(terminal.session_id(), &self.directory)?;
            terminals.resize(terminal.session_id(), 160, 45)?;
            crate::mcp::launch_tracked_command(
                &self.config,
                &terminal,
                "bash",
                &format!(
                    "exec bash -- {}\n",
                    quote(script.to_str().context("script path must be UTF-8")?)
                ),
                false,
            )?;
            db.set_terminal_status(terminal.id, "running")?;
            Ok(())
        })();
        if let Err(error) = started {
            terminals
                .stop(terminal.session_id())
                .context("clean up failed autoboot start")?;
            db.set_terminal_status(terminal.id, "stopped")?;
            return Err(error);
        }
        self.state.entries.get_mut(name).unwrap().fingerprint = Some(expected.clone());
        self.save()?;
        tracing::info!(
            script = name,
            terminal_id = terminal.id,
            "autoboot script started"
        );
        Ok(())
    }
    fn tick(&mut self) -> Result<()> {
        // An unreadable directory is NOT an empty directory: preserve existing apps.
        let scan = match fs::symlink_metadata(&self.directory) {
            Ok(_) => {
                regular_directory(&self.config, &self.directory)?;
                snapshot(&self.directory)?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Snapshot::default(),
            Err(e) => return Err(e.into()),
        };
        let scripts = scan.scripts;
        for (name, message) in &scan.errors {
            if self
                .retries
                .get(name)
                .is_none_or(|(_, previous)| previous != message)
            {
                tracing::warn!(script = %name, error = %message, "autoboot file unreadable; its existing terminal is preserved");
            }
            self.retries
                .insert(name.clone(), (Instant::now(), message.clone()));
        }
        let db = Database::open_config(&self.config)?;
        let terminals = TerminalManager::new(&self.config)?;
        let names: std::collections::BTreeSet<_> = scripts
            .keys()
            .chain(self.state.entries.keys())
            .cloned()
            .collect();
        self.pending
            .retain(|name, _| names.contains(name) && !scan.errors.contains_key(name));
        for name in names {
            if scan.errors.contains_key(&name) {
                continue;
            }
            let desired = scripts.get(&name).cloned();
            let current = self
                .state
                .entries
                .get(&name)
                .and_then(|s| s.fingerprint.clone());
            let absent = !self.state.entries.contains_key(&name);
            let missing_session = if !absent && desired == current {
                let probe = self.owned(&name, &db).and_then(|owned| match owned {
                    Some(t) => terminals.has_session(t.session_id()).map(|live| !live),
                    None => Ok(true),
                });
                match probe {
                    Ok(missing) => missing,
                    Err(error) => {
                        let message = format!("{error:#}");
                        if self
                            .retries
                            .get(&name)
                            .is_none_or(|(_, previous)| previous != &message)
                        {
                            tracing::warn!(script = %name, error = %message, "autoboot ownership/runtime probe deferred");
                        }
                        self.retries.insert(name, (Instant::now(), message));
                        continue;
                    }
                }
            } else {
                false
            };
            let changed = desired != current || absent || missing_session;
            if !changed {
                self.pending.remove(&name);
                self.retries.remove(&name);
                continue;
            }
            let pending = self.pending.entry(name.clone()).or_insert_with(|| Pending {
                fingerprint: desired.clone(),
                since: Instant::now(),
            });
            if pending.fingerprint != desired {
                *pending = Pending {
                    fingerprint: desired.clone(),
                    since: Instant::now(),
                };
            }
            if pending.since.elapsed() < SETTLE {
                continue;
            }
            if self
                .retries
                .get(&name)
                .is_some_and(|(at, _)| at.elapsed() < RETRY)
            {
                continue;
            }
            let result = match &desired {
                Some(fp) => self.launch(&name, fp),
                None => self.remove(&name),
            };
            match result {
                Ok(()) => {
                    self.pending.remove(&name);
                    self.retries.remove(&name);
                }
                Err(error) => {
                    let message = format!("{error:#}");
                    if self
                        .retries
                        .get(&name)
                        .is_none_or(|(_, previous)| previous != &message)
                    {
                        tracing::warn!(script = %name, error = %message, "autoboot action deferred");
                    }
                    self.retries.insert(name, (Instant::now(), message));
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn defaults_are_ten_seconds_and_enabled() {
        let c = Config::default();
        assert!(c.autoboot_enabled);
        assert_eq!(c.autoboot_delay_seconds, 10);
        assert!(c.autoboot_dir.is_none());
    }
    #[test]
    fn scans_only_direct_regular_sh_and_hashes_contents() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        fs::write(p.join("a.sh"), "one").unwrap();
        fs::write(p.join("note.txt"), "no").unwrap();
        fs::create_dir(p.join("nested")).unwrap();
        fs::write(p.join("nested/b.sh"), "no").unwrap();
        fs::create_dir(p.join("directory.sh")).unwrap();
        std::os::unix::fs::symlink(p.join("a.sh"), p.join("link.sh")).unwrap();
        let first = snapshot(p).unwrap().scripts;
        assert_eq!(first.len(), 1);
        fs::write(p.join("a.sh"), "two").unwrap();
        assert_ne!(first["a.sh"], snapshot(p).unwrap().scripts["a.sh"]);
    }
    #[test]
    fn scope_is_checked_before_creation() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = Config::default();
        c.workspace_roots = vec![dir.path().into()];
        assert!(permitted_directory(&c, &dir.path().join("new/autoboot")).is_ok());
        assert!(permitted_directory(&c, Path::new("/outside-autoboot-test/new")).is_err());
        assert!(permitted_directory(&c, &dir.path().join("../outside")).is_err());
    }
    #[test]
    fn singleton_and_template_preservation() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = Config::default();
        c.workspace_roots = vec![dir.path().into()];
        c.database_path = dir.path().join("state/db");
        c.runtime_socket = dir.path().join("r.sock");
        c.tmux_socket = dir.path().join("t.sock");
        let path = dir.path().join("project/autoboot");
        let manager = Manager::open(c.clone(), path.clone()).unwrap().unwrap();
        assert_eq!(
            fs::read_to_string(path.join("template.sh")).unwrap(),
            TEMPLATE
        );
        assert!(Manager::open(c.clone(), path.clone()).unwrap().is_none());
        fs::remove_file(path.join("template.sh")).unwrap();
        drop(manager);
        let _manager = Manager::open(c.clone(), path.clone()).unwrap().unwrap();
        assert!(!path.join("template.sh").exists());
    }
    #[test]
    fn existing_template_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        fs::write(p.join("template.sh"), "user file").unwrap();
        let c = Config {
            workspace_roots: vec![p.into()],
            database_path: p.join("db"),
            runtime_socket: p.join("r.sock"),
            tmux_socket: p.join("t.sock"),
            ..Config::default()
        };
        let _manager = Manager::open(c, p.into()).unwrap().unwrap();
        assert_eq!(
            fs::read_to_string(p.join("template.sh")).unwrap(),
            "user file"
        );
    }
    #[test]
    fn quoted_filenames_are_literal() {
        assert_eq!(quote("a'b.sh"), "'a'\"'\"'b.sh'");
    }
    #[test]
    fn disabled_never_creates_a_worker() {
        assert!(
            start(Config {
                autoboot_enabled: false,
                ..Config::default()
            })
            .unwrap()
            .is_none()
        );
    }
}
