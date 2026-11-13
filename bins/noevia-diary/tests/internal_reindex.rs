//! `POST /internal/reindex` (R9 indexing step 1) over a synthetic managed tenant with a dirty
//! document and a stub embeddings server: the re-index under the writer role clears the mark and
//! stores the chunks with their vectors; without the writer header, without the start-up config,
//! or with an undrained index outbox, nothing is written. Synthetic data only.
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

async fn call(app: &Arc<noevia_diary::server::App>, writer: bool) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .method("POST")
        .uri(format!("/internal/reindex?tenant={UID}"))
        .header("x-noevia-diary-control", "ctl");
    if writer {
        req = req.header("x-noevia-diary-writer", "1");
    }
    let body = json!({"embedding": {"active": {"base_url": null, "model": "qa-embed-a", "profile": "nomic-v1"},
                                    "next": null, "previous": null, "source": "environment", "version": 0}});
    let resp =
        noevia_diary::server::handle(app.clone(), req.body(Body::from(body.to_string())).unwrap())
            .await;
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 24)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn mark_dirty(root: &Path, document: &str) {
    let c =
        rusqlite::Connection::open(root.join("users").join(UID).join("managed-index.db")).unwrap();
    c.execute(
        "INSERT OR IGNORE INTO dirty_documents (document) VALUES (?)",
        [document],
    )
    .unwrap();
}

fn dirty(root: &Path) -> Vec<String> {
    let c =
        rusqlite::Connection::open(root.join("users").join(UID).join("managed-index.db")).unwrap();
    let mut stmt = c.prepare("SELECT document FROM dirty_documents").unwrap();
    let out: Vec<String> = stmt
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    out
}

fn chunks(root: &Path, file: &str) -> i64 {
    let c =
        rusqlite::Connection::open(root.join("users").join(UID).join("managed-index.db")).unwrap();
    c.query_row("SELECT COUNT(*) FROM chunks WHERE file = ?", [file], |r| {
        r.get(0)
    })
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dirty_documents_are_reindexed_under_the_writer_role() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    build_tenant(root, true);
    mark_dirty(root, ENTRY_PATH);
    let (engine, _embed, seen) = stub_engines().await;
    let off = Arc::new(noevia_diary::server::App::new(
        noevia_diary::config::parse(&config(root, &engine, false)).unwrap(),
    ));
    assert_eq!(call(&off, true).await.0, StatusCode::NOT_FOUND);
    let app = Arc::new(noevia_diary::server::App::new(
        noevia_diary::config::parse(&config(root, &engine, true)).unwrap(),
    ));
    assert_eq!(
        call(&app, false).await.0,
        StatusCode::CONFLICT,
        "only under Python's writer lock"
    );
    assert_eq!(dirty(root), vec![ENTRY_PATH.to_owned()]);
    assert_eq!(chunks(root, ENTRY_PATH), 0);

    let (status, v) = call(&app, true).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(
        v,
        json!({"documents": [{"document": ENTRY_PATH, "embedded": 1, "pending": false, "error": null}]})
    );
    assert!(dirty(root).is_empty(), "the mark is cleared");
    assert_eq!(chunks(root, ENTRY_PATH), 1);
    assert!(seen
        .lock()
        .unwrap()
        .iter()
        .any(|r| r["path"] == json!("/v1/embeddings")));
    // A second call has nothing to do.
    assert_eq!(call(&app, true).await.1, json!({"documents": []}));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_undrained_outbox_leaves_the_index_alone() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    build_tenant(root, true);
    mark_dirty(root, ENTRY_PATH);
    let primary =
        rusqlite::Connection::open(root.join("users").join(UID).join("managed-diary.db")).unwrap();
    primary
        .execute(
            "INSERT INTO workspace_index_outbox (path) VALUES ('AI Memory/notes.md')",
            [],
        )
        .unwrap();
    drop(primary);
    let (engine, _embed, _seen) = stub_engines().await;
    let app = Arc::new(noevia_diary::server::App::new(
        noevia_diary::config::parse(&config(root, &engine, true)).unwrap(),
    ));
    let (status, v) = call(&app, true).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(v["documents"][0]["pending"], json!(true));
    assert!(v["documents"][0]["error"].is_string());
    assert_eq!(dirty(root), vec![ENTRY_PATH.to_owned()], "the mark stays");
    assert_eq!(chunks(root, ENTRY_PATH), 0, "nothing was written");
}

async fn plan(app: &Arc<noevia_diary::server::App>, body: &Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("POST")
        .uri(format!("/internal/reindex?tenant={UID}"))
        .header("x-noevia-diary-control", "ctl")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = noevia_diary::server::handle(app.clone(), req).await;
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 24)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_plan_only_call_returns_the_plans_and_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    build_tenant(root, true);
    mark_dirty(root, ENTRY_PATH);
    let (engine, _embed, seen) = stub_engines().await;
    let app = Arc::new(noevia_diary::server::App::new(
        noevia_diary::config::parse(&config(root, &engine, true)).unwrap(),
    ));
    let live = json!({"active": {"base_url": null, "model": "qa-embed-a", "profile": "nomic-v1"},
                      "next": null, "previous": null, "source": "environment", "version": 0});
    assert_eq!(
        plan(&app, &json!({"embedding": live, "plan_only": true}))
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    // No writer header needed: nothing is written.
    let (status, v) = plan(
        &app,
        &json!({"embedding": live, "plan_only": true, "documents": [ENTRY_PATH, PAST_PATH, "../escape.md"]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{v}");
    let docs = v["documents"].as_array().unwrap();
    assert_eq!(docs.len(), 3);
    // The dirty document: one new chunk, embedded with the live target.
    let new = &docs[0];
    assert_eq!(new["document"], json!(ENTRY_PATH));
    let rows = new["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], json!("2031-03-20"));
    let hash = rows[0][2].as_str().unwrap();
    let embeddings = new["embeddings"].as_object().unwrap();
    assert_eq!(embeddings.keys().collect::<Vec<_>>(), vec![hash]);
    let sent = seen
        .lock()
        .unwrap()
        .iter()
        .filter(|r| r["path"] == json!("/v1/embeddings"))
        .map(|r| r["body"]["input"][0].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(sent.len(), 1, "only the new chunk is embedded");
    assert_eq!(embeddings[hash], json!(fake_vector(&sent[0])));
    assert_eq!(new["pending"], json!(false));
    assert_eq!(
        new["target"],
        json!({"base_url": null, "model": "qa-embed-a", "profile": "nomic-v1"})
    );
    // The indexed document: its row, nothing to embed.
    assert_eq!(docs[1]["rows"].as_array().unwrap().len(), 1);
    assert_eq!(docs[1]["embeddings"], json!({}));
    // An invalid path is that document's error, not the call's.
    assert_eq!(docs[2]["error"], json!("read"));
    assert_eq!(dirty(root), vec![ENTRY_PATH.to_owned()], "the mark stays");
    assert_eq!(chunks(root, ENTRY_PATH), 0, "nothing was written");

    // A tenant that is not ready is declined.
    let dir2 = tempfile::tempdir().unwrap();
    build_tenant(dir2.path(), false);
    let app2 = Arc::new(noevia_diary::server::App::new(
        noevia_diary::config::parse(&config(dir2.path(), &engine, true)).unwrap(),
    ));
    let (status, _) = plan(
        &app2,
        &json!({"embedding": live, "plan_only": true, "documents": [ENTRY_PATH]}),
    )
    .await;
    assert_eq!(status, StatusCode::MISDIRECTED_REQUEST);
}
