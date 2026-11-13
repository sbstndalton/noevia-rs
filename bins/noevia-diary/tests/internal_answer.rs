//! `POST /internal/answer` (R9 answering step 1) over a synthetic managed tenant, with a stub
//! companion engine and a stub embeddings server on 127.0.0.1: the refusals (no config, wrong
//! token, the writer role, a bad tenant, a tenant that is not ready), the context mode, and the
//! plain and streamed answer modes. Nothing here writes the tenant: the test checks the tenant's
//! files and index are unchanged afterwards. Synthetic data only.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use std::path::Path;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, Response, StatusCode};
use diary_retrieval::index::{Embedder, Kind, Options, Retriever};
use diary_retrieval::{NoGuard, Target};
use serde_json::{json, Value};
use sha2::Digest;

const UID: &str = "00000000-0000-4000-8000-0000000000b9";
const DAY: &str = "2031-03-20";
const ENTRY_PATH: &str = "Entries/2031/March/March 20, 2031.md";
const PAST_PATH: &str = "Entries/2031/March/March 4, 2031.md";

fn sha256(data: &[u8]) -> String {
    sha2::Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Bag of hashed words (as the retrieval fixtures' fake embedder), so a query finds its entry.
fn fake_vector(text: &str) -> Vec<f64> {
    let mut v = vec![0.0; 16];
    for token in text
        .to_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| !t.is_empty())
    {
        let h = sha2::Sha256::digest(format!("nomic-v1|{token}").as_bytes());
        v[usize::from(h[0]) % 16] += 1.0 + f64::from(h[1] % 4) * 0.5;
    }
    v[15] += 0.25;
    v
}

struct Fake;

impl Embedder for Fake {
    fn active(&self) -> Target {
        Target::new(None, "qa-embed-a", "nomic-v1")
    }
    fn embed(&self, _t: &Target, texts: &[&str], _k: Kind) -> Result<Vec<Vec<f64>>, String> {
        Ok(texts.iter().map(|t| fake_vector(t)).collect())
    }
}

fn build_tenant(root: &Path, ready: bool) {
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
        ("tenant", UID.to_string()),
        ("active", "1".to_string()),
        ("generation", "1".to_string()),
        (
            "settings",
            json!({"entry_layout": "daily", "index_enabled": false}).to_string(),
        ),
    ] {
        db.execute("INSERT INTO meta VALUES (?,?)", rusqlite::params![k, v])
            .unwrap();
    }
    let today = "## Thursday, March 20, 2031\n\n### 09:00 — Morning\n\n**Me:** Synthetic QA note about the canal walk today.\n";
    let past = "## Tuesday, March 4, 2031\n\n### 18:00 — Kites\n\n**Me:** Flew the teal kite with Qa Orrin Vell at the quay.\n";
    for (path, text) in [
        (ENTRY_PATH, today),
        (PAST_PATH, past),
        ("AI Memory/notes.md", "QA memory: prefers short answers.\n"),
    ] {
        db.execute(
            "INSERT INTO files VALUES (?,?,?,1.0)",
            rusqlite::params![path, text.as_bytes(), sha256(text.as_bytes())],
        )
        .unwrap();
    }
    let idx_path = t.join("managed-index.db");
    let idx = rusqlite::Connection::open(&idx_path).unwrap();
    idx.execute_batch(
        "CREATE TABLE journal (id TEXT PRIMARY KEY, created_at TEXT NOT NULL, kind TEXT NOT NULL, payload TEXT NOT NULL,
             applied INTEGER NOT NULL DEFAULT 0, applied_at TEXT, attempts INTEGER NOT NULL DEFAULT 0, last_error TEXT);",
    )
    .unwrap();
    drop(idx);
    let mut r = Retriever::open(&idx_path, Fake, NoGuard, Options::default()).unwrap();
    r.reindex_file(PAST_PATH, Some(past)).unwrap();
    drop(r);
    if ready {
        std::fs::write(t.join("managed-diary.active"), UID).unwrap();
    }
}

type Seen = Arc<Mutex<Vec<Value>>>;

async fn stub_engines() -> (String, String, Seen) {
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let s = seen.clone();
    let app = axum::Router::new().fallback(move |req: Request<Body>| {
        let seen = s.clone();
        async move {
            let path = req.uri().path().to_owned();
            let bytes = axum::body::to_bytes(req.into_body(), 1 << 22).await.unwrap_or_default();
            let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
            seen.lock().unwrap().push(json!({"path": path, "body": body}));
            if path.ends_with("/embeddings") {
                let data: Vec<Value> = body["input"].as_array().unwrap().iter().enumerate()
                    .map(|(i, t)| json!({"index": i, "embedding": fake_vector(t.as_str().unwrap())})).collect();
                return Response::builder().header("content-type", "application/json")
                    .body(Body::from(json!({"data": data}).to_string())).unwrap();
            }
            let stream = body["stream"].as_bool().unwrap_or(false);
            let lines = [
                json!({"choices": [{"delta": {"content": "The synthetic diary says you flew the teal kite."}}]}),
                json!({"choices": [{"delta": {"content": "\n[LOG: ok]"}, "finish_reason": "stop"}]}),
            ];
            let text: String = lines.iter().map(|l| format!("data: {l}\n\n")).collect::<String>() + "data: [DONE]\n\n";
            let _ = stream;
            Response::builder().header("content-type", "text/event-stream").body(Body::from(text)).unwrap()
        }
    });
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (
        format!("http://{addr}/v1"),
        format!("http://{addr}/v1"),
        seen,
    )
}

fn config(root: &Path, engine: &str, with_answer: bool) -> String {
    let mut cfg = json!({
        "port": 0, "control_token": "ctl", "forward_secret": "fwd", "auth_token": "", "tenant_key": "",
        "data_root": root.to_str().unwrap(), "legacy_user_id": "",
        "corpus": {"entry_layout": "daily", "entries_prefix": "Entries", "index_enabled": false,
                   "month_file_template": "{year}-{month02}.md", "monthly_prefix": "", "index_file": "INDEX.md"},
        "llm": {"main": {"base_url": engine, "chat_model": "qa-chat", "setting_key": "LLM_CHAT_MODEL",
                          "fallback_setting_key": "LLM_CHAT_FALLBACK_MODEL"},
                "aux": {"base_url": engine, "chat_model": "qa-aux", "setting_key": "LLM_AUX_MODEL",
                         "fallback_setting_key": "LLM_AUX_FALLBACK_MODEL"},
                "aux_prefers_main": true, "configured": "qa-chat"}
    });
    if with_answer {
        cfg["answer"] =
            json!({"embedding_seed": {"llm": {"embed_model": "qa-embed-a"}}, "max_retries": 1});
    }
    cfg.to_string()
}

fn request(mode: &str, stream: bool) -> Value {
    json!({
        "mode": mode, "message": "What colour was the kite at the quay?", "turns": [], "day": DAY,
        "extra_context": "", "trace": true, "max_answer_tokens": 1024, "stream": stream,
        "cfg": {"retrieval": {}, "context": {}}, "system_prompt": "QA SYSTEM PROMPT",
        "embedding": live()
    })
}

/// Python's live embedding configuration (`config.get()`) for the seeded target.
fn live() -> Value {
    json!({"active": {"base_url": null, "model": "qa-embed-a", "profile": "nomic-v1"}, "next": null,
           "previous": null, "source": "environment", "version": 0})
}

async fn call(
    app: &Arc<noevia_diary::server::App>,
    query: &str,
    token: &str,
    writer: bool,
    body: &Value,
) -> (StatusCode, Vec<u8>, Option<String>) {
    let mut req = Request::builder()
        .method("POST")
        .uri(format!("/internal/answer{query}"))
        .header("x-noevia-diary-control", token);
    if writer {
        req = req.header("x-noevia-diary-writer", "1");
    }
    let resp =
        noevia_diary::server::handle(app.clone(), req.body(Body::from(body.to_string())).unwrap())
            .await;
    let status = resp.status();
    let ctype = resp
        .headers()
        .get("content-type")
        .map(|v| v.to_str().unwrap().to_owned());
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 24)
        .await
        .unwrap()
        .to_vec();
    (status, bytes, ctype)
}

fn snapshot(root: &Path) -> Vec<(String, String)> {
    let t = root.join("users").join(UID);
    let mut out = Vec::new();
    for db in ["managed-diary.db", "managed-index.db"] {
        let c = rusqlite::Connection::open(t.join(db)).unwrap();
        let mut names: Vec<String> = c
            .prepare("SELECT name FROM sqlite_master WHERE type='table'")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        names.sort();
        for n in names.into_iter().filter(|n| !n.starts_with("vec_items_")) {
            let rows: Vec<String> = c
                .prepare(&format!("SELECT * FROM \"{n}\""))
                .map(|mut s| {
                    let cols = s.column_count();
                    s.query_map([], |r| {
                        Ok((0..cols)
                            .map(|i| {
                                format!("{:?}", r.get::<_, rusqlite::types::Value>(i).unwrap())
                            })
                            .collect::<Vec<_>>()
                            .join("|"))
                    })
                    .unwrap()
                    .map(Result::unwrap)
                    .collect()
                })
                .unwrap_or_default();
            out.push((format!("{db}/{n}"), rows.join("\n")));
        }
    }
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_internal_answer_endpoint() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    build_tenant(root, true);
    let (engine, _embed, seen) = stub_engines().await;
    let before = snapshot(root);
    let tenant = format!("?tenant={UID}");

    // Without the `answer` start-up config the endpoint does not exist.
    let off = Arc::new(noevia_diary::server::App::new(
        noevia_diary::config::parse(&config(root, &engine, false)).unwrap(),
    ));
    assert_eq!(
        call(&off, &tenant, "ctl", false, &request("context", false))
            .await
            .0,
        StatusCode::NOT_FOUND
    );

    let app = Arc::new(noevia_diary::server::App::new(
        noevia_diary::config::parse(&config(root, &engine, true)).unwrap(),
    ));
    assert_eq!(
        call(&app, &tenant, "wrong", false, &request("context", false))
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(&app, &tenant, "ctl", true, &request("context", false))
            .await
            .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        call(
            &app,
            "?tenant=not-a-uuid",
            "ctl",
            false,
            &request("context", false)
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );

    // Context mode: the messages the companion would be sent.
    let (status, bytes, _) = call(&app, &tenant, "ctl", false, &request("context", false)).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&bytes)
    );
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    let messages = v["messages"].as_array().unwrap();
    assert_eq!(
        messages[0],
        json!({"role": "system", "content": "QA SYSTEM PROMPT"})
    );
    let block = messages[1]["content"].as_str().unwrap();
    assert!(
        block.contains("Synthetic QA note about the canal walk today."),
        "today's log"
    );
    assert!(
        block.contains("Flew the teal kite with Qa Orrin Vell"),
        "the retrieved past entry"
    );
    assert!(
        block.contains("--- AI Memory/notes.md ---\nQA memory: prefers short answers."),
        "the memory block"
    );
    assert_eq!(
        messages[2]["content"],
        json!("What colour was the kite at the quay?")
    );
    assert_eq!(v["trace"]["status"], json!("ok"));
    assert!(
        seen.lock()
            .unwrap()
            .iter()
            .any(|r| r["path"] == json!("/v1/embeddings")),
        "the query was embedded"
    );

    // Answer mode, plain and streamed.
    let (status, bytes, _) = call(&app, &tenant, "ctl", false, &request("answer", false)).await;
    assert_eq!(status, StatusCode::OK);
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        v["visible"],
        json!("The synthetic diary says you flew the teal kite.")
    );
    assert_eq!(v["marker"], json!("ok"));
    assert_eq!(v["truncated"], json!(false));
    let (status, bytes, ctype) = call(&app, &tenant, "ctl", false, &request("answer", true)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(ctype.unwrap().starts_with("text/event-stream"));
    let text = String::from_utf8(bytes).unwrap();
    let events: Vec<Value> = text
        .split("\n\n")
        .filter_map(|f| f.strip_prefix("data: "))
        .map(|d| serde_json::from_str(d).unwrap())
        .collect();
    assert_eq!(events.last().unwrap()["type"], json!("answered"));
    assert!(
        events.iter().any(|e| e["type"] == json!("answer")),
        "the model's answer event"
    );

    // Nothing was written.
    assert_eq!(snapshot(root), before);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_or_damaged_embedding_configuration_is_declined_without_any_call() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    build_tenant(root, true);
    let (engine, _embed, seen) = stub_engines().await;
    let app = Arc::new(noevia_diary::server::App::new(
        noevia_diary::config::parse(&config(root, &engine, true)).unwrap(),
    ));
    let tenant = format!("?tenant={UID}");
    // Python refused the seeded address in memory: the profile is "unavailable".
    let mut refused = request("answer", false);
    refused["embedding"]["active"]["profile"] = json!("unavailable");
    assert_eq!(
        call(&app, &tenant, "ctl", false, &refused).await.0.as_u16(),
        421
    );
    let mut damaged = request("answer", false);
    damaged["embedding"]["damaged"] = json!("The saved embedding configuration cannot be read.");
    assert_eq!(
        call(&app, &tenant, "ctl", false, &damaged).await.0.as_u16(),
        421
    );
    assert!(
        seen.lock().unwrap().is_empty(),
        "nothing was sent to any engine"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tenant_without_an_index_is_declined_and_no_file_appears() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    build_tenant(root, true);
    let index = root.join("users").join(UID).join("managed-index.db");
    std::fs::remove_file(&index).unwrap();
    let (engine, _embed, _seen) = stub_engines().await;
    let app = Arc::new(noevia_diary::server::App::new(
        noevia_diary::config::parse(&config(root, &engine, true)).unwrap(),
    ));
    let (status, _, _) = call(
        &app,
        &format!("?tenant={UID}"),
        "ctl",
        false,
        &request("context", false),
    )
    .await;
    assert_eq!(status.as_u16(), 421);
    assert!(!index.exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tenant_that_is_not_ready_is_declined() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    build_tenant(root, false);
    let (engine, _embed, _seen) = stub_engines().await;
    let app = Arc::new(noevia_diary::server::App::new(
        noevia_diary::config::parse(&config(root, &engine, true)).unwrap(),
    ));
    let (status, _, _) = call(
        &app,
        &format!("?tenant={UID}"),
        "ctl",
        false,
        &request("context", false),
    )
    .await;
    assert_eq!(status.as_u16(), 421);
}

/// A chat body the stub writes piece by piece; dropping it (the reader went away) is recorded.
struct Paced {
    rx: tokio::sync::mpsc::UnboundedReceiver<axum::body::Bytes>,
    dropped: Arc<std::sync::atomic::AtomicBool>,
}

impl http_body::Body for Paced {
    type Data = axum::body::Bytes;
    type Error = std::convert::Infallible;
    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        self.rx
            .poll_recv(cx)
            .map(|f| f.map(|b| Ok(http_body::Frame::data(b))))
    }
}

impl Drop for Paced {
    fn drop(&mut self) {
        self.dropped
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

/// An engine whose streamed answer sends its first delta, then waits for `release` (the rest)
/// or, without one, sends a keep-alive line every 50 ms until its reader goes away.
async fn paced_engine(
    release: Option<Arc<tokio::sync::Notify>>,
) -> (String, Arc<std::sync::atomic::AtomicBool>, Arc<std::sync::atomic::AtomicBool>) {
    let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let st = started.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let d = dropped.clone();
    let app = axum::Router::new().fallback(move |req: Request<Body>| {
        let dropped = d.clone();
        let started = st.clone();
        let release = release.clone();
        async move {
            let path = req.uri().path().to_owned();
            let bytes = axum::body::to_bytes(req.into_body(), 1 << 22).await.unwrap_or_default();
            let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
            if path.ends_with("/embeddings") {
                let data: Vec<Value> = body["input"].as_array().unwrap().iter().enumerate()
                    .map(|(i, t)| json!({"index": i, "embedding": fake_vector(t.as_str().unwrap())})).collect();
                return Response::builder().header("content-type", "application/json")
                    .body(Body::from(json!({"data": data}).to_string())).unwrap();
            }
            started.store(true, std::sync::atomic::Ordering::SeqCst);
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            // More than the 128 characters a stream holds back, so a delta is shown at once.
            let first = "The synthetic diary keeps a long quiet note about the teal kite at the quay, "
                .repeat(3);
            let line = |v: Value| axum::body::Bytes::from(format!("data: {v}\n\n"));
            tx.send(line(json!({"choices": [{"delta": {"content": first}}]}))).unwrap();
            tokio::spawn(async move {
                match release {
                    Some(r) => {
                        r.notified().await;
                        let _ = tx.send(line(json!({"choices": [{"delta": {"content": "\n[LOG: ok]"}, "finish_reason": "stop"}]})));
                        let _ = tx.send(axum::body::Bytes::from("data: [DONE]\n\n"));
                    }
                    None => {
                        while tx.send(axum::body::Bytes::from(": waiting\n\n")).is_ok() {
                            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                        }
                    }
                }
            });
            Response::builder()
                .header("content-type", "text/event-stream")
                .body(Body::new(Paced { rx, dropped }))
                .unwrap()
        }
    });
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}/v1"), started, dropped)
}

async fn first_event(body: &mut Body) -> Value {
    use http_body_util::BodyExt;
    let frame = tokio::time::timeout(std::time::Duration::from_secs(20), body.frame())
        .await
        .expect("a first event before the model finished")
        .unwrap()
        .unwrap();
    let text = String::from_utf8(frame.into_data().unwrap().to_vec()).unwrap();
    serde_json::from_str(text.strip_prefix("data: ").unwrap().trim_end()).unwrap()
}

fn streamed_request(app: &Arc<noevia_diary::server::App>) -> impl std::future::Future<Output = Response<Body>> {
    let req = Request::builder()
        .method("POST")
        .uri(format!("/internal/answer?tenant={UID}"))
        .header("x-noevia-diary-control", "ctl")
        .body(Body::from(request("answer", true).to_string()))
        .unwrap();
    noevia_diary::server::handle(app.clone(), req)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_streamed_answer_is_relayed_while_the_model_is_still_answering() {
    let dir = tempfile::tempdir().unwrap();
    build_tenant(dir.path(), true);
    let release = Arc::new(tokio::sync::Notify::new());
    let (engine, _, _) = paced_engine(Some(release.clone())).await;
    let app = Arc::new(noevia_diary::server::App::new(
        noevia_diary::config::parse(&config(dir.path(), &engine, true)).unwrap(),
    ));
    let resp = streamed_request(&app).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let mut body = resp.into_body();
    // The model has not finished (it waits for `release`), yet its first delta has arrived.
    let first = first_event(&mut body).await;
    assert_eq!(first["type"], json!("delta"), "{first}");
    release.notify_one();
    let rest = axum::body::to_bytes(body, 1 << 24).await.unwrap();
    let events: Vec<Value> = String::from_utf8(rest.to_vec())
        .unwrap()
        .split("\n\n")
        .filter_map(|f| f.strip_prefix("data: "))
        .map(|d| serde_json::from_str(d).unwrap())
        .collect();
    let last = events.last().unwrap();
    assert_eq!(last["type"], json!("answered"), "{last}");
    assert_eq!(last["marker"], json!("ok"));
    assert!(last["visible"].as_str().unwrap().starts_with("The synthetic diary keeps"));
    assert_eq!(last["trace"]["status"], json!("ok"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_caller_that_leaves_ends_the_model_call() {
    let dir = tempfile::tempdir().unwrap();
    build_tenant(dir.path(), true);
    let before = snapshot(dir.path());
    let (engine, _, dropped) = paced_engine(None).await;
    let app = Arc::new(noevia_diary::server::App::new(
        noevia_diary::config::parse(&config(dir.path(), &engine, true)).unwrap(),
    ));
    let resp = streamed_request(&app).await;
    let mut body = resp.into_body();
    assert_eq!(first_event(&mut body).await["type"], json!("delta"));
    assert!(!dropped.load(std::sync::atomic::Ordering::SeqCst));
    drop(body);
    // The upstream model call is closed promptly, not left to run to its own deadline.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !dropped.load(std::sync::atomic::Ordering::SeqCst) {
        assert!(std::time::Instant::now() < deadline, "the model call is still open");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    // The plain answer: a caller that gives up drops the handler, which ends the call too.
    let (engine, started, dropped) = paced_engine(None).await;
    let app = Arc::new(noevia_diary::server::App::new(
        noevia_diary::config::parse(&config(dir.path(), &engine, true)).unwrap(),
    ));
    let req = Request::builder()
        .method("POST")
        .uri(format!("/internal/answer?tenant={UID}"))
        .header("x-noevia-diary-control", "ctl")
        .body(Body::from(request("answer", false).to_string()))
        .unwrap();
    let pending = tokio::spawn(noevia_diary::server::handle(app.clone(), req));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while !started.load(std::sync::atomic::Ordering::SeqCst) {
        assert!(std::time::Instant::now() < deadline, "the model was never called");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    pending.abort();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !dropped.load(std::sync::atomic::Ordering::SeqCst) {
        assert!(std::time::Instant::now() < deadline, "the plain model call is still open");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(snapshot(dir.path()), before);
}
