use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::{config::Config, runtime::RuntimeClient, tmux::Tmux};

pub const NATIVE_PREFIX: &str = "pty-";
pub const LEGACY_PREFIX: &str = "wt-";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminalBackend {
    NativePty,
    LegacyTmux,
}

impl TerminalBackend {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NativePty => "native-pty",
            Self::LegacyTmux => "legacy-tmux",
        }
    }
}

pub fn backend_for_session(session_id: &str) -> Result<TerminalBackend> {
    if session_id.starts_with(NATIVE_PREFIX) {
        Ok(TerminalBackend::NativePty)
    } else if session_id.starts_with(LEGACY_PREFIX) {
        Ok(TerminalBackend::LegacyTmux)
    } else {
        bail!("unsupported terminal session ID {session_id:?}")
    }
}

/// Routes native sessions to the independent runtime daemon and preserves
/// access to already-running legacy tmux sessions.
#[derive(Clone)]
pub struct TerminalManager {
    native: RuntimeClient,
    legacy: Tmux,
    legacy_socket: PathBuf,
}

impl TerminalManager {
    pub fn new(config: &Config) -> Result<Self> {
        Ok(Self {
            native: RuntimeClient::new(&config.runtime_socket)
                .context("configure native terminal runtime client")?,
            legacy: Tmux::new(&config.tmux_socket).context("configure legacy tmux client")?,
            legacy_socket: config.tmux_socket.clone(),
        })
    }

    pub fn native(&self) -> &RuntimeClient {
        &self.native
    }

    /// New sessions are native-only. A caller cannot accidentally recreate a
    /// stopped historical `wt-` record as a new tmux process.
    pub fn create(&self, session_id: &str, working_directory: &Path) -> Result<()> {
        match backend_for_session(session_id)? {
            TerminalBackend::NativePty => self.native.create(session_id, working_directory),
            TerminalBackend::LegacyTmux => {
                bail!("new terminal sessions must use a {NATIVE_PREFIX} session ID")
            }
        }
    }

    pub fn has_session(&self, session_id: &str) -> Result<bool> {
        match backend_for_session(session_id)? {
            TerminalBackend::NativePty => self.native.has_session(session_id),
            TerminalBackend::LegacyTmux => {
                if !self.legacy_socket.exists() {
                    bail!(
                        "legacy tmux backend is unavailable at {}",
                        self.legacy_socket.display()
                    )
                }
                self.legacy.has_session(session_id)
            }
        }
    }

    pub fn write(&self, session_id: &str, data: &str, enter: bool) -> Result<()> {
        match backend_for_session(session_id)? {
            TerminalBackend::NativePty => self.native.write(session_id, data, enter),
            TerminalBackend::LegacyTmux => {
                self.require_live_legacy(session_id)?;
                self.legacy.write(session_id, data, enter)
            }
        }
    }

    pub fn write_bytes(&self, session_id: &str, data: &[u8]) -> Result<()> {
        match backend_for_session(session_id)? {
            TerminalBackend::NativePty => self.native.write_bytes(session_id, data),
            TerminalBackend::LegacyTmux => {
                let data =
                    std::str::from_utf8(data).context("legacy tmux input must be valid UTF-8")?;
                self.write(session_id, data, false)
            }
        }
    }

    pub fn resize(&self, session_id: &str, columns: u16, rows: u16) -> Result<()> {
        match backend_for_session(session_id)? {
            TerminalBackend::NativePty => self.native.resize(session_id, columns, rows),
            TerminalBackend::LegacyTmux => {
                self.require_live_legacy(session_id)?;
                self.legacy.resize(session_id, columns, rows)
            }
        }
    }

    pub fn stop(&self, session_id: &str) -> Result<()> {
        match backend_for_session(session_id)? {
            TerminalBackend::NativePty => self.native.stop(session_id),
            TerminalBackend::LegacyTmux => {
                if self.legacy.has_session(session_id)? {
                    self.legacy.stop(session_id)?;
                }
                Ok(())
            }
        }
    }

    pub fn capture(&self, session_id: &str, lines: u16) -> Result<String> {
        match backend_for_session(session_id)? {
            TerminalBackend::NativePty => self.native.capture(session_id, lines),
            TerminalBackend::LegacyTmux => {
                self.require_live_legacy(session_id)?;
                self.legacy.capture(session_id, lines)
            }
        }
    }

    pub fn attach(&self, session_id: &str) -> Result<()> {
        match backend_for_session(session_id)? {
            TerminalBackend::NativePty => self.native.attach(session_id),
            TerminalBackend::LegacyTmux => {
                self.require_live_legacy(session_id)?;
                let status = self.legacy.attach(session_id)?;
                if !status.success() {
                    bail!("legacy tmux attach exited with {status}")
                }
                Ok(())
            }
        }
    }

    pub fn follow_client_size(&self, session_id: &str) -> Result<()> {
        match backend_for_session(session_id)? {
            TerminalBackend::NativePty => Ok(()),
            TerminalBackend::LegacyTmux => {
                self.require_live_legacy(session_id)?;
                self.legacy.follow_client_size(session_id)
            }
        }
    }

    fn require_live_legacy(&self, session_id: &str) -> Result<()> {
        if !self.legacy.has_session(session_id)? {
            bail!("legacy terminal process is not running")
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_prefixes_select_only_known_backends() {
        assert_eq!(
            backend_for_session("pty-example").unwrap(),
            TerminalBackend::NativePty
        );
        assert_eq!(
            backend_for_session("wt-example").unwrap(),
            TerminalBackend::LegacyTmux
        );
        assert!(backend_for_session("example").is_err());
    }
}
