//! The pair's lifetime: the pair lock, both children, the first exit or a stop signal, and the
//! ordered stop (the front joined first, bounded, then Core).

use std::fs::OpenOptions;
use std::path::Path;
use std::process::{Command, ExitStatus};
use std::time::Duration;

use tokio::process::Child;

use nix::fcntl::{fcntl, FcntlArg, FdFlag, Flock, FlockArg};
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;

use crate::env::Env;
use crate::plan::{ChildSpec, Launch};

/// The front's share of Docker's default 10 s stop: Core keeps 3 s for its shutdown hooks.
pub const FRONT_STOP_DEADLINE: Duration = Duration::from_secs(5);

/// Best-effort signal to one process id.
pub fn signal(pid: u32, sig: Signal) {
    if let Ok(pid) = i32::try_from(pid) {
        let _ = kill(Pid::from_raw(pid), sig);
    }
}

/// The exit code `wait` reports: the code, or 128 + the signal that ended it.
pub fn status_code(status: ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
}

/// Takes `<data>/embedding-pair.lock` without waiting and leaves it open, inheritable, for the
/// container's life: both children hold it, so a replacement pair cannot start before both exit.
/// The file is never unlinked and the lock never released explicitly.
pub fn take_pair_lock(data: &Path) -> Result<(), String> {
    std::fs::create_dir_all(data)
        .map_err(|e| format!("web-supervisor: cannot create {}: {e}", data.display()))?;
    let path = data.join("embedding-pair.lock");
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)
        .map_err(|e| format!("web-supervisor: cannot open {}: {e}", path.display()))?;
    let lock = Flock::lock(file, FlockArg::LockExclusiveNonblock)
        .map_err(|_| "web-supervisor: previous Core/front pair is not quiescent".to_owned())?;
    fcntl(&*lock, FcntlArg::F_SETFD(FdFlag::empty()))
        .map_err(|e| format!("web-supervisor: cannot share the pair lock: {e}"))?;
    // Dropping the guard would unlock the shared open file the children inherit.
    std::mem::forget(lock);
    Ok(())
}

fn command(spec: &ChildSpec, env: &Env) -> tokio::process::Command {
    let mut cmd = Command::new(&spec.program);
    // stdin from /dev/null, as bash gives its background jobs.
    cmd.args(&spec.args)
        .stdin(std::process::Stdio::null())
        .env_clear()
        .envs(env.vars())
        .envs(spec.set.iter().map(|(k, v)| (k, v)));
    tokio::process::Command::from(cmd)
}

fn pid_of(child: &Child) -> Option<u32> {
    child.id()
}

/// Front first: TERM, join within FRONT_STOP_DEADLINE or KILL it, and only then TERM Core, so two
/// owners never overlap (revoking the proof cannot retract a batch the front already admitted).
async fn stop(front: &mut Child, node: &mut Child) {
    if let (Ok(None), Some(pid)) = (front.try_wait(), pid_of(front)) {
        signal(pid, Signal::SIGTERM);
        if tokio::time::timeout(FRONT_STOP_DEADLINE, front.wait())
            .await
            .is_err()
        {
            eprintln!("web-supervisor: front stop deadline reached; killing the front before stopping Core");
            let _ = front.kill().await;
        }
    }
    let _ = front.wait().await;
    if let (Ok(None), Some(pid)) = (node.try_wait(), pid_of(node)) {
        signal(pid, Signal::SIGTERM);
    }
}

/// Starts Node, then the front, and supervises both. Returns the container's exit code: 0 after
/// SIGTERM/SIGINT, otherwise the first exit's code (1 when that was 0).
pub fn supervise(env: &Env, data: &Path, launch: &Launch) -> i32 {
    if let Err(msg) = take_pair_lock(data) {
        eprintln!("{msg}");
        return 2;
    }
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("web-supervisor: cannot start the runtime: {e}");
            return 2;
        }
    };
    runtime.block_on(run_pair(env, launch))
}

async fn run_pair(env: &Env, launch: &Launch) -> i32 {
    use tokio::signal::unix::{signal as listen, SignalKind};
    // The stop signals are taken before either child exists (the script's trap came first too).
    let (mut term, mut int) = match (
        listen(SignalKind::terminate()),
        listen(SignalKind::interrupt()),
    ) {
        (Ok(t), Ok(i)) => (t, i),
        _ => {
            eprintln!("web-supervisor: cannot listen for the stop signals");
            return 2;
        }
    };
    let mut node = match command(&launch.node, env).spawn() {
        Ok(child) => child,
        Err(e) => {
            // The script's `node … &` would exit 127 at once and stop the front it started next.
            eprintln!("web-supervisor: cannot start node: {e}");
            eprintln!("web-supervisor: node exited (127); stopping the other");
            return 127;
        }
    };
    let mut front = match command(&launch.front, env).spawn() {
        Ok(child) => child,
        Err(e) => {
            eprintln!("web-supervisor: cannot start {}: {e}", launch.front.program);
            eprintln!("web-supervisor: noevia-server exited (126); stopping the other");
            if let Some(pid) = pid_of(&node) {
                signal(pid, Signal::SIGTERM);
            }
            let _ = node.wait().await;
            return 126;
        }
    };
    // Whichever exits first (or a signal) ends the wait.
    let first = tokio::select! {
        s = node.wait() => Some(s.map(status_code).unwrap_or(1)),
        s = front.wait() => Some(s.map(status_code).unwrap_or(1)),
        _ = term.recv() => None,
        _ = int.recv() => None,
    };
    if let Some(code) = first {
        let which = if matches!(node.try_wait(), Ok(None)) {
            "noevia-server"
        } else {
            "node"
        };
        eprintln!("web-supervisor: {which} exited ({code}); stopping the other");
    }
    stop(&mut front, &mut node).await;
    let _ = node.wait().await;
    let _ = front.wait().await;
    // The script's trap still marks a docker stop that lands during a crash's stop sequence, and
    // then exits 0: a stop signal received by now counts as signalled.
    let late_signal = tokio::select! {
        biased;
        _ = term.recv() => true,
        _ = int.recv() => true,
        () = std::future::ready(()) => false,
    };
    let first = if late_signal { None } else { first };
    // docker stop: a clean exit. Anything else: non-zero, even when the process that left said 0.
    match first {
        None => 0,
        Some(0) => 1,
        Some(code) => code,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn a_second_pair_lock_on_the_same_directory_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        // The first lock is held by this test process (leaked like the supervisor's own).
        take_pair_lock(dir.path()).unwrap();
        // flock locks belong to the open file: a second open in the same process conflicts too.
        let err = take_pair_lock(dir.path()).unwrap_err();
        assert!(err.contains("not quiescent"), "{err}");
        assert!(dir.path().join("embedding-pair.lock").exists());
    }

    #[test]
    fn exit_codes_follow_wait() {
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(status_code(ExitStatus::from_raw(3 << 8)), 3);
        assert_eq!(status_code(ExitStatus::from_raw(15)), 143);
    }
}
