//! Real Bash control-plane execution. Native CLI commands never recurse into MCP.
//! Controls use no persistent PTY slots; ordinary Bash uses the durable PTY runner.
use crate::{config::Config, webterm_cmd};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::{
    fs,
    io::{Read, Write},
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\"'\"'"))
}

fn native_command(text: &str) -> bool {
    let mut words = text.split_whitespace();
    let first = words.next();
    match first {
        Some(
            "webterm" | "help" | "status" | "new" | "create" | "read" | "capture" | "write" | "run"
            | "list" | "ensure" | "resize" | "stop",
        ) => true,
        Some("ls") => matches!(words.next(), Some("terminals" | "workspaces")),
        Some("python") => words.next().is_some_and(|w| w.starts_with('/')),
        _ => false,
    }
}

pub fn execute(config: &Config, cmd: &str) -> Result<Value> {
    execute_with_text(config, cmd, None)
}

pub fn execute_with_text(config: &Config, cmd: &str, text: Option<&str>) -> Result<Value> {
    execute_with_metadata(config, cmd, text, None, None, None)
}

pub fn execute_with_metadata(
    config: &Config,
    cmd: &str,
    text: Option<&str>,
    workspace: Option<&str>,
    task: Option<&str>,
    summary: Option<&str>,
) -> Result<Value> {
    let context = crate::call_context::CallContext::new(config, workspace, task, summary)?;
    let context_json = serde_json::to_string(&context)?;
    let literal_command = text
        .map(|text| webterm_cmd::with_text(cmd, text))
        .transpose()?;
    if cmd.trim().is_empty() || cmd.len() > webterm_cmd::MAX_CMD_BYTES || cmd.contains('\0') {
        bail!("cmd must be nonempty Bash text of at most 98304 UTF-8 bytes without NUL");
    }
    let mut safe = config.clone();
    safe.auth_token_file = None;
    safe.auth_token = None;
    safe.web_password = None;
    safe.web_password_hash_file = None;
    let config_text = toml::to_string(&safe)?;
    let exe = std::env::current_exe()?;
    let cwd = context
        .workspace
        .as_ref()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            config
                .workspace_roots
                .iter()
                .find(|p| p.is_dir())
                .cloned()
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| "/".into()))
        });
    if literal_command.is_none() && !native_command(cmd) {
        if cmd.len() > 32000 {
            bail!(
                "ordinary Bash scripts must be at most 32000 bytes; save larger scripts in a workspace file"
            );
        }
        config.ensure_state_dirs()?;
        let file = config.database_path.with_extension("shell.toml");
        private_file(&file, config_text.as_bytes())?;
        let prelude = format!(
            "set -o pipefail\nexport WEBTERM_CALL_CONTEXT={}\nwebterm() {{ command {} --config {} _native \"$@\"; }}\n{}",
            quote(&context_json),
            quote(&exe.to_string_lossy()),
            quote(&file.to_string_lossy()),
            cmd
        );
        return crate::mcp::durable_shell(config, &prelude, &cwd);
    }
    let command = if cmd.split_whitespace().next() == Some("webterm") {
        cmd.to_owned()
    } else {
        format!("webterm {cmd}")
    };
    let helper = format!(
        "{}\n{}",
        include_str!("terminal_filter.py")
            .split("if __name__ == '__main__':")
            .next()
            .unwrap(),
        include_str!("bash_control.py")
    );
    let request = serde_json::to_vec(
        &json!({"command":command,"literal_command":literal_command,"config":config_text,"exe":exe,"cwd":cwd,"context":context}),
    )?;
    let mut child = Command::new("python3")
        .args(["-c", &helper])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("start Bash control helper")?;
    let stdout = child.stdout.take().context("helper stdout")?;
    let stderr = child.stderr.take().context("helper stderr")?;
    let stderr_reader = std::thread::spawn(move || crate::output_preview::drain_stderr(stderr));
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout
            .take(4 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map(|_| bytes)
    });
    let mut stdin = child.stdin.take().context("helper stdin")?;
    if let Err(e) = stdin.write_all(&request) {
        let _ = child.kill();
        let _ = child.wait();
        drop(stdin);
        let _ = reader.join();
        let diagnostic = stderr_reader
            .join()
            .ok()
            .and_then(|result| result.ok())
            .unwrap_or_default();
        return Err(e).context(format!("helper input failed; stderr: {diagnostic}"));
    }
    drop(stdin);
    let began = Instant::now();
    let exit = loop {
        if let Some(exit) = child.try_wait()? {
            break exit;
        }
        if began.elapsed() > Duration::from_secs(30) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = reader.join();
            let stderr = stderr_reader
                .join()
                .ok()
                .and_then(|result| result.ok())
                .unwrap_or_default();
            bail!(
                "Bash control helper exceeded its deadline; do not rerun automatically\n[stderr]\n{stderr}"
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let bytes = reader
        .join()
        .map_err(|_| anyhow::anyhow!("Bash output reader failed"))??;
    let stderr = stderr_reader
        .join()
        .map_err(|_| anyhow::anyhow!("stderr reader failed"))??;
    if !exit.success() || bytes.len() > 4 * 1024 * 1024 {
        bail!("Bash control helper failed or returned excessive data\n[stderr]\n{stderr}");
    }
    let value: Value = serde_json::from_slice(&bytes)
        .with_context(|| format!("decode Bash control result\n[stderr]\n{stderr}"))?;
    if let Some(e) = value.get("native_error").and_then(Value::as_str) {
        bail!("{e}");
    }
    Ok(value)
}

fn private_file(path: &Path, data: &[u8]) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let temp = path.with_extension(format!("{}.new", uuid::Uuid::new_v4()));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)?;
    file.write_all(data)?;
    file.sync_all()?;
    fs::rename(temp, path)?;
    Ok(())
}

fn receipt(value: &Value, text: &str, code: i32) -> Result<()> {
    // Optional out-of-band metadata for one native command. It is not an auth boundary.
    let Some(dir) = std::env::var_os("WEBTERM_RECEIPT_DIR") else {
        return Ok(());
    };
    let dir = Path::new(&dir);
    use std::os::unix::fs::OpenOptionsExt;
    let mut events = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(dir.join("events"))?;
    if events.metadata()?.len() < 2 {
        events.write_all(b"+")?;
    }
    private_file(
        &dir.join("receipt.json"),
        serde_json::to_string(&json!({"value":value,"stdout":text,"exit_status":code}))?.as_bytes(),
    )
}

/// Render native CLI output for Bash: read emits raw retained text; ls emits rows.
/// --json requests structured output. MCP preserves metadata for a single unfiltered command.
pub fn native(config: &Config, args: &[String]) -> Result<()> {
    if args.first().is_some_and(|a| a == "_native_text") {
        if args.len() != 2 {
            bail!("internal text command requires its private payload file");
        }
        let file = fs::File::open(&args[1])?;
        let mut payload = String::new();
        file.take((webterm_cmd::MAX_CMD_BYTES + 1) as u64)
            .read_to_string(&mut payload)?;
        if payload.len() > webterm_cmd::MAX_CMD_BYTES {
            bail!("text command exceeds its size limit");
        }
        return native(config, &["cmd".into(), payload]);
    }

    let args = if args.first().is_some_and(|a| a == "_native") {
        &args[1..]
    } else {
        args
    };
    let legacy = args.first().is_some_and(|a| a == "cmd");
    let text = if legacy {
        if args.len() != 2 {
            bail!("webterm cmd requires one quoted legacy command string");
        }
        args[1].clone()
    } else {
        webterm_cmd::from_argv(args)
    };
    let result = (|| -> Result<(Value, String, i32)> {
        let parsed = webterm_cmd::parse(&text)?;
        if parsed.op == "read" && !parsed.json && !legacy {
            let mut full_args = args.to_vec();
            // Render raw output before pipes. Explicit budgets remain opt-in.
            if !parsed.full
                && parsed.max_chars == crate::output_preview::DEFAULT_CHARS
                && !args.iter().any(|v| v.starts_with("--max-chars"))
            {
                full_args.push("--full".into());
            }
            // Snapshot comparison uses the client's output budget, not raw pipe input size.
            let mut cleaned = Vec::new();
            let mut i = 0;
            while i < full_args.len() {
                if full_args[i] == "--if-changed" || full_args[i] == "--wait" {
                    i += 2;
                    continue;
                }
                if full_args[i].starts_with("--if-changed=") || full_args[i].starts_with("--wait=")
                {
                    i += 1;
                    continue;
                }
                cleaned.push(full_args[i].clone());
                i += 1;
            }
            let mut raw = crate::mcp::execute_cmd(config, &webterm_cmd::from_argv(&cleaned))?;
            let mut snapshot_raw = raw.clone();
            snapshot_raw["output_chars"] = raw["chars"].clone();
            let mut value = webterm_cmd::compact_output(&snapshot_raw, &parsed);
            if parsed.wait > 0.0
                && raw["running"] == true
                && (parsed.if_changed.is_none() || value.get("unchanged") == Some(&json!(true)))
            {
                let mut waiting = cleaned.clone();
                waiting.extend([
                    "--wait".into(),
                    parsed.wait.to_string(),
                    "--if-changed".into(),
                    raw["snapshot"].as_str().context("read snapshot")?.into(),
                ]);
                let updated = crate::mcp::execute_cmd(config, &webterm_cmd::from_argv(&waiting))?;
                if updated.get("unchanged") != Some(&json!(true)) {
                    raw = updated;
                }
                snapshot_raw = raw.clone();
                snapshot_raw["output_chars"] = raw["chars"].clone();
                value = webterm_cmd::compact_output(&snapshot_raw, &parsed);
            }
            let stdout = if value.get("unchanged") == Some(&json!(true)) {
                String::new()
            } else {
                raw["output"].as_str().unwrap_or("").to_owned()
            };
            return Ok((value, stdout, 0));
        }
        let value = crate::mcp::execute_cmd(config, &text)?;
        let stdout = if parsed.op == "ls" && !parsed.json && !legacy {
            let mut rows = String::new();
            if let Some(items) = value.get("terminals").and_then(Value::as_array) {
                for t in items {
                    rows.push_str(&format!(
                        "{}\t{}\t{}\t{}\n",
                        t["terminal_id"],
                        t["status"].as_str().unwrap_or(""),
                        t.get("workspace_id")
                            .unwrap_or(&value["workspace_id"])
                            .as_str()
                            .unwrap_or(""),
                        t["name"].as_str().unwrap_or("").replace(['\t', '\n'], " ")
                    ));
                }
            } else if let Some(items) = value.get("workspaces").and_then(Value::as_array) {
                for w in items {
                    rows.push_str(&format!(
                        "{}\t{}\n",
                        w["workspace_id"].as_str().unwrap_or(""),
                        w["name"].as_str().unwrap_or("").replace(['\t', '\n'], " ")
                    ));
                }
            }
            rows
        } else {
            format!("{value}\n")
        };
        let code = if matches!(parsed.op.as_str(), "run" | "python") && value["running"] == false {
            value["exit_code"]
                .as_i64()
                .filter(|n| (0..=255).contains(n))
                .unwrap_or(0) as i32
        } else {
            0
        };
        Ok((value, stdout, code))
    })();
    match result {
        Ok((value, text, code)) => {
            receipt(&value, &text, code)?;
            match std::io::stdout().write_all(text.as_bytes()) {
                Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {}
                result => result?,
            }
            if code != 0 {
                std::process::exit(code);
            }
            Ok(())
        }

        Err(e) => {
            let _ = receipt(&json!({"native_error":format!("{e:#}")}), "", 1);
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn routes_native_and_shell_without_interpreting_pipes() {
        assert!(native_command("webterm read 1 | grep a"));
        assert!(native_command("read 1 | sed -n '2p'"));
        assert!(!native_command("ls -la"));
        assert!(!native_command("python -c 'print(1)'"));
        assert!(native_command("webterm\tread 1"));
        assert!(!native_command("printf '%s' text"));
        assert!(!native_command("cd /tmp && webterm status"));
    }
    #[test]
    fn rejects_invalid_source_before_starting() {
        assert!(execute(&Config::default(), " ").is_err());
        assert!(execute(&Config::default(), "x\0").is_err());
        assert!(execute(&Config::default(), &"x".repeat(98305)).is_err());
    }
}
