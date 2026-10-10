//! replay: run a recorded noevia HTTP contract corpus against a server and diff the answers.
//!
//! replay run --corpus DIR --base http://127.0.0.1:PORT [options]
//!   --seed DIR             copy DIR (may be empty) to a fresh work dir and use it as UI_DATA_DIR
//!   --work DIR             where the copy goes (default: a new dir under the system temp dir);
//!                          must not exist yet
//!   --data-dir DIR         the data dir of an already-running server (instead of --seed)
//!   --server-cmd CMD       start the server with `sh -c CMD` (UI_DATA_DIR = the work dir, UI_PORT
//!                          and UI_HOST from --base); stopped after the replay
//!   --server-env K=V       extra environment for --server-cmd (repeatable)
//!   --ready-timeout SECS   wait this long for GET /api/ready (default 60)
//!   --timeout SECS         per-request timeout (default 120)
//!   --bind PH=VALUE        bind a placeholder before replaying (repeatable)
//!   --ignore-header NAME   leave a response header out of the comparison (repeatable)
//!   --state-out FILE       write the data dir snapshot (tree + cowork.db) after the replay
//!   --expect-state FILE    diff the snapshot against FILE (one written by --state-out)
//!   --state-ignore PATH    leave a path (or `*suffix`) out of the snapshot (repeatable)
//!   --report FILE          write the outcomes as JSON
//!   --stop-on-first        stop at the first exchange that differs
//! replay coverage --corpus DIR --routes contracts/http/routes.toml
//!   list routes with no recorded exchange
//!
//! Exit status: 0 clean, 1 differences, 2 usage or setup error.

use replay::bind::Bindings;
use replay::{corpus, diff, http, routes, state, Options, Outcome};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Default)]
struct Args {
    flags: Vec<(String, String)>,
    switches: Vec<String>,
}

impl Args {
    fn parse(argv: &[String]) -> Result<Self, String> {
        let mut a = Args::default();
        let mut it = argv.iter();
        while let Some(k) = it.next() {
            let Some(name) = k.strip_prefix("--") else {
                return Err(format!("unexpected argument {k}"));
            };
            if name == "stop-on-first" {
                a.switches.push(name.to_string());
                continue;
            }
            let v = it.next().ok_or_else(|| format!("--{name} needs a value"))?;
            a.flags.push((name.to_string(), v.clone()));
        }
        Ok(a)
    }
    fn one(&self, k: &str) -> Option<&str> {
        self.flags
            .iter()
            .rev()
            .find(|(n, _)| n == k)
            .map(|(_, v)| v.as_str())
    }
    fn all(&self, k: &str) -> Vec<&str> {
        self.flags
            .iter()
            .filter(|(n, _)| n == k)
            .map(|(_, v)| v.as_str())
            .collect()
    }
    fn need(&self, k: &str) -> Result<&str, String> {
        self.one(k).ok_or_else(|| format!("--{k} is required"))
    }
    fn secs(&self, k: &str, default: u64) -> Result<Duration, String> {
        match self.one(k) {
            Some(v) => v
                .parse::<u64>()
                .map(Duration::from_secs)
                .map_err(|_| format!("--{k} must be whole seconds")),
            None => Ok(Duration::from_secs(default)),
        }
    }
}

fn copy_tree(from: &Path, to: &Path) -> Result<(), String> {
    std::fs::create_dir_all(to).map_err(|e| format!("create {}: {e}", to.display()))?;
    for e in std::fs::read_dir(from).map_err(|e| format!("read {}: {e}", from.display()))? {
        let e = e.map_err(|e| e.to_string())?;
        let ty = e.file_type().map_err(|e| e.to_string())?;
        let dest = to.join(e.file_name());
        if ty.is_dir() {
            copy_tree(&e.path(), &dest)?;
        } else if ty.is_file() {
            std::fs::copy(e.path(), &dest)
                .map_err(|err| format!("copy {}: {err}", e.path().display()))?;
        } else {
            return Err(format!(
                "{} is neither a file nor a directory; seeds hold plain files",
                e.path().display()
            ));
        }
    }
    Ok(())
}

/// What a recorded `<ts>` in a request becomes: a fixed instant, so a replay does not depend on
/// when it runs.
const REQUEST_TS: &str = "2026-01-01T00:00:00.000Z";

struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn start_server(
    cmd: &str,
    env: &[&str],
    base: &http::Base,
    data: &Path,
    ready: Duration,
) -> Result<Server, String> {
    let mut c = Command::new("sh");
    c.arg("-c")
        .arg(cmd)
        .env("UI_DATA_DIR", data)
        .env("UI_PORT", base.port.to_string())
        .env("UI_HOST", &base.host)
        .stdin(Stdio::null());
    for kv in env {
        let (k, v) = kv
            .split_once('=')
            .ok_or_else(|| format!("--server-env {kv}: expected K=V"))?;
        c.env(k, v);
    }
    let mut server = Server(c.spawn().map_err(|e| format!("start server: {e}"))?);
    let deadline = Instant::now() + ready;
    while Instant::now() < deadline {
        if let Ok(Some(status)) = server.0.try_wait() {
            return Err(format!("server exited before it was ready ({status})"));
        }
        if let Ok(r) = http::send(base, "GET", "/api/ready", &[], b"", Duration::from_secs(2)) {
            if r.status == 200 {
                return Ok(server);
            }
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    Err(format!("server not ready after {}s", ready.as_secs()))
}

fn print_outcome(o: &Outcome) {
    let status = o.actual_status.map_or("-".to_string(), |s| s.to_string());
    if o.clean() {
        println!("ok    {} {} {} ({})", o.file, o.method, o.path, status);
        return;
    }
    println!(
        "DIFF  {} {} {} (expected {}, got {})",
        o.file, o.method, o.path, o.expected_status, status
    );
    if let Some(e) = &o.error {
        println!("      error: {e}");
    }
    for d in &o.diffs {
        println!("      {}: expected {} got {}", d.at, d.expected, d.actual);
    }
}

fn report(outcomes: &[Outcome]) -> Value {
    Value::Array(
        outcomes
            .iter()
            .map(|o| {
                json!({
                    "file": o.file, "method": o.method, "path": o.path,
                    "expectedStatus": o.expected_status, "status": o.actual_status,
                    "error": o.error, "generated": o.generated,
                    "diffs": o.diffs.iter().map(|d| json!({"at": d.at, "expected": d.expected, "actual": d.actual})).collect::<Vec<_>>(),
                })
            })
            .collect(),
    )
}

fn write_json(path: &str, v: &Value) -> Result<(), String> {
    let text = serde_json::to_string_pretty(v).map_err(|e| e.to_string())?;
    std::fs::write(path, format!("{text}\n")).map_err(|e| format!("write {path}: {e}"))
}

fn run(a: &Args) -> Result<bool, String> {
    let corpus = corpus::load(Path::new(a.need("corpus")?))?;
    let base = http::Base::parse(a.need("base")?)?;
    let work: Option<PathBuf> = if let Some(seed) = a.one("seed") {
        let work = match a.one("work") {
            Some(w) => PathBuf::from(w),
            None => std::env::temp_dir().join(format!(
                "noevia-replay-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_millis())
                    .unwrap_or(0)
            )),
        };
        if work.exists() {
            return Err(format!(
                "{} already exists; a replay starts from a fresh copy",
                work.display()
            ));
        }
        copy_tree(Path::new(seed), &work)?;
        Some(work)
    } else {
        a.one("data-dir").map(PathBuf::from)
    };
    let server = match a.one("server-cmd") {
        Some(cmd) => {
            let data = work
                .as_ref()
                .ok_or("--server-cmd needs --seed (the server gets a fresh copy)")?;
            Some(start_server(
                cmd,
                &a.all("server-env"),
                &base,
                data,
                a.secs("ready-timeout", 60)?,
            )?)
        }
        None => None,
    };
    let mut b = Bindings::new(&base.origin());
    for kv in a.all("bind") {
        let (k, v) = kv
            .split_once('=')
            .ok_or_else(|| format!("--bind {kv}: expected PLACEHOLDER=VALUE"))?;
        b.bind(k, v);
    }
    let opts = Options {
        base,
        timeout: a.secs("timeout", 120)?,
        ignore_headers: a
            .all("ignore-header")
            .into_iter()
            .map(str::to_ascii_lowercase)
            .collect(),
        data_dir: work.clone(),
        now: REQUEST_TS.to_string(),
    };
    let outcomes = replay::replay(
        &corpus,
        &opts,
        &mut b,
        a.switches.iter().any(|s| s == "stop-on-first"),
    );
    drop(server);
    for o in &outcomes {
        print_outcome(o);
    }
    let mut clean = outcomes.iter().all(Outcome::clean);
    if let Some(r) = a.one("report") {
        write_json(r, &report(&outcomes))?;
    }
    if a.one("state-out").is_some() || a.one("expect-state").is_some() {
        let dir = work
            .as_ref()
            .ok_or("a state snapshot needs --seed or --data-dir")?;
        let ignore: Vec<String> = a
            .all("state-ignore")
            .into_iter()
            .map(str::to_string)
            .collect();
        let snap = state::snapshot(dir, "cowork.db", &ignore, &b);
        if let Some(p) = a.one("state-out") {
            write_json(p, &snap)?;
        }
        if let Some(p) = a.one("expect-state") {
            let text = std::fs::read_to_string(p).map_err(|e| format!("read {p}: {e}"))?;
            let expected: Value = serde_json::from_str(&text).map_err(|e| format!("{p}: {e}"))?;
            let mut diffs = Vec::new();
            diff::json(
                &expected,
                &snap,
                "state",
                &mut Bindings::new(""),
                &mut diffs,
            );
            for d in &diffs {
                println!("STATE {}: expected {} got {}", d.at, d.expected, d.actual);
            }
            clean &= diffs.is_empty();
        }
    }
    let differing = outcomes.iter().filter(|o| !o.clean()).count();
    println!(
        "replay: {} exchanges, {} differ, {} dropped at record time",
        outcomes.len(),
        differing,
        corpus.dropped
    );
    Ok(clean)
}

fn coverage(a: &Args) -> Result<bool, String> {
    let corpus = corpus::load(Path::new(a.need("corpus")?))?;
    let routes_path = a.need("routes")?;
    let text =
        std::fs::read_to_string(routes_path).map_err(|e| format!("read {routes_path}: {e}"))?;
    let all = routes::paths(&text);
    let mut missing = 0;
    for r in &all {
        let hits: Vec<String> = corpus
            .exchanges
            .iter()
            .filter(|e| routes::covers(r, e.path()))
            .map(|e| e.method().to_string())
            .collect();
        if hits.is_empty() {
            missing += 1;
            println!("uncovered {r}");
        }
    }
    println!(
        "coverage: {} of {} routes have at least one exchange",
        all.len() - missing,
        all.len()
    );
    Ok(true)
}

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let Some((cmd, rest)) = argv.split_first() else {
        eprintln!("usage: replay run --corpus DIR --base URL [...] | replay coverage --corpus DIR --routes FILE");
        return ExitCode::from(2);
    };
    let result = Args::parse(rest).and_then(|a| match cmd.as_str() {
        "run" => run(&a),
        "coverage" => coverage(&a),
        other => Err(format!("unknown command {other}")),
    });
    match result {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(e) => {
            eprintln!("replay: {e}");
            ExitCode::from(2)
        }
    }
}
