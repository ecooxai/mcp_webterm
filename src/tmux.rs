use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Output},
};

use anyhow::{Context, Result, bail};

#[derive(Clone, Debug)]
pub struct Tmux {
    socket: PathBuf,
}

impl Tmux {
    pub fn new(socket: &Path) -> Result<Self> {
        if !socket.is_absolute() {
            bail!("tmux_socket must be an absolute path")
        }
        #[cfg(unix)]
        if socket.as_os_str().as_encoded_bytes().len() >= 100 {
            bail!(
                "tmux_socket is too long for a Unix socket: {}",
                socket.display()
            )
        }
        if let Some(parent) = socket.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create tmux socket directory {}", parent.display()))?;
        }
        Ok(Self {
            socket: socket.to_owned(),
        })
    }

    pub fn create(&self, session: &str, working_directory: &Path) -> Result<()> {
        let shell = self.configured_login_shell()?;
        // Keep the pane and session alive if the interactive login shell exits.
        // The delay prevents a broken shell executable from becoming a tight
        // respawn loop. Killing the tmux session also kills this supervisor, so
        // explicit stop/delete operations remain permanent.
        let shell_command = format!(
            "unset TMUX; while :; do {} -l; sleep 1; done",
            shell_quote(&shell)?
        );
        let output = self
            .command()
            .args(["new-session", "-d", "-s", session, "-c"])
            .arg(working_directory)
            .arg(shell_command)
            .output()
            .context("start tmux terminal")?;
        expect_success(output, "start tmux terminal")
    }

    fn configured_login_shell(&self) -> Result<PathBuf> {
        let output = self
            .command()
            .args(["show-options", "-gv", "default-shell"])
            .output()
            .context("query tmux default shell")?;
        if !output.status.success() {
            return expect_success(output, "query tmux default shell").map(|()| PathBuf::new());
        }
        let shell = String::from_utf8(output.stdout)
            .context("tmux default shell is not UTF-8")?
            .trim()
            .to_owned();
        let path = PathBuf::from(&shell);
        if shell.is_empty() || !path.is_absolute() || !path.is_file() {
            bail!("tmux default shell is not an absolute executable file: {shell:?}")
        }
        Ok(path)
    }

    pub fn has_session(&self, session: &str) -> Result<bool> {
        let output = self
            .command()
            .args(["has-session", "-t", &exact_target(session)])
            .output()
            .context("query tmux terminal")?;
        Ok(output.status.success())
    }

    /// `resize-window` switches tmux into manual sizing. A newly attached
    /// browser must follow its PTY again rather than show a fixed-size canvas.
    pub fn follow_client_size(&self, session: &str) -> Result<()> {
        let output = self
            .command()
            .args([
                "set-option",
                "-w",
                "-t",
                &window_target(session),
                "window-size",
                "latest",
            ])
            .output()
            .context("restore automatic tmux window sizing")?;
        expect_success(output, "restore automatic tmux window sizing")
    }

    pub fn attach(&self, session: &str) -> Result<ExitStatus> {
        if !self.has_session(session)? {
            bail!("terminal process is not running")
        }
        self.follow_client_size(session)?;
        self.command()
            .args(["attach-session", "-t", &exact_target(session)])
            .status()
            .context("attach tmux terminal")
    }

    pub fn write(&self, session: &str, data: &str, enter: bool) -> Result<()> {
        if !self.has_session(session)? {
            bail!("terminal process is not running")
        }
        let output = self
            .command()
            .args(["send-keys", "-t", &pane_target(session), "-l", "--", data])
            .output()
            .context("write to tmux terminal")?;
        expect_success(output, "write to tmux terminal")?;
        if enter {
            let output = self
                .command()
                .args(["send-keys", "-t", &pane_target(session), "Enter"])
                .output()
                .context("send Enter to tmux terminal")?;
            expect_success(output, "send Enter to tmux terminal")?;
        }
        Ok(())
    }

    pub fn resize(&self, session: &str, columns: u16, rows: u16) -> Result<()> {
        if columns < 2 || rows < 2 {
            bail!("terminal dimensions must each be at least 2")
        }
        let output = self
            .command()
            .args([
                "resize-window",
                "-t",
                &window_target(session),
                "-x",
                &columns.to_string(),
                "-y",
                &rows.to_string(),
            ])
            .output()
            .context("resize tmux terminal")?;
        expect_success(output, "resize tmux terminal")
    }

    pub fn stop(&self, session: &str) -> Result<()> {
        if !self.has_session(session)? {
            return Ok(());
        }
        let output = self
            .command()
            .args(["kill-session", "-t", &exact_target(session)])
            .output()
            .context("stop tmux terminal")?;
        expect_success(output, "stop tmux terminal")
    }

    pub fn capture(&self, session: &str, lines: u16) -> Result<String> {
        if !self.has_session(session)? {
            bail!("terminal process is not running")
        }
        let output = self
            .command()
            .args([
                "capture-pane",
                "-p",
                "-t",
                &pane_target(session),
                "-S",
                &format!("-{lines}"),
            ])
            .output()
            .context("capture tmux terminal")?;
        if !output.status.success() {
            return expect_success(output, "capture tmux terminal").map(|()| String::new());
        }
        String::from_utf8(output.stdout).context("tmux output is not UTF-8")
    }

    fn command(&self) -> Command {
        let mut command = Command::new("tmux");
        // -N prevents clients from silently starting an unmanaged server. The
        // foreground tmux server is owned by webterm-tmux.service in its own
        // cgroup, so restarting the HTTP service cannot kill terminal panes.
        command.args(["-S"]).arg(&self.socket).arg("-N");
        command
    }
}

fn exact_target(session: &str) -> String {
    format!("={session}")
}

fn window_target(session: &str) -> String {
    format!("={session}:")
}

fn pane_target(session: &str) -> String {
    format!("={session}:")
}

fn shell_quote(value: &Path) -> Result<String> {
    let value = value
        .to_str()
        .with_context(|| format!("login shell path is not valid UTF-8: {}", value.display()))?;
    Ok(format!("'{}'", value.replace('\'', "'\"'\"'")))
}

fn expect_success(output: Output, action: &str) -> Result<()> {
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    bail!("{action} failed: {}", stderr.trim())
}

#[cfg(test)]
mod tests {
    use std::{
        process::{Child as ProcessChild, Stdio},
        thread,
        time::Duration,
    };

    use portable_pty::{CommandBuilder, PtySize, native_pty_system};
    use tempfile::TempDir;

    use super::*;

    struct TestServer(ProcessChild);

    impl Drop for TestServer {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn tmux_available() -> bool {
        Command::new("tmux")
            .arg("-V")
            .stdout(Stdio::null())
            .status()
            .is_ok()
    }

    fn start_server(socket: &Path) -> Option<TestServer> {
        let child = Command::new("tmux")
            .arg("-S")
            .arg(socket)
            .args(["-f", "/dev/null", "-D"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut server = TestServer(child);
        for _ in 0..40 {
            if socket.exists() {
                return Some(server);
            }
            if server.0.try_wait().unwrap().is_some() {
                return None;
            }
            thread::sleep(Duration::from_millis(25));
        }
        None
    }

    fn capture_until(tmux: &Tmux, session: &str, needle: &str) -> String {
        let mut captured = String::new();
        for _ in 0..60 {
            captured = tmux.capture(session, 40).unwrap();
            if captured.contains(needle) {
                return captured;
            }
            thread::sleep(Duration::from_millis(50));
        }
        captured
    }

    fn pane_pid(tmux: &Tmux, session: &str) -> String {
        let output = tmux
            .command()
            .args([
                "display-message",
                "-p",
                "-t",
                &pane_target(session),
                "#{pane_pid}",
            ])
            .output()
            .unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    #[test]
    fn terminal_lifecycle_uses_private_socket() {
        if !tmux_available() {
            return;
        }
        let temp = TempDir::new().unwrap();
        let socket = temp.path().join("tmux.sock");
        let Some(_server) = start_server(&socket) else {
            return;
        };
        let tmux = Tmux::new(&socket).unwrap();
        let session = format!("wt-test-{}", uuid::Uuid::new_v4().simple());
        tmux.create(&session, temp.path()).unwrap();
        assert!(tmux.has_session(&session).unwrap());
        tmux.resize(&session, 90, 30).unwrap();
        tmux.follow_client_size(&session).unwrap();
        let policy = tmux
            .command()
            .args([
                "show-options",
                "-wv",
                "-t",
                &window_target(&session),
                "window-size",
            ])
            .output()
            .unwrap();
        assert!(policy.status.success());
        assert_eq!(String::from_utf8(policy.stdout).unwrap().trim(), "latest");
        tmux.write(&session, "printf WEBTERM_TMUX_TEST", true)
            .unwrap();
        let mut captured = capture_until(&tmux, &session, "WEBTERM_TMUX_TEST");
        assert!(captured.contains("WEBTERM_TMUX_TEST"));
        tmux.write(
            &session,
            "printf 'WEBTERM_TMUX_ENV=<%s>\\n' \"${TMUX-unset}\"",
            true,
        )
        .unwrap();
        captured = capture_until(&tmux, &session, "WEBTERM_TMUX_ENV=<unset>");
        assert!(captured.contains("WEBTERM_TMUX_ENV=<unset>"));
        tmux.stop(&session).unwrap();
        assert!(!tmux.has_session(&session).unwrap());
    }

    #[test]
    fn shell_survives_idle_time_and_attach_client_disconnect() {
        if !tmux_available() {
            return;
        }
        let temp = TempDir::new().unwrap();
        let socket = temp.path().join("tmux.sock");
        let Some(_server) = start_server(&socket) else {
            return;
        };
        let tmux = Tmux::new(&socket).unwrap();
        let session = format!("wt-idle-{}", uuid::Uuid::new_v4().simple());
        tmux.create(&session, temp.path()).unwrap();
        let original_pane_pid = pane_pid(&tmux, &session);

        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut command = CommandBuilder::new("tmux");
        command.arg("-S");
        command.arg(&socket);
        command.arg("-N");
        command.arg("attach-session");
        command.arg("-t");
        command.arg(exact_target(&session));
        command.env("TERM", "xterm-256color");
        let mut client = pair.slave.spawn_command(command).unwrap();
        drop(pair.slave);
        thread::sleep(Duration::from_millis(250));
        client.kill().unwrap();
        client.wait().unwrap();
        drop(pair.master);

        thread::sleep(Duration::from_millis(300));
        assert!(tmux.has_session(&session).unwrap());
        assert_eq!(pane_pid(&tmux, &session), original_pane_pid);
        tmux.write(&session, "printf WEBTERM_AFTER_DETACH", true)
            .unwrap();
        assert!(
            capture_until(&tmux, &session, "WEBTERM_AFTER_DETACH").contains("WEBTERM_AFTER_DETACH")
        );
        tmux.stop(&session).unwrap();
    }

    #[test]
    fn login_shell_restarts_after_exit_but_explicit_stop_is_permanent() {
        if !tmux_available() {
            return;
        }
        let temp = TempDir::new().unwrap();
        let socket = temp.path().join("tmux.sock");
        let Some(_server) = start_server(&socket) else {
            return;
        };
        let tmux = Tmux::new(&socket).unwrap();
        let session = format!("wt-restart-{}", uuid::Uuid::new_v4().simple());
        tmux.create(&session, temp.path()).unwrap();
        let original_pane_pid = pane_pid(&tmux, &session);

        tmux.write(&session, "printf WEBTERM_SHELL_EXITING; exit", true)
            .unwrap();
        assert!(
            capture_until(&tmux, &session, "WEBTERM_SHELL_EXITING")
                .contains("WEBTERM_SHELL_EXITING")
        );
        thread::sleep(Duration::from_millis(1_250));
        assert!(tmux.has_session(&session).unwrap());
        assert_eq!(pane_pid(&tmux, &session), original_pane_pid);
        tmux.write(&session, "printf WEBTERM_SHELL_RESTARTED", true)
            .unwrap();
        assert!(
            capture_until(&tmux, &session, "WEBTERM_SHELL_RESTARTED")
                .contains("WEBTERM_SHELL_RESTARTED")
        );

        tmux.stop(&session).unwrap();
        thread::sleep(Duration::from_millis(1_250));
        assert!(!tmux.has_session(&session).unwrap());
    }
}
