//! Per-call metadata is passed only to child processes, never process-global state.
use crate::{config::Config, db::canonical_workspace_path, webterm_cmd::Parsed};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::Path;

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CallContext {
    pub workspace: Option<String>,
    pub task: Option<String>,
    pub summary: Option<String>,
}
impl CallContext {
    pub fn new(
        config: &Config,
        workspace: Option<&str>,
        task: Option<&str>,
        summary: Option<&str>,
    ) -> Result<Self> {
        match (task, summary) {
            (Some(task), Some(summary)) => {
                crate::mcp::validate_tracking(&json!(task), &json!(summary))?
            }
            (None, None) => (),
            _ => bail!("task and summary must be supplied together"),
        }
        let workspace = workspace
            .map(|path| -> Result<String> {
                if !path.starts_with('/')
                    || path.chars().count() > 4096
                    || path.chars().any(char::is_control)
                {
                    bail!("workspace must be an absolute folder path without control characters");
                }
                Ok(canonical_workspace_path(config, Path::new(path))?
                    .to_string_lossy()
                    .into_owned())
            })
            .transpose()?;
        Ok(Self {
            workspace,
            task: task.map(str::to_owned),
            summary: summary.map(str::to_owned),
        })
    }
    pub fn from_env(config: &Config) -> Result<Self> {
        match std::env::var("WEBTERM_CALL_CONTEXT") {
            Ok(value) => {
                let raw: Self =
                    serde_json::from_str(&value).context("invalid per-call metadata")?;
                Self::new(
                    config,
                    raw.workspace.as_deref(),
                    raw.task.as_deref(),
                    raw.summary.as_deref(),
                )
            }
            Err(std::env::VarError::NotPresent) => Ok(Self::default()),
            Err(error) => Err(error.into()),
        }
    }
    pub fn apply(&self, config: &Config, cmd: &mut Parsed) -> Result<()> {
        if let Some(workspace) = &self.workspace {
            if !matches!(cmd.op.as_str(), "help" | "status") {
                if let Some(explicit) = cmd.args.get("workspace_id").and_then(|v| v.as_str()) {
                    let path = canonical_workspace_path(config, Path::new(explicit))?;
                    if path != Path::new(workspace) {
                        bail!("command workspace does not match the workspace parameter");
                    }
                }
                cmd.args.insert("workspace_id".into(), json!(workspace));
            }
        }
        // Explicit tool parameters take precedence over legacy inline attribution.
        if self.task.is_some() {
            cmd.task = self.task.clone();
            cmd.summary = self.summary.clone();
        }
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn validates_and_scopes_context_without_global_environment() {
        let root = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.workspace_roots = vec![root.path().to_owned()];
        let path = root.path().to_str().unwrap();
        let context = CallContext::new(
            &config,
            Some(path),
            Some("Read build"),
            Some("40/100 Reading current test progress"),
        )
        .unwrap();
        let mut cmd = crate::webterm_cmd::parse("read 12").unwrap();
        context.apply(&config, &mut cmd).unwrap();
        assert_eq!(cmd.args["workspace_id"], path);
        assert_eq!(cmd.task.as_deref(), Some("Read build"));
        assert!(CallContext::new(&config, Some("relative"), None, None).is_err());
        assert!(CallContext::new(&config, Some("/"), None, None).is_err());
        assert!(CallContext::new(&config, None, Some("Task"), None).is_err());
        assert!(CallContext::new(&config, None, None, Some("50/100 Read")).is_err());
        let other = tempfile::tempdir_in(root.path()).unwrap();
        let mut mismatch =
            crate::webterm_cmd::parse(&format!("read {} 12", other.path().display())).unwrap();
        assert!(context.apply(&config, &mut mismatch).is_err());
    }
}
