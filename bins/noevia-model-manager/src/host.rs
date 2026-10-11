//! Native read-only host telemetry. Production paths are fixed; fixtures never configure them.
use serde::Serialize;
use std::{
    collections::VecDeque,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, BufReader},
    sync::Mutex,
};

pub const HISTORY_CAP: usize = 450;
pub const RESPONSE_CAP: usize = 180;
pub const PROC_CAP: usize = 64 * 1024;
const INTEGER_CAP: i64 = 1 << 53;
const READ_TIMEOUT: Duration = Duration::from_millis(250);

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct HostPoint {
    pub ts: f64,
    pub cpu_pct: f64,
    pub mem_used_gb: f64,
    pub mem_total_gb: f64,
    pub mem_available_gb: f64,
}
#[derive(Debug, Serialize)]
pub struct HostResponse {
    pub current: Option<HostPoint>,
    pub history: Vec<HostPoint>,
}
#[derive(Default)]
pub struct HostHistory {
    last_cpu: Option<(i64, i64)>,
    points: VecDeque<HostPoint>,
}
fn integer(raw: &str) -> Result<i64, &'static str> {
    if raw.len() > 18 || !raw.is_ascii() {
        return Err("invalid proc integer");
    }
    let value: i64 = raw.parse().map_err(|_| "invalid proc integer")?;
    if !(-INTEGER_CAP..=INTEGER_CAP).contains(&value) {
        return Err("proc integer exceeded limit");
    }
    Ok(value)
}
// Decimal formatting rounds the exact binary float with half-even ties, as Python round does.
fn rounded(value: f64, digits: usize) -> Result<f64, &'static str> {
    if !value.is_finite() {
        return Err("nonfinite host value");
    }
    format!("{value:.digits$}")
        .parse()
        .map_err(|_| "invalid host value")
}
impl HostHistory {
    /// CPU state advances before memory parsing, including when the memory read fails.
    pub fn sample(
        &mut self,
        stat: Result<&str, &'static str>,
        mem: Result<&str, &'static str>,
        ts: f64,
    ) -> Result<HostPoint, &'static str> {
        let stat = stat?;
        if stat.len() > PROC_CAP {
            return Err("proc input exceeded limit");
        }
        let mut words = stat
            .lines()
            .next()
            .ok_or("missing cpu line")?
            .split_whitespace();
        if words.next() != Some("cpu") {
            return Err("missing cpu line");
        }
        let mut total = 0i64;
        let mut idle = 0i64;
        let mut count = 0usize;
        for raw in words {
            if count >= 64 {
                return Err("cpu fields exceeded limit");
            }
            let value = integer(raw)?;
            total = total.checked_add(value).ok_or("cpu total exceeded limit")?;
            if count == 3 || count == 4 {
                idle = idle.checked_add(value).ok_or("cpu idle exceeded limit")?;
            }
            count += 1;
        }
        if count < 4
            || !(-INTEGER_CAP..=INTEGER_CAP).contains(&total)
            || !(-INTEGER_CAP..=INTEGER_CAP).contains(&idle)
        {
            return Err("invalid cpu totals");
        }
        let cpu = match self.last_cpu {
            Some((old_idle, old_total)) if total > old_total => {
                100.0 * (1.0 - (idle - old_idle) as f64 / (total - old_total) as f64)
            }
            _ => 0.0,
        };
        self.last_cpu = Some((idle, total));
        let mem = mem?;
        if mem.len() > PROC_CAP {
            return Err("proc input exceeded limit");
        }
        let mut total_kb = 0i64;
        let mut available_kb = 0i64;
        for line in mem.lines() {
            if let Some((key, rest)) = line.split_once(':') {
                if key == "MemTotal" || key == "MemAvailable" {
                    let value = integer(
                        rest.split_whitespace()
                            .next()
                            .ok_or("missing memory value")?,
                    )?;
                    if key == "MemTotal" {
                        total_kb = value;
                    } else {
                        available_kb = value;
                    }
                }
            }
        }
        if !ts.is_finite() || ts < 0.0 {
            return Err("invalid host timestamp");
        }
        let total_gb = total_kb as f64 / 1024.0 / 1024.0;
        let available_gb = available_kb as f64 / 1024.0 / 1024.0;
        let point = HostPoint {
            ts,
            cpu_pct: rounded(cpu.max(0.0), 1)?,
            mem_used_gb: rounded(total_gb - available_gb, 2)?,
            mem_total_gb: rounded(total_gb, 2)?,
            mem_available_gb: rounded(available_gb, 2)?,
        };
        if self.points.len() == HISTORY_CAP {
            self.points.pop_front();
        }
        self.points
            .try_reserve(1)
            .map_err(|_| "host allocation refused")?;
        self.points.push_back(point.clone());
        Ok(point)
    }
    pub fn response(&self) -> HostResponse {
        HostResponse {
            current: self.points.back().cloned(),
            history: self
                .points
                .iter()
                .skip(self.points.len().saturating_sub(RESPONSE_CAP))
                .cloned()
                .collect(),
        }
    }
    pub fn retained(&self) -> usize {
        self.points.len()
    }
}

async fn read_fixed(path: &std::path::Path, first_line: bool) -> Result<String, &'static str> {
    let file = tokio::fs::File::open(path)
        .await
        .map_err(|_| "proc read failed")?;
    let reader = file.take((PROC_CAP + 1) as u64);
    let mut reader = BufReader::new(reader);
    let mut bytes = Vec::new();
    if first_line {
        reader
            .read_until(b'\n', &mut bytes)
            .await
            .map_err(|_| "proc read failed")?;
    } else {
        reader
            .read_to_end(&mut bytes)
            .await
            .map_err(|_| "proc read failed")?;
    }
    if bytes.len() > PROC_CAP {
        return Err("proc input exceeded limit");
    }
    if first_line {
        if let Some(end) = bytes.iter().position(|b| *b == b'\n') {
            bytes.truncate(end);
        }
    }
    String::from_utf8(bytes).map_err(|_| "proc encoding refused")
}
/// A single sequential task bounds work: timeout ends sampling, never spawning more reads.
/// Drop aborts the sampler immediately; a pending fixed-path filesystem operation has at most
/// one bounded read outstanding in Tokio's blocking pool.
pub struct HostSampler {
    task: tokio::task::JoinHandle<()>,
}
impl Drop for HostSampler {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl HostSampler {
    pub fn start(history: Arc<Mutex<HostHistory>>) -> Self {
        Self::start_with(history, || async {
            let stat = read_fixed(std::path::Path::new("/proc/stat"), true).await;
            let mem = read_fixed(std::path::Path::new("/proc/meminfo"), false).await;
            (stat, mem)
        })
    }
    pub fn start_with<F, Fut>(history: Arc<Mutex<HostHistory>>, mut read: F) -> Self
    where
        F: FnMut() -> Fut + Send + 'static,
        Fut: std::future::Future<
                Output = (Result<String, &'static str>, Result<String, &'static str>),
            > + Send,
    {
        let task = tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(2));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                let Ok((stat, mem)) = tokio::time::timeout(READ_TIMEOUT, read()).await else {
                    return;
                };
                let Ok(ts) = SystemTime::now().duration_since(UNIX_EPOCH) else {
                    continue;
                };
                let _ = history.lock().await.sample(
                    stat.as_deref().map_err(|e| *e),
                    mem.as_deref().map_err(|e| *e),
                    ts.as_secs_f64(),
                );
            }
        });
        Self { task }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn proc_adapter_reads_only_first_cpu_line_and_caps_memory() {
        let dir = tempfile::tempdir().unwrap();
        let stat = dir.path().join("stat");
        let mem = dir.path().join("meminfo");
        std::fs::write(&stat, format!("cpu 1 2 3 4\n{}", "x".repeat(PROC_CAP * 2))).unwrap();
        assert_eq!(read_fixed(&stat, true).await.unwrap(), "cpu 1 2 3 4");
        std::fs::write(&mem, "MemTotal: 1048576 kB\n").unwrap();
        assert_eq!(
            read_fixed(&mem, false).await.unwrap(),
            "MemTotal: 1048576 kB\n"
        );
        std::fs::write(&mem, vec![b'x'; PROC_CAP + 1]).unwrap();
        assert!(read_fixed(&mem, false).await.is_err());
        std::fs::write(&stat, vec![b'x'; PROC_CAP + 1]).unwrap();
        assert!(read_fixed(&stat, true).await.is_err());
        std::fs::write(&mem, [255]).unwrap();
        assert!(read_fixed(&mem, false).await.is_err());
        assert!(read_fixed(&dir.path().join("missing"), false)
            .await
            .is_err());
    }
}
