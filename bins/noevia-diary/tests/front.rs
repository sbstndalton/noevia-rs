//! R9 S2a: `noevia-diary --main` (front.rs) over a stub Python child. In-process tests drive the
//! public and control handlers; the process tests run the real binary with a python3 stub child.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use axum::body::Body;
use axum::http::{Request, Response};
use noevia_diary::front::{self, Front};
use std::path::Path;
use std::sync::{Arc, Mutex};

const KEY: &str = "synthetic-tenant-key";
const TOKEN: &str = "synthetic-service-token";
const CTL: &str = "synthetic-control-token";
const SECRET: &str = "synthetic-parent-secret";
const UID: &str = "0b5f6c1e-3a2d-4c8e-9f10-1a2b3c4d5e6f";

fn managed_tenant(root: &Path, files: &[(&str, &str)]) {
    let t = root.join("users").join(UID);
    std::fs::create_dir_all(&t).unwrap();
    let db = rusqlite::Connection::open(t.join("managed-diary.db")).unwrap();
    db.execute_batch(
        "CREATE TABLE files(path TEXT PRIMARY KEY, data BLOB NOT NULL, version TEXT NOT NULL, updated REAL NOT NULL);
         CREATE TABLE directories(path TEXT PRIMARY KEY);
         CREATE TABLE workspace_imports(fingerprint TEXT PRIMARY KEY, destination TEXT NOT NULL);
         CREATE TABLE workspace_index_outbox(path TEXT PRIMARY KEY);
         CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
         CREATE TABLE backups(destination TEXT PRIMARY KEY, generation INTEGER NOT NULL DEFAULT -1,
             last_success REAL, retry_at REAL NOT NULL DEFAULT 0, failures INTEGER NOT NULL DEFAULT 0, error TEXT);",
    )
    .unwrap();
    for (k, v) in [
        ("tenant", UID),
        ("active", "1"),
        ("generation", "1"),
        ("settings", "{}"),
    ] {
        db.execute("INSERT INTO meta VALUES (?,?)", [k, v]).unwrap();
    }
    for (p, d) in files {
        db.execute(
            "INSERT INTO files VALUES (?,?,'v',1.0)",
            rusqlite::params![p, d.as_bytes()],
        )
        .unwrap();
    }
    let idx = rusqlite::Connection::open(t.join("managed-index.db")).unwrap();
    idx.execute_batch(
        "CREATE TABLE dirty_documents (document TEXT PRIMARY KEY);
         CREATE TABLE journal (id TEXT PRIMARY KEY, created_at TEXT NOT NULL, kind TEXT NOT NULL, payload TEXT NOT NULL,
             applied INTEGER NOT NULL DEFAULT 0, applied_at TEXT, attempts INTEGER NOT NULL DEFAULT 0, last_error TEXT);",
    )
    .unwrap();
    std::fs::write(t.join("managed-diary.active"), UID).unwrap();
}

/// The start-up config the child POSTs (rust_forward.server_config's shape).
fn child_config(root: &Path, key: &str) -> String {
    serde_json::json!({
        "port": 0, "control_token": CTL, "forward_secret": SECRET, "auth_token": TOKEN, "tenant_key": key,
        "data_root": root.to_str().unwrap(), "legacy_user_id": "",
        "corpus": {"entry_layout": "daily", "entries_prefix": "Entries", "index_enabled": false,
                   "month_file_template": "{year}-{month02}.md", "monthly_prefix": "", "index_file": "INDEX.md"},
        "llm": {"main": {"base_url": "http://127.0.0.1:9/v1", "chat_model": "chat-a", "setting_key": "LLM_CHAT_MODEL",
                          "fallback_setting_key": "LLM_CHAT_FALLBACK_MODEL"},
                "aux": {"base_url": "http://127.0.0.1:9/v1", "chat_model": "aux-a", "setting_key": "LLM_AUX_MODEL",
                         "fallback_setting_key": "LLM_AUX_FALLBACK_MODEL"},
                "aux_prefers_main": true, "configured": "chat-a"},
    })
    .to_string()
}

/// What the stub child saw of one request.
#[derive(Clone, Debug)]
struct Seen {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Seen {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

type Log = Arc<Mutex<Vec<Seen>>>;

/// A stub child on an ephemeral port: records each request, answers 207 `child` with an internal
/// header the parent must drop, and stamps the tenant recovered. `/stream` answers two chunks
/// 300 ms apart.
async fn stub_child() -> (u16, Log) {
    let log: Log = Arc::new(Mutex::new(Vec::new()));
    let l2 = log.clone();
    let router = axum::Router::new().fallback(move |req: Request<Body>| {
        let log = l2.clone();
        async move {
            let (parts, body) = req.into_parts();
            let body = axum::body::to_bytes(body, usize::MAX).await.unwrap().to_vec();
            log.lock().unwrap().push(Seen {
                method: parts.method.to_string(),
                target: parts.uri.to_string(),
                headers: parts
                    .headers
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("?").to_string()))
                    .collect(),
                body,
            });
            if parts.uri.path() == "/stream" {
                let s = async_stream(vec![b"first".to_vec(), b"second".to_vec()]);
                return Response::builder()
                    .status(200)
                    .body(Body::from_stream(s))
                    .unwrap();
            }
            Response::builder()
                .status(207)
                .header("x-noevia-diary-secret-detail", "drop me")
                .header("x-noevia-diary-recovered", "1")
                .header("x-child", "kept")
                .body(Body::from("child"))
                .unwrap()
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, router).await });
    (port, log)
}

fn async_stream(
    chunks: Vec<Vec<u8>>,
) -> impl futures_util::Stream<Item = Result<Vec<u8>, std::io::Error>> + Send {
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    tokio::spawn(async move {
        for (i, c) in chunks.into_iter().enumerate() {
            if i > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            }
            if tx.send(Ok(c)).await.is_err() {
                return;
            }
        }
    });
    tokio_stream_from(rx)
}

fn tokio_stream_from<T: Send + 'static>(
    mut rx: tokio::sync::mpsc::Receiver<T>,
) -> impl futures_util::Stream<Item = T> + Send {
    futures_util::stream::poll_fn(move |cx| rx.poll_recv(cx))
}

struct Started {
    public: std::net::SocketAddr,
    control: std::net::SocketAddr,
    log: Log,
}

async fn start(root: &Path, key: &str, install: bool) -> Started {
    let (child_port, log) = stub_child().await;
    let f = Arc::new(Front::new(CTL.into(), SECRET.into(), child_port));
    let public = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let control = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let s = Started {
        public: public.local_addr().unwrap(),
        control: control.local_addr().unwrap(),
        log,
    };
    tokio::spawn(front::serve_with(public, f.clone(), front::public));
    tokio::spawn(front::serve_with(control, f.clone(), front::control));
    if install {
        let r = reqwest::Client::new()
            .post(format!("http://{}/internal/child/config", s.control))
            .header("x-noevia-diary-control", CTL)
            .body(child_config(root, key))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
    }
    s
}

static NONCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn sign(method: &str, path: &str, query: &str, body: &[u8]) -> String {
    let hash = noevia_diary::assertion::sha256_hex(body);
    let req = tenant_assertion::Request {
        key: KEY,
        user_id: UID,
        assertion: "",
        method,
        path,
        query: query.as_bytes(),
        body_hash: &hash,
        storage: "",
        legacy_owner: "",
        blocked: "",
        now: 0.0,
    };
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let nonce = format!(
        "{:032x}",
        NONCE.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    );
    tenant_assertion::sign(&req, ts, &nonce).unwrap()
}

#[tokio::test]
async fn before_the_child_config_everything_is_503_and_the_config_is_checked() {
    let dir = tempfile::tempdir().unwrap();
    let s = start(dir.path(), "", false).await;
    let r = reqwest::get(format!("http://{}/api/health", s.public))
        .await
        .unwrap();
    assert_eq!(r.status(), 503);
    assert_eq!(
        r.text().await.unwrap().as_bytes(),
        front::STARTING,
        "starting body"
    );
    let post = |token: &'static str, body: String| {
        let url = format!("http://{}/internal/child/config", s.control);
        async move {
            reqwest::Client::new()
                .post(url)
                .header("x-noevia-diary-control", token)
                .body(body)
                .send()
                .await
                .unwrap()
                .status()
                .as_u16()
        }
    };
    assert_eq!(post("wrong", child_config(dir.path(), "")).await, 403);
    assert_eq!(post(CTL, "{".into()).await, 400);
    // A config that does not echo this process's secret is refused.
    let forged = child_config(dir.path(), "").replace(SECRET, "other-secret");
    assert_eq!(post(CTL, forged).await, 409);
    assert_eq!(post(CTL, child_config(dir.path(), "")).await, 200);
    assert_eq!(post(CTL, child_config(dir.path(), "")).await, 409);
    // The control listener serves only /internal/ paths.
    let r = reqwest::get(format!("http://{}/api/health", s.control))
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
    let r = reqwest::Client::new()
        .get(format!("http://{}/internal/ready", s.control))
        .header("x-noevia-diary-control", CTL)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(s.log.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_python_route_is_forwarded_with_the_parent_facts_only() {
    let dir = tempfile::tempdir().unwrap();
    let s = start(dir.path(), "", true).await;
    let r = reqwest::Client::new()
        .post(format!("http://{}/v1/chat/completions?x=1&y=%20", s.public))
        .header("authorization", format!("Bearer {TOKEN}"))
        .header("content-type", "application/json")
        .header("x-noevia-diary-parent", "client-forged")
        .header("x-noevia-diary-asserted", "1")
        .header("x-noevia-diary-recovered", "1")
        .body(r#"{"messages":[]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 207);
    assert_eq!(r.headers().get("x-child").unwrap(), "kept");
    assert!(r
        .headers()
        .keys()
        .all(|k| !k.as_str().starts_with("x-noevia-diary-")));
    assert_eq!(r.text().await.unwrap(), "child");
    let seen = s.log.lock().unwrap().clone();
    assert_eq!(seen.len(), 1);
    let seen = &seen[0];
    assert_eq!(seen.method, "POST");
    assert_eq!(seen.target, "/v1/chat/completions?x=1&y=%20");
    assert_eq!(seen.body, br#"{"messages":[]}"#);
    assert_eq!(seen.header("x-noevia-diary-parent"), Some(SECRET));
    assert_eq!(seen.header("x-noevia-diary-asserted"), Some("0"));
    assert_eq!(seen.header("x-noevia-diary-recovered"), None);
    assert_eq!(
        seen.header("authorization"),
        Some(format!("Bearer {TOKEN}").as_str())
    );
    assert_eq!(
        seen.headers
            .iter()
            .filter(|(k, _)| k == "x-noevia-diary-parent")
            .count(),
        1
    );
}

#[tokio::test]
async fn the_parent_is_the_only_verifier_and_nonce_owner() {
    let dir = tempfile::tempdir().unwrap();
    let s = start(dir.path(), KEY, true).await;
    let client = reqwest::Client::new();
    let body = br#"{"day":"2030-01-06"}"#;
    let url = format!("http://{}/api/entries/append", s.public);
    let send = |assertion: String| {
        client
            .post(url.clone())
            .header("x-cowork-user-id", UID)
            .header("x-cowork-tenant-assertion", assertion)
            .header("content-type", "application/json")
            .body(body.to_vec())
            .send()
    };
    // A forged assertion is refused here; the child never sees it.
    let r = send("v2.1.00000000000000000000000000000000.AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".into())
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    assert_eq!(
        r.text().await.unwrap(),
        r#"{"detail":"invalid tenant assertion"}"#
    );
    assert!(s.log.lock().unwrap().is_empty());
    // A valid one goes to the child as asserted, with the exact body it signed.
    let good = sign("POST", "/api/entries/append", "", body);
    let r = send(good.clone()).await.unwrap();
    assert_eq!(r.status(), 207);
    {
        let seen = s.log.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].header("x-noevia-diary-asserted"), Some("1"));
        assert_eq!(seen[0].body, body);
    }
    // Its nonce is spent: a replay is refused here.
    let r = send(good).await.unwrap();
    assert_eq!(r.status(), 401);
    assert_eq!(s.log.lock().unwrap().len(), 1);
    // A non-JSON body signs "stream" and is passed through as sent.
    let zip = b"PK\x03\x04synthetic".to_vec();
    let req = tenant_assertion::Request {
        key: KEY,
        user_id: UID,
        assertion: "",
        method: "POST",
        path: "/api/workspace-import",
        query: b"",
        body_hash: "stream",
        storage: "",
        legacy_owner: "",
        blocked: "",
        now: 0.0,
    };
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let a = tenant_assertion::sign(&req, ts, &format!("{:032x}", 9_999_999u64)).unwrap();
    let r = client
        .post(format!("http://{}/api/workspace-import", s.public))
        .header("x-cowork-user-id", UID)
        .header("x-cowork-tenant-assertion", a)
        .header("content-type", "application/zip")
        .body(zip.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 207);
    let seen = s.log.lock().unwrap();
    assert_eq!(seen[1].body, zip);
    assert_eq!(seen[1].header("x-noevia-diary-asserted"), Some("1"));
}

#[tokio::test]
async fn native_reads_wait_for_the_childs_recovered_stamp() {
    let dir = tempfile::tempdir().unwrap();
    let day = "## Sunday, January 6, 2030\n\n### 09:15 — walk\n\n**Me:** Walked by the river.\n\n**Assistant:** Noted.\n\n<!-- xid:aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee -->\n";
    managed_tenant(dir.path(), &[("Entries/2030/January/January 6, 2030.md", day)]);
    let s = start(dir.path(), KEY, true).await;
    let client = reqwest::Client::new();
    let get = || {
        client
            .get(format!("http://{}/api/months", s.public))
            .header("authorization", format!("Bearer {TOKEN}"))
            .header("x-cowork-user-id", UID)
            .header(
                "x-cowork-tenant-assertion",
                sign("GET", "/api/months", "", b""),
            )
            .send()
    };
    // Not yet recovered by the child: declined here, so the child serves it (and stamps it).
    let r = get().await.unwrap();
    assert_eq!(r.status(), 207);
    assert_eq!(s.log.lock().unwrap().len(), 1);
    // Now served natively, exactly as when Python forwarded it.
    let r = get().await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(
        r.text().await.unwrap(),
        r#"{"months":[{"id":"2030-01","label":"January 2030","file":"Entries/2030/January"}]}"#
    );
    assert_eq!(s.log.lock().unwrap().len(), 1);
    // A write is never native in S2a.
    let r = client
        .put(format!("http://{}/api/file", s.public))
        .header("authorization", format!("Bearer {TOKEN}"))
        .header("x-cowork-user-id", UID)
        .header(
            "x-cowork-tenant-assertion",
            sign("PUT", "/api/file", "", b"{}"),
        )
        .header("content-type", "application/json")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 207);
    assert_eq!(s.log.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn replies_stream_and_a_missing_child_is_503() {
    let dir = tempfile::tempdir().unwrap();
    let s = start(dir.path(), "", true).await;
    let mut r = reqwest::get(format!("http://{}/stream", s.public))
        .await
        .unwrap();
    let t0 = std::time::Instant::now();
    let first = r.chunk().await.unwrap().unwrap();
    assert_eq!(&first[..], b"first");
    assert!(
        t0.elapsed() < std::time::Duration::from_millis(250),
        "first chunk relayed before the second was produced"
    );
    let second = r.chunk().await.unwrap().unwrap();
    assert_eq!(&second[..], b"second");

    // A front whose child is gone answers 503 with a fixed body.
    let free = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let dead_port = free.local_addr().unwrap().port();
    drop(free);
    let f = Arc::new(Front::new(CTL.into(), SECRET.into(), dead_port));
    f.install(noevia_diary::config::parse(&child_config(dir.path(), "")).unwrap())
        .unwrap();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(front::serve_with(l, f, front::public));
    let r = reqwest::get(format!("http://{addr}/api/internal/embedding"))
        .await
        .unwrap();
    assert_eq!(r.status(), 503);
    assert_eq!(r.text().await.unwrap().as_bytes(), front::CHILD_DOWN);
}

// ---------------------------------------------------------------- the real process

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// A python3 child: reads the handshake, POSTs a config, serves `child` on its port, and writes
/// `term` to `marker` when it gets SIGTERM. Exits by itself after `exit_after` seconds if given.
const STUB: &str = r#"
import json, os, signal, sys, threading, urllib.request
from http.server import BaseHTTPRequestHandler, HTTPServer
hs = json.loads(sys.stdin.readline())
port, marker, cfg, exit_after = int(sys.argv[1]), sys.argv[2], json.loads(sys.argv[3]), float(sys.argv[4])
assert os.environ.get("DIARY_PARENT") == "rust"
cfg["control_token"], cfg["forward_secret"] = hs["control_token"], hs["parent_secret"]
class H(BaseHTTPRequestHandler):
    def do_GET(self):
        ok = self.headers.get("x-noevia-diary-parent") == hs["parent_secret"]
        body = b"child" if ok else b"no-secret"
        self.send_response(200); self.send_header("content-length", str(len(body))); self.end_headers(); self.wfile.write(body)
    def log_message(self, *a): pass
srv = HTTPServer(("127.0.0.1", port), H)
def term(*_):
    open(marker, "w").write("term"); os._exit(0)
signal.signal(signal.SIGTERM, term)
req = urllib.request.Request("http://127.0.0.1:%d/internal/child/config" % hs["control_port"], data=json.dumps(cfg).encode(),
                             headers={"x-noevia-diary-control": hs["control_token"]}, method="POST")
urllib.request.urlopen(req, timeout=5).read()
if exit_after > 0:
    threading.Timer(exit_after, lambda: os._exit(7)).start()
srv.serve_forever()
"#;

fn spawn_main(dir: &Path, exit_after: f64) -> (std::process::Child, u16, std::path::PathBuf) {
    let (port, control, child) = (free_port(), free_port(), free_port());
    let marker = dir.join("marker");
    let proc = std::process::Command::new(env!("CARGO_BIN_EXE_noevia-diary"))
        .args([
            "--main",
            "--host",
            "127.0.0.1",
            "--port",
            &port.to_string(),
            "--control-port",
            &control.to_string(),
            "--child-port",
            &child.to_string(),
            "--",
            "python3",
            "-c",
            STUB,
            &child.to_string(),
            marker.to_str().unwrap(),
            &child_config(dir, ""),
            &exit_after.to_string(),
        ])
        .spawn()
        .unwrap();
    (proc, port, marker)
}

async fn wait_child(port: u16) -> String {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        if let Ok(r) = reqwest::get(format!("http://127.0.0.1:{port}/anything")).await {
            if r.status() == 200 {
                return r.text().await.unwrap();
            }
        }
        assert!(std::time::Instant::now() < deadline, "child never ready");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

fn wait_exit(p: &mut std::process::Child) -> Option<i32> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        if let Some(s) = p.try_wait().unwrap() {
            return s.code();
        }
        if std::time::Instant::now() > deadline {
            let _ = p.kill();
            panic!("main process did not exit");
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

#[tokio::test]
async fn the_process_passes_sigterm_to_its_child_and_exits_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let (mut p, port, marker) = spawn_main(dir.path(), 0.0);
    assert_eq!(wait_child(port).await, "child");
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(i32::try_from(p.id()).unwrap()),
        nix::sys::signal::Signal::SIGTERM,
    )
    .unwrap();
    assert_eq!(wait_exit(&mut p), Some(0));
    assert_eq!(std::fs::read_to_string(&marker).unwrap(), "term");
}

#[tokio::test]
async fn the_process_exits_non_zero_when_its_child_dies() {
    let dir = tempfile::tempdir().unwrap();
    let (mut p, port, _) = spawn_main(dir.path(), 1.0);
    assert_eq!(wait_child(port).await, "child");
    assert_eq!(wait_exit(&mut p), Some(1));
}
