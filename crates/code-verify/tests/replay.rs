//! crates/code-verify: what core's own server/code-verify.cjs and server/code-pipeline-verify.cjs
//! did against a scripted fake verifier (tests/fixtures/node.v1.json, written by
//! tools/gen-fixtures.cjs), replayed against the Rust crate with the same fake verifier driven
//! from the same script, over a real unix socket (or TCP) in a temp directory: the outcome,
//! every event and log entry in order, the request lines the verifier received, whether the
//! client hung up where the script waited for it, and what the checkout stub was asked must match.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use code_verify::{
    Adapter, Cancel, CheckoutError, Event, LogEntry, Options, Outcome, Request, Status, Verifier,
    Verify, VerifyError,
};
use js_json::{wtf16, JValue};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

const PINNED_VERIFY: &str = "47ea5c62209f4ccc24fc9f94a03656ed7d2ed32aa161d4c27edb307d2aa1fee3";
const PINNED_PIPELINE: &str = "c0683af5cb1d10027e893fc3741c02233eb693263d278014abb474d10dee676a";
const PINNED_ACP: &str = "5521d82bca54984b40874865483447758cc7cb42ab5497ee3e42a592c874ffc2";
const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/node.v1.json");

fn fixture() -> &'static JValue {
    static F: OnceLock<JValue> = OnceLock::new();
    F.get_or_init(|| wtf16::parse(&fs::read_to_string(FIXTURE).unwrap()).unwrap())
}

fn text(v: &JValue) -> String {
    wtf16::stringify(v).unwrap()
}

/// `{ "$repeat": [text, n] }` and `{ "$cat": [...] }`: the generator's long strings.
fn expand(v: &JValue) -> JValue {
    match v {
        JValue::Arr(items) => JValue::Arr(items.iter().map(expand).collect()),
        JValue::Obj(items) => {
            if let JValue::Arr(a) = v.get("$repeat") {
                let s = a[0].as_str().unwrap();
                let n = a[1].as_num().unwrap() as usize;
                return JValue::Str(s.repeat(n));
            }
            if let JValue::Arr(parts) = v.get("$cat") {
                let mut out = String::new();
                for p in parts {
                    match expand(p) {
                        JValue::Str(s) => out.push_str(&s),
                        _ => panic!("$cat of a non-string"),
                    }
                }
                return JValue::Str(out);
            }
            JValue::Obj(items.iter().map(|(k, x)| (k.clone(), expand(x))).collect())
        }
        other => other.clone(),
    }
}

fn unhex(h: &str) -> Vec<u8> {
    (0..h.len() / 2)
        .map(|i| u8::from_str_radix(&h[2 * i..2 * i + 2], 16).unwrap())
        .collect()
}

// ---- the fake verifier ----------------------------------------------------------------------------

enum Sock {
    Unix(UnixStream),
    Tcp(TcpStream),
}

impl Sock {
    fn try_clone(&self) -> Sock {
        match self {
            Sock::Unix(s) => Sock::Unix(s.try_clone().unwrap()),
            Sock::Tcp(s) => Sock::Tcp(s.try_clone().unwrap()),
        }
    }
    fn set_read_timeout(&self, d: Duration) {
        match self {
            Sock::Unix(s) => s.set_read_timeout(Some(d)).unwrap(),
            Sock::Tcp(s) => s.set_read_timeout(Some(d)).unwrap(),
        }
    }
    fn shutdown(&self) {
        let _ = match self {
            Sock::Unix(s) => s.shutdown(Shutdown::Both),
            Sock::Tcp(s) => s.shutdown(Shutdown::Both),
        };
    }
}

impl Read for Sock {
    fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Sock::Unix(s) => s.read(b),
            Sock::Tcp(s) => s.read(b),
        }
    }
}

impl Write for Sock {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        match self {
            Sock::Unix(s) => s.write(b),
            Sock::Tcp(s) => s.write(b),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[derive(Default)]
struct Seen {
    lines: Vec<String>,
    closes: Vec<bool>,
}

struct Ctx {
    seen: Mutex<Seen>,
    cancel: Arc<Cancel>,
}

fn script(sock: Sock, steps: &[JValue], ctx: &Arc<Ctx>) {
    sock.set_read_timeout(Duration::from_secs(10));
    let mut reader = BufReader::new(sock.try_clone());
    let mut writer = sock.try_clone();
    for step in steps {
        if step.get("line").as_bool() == Some(true) {
            let mut buf = Vec::new();
            if reader.read_until(b'\n', &mut buf).is_ok() && buf.last() == Some(&b'\n') {
                buf.pop();
                ctx.seen
                    .lock()
                    .unwrap()
                    .lines
                    .push(String::from_utf8_lossy(&buf).into_owned());
            }
        } else if !matches!(step.get("reply"), JValue::Undefined) {
            let mut t = text(&expand(step.get("reply")));
            t.push('\n');
            let _ = writer.write_all(t.as_bytes());
        } else if let Some(t) = step.get("send").as_str() {
            let _ = writer.write_all(t.as_bytes());
        } else if let JValue::Arr(chunks) = step.get("sendHex") {
            for c in chunks {
                let _ = writer.write_all(&unhex(c.as_str().unwrap()));
                if let Some(ms) = step.get("gapMs").as_num() {
                    std::thread::sleep(Duration::from_millis(ms as u64));
                }
            }
        } else if let Some(n) = step.get("sendBig").as_num() {
            let mut left = n as usize;
            let block = vec![b'x'; 64 * 1024];
            while left > 0 {
                let k = left.min(block.len());
                if writer.write_all(&block[..k]).is_err() {
                    break;
                }
                left -= k;
            }
        } else if let Some(ms) = step.get("wait").as_num() {
            std::thread::sleep(Duration::from_millis(ms as u64));
        } else if step.get("close").as_bool() == Some(true) {
            sock.shutdown();
        } else if step.get("expectClose").as_bool() == Some(true) {
            let mut tmp = [0u8; 4096];
            let closed = loop {
                match reader.read(&mut tmp) {
                    Ok(0) => break true,
                    Ok(_) => {}
                    Err(e)
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) =>
                    {
                        break false
                    }
                    Err(_) => break true,
                }
            };
            ctx.seen.lock().unwrap().closes.push(closed);
        } else if step.get("abort").as_bool() == Some(true) {
            ctx.cancel.cancel();
        } else if let Some(ms) = step.get("abortAfter").as_num() {
            let cancel = ctx.cancel.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(ms as u64));
                cancel.cancel();
            });
        }
    }
}

enum Listener {
    Unix(UnixListener),
    Tcp(TcpListener),
}

impl Listener {
    fn accept(&self) -> Option<Sock> {
        match self {
            Listener::Unix(l) => l.accept().ok().map(|(s, _)| Sock::Unix(s)),
            Listener::Tcp(l) => l.accept().ok().map(|(s, _)| Sock::Tcp(s)),
        }
    }
}

/// Serves `connections` (one script per accepted connection, the last repeating) until stopped.
struct Fake {
    stop: Arc<AtomicBool>,
    accept: Option<std::thread::JoinHandle<Vec<std::thread::JoinHandle<()>>>>,
}

fn serve(listener: Listener, connections: Vec<JValue>, ctx: Arc<Ctx>) -> Fake {
    match &listener {
        Listener::Unix(l) => l.set_nonblocking(true).unwrap(),
        Listener::Tcp(l) => l.set_nonblocking(true).unwrap(),
    }
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    let accept = std::thread::spawn(move || {
        let mut running = Vec::new();
        let mut count = 0usize;
        while !flag.load(Ordering::SeqCst) {
            let Some(sock) = listener.accept() else {
                std::thread::sleep(Duration::from_millis(2));
                continue;
            };
            // Accepted sockets are blocking again (the listener's flag may be inherited).
            match &sock {
                Sock::Unix(s) => s.set_nonblocking(false).unwrap(),
                Sock::Tcp(s) => s.set_nonblocking(false).unwrap(),
            }
            let steps: Vec<JValue> =
                match connections.get(count.min(connections.len().saturating_sub(1))) {
                    Some(JValue::Arr(s)) => s.clone(),
                    _ => vec![],
                };
            count += 1;
            let ctx = ctx.clone();
            running.push(std::thread::spawn(move || script(sock, &steps, &ctx)));
        }
        running
    });
    Fake {
        stop,
        accept: Some(accept),
    }
}

impl Fake {
    /// Waits for every connection's script (each read has a 10 s limit), then stops listening.
    fn finish(mut self) {
        std::thread::sleep(Duration::from_millis(30));
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.accept.take() {
            for t in h.join().unwrap() {
                let _ = t.join();
            }
        }
    }
}

// ---- adapters ---------------------------------------------------------------------------------------

enum Mode {
    Real,
    Unmeasured,
    ReboundTask,
    ReboundRevision,
    Stub(Status, Option<VerifyError>),
    Threw,
    AvailableThrows,
}

struct Wrapped {
    inner: Verify,
    mode: Mode,
}

impl Verifier for Wrapped {
    fn available(&self) -> bool {
        match self.mode {
            Mode::AvailableThrows => panic!("qa"),
            Mode::Stub(..) | Mode::Threw => true,
            _ => self.inner.available(),
        }
    }
    fn run(&self, request: &Request<'_>, emit: &mut dyn FnMut(&Event<'_>)) -> Outcome {
        match &self.mode {
            Mode::ReboundTask => self.inner.run(
                &Request {
                    task_id: "qa-other-task",
                    repo: request.repo,
                    head_sha: request.head_sha,
                    revision: request.revision,
                    verify_checkout: request.verify_checkout,
                    cancel: request.cancel,
                },
                emit,
            ),
            Mode::ReboundRevision => self.inner.run(
                &Request {
                    task_id: request.task_id,
                    repo: request.repo,
                    head_sha: request.head_sha,
                    revision: request.revision + 1.0,
                    verify_checkout: request.verify_checkout,
                    cancel: request.cancel,
                },
                emit,
            ),
            Mode::Stub(status, error) => Outcome {
                status: *status,
                report: None,
                error: error.clone(),
            },
            Mode::Threw => panic!("qa boom"),
            _ => self.inner.run(request, emit),
        }
    }
    fn is_measured(&self, report: &code_verify::Report, t: &str, h: &str, r: f64) -> bool {
        match self.mode {
            Mode::Unmeasured => false,
            Mode::Stub(..) | Mode::Threw | Mode::AvailableThrows => true,
            _ => code_verify::is_measured(report, t, h, r),
        }
    }
}

fn mode_of(a: &JValue) -> Mode {
    match a.get("mode").as_str().unwrap() {
        "real" => Mode::Real,
        "unmeasured" => Mode::Unmeasured,
        "rebound-task" => Mode::ReboundTask,
        "rebound-revision" => Mode::ReboundRevision,
        "threw" => Mode::Threw,
        "available-throws" => Mode::AvailableThrows,
        "stub" => {
            let s = a.get("stub");
            let status = match s.get("status").as_str().unwrap() {
                "passed" => Status::Passed,
                "failed" => Status::Failed,
                "error" => Status::Error,
                "unavailable" => Status::Unavailable,
                other => panic!("status {other}"),
            };
            let error = match s.get("error") {
                JValue::Obj(_) => Some(VerifyError {
                    code: s.get("error").get("code").as_str().unwrap().to_string(),
                    message: s.get("error").get("message").as_str().unwrap().to_string(),
                }),
                _ => None,
            };
            Mode::Stub(status, error)
        }
        other => panic!("mode {other}"),
    }
}

// ---- one scenario -----------------------------------------------------------------------------------

fn clock_for(spec: &JValue) -> Box<dyn Fn() -> f64 + Send + Sync> {
    let values: Option<Vec<f64>> = match spec {
        JValue::Arr(a) => Some(a.iter().map(|v| v.as_num().unwrap()).collect()),
        _ => None,
    };
    let calls = AtomicUsize::new(0);
    Box::new(move || {
        let i = calls.fetch_add(1, Ordering::SeqCst);
        match &values {
            Some(v) => v[i.min(v.len() - 1)],
            None => 1_700_000_000_000.0 + 7.0 * (i as f64 + 1.0),
        }
    })
}

fn run_scenario(sc: &JValue) -> JValue {
    let dir = tempfile::tempdir().unwrap();
    let dir_path = dir.path().to_str().unwrap().to_string();
    if let JValue::Arr(files) = sc.get("files") {
        for f in files {
            fs::write(dir.path().join(f.as_str().unwrap()), "qa").unwrap();
        }
    }
    let cancel = Cancel::new();
    let ctx = Arc::new(Ctx {
        seen: Mutex::new(Seen::default()),
        cancel: cancel.clone(),
    });
    let connections = match sc.get("connections") {
        JValue::Arr(c) => c.clone(),
        _ => vec![],
    };
    let mut port = 0u16;
    let fake = match sc.get("server").as_str().unwrap() {
        "unix" => {
            let l = UnixListener::bind(dir.path().join("v.sock")).unwrap();
            Some(serve(Listener::Unix(l), connections, ctx.clone()))
        }
        "tcp" => {
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            port = l.local_addr().unwrap().port();
            Some(serve(Listener::Tcp(l), connections, ctx.clone()))
        }
        "refused-tcp" => {
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            port = l.local_addr().unwrap().port();
            drop(l);
            None
        }
        _ => None,
    };

    let o = sc.get("options");
    let endpoint = o.get("endpoint").as_str().map(|e| {
        e.replace("$DIR", &dir_path)
            .replace("$PORT", &port.to_string())
    });
    let trace: Arc<Mutex<Vec<JValue>>> = Arc::new(Mutex::new(Vec::new()));
    let nonces = Arc::new(AtomicUsize::new(0));
    let mut options = Options::new(endpoint);
    options.timeout = Duration::from_millis(o.get("timeoutMs").as_num().unwrap() as u64);
    options.connect_retry = Duration::from_millis(o.get("connectRetryMs").as_num().unwrap() as u64);
    if let Some(t) = o.get("tailBytes").as_num() {
        options.tail_bytes = t;
    }
    options.now = clock_for(o.get("clock"));
    options.nonce = Box::new(move || {
        Some(format!(
            "nonce-{}",
            nonces.fetch_add(1, Ordering::SeqCst) + 1
        ))
    });
    let log_trace = trace.clone();
    options.log = Box::new(move |e: &LogEntry| {
        log_trace
            .lock()
            .unwrap()
            .push(JValue::obj([("log", e.to_json())]));
    });
    let verify = Verify::new(options);

    let calls: Mutex<Vec<JValue>> = Mutex::new(Vec::new());
    let source = fixture().get("source").as_str().unwrap().to_string();
    let checkout_spec = sc.get("checkout").clone();
    let checkout = |task: &str, head: &str| -> Result<String, CheckoutError> {
        calls
            .lock()
            .unwrap()
            .push(JValue::Arr(vec![task.into(), head.into()]));
        if let JValue::Obj(_) = checkout_spec.get("throw") {
            let t = checkout_spec.get("throw");
            return Err(CheckoutError {
                code: t.get("code").as_str().map(str::to_string),
                message: t.get("message").as_str().map(str::to_string),
            });
        }
        if !matches!(checkout_spec.get("throwRaw"), JValue::Undefined) {
            return Err(CheckoutError::default());
        }
        Ok(source.clone())
    };

    let r = sc.get("request");
    let request = Request {
        task_id: r.get("taskId").as_str().unwrap(),
        repo: r.get("repo").as_str().unwrap(),
        head_sha: r.get("headSha").as_str().unwrap(),
        revision: r.get("revision").as_num().unwrap_or(f64::NAN),
        verify_checkout: &checkout,
        cancel: Some(cancel.as_ref()),
    };
    if sc.get("cancelBefore").as_bool() == Some(true) {
        cancel.cancel();
    }
    let event_trace = trace.clone();
    let mut emit = |e: &Event<'_>| {
        event_trace.lock().unwrap().push(JValue::obj([(
            "event",
            JValue::obj([("type", JValue::Str(e.kind().into())), ("data", e.data())]),
        )]));
    };
    let (available, outcome) = if matches!(sc.get("adapter"), JValue::Obj(_)) {
        let a = sc.get("adapter");
        let adapter = if a.get("mode").as_str() == Some("none") {
            Adapter::new(None)
        } else {
            Adapter::new(Some(Box::new(Wrapped {
                inner: verify,
                mode: mode_of(a),
            })))
        };
        (adapter.available(), adapter.run(&request, &mut emit))
    } else {
        (verify.available(), verify.run(&request, &mut emit))
    };

    if let Some(f) = fake {
        f.finish();
    }
    let seen = ctx.seen.lock().unwrap();
    let record = JValue::obj([
        ("available", JValue::Bool(available)),
        ("outcome", outcome.to_json()),
        ("trace", JValue::Arr(trace.lock().unwrap().clone())),
        (
            "lines",
            JValue::Arr(seen.lines.iter().map(|l| l.as_str().into()).collect()),
        ),
        (
            "closes",
            JValue::Arr(seen.closes.iter().map(|c| JValue::Bool(*c)).collect()),
        ),
        ("checkoutCalls", JValue::Arr(calls.lock().unwrap().clone())),
    ]);
    // The temp directory is part of some messages: normalised, as the generator did.
    wtf16::parse(&text(&record).replace(&dir_path, "<DIR>")).unwrap()
}

const KEYS: [&str; 6] = [
    "available",
    "outcome",
    "trace",
    "lines",
    "closes",
    "checkoutCalls",
];

fn compare(name: &str, sc: &JValue) -> Vec<String> {
    let got = run_scenario(sc);
    let node = sc.get("expect");
    KEYS.iter()
        .filter(|k| text(got.get(k)) != text(node.get(k)))
        .map(|k| {
            let clip = |t: String| t.chars().take(1500).collect::<String>();
            format!(
                "{name}: {k}\n  node: {}\n  rust: {}",
                clip(text(node.get(k))),
                clip(text(got.get(k)))
            )
        })
        .collect()
}

fn scenarios() -> Vec<(String, JValue)> {
    let JValue::Obj(all) = fixture().get("scenarios") else {
        panic!()
    };
    all.clone()
}

#[test]
fn every_node_scenario_replays() {
    let started = Instant::now();
    let mut failures = Vec::new();
    let mut replayed = 0;
    let mut skipped = Vec::new();
    for (name, sc) in scenarios() {
        if let Some(why) = sc.get("nodeOnly").as_str() {
            skipped.push(format!("{name}: {why}"));
            continue;
        }
        failures.extend(compare(&name, &sc));
        replayed += 1;
    }
    eprintln!(
        "replayed {replayed} scenarios in {:?}; skipped (Node-only): {skipped:#?}",
        started.elapsed()
    );
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    assert!(replayed >= 150, "only {replayed} scenarios replayed");
}

#[test]
fn the_fixture_covers_the_required_scenarios() {
    let names: Vec<String> = scenarios().into_iter().map(|(n, _)| n).collect();
    for required in [
        "no-endpoint",
        "bad-repo-empty",
        "bad-sha-39",
        "bad-sha-uppercase",
        "bad-revision-1.5",
        "bad-revision-minus-1",
        "checkout-throws-with-code",
        "checkout-throws-no-code",
        "passed",
        "failed-exit-1",
        "signal-valid",
        "signal-invalid-lowercase",
        "timed-out",
        "tail-controls",
        "tail-cut-3-byte",
        "total-larger-than-tail",
        "total-negative",
        "wrong-nonce",
        "wrong-head-sha",
        "not-a-verify-result",
        "refused-not-configured",
        "refused-busy-after-retries",
        "refused-long-code",
        "refused-code-not-string",
        "refused-message-controls",
        "answer-not-json",
        "answer-over-2-mib-no-newline",
        "busy-then-success",
        "not-listening-immediate-close",
        "socket-file-missing",
        "never-answers-timeout",
        "accepts-waits-then-closes-unreachable",
        "endpoint-relative-unix",
        "cancel-before",
        "cancel-during",
        "tail-bytes-0-default",
        "tail-bytes-10-min",
        "tail-bytes-1e9-max",
        "passed-over-tcp",
        "adapter-unavailable-no-endpoint",
        "adapter-error-bad-sha",
        "adapter-failed",
        "adapter-passed",
        "adapter-unmeasured-report",
    ] {
        assert!(names.iter().any(|n| n == required), "missing {required}");
    }
}

#[test]
fn fixture_sources_are_the_pinned_ones() {
    // core 0c294a3: a fixture regenerated from other sources fails here until updated on purpose.
    let f = fixture();
    let s = f.get("sources");
    assert_eq!(s.get("code-verify.cjs").as_str(), Some(PINNED_VERIFY));
    assert_eq!(
        s.get("code-pipeline-verify.cjs").as_str(),
        Some(PINNED_PIPELINE)
    );
    assert_eq!(s.get("code-acp.cjs").as_str(), Some(PINNED_ACP));
}

#[test]
fn a_cancel_during_the_retry_wait_returns_at_once() {
    // Difference 3: Node let the 250 ms pass; here the wait wakes at the cancel.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("none.sock");
    let mut o = Options::new(Some(format!("unix:{}", path.display())));
    o.connect_retry = Duration::from_secs(30);
    o.nonce = Box::new(|| Some("n".into()));
    let verify = Verify::new(o);
    let cancel = Cancel::new();
    let c = cancel.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(60));
        c.cancel();
    });
    let checkout = |_: &str, _: &str| Ok("/src".to_string());
    let request = Request {
        task_id: "t",
        repo: "qa-repo",
        head_sha: &"a".repeat(40),
        revision: 0.0,
        verify_checkout: &checkout,
        cancel: Some(cancel.as_ref()),
    };
    let started = Instant::now();
    let out = verify.run(&request, &mut |_| {});
    assert_eq!(out.error.unwrap().code, "cancelled");
    assert!(started.elapsed() < Duration::from_millis(200));
}

#[test]
fn a_report_is_only_made_by_a_run_and_a_clone_stays_bound() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("v.sock");
    let listener = UnixListener::bind(&sock).unwrap();
    let head = "b".repeat(40);
    let h = head.clone();
    let server = std::thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        let mut line = String::new();
        BufReader::new(s.try_clone().unwrap())
            .read_line(&mut line)
            .unwrap();
        writeln!(
            s,
            r#"{{"noevia":"verify-result","ok":true,"nonce":"n","headSha":"{h}","exitCode":0}}"#
        )
        .unwrap();
    });
    let mut o = Options::new(Some(format!("unix:{}", sock.display())));
    o.nonce = Box::new(|| Some("n".into()));
    let verify = Verify::new(o);
    let checkout = |_: &str, _: &str| Ok("/src".to_string());
    let request = Request {
        task_id: "t",
        repo: "qa-repo",
        head_sha: &head,
        revision: 2.0,
        verify_checkout: &checkout,
        cancel: None,
    };
    let out = verify.run(&request, &mut |_| {});
    server.join().unwrap();
    let report = out.report.unwrap();
    let copy = report.clone();
    assert!(code_verify::is_measured(&copy, "t", &head, 2.0));
    assert!(!code_verify::is_measured(&copy, "t", &head, 3.0));
    // Its JSON is a record: nothing reads it back into a Report (no constructor, no Deserialize).
    assert_eq!(
        report.to_json().get("measuredBy").as_str(),
        Some("verifier")
    );
}
