//! The pair's lifetime: the pair lock, both children, the first exit or a stop signal, and the
//! ordered stop (the front joined first, bounded, then Core).

use std::fs::OpenOptions;
use std::path::Path;
use std::process::{Child, Command, ExitStatus};
use std::time::{Duration, Instant};

use nix::fcntl::{fcntl, FcntlArg, FdFlag, Flock, FlockArg};
use nix::sys::signal::{kill, SigSet, Signal};
use nix::unistd::Pid;

use crate::env::Env;
use crate::plan::{ChildSpec, Launch};

/// The front's share of Docker's default 10 s stop: Core keeps 3 s for its shutdown hooks.
pub const FRONT_STOP_DEADLINE: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(50);

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

fn command(spec: &ChildSpec, env: &Env) -> Command {
    let mut cmd = Command::new(&spec.program);
    cmd.args(&spec.args)
        .env_clear()
        .envs(env.vars())
        .envs(spec.set.iter().map(|(k, v)| (k, v)));
    cmd
}

struct Proc {
    child: Child,
    status: Option<ExitStatus>,
}

impl Proc {
    fn poll(&mut self) -> Option<ExitStatus> {
        if self.status.is_none() {
            if let Ok(Some(s)) = self.child.try_wait() {
                self.status = Some(s);
            }
        }
        self.status
    }

    fn running(&mut self) -> bool {
        self.poll().is_none()
    }

    fn wait(&mut self) {
        if self.status.is_none() {
            self.status = self.child.wait().ok();
        }
    }
}

/// Front first: TERM, join within FRONT_STOP_DEADLINE or KILL it, and only then TERM Core, so two
/// owners never overlap (revoking the proof cannot retract a batch the front already admitted).
fn stop(front: &mut Proc, node: &mut Proc) {
    if front.running() {
        signal(front.child.id(), Signal::SIGTERM);
        let deadline = Instant::now() + FRONT_STOP_DEADLINE;
        while front.running() {
            if Instant::now() >= deadline {
                eprintln!("web-supervisor: front stop deadline reached; killing the front before stopping Core");
                let _ = front.child.kill();
                break;
            }
            std::thread::sleep(POLL);
        }
        front.wait();
    }
    if node.running() {
        signal(node.child.id(), Signal::SIGTERM);
    }
}

/// Starts Node, then the front, and supervises both. Returns the container's exit code: 0 after
/// SIGTERM/SIGINT, otherwise the first exit's code (1 when that was 0).
pub fn supervise(env: &Env, data: &Path, launch: &Launch) -> i32 {
    if let Err(msg) = take_pair_lock(data) {
        eprintln!("{msg}");
        return 2;
    }
    // Signals are taken synchronously: blocked here, waited for below. Children start with an
    // empty mask (std resets it at spawn).
    let mut set = SigSet::empty();
    for sig in [Signal::SIGTERM, Signal::SIGINT, Signal::SIGCHLD] {
        set.add(sig);
    }
    if set.thread_block().is_err() {
        eprintln!("web-supervisor: cannot block the stop signals");
        return 2;
    }
    let mut node = match command(&launch.node, env).spawn() {
        Ok(child) => Proc {
            child,
            status: None,
        },
        Err(e) => {
            // The script's `node … &` would exit 127 at once and stop the front it started next.
            eprintln!("web-supervisor: cannot start node: {e}");
            eprintln!("web-supervisor: node exited (127); stopping the other");
            return 127;
        }
    };
    let mut front = match command(&launch.front, env).spawn() {
        Ok(child) => Proc {
            child,
            status: None,
        },
        Err(e) => {
            eprintln!("web-supervisor: cannot start {}: {e}", launch.front.program);
            eprintln!("web-supervisor: noevia-server exited (126); stopping the other");
            signal(node.child.id(), Signal::SIGTERM);
            node.wait();
            return 126;
        }
    };
    // Whichever exits first (or a signal) ends the wait.
    let mut signalled = false;
    let code = loop {
        if let Some(s) = node.poll() {
            break status_code(s);
        }
        if let Some(s) = front.poll() {
            break status_code(s);
        }
        match set.wait() {
            Ok(Signal::SIGTERM | Signal::SIGINT) => {
                signalled = true;
                break 0;
            }
            Ok(_) => {}
            Err(_) => std::thread::sleep(POLL),
        }
    };
    if !signalled {
        let which = if node.running() {
            "noevia-server"
        } else {
            "node"
        };
        eprintln!("web-supervisor: {which} exited ({code}); stopping the other");
    }
    stop(&mut front, &mut node);
    node.wait();
    front.wait();
    // docker stop: a clean exit. Anything else: non-zero, even when the process that left said 0.
    if signalled {
        0
    } else if code == 0 {
        1
    } else {
        code
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
