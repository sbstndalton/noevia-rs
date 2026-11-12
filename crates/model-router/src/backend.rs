//! One running engine: a `sh -c` process in its own process group, its health check, and the stop
//! sequence (stop command, SIGTERM then SIGKILL of the group, docker removal verified by name).

use crate::config::Model;
use nix::sys::signal::{killpg, Signal};
use nix::unistd::Pid;
use serde_json::{json, Value};
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::{Child, Command};
use tokio::sync::watch;
use tokio::time::{sleep, timeout, Instant};

/// How a start ended when it did not become ready.
#[derive(Debug, Clone, PartialEq)]
pub enum StartError {
    Exited(String),
    Timeout(Duration),
    Spawn(String),
    Cancelled,
}

impl std::fmt::Display for StartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StartError::Exited(status) => {
                write!(f, "backend exited ({status}) before it was ready")
            }
            StartError::Timeout(t) => write!(f, "backend not ready after {:.0}s", t.as_secs_f64()),
            StartError::Spawn(e) => write!(f, "backend could not be started: {e}"),
            StartError::Cancelled => f.write_str("router is shutting down"),
        }
    }
}

pub struct Backend {
    pub model: Model,
    child: Child,
    pgid: Option<Pid>,
    log: std::fs::File,
}

fn log_line(log: &mut std::fs::File, line: &str) {
    use std::io::Write;
    let _ = writeln!(log, "=== {} {line}", crate::events::utc_now());
}

fn open_log(logdir: &Path, name: &str) -> std::io::Result<std::fs::File> {
    let safe: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "._-".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(logdir.join(format!("backend-{safe}.log")))
}

impl Backend {
    pub fn spawn(model: &Model, logdir: &Path) -> Result<Backend, StartError> {
        let mut log =
            open_log(logdir, &model.name).map_err(|e| StartError::Spawn(e.to_string()))?;
        log_line(&mut log, &format!("start: {}", model.cmd));
        let out = log
            .try_clone()
            .map_err(|e| StartError::Spawn(e.to_string()))?;
        let errf = log
            .try_clone()
            .map_err(|e| StartError::Spawn(e.to_string()))?;
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg(&model.cmd)
            .current_dir(&model.cwd)
            .envs(model.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .stdin(Stdio::null())
            .stdout(Stdio::from(out))
            .stderr(Stdio::from(errf))
            .process_group(0);
        let child = cmd.spawn().map_err(|e| StartError::Spawn(e.to_string()))?;
        let pgid = child
            .id()
            .and_then(|id| i32::try_from(id).ok())
            .map(Pid::from_raw);
        Ok(Backend {
            model: model.clone(),
            child,
            pgid,
            log,
        })
    }

    /// `Some(status)` once the process has exited.
    pub fn exited(&mut self) -> Option<String> {
        match self.child.try_wait() {
            Ok(Some(status)) => Some(status.to_string()),
            Ok(None) => None,
            Err(e) => Some(format!("wait failed: {e}")),
        }
    }

    pub async fn healthy(&self, client: &reqwest::Client) -> bool {
        let Some(path) = &self.model.health else {
            return true;
        };
        let url = format!("{}{}", self.model.proxy, path);
        let Ok(resp) = client
            .get(&url)
            .timeout(Duration::from_secs(3))
            .send()
            .await
        else {
            return false;
        };
        if resp.status() != reqwest::StatusCode::OK {
            return false;
        }
        match resp.text().await {
            Ok(body) => body.contains(&self.model.health_expect),
            Err(_) => false,
        }
    }

    /// Poll health every 250 ms until ready, the process exits, `ready_timeout` passes or the
    /// router shuts down.
    pub async fn wait_ready(
        &mut self,
        client: &reqwest::Client,
        shutdown: &mut watch::Receiver<bool>,
    ) -> Result<(), StartError> {
        let deadline = Instant::now() + self.model.ready_timeout;
        loop {
            if *shutdown.borrow() {
                return Err(StartError::Cancelled);
            }
            if let Some(status) = self.exited() {
                return Err(StartError::Exited(status));
            }
            if self.healthy(client).await {
                // A process that answered and then died is not ready.
                return match self.exited() {
                    Some(status) => Err(StartError::Exited(status)),
                    None => Ok(()),
                };
            }
            if Instant::now() >= deadline {
                return Err(StartError::Timeout(self.model.ready_timeout));
            }
            tokio::select! {
                _ = sleep(Duration::from_millis(250)) => {}
                _ = shutdown.changed() => {}
            }
        }
    }

    fn signal(&self, sig: Signal) -> bool {
        match self.pgid {
            Some(pgid) => killpg(pgid, sig).is_ok(),
            None => false,
        }
    }

    /// The full stop: stop command, SIGTERM, SIGKILL after `stop_timeout`, a final SIGKILL of the
    /// group, then every configured container removed and verified gone. The report lists the
    /// containers that are still there (non-empty means the GPU may still be in use).
    pub async fn stop(mut self, docker: &str) -> StopReport {
        let t0 = Instant::now();
        let mut report = StopReport::default();
        if !self.model.stop.is_empty() {
            let pid = self
                .pgid
                .map(|p| p.as_raw().to_string())
                .unwrap_or_default();
            let cmdline = self.model.stop.replace("{pid}", &pid);
            report.stop_cmd_rc =
                Some(run_bounded(&cmdline, &self.model, &self.log, self.model.stop_timeout).await);
        }
        for (sig, wait) in [
            (Signal::SIGTERM, self.model.stop_timeout),
            (Signal::SIGKILL, Duration::from_secs(10)),
        ] {
            if self.exited().is_some() {
                break;
            }
            if !self.signal(sig) {
                break;
            }
            report.signals.push(sig.as_str().to_string());
            let _ = timeout(wait, self.child.wait()).await;
        }
        // The shell is gone; anything else left in its group goes too, and must be seen to go: a
        // host process (not a container) can hold the GPU as well.
        let _ = self.signal(Signal::SIGKILL);
        if let Some(pgid) = self.pgid {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                // Reap our own child each pass: its zombie would count as a live member.
                let _ = self.exited();
                if !group_alive(pgid) {
                    break;
                }
                if Instant::now() >= deadline {
                    report.group_left = Some(pgid.as_raw());
                    break;
                }
                sleep(Duration::from_millis(100)).await;
            }
        }
        report.exited = self.exited();
        report.containers_left = remove_containers(docker, &self.model.containers).await;
        report.stop_s = round2(t0.elapsed());
        log_line(&mut self.log, &format!("stopped {}", report.to_json()));
        report
    }
}

#[derive(Debug, Default, Clone)]
pub struct StopReport {
    /// The stop command's exit code, `-1` for a timeout or a spawn failure.
    pub stop_cmd_rc: Option<i32>,
    pub signals: Vec<String>,
    pub exited: Option<String>,
    pub containers_left: Vec<String>,
    /// The process group, if a member was still alive 10 s after the last SIGKILL.
    pub group_left: Option<i32>,
    pub stop_s: f64,
}

impl StopReport {
    pub fn to_json(&self) -> Value {
        json!({
            "cmd_rc": self.stop_cmd_rc,
            "signals": self.signals,
            "containers_left": self.containers_left,
            "group_left": self.group_left,
            "s": self.stop_s,
        })
    }
}

/// Whether any process of this group still exists.
pub fn group_alive(pgid: Pid) -> bool {
    killpg(pgid, None).is_ok()
}

pub fn round2(d: Duration) -> f64 {
    (d.as_secs_f64() * 100.0).round() / 100.0
}

async fn run_bounded(cmdline: &str, model: &Model, log: &std::fs::File, limit: Duration) -> i32 {
    let (Ok(out), Ok(errf)) = (log.try_clone(), log.try_clone()) else {
        return -1;
    };
    let child = Command::new("sh")
        .arg("-c")
        .arg(cmdline)
        .current_dir(&model.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(errf))
        .process_group(0)
        .kill_on_drop(true)
        .spawn();
    let Ok(mut child) = child else { return -1 };
    match timeout(limit, child.wait()).await {
        Ok(Ok(status)) => status.code().unwrap_or(-1),
        _ => {
            if let Some(pid) = child.id().and_then(|id| i32::try_from(id).ok()) {
                let _ = killpg(Pid::from_raw(pid), Signal::SIGKILL);
            }
            let _ = child.wait().await;
            -1
        }
    }
}

/// Whether docker lists a container with exactly this name. `None`: docker could not be asked,
/// which never counts as gone.
pub async fn container_exists(docker: &str, name: &str) -> Option<bool> {
    let out = timeout(
        Duration::from_secs(30),
        Command::new(docker)
            .args([
                "ps",
                "-a",
                "--filter",
                &format!("name=^{name}$"),
                "--format",
                "{{.Names}}",
            ])
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .any(|l| l.trim().trim_start_matches('/') == name),
    )
}

/// `docker rm -f` each name until docker no longer lists it (six removals, one second apart, and a
/// last check).
/// Returns the names that are still there or could not be checked.
pub async fn remove_containers(docker: &str, names: &[String]) -> Vec<String> {
    let mut left = Vec::new();
    for name in names {
        let mut gone = false;
        for attempt in 0..7 {
            match container_exists(docker, name).await {
                Some(false) => {
                    gone = true;
                    break;
                }
                // The seventh look only checks the last removal.
                Some(true) if attempt < 6 => {
                    let _ = timeout(
                        Duration::from_secs(60),
                        Command::new(docker)
                            .args(["rm", "-f", name])
                            .stdin(Stdio::null())
                            .stdout(Stdio::null())
                            .stderr(Stdio::null())
                            .kill_on_drop(true)
                            .status(),
                    )
                    .await;
                }
                _ => {}
            }
            if attempt < 6 {
                sleep(Duration::from_secs(1)).await;
            }
        }
        if !gone {
            left.push(name.clone());
        }
    }
    left
}
