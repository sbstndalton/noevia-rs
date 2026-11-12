#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
//! The router binary against fake backends (tests/support/fake_backend.py) and a fake docker CLI:
//! no GPU, no real containers. The first twelve tests port the prototype's suite; the rest cover
//! client aborts, requests arriving during a swap, a backend that never becomes healthy, a stop that
//! leaves its container behind, ttl unloads, the GPU lock and SIGKILL escalation.

use futures_util::StreamExt;
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use serde_json::{json, Value};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_noevia-router");

fn support(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/support")
        .join(name)
}

/// A free loopback port, never the same twice in this process: tests run in parallel and each
/// backend binds its port later, so a port handed out once must not be handed out again.
fn free_port() -> u16 {
    static GIVEN: std::sync::Mutex<Vec<u16>> = std::sync::Mutex::new(Vec::new());
    loop {
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let mut given = GIVEN.lock().unwrap();
        if !given.contains(&port) {
            given.push(port);
            return port;
        }
    }
}

fn port_open(port: u16) -> bool {
    TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), Duration::from_millis(200)).is_ok()
}

fn router_answers(port: u16) -> bool {
    use std::io::{Read, Write};
    let Ok(mut c) =
        TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), Duration::from_millis(200))
    else {
        return false;
    };
    let _ = c.set_read_timeout(Some(Duration::from_millis(500)));
    if c.write_all(b"GET /health HTTP/1.0\r\n\r\n").is_err() {
        return false;
    }
    let mut buf = String::new();
    let _ = c.read_to_string(&mut buf);
    buf.starts_with("HTTP/1.") && buf.contains("\"status\":\"ok\"")
}

/// Wait (bounded) for a condition that a background task makes true.
async fn eventually(what: &str, limit: Duration, mut f: impl FnMut() -> bool) {
    let t0 = Instant::now();
    while !f() {
        assert!(t0.elapsed() < limit, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

struct Harness {
    dir: tempfile::TempDir,
    port: u16,
    pa: u16,
    pb: u16,
    pc: u16,
    child: Option<Child>,
    http: reqwest::Client,
}

impl Harness {
    fn config_path(&self) -> PathBuf {
        self.dir.path().join("r.toml")
    }
    fn docker_dir(&self) -> PathBuf {
        self.dir.path().join("docker")
    }
    fn logdir(&self) -> PathBuf {
        self.dir.path().join("logs")
    }

    /// The prototype's test table (dense-a, moe-b, broken, short-ttl, disabled) plus `extra`
    /// models; `router_extra` goes into `[router]`.
    async fn start(router_extra: &str, extra: &str) -> Harness {
        let dir = tempfile::tempdir().unwrap();
        let (port, pa, pb, pc) = (free_port(), free_port(), free_port(), free_port());
        let fb = format!("python3 {}", support("fake_backend.py").display());
        let docker = support("fake-docker");
        let cfg = format!(
            r#"
[router]
host = "127.0.0.1"
port = {port}
docker = "{docker}"
shutdown_grace = 3
{router_extra}
[defaults]
ready_timeout = 20
stop_timeout = 5
[[models]]
name = "dense-a"
engine = "llamacpp"
aliases = ["a"]
cmd = "exec {fb} --port {pa} --name A --delay 0.5"
proxy = "http://127.0.0.1:{pa}"
health_expect = '"ok"'
[[models]]
name = "moe-b"
engine = "strata"
upstream_model = "upstream-b"
cmd = "exec {fb} --port {pb} --name B --delay 1 --slow 0.05"
proxy = "http://127.0.0.1:{pb}"
[[models]]
name = "broken"
engine = "llamacpp"
cmd = "exec {fb} --port {pc} --name C --delay 999 --exit-after 0.5"
proxy = "http://127.0.0.1:{pc}"
[[models]]
name = "short-ttl"
engine = "llamacpp"
ttl = 2
cmd = "exec {fb} --port {pc} --name D --slow 0.2"
proxy = "http://127.0.0.1:{pc}"
[[models]]
name = "disabled"
engine = "strata"
enabled = false
cmd = "false"
proxy = "http://127.0.0.1:1"
{extra}
"#,
            docker = docker.display(),
        );
        std::fs::write(dir.path().join("r.toml"), cfg).unwrap();
        std::fs::create_dir_all(dir.path().join("docker")).unwrap();
        let mut h = Harness {
            dir,
            port,
            pa,
            pb,
            pc,
            child: None,
            http: reqwest::Client::new(),
        };
        h.spawn_router();
        h
    }

    fn spawn_router(&mut self) {
        let err = std::fs::File::create(self.dir.path().join("router.err")).unwrap();
        let child = Command::new(BIN)
            .arg("--config")
            .arg(self.config_path())
            .arg("--logdir")
            .arg(self.logdir())
            .env("FAKE_DOCKER_DIR", self.docker_dir())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(err)
            .spawn()
            .unwrap();
        self.child = Some(child);
        // Ready means the router answers /health: a bare TCP connect can succeed against a port
        // the kernel just used as its own ephemeral source port (a loopback self-connect).
        let t0 = Instant::now();
        while !router_answers(self.port) {
            assert!(
                t0.elapsed() < Duration::from_secs(20),
                "router did not listen: {}",
                self.router_err()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn router_err(&self) -> String {
        std::fs::read_to_string(self.dir.path().join("router.err")).unwrap_or_default()
    }

    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }

    async fn req(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> (u16, String) {
        let mut r = self
            .http
            .request(method, self.url(path))
            .timeout(Duration::from_secs(60));
        if let Some(b) = body {
            r = r.json(&b);
        }
        let resp = r.send().await.unwrap();
        let status = resp.status().as_u16();
        (status, resp.text().await.unwrap())
    }

    async fn chat(&self, model: &str, text: &str, stream: bool) -> (u16, String) {
        let body = json!({"model": model, "stream": stream, "messages": [{"role": "user", "content": text}]});
        self.req(reqwest::Method::POST, "/v1/chat/completions", Some(body))
            .await
    }

    async fn status(&self) -> Value {
        serde_json::from_str(
            &self
                .req(reqwest::Method::GET, "/router/status", None)
                .await
                .1,
        )
        .unwrap()
    }

    async fn swaps(&self) -> Vec<Value> {
        self.status().await["history"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|h| h["kind"] == "swap")
            .cloned()
            .collect()
    }

    fn events(&self) -> Vec<Value> {
        std::fs::read_to_string(self.logdir().join("router-events.jsonl"))
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }

    /// SIGTERM the router and wait (bounded) for it to exit.
    fn terminate(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = kill(
                Pid::from_raw(i32::try_from(child.id()).unwrap()),
                Signal::SIGTERM,
            );
            let t0 = Instant::now();
            loop {
                if child.try_wait().unwrap().is_some() {
                    break;
                }
                if t0.elapsed() > Duration::from_secs(30) {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!(
                        "router did not exit within 30 s of SIGTERM: {}",
                        self.router_err()
                    );
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }

    /// The prototype's tearDown: stop the router, then no backend may be left listening.
    fn finish(mut self) {
        self.terminate();
        for p in [self.pa, self.pb, self.pc] {
            assert!(
                !port_open(p),
                "backend on {p} left running after router stop\n{}\n{:#?}",
                self.router_err(),
                self.events()
            );
        }
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!(
                "--- router stderr ---\n{}\n--- events ---\n{:?}",
                self.router_err(),
                self.events()
            );
        }
        // SIGTERM first, so the router stops its backend: a SIGKILLed router leaves the backend
        // running in its own process group (noevia#2444), which a failing test must not do.
        if let Some(mut child) = self.child.take() {
            if let Ok(pid) = i32::try_from(child.id()) {
                let _ = kill(Pid::from_raw(pid), Signal::SIGTERM);
            }
            let t0 = Instant::now();
            while matches!(child.try_wait(), Ok(None)) && t0.elapsed() < Duration::from_secs(20) {
                std::thread::sleep(Duration::from_millis(50));
            }
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn ids(models: &str) -> Vec<String> {
    let v: Value = serde_json::from_str(models).unwrap();
    v["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap().to_string())
        .collect()
}

// ---------- the prototype's twelve ----------

#[tokio::test(flavor = "multi_thread")]
async fn models_list_excludes_disabled() {
    let h = Harness::start("", "").await;
    let (s, d) = h.req(reqwest::Method::GET, "/v1/models", None).await;
    assert_eq!(s, 200);
    let ids = ids(&d);
    assert!(
        ids.contains(&"dense-a".into()) && ids.contains(&"a".into()),
        "{ids:?}"
    );
    assert!(!ids.contains(&"disabled".into()));
    h.finish();
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_model_404() {
    let h = Harness::start("", "").await;
    let (s, d) = h.chat("nope", "hi", false).await;
    assert_eq!(s, 404);
    assert!(d.contains("model_not_found"), "{d}");
    h.finish();
}

#[tokio::test(flavor = "multi_thread")]
async fn route_alias_and_swap() {
    let h = Harness::start("", "").await;
    let (s, d) = h.chat("a", "one", false).await;
    assert_eq!(s, 200);
    assert!(d.contains("A: one"), "{d}");
    assert!(port_open(h.pa));
    let (s, d) = h.chat("moe-b", "two", false).await;
    assert_eq!(s, 200);
    assert!(
        d.contains("B: two") && d.contains("model=upstream-b"),
        "upstream_model rewrite: {d}"
    );
    assert!(!port_open(h.pa), "previous backend still up after swap");
    let (_, d) = h.chat("dense-a", "three", false).await;
    assert!(d.contains("A: three"));
    assert!(!port_open(h.pb));
    let swaps = h.swaps().await;
    let pairs: Vec<(Value, Value)> = swaps
        .iter()
        .map(|s| (s["from"].clone(), s["to"].clone()))
        .collect();
    assert_eq!(
        pairs,
        vec![
            (Value::Null, json!("dense-a")),
            (json!("dense-a"), json!("moe-b")),
            (json!("moe-b"), json!("dense-a"))
        ]
    );
    assert!(swaps
        .iter()
        .all(|s| s["ready_s"].is_number() && s["total_s"].is_number()));
    assert_eq!(h.events().iter().filter(|e| e["kind"] == "swap").count(), 3);
    h.finish();
}

#[tokio::test(flavor = "multi_thread")]
async fn no_swap_for_same_model() {
    let h = Harness::start("", "").await;
    h.chat("dense-a", "x", false).await;
    h.chat("dense-a", "x", false).await;
    h.chat("a", "x", false).await;
    assert_eq!(h.swaps().await.len(), 1);
    h.finish();
}

#[tokio::test(flavor = "multi_thread")]
async fn streaming_passthrough() {
    let h = Harness::start("", "").await;
    let body = json!({"model": "moe-b", "stream": true, "messages": [{"role": "user", "content": "x y z"}]});
    let t0 = Instant::now();
    let resp = h
        .http
        .post(h.url("/v1/chat/completions?probe=1"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["content-type"], "text/event-stream");
    let mut stream = resp.bytes_stream();
    let (mut first, mut buf) = (None, Vec::new());
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.unwrap();
        if first.is_none() && chunk.windows(6).any(|w| w == b"data: ") {
            first = Some(t0.elapsed());
        }
        buf.extend_from_slice(&chunk);
    }
    let total = t0.elapsed();
    let text = String::from_utf8(buf).unwrap();
    let lines: Vec<&str> = text.lines().filter(|l| l.starts_with("data: ")).collect();
    assert_eq!(lines.last().copied(), Some("data: [DONE]"));
    let content: String = lines[..lines.len() - 1]
        .iter()
        .map(|l| {
            serde_json::from_str::<Value>(&l[6..]).unwrap()["choices"][0]["delta"]["content"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    assert!(content.contains("B: x y z"), "{content}");
    assert!(
        content.contains("path=/v1/chat/completions?probe=1"),
        "query string forwarded: {content}"
    );
    assert!(
        first.unwrap() < total - Duration::from_millis(150),
        "first chunk was not forwarded before the stream ended"
    );
    h.finish();
}

#[tokio::test(flavor = "multi_thread")]
async fn failed_start_is_503_and_router_recovers() {
    let h = Harness::start("", "").await;
    h.chat("dense-a", "x", false).await;
    let (s, d) = h.chat("broken", "x", false).await;
    assert_eq!(s, 503);
    assert!(d.contains("exited"), "{d}");
    assert!(h.status().await["loaded"].is_null());
    let (s, _) = h.chat("dense-a", "x", false).await;
    assert_eq!(s, 200);
    h.finish();
}

#[tokio::test(flavor = "multi_thread")]
async fn swap_waits_for_inflight_stream() {
    let h = std::sync::Arc::new(Harness::start("", "").await);
    let long = {
        let h = h.clone();
        let words = vec!["w"; 40].join(" "); // about 2 s of chunks
        tokio::spawn(async move { h.chat("moe-b", &words, true).await })
    };
    tokio::time::sleep(Duration::from_millis(1600)).await; // B is loaded and streaming
    let (s, d) = h.chat("dense-a", "after", false).await;
    let (bs, bd) = long.await.unwrap();
    assert_eq!(bs, 200);
    assert!(
        bd.contains("[DONE]"),
        "the stream was cut by the swap: {bd}"
    );
    assert_eq!(s, 200);
    assert!(d.contains("A: after"));
    std::sync::Arc::into_inner(h).unwrap().finish();
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_mixed_requests_all_answered() {
    let h = std::sync::Arc::new(Harness::start("", "").await);
    let mut tasks = Vec::new();
    for i in 0..6 {
        let h = h.clone();
        let m = ["dense-a", "moe-b"][i % 2];
        tasks.push(tokio::spawn(async move {
            (m, h.chat(m, &format!("q{i}"), false).await)
        }));
    }
    for t in tasks {
        let (m, (s, d)) = t.await.unwrap();
        assert_eq!(s, 200, "{d}");
        assert!(
            d.contains(if m == "dense-a" { "A: " } else { "B: " }),
            "{m}: {d}"
        );
    }
    std::sync::Arc::into_inner(h).unwrap().finish();
}

#[tokio::test(flavor = "multi_thread")]
async fn unload_and_ttl() {
    let h = Harness::start("", "").await;
    h.chat("dense-a", "x", false).await;
    let (_, d) = h
        .req(reqwest::Method::POST, "/router/unload", Some(json!({})))
        .await;
    assert_eq!(
        serde_json::from_str::<Value>(&d).unwrap()["unloaded"],
        "dense-a"
    );
    assert!(!port_open(h.pa));
    h.chat("short-ttl", "x", false).await;
    assert!(port_open(h.pc));
    tokio::time::sleep(Duration::from_millis(4500)).await;
    assert!(h.status().await["loaded"].is_null());
    assert!(!port_open(h.pc));
    h.finish();
}

#[tokio::test(flavor = "multi_thread")]
async fn router_sigterm_stops_backend() {
    let mut h = Harness::start("", "").await;
    h.chat("dense-a", "x", false).await;
    assert!(port_open(h.pa));
    h.terminate();
    assert!(!port_open(h.pa));
    assert!(h
        .events()
        .iter()
        .any(|e| e["kind"] == "shutdown" && e["model"] == "dense-a"));
    h.finish();
}

#[tokio::test(flavor = "multi_thread")]
async fn refuses_non_loopback() {
    let h = Harness::start("", "").await;
    let out = Command::new(BIN)
        .arg("--config")
        .arg(h.config_path())
        .args(["--host", "0.0.0.0", "--port", &free_port().to_string()])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("loopback"));
    h.finish();
}

/// The real tiered-engine OpenAI server with its mock engine as a backend (no GPU); skipped where
/// that checkout is absent (CI).
#[tokio::test(flavor = "multi_thread")]
async fn strata_mock_stream_and_swap() {
    let (Some(py), Some(srv)) = (
        std::env::var_os("STRATA_PY"),
        std::env::var_os("STRATA_SRV"),
    ) else {
        eprintln!("skipped: set STRATA_PY and STRATA_SRV to the tiered engine's venv python and serve/server_intel.py");
        return;
    };
    let ps = free_port();
    let extra = format!(
        "[[models]]\nname = \"strata-mock\"\nengine = \"strata\"\ncmd = \"exec {} {} --engine mock --host 127.0.0.1 --port {ps} --offline\"\nproxy = \"http://127.0.0.1:{ps}\"\nhealth_expect = '\"status\": \"ok\"'\n",
        Path::new(&py).display(),
        Path::new(&srv).display()
    );
    let h = Harness::start("", &extra).await;
    let (s, d) = h.chat("strata-mock", "hello", true).await;
    assert_eq!(s, 200, "{d}");
    assert!(
        d.contains("chat.completion.chunk") && d.contains("[DONE]"),
        "{d}"
    );
    let (_, d) = h.chat("strata-mock", "hello", false).await;
    assert!(d.contains("Hello from the mock engine"), "{d}");
    let (s, _) = h.chat("dense-a", "x", false).await;
    assert_eq!(s, 200);
    assert!(!port_open(ps));
    h.finish();
    assert!(!port_open(ps));
}

// ---------- new cases ----------

/// A client that drops a stream half way must release its lease (no wedged swap) and the
/// upstream must see the disconnect (the generation is cancelled, not run to the end).
#[tokio::test(flavor = "multi_thread")]
async fn client_abort_mid_stream_does_not_wedge() {
    let abort_file = tempfile::NamedTempFile::new().unwrap();
    let pe = free_port();
    let fb = format!("python3 {}", support("fake_backend.py").display());
    let extra = format!(
        "[[models]]\nname = \"slow-e\"\ncmd = \"exec {fb} --port {pe} --name E --slow 0.2 --abort-file {}\"\nproxy = \"http://127.0.0.1:{pe}\"\n",
        abort_file.path().display()
    );
    let h = Harness::start("", &extra).await;
    let body = json!({"model": "slow-e", "stream": true, "messages": [{"role": "user", "content": vec!["w"; 200].join(" ")}]});
    let resp = h
        .http
        .post(h.url("/v1/chat/completions"))
        .json(&body)
        .send()
        .await
        .unwrap();
    let mut stream = resp.bytes_stream();
    stream.next().await.unwrap().unwrap();
    stream.next().await.unwrap().unwrap();
    assert_eq!(h.status().await["inflight"], 1);
    drop(stream); // the client goes away about 40 s before the stream would end
    let t0 = Instant::now();
    loop {
        if h.status().await["inflight"] == 0 {
            break;
        }
        assert!(
            t0.elapsed() < Duration::from_secs(5),
            "inflight stuck after a client abort"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    eventually(
        "the upstream to see the disconnect",
        Duration::from_secs(5),
        || {
            std::fs::read_to_string(abort_file.path())
                .unwrap_or_default()
                .contains("E client gone")
        },
    )
    .await;
    let (s, d) = tokio::time::timeout(Duration::from_secs(20), h.chat("dense-a", "next", false))
        .await
        .expect("swap after abort wedged");
    assert_eq!(s, 200, "{d}");
    assert!(!port_open(pe));
    h.finish();
    assert!(!port_open(pe));
}

/// Requests that arrive while a swap is in progress wait for it and share it: one swap, all
/// answered. Requests queued for a start that fails all get its error, after one attempt.
#[tokio::test(flavor = "multi_thread")]
async fn concurrent_requests_during_swap_share_it() {
    let h = std::sync::Arc::new(Harness::start("", "").await);
    h.chat("dense-a", "x", false).await;
    let first = {
        let h = h.clone();
        tokio::spawn(async move { h.chat("moe-b", "first", false).await })
    };
    let t0 = Instant::now();
    while h.status().await["swapping"] != true {
        assert!(t0.elapsed() < Duration::from_secs(5), "swap never started");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let mut late = Vec::new();
    for i in 0..4 {
        let h = h.clone();
        let m = if i == 3 { "dense-a" } else { "moe-b" };
        late.push(tokio::spawn(async move {
            (m, h.chat(m, &format!("late{i}"), false).await)
        }));
    }
    let (s, d) = first.await.unwrap();
    assert_eq!(s, 200, "{d}");
    for t in late {
        let (m, (s, d)) = t.await.unwrap();
        assert_eq!(s, 200, "{m}: {d}");
        assert!(
            d.contains(if m == "dense-a" { "A: late" } else { "B: late" }),
            "{d}"
        );
    }
    let to: Vec<Value> = h.swaps().await.iter().map(|s| s["to"].clone()).collect();
    assert_eq!(
        to,
        vec![json!("dense-a"), json!("moe-b"), json!("dense-a")],
        "the moe-b requests shared one swap: {}",
        h.router_err()
    );

    let mut broken = Vec::new();
    for _ in 0..3 {
        let h = h.clone();
        broken.push(tokio::spawn(
            async move { h.chat("broken", "x", false).await },
        ));
    }
    for t in broken {
        let (s, d) = t.await.unwrap();
        assert_eq!(s, 503, "{d}");
    }
    let attempts = h
        .swaps()
        .await
        .iter()
        .filter(|s| s["to"] == "broken")
        .count();
    assert_eq!(
        attempts, 1,
        "queued requests must share the failed start, not retry it each"
    );
    std::sync::Arc::into_inner(h).unwrap().finish();
}

/// A backend that never becomes healthy is given up at its ready_timeout, stopped, and the
/// router keeps serving.
#[tokio::test(flavor = "multi_thread")]
async fn never_healthy_backend_times_out() {
    let pn = free_port();
    let fb = format!("python3 {}", support("fake_backend.py").display());
    let extra = format!(
        "[[models]]\nname = \"never\"\nready_timeout = 2\ncmd = \"exec {fb} --port {pn} --name N --delay 999\"\nproxy = \"http://127.0.0.1:{pn}\"\n"
    );
    let h = Harness::start("", &extra).await;
    let t0 = Instant::now();
    let (s, d) = h.chat("never", "x", false).await;
    assert_eq!(s, 503);
    assert!(d.contains("not ready after 2s"), "{d}");
    assert!(t0.elapsed() < Duration::from_secs(12), "{:?}", t0.elapsed());
    assert!(!port_open(pn), "the unhealthy backend was left running");
    let swap = h.swaps().await.pop().unwrap();
    assert!(
        swap["failed_stop_s"].is_number() && swap["error"].as_str().unwrap().contains("never"),
        "{swap}"
    );
    let (s, _) = h.chat("dense-a", "x", false).await;
    assert_eq!(s, 200);
    h.finish();
}

/// A stop that leaves the backend's container behind must refuse the swap: the next backend is
/// not started while the old one may still hold the GPU, until the container is gone.
#[tokio::test(flavor = "multi_thread")]
async fn stuck_container_refuses_swap() {
    let ps = free_port();
    let fb = format!("python3 {}", support("fake_backend.py").display());
    let docker = support("fake-docker");
    let extra = format!(
        "[[models]]\nname = \"boxed\"\ncmd = \"exec {fb} --port {ps} --name S --container nrouter-test\"\nproxy = \"http://127.0.0.1:{ps}\"\nstop = \"{} stop nrouter-test\"\ncontainers = [\"nrouter-test\"]\n",
        docker.display()
    );
    let h = Harness::start("", &extra).await;
    let (s, d) = h.chat("boxed", "x", false).await;
    assert_eq!(s, 200, "{d}");
    let container = h.docker_dir().join("nrouter-test");
    assert!(container.exists());
    std::fs::write(&container, "sticky").unwrap(); // `docker rm -f` will not remove it now
                                                   // An unload that cannot verify the container gone says so.
    let (s, d) = h
        .req(reqwest::Method::POST, "/router/unload", Some(json!({})))
        .await;
    assert_eq!(s, 503, "{d}");
    assert!(d.contains("nrouter-test"), "{d}");
    let unload = h.swaps().await.pop().unwrap();
    assert_eq!(unload["stop_cmd_rc"], 0, "{unload}");
    assert_eq!(unload["stop_containers_left"], json!(["nrouter-test"]));
    let (s, d) = h.chat("dense-a", "x", false).await;
    assert_eq!(s, 503, "{d}");
    assert!(d.contains("nrouter-test") && d.contains("refusing"), "{d}");
    assert!(
        !port_open(h.pa),
        "the next backend was started while the old container remained"
    );
    let st = h.status().await;
    assert_eq!(st["stuck_containers"], json!(["nrouter-test"]));
    assert!(st["loaded"].is_null());
    let swap = h.swaps().await.pop().unwrap();
    assert!(
        swap["error"].as_str().unwrap().contains("still present"),
        "{swap}"
    );
    // The container finally goes: the next request removes it, verifies, and starts.
    std::fs::write(&container, "").unwrap();
    let (s, d) = h.chat("dense-a", "y", false).await;
    assert_eq!(s, 200, "{d}");
    assert!(!container.exists());
    assert_eq!(h.status().await["stuck_containers"], json!([]));
    h.finish();
}

/// ttl counts from the end of the last request: a stream longer than the ttl is not cut, and the
/// model unloads once it has been idle for the ttl.
#[tokio::test(flavor = "multi_thread")]
async fn ttl_unload_after_idle_only() {
    let h = Harness::start("", "").await;
    let t0 = Instant::now();
    let (s, d) = h.chat("short-ttl", &vec!["w"; 18].join(" "), true).await; // about 4 s at 0.2 s a chunk, ttl 2 s
    assert_eq!(s, 200);
    assert!(d.contains("[DONE]"), "the stream was cut by the ttl: {d}");
    assert!(t0.elapsed() > Duration::from_secs(3));
    assert_eq!(h.status().await["loaded"], "short-ttl");
    tokio::time::sleep(Duration::from_millis(1000)).await;
    assert_eq!(
        h.status().await["loaded"],
        "short-ttl",
        "unloaded before the ttl passed"
    );
    let t1 = Instant::now();
    loop {
        // `loaded` clears when the unload starts; wait for the stop to finish too.
        let st = h.status().await;
        if st["loaded"].is_null() && st["swapping"] == false {
            break;
        }
        assert!(
            t1.elapsed() < Duration::from_secs(8),
            "ttl unload did not happen"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(!port_open(h.pc));
    let last = h.swaps().await.pop().unwrap();
    assert_eq!(
        (last["from"].as_str(), last["reason"].as_str()),
        (Some("short-ttl"), Some("ttl"))
    );
    h.finish();
}

fn lock_is_free(path: &Path) -> bool {
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .unwrap();
    rustix::fs::flock(&f, rustix::fs::FlockOperation::NonBlockingLockExclusive).is_ok()
}

/// With `gpu_lock` the router holds the lock while a backend runs, frees it on unload, and waits
/// (then refuses) while another job holds it.
#[tokio::test(flavor = "multi_thread")]
async fn gpu_lock_held_while_loaded() {
    let lockdir = tempfile::tempdir().unwrap();
    let lock = lockdir.path().join("gpu.lock");
    let h = Harness::start(
        &format!("gpu_lock = \"{}\"\ngpu_lock_timeout = 1.5", lock.display()),
        "",
    )
    .await;
    assert!(lock_is_free(&lock));
    let (s, _) = h.chat("dense-a", "x", false).await;
    assert_eq!(s, 200);
    assert!(!lock_is_free(&lock), "lock not held while a backend runs");
    assert_eq!(h.status().await["gpu_lock"]["held"], true);
    h.req(reqwest::Method::POST, "/router/unload", Some(json!({})))
        .await;
    assert!(lock_is_free(&lock), "lock still held after unload");
    // Another job holds it: the start waits gpu_lock_timeout, then refuses without starting.
    let other = std::fs::OpenOptions::new().write(true).open(&lock).unwrap();
    rustix::fs::flock(&other, rustix::fs::FlockOperation::NonBlockingLockExclusive).unwrap();
    let (s, d) = h.chat("dense-a", "x", false).await;
    assert_eq!(s, 503);
    assert!(d.contains("held by another job"), "{d}");
    assert!(!port_open(h.pa));
    drop(other);
    let (s, _) = h.chat("dense-a", "x", false).await;
    assert_eq!(s, 200);
    h.finish();
    assert!(lock_is_free(&lock));
}

/// A backend that ignores SIGTERM is SIGKILLed after its stop_timeout.
#[tokio::test(flavor = "multi_thread")]
async fn stop_escalates_to_sigkill() {
    let pk = free_port();
    let fb = format!("python3 {}", support("fake_backend.py").display());
    let extra = format!(
        "[[models]]\nname = \"stubborn\"\nstop_timeout = 1\ncmd = \"exec {fb} --port {pk} --name K --ignore-term\"\nproxy = \"http://127.0.0.1:{pk}\"\n"
    );
    let h = Harness::start("", &extra).await;
    assert_eq!(h.chat("stubborn", "x", false).await.0, 200);
    assert_eq!(h.chat("dense-a", "x", false).await.0, 200);
    assert!(!port_open(pk));
    let swap = h.swaps().await.pop().unwrap();
    assert_eq!(
        swap["stop_signals"],
        json!(["SIGTERM", "SIGKILL"]),
        "{swap}"
    );
    h.finish();
}

/// A busy model cannot starve a request for another one: requests for the loaded model that arrive
/// after it queue behind it.
#[tokio::test(flavor = "multi_thread")]
async fn busy_model_does_not_starve_another() {
    let h = std::sync::Arc::new(Harness::start("", "").await);
    h.chat("moe-b", "warm", false).await;
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut pumps = Vec::new();
    for _ in 0..2 {
        let (h, stop) = (h.clone(), stop.clone());
        pumps.push(tokio::spawn(async move {
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                let (s, _) = h.chat("moe-b", "w w w w w w w w", true).await;
                assert_eq!(s, 200);
            }
        }));
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    let (s, d) = tokio::time::timeout(Duration::from_secs(15), h.chat("dense-a", "fair", false))
        .await
        .expect("starved");
    assert_eq!(s, 200, "{d}");
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    for p in pumps {
        p.await.unwrap();
    }
    std::sync::Arc::into_inner(h).unwrap().finish();
}

/// An upstream that accepts a request and then goes silent is cut at `upstream_read_timeout`:
/// the lease is freed and the router can swap again.
#[tokio::test(flavor = "multi_thread")]
async fn hung_upstream_frees_its_lease() {
    let ph = free_port();
    let fb = format!("python3 {}", support("fake_backend.py").display());
    let extra = format!(
        "[[models]]\nname = \"hung\"\ncmd = \"exec {fb} --port {ph} --name H --hang\"\nproxy = \"http://127.0.0.1:{ph}\"\n"
    );
    let h = Harness::start("upstream_read_timeout = 1.5", &extra).await;
    let t0 = Instant::now();
    let (s, d) = h.chat("hung", "x", false).await;
    assert_eq!(s, 502, "{d}");
    assert!(t0.elapsed() < Duration::from_secs(15), "{:?}", t0.elapsed());
    assert_eq!(h.status().await["inflight"], 0);
    let (s, _) = h.chat("dense-a", "x", false).await;
    assert_eq!(s, 200);
    assert!(!port_open(ph));
    h.finish();
}

/// SIGTERM while a swap is still stopping the old backend: the router exits without starting the
/// new one.
#[tokio::test(flavor = "multi_thread")]
async fn sigterm_during_stop_never_starts_the_next_backend() {
    let pw = free_port();
    let fb = format!("python3 {}", support("fake_backend.py").display());
    let extra = format!(
        "[[models]]\nname = \"slow-stop\"\nstop = \"sleep 3\"\ncmd = \"exec {fb} --port {pw} --name W\"\nproxy = \"http://127.0.0.1:{pw}\"\n"
    );
    let mut h = Harness::start("", &extra).await;
    assert_eq!(h.chat("slow-stop", "x", false).await.0, 200);
    let url = h.url("/v1/chat/completions");
    let pending = tokio::spawn(async move {
        let body = json!({"model": "dense-a", "messages": [{"role": "user", "content": "x"}]});
        reqwest::Client::new()
            .post(url)
            .json(&body)
            .send()
            .await
            .map(|r| r.status().as_u16())
    });
    tokio::time::sleep(Duration::from_millis(1000)).await; // the stop command is running
    assert_eq!(h.status().await["swapping"], true);
    let watcher = {
        let pa = h.pa;
        std::thread::spawn(move || {
            let t0 = Instant::now();
            let mut opened = false;
            while t0.elapsed() < Duration::from_secs(8) {
                opened |= port_open(pa);
                std::thread::sleep(Duration::from_millis(50));
            }
            opened
        })
    };
    h.terminate();
    let status = pending.await.unwrap();
    assert!(status.as_ref().map_or(true, |s| *s == 503), "{status:?}");
    assert!(
        !watcher.join().unwrap(),
        "the next backend started during shutdown\n{}\n{:?}",
        h.router_err(),
        h.events()
    );
    assert!(!port_open(pw));
    h.finish();
}
