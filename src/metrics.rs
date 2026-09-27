use std::{
    fs::File,
    io::{Read, Take},
    path::Path,
    sync::{Mutex, MutexGuard},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use serde::Serialize;

const MAX_PROC_FILE_BYTES: u64 = 64 * 1024;
const SAMPLE_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Clone, Debug, Default, Serialize)]
pub struct SystemMetrics {
    pub cpu_percent: Option<f64>,
    pub memory_used_bytes: Option<u64>,
    pub memory_total_bytes: Option<u64>,
    pub memory_percent: Option<f64>,
}

#[derive(Debug)]
pub struct MetricsSampler {
    state: Mutex<SamplerState>,
}

#[derive(Debug, Default)]
struct SamplerState {
    sampled_at: Option<Instant>,
    previous_cpu: Option<CpuTimes>,
    cached: SystemMetrics,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CpuTimes {
    idle: u64,
    total: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct MemoryValues {
    available_bytes: u64,
    total_bytes: u64,
}

impl MetricsSampler {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(SamplerState::default()),
        }
    }

    /// Returns a process-wide cached sample. Concurrent browser requests share
    /// both the CPU baseline and the one-second sampling interval.
    pub fn sample(&self) -> SystemMetrics {
        let now = Instant::now();
        let mut state = lock(&self.state);
        if state
            .sampled_at
            .is_some_and(|sampled_at| now.saturating_duration_since(sampled_at) < SAMPLE_INTERVAL)
        {
            return state.cached.clone();
        }

        state.cached = sample_system(&mut state.previous_cpu);
        state.sampled_at = Some(now);
        state.cached.clone()
    }
}

impl Default for MetricsSampler {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(target_os = "linux")]
fn sample_system(previous_cpu: &mut Option<CpuTimes>) -> SystemMetrics {
    let cpu_percent = read_bounded(Path::new("/proc/stat"))
        .and_then(|contents| parse_proc_stat(&contents))
        .ok()
        .and_then(|current| {
            let percent = previous_cpu.and_then(|previous| cpu_delta_percent(previous, current));
            *previous_cpu = Some(current);
            percent
        });

    let memory = read_bounded(Path::new("/proc/meminfo"))
        .and_then(|contents| parse_proc_meminfo(&contents))
        .ok();
    let (memory_used_bytes, memory_total_bytes, memory_percent) = memory
        .and_then(|memory| {
            let used = memory.total_bytes.checked_sub(memory.available_bytes)?;
            let percent = (memory.total_bytes != 0)
                .then_some(used as f64 * 100.0 / memory.total_bytes as f64)?;
            Some((Some(used), Some(memory.total_bytes), Some(percent)))
        })
        .unwrap_or((None, None, None));

    SystemMetrics {
        cpu_percent,
        memory_used_bytes,
        memory_total_bytes,
        memory_percent,
    }
}

#[cfg(not(target_os = "linux"))]
fn sample_system(_previous_cpu: &mut Option<CpuTimes>) -> SystemMetrics {
    SystemMetrics::default()
}

fn read_bounded(path: &Path) -> Result<String> {
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut reader: Take<File> = file.take(MAX_PROC_FILE_BYTES + 1);
    let mut bytes = Vec::new();
    reader
        .read_to_end(&mut bytes)
        .with_context(|| format!("read {}", path.display()))?;
    if bytes.len() as u64 > MAX_PROC_FILE_BYTES {
        bail!("{} exceeds bounded metrics input", path.display())
    }
    String::from_utf8(bytes).with_context(|| format!("{} is not UTF-8", path.display()))
}

fn parse_proc_stat(input: &str) -> Result<CpuTimes> {
    if input.len() as u64 > MAX_PROC_FILE_BYTES {
        bail!("/proc/stat input exceeds limit")
    }
    let line = input.lines().next().context("/proc/stat is empty")?;
    if line.len() > 4096 {
        bail!("aggregate CPU line exceeds limit")
    }
    let mut fields = line.split_ascii_whitespace();
    if fields.next() != Some("cpu") {
        bail!("/proc/stat does not start with aggregate CPU counters")
    }

    let mut counters = [0_u64; 8];
    for (index, counter) in counters.iter_mut().enumerate() {
        let Some(value) = fields.next() else {
            if index >= 4 {
                break;
            }
            bail!("aggregate CPU counters are incomplete")
        };
        *counter = value
            .parse::<u64>()
            .with_context(|| format!("invalid aggregate CPU counter {index}"))?;
    }
    let total = counters
        .iter()
        .try_fold(0_u64, |total, value| total.checked_add(*value))
        .context("aggregate CPU counters overflow")?;
    let idle = counters[3]
        .checked_add(counters[4])
        .context("aggregate CPU idle counters overflow")?;
    Ok(CpuTimes { idle, total })
}

fn cpu_delta_percent(previous: CpuTimes, current: CpuTimes) -> Option<f64> {
    let total = current.total.checked_sub(previous.total)?;
    let idle = current.idle.checked_sub(previous.idle)?;
    if total == 0 || idle > total {
        return None;
    }
    Some((total - idle) as f64 * 100.0 / total as f64)
}

fn parse_proc_meminfo(input: &str) -> Result<MemoryValues> {
    if input.len() as u64 > MAX_PROC_FILE_BYTES {
        bail!("/proc/meminfo input exceeds limit")
    }
    let mut total_bytes = None;
    let mut available_bytes = None;
    for line in input.lines() {
        let mut fields = line.split_ascii_whitespace();
        let Some(key) = fields.next() else { continue };
        if !matches!(key, "MemTotal:" | "MemAvailable:") {
            continue;
        }
        let value = fields
            .next()
            .with_context(|| format!("{key} has no value"))?
            .parse::<u64>()
            .with_context(|| format!("{key} has an invalid value"))?;
        if fields.next() != Some("kB") || fields.next().is_some() {
            bail!("{key} must use kB units")
        }
        let bytes = value.checked_mul(1024).context("memory value overflow")?;
        match key {
            "MemTotal:" => total_bytes = Some(bytes),
            "MemAvailable:" => available_bytes = Some(bytes),
            _ => unreachable!(),
        }
    }
    Ok(MemoryValues {
        available_bytes: available_bytes.context("MemAvailable is missing")?,
        total_bytes: total_bytes.context("MemTotal is missing")?,
    })
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_aggregate_cpu_counters_without_double_counting_guests() {
        let parsed =
            parse_proc_stat("cpu  100 20 30 400 50 6 7 8 90 10\ncpu0 50 10 15 200 25 3 4 4 45 5\n")
                .unwrap();
        assert_eq!(
            parsed,
            CpuTimes {
                idle: 450,
                total: 621
            }
        );
    }

    #[test]
    fn computes_cpu_usage_from_counter_deltas() {
        let previous = CpuTimes {
            idle: 400,
            total: 1_000,
        };
        let current = CpuTimes {
            idle: 430,
            total: 1_100,
        };
        assert_eq!(cpu_delta_percent(previous, current), Some(70.0));
        assert_eq!(cpu_delta_percent(current, previous), None);
        assert_eq!(cpu_delta_percent(current, current), None);
    }

    #[test]
    fn parses_available_memory_and_calculates_used_memory() {
        let parsed = parse_proc_meminfo(
            "MemTotal:       1000 kB\nMemFree:         100 kB\nMemAvailable:    600 kB\n",
        )
        .unwrap();
        assert_eq!(parsed.total_bytes, 1_024_000);
        assert_eq!(parsed.available_bytes, 614_400);
        let used = parsed.total_bytes - parsed.available_bytes;
        assert_eq!(used, 409_600);
        assert_eq!(used as f64 * 100.0 / parsed.total_bytes as f64, 40.0);
    }

    #[test]
    fn rejects_unbounded_or_incomplete_proc_input() {
        let oversized = "x".repeat(MAX_PROC_FILE_BYTES as usize + 1);
        assert!(parse_proc_stat(&oversized).is_err());
        assert!(parse_proc_meminfo("MemTotal: 10 kB\n").is_err());
    }
}
