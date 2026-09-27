use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Serialize, Serializer, ser::SerializeStruct};

use crate::config::Config;

const MIGRATION_1: &str = include_str!("../migrations/001_workspaces.sql");
const MIGRATION_2: &str = include_str!("../migrations/002_terminals.sql");

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Workspace {
    pub id: i64,
    pub name: String,
    pub path: PathBuf,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Terminal {
    pub id: i64,
    pub workspace_id: i64,
    pub name: String,
    pub tmux_session: String,
    pub status: String,
    pub created_at: i64,
    pub updated_at: i64,
}

impl Terminal {
    pub fn session_id(&self) -> &str {
        &self.tmux_session
    }

    pub fn backend(&self) -> &'static str {
        if self.tmux_session.starts_with("pty-") {
            "native-pty"
        } else {
            "legacy-tmux"
        }
    }
}

impl Serialize for Terminal {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut value = serializer.serialize_struct("Terminal", 8)?;
        value.serialize_field("id", &self.id)?;
        value.serialize_field("workspace_id", &self.workspace_id)?;
        value.serialize_field("name", &self.name)?;
        value.serialize_field("session_id", &self.tmux_session)?;
        value.serialize_field("backend", self.backend())?;
        value.serialize_field("status", &self.status)?;
        value.serialize_field("created_at", &self.created_at)?;
        value.serialize_field("updated_at", &self.updated_at)?;
        value.end()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FallbackTerminalReservation {
    pub terminal: Terminal,
    pub should_start: bool,
    pub restarted: bool,
}

pub struct Database {
    connection: Connection,
}

impl Database {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create database directory {}", parent.display()))?;
        }
        let connection =
            Connection::open(path).with_context(|| format!("open database {}", path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600))
                .with_context(|| format!("secure database permissions {}", path.display()))?;
        }
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        // Concurrent first-open PRAGMA journal_mode can return SQLITE_BUSY
        // immediately even with busy_timeout. Retry only lock contention, with
        // one bounded deadline, before acquiring the migration transaction.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match connection.execute_batch(
                "PRAGMA foreign_keys = ON; PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL;",
            ) {
                Ok(()) => break,
                Err(rusqlite::Error::SqliteFailure(error, _))
                    if matches!(
                        error.code,
                        rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
                    ) && std::time::Instant::now() < deadline =>
                {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                Err(error) => return Err(error).context("configure SQLite connection"),
            }
        }
        let mut database = Self { connection };
        database.migrate()?;
        Ok(database)
    }

    pub fn open_config(config: &Config) -> Result<Self> {
        config.ensure_state_dirs()?;
        Self::open(&config.database_path)
    }

    fn migrate(&mut self) -> Result<()> {
        // Acquire the write reservation before reading user_version. A deferred
        // transaction can otherwise observe an old version and then fail with
        // SQLITE_BUSY_SNAPSHOT when another first-open migrator commits.
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .context("acquire database migration lock")?;
        let version: i64 = transaction
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .context("read database schema version")?;
        if version > 2 {
            bail!("database schema version {version} is newer than this binary supports")
        }
        if version < 1 {
            transaction.execute_batch(MIGRATION_1)?;
            transaction.pragma_update(None, "user_version", 1)?;
        }
        if version < 2 {
            transaction.execute_batch(MIGRATION_2)?;
            transaction.pragma_update(None, "user_version", 2)?;
        }
        transaction.commit().context("commit database migrations")?;
        Ok(())
    }

    pub fn create_workspace(&self, name: &str, path: &Path) -> Result<Workspace> {
        validate_workspace_name(name)?;
        let now = unix_timestamp()?;
        self.connection
            .execute(
                "INSERT INTO workspaces (name, path, created_at, updated_at) VALUES (?1, ?2, ?3, ?3)",
                params![name, path_to_text(path)?, now],
            )
            .with_context(|| format!("create workspace {name:?}"))?;
        self.workspace_by_id(self.connection.last_insert_rowid())
    }

    /// Idempotent workspace registration: the canonical folder is the identity.
    /// The UNIQUE(path) index serializes races from HTTP, MCP and CLI clients.
    pub fn ensure_workspace(&self, path: &Path) -> Result<Workspace> {
        let text = path_to_text(path)?;
        let base: String = path
            .file_name()
            .and_then(|v| v.to_str())
            .unwrap_or("workspace")
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || "-_.".contains(*c))
            .take(40)
            .collect();
        let base = if base.is_empty() || base.chars().all(|c| c.is_ascii_digit()) {
            "workspace"
        } else {
            &base
        };
        for attempt in 0..5 {
            if let Some(found) = self
                .connection
                .query_row(
                    "SELECT id,name,path,created_at,updated_at FROM workspaces WHERE path=?1",
                    [&text],
                    workspace_from_row,
                )
                .optional()?
            {
                return Ok(found);
            }
            let name = if attempt == 0 {
                base.to_owned()
            } else {
                format!(
                    "{}-{}",
                    base,
                    &uuid::Uuid::new_v4().simple().to_string()[..8]
                )
            };
            let now = unix_timestamp()?;
            self.connection.execute("INSERT OR IGNORE INTO workspaces(name,path,created_at,updated_at) VALUES(?1,?2,?3,?3)",params![name,text,now])?;
        }
        self.connection
            .query_row(
                "SELECT id,name,path,created_at,updated_at FROM workspaces WHERE path=?1",
                [&text],
                workspace_from_row,
            )
            .context("register workspace path")
    }

    pub fn list_workspaces(&self) -> Result<Vec<Workspace>> {
        let mut statement = self.connection.prepare(
            "SELECT id, name, path, created_at, updated_at FROM workspaces ORDER BY name COLLATE NOCASE",
        )?;
        let rows = statement.query_map([], workspace_from_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn workspace(&self, selector: &str) -> Result<Workspace> {
        if let Ok(id) = selector.parse::<i64>() {
            return self.workspace_by_id(id);
        }
        self.connection
            .query_row(
                "SELECT id, name, path, created_at, updated_at FROM workspaces WHERE name = ?1 COLLATE NOCASE",
                [selector],
                workspace_from_row,
            )
            .optional()?
            .with_context(|| format!("workspace {selector:?} not found"))
    }

    pub fn workspace_by_id(&self, id: i64) -> Result<Workspace> {
        self.connection
            .query_row(
                "SELECT id, name, path, created_at, updated_at FROM workspaces WHERE id = ?1",
                [id],
                workspace_from_row,
            )
            .optional()?
            .with_context(|| format!("workspace {id} not found"))
    }

    pub fn update_workspace(
        &self,
        selector: &str,
        name: Option<&str>,
        path: Option<&Path>,
    ) -> Result<Workspace> {
        if name.is_none() && path.is_none() {
            bail!("provide --name and/or --path")
        }
        let current = self.workspace(selector)?;
        let name = name.unwrap_or(&current.name);
        validate_workspace_name(name)?;
        let path = path.unwrap_or(&current.path);
        let now = unix_timestamp()?;
        self.connection
            .execute(
                "UPDATE workspaces SET name = ?1, path = ?2, updated_at = ?3 WHERE id = ?4",
                params![name, path_to_text(path)?, now, current.id],
            )
            .with_context(|| format!("update workspace {selector:?}"))?;
        self.workspace_by_id(current.id)
    }

    pub fn remove_workspace(&self, selector: &str) -> Result<Workspace> {
        let workspace = self.workspace(selector)?;
        self.connection
            .execute("DELETE FROM workspaces WHERE id = ?1", [workspace.id])?;
        Ok(workspace)
    }

    pub fn reserve_terminal(&self, workspace_id: i64, name: &str) -> Result<Terminal> {
        validate_terminal_name(name)?;
        self.workspace_by_id(workspace_id)?;
        let now = unix_timestamp()?;
        let tmux_session = new_native_session_id();
        self.connection
            .execute(
                "INSERT INTO terminals (workspace_id, name, tmux_session, status, created_at, updated_at)
                 VALUES (?1, ?2, ?3, 'starting', ?4, ?4)",
                params![workspace_id, name, tmux_session, now],
            )
            .with_context(|| format!("create terminal {name:?}"))?;
        self.terminal_by_id(self.connection.last_insert_rowid())
    }

    /// Atomically reserves the first available positive integer name in a workspace.
    ///
    /// The immediate transaction keeps two concurrent browser requests from choosing
    /// the same name between discovery and insertion.
    pub fn reserve_default_terminal(&mut self, workspace_id: i64) -> Result<Terminal> {
        self.workspace_by_id(workspace_id)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .context("acquire default terminal name lock")?;
        let used = {
            let mut statement =
                transaction.prepare("SELECT name FROM terminals WHERE workspace_id = ?1")?;
            let rows = statement.query_map([workspace_id], |row| row.get::<_, String>(0))?;
            rows.filter_map(|row| row.ok())
                .filter_map(|name| name.parse::<u64>().ok())
                .filter(|number| *number > 0)
                .collect::<HashSet<_>>()
        };
        let name = (1..=u64::MAX)
            .find(|number| !used.contains(number))
            .context("no default terminal names remain")?
            .to_string();
        let now = unix_timestamp()?;
        let tmux_session = new_native_session_id();
        transaction
            .execute(
                "INSERT INTO terminals (workspace_id, name, tmux_session, status, created_at, updated_at)
                 VALUES (?1, ?2, ?3, 'starting', ?4, ?4)",
                params![workspace_id, name, tmux_session, now],
            )
            .with_context(|| format!("create default terminal in workspace {workspace_id}"))?;
        let id = transaction.last_insert_rowid();
        transaction
            .commit()
            .context("commit default terminal reservation")?;
        self.terminal_by_id(id)
    }

    /// Reserves the workspace fallback terminal without racing concurrent clients.
    /// A `starting` record is an in-progress lease; only the caller receiving
    /// `should_start` may create the native PTY session.
    pub fn reserve_fallback_terminal(
        &mut self,
        workspace_id: i64,
    ) -> Result<FallbackTerminalReservation> {
        self.workspace_by_id(workspace_id)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .context("acquire fallback terminal lock")?;
        let active = transaction
            .query_row(
                "SELECT id, workspace_id, name, tmux_session, status, created_at, updated_at
                 FROM terminals
                 WHERE workspace_id = ?1 AND status IN ('running', 'starting')
                 ORDER BY CASE status WHEN 'running' THEN 0 ELSE 1 END, id
                 LIMIT 1",
                [workspace_id],
                terminal_from_row,
            )
            .optional()?;
        if let Some(terminal) = active {
            transaction.commit()?;
            return Ok(FallbackTerminalReservation {
                terminal,
                should_start: false,
                restarted: false,
            });
        }

        let existing = transaction
            .query_row(
                "SELECT id, workspace_id, name, tmux_session, status, created_at, updated_at
                 FROM terminals WHERE workspace_id = ?1 AND name = 'term1' COLLATE NOCASE",
                [workspace_id],
                terminal_from_row,
            )
            .optional()?;
        let now = unix_timestamp()?;
        let (id, restarted) = if let Some(terminal) = existing {
            let session_id = new_native_session_id();
            transaction.execute(
                "UPDATE terminals SET tmux_session = ?1, status = 'starting', updated_at = ?2 WHERE id = ?3",
                params![session_id, now, terminal.id],
            )?;
            (terminal.id, true)
        } else {
            let tmux_session = new_native_session_id();
            transaction.execute(
                "INSERT INTO terminals (workspace_id, name, tmux_session, status, created_at, updated_at)
                 VALUES (?1, 'term1', ?2, 'starting', ?3, ?3)",
                params![workspace_id, tmux_session, now],
            )?;
            (transaction.last_insert_rowid(), false)
        };
        let terminal = transaction.query_row(
            "SELECT id, workspace_id, name, tmux_session, status, created_at, updated_at
             FROM terminals WHERE id = ?1",
            [id],
            terminal_from_row,
        )?;
        transaction
            .commit()
            .context("commit fallback terminal reservation")?;
        Ok(FallbackTerminalReservation {
            terminal,
            should_start: true,
            restarted,
        })
    }

    pub fn terminal(&self, workspace_id: i64, selector: &str) -> Result<Terminal> {
        if let Ok(id) = selector.parse::<i64>() {
            let terminal = self.terminal_by_id(id)?;
            if terminal.workspace_id == workspace_id {
                return Ok(terminal);
            }
            bail!("terminal {id} does not belong to workspace {workspace_id}")
        }
        self.connection
            .query_row(
                "SELECT id, workspace_id, name, tmux_session, status, created_at, updated_at
                 FROM terminals WHERE workspace_id = ?1 AND name = ?2 COLLATE NOCASE",
                params![workspace_id, selector],
                terminal_from_row,
            )
            .optional()?
            .with_context(|| format!("terminal {selector:?} not found"))
    }

    pub fn terminal_by_id(&self, id: i64) -> Result<Terminal> {
        self.connection
            .query_row(
                "SELECT id, workspace_id, name, tmux_session, status, created_at, updated_at
                 FROM terminals WHERE id = ?1",
                [id],
                terminal_from_row,
            )
            .optional()?
            .with_context(|| format!("terminal {id} not found"))
    }

    pub fn list_terminals(&self, workspace_id: Option<i64>) -> Result<Vec<Terminal>> {
        let (query, parameter) = match workspace_id {
            Some(id) => (
                "SELECT id, workspace_id, name, tmux_session, status, created_at, updated_at
                 FROM terminals WHERE workspace_id = ?1 ORDER BY name COLLATE NOCASE",
                Some(id),
            ),
            None => (
                "SELECT id, workspace_id, name, tmux_session, status, created_at, updated_at
                 FROM terminals ORDER BY workspace_id, name COLLATE NOCASE",
                None,
            ),
        };
        let mut statement = self.connection.prepare(query)?;
        let rows = if let Some(id) = parameter {
            statement.query_map([id], terminal_from_row)?
        } else {
            statement.query_map([], terminal_from_row)?
        };
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn set_terminal_status(&self, id: i64, status: &str) -> Result<Terminal> {
        if !matches!(status, "starting" | "running" | "stopped") {
            bail!("invalid terminal status {status:?}")
        }
        let changed = self.connection.execute(
            "UPDATE terminals SET status = ?1, updated_at = ?2 WHERE id = ?3",
            params![status, unix_timestamp()?, id],
        )?;
        if changed == 0 {
            bail!("terminal {id} not found")
        }
        self.terminal_by_id(id)
    }

    pub fn rename_terminal(&self, id: i64, name: &str) -> Result<Terminal> {
        validate_terminal_name(name)?;
        let changed = self
            .connection
            .execute(
                "UPDATE terminals SET name = ?1, updated_at = ?2 WHERE id = ?3",
                params![name, unix_timestamp()?, id],
            )
            .with_context(|| format!("rename terminal {id}"))?;
        if changed == 0 {
            bail!("terminal {id} not found")
        }
        self.terminal_by_id(id)
    }

    pub fn delete_terminal(&self, id: i64) -> Result<()> {
        self.connection
            .execute("DELETE FROM terminals WHERE id = ?1", [id])?;
        Ok(())
    }
}

fn new_native_session_id() -> String {
    format!("pty-{}", uuid::Uuid::new_v4().simple())
}

pub fn canonical_workspace_path(config: &Config, path: &Path) -> Result<PathBuf> {
    let canonical = path
        .canonicalize()
        .with_context(|| format!("resolve workspace path {}", path.display()))?;
    if !canonical.is_dir() {
        bail!("workspace path is not a directory: {}", canonical.display())
    }
    for root in canonical_workspace_roots(config)? {
        if canonical.starts_with(&root) {
            return Ok(canonical);
        }
    }
    bail!(
        "workspace path {} is outside configured workspace_roots",
        canonical.display()
    )
}

pub fn canonical_workspace_roots(config: &Config) -> Result<Vec<PathBuf>> {
    if config.workspace_roots.is_empty() {
        bail!("no workspace_roots are configured")
    }
    config
        .workspace_roots
        .iter()
        .map(|root| {
            let canonical = root
                .canonicalize()
                .with_context(|| format!("resolve configured workspace root {}", root.display()))?;
            if !canonical.is_dir() {
                bail!(
                    "configured workspace root is not a directory: {}",
                    canonical.display()
                )
            }
            Ok(canonical)
        })
        .collect()
}

/// Creates `~/project` when it is inside a configured workspace root.
/// Other configured roots are never created implicitly.
pub fn ensure_default_workspace_folder(config: &Config) -> Result<()> {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return Ok(());
    };
    let preferred = home.join("project");
    let preferred_absolute = if preferred.is_absolute() {
        preferred.clone()
    } else {
        std::env::current_dir()?.join(&preferred)
    };
    let allowed_lexically = config.workspace_roots.iter().any(|root| {
        let absolute = if root.is_absolute() {
            root.clone()
        } else {
            std::env::current_dir()
                .map(|cwd| cwd.join(root))
                .unwrap_or_else(|_| root.clone())
        };
        preferred_absolute.starts_with(absolute)
    });
    if allowed_lexically {
        fs::create_dir_all(&preferred)
            .with_context(|| format!("create default workspace folder {}", preferred.display()))?;
        canonical_workspace_path(config, &preferred)?;
    }
    Ok(())
}

fn validate_display_name(name: &str) -> Result<()> {
    let length = name.chars().count();
    if !(1..=64).contains(&length) || name.trim() != name {
        bail!("name must contain 1-64 characters with no leading or trailing whitespace")
    }
    if name.chars().any(char::is_control) {
        bail!("name must not contain control characters")
    }
    Ok(())
}

fn validate_workspace_name(name: &str) -> Result<()> {
    validate_display_name(name)?;
    if name.chars().all(|character| character.is_ascii_digit()) {
        bail!("name must not be purely numeric because numeric selectors are IDs")
    }
    Ok(())
}

fn validate_terminal_name(name: &str) -> Result<()> {
    validate_display_name(name)
}

fn workspace_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Workspace> {
    Ok(Workspace {
        id: row.get(0)?,
        name: row.get(1)?,
        path: PathBuf::from(row.get::<_, String>(2)?),
        created_at: row.get(3)?,
        updated_at: row.get(4)?,
    })
}

fn terminal_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Terminal> {
    Ok(Terminal {
        id: row.get(0)?,
        workspace_id: row.get(1)?,
        name: row.get(2)?,
        tmux_session: row.get(3)?,
        status: row.get(4)?,
        created_at: row.get(5)?,
        updated_at: row.get(6)?,
    })
}

fn path_to_text(path: &Path) -> Result<&str> {
    path.to_str()
        .with_context(|| format!("path is not valid UTF-8: {}", path.display()))
}

fn unix_timestamp() -> Result<i64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before Unix epoch")?
        .as_secs() as i64)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Barrier};

    use tempfile::TempDir;

    use super::*;

    #[test]
    fn workspace_crud_is_case_insensitively_named() {
        let temp = TempDir::new().unwrap();
        let database = Database::open(&temp.path().join("state.db")).unwrap();
        let folder = temp.path().join("folder");
        fs::create_dir(&folder).unwrap();
        let created = database.create_workspace("Alpha", &folder).unwrap();
        assert_eq!(database.workspace("alpha").unwrap(), created);
        let updated = database
            .update_workspace("Alpha", Some("Beta"), None)
            .unwrap();
        assert_eq!(updated.name, "Beta");
        assert_eq!(database.list_workspaces().unwrap().len(), 1);
        database.remove_workspace("Beta").unwrap();
        assert!(database.list_workspaces().unwrap().is_empty());
    }

    #[test]
    fn canonical_path_enforces_roots_and_resolves_symlinks() {
        let temp = TempDir::new().unwrap();
        let allowed = temp.path().join("allowed");
        let denied = temp.path().join("denied");
        fs::create_dir(&allowed).unwrap();
        fs::create_dir(&denied).unwrap();
        let config = Config {
            workspace_roots: vec![allowed.clone()],
            ..Config::default()
        };
        assert_eq!(
            canonical_workspace_path(&config, &allowed).unwrap(),
            allowed
        );
        assert!(canonical_workspace_path(&config, &denied).is_err());
    }

    #[test]
    fn terminals_are_scoped_to_workspace() {
        let temp = TempDir::new().unwrap();
        let database = Database::open(&temp.path().join("state.db")).unwrap();
        let first = database.create_workspace("First", temp.path()).unwrap();
        let second_folder = temp.path().join("second");
        fs::create_dir(&second_folder).unwrap();
        let second = database.create_workspace("Second", &second_folder).unwrap();
        let terminal = database.reserve_terminal(first.id, "shell").unwrap();
        assert_eq!(database.terminal(first.id, "SHELL").unwrap(), terminal);
        assert!(database.terminal(second.id, "shell").is_err());
        assert_eq!(
            database
                .set_terminal_status(terminal.id, "running")
                .unwrap()
                .status,
            "running"
        );
    }

    #[test]
    fn default_terminal_names_fill_the_first_positive_gap() {
        let temp = TempDir::new().unwrap();
        let mut database = Database::open(&temp.path().join("state.db")).unwrap();
        let workspace = database.create_workspace("First", temp.path()).unwrap();
        assert_eq!(
            database
                .reserve_default_terminal(workspace.id)
                .unwrap()
                .name,
            "1"
        );
        database.reserve_terminal(workspace.id, "3").unwrap();
        assert_eq!(
            database
                .reserve_default_terminal(workspace.id)
                .unwrap()
                .name,
            "2"
        );
        database.rename_terminal(1, "renamed").unwrap();
        assert_eq!(
            database
                .reserve_default_terminal(workspace.id)
                .unwrap()
                .name,
            "1"
        );
    }

    #[test]
    fn numeric_workspace_names_are_rejected_but_terminal_names_are_valid() {
        let temp = TempDir::new().unwrap();
        let database = Database::open(&temp.path().join("state.db")).unwrap();
        assert!(database.create_workspace("123", temp.path()).is_err());
        let workspace = database.create_workspace("valid", temp.path()).unwrap();
        assert_eq!(
            database.reserve_terminal(workspace.id, "123").unwrap().name,
            "123"
        );
    }

    #[test]
    fn fallback_terminal_reuses_term1_and_only_one_concurrent_caller_starts() {
        const COUNT: usize = 8;
        let temp = TempDir::new().unwrap();
        let database_path = temp.path().join("state.db");
        let database = Database::open(&database_path).unwrap();
        let workspace = database.create_workspace("Fallback", temp.path()).unwrap();
        drop(database);
        let barrier = Arc::new(Barrier::new(COUNT));
        let mut workers = Vec::new();
        for _ in 0..COUNT {
            let barrier = Arc::clone(&barrier);
            let database_path = database_path.clone();
            workers.push(std::thread::spawn(move || {
                let mut database = Database::open(&database_path).unwrap();
                barrier.wait();
                database.reserve_fallback_terminal(workspace.id).unwrap()
            }));
        }
        let reservations: Vec<_> = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect();
        assert_eq!(
            reservations
                .iter()
                .filter(|reservation| reservation.should_start)
                .count(),
            1
        );
        assert!(reservations.iter().all(|reservation| {
            reservation.terminal.id == reservations[0].terminal.id
                && reservation.terminal.name == "term1"
        }));

        let mut database = Database::open(&database_path).unwrap();
        let terminal = reservations[0].terminal.clone();
        database
            .set_terminal_status(terminal.id, "stopped")
            .unwrap();
        let restarted = database.reserve_fallback_terminal(workspace.id).unwrap();
        assert!(restarted.should_start);
        assert!(restarted.restarted);
        assert_eq!(restarted.terminal.id, terminal.id);
        assert_eq!(
            database.list_terminals(Some(workspace.id)).unwrap().len(),
            1
        );
    }

    #[cfg(unix)]
    #[test]
    fn database_permissions_are_repaired_to_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let temp = TempDir::new().unwrap();
        let path = temp.path().join("state.db");
        fs::write(&path, []).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        drop(Database::open(&path).unwrap());
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    #[test]
    fn stopped_legacy_fallback_becomes_native_without_changing_database_identity() {
        let temp = TempDir::new().unwrap();
        let mut database = Database::open(&temp.path().join("state.db")).unwrap();
        let workspace = database
            .create_workspace("native-migration", temp.path())
            .unwrap();
        let terminal = database.reserve_terminal(workspace.id, "term1").unwrap();
        database
            .connection
            .execute(
                "UPDATE terminals SET tmux_session = 'wt-legacy', status = 'running' WHERE id = ?1",
                [terminal.id],
            )
            .unwrap();
        let live = database.reserve_fallback_terminal(workspace.id).unwrap();
        assert!(!live.should_start);
        assert_eq!(live.terminal.tmux_session, "wt-legacy");
        database
            .set_terminal_status(terminal.id, "stopped")
            .unwrap();
        let restarted = database.reserve_fallback_terminal(workspace.id).unwrap();
        assert!(restarted.should_start && restarted.restarted);
        assert_eq!(restarted.terminal.id, terminal.id);
        assert_eq!(restarted.terminal.workspace_id, workspace.id);
        assert_eq!(restarted.terminal.name, "term1");
        assert!(restarted.terminal.session_id().starts_with("pty-"));
    }
}
