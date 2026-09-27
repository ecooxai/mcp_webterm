//! Isolated bounded Bash filter on a text snapshot; never write to the source PTY.
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

pub fn apply(command: &str, text: &str, cwd: &Path) -> Result<Value> {
    let request = serde_json::to_vec(&json!({"command":command,"text":text,"cwd":cwd}))?;
    let mut child = Command::new("python3")
        .args(["-c", include_str!("terminal_filter.py")])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("start terminal filter (Python 3 and Bash required)")?;
    let mut stdin = child.stdin.take().context("filter stdin")?;
    let stdout = child.stdout.take().context("filter stdout")?;
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout
            .take(4 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map(|_| bytes)
    });
    if let Err(e) = stdin.write_all(&request) {
        let _ = child.kill();
        let _ = child.wait();
        drop(stdin);
        let _ = reader.join();
        return Err(e).context("write filter snapshot");
    }
    drop(stdin);
    let began = Instant::now();
    let exit = loop {
        if let Some(exit) = child.try_wait()? {
            break exit;
        }
        if began.elapsed() > Duration::from_secs(8) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = reader.join();
            bail!("Terminal filter helper timed out");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let bytes = reader
        .join()
        .map_err(|_| anyhow::anyhow!("filter output reader failed"))??;
    if !exit.success() || bytes.len() > 4 * 1024 * 1024 {
        bail!("Terminal filter failed or returned oversized data");
    }
    serde_json::from_slice(&bytes).context("decode terminal filter result")
}
