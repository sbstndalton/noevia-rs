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
pub const NET: &[u8; 8] = b"M4NET001";
enum Envelope {
    Body { cap: usize },
    Http { cap: usize },
}
impl Envelope {
    fn body(cap: usize) -> Result<Self, Error> {
        if cap > crate::LIST_BYTES {
            return Err(Error::Input);
        }
        Ok(Self::Body { cap })
    }
    fn http(cap: usize) -> Result<Self, Error> {
        if !matches!(cap, crate::LIST_BYTES | crate::TEXT_BYTES) {
            return Err(Error::Input);
        }
        Ok(Self::Http { cap })
    }
    fn tag(&self) -> &[u8; 8] {
        match self {
            Self::Body { .. } => BODY,
            Self::Http { .. } => NET,
        }
    }
    fn maximum(&self) -> usize {
        match self {
            Self::Body { cap } => *cap,
            Self::Http { cap } => crate::response_ipc::METADATA + cap + 1,
        }
    }
    fn validate(&self, out: &[u8]) -> Result<(), Error> {
        let offset = READY.len();
        let head = offset + 8 + 4;
        if out.len() < head {
            return Ok(());
        }
        if out.get(offset..offset + 8) != Some(self.tag().as_slice()) {
            return Err(Error::Protocol);
        }
        let size = usize::try_from(u32::from_be_bytes(
            out.get(offset + 8..head)
                .ok_or(Error::Protocol)?
                .try_into()
                .map_err(|_| Error::Protocol)?,
        ))
        .map_err(|_| Error::Protocol)?;
        if size > self.maximum() {
            return Err(Error::Output);
        }
        if out.len() > head + size {
            return Err(Error::Protocol);
        }
        if let Self::Http { cap } = self {
            if size < crate::response_ipc::METADATA {
                return Err(Error::Protocol);
            }
            if out.len() >= head + crate::response_ipc::METADATA {
                let payload = crate::response_ipc::validate_metadata(
                    out.get(head..).ok_or(Error::Protocol)?,
                    *cap,
                )?;
                if size != crate::response_ipc::METADATA + payload {
                    return Err(Error::Protocol);
                }
            }
        }
        Ok(())
    }
}
/// Constructed only after complete framing, normal exit, EOF and confirmed reap.
pub struct OwnedReply {
    bytes: Vec<u8>,
    cap: usize,
}
impl OwnedReply {
    pub fn view(&self) -> Result<crate::response_ipc::Reply<'_>, Error> {
        crate::response_ipc::decode(&self.bytes, self.cap)
    }
}
pub fn attempt_http(
    command: Command,
    request: crate::protocol::Request,
    deadline: Duration,
    cancelled: &AtomicBool,
    permit: Permit,
) -> Result<OwnedReply, Error> {
    attempt_http_observed(command, request, deadline, cancelled, permit, |_, _| {})
}
/// Owned main-thread DNS: setup, IPC and transport share the enclosing budget.
/// The command must bind the fixed DNS child; serialization precedes pump timing.
pub fn attempt_dns(
    command: Command,
    request: crate::protocol::Request,
    outer_remaining: Duration,
    cancelled: &AtomicBool,
    permit: impl Into<Option<Permit>>,
) -> Result<OwnedReply, Error> {
    attempt_dns_observed(
        command,
        request,
        outer_remaining,
        cancelled,
        permit,
        |_, _| {},
    )
}
pub fn attempt_dns_observed(
    command: Command,
    request: crate::protocol::Request,
    outer_remaining: Duration,
    cancelled: &AtomicBool,
    permit: impl Into<Option<Permit>>,
    observe: impl FnMut(NetworkObservation, u32),
) -> Result<OwnedReply, Error> {
    let budget = outer_remaining.min(request.attempt_timeout);
    if budget.is_zero() {
        return Err(Error::Deadline);
    }
    attempt_http_observed(command, request, budget, cancelled, permit, observe)
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum NetworkObservation {
    OutputProgress(usize),
    BeforeReap,
}
enum PumpObservation {
    Existing(Observation),
    OutputProgress(usize),
}
pub fn attempt_http_observed(
    command: Command,
    request: crate::protocol::Request,
    deadline: Duration,
    cancelled: &AtomicBool,
    permit: impl Into<Option<Permit>>,
    mut observe: impl FnMut(NetworkObservation, u32),
) -> Result<OwnedReply, Error> {
    let cap = request.body_cap;
    let envelope = Envelope::http(cap)?;
    let input = crate::request_ipc::encode(&request)?;
    let bytes = pump(
        command,
        input,
        envelope,
        deadline,
        cancelled,
        permit.into(),
        |stage, pid| match stage {
            PumpObservation::OutputProgress(bytes) => {
                observe(NetworkObservation::OutputProgress(bytes), pid)
            }
            PumpObservation::Existing(Observation::BeforeReap) => {
                observe(NetworkObservation::BeforeReap, pid)
            }
            _ => {}
        },
    )?;
    crate::response_ipc::decode(&bytes, cap)?;
    Ok(OwnedReply { bytes, cap })
}
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
    command: Command,
    input: Zeroizing<Vec<u8>>,
    body_cap: usize,
    deadline: Duration,
    cancelled: &AtomicBool,
    permit: Permit,
) -> Result<Vec<u8>, Error> {
    attempt_observed(
        command,
        input,
        body_cap,
        deadline,
        cancelled,
        permit,
        |_, _| {},
    )
}

/// Private proof observations only; callbacks must be finite and secret-free.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Observation {
    InputProgress(usize),
    BeforeReap,
}
pub fn attempt_observed(
    command: Command,
    input: Zeroizing<Vec<u8>>,
    body_cap: usize,
    deadline: Duration,
    cancelled: &AtomicBool,
    permit: Permit,
    mut observe: impl FnMut(Observation, u32),
) -> Result<Vec<u8>, Error> {
    pump(
        command,
        input,
        Envelope::body(body_cap)?,
        deadline,
        cancelled,
        Some(permit),
        |stage, pid| {
            if let PumpObservation::Existing(stage) = stage {
                observe(stage, pid)
            }
        },
    )
}
fn pump(
    mut command: Command,
    input: Zeroizing<Vec<u8>>,
    envelope: Envelope,
    deadline: Duration,
    cancelled: &AtomicBool,
    permit: Option<Permit>,
    mut observe: impl FnMut(PumpObservation, u32),
) -> Result<Vec<u8>, Error> {
    if input.len() > INPUT {
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
        permit,
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
        let cap = envelope
            .maximum()
            .checked_add(READY.len() + envelope.tag().len() + 4)
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
                let previous = out.len();
                out_done = read_step(&mut stdout, &mut out, cap)?;
                if out.len() != previous {
                    observe(PumpObservation::OutputProgress(out.len()), owned.child.id());
                }
            }
            if !err_done {
                err_done = read_step(&mut stderr, &mut err, STDERR)?;
            }
            envelope.validate(&out)?;
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
                            Ok(n) => {
                                sent += n;
                                observe(
                                    PumpObservation::Existing(Observation::InputProgress(sent)),
                                    owned.child.id(),
                                );
                            }
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
        if out.get(offset..offset + envelope.tag().len()) != Some(envelope.tag().as_slice()) {
            return Err(Error::Protocol);
        }
        let offset = offset + envelope.tag().len();
        let size: [u8; 4] = out
            .get(offset..offset + 4)
            .ok_or(Error::Protocol)?
            .try_into()
            .map_err(|_| Error::Protocol)?;
        let size = usize::try_from(u32::from_be_bytes(size)).map_err(|_| Error::Protocol)?;
        if size > envelope.maximum() || out.len() != offset + 4 + size {
            return Err(Error::Protocol);
        }
        // Shift in place, no duplicate body allocation. Raw bytes decoded by caller.
        out.drain(..offset + 4);
        Ok(out)
    })();
    observe(
        PumpObservation::Existing(Observation::BeforeReap),
        owned.child.id(),
    );
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
