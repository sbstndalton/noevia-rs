//! Private synchronous owned lifecycle proof. No network/production adapter.
//! Cancellation is polled; cleanup owns admission until confirmed reap.
use crate::admission::Permit;
use std::{
    io::{Read, Write},
    os::{fd::AsFd, unix::process::CommandExt},
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};
use zeroize::Zeroizing;

pub const INPUT: usize = 16 * 1024 * 1024;
pub const READY: &[u8; 8] = b"M4READY1";
pub const BODY: &[u8; 8] = b"M4BODY01";
const STDERR: usize = 4096;
const STEP: usize = 4096;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Setup,
    Input,
    Deadline,
    Cancelled,
    Output,
    Protocol,
    Exit,
    Reap,
}

struct Owned {
    child: Child,
    permit: Option<Permit>,
    reaped: bool,
}
impl Owned {
    fn pid(&self) -> Result<rustix::process::Pid, Error> {
        i32::try_from(self.child.id())
            .ok()
            .and_then(rustix::process::Pid::from_raw)
            .ok_or(Error::Setup)
    }
    fn exited(&self) -> Result<bool, Error> {
        rustix::process::waitid(
            rustix::process::WaitId::Pid(self.pid()?),
            rustix::process::WaitIdOptions::EXITED
                | rustix::process::WaitIdOptions::NOHANG
                | rustix::process::WaitIdOptions::NOWAIT,
        )
        .map(|v| v.is_some())
        .map_err(|_| Error::Reap)
    }
    fn cleanup(&mut self) -> Result<bool, Error> {
        if self.reaped {
            return Err(Error::Reap);
        }
        // Leader has never been consumed: group identity is retained through signal.
        if let Ok(pid) = self.pid() {
            let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
        }
        let _ = self.child.kill();
        // Synchronous proof only: hold admission while the kernel has not exited.
        // No hard real-time reap claim. Async supervisor adapter remains held.
        loop {
            if self.exited()? {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        let status = self.child.wait().map_err(|_| Error::Reap)?;
        self.reaped = true;
        self.permit.take();
        Ok(status.success())
    }
}
impl Drop for Owned {
    fn drop(&mut self) {
        if !self.reaped && self.cleanup().is_err() {
            // Quarantine the admission entry if reap cannot be confirmed.
            // Leaking one small permit is intentional fail-closed availability loss.
            if let Some(permit) = self.permit.take() {
                std::mem::forget(permit);
            }
        }
    }
}
fn nonblocking(pipe: &impl AsFd) -> Result<(), Error> {
    let flags = rustix::fs::fcntl_getfl(pipe).map_err(|_| Error::Setup)?;
    rustix::fs::fcntl_setfl(pipe, flags | rustix::fs::OFlags::NONBLOCK).map_err(|_| Error::Setup)
}
fn transient(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
    )
}
fn read_step(pipe: &mut impl Read, output: &mut Vec<u8>, cap: usize) -> Result<bool, Error> {
    let mut scratch = [0; STEP];
    match pipe.read(&mut scratch) {
        Ok(0) => Ok(true),
        Ok(n) => {
            if output.len().checked_add(n).is_none_or(|v| v > cap) {
                return Err(Error::Output);
            }
            output.extend_from_slice(scratch.get(..n).ok_or(Error::Output)?);
            Ok(false)
        }
        Err(e) if transient(&e) => Ok(false),
        Err(_) => Err(Error::Output),
    }
}
/// Takes admission ownership even on spawn failure; no credentials sent before READY.
/// `command` must identify an owned fixed executable/mode, never a user command.
pub fn attempt(
    mut command: Command,
    input: Zeroizing<Vec<u8>>,
    body_cap: usize,
    deadline: Duration,
    cancelled: &AtomicBool,
    permit: Permit,
) -> Result<Vec<u8>, Error> {
    if input.len() > INPUT || body_cap > crate::LIST_BYTES {
        return Err(Error::Input);
    }
    let start = Instant::now();
    command
        .env_clear()
        .process_group(0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = command.spawn().map_err(|_| Error::Setup)?;
    let mut owned = Owned {
        child,
        permit: Some(permit),
        reaped: false,
    };
    let result = (|| {
        let mut stdin = Some(owned.child.stdin.take().ok_or(Error::Setup)?);
        let mut stdout = owned.child.stdout.take().ok_or(Error::Setup)?;
        let mut stderr = owned.child.stderr.take().ok_or(Error::Setup)?;
        nonblocking(stdin.as_ref().ok_or(Error::Setup)?)?;
        nonblocking(&stdout)?;
        nonblocking(&stderr)?;
        // Explicit capacities: no length-triggered geometric growth in IPC buffers.
        let cap = body_cap
            .checked_add(READY.len() + BODY.len() + 4)
            .ok_or(Error::Output)?;
        let mut out = Vec::new();
        out.try_reserve_exact(cap).map_err(|_| Error::Output)?;
        let mut err = Vec::new();
        err.try_reserve_exact(STDERR).map_err(|_| Error::Output)?;
        let mut ready = false;
        let mut sent = 0;
        let mut out_done = false;
        let mut err_done = false;
        // Length header uses fixed stack storage, never JSON expansion of credentials.
        let length = u32::try_from(input.len())
            .map_err(|_| Error::Input)?
            .to_be_bytes();
        loop {
            if cancelled.load(Ordering::Acquire) {
                return Err(Error::Cancelled);
            }
            if start.elapsed() >= deadline || (!ready && start.elapsed() >= Duration::from_secs(1))
            {
                return Err(Error::Deadline);
            }
            if !out_done {
                out_done = read_step(&mut stdout, &mut out, cap)?;
            }
            if !err_done {
                err_done = read_step(&mut stderr, &mut err, STDERR)?;
            }
            if !ready && out.len() >= READY.len() {
                if out.get(..READY.len()) != Some(READY.as_slice()) {
                    return Err(Error::Protocol);
                }
                if out.len() != READY.len() {
                    return Err(Error::Protocol);
                }
                ready = true;
            }
            if ready {
                if let Some(pipe) = stdin.as_mut() {
                    let total = length.len() + input.len();
                    if sent == total {
                        stdin = None;
                    } else {
                        let bytes = if sent < length.len() {
                            length.get(sent..).ok_or(Error::Input)?
                        } else {
                            input.get(sent - length.len()..).ok_or(Error::Input)?
                        };
                        let bytes = bytes.get(..bytes.len().min(STEP)).ok_or(Error::Input)?;
                        match pipe.write(bytes) {
                            Ok(0) => return Err(Error::Protocol),
                            Ok(n) => sent += n,
                            Err(e) if transient(&e) => {}
                            Err(_) => return Err(Error::Protocol),
                        }
                    }
                }
            }
            if owned.exited()? && out_done && err_done {
                break;
            }
            std::thread::sleep(
                Duration::from_millis(2).min(deadline.saturating_sub(start.elapsed())),
            );
        }
        if !ready || sent != length.len() + input.len() {
            return Err(Error::Protocol);
        }
        let offset = READY.len();
        if out.get(offset..offset + BODY.len()) != Some(BODY.as_slice()) {
            return Err(Error::Protocol);
        }
        let offset = offset + BODY.len();
        let size: [u8; 4] = out
            .get(offset..offset + 4)
            .ok_or(Error::Protocol)?
            .try_into()
            .map_err(|_| Error::Protocol)?;
        let size = usize::try_from(u32::from_be_bytes(size)).map_err(|_| Error::Protocol)?;
        if size > body_cap || out.len() != offset + 4 + size {
            return Err(Error::Protocol);
        }
        // Shift in place, no duplicate body allocation. Raw bytes decoded by caller.
        out.drain(..offset + 4);
        Ok(out)
    })();
    let success = owned.cleanup()?;
    match result {
        Err(error) => Err(error),
        Ok(bytes) if success => Ok(bytes),
        Ok(_) => Err(Error::Exit),
    }
}

/// Called in the child before READY, stdin, runtime, DNS or secret-bearing input.
#[cfg(target_os = "linux")]
pub fn linux_setup() -> Result<(), Error> {
    use rustix::process::{DumpableBehavior, Resource, Rlimit};
    rustix::process::set_dumpable_behavior(DumpableBehavior::NotDumpable)
        .map_err(|_| Error::Setup)?;
    if rustix::process::dumpable_behavior().map_err(|_| Error::Setup)?
        != DumpableBehavior::NotDumpable
    {
        return Err(Error::Setup);
    }
    for (resource, current, maximum) in [
        (Resource::Core, 0, 0),
        (Resource::As, 256 * 1024 * 1024, 256 * 1024 * 1024),
        (Resource::Cpu, 5, 6),
    ] {
        let limit = Rlimit {
            current: Some(current),
            maximum: Some(maximum),
        };
        rustix::process::setrlimit(resource, limit).map_err(|_| Error::Setup)?;
        if rustix::process::getrlimit(resource) != limit {
            return Err(Error::Setup);
        }
    }
    let mut bytes = Vec::new();
    std::fs::File::open("/proc/self/statm")
        .map_err(|_| Error::Setup)?
        .take(257)
        .read_to_end(&mut bytes)
        .map_err(|_| Error::Setup)?;
    if bytes.len() > 256 {
        return Err(Error::Setup);
    }
    let text = std::str::from_utf8(&bytes).map_err(|_| Error::Setup)?;
    let pages = text
        .split_whitespace()
        .next()
        .ok_or(Error::Setup)?
        .parse::<u64>()
        .map_err(|_| Error::Setup)?;
    let page_size = u64::try_from(rustix::param::page_size()).map_err(|_| Error::Setup)?;
    if pages
        .checked_mul(page_size)
        .is_none_or(|v| v > 128 * 1024 * 1024)
    {
        return Err(Error::Setup);
    }
    Ok(())
}
#[cfg(not(target_os = "linux"))]
pub fn linux_setup() -> Result<(), Error> {
    Err(Error::Setup)
}
