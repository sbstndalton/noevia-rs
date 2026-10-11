//! Private owned resolver substrate; no HTTP/worker/config caller is wired yet.
//! Logical payload bounds and cooperative deadlines do not bound NSS allocation,
//! kernel spawn/kill/wait latency, RSS/OOM, or descendant cleanup.
use std::{
    collections::BTreeSet,
    io::{Read, Write},
    net::{IpAddr, ToSocketAddrs},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
const RAW: usize = 8192;
const HOSTS: usize = 32;
const IPS: usize = 256;
const OUTPUT: usize = 32768;
const STDERR: usize = 4096;
const ATTEMPT: Duration = Duration::from_secs(5);
const TTL: Duration = Duration::from_secs(60);
pub const HELPER_ARG: &str = "--private-code-net-resolver-v1";
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    Input,
    Cap,
    Busy,
    Conflict,
    Protocol,
    Spawn,
    Deadline,
    Cleanup,
    Stopping,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Spec {
    literals: BTreeSet<IpAddr>,
    hosts: Vec<String>,
}
fn ip(text: &str) -> Result<IpAddr, Fault> {
    if text.len() > 64 || !text.is_ascii() || text.contains('%') {
        return Err(Fault::Input);
    }
    let value: IpAddr = text.parse().map_err(|_| Fault::Input)?;
    Ok(match value {
        IpAddr::V6(v) => v.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(value),
        _ => value,
    })
}
impl Spec {
    pub fn parse(raw: &str) -> Result<Self, Fault> {
        // Borrowed pre-scan precedes UTF-16 and crate parser allocation.
        if raw.len() > RAW {
            return Err(Fault::Cap);
        }
        if !raw.is_ascii() {
            return Err(Fault::Input);
        }
        let mut hosts = 0usize;
        let mut literals = 0usize;
        for entry in raw
            .split([',', ' ', '\t', '\n', '\r', '\u{b}', '\u{c}'])
            .filter(|s| !s.is_empty())
        {
            if code_net_guard::is_ip(entry.as_bytes()) {
                literals += 1;
            } else {
                hosts += 1;
                if entry.len() > 253 {
                    return Err(Fault::Cap);
                }
            }
            if hosts > HOSTS || literals > IPS {
                return Err(Fault::Cap);
            }
        }
        let units: Vec<_> = raw.encode_utf16().collect();
        let code_net_guard::Spec::Ok { literals, hosts } =
            code_net_guard::parse_spec(&units).map_err(|_| Fault::Input)?
        else {
            return Err(Fault::Input);
        };
        let literals = literals.iter().map(|s| ip(s)).collect::<Result<_, _>>()?;
        Ok(Self { literals, hosts })
    }
    pub fn enabled(&self) -> bool {
        !self.literals.is_empty() || !self.hosts.is_empty()
    }
}
#[derive(Clone, Debug)]
pub enum Snapshot {
    Disabled,
    Pending {
        deny_union: BTreeSet<IpAddr>,
    },
    Ready {
        generation: u64,
        expires: Option<Instant>,
        deny_union: BTreeSet<IpAddr>,
    },
    Failed,
    Quarantined,
    Stopping,
}
impl Snapshot {
    /// None means unresolved/expired; callers must refuse rather than grant access.
    pub fn refuses(&self, local: Option<IpAddr>, now: Instant) -> Option<bool> {
        match self {
            Self::Disabled => Some(false),
            Self::Ready {
                expires,
                deny_union,
                ..
            } => {
                let Some(local) = local else {
                    return Some(true);
                };
                let local = match local {
                    IpAddr::V6(v) => v.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(local),
                    _ => local,
                };
                if deny_union.contains(&local) {
                    return Some(true);
                }
                if expires.is_some_and(|t| now >= t) {
                    None
                } else {
                    Some(false)
                }
            }
            Self::Pending { deny_union } => {
                let Some(local) = local else {
                    return Some(true);
                };
                let local = match local {
                    IpAddr::V6(v) => v.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(local),
                    _ => local,
                };
                if deny_union.contains(&local) {
                    Some(true)
                } else {
                    None
                }
            }

            _ => Some(true),
        }
    }
}
fn append(out: &mut Vec<u8>, bytes: &[u8], cap: usize) -> Result<(), Fault> {
    if out.len().checked_add(bytes.len()).is_none_or(|n| n > cap) {
        return Err(Fault::Cap);
    }
    out.extend_from_slice(bytes);
    Ok(())
}
fn request(spec: &Spec, generation: u64) -> Result<Vec<u8>, Fault> {
    let mut out = Vec::new();
    append(&mut out, b"NR01", RAW)?;
    append(&mut out, &generation.to_be_bytes(), RAW)?;
    append(
        &mut out,
        &[u8::try_from(spec.hosts.len()).map_err(|_| Fault::Cap)?],
        RAW,
    )?;
    for host in &spec.hosts {
        append(
            &mut out,
            &[u8::try_from(host.len()).map_err(|_| Fault::Cap)?],
            RAW,
        )?;
        append(&mut out, host.as_bytes(), RAW)?;
    }
    Ok(out)
}
struct Cursor<'a> {
    data: &'a [u8],
    at: usize,
}
impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], Fault> {
        let end = self.at.checked_add(n).ok_or(Fault::Cap)?;
        let value = self.data.get(self.at..end).ok_or(Fault::Protocol)?;
        self.at = end;
        Ok(value)
    }
    fn byte(&mut self) -> Result<u8, Fault> {
        self.take(1)?.first().copied().ok_or(Fault::Protocol)
    }
    fn generation(&mut self) -> Result<u64, Fault> {
        Ok(u64::from_be_bytes(
            self.take(8)?.try_into().map_err(|_| Fault::Protocol)?,
        ))
    }
    fn end(&self) -> Result<(), Fault> {
        if self.at == self.data.len() {
            Ok(())
        } else {
            Err(Fault::Protocol)
        }
    }
}
fn parse_request(raw: &[u8]) -> Result<(u64, Spec), Fault> {
    if raw.len() > RAW {
        return Err(Fault::Cap);
    }
    let mut c = Cursor { data: raw, at: 0 };
    if c.take(4)? != b"NR01" {
        return Err(Fault::Protocol);
    }
    let generation = c.generation()?;
    let count = usize::from(c.byte()?);
    if count == 0 || count > HOSTS || generation == 0 {
        return Err(Fault::Protocol);
    }
    let mut hosts = Vec::new();
    for _ in 0..count {
        let size = usize::from(c.byte()?);
        if size == 0 || size > 253 {
            return Err(Fault::Cap);
        }
        let host = std::str::from_utf8(c.take(size)?).map_err(|_| Fault::Input)?;
        let parsed = Spec::parse(host)?;
        if parsed.hosts.len() != 1
            || !parsed.literals.is_empty()
            || parsed.hosts.first().map(String::as_str) != Some(host)
        {
            return Err(Fault::Protocol);
        }
        hosts.push(host.to_owned());
    }
    c.end()?;
    Ok((
        generation,
        Spec {
            literals: BTreeSet::new(),
            hosts,
        },
    ))
}
#[derive(Debug, PartialEq, Eq)]
struct Candidate {
    addresses: BTreeSet<IpAddr>,
    complete: bool,
}
fn response(raw: &[u8], spec: &Spec, generation: u64) -> Result<Candidate, Fault> {
    if raw.len() > OUTPUT {
        return Err(Fault::Cap);
    }
    let mut c = Cursor { data: raw, at: 0 };
    if c.take(4)? != b"NA01"
        || c.generation()? != generation
        || usize::from(c.byte()?) != spec.hosts.len()
    {
        return Err(Fault::Protocol);
    }
    let mut union = BTreeSet::new();
    let mut records = 0usize;
    let mut complete = true;
    for ordinal in 0..spec.hosts.len() {
        if usize::from(c.byte()?) != ordinal {
            return Err(Fault::Protocol);
        }
        let status = c.byte()?;
        let count = usize::from(u16::from_be_bytes(
            c.take(2)?.try_into().map_err(|_| Fault::Protocol)?,
        ));
        if status > 1 || (status == 0 && count == 0) || (status == 1 && count != 0) {
            return Err(Fault::Protocol);
        }
        if status == 1 {
            complete = false;
        }
        if records.checked_add(count).is_none_or(|n| n > IPS) {
            return Err(Fault::Cap);
        }
        records += count;
        for _ in 0..count {
            let size = usize::from(c.byte()?);
            if size == 0 || size > 64 {
                return Err(Fault::Cap);
            }
            let addr = ip(std::str::from_utf8(c.take(size)?).map_err(|_| Fault::Input)?)?;
            if !union.contains(&addr) && union.len() >= IPS {
                return Err(Fault::Cap);
            }
            union.insert(addr);
        }
    }
    c.end()?;
    Ok(Candidate {
        addresses: union,
        complete,
    })
}
/// Main OS thread only; dispatched before Tokio. No App, storage, logging or listener.
/// std/libc/NSS may block/allocate before returning the iterator (source-only qualification).
pub fn helper_main() -> Result<(), Fault> {
    let mut raw = Vec::new();
    std::io::stdin()
        .take((RAW + 1) as u64)
        .read_to_end(&mut raw)
        .map_err(|_| Fault::Protocol)?;
    let (generation, spec) = parse_request(&raw)?;
    let deadline = Instant::now() + ATTEMPT;
    let mut out = Vec::new();
    append(&mut out, b"NA01", OUTPUT)?;
    append(&mut out, &generation.to_be_bytes(), OUTPUT)?;
    append(
        &mut out,
        &[u8::try_from(spec.hosts.len()).map_err(|_| Fault::Cap)?],
        OUTPUT,
    )?;
    let mut total = 0usize;
    for (ordinal, host) in spec.hosts.iter().enumerate() {
        if Instant::now() >= deadline {
            return Err(Fault::Deadline);
        }
        let mut values = Vec::new();
        let mut status = 0u8;
        match (host.as_str(), 0).to_socket_addrs() {
            Ok(answers) => {
                for address in answers {
                    if Instant::now() >= deadline {
                        return Err(Fault::Deadline);
                    }
                    total += 1;
                    if total > IPS {
                        return Err(Fault::Cap);
                    }
                    values.push(address.ip());
                }
                if values.is_empty() {
                    status = 1;
                }
            }
            Err(_) => status = 1,
        }
        append(
            &mut out,
            &[u8::try_from(ordinal).map_err(|_| Fault::Cap)?, status],
            OUTPUT,
        )?;
        append(
            &mut out,
            &u16::try_from(values.len())
                .map_err(|_| Fault::Cap)?
                .to_be_bytes(),
            OUTPUT,
        )?;
        for value in values {
            let text = value.to_string();
            append(
                &mut out,
                &[u8::try_from(text.len()).map_err(|_| Fault::Cap)?],
                OUTPUT,
            )?;
            append(&mut out, text.as_bytes(), OUTPUT)?;
        }
    }
    if Instant::now() >= deadline {
        return Err(Fault::Deadline);
    }
    std::io::stdout()
        .write_all(&out)
        .map_err(|_| Fault::Protocol)
}
enum Executable {
    Trusted,
    #[cfg(test)]
    Fixture(std::path::PathBuf),
    #[cfg(test)]
    PanicFixture(std::path::PathBuf),
}
struct Owner {
    spec: Spec,
    snapshot: Mutex<Arc<Snapshot>>,
    stop: AtomicBool,
    quarantine: AtomicBool,
    lease: AtomicBool,
    generation: AtomicU64,
    handle: Mutex<Option<JoinHandle<Result<(), Fault>>>>,
    child: Mutex<Option<Child>>,
    quarantined_snapshot: Arc<Snapshot>,
    stopping_snapshot: Arc<Snapshot>,
}
static GLOBAL: Mutex<Option<Arc<Owner>>> = Mutex::new(None);
#[derive(Clone)]
#[must_use = "retain the owner and explicitly shutdown/join it"]
pub struct Resolver {
    owner: Arc<Owner>,
}
impl Resolver {
    pub fn new(spec: Spec) -> Result<Self, Fault> {
        Self::construct(spec, Executable::Trusted)
    }
    fn construct(spec: Spec, fixture: Executable) -> Result<Self, Fault> {
        let mut registry = GLOBAL.try_lock().map_err(|_| Fault::Busy)?;
        if let Some(owner) = registry.as_ref() {
            if owner.spec != spec {
                return Err(Fault::Conflict);
            }
            if owner.stop.load(Ordering::Acquire) || owner.quarantine.load(Ordering::Acquire) {
                return Err(Fault::Stopping);
            }
            return Ok(Self {
                owner: owner.clone(),
            });
        }
        let initial = if !spec.enabled() {
            Snapshot::Disabled
        } else if spec.hosts.is_empty() {
            Snapshot::Ready {
                generation: 0,
                expires: None,
                deny_union: spec.literals.clone(),
            }
        } else {
            Snapshot::Pending {
                deny_union: spec.literals.clone(),
            }
        };
        let owner = Arc::new(Owner {
            spec,
            snapshot: Mutex::new(Arc::new(initial)),
            stop: AtomicBool::new(false),
            quarantine: AtomicBool::new(false),
            lease: AtomicBool::new(false),
            generation: AtomicU64::new(0),
            handle: Mutex::new(None),
            child: Mutex::new(None),
            quarantined_snapshot: Arc::new(Snapshot::Quarantined),
            stopping_snapshot: Arc::new(Snapshot::Stopping),
        });
        // Publish registry and retained controller handle before start permission.
        *registry = Some(owner.clone());
        if !owner.spec.hosts.is_empty() {
            let mut slot = owner.handle.try_lock().map_err(|_| Fault::Busy)?;
            let (tx, rx) = std::sync::mpsc::sync_channel(1);
            let worker = owner.clone();
            match thread::Builder::new()
                .name("code-net-owned-controller".into())
                .spawn(move || {
                    if rx.recv().is_err() {
                        return Err(Fault::Stopping);
                    }
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        controller(&worker, fixture)
                    }))
                    .unwrap_or(Err(Fault::Cleanup));
                    if result.is_err() {
                        worker.quarantine.store(true, Ordering::Release);
                    }
                    result
                }) {
                Ok(handle) => {
                    *slot = Some(handle);
                    if tx.try_send(()).is_err() {
                        owner.quarantine.store(true, Ordering::Release);
                        return Err(Fault::Cleanup);
                    }
                }
                Err(_) => {
                    owner.quarantine.store(true, Ordering::Release);
                    return Err(Fault::Spawn);
                }
            }
        }
        Ok(Self { owner })
    }
    /// Try-only Arc transfer; no address-vector recopy or queued mutex acquisition.
    pub fn snapshot(&self) -> Result<Arc<Snapshot>, Fault> {
        if self.owner.quarantine.load(Ordering::Acquire) {
            return Ok(self.owner.quarantined_snapshot.clone());
        }
        if self.owner.stop.load(Ordering::Acquire) {
            return Ok(self.owner.stopping_snapshot.clone());
        }
        self.owner
            .snapshot
            .try_lock()
            .map(|s| s.clone())
            .map_err(|_| Fault::Busy)
    }
    pub fn generation(&self) -> u64 {
        self.owner.generation.load(Ordering::Acquire)
    }
    pub fn leased(&self) -> bool {
        self.owner.lease.load(Ordering::Acquire)
    }
    /// Cancellation requests do not release the child or controller lease.
    pub fn request_shutdown(&self) {
        self.owner.stop.store(true, Ordering::Release);
    }
    /// Explicit positive join; unfinished/failed cleanup keeps global capacity retained.
    /// is_finished merely avoids waiting: only join plus no child lease permits release.
    pub fn shutdown(&self) -> Result<(), Fault> {
        self.request_shutdown();
        let mut registry = GLOBAL.try_lock().map_err(|_| Fault::Busy)?;
        let mut slot = self.owner.handle.try_lock().map_err(|_| Fault::Busy)?;
        if let Some(handle) = slot.as_ref() {
            if !handle.is_finished() {
                return Err(Fault::Busy);
            }
        }
        if let Some(handle) = slot.take() {
            if !matches!(handle.join(), Ok(Ok(()))) {
                self.owner.quarantine.store(true, Ordering::Release);
            }
        }
        // A controller panic cannot discard its retained Child. Poison is only
        // recovered here for explicit cleanup, never for ordinary admission.
        let mut child_slot = match self.owner.child.try_lock() {
            Ok(slot) => slot,
            Err(std::sync::TryLockError::Poisoned(e)) => e.into_inner(),
            Err(_) => return Err(Fault::Busy),
        };
        if let Some(child) = child_slot.as_mut() {
            let _ = child.kill();
            match child.try_wait() {
                Ok(Some(_)) => {
                    child_slot.take();
                    self.owner.lease.store(false, Ordering::Release);
                }
                Ok(None) => return Err(Fault::Busy),
                Err(_) => {
                    self.owner.quarantine.store(true, Ordering::Release);
                    return Err(Fault::Cleanup);
                }
            }
        }
        if self.owner.lease.load(Ordering::Acquire) || self.owner.quarantine.load(Ordering::Acquire)
        {
            return Err(Fault::Cleanup);
        }
        if registry
            .as_ref()
            .is_some_and(|o| Arc::ptr_eq(o, &self.owner))
        {
            *registry = None;
        }
        Ok(())
    }
}
fn publish(owner: &Owner, value: Snapshot) -> Result<(), Fault> {
    *owner.snapshot.try_lock().map_err(|_| Fault::Busy)? = Arc::new(value);
    Ok(())
}
fn nonblocking(fd: impl std::os::fd::AsFd) -> Result<(), Fault> {
    let flags = rustix::fs::fcntl_getfl(&fd).map_err(|_| Fault::Spawn)?;
    rustix::fs::fcntl_setfl(&fd, flags | rustix::fs::OFlags::NONBLOCK).map_err(|_| Fault::Spawn)
}
fn drain(reader: &mut impl Read, output: &mut Vec<u8>, cap: usize) -> Result<bool, Fault> {
    let mut bytes = [0u8; 1024];
    // Fixed reads per tick prevent a flood from monopolising deadline checks.
    for _ in 0..8 {
        match reader.read(&mut bytes) {
            Ok(0) => return Ok(true),
            Ok(n) => append(output, bytes.get(..n).ok_or(Fault::Protocol)?, cap)?,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(false),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return Err(Fault::Protocol),
        }
    }
    Ok(false)
}
fn attempt(owner: &Owner, generation: u64, fixture: &Executable) -> Result<Candidate, Fault> {
    let deadline = Instant::now() + ATTEMPT;
    let frame = request(&owner.spec, generation)?;
    let executable = match fixture {
        Executable::Trusted => std::env::current_exe().map_err(|_| Fault::Spawn)?,
        #[cfg(test)]
        Executable::Fixture(p) => p.to_owned(),
        #[cfg(test)]
        Executable::PanicFixture(p) => p.to_owned(),
    };

    let mut command = Command::new(executable);
    command
        .env_clear()
        .arg(HELPER_ARG)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut retained = owner.child.try_lock().map_err(|_| Fault::Busy)?;
    if retained.is_some() {
        return Err(Fault::Cleanup);
    }
    owner.lease.store(true, Ordering::Release);
    let child = match command.spawn() {
        Ok(c) => c,
        Err(_) => {
            owner.lease.store(false, Ordering::Release);
            return Err(Fault::Spawn);
        }
    };
    *retained = Some(child);
    #[cfg(test)]
    if matches!(fixture, Executable::PanicFixture(_)) {
        tests::injected_controller_panic();
    }
    let child = retained.as_mut().ok_or(Fault::Cleanup)?;
    let result = transfer(child, &frame, owner, deadline);
    // No early return after spawn: child remains retained through kill/reap failure.
    if result.is_err() {
        let _ = child.kill();
    }
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                retained.take();
                owner.lease.store(false, Ordering::Release);
                if !status.success() {
                    return Err(Fault::Protocol);
                }
                let answer = response(&result?, &owner.spec, generation);
                if Instant::now() >= deadline {
                    return Err(Fault::Deadline);
                }
                return answer;
            }
            Ok(None) => {
                if owner.stop.load(Ordering::Acquire) || Instant::now() >= deadline {
                    let _ = child.kill();
                }
            }
            Err(_) => {
                owner.quarantine.store(true, Ordering::Release);
                let _ = child.kill();
            }
        }
        // Unconfirmed reap retains Child and lease; no replacement child/controller.
        thread::park_timeout(Duration::from_millis(5));
    }
}
fn transfer(
    child: &mut Child,
    frame: &[u8],
    owner: &Owner,
    deadline: Instant,
) -> Result<Vec<u8>, Fault> {
    let input = child.stdin.take().ok_or(Fault::Spawn)?;
    let mut output = child.stdout.take().ok_or(Fault::Spawn)?;
    let mut errors = child.stderr.take().ok_or(Fault::Spawn)?;
    nonblocking(&input)?;
    nonblocking(&output)?;
    nonblocking(&errors)?;
    let mut offset = 0usize;
    let mut input = Some(input);
    let mut out = Vec::new();
    let mut err = Vec::new();
    let mut out_eof = false;
    let mut err_eof = false;
    loop {
        if owner.stop.load(Ordering::Acquire) {
            return Err(Fault::Stopping);
        }
        if Instant::now() >= deadline {
            return Err(Fault::Deadline);
        }
        if let Some(writer) = input.as_mut() {
            let remaining = frame.get(offset..).ok_or(Fault::Protocol)?;
            match writer.write(remaining) {
                Ok(0) => return Err(Fault::Protocol),
                Ok(n) => offset = offset.checked_add(n).ok_or(Fault::Cap)?,
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                    ) => {}
                Err(_) => return Err(Fault::Protocol),
            }
            if offset == frame.len() {
                input = None;
            }
        }
        if !out_eof {
            out_eof = drain(&mut output, &mut out, OUTPUT)?;
        }
        if !err_eof {
            err_eof = drain(&mut errors, &mut err, STDERR)?;
        }
        if !err.is_empty() {
            return Err(Fault::Protocol);
        }
        if out_eof && err_eof {
            return Ok(out);
        }
        thread::park_timeout(Duration::from_millis(5));
    }
}
fn merge(union: &mut BTreeSet<IpAddr>, found: BTreeSet<IpAddr>) -> Result<(), Fault> {
    for addr in found {
        if !union.contains(&addr) && union.len() >= IPS {
            return Err(Fault::Cap);
        }
        union.insert(addr);
    }
    Ok(())
}
fn backoff(wait: Duration) -> Duration {
    (wait * 2).min(Duration::from_secs(300))
}
fn controller(owner: &Owner, fixture: Executable) -> Result<(), Fault> {
    let mut union = owner.spec.literals.clone();
    let mut generation = 0u64;
    let mut wait = Duration::from_secs(5);
    let mut next = Instant::now();
    loop {
        if owner.stop.load(Ordering::Acquire) {
            return Ok(());
        }
        if Instant::now() < next {
            thread::park_timeout(Duration::from_millis(10));
            continue;
        }
        generation = generation.checked_add(1).ok_or(Fault::Cap)?;
        owner.generation.store(generation, Ordering::Release);
        match attempt(owner, generation, &fixture) {
            Ok(found) => {
                if merge(&mut union, found.addresses).is_err() {
                    owner.quarantine.store(true, Ordering::Release);
                    return Err(Fault::Cap);
                }
                let now = Instant::now();
                if found.complete {
                    publish(
                        owner,
                        Snapshot::Ready {
                            generation,
                            expires: Some(now + TTL),
                            deny_union: union.clone(),
                        },
                    )?;
                    wait = Duration::from_secs(5);
                    next = now + TTL;
                } else {
                    // A valid typed NX/no-answer frame preserves every partial answer,
                    // without claiming a complete permissive generation.
                    publish(
                        owner,
                        Snapshot::Pending {
                            deny_union: union.clone(),
                        },
                    )?;
                    next = now + wait;
                    wait = backoff(wait);
                }
            }
            Err(fault) => {
                if owner.stop.load(Ordering::Acquire) {
                    return Ok(());
                }
                if fault != Fault::Spawn {
                    // Unknown/truncated/capped/timed-out history cannot be forgotten
                    // before a later Ready. Quarantine rather than retry around it.
                    owner.quarantine.store(true, Ordering::Release);
                    return Err(fault);
                }
                next = Instant::now() + wait;
                wait = backoff(wait);
            }
        }
    }
}
#[cfg(test)]
#[path = "../tests/support/resolver_controls.rs"]
mod tests;

/// CLOSED sibling controls only; production executable selection remains unchanged.
#[cfg(test)]
pub(crate) fn closed_fixture(spec: Spec, path: std::path::PathBuf) -> Result<Resolver, Fault> {
    Resolver::construct(spec, Executable::Fixture(path))
}
