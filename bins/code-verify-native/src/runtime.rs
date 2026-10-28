//! Linux namespace-init execution authority. Never run as a privileged host test.
use super::budget::{bootstrap_deadline, bootstrap_live, Admission, Budget};
use super::engine::{self, tree_permissions, Config, Execution, Identity, Plan, Runner};
use super::lifecycle::{Children, Terminal};
use code_verify_native::{Commands, Request, Tail, MAX_REQUEST_BYTES, MAX_TAIL_BYTES};
use nix::errno::Errno;
use nix::sys::resource::{getrlimit, setrlimit, Resource};
use nix::sys::signal::{SigSet, Signal as NixSignal};
use nix::sys::signalfd::{SfdFlags, SignalFd};
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::{
    getgid, getpid, getppid, getuid, setgroups, setresgid, setresuid, Gid, Pid, Uid,
};
use rustix::fs::{fcntl_getfl, fcntl_setfl, open, openat, Mode, OFlags};
use rustix::net::sockopt::socket_peercred;
use rustix::net::{recv, RecvFlags};
use rustix::process::{pidfd_open, pidfd_send_signal, PidfdFlags, Signal};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
const TICK: Duration = Duration::from_millis(5);
const BOOTSTRAP_CAP: u64 = 64 * 1024;
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    deadline_ns: u64,
    plan: Plan,
}
fn monotonic_ns() -> Result<u64> {
    let t = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    let seconds = u64::try_from(t.tv_sec)?;
    let nanos = u64::try_from(t.tv_nsec)?;
    if nanos >= 1_000_000_000 {
        return Err("monotonic clock evidence".into());
    }
    seconds
        .checked_mul(1_000_000_000)
        .and_then(|v| v.checked_add(nanos))
        .ok_or_else(|| "monotonic clock range".into())
}
fn status_field<'a>(text: &'a str, key: &str) -> Result<&'a str> {
    Ok(text
        .lines()
        .find_map(|l| l.strip_prefix(key))
        .ok_or("missing process evidence")?
        .trim())
}
fn privileged_shape() -> Result<()> {
    let text = fs::read_to_string("/proc/self/status")?;
    if getuid().as_raw() != 0
        || getgid().as_raw() != 0
        || status_field(&text, "NoNewPrivs:")? != "1"
        || u64::from_str_radix(status_field(&text, "CapEff:")?, 16)? != 0xc0
        || u64::from_str_radix(status_field(&text, "CapPrm:")?, 16)? != 0xc0
        || u64::from_str_radix(status_field(&text, "CapInh:")?, 16)? != 0
        || u64::from_str_radix(status_field(&text, "CapAmb:")?, 16)? != 0
        || u64::from_str_radix(status_field(&text, "SigIgn:")?, 16)? & (1 << 16) != 0
        || status_field(&text, "Threads:")? != "1"
    {
        return Err("isolation prerequisite".into());
    }
    Ok(())
}
fn same_executable() -> Result<()> {
    let a = fs::metadata(std::env::current_exe()?)?;
    let b = fs::metadata("/proc/1/exe")?;
    if (a.dev(), a.ino()) != (b.dev(), b.ino()) {
        return Err("untrusted init".into());
    }
    Ok(())
}
fn drop_identity(who: Identity) -> Result<()> {
    setgroups(&[])?;
    let gid = Gid::from_raw(who.gid);
    let uid = Uid::from_raw(who.uid);
    setresgid(gid, gid, gid)?;
    setresuid(uid, uid, uid)?;
    let s = fs::read_to_string("/proc/self/status")?;
    let uids: Vec<u32> = status_field(&s, "Uid:")?
        .split_whitespace()
        .map(str::parse)
        .collect::<std::result::Result<_, _>>()?;
    let gids: Vec<u32> = status_field(&s, "Gid:")?
        .split_whitespace()
        .map(str::parse)
        .collect::<std::result::Result<_, _>>()?;
    if uids.len() != 4
        || uids.iter().any(|x| *x != who.uid)
        || gids.len() != 4
        || gids.iter().any(|x| *x != who.gid)
        || !status_field(&s, "Groups:")?.is_empty()
        || u64::from_str_radix(status_field(&s, "CapEff:")?, 16)? != 0
        || u64::from_str_radix(status_field(&s, "CapPrm:")?, 16)? != 0
        || status_field(&s, "NoNewPrivs:")? != "1"
    {
        return Err("identity drop".into());
    }
    Ok(())
}
fn helper() -> Result<()> {
    privileged_shape()?;
    same_executable()?;
    if getppid().as_raw() != 1 || getpid().as_raw() == 1 {
        return Err("helper parent".into());
    }
    let stdin = io::stdin();
    let creds = socket_peercred(&stdin)?;
    if creds.pid.as_raw_nonzero().get() != 1 || creds.uid.as_raw() != 0 || creds.gid.as_raw() != 0 {
        return Err("bootstrap peer".into());
    }
    let mut channel = UnixStream::from(stdin.as_fd().try_clone_to_owned()?);
    channel.set_read_timeout(Some(Duration::from_secs(2)))?;
    let mut bytes = Vec::new();
    (&mut channel)
        .take(BOOTSTRAP_CAP + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > BOOTSTRAP_CAP {
        return Err("bootstrap cap".into());
    }
    let envelope: Envelope = serde_json::from_slice(&bytes)?;
    if !bootstrap_live(monotonic_ns()?, envelope.deadline_ns) {
        return Err("expired helper admission".into());
    }
    let plan = envelope.plan;
    drop(channel);
    let who = plan.who();
    // Root is allowed only for exact-pidfd signalling of a still-privileged owned helper.
    if who.uid == 0 && !matches!(plan, Plan::Signal { .. }) {
        return Err("root execution refused".into());
    }
    if who.uid != 0 {
        who.validate_worker()?;
        drop_identity(who)?;
    }
    rustix::process::umask(Mode::from_raw_mode(0o022));
    match plan {
        Plan::Signal { .. } => {
            pidfd_send_signal(io::stdout().as_fd(), Signal::KILL)?;
        }
        Plan::Prepare { directories, .. } => {
            for d in directories {
                fs::create_dir(&d)?;
                fs::set_permissions(d, fs::Permissions::from_mode(0o700))?;
            }
        }
        Plan::Readonly { tree, parent, .. } => {
            tree_permissions(&tree, true)?;
            fs::set_permissions(parent, fs::Permissions::from_mode(0o555))?;
        }
        Plan::Remove { path, .. } => {
            if path.try_exists()? {
                tree_permissions(&path, false)?;
                fs::remove_dir_all(path)?;
            }
        }
        Plan::Exec {
            program,
            args,
            cwd,
            env,
            cpu,
            ..
        } => {
            super::session::enter_untrusted_session()?;
            setrlimit(Resource::RLIMIT_CORE, 0, 0)?;
            let (_, hard) = getrlimit(Resource::RLIMIT_CPU)?;
            let limit = cpu.min(hard).max(1);
            setrlimit(Resource::RLIMIT_CPU, limit, limit)?;
            // Do not leak the init supervisor's blocked termination signals into repository code.
            SigSet::empty().thread_set_mask()?;
            // Every inherited Rust-owned fd is CLOEXEC. Replace bootstrap fd0 with /dev/null.
            // No plan channel/listener/pidfd reaches Git or the operator command.
            if !bootstrap_live(monotonic_ns()?, envelope.deadline_ns) {
                return Err("expired exec admission".into());
            }
            return Err(Command::new(program)
                .args(args)
                .current_dir(cwd)
                .env_clear()
                .envs(env)
                .stdin(Stdio::null())
                .exec()
                .into());
        }
    }
    Ok(())
}
struct Authority {
    executable: PathBuf,
    git: Identity,
    test: Identity,
    children: Children,
    shutdown: SignalFd,
    stopping: bool,
}
impl Authority {
    fn stopping(&mut self) -> Result<bool> {
        if self.shutdown.read_signal()?.is_some() {
            self.stopping = true;
        }
        Ok(self.stopping)
    }
    fn collect(&mut self) -> Result<bool> {
        // A continuously exiting fork tree cannot monopolize the deadline/cancellation loop.
        for _ in 0..512 {
            match waitpid(
                Pid::from_raw(-1),
                Some(WaitPidFlag::WNOHANG | WaitPidFlag::__WALL),
            ) {
                Ok(WaitStatus::Exited(pid, code)) => self.children.terminal(
                    pid.as_raw(),
                    Terminal {
                        code: Some(code),
                        signal: None,
                    },
                ),
                Ok(WaitStatus::Signaled(pid, signal, _)) => self.children.terminal(
                    pid.as_raw(),
                    Terminal {
                        code: None,
                        signal: Some(signal as i32),
                    },
                ),
                Ok(WaitStatus::StillAlive) => {
                    self.children.still_alive();
                    return Ok(false);
                }
                Ok(_) => self.children.still_alive(),
                Err(Errno::EINTR) => {}
                Err(Errno::ECHILD) => {
                    self.children.all_child_echild()?;
                    return Ok(true);
                }
                Err(e) => return Err(e.into()),
            }
        }
        self.children.still_alive();
        Ok(false)
    }
    fn spawn(
        &mut self,
        plan: &Plan,
        signal_fd: Option<OwnedFd>,
        admission: Admission,
    ) -> Result<(Child, u64)> {
        let stamp = monotonic_ns()?;
        let remaining = admission
            .deadline()
            .checked_duration_since(Instant::now())
            .ok_or("expired spawn admission")?;
        let deadline_ns = bootstrap_deadline(stamp, remaining)?;
        let bytes =
            serde_json::to_vec(&serde_json::json!({"deadline_ns":deadline_ns,"plan":plan}))?;
        if bytes.len() as u64 > BOOTSTRAP_CAP {
            return Err("plan too large".into());
        }
        let (mut parent, child) = UnixStream::pair()?;
        parent.set_write_timeout(Some(Duration::from_secs(2)))?;
        let mut command = Command::new(&self.executable);
        command
            .arg("--owned-helper")
            .env_clear()
            .current_dir("/")
            .stdin(Stdio::from(OwnedFd::from(child)))
            .stdout(signal_fd.map_or_else(Stdio::piped, Stdio::from))
            .stderr(Stdio::piped());
        if !bootstrap_live(monotonic_ns()?, deadline_ns) {
            return Err("expired prepared admission".into());
        }
        let child = command.spawn()?;
        let ticket = self.children.register(i32::try_from(child.id())?)?;
        // A write failure is cleanup uncertainty: the server exits, never passes.
        parent.write_all(&bytes)?;
        parent.shutdown(Shutdown::Write)?;
        drop(parent);
        Ok((child, ticket))
    }
    fn direct(&self) -> Result<Vec<(i32, Identity, OwnedFd)>> {
        let text = fs::read_to_string("/proc/1/task/1/children")?;
        let mut result = Vec::new();
        for token in text.split_whitespace() {
            let pid: i32 = token.parse()?;
            if pid <= 1 {
                return Err("child identity".into());
            }
            let p = PathBuf::from(format!("/proc/{pid}/status"));
            let before = match fs::read_to_string(&p) {
                Ok(s) => s,
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e.into()),
            };
            if status_field(&before, "PPid:")? != "1" {
                continue;
            }
            let uid: u32 = status_field(&before, "Uid:")?
                .split_whitespace()
                .next()
                .ok_or("uid")?
                .parse()?;
            let who = if uid == 0 {
                Identity { uid: 0, gid: 0 }
            } else if uid == self.git.uid {
                self.git
            } else if uid == self.test.uid {
                self.test
            } else {
                return Err("unexpected child identity".into());
            };
            let Some(p) = rustix::process::Pid::from_raw(pid) else {
                return Err("child pid".into());
            };
            let fd = match pidfd_open(p, PidfdFlags::empty()) {
                Ok(fd) => fd,
                Err(rustix::io::Errno::SRCH) => continue,
                Err(e) => return Err(e.into()),
            };
            // No numeric PID signal. Revalidate parent and UID after opening its handle.
            let after = match fs::read_to_string(p_path(pid)) {
                Ok(s) => s,
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e.into()),
            };
            if status_field(&after, "PPid:")? != "1"
                || status_field(&after, "Uid:")? != status_field(&before, "Uid:")?
            {
                continue;
            }
            result.push((pid, who, fd));
        }
        Ok(result)
    }
    fn drain(&mut self) -> Result<()> {
        let end = Instant::now() + Duration::from_secs(5);
        loop {
            if self.collect()? {
                return Ok(());
            }
            if Instant::now() >= end {
                return Err("owned drain uncertain".into());
            }
            let victims = self.direct()?;
            for (_pid, who, fd) in victims {
                // Check whether a live target is ours through the descriptor, never UID selection.
                let (_child, id) = self.spawn(
                    &Plan::Signal { who },
                    Some(fd),
                    Budget::new(end).admit(Instant::now())?,
                )?;
                loop {
                    self.collect()?;
                    if let Some(s) = self.children.take(id) {
                        // ESRCH raced exit is harmless only because the final all-child wait still follows.
                        if s.code != Some(0) && Instant::now() >= end {
                            return Err("signal helper uncertain".into());
                        }
                        break;
                    }
                    if Instant::now() >= end {
                        return Err("signal helper timeout".into());
                    }
                    sleep(TICK);
                }
            }
            sleep(TICK);
        }
    }
}
fn p_path(pid: i32) -> PathBuf {
    PathBuf::from(format!("/proc/{pid}/status"))
}
fn nonblocking(fd: impl AsFd) -> Result<()> {
    let f = fcntl_getfl(&fd)?;
    fcntl_setfl(fd, f | OFlags::NONBLOCK)?;
    Ok(())
}
fn connected(s: &UnixStream) -> Result<bool> {
    let mut b = [0u8; 1];
    match recv(s, &mut b, RecvFlags::PEEK | RecvFlags::DONTWAIT) {
        Ok((0, _)) => Ok(false),
        Ok(_) => Ok(true),
        Err(rustix::io::Errno::AGAIN | rustix::io::Errno::INTR) => Ok(true),
        Err(_) => Ok(false),
    }
}
fn pump(r: &mut impl Read, tail: &mut Tail, combined: &mut Tail) -> Result<bool> {
    let mut b = [0u8; 8192];
    for _ in 0..8 {
        match r.read(&mut b) {
            Ok(0) => return Ok(true),
            Ok(n) => {
                let raw = b.get(..n).ok_or("read length")?;
                tail.push(raw);
                combined.push(raw);
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(false),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(false)
}
fn execute(
    a: &mut Authority,
    plan: &Plan,
    socket: &UnixStream,
    deadline: Instant,
    cap: usize,
) -> Result<Execution> {
    if a.stopping()? {
        return Err("stopping".into());
    }
    let mut budget = Budget::new(deadline);
    let (mut child, id) = a.spawn(plan, None, budget.admit(Instant::now())?)?;
    let mut stdout = child.stdout.take().ok_or("stdout")?;
    let mut stderr = child.stderr.take().ok_or("stderr")?;
    nonblocking(&stdout)?;
    nonblocking(&stderr)?;
    let mut out = Tail::new(cap);
    let mut err = Tail::new(cap);
    let mut combined = Tail::new(cap);
    let mut done_out = false;
    let mut done_err = false;
    let mut timed_out = false;
    let mut terminal = loop {
        done_out |= pump(&mut stdout, &mut out, &mut combined)?;
        done_err |= pump(&mut stderr, &mut err, &mut combined)?;
        a.collect()?;
        let observed = a.children.take(id);
        if let Some(completion) = budget.observe(Instant::now(), observed) {
            timed_out = completion.timed_out;
            break completion.terminal.unwrap_or(Terminal {
                code: None,
                signal: None,
            });
        }
        if a.stopping()? || !connected(socket)? {
            break Terminal {
                code: None,
                signal: None,
            };
        }
        sleep(TICK);
    };
    // Captured direct-worker status is authoritative; no Child::wait races the all-child collector.
    a.drain()?;
    if terminal.code.is_none() && terminal.signal.is_none() {
        terminal = a.children.take(id).ok_or("missing worker status")?;
    }
    let end = Instant::now() + Duration::from_millis(500);
    while !done_out || !done_err {
        done_out |= pump(&mut stdout, &mut out, &mut combined)?;
        done_err |= pump(&mut stderr, &mut err, &mut combined)?;
        if Instant::now() >= end && (!done_out || !done_err) {
            return Err("pipe ownership uncertain".into());
        }
        sleep(TICK);
    }
    if a.stopping()? || !connected(socket)? {
        return Err("cancelled".into());
    }
    timed_out |= budget.expired(Instant::now());
    Ok(Execution {
        terminal,
        out: out.read(),
        timed_out,
        combined: combined.read(),
    })
}
fn positive(raw: Option<String>, fallback: u64) -> u64 {
    code_verify_native::operator_positive(raw.as_deref(), fallback)
}
fn constructor_option(raw: Option<String>, fallback: u64) -> u64 {
    let n = positive(raw, fallback);
    if n == 0 {
        fallback
    } else {
        n
    }
}
fn trusted_directory(p: &Path, uid: u32, group_write: bool) -> Result<PathBuf> {
    let real = fs::canonicalize(p)?;
    if real != p {
        return Err("directory alias".into());
    }
    let m = fs::symlink_metadata(p)?;
    if !m.is_dir() || m.uid() != uid || m.mode() & (if group_write { 0o002 } else { 0o022 }) != 0 {
        return Err("directory ownership".into());
    }
    Ok(real)
}
fn config() -> Result<(Config, Authority)> {
    privileged_shape()?;
    same_executable()?;
    if getpid().as_raw() != 1 {
        return Err("namespace init required".into());
    }
    let git = Identity::worker(&std::env::var("VERIFY_GIT_USER")?)?;
    let test = Identity::worker(&std::env::var("VERIFY_TEST_USER")?)?;
    if git.uid == test.uid {
        return Err("distinct identities".into());
    }
    let root = fs::canonicalize(std::env::var("VERIFY_SOURCE_ROOT")?)?;
    let copy = trusted_directory(
        Path::new(&std::env::var("VERIFY_COPY_ROOT").unwrap_or_else(|_| "/verify/copy".into())),
        git.uid,
        false,
    )?;
    let scratch = trusted_directory(
        Path::new(&std::env::var("VERIFY_SCRATCH_ROOT").unwrap_or_else(|_| "/verify/run".into())),
        test.uid,
        false,
    )?;
    let control = trusted_directory(
        Path::new(&std::env::var("VERIFY_CTL_ROOT").unwrap_or_else(|_| "/tmp".into())),
        0,
        false,
    )?;
    let socket = PathBuf::from(std::env::var("VERIFY_SOCKET")?);
    let parent = socket.parent().ok_or("socket parent")?;
    let parent = trusted_directory(parent, 0, true)?;
    let m = fs::metadata(&parent)?;
    if m.gid() != 0 || m.mode() & 0o007 != 0 {
        return Err("socket isolation".into());
    }
    let roots = [&root, &copy, &scratch, &control, &parent];
    if roots
        .iter()
        .copied()
        .collect::<std::collections::HashSet<_>>()
        .len()
        != 5
    {
        return Err("duplicate authority roots".into());
    }
    for a in [&root, &copy, &scratch, &control, &parent] {
        for b in [&root, &copy, &scratch, &control, &parent] {
            if a != b && (a.starts_with(b) || b.starts_with(a)) {
                return Err("overlapping authority roots".into());
            }
        }
    }
    let c = Config {
        root,
        copy,
        scratch,
        control,
        socket,
        commands: Commands::parse(&std::env::var("CODE_VERIFY").unwrap_or_default()),
        path: std::env::var("PATH").unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin".into()),
        wall: constructor_option(std::env::var("CODE_VERIFY_WALL_MS").ok(), 600_000),
        tail: usize::try_from(constructor_option(
            std::env::var("CODE_VERIFY_TAIL_BYTES").ok(),
            16384,
        ))?
        .min(MAX_TAIL_BYTES),
        min_uptime: positive(std::env::var("VERIFY_MIN_UPTIME_MS").ok(), 11000),
    };
    let mut signals = SigSet::empty();
    for signal in [
        NixSignal::SIGTERM,
        NixSignal::SIGINT,
        NixSignal::SIGHUP,
        NixSignal::SIGQUIT,
    ] {
        signals.add(signal);
    }
    signals.thread_block()?;
    let shutdown = SignalFd::with_flags(&signals, SfdFlags::SFD_CLOEXEC | SfdFlags::SFD_NONBLOCK)?;
    let mut a = Authority {
        executable: std::env::current_exe()?,
        git,
        test,
        children: Children::default(),
        shutdown,
        stopping: false,
    };
    if !a.collect()? {
        return Err("unexpected startup children".into());
    }
    let groups = fs::read_to_string("/proc/self/status")?;
    if status_field(&groups, "Groups:")?
        .split_whitespace()
        .any(|v| v.parse::<u32>().map_or(true, |g| g != 0 && g != git.gid))
    {
        return Err("unexpected server groups".into());
    }
    // Prove the pidfd kernel prerequisite without signalling anything.
    let _self_handle = pidfd_open(rustix::process::getpid(), PidfdFlags::empty())?;
    Ok((c, a))
}
fn source_config(root: &Path, source: &Path) -> Result<Vec<u8>> {
    let rel = source.strip_prefix(root)?;
    let mut fd = open(
        root,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    for component in rel
        .components()
        .chain([Component::Normal(std::ffi::OsStr::new(".git"))])
    {
        let Component::Normal(name) = component else {
            return Err("source component".into());
        };
        fd = openat(
            &fd,
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
    }
    let file = match openat(
        fd,
        "config",
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => File::from(fd),
        Err(rustix::io::Errno::NOENT) => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let m = file.metadata()?;
    if !m.is_file() || m.nlink() != 1 || m.len() > 1_048_576 {
        return Err("config file".into());
    }
    let mut data = Vec::new();
    file.take(1_048_577).read_to_end(&mut data)?;
    if data.len() > 1_048_576 {
        return Err("config cap".into());
    }
    Ok(data)
}
fn checked(a: &mut Authority, plan: Plan, s: &UnixStream, end: Instant) -> Result<String> {
    let x = execute(a, &plan, s, end, 1_048_576)?;
    if x.terminal.code != Some(0) || x.timed_out || x.out.truncated {
        return Err("verified operation failed".into());
    }
    Ok(x.out.tail.trim().to_owned())
}
struct ProcessRunner<'a> {
    authority: &'a mut Authority,
    socket: &'a UnixStream,
}
impl Runner for ProcessRunner<'_> {
    fn git(&self) -> Identity {
        self.authority.git
    }
    fn test(&self) -> Identity {
        self.authority.test
    }
    fn source_config(&self, root: &Path, source: &Path) -> Result<Vec<u8>> {
        source_config(root, source)
    }
    fn execute(&mut self, plan: &Plan, deadline: Instant, cap: usize) -> Result<Execution> {
        execute(self.authority, plan, self.socket, deadline, cap)
    }
}
fn run(c: &Config, a: &mut Authority, s: &UnixStream, r: &Request) -> Result<Value> {
    let result = engine::verify(
        c,
        &mut ProcessRunner {
            authority: a,
            socket: s,
        },
        r,
    )?;
    Ok(
        json!({"noevia":"verify-result","nonce":r.nonce,"ok":true,"repo":r.repo,"headSha":r.head_sha,
        "exitCode":result.terminal.code,"signal":result.terminal.signal.map(|n|nix::sys::signal::Signal::try_from(n).map(|signal|format!("{signal:?}"))).transpose()?,
        "timedOut":result.timed_out,"durationMs":result.duration_ms,"tail":result.output.tail,"tailBytes":result.output.tail_bytes,
        "totalBytes":result.output.total_bytes,"truncated":result.output.truncated}),
    )
}
fn serve(c: Config, mut a: Authority) -> Result<()> {
    let began = Instant::now();
    match fs::symlink_metadata(&c.socket) {
        Ok(m) => {
            if !m.file_type().is_socket() || m.uid() != 0 || m.nlink() != 1 {
                return Err("socket ownership".into());
            }
            fs::remove_file(&c.socket)?;
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    let listener = UnixListener::bind(&c.socket)?;
    fs::set_permissions(&c.socket, fs::Permissions::from_mode(0o660))?;
    listener.set_nonblocking(true)?;
    let (mut socket, _) = loop {
        if a.stopping()? {
            return Err("stopping".into());
        }
        match listener.accept() {
            Ok(connection) => break connection,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => sleep(TICK),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e.into()),
        }
    };
    drop(listener);
    socket.set_nonblocking(true)?;
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut line = Vec::new();
    let mut b = [0u8; 1024];
    let request = loop {
        if a.stopping()? || Instant::now() >= deadline {
            return Err("request timeout or stopping".into());
        }
        match socket.read(&mut b) {
            Ok(0) => return Err("disconnected".into()),
            Ok(n) => {
                for byte in b.get(..n).ok_or("read count")? {
                    if *byte == b'\n' {
                        break;
                    }
                    line.push(*byte);
                    if line.len() >= MAX_REQUEST_BYTES {
                        return Err("request cap".into());
                    }
                }
                if b.get(..n).ok_or("read count")?.contains(&b'\n') {
                    break Request::parse_line(&line).map_err(|_| "request refused")?;
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => sleep(TICK),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e.into()),
        }
    };
    let response = if c.commands.get(&request.repo).is_none() {
        json!({"noevia":"verify-result","nonce":request.nonce,"ok":false,"error":"not_configured","message":"No verification command is configured."})
    } else {
        match run(&c, &mut a, &socket, &request) {
            Ok(response) => response,
            Err(error) => {
                let code = error
                    .downcast_ref::<engine::Refusal>()
                    .map_or("checkout", |refusal| refusal.code());
                // Refuse only after proving all admitted work and cleanup helpers terminated.
                // If any proof/removal fails this propagates to namespace-init exit, with no result.
                a.drain()?;
                let cleanup_end = Instant::now() + Duration::from_secs(5);
                let test = a.test;
                let git = a.git;
                checked(
                    &mut a,
                    Plan::Remove {
                        who: test,
                        path: c.scratch.join(format!("noevia-verify-{}", request.nonce)),
                    },
                    &socket,
                    cleanup_end,
                )?;
                checked(
                    &mut a,
                    Plan::Remove {
                        who: git,
                        path: c.copy.join(format!("noevia-verify-{}", request.nonce)),
                    },
                    &socket,
                    cleanup_end,
                )?;
                let ctl = c.control.join(format!("noevia-verify-{}", request.nonce));
                if ctl.try_exists()? {
                    fs::remove_dir_all(ctl)?;
                }
                json!({"noevia":"verify-result","nonce":request.nonce,"ok":false,"error":code,"message":"Could not complete an isolated verification."})
            }
        }
    };
    // All possible process admission has completed; ECHILD again immediately before result.
    if a.stopping()? || !a.collect()? || !a.children.receipt_ready() {
        return Err("final child barrier".into());
    }
    socket.set_nonblocking(false)?;
    socket.set_write_timeout(Some(Duration::from_secs(2)))?;
    socket.write_all((serde_json::to_string(&response)? + "\n").as_bytes())?;
    socket.shutdown(Shutdown::Both)?;
    let minimum = Duration::from_millis(c.min_uptime);
    while began.elapsed() < minimum {
        if a.stopping()? {
            break;
        }
        sleep(TICK);
    }
    Ok(())
}
pub fn entry() -> Result<()> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args == [std::ffi::OsString::from("--owned-helper")] {
        return helper();
    }
    if !args.is_empty() {
        return Err("unsupported arguments".into());
    }
    let (c, a) = config()?;
    serve(c, a)
}
