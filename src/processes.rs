//! Run the bounded, dependency-free Linux collector outside Tokio's executor.
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    process::{Command, Stdio},
    sync::Mutex,
    time::{Duration, Instant},
};

const COLLECTOR: &str = include_str!("process_monitor.py");
const MAX_OUTPUT: u64 = 4 * 1024 * 1024;
#[derive(Default)]
struct Samples {
    previous: Value,
    data: Value,
    sampled: Option<Instant>,
}
#[derive(Default)]
pub struct ProcessMonitor {
    samples: Mutex<Samples>,
}

impl ProcessMonitor {
    pub fn snapshot(&self) -> Result<Value> {
        let mut sample = self.samples.lock().unwrap_or_else(|e| e.into_inner());
        if sample
            .sampled
            .is_some_and(|at| at.elapsed() < Duration::from_secs(1))
        {
            return Ok(json!({"data": sample.data}));
        }
        let value = collect(json!({"operation":"snapshot", "previous":sample.previous}))?;
        if value.get("error").is_none() {
            sample.previous = value["state"].clone();
            sample.data = value["data"].clone();
            sample.sampled = Some(Instant::now());
        }
        Ok(
            json!({"data":value.get("data"), "error":value.get("error"), "status":value.get("status")}),
        )
    }
}

pub fn collect(mut request: Value) -> Result<Value> {
    request["parent"] = json!(std::process::id());
    let mut child = Command::new("python3")
        .args(["-c", COLLECTOR])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("start process monitor; Python 3 and Linux /proc are required")?;
    let output = child.stdout.take().context("monitor stdout")?;
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        output
            .take(MAX_OUTPUT + 1)
            .read_to_end(&mut bytes)
            .map(|_| bytes)
    });
    let mut input = child.stdin.take().context("monitor stdin")?;
    if let Err(error) = input.write_all(&serde_json::to_vec(&request)?) {
        let _ = child.kill();
        let _ = child.wait();
        let _ = reader.join();
        return Err(error).context("write monitor request");
    }
    drop(input);
    let began = Instant::now();
    let exit = loop {
        if let Some(exit) = child.try_wait()? {
            break exit;
        }
        if began.elapsed() > Duration::from_secs(10) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = reader.join();
            bail!("process monitor timed out; no result is available");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let bytes = reader
        .join()
        .map_err(|_| anyhow::anyhow!("monitor output reader failed"))??;
    if !exit.success() || bytes.len() as u64 > MAX_OUTPUT {
        bail!("process monitor returned an invalid or oversized result");
    }
    serde_json::from_slice(&bytes).context("decode process monitor result")
}
