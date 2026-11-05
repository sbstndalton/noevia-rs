//! Bounded, cancellable Git subprocesses for the workspace owner. The caller supplies only
//! registered repository paths and validated task branches; ambient credentials never enter Git.
use rustix::{
    fs::{fcntl_getfl, fcntl_setfl, OFlags},
    process::{kill_process_group, waitid, Pid, Signal, WaitId, WaitIdOptions},
};
use std::{
    collections::BTreeMap,
    io::{self, Read},
    path::Path,
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

pub const HOSTILE_OFF: &[&str] = &[
    "-c",
    "core.fsmonitor=false",
    "-c",
    "core.hooksPath=/dev/null",
    "-c",
    "core.attributesFile=/dev/null",
];
const ENV_KEYS: &[&str] = &[
    "PATH", "HOME", "LANG", "LC_ALL", "LC_CTYPE", "TZ", "TMPDIR", "USER", "LOGNAME",
];
#[derive(Debug)]
pub enum Error {
    Setup,
    Io(io::Error),
    Cancelled,
    Timeout,
    TooLarge,
    Failed { stderr: String },
}
impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}
struct Owned {
    child: Child,
    pid: Pid,
    reaped: bool,
}
impl Owned {
    fn exited(&self) -> Result<bool, Error> {
        waitid(
            WaitId::Pid(self.pid),
            WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT,
        )
        .map(|status| status.is_some())
        .map_err(|_| Error::Setup)
    }
    fn finish(&mut self) -> Result<bool, Error> {
        // Keep the unreaped leader alive until after the group signal: its PID cannot be reused.
        let _ = kill_process_group(self.pid, Signal::KILL);
        let status = self.child.wait()?;
        self.reaped = true;
        Ok(status.success())
    }
}
impl Drop for Owned {
    fn drop(&mut self) {
        if !self.reaped {
            let _ = kill_process_group(self.pid, Signal::KILL);
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}
fn nonblocking(fd: &impl std::os::fd::AsFd) -> Result<(), Error> {
    let flags = fcntl_getfl(fd).map_err(io::Error::from)?;
    fcntl_setfl(fd, flags | OFlags::NONBLOCK).map_err(io::Error::from)?;
    Ok(())
}
fn drain(reader: &mut impl Read, bytes: &mut Vec<u8>, max: usize) -> Result<bool, Error> {
    let mut buffer = [0; 8192];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => return Ok(true),
            Ok(count) => {
                if count > max.saturating_sub(bytes.len()) {
                    return Err(Error::TooLarge);
                }
                bytes.extend_from_slice(buffer.get(..count).ok_or(Error::Setup)?);
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(false),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error.into()),
        }
    }
}
/// Blocking actor work only. Cancellation remains observable while draining either pipe, and
/// every return path synchronously kills/reaps the owned process group before releasing its caller.
pub fn run(
    args: &[String],
    cwd: &Path,
    environment: &BTreeMap<String, String>,
    max: usize,
    raw: bool,
    cancelled: &AtomicBool,
) -> Result<String, Error> {
    use std::os::unix::process::CommandExt;
    if cancelled.load(Ordering::Acquire) {
        return Err(Error::Cancelled);
    }
    let mut command = Command::new("git");
    command
        .env_clear()
        .current_dir(cwd)
        .args(args)
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for key in ENV_KEYS {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    command.envs(environment);
    let child = command.spawn()?;
    let Some(pid) = i32::try_from(child.id()).ok().and_then(Pid::from_raw) else {
        let mut child = child;
        let _ = child.kill();
        let _ = child.wait();
        return Err(Error::Setup);
    };
    let mut owned = Owned {
        child,
        pid,
        reaped: false,
    };
    let mut stdout = owned.child.stdout.take().ok_or(Error::Setup)?;
    let mut stderr = owned.child.stderr.take().ok_or(Error::Setup)?;
    nonblocking(&stdout)?;
    nonblocking(&stderr)?;
    let start = Instant::now();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let success = loop {
        if cancelled.load(Ordering::Acquire) {
            return Err(Error::Cancelled);
        }
        if start.elapsed() >= Duration::from_secs(120) {
            return Err(Error::Timeout);
        }
        let _ = drain(&mut stdout, &mut out, max)?;
        let _ = drain(&mut stderr, &mut err, max)?;
        if owned.exited()? {
            break owned.finish()?;
        }
        std::thread::sleep(Duration::from_millis(2));
    };
    // All members of the original group have been signalled before the leader is reaped.
    let _ = drain(&mut stdout, &mut out, max)?;
    let _ = drain(&mut stderr, &mut err, max)?;
    if !success {
        return Err(Error::Failed {
            stderr: String::from_utf8_lossy(&err).into_owned(),
        });
    }
    let text = String::from_utf8_lossy(&out).into_owned();
    Ok(if raw {
        text
    } else {
        server_account::util::js_trim(&text).to_owned()
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use std::sync::Arc;
    #[test]
    fn cancellation_kills_the_real_process_group_before_returning() {
        let dir = tempfile::tempdir().unwrap();
        let heartbeat = dir.path().join("heartbeat");
        let env = BTreeMap::from([
            ("GIT_CONFIG_GLOBAL".into(), "/dev/null".into()),
            ("GIT_CONFIG_SYSTEM".into(), "/dev/null".into()),
            (
                "SYNTHETIC_HEARTBEAT".into(),
                heartbeat.to_string_lossy().into_owned(),
            ),
        ]);
        let cancelled = Arc::new(AtomicBool::new(false));
        let signal = cancelled.clone();
        let observed = heartbeat.clone();
        let notifier = std::thread::spawn(move || {
            let start = Instant::now();
            while !observed.exists() && start.elapsed() < Duration::from_secs(30) {
                std::thread::sleep(Duration::from_millis(2));
            }
            signal.store(true, Ordering::Release);
        });
        let args = [
            "-c",
            "alias.synthetic=!while :; do printf x >> \"$SYNTHETIC_HEARTBEAT\"; sleep 0.02; done",
            "synthetic",
        ]
        .map(str::to_owned);
        let result = run(&args, dir.path(), &env, 1024, false, &cancelled);
        notifier.join().unwrap();
        assert!(matches!(result, Err(Error::Cancelled)));
        let length = fs_len(&heartbeat);
        assert!(length > 0, "prove the child executed before cancellation");
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(
            fs_len(&heartbeat),
            length,
            "descendants must not keep running after cancellation"
        );
    }
    fn fs_len(path: &Path) -> u64 {
        std::fs::metadata(path).unwrap().len()
    }
}
