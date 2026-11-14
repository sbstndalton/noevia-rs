//! Native port of noevia `build/web-supervisor.sh`, the web image's entrypoint (inert: no image
//! starts this binary yet; the script stays the entrypoint until a release switches it).
//!
//! noevia-server faces the network on UI_HOST:UI_PORT with Node behind it on UI_PORT+1000. Before
//! either starts, the selected front's `--features` is paired with the installed Core (its files and
//! exported markers): old/old and new/new start, a mix exits 2. Then both run under one pair lock
//! with a per-start store secret and start id; if either exits the other is stopped (the front
//! first, bounded, then Core) and the container exits non-zero; SIGTERM/SIGINT exit 0.
//!
//! With `NOEVIA_SUPERVISOR_FRONT_ONLY=1` (off by default) there is no Core: the front is prepared
//! and started alone (front_only.rs).
//!
//! Behaviour oracle: tests/parity/test-web-supervisor.sh (the script's own suite, copied with the
//! script at noevia 11bf2da7) runs against this binary in tests/parity.rs.

pub mod ensure;
pub mod env;
pub mod front_only;
pub mod pairing;
pub mod plan;
pub mod supervise;

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use env::Env;
use pairing::{Bound, CoreView, Features, NodeOut, Refusal};

/// The script's `timeout -k 2 10`, kept for the three probes it bounded (then KILL two seconds
/// after the TERM).
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
/// The probes and the `--features` query the script left unbounded: a cold or loaded host may be
/// slow there, so the limit is generous, and a run it stops refuses with its own message rather
/// than reading as a pairing mismatch (noevia#2702 item 1). It still keeps a hung require from
/// holding the container's start forever.
pub const OPEN_PROBE_TIMEOUT: Duration = Duration::from_secs(120);
const PROBE_KILL_AFTER: Duration = Duration::from_secs(2);

/// The time limit of a bound.
pub fn limit(bound: Bound) -> Duration {
    match bound {
        Bound::Script => PROBE_TIMEOUT,
        Bound::Open => OPEN_PROBE_TIMEOUT,
    }
}

/// Runs a command with the given environment, stdout captured, stderr dropped, stopped at `limit`
/// (TERM, then KILL two seconds later).
pub fn run_bounded(mut cmd: Command, env: &Env, cwd: &Path, limit: Duration) -> NodeOut {
    cmd.env_clear()
        .envs(env.vars())
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let Ok(mut child) = cmd.spawn() else {
        return NodeOut::default();
    };
    let reader = child.stdout.take().map(|mut out| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = out.read_to_end(&mut buf);
            buf
        })
    });
    let started = Instant::now();
    let mut termed = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {}
            Err(_) => break None,
        }
        let elapsed = started.elapsed();
        if !termed && elapsed >= limit {
            supervise::signal(child.id(), nix::sys::signal::Signal::SIGTERM);
            termed = true;
        } else if elapsed >= limit + PROBE_KILL_AFTER {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let stdout = reader.and_then(|r| r.join().ok()).unwrap_or_default();
    NodeOut {
        ok: status.is_some_and(|s| s.success()) && !termed,
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        timed_out: termed,
    }
}

/// The installed Core in the working directory.
pub struct FsCore<'a> {
    pub dir: PathBuf,
    pub env: &'a Env,
}

impl FsCore<'_> {
    fn path(&self, rel: &str) -> PathBuf {
        self.dir.join(rel)
    }
}

impl CoreView for FsCore<'_> {
    fn read(&self, rel: &str) -> Option<String> {
        std::fs::read(self.path(rel))
            .ok()
            .map(|b| String::from_utf8_lossy(&b).into_owned())
    }
    fn exists(&self, rel: &str) -> bool {
        self.path(rel).exists()
    }
    fn is_file(&self, rel: &str) -> bool {
        self.path(rel).is_file()
    }
    fn node(&self, script: &str, args: &[String], bound: Bound) -> NodeOut {
        let mut cmd = Command::new("node");
        cmd.arg("-e").arg(script).args(args);
        run_bounded(cmd, self.env, &self.dir, limit(bound))
    }
    fn has_node(&self) -> bool {
        on_path(self.env.get("PATH"), "node")
    }
    fn abs(&self, rel: &str) -> String {
        self.path(rel).to_string_lossy().into_owned()
    }
}

/// `command -v <name>` for a plain name: an executable file in one PATH directory.
pub fn on_path(path: &str, name: &str) -> bool {
    path.split(':')
        .map(|d| if d.is_empty() { "." } else { d })
        .any(|d| executable(&Path::new(d).join(name)))
}

/// `[ -x path ]`.
pub fn executable(path: &Path) -> bool {
    nix::unistd::access(path, nix::unistd::AccessFlags::X_OK).is_ok()
}

fn random(n: usize) -> Vec<u8> {
    let mut buf = vec![0u8; n];
    match std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut buf)) {
        Ok(()) => buf,
        Err(_) => Vec::new(),
    }
}

const DEFAULT_SERVER_BIN: &str = "/usr/local/bin/noevia-server";

/// The data directory both children use: UI_DATA_DIR, or Core's own `server/ui-data`.
pub fn data_dir(env: &Env, cwd: &Path) -> PathBuf {
    match env.get("UI_DATA_DIR") {
        "" => cwd.join("server/ui-data"),
        dir => PathBuf::from(dir),
    }
}

/// What a start runs: the pair (the script's behaviour) or, behind its switch, the front alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Start {
    Pair(plan::Launch),
    FrontOnly(front_only::Solo),
}

/// Everything before the launch: the refusal (exit 2) or the plan.
pub fn prepare(env: &mut Env, cwd: &Path, warn: &mut dyn FnMut(String)) -> Result<Start, Refusal> {
    for w in env.scrub() {
        warn(w);
    }
    let env: &Env = env;
    // noevia#2702 item 2: the script compared these bytes; a value read as empty here would not.
    if let Some(name) = env.non_utf8_read() {
        return Err(Refusal(format!(
            "web-supervisor: {name} is not valid UTF-8; set it to a plain value"
        )));
    }
    // Rust is the only front (#1071, #2094): unset or empty means rust.
    match env.get_or("NOEVIA_FRONT", "rust") {
        "rust" => {}
        "node" => return Err(Refusal("web-supervisor: NOEVIA_FRONT=node is no longer supported: the Rust front owns sign-in and routes Node no longer has (sbstndalton/noevia#1071); set NOEVIA_FRONT=rust".to_owned())),
        other => return Err(Refusal(format!("web-supervisor: NOEVIA_FRONT must be rust, got '{other}'"))),
    }
    let bin = env
        .get_or("NOEVIA_SERVER_BIN", DEFAULT_SERVER_BIN)
        .to_owned();
    if !executable(Path::new(&bin)) {
        return Err(Refusal("web-supervisor: NOEVIA_FRONT=rust but this image has no noevia-server (its NOEVIA_RS_REF predates bins/noevia-server); update the image".to_owned()));
    }
    let mut cmd = Command::new(&bin);
    cmd.arg("--features");
    let answer = run_bounded(cmd, env, cwd, OPEN_PROBE_TIMEOUT);
    if answer.timed_out {
        return Err(Refusal(format!("web-supervisor: noevia-server --features did not answer within {} s; this is not a pairing mismatch: retry the start", OPEN_PROBE_TIMEOUT.as_secs())));
    }
    let features = Features::parse(&answer.stdout);
    if front_only::enabled(env) {
        // No Core: none of its files, markers or `node` are looked at.
        let solo = front_only::plan(
            env,
            &bin,
            &features,
            &data_dir(env, cwd),
            &random(32),
            &random(16),
        )?;
        for w in &solo.warnings {
            warn(w.clone());
        }
        return Ok(Start::FrontOnly(solo));
    }
    let core = FsCore {
        dir: cwd.to_path_buf(),
        env,
    };
    let paired = pairing::check(env, &bin, &features, &core, warn)?;
    let mut launch = plan::plan(env, &bin, &paired, &random(32), &random(16))?;
    launch.ensure = ensure::plan(env, &bin, &features, &data_dir(env, cwd))?;
    for w in &launch.warnings {
        warn(w.clone());
    }
    Ok(Start::Pair(launch))
}

const SECRET: &[&str] = &["NOEVIA_PROJECT_STORE_TOKEN", "NOEVIA_START_ID"];

fn describe_child(out: &mut String, name: &str, spec: &plan::ChildSpec) {
    out.push_str(format!("{name} {} {}", spec.program, spec.args.join(" ")).trim_end());
    for (k, v) in spec
        .set
        .iter()
        .filter(|(k, _)| !SECRET.contains(&k.as_str()))
    {
        out.push_str(&format!(" {k}={v}"));
    }
    out.push('\n');
}

fn describe_steps(out: &mut String, steps: &[plan::ChildSpec]) {
    for step in steps {
        out.push_str(&format!(
            "ensure {} {}\n",
            step.program,
            step.args.join(" ")
        ));
    }
}

/// The secret-free summary `--check` prints for either kind of start.
pub fn describe_start(start: &Start) -> String {
    match start {
        Start::Pair(launch) => describe(launch),
        Start::FrontOnly(solo) => {
            let mut out = String::from("web-supervisor: front only (no Core)\n");
            describe_child(&mut out, "front", &solo.front);
            describe_steps(&mut out, &solo.ensure);
            out
        }
    }
}

/// The secret-free summary `--check` prints: what each child would be told, never the store
/// secret or the start id.
pub fn describe(launch: &plan::Launch) -> String {
    let mut out = String::from("web-supervisor: pair ok\n");
    for (name, spec) in [("node", &launch.node), ("front", &launch.front)] {
        describe_child(&mut out, name, spec);
    }
    describe_steps(&mut out, &launch.ensure);
    out
}

/// The entrypoint: the exit code the container ends with. With `--check` it runs every pre-launch
/// check, prints the plan and exits (0 paired, 2 refused) without taking the lock or starting
/// anything; any other argument is ignored, as the script ignored its arguments.
pub fn run() -> i32 {
    let check_only = std::env::args_os().skip(1).any(|a| a == "--check");
    let mut env = Env::from_process();
    let Ok(cwd) = std::env::current_dir() else {
        eprintln!("web-supervisor: no working directory");
        return 2;
    };
    let mut warn = |w: String| eprintln!("{w}");
    let start = match prepare(&mut env, &cwd, &mut warn) {
        Ok(start) => start,
        Err(Refusal(msg)) => {
            eprintln!("{msg}");
            return 2;
        }
    };
    if check_only {
        print!("{}", describe_start(&start));
        return 0;
    }
    let data = data_dir(&env, &cwd);
    match &start {
        Start::Pair(launch) => supervise::supervise(&env, &data, &cwd, launch),
        Start::FrontOnly(solo) => supervise::supervise_solo(&env, &data, &cwd, solo),
    }
}
