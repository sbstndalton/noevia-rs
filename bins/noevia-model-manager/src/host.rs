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
    /// #1747: iGPU GTT in use (system RAM on an APU, invisible to `docker stats`). Absent when
    /// no card exposes `mem_info_gtt_used`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gtt_used_gb: Option<f64>,
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
            gtt_used_gb: None,
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
    /// #1747: attaches the GTT reading to the newest point (sampled right after it).
    pub fn record_gtt(&mut self, bytes: Option<u64>) {
        if let (Some(point), Some(bytes)) = (self.points.back_mut(), bytes) {
            point.gtt_used_gb = rounded(bytes as f64 / 1024.0 / 1024.0 / 1024.0, 2).ok();
        }
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

/// #1747: total GTT bytes in use across `<drm>/card*/device/mem_info_gtt_used` (connector
/// entries such as `card0-DP-1` are skipped). `None` when no card exposes a readable counter.
pub fn gtt_used_bytes(drm: &std::path::Path) -> Option<u64> {
    let mut total: Option<u64> = None;
    for entry in std::fs::read_dir(drm).ok()?.flatten().take(64) {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(index) = name.strip_prefix("card") else {
            continue;
        };
        if index.is_empty() || !index.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let Ok(raw) = std::fs::read_to_string(entry.path().join("device/mem_info_gtt_used")) else {
            continue;
        };
        let raw = raw.trim();
        if raw.len() > 20 {
            continue;
        }
        if let Ok(value) = raw.parse::<u64>() {
            total = Some(total.unwrap_or(0).saturating_add(value));
        }
    }
    total
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
///
/// #1747: GTT is read by a second, independent task that publishes its latest value. A stuck
/// sysfs read ends only that task (its reading is cleared), never the /proc sampler.
pub struct HostSampler {
    task: tokio::task::JoinHandle<()>,
    gtt_task: tokio::task::JoinHandle<()>,
}
impl Drop for HostSampler {
    fn drop(&mut self) {
        self.task.abort();
        self.gtt_task.abort();
    }
}
/// No GTT reading (none yet, unreadable, or the reader stopped).
const GTT_NONE: u64 = u64::MAX;
impl HostSampler {
    pub fn start(history: Arc<Mutex<HostHistory>>) -> Self {
        Self::start_with_gtt(
            history,
            || async {
                let stat = read_fixed(std::path::Path::new("/proc/stat"), true).await;
                let mem = read_fixed(std::path::Path::new("/proc/meminfo"), false).await;
                (stat, mem)
            },
            || async {
                tokio::task::spawn_blocking(|| {
                    gtt_used_bytes(std::path::Path::new("/sys/class/drm"))
                })
                .await
                .ok()
                .flatten()
            },
        )
    }
    pub fn start_with<F, Fut>(history: Arc<Mutex<HostHistory>>, read: F) -> Self
    where
        F: FnMut() -> Fut + Send + 'static,
        Fut: std::future::Future<
                Output = (Result<String, &'static str>, Result<String, &'static str>),
            > + Send,
    {
        Self::start_with_gtt(history, read, || async { None })
    }
    pub fn start_with_gtt<F, Fut, G, GFut>(
        history: Arc<Mutex<HostHistory>>,
        mut read: F,
        mut gtt: G,
    ) -> Self
    where
        F: FnMut() -> Fut + Send + 'static,
        Fut: std::future::Future<
                Output = (Result<String, &'static str>, Result<String, &'static str>),
            > + Send,
        G: FnMut() -> GFut + Send + 'static,
        GFut: std::future::Future<Output = Option<u64>> + Send,
    {
        let latest = Arc::new(std::sync::atomic::AtomicU64::new(GTT_NONE));
        let publish = latest.clone();
        let gtt_task = tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(2));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                match tokio::time::timeout(READ_TIMEOUT, gtt()).await {
                    Ok(value) => publish.store(
                        value.filter(|v| *v != GTT_NONE).unwrap_or(GTT_NONE),
                        std::sync::atomic::Ordering::Relaxed,
                    ),
                    Err(_) => {
                        publish.store(GTT_NONE, std::sync::atomic::Ordering::Relaxed);
                        break;
                    }
                }
            }
        });
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
                let gtt = Some(latest.load(std::sync::atomic::Ordering::Relaxed))
                    .filter(|v| *v != GTT_NONE);
                let mut history = history.lock().await;
                if history
                    .sample(
                        stat.as_deref().map_err(|e| *e),
                        mem.as_deref().map_err(|e| *e),
                        ts.as_secs_f64(),
                    )
                    .is_ok()
                {
                    history.record_gtt(gtt);
                }
            }
        });
        Self { task, gtt_task }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
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

    #[test]
    fn gtt_sums_cards_and_skips_connectors() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(gtt_used_bytes(dir.path()), None);
        for (name, value) in [
            ("card0", "1073741824\n"),
            ("card1", "536870912\n"),
            ("card0-DP-1", "999\n"),
        ] {
            let device = dir.path().join(name).join("device");
            std::fs::create_dir_all(&device).unwrap();
            std::fs::write(device.join("mem_info_gtt_used"), value).unwrap();
        }
        std::fs::create_dir_all(dir.path().join("renderD128")).unwrap();
        assert_eq!(gtt_used_bytes(dir.path()), Some(1_610_612_736));
        assert_eq!(gtt_used_bytes(&dir.path().join("missing")), None);
    }

    #[test]
    fn gtt_is_reported_only_when_readable() {
        let mut history = HostHistory::default();
        history.record_gtt(Some(1 << 30));
        assert!(history.response().current.is_none());
        history
            .sample(Ok("cpu 1 1 1 1"), Ok("MemTotal: 1048576 kB\n"), 1.0)
            .unwrap();
        let json = serde_json::to_value(history.response()).unwrap();
        assert!(json["current"].get("gtt_used_gb").is_none());
        history.record_gtt(Some(7_086_696_448));
        let json = serde_json::to_value(history.response()).unwrap();
        assert_eq!(json["current"]["gtt_used_gb"], 6.6);
    }
}
