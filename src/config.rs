use std::{
    env, fs,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub listen: SocketAddr,
    pub database_path: PathBuf,
    pub runtime_socket: PathBuf,
    pub tmux_socket: PathBuf,
    pub workspace_roots: Vec<PathBuf>,
    pub autoboot_enabled: bool,
    pub autoboot_dir: Option<PathBuf>,
    pub autoboot_delay_seconds: u64,
    pub auth_token_file: Option<PathBuf>,
    pub web_password_hash_file: Option<PathBuf>,
    pub web_session_ttl_seconds: u64,
    #[serde(skip_serializing)]
    pub auth_token: Option<String>,
    #[serde(skip_serializing, skip_deserializing)]
    pub web_password: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        let state_dir = env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
            .unwrap_or_else(|| PathBuf::from("."))
            .join("webterm");
        Self {
            listen: SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 10000),
            database_path: state_dir.join("webterm.db"),
            runtime_socket: state_dir.join("runtime.sock"),
            tmux_socket: state_dir.join("tmux.sock"),
            workspace_roots: Vec::new(),
            autoboot_enabled: true,
            autoboot_dir: None,
            autoboot_delay_seconds: 10,
            auth_token_file: None,
            web_password_hash_file: None,
            web_session_ttl_seconds: 28_800,
            auth_token: None,
            web_password: None,
        }
    }
}

impl Config {
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let explicit = path
            .map(PathBuf::from)
            .or_else(|| env::var_os("WEBTERM_CONFIG").map(PathBuf::from));
        let mut config = match explicit {
            Some(path) => {
                let content = fs::read_to_string(&path)
                    .with_context(|| format!("read config {}", path.display()))?;
                toml::from_str(&content)
                    .with_context(|| format!("parse config {}", path.display()))?
            }
            None => Self::default(),
        };

        if let Ok(value) = env::var("WEBTERM_LISTEN") {
            config.listen = value.parse().context("parse WEBTERM_LISTEN")?;
        }
        if let Some(value) = env::var_os("WEBTERM_DATABASE_PATH") {
            config.database_path = value.into();
        }
        if let Some(value) = env::var_os("WEBTERM_RUNTIME_SOCKET") {
            config.runtime_socket = value.into();
        }
        if let Some(value) = env::var_os("WEBTERM_TMUX_SOCKET") {
            config.tmux_socket = value.into();
        }
        if let Ok(value) = env::var("WEBTERM_AUTH_TOKEN") {
            config.auth_token = Some(normalize_token(value, "WEBTERM_AUTH_TOKEN")?);
        } else if let Some(path) = &config.auth_token_file {
            let token = fs::read_to_string(path)
                .with_context(|| format!("read auth token {}", path.display()))?;
            config.auth_token = Some(normalize_token(token, "auth token file")?);
        }
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        if let Some(token) = &self.auth_token
            && token.len() < 24
        {
            bail!("authentication token must be at least 24 characters")
        }
        if !(300..=604_800).contains(&self.web_session_ttl_seconds) {
            bail!("web_session_ttl_seconds must be between 300 and 604800")
        }
        if self.autoboot_delay_seconds > 3600 {
            bail!("autoboot_delay_seconds must be at most 3600");
        }
        if let Some(path) = &self.autoboot_dir {
            if !path.is_absolute()
                || path
                    .components()
                    .any(|p| matches!(p, std::path::Component::ParentDir))
            {
                bail!("autoboot_dir must be an absolute folder without .. components");
            }
        }
        validate_runtime_socket(&self.runtime_socket)?;
        if self.runtime_socket == self.tmux_socket {
            bail!("runtime_socket and tmux_socket must be distinct")
        }
        if self.runtime_socket == self.database_path {
            bail!("runtime_socket and database_path must be distinct")
        }
        Ok(())
    }

    pub fn ensure_state_dirs(&self) -> Result<()> {
        for path in [&self.database_path, &self.runtime_socket, &self.tmux_socket] {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("create state directory {}", parent.display()))?;
            }
        }
        Ok(())
    }
}

fn validate_runtime_socket(socket: &Path) -> Result<()> {
    if !socket.is_absolute() {
        bail!("runtime_socket must be an absolute path")
    }
    #[cfg(unix)]
    if socket.as_os_str().as_encoded_bytes().len() >= 100 {
        bail!(
            "runtime_socket is too long for a Unix socket: {}",
            socket.display()
        )
    }
    Ok(())
}

fn normalize_token(value: String, source: &str) -> Result<String> {
    let value = value.trim().to_owned();
    if value.contains(char::is_whitespace) {
        bail!("{source} contains whitespace")
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_bind_to_all_interfaces() {
        assert!(Config::default().listen.ip().is_unspecified());
        assert_eq!(Config::default().listen.port(), 10000);
    }

    #[test]
    fn rejects_short_token() {
        let config = Config {
            auth_token: Some("too-short".into()),
            ..Config::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn runtime_socket_must_be_absolute_and_distinct() {
        let relative = Config {
            runtime_socket: "runtime.sock".into(),
            ..Config::default()
        };
        assert!(relative.validate().is_err());

        let duplicate = Config {
            runtime_socket: Config::default().tmux_socket,
            ..Config::default()
        };
        assert!(duplicate.validate().is_err());
    }
}
