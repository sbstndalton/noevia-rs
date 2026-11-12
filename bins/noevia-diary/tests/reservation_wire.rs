//! DIARY-UNION-SOL1 regressions over the wire: the real server on an ephemeral port, real SQLite,
//! a synthetic managed tenant (no real data). HTTP only, so the same file also runs unchanged
//! against the reviewed base as its before evidence.
//!
//! - SOL1-02: an ordinary SQLite failure whose text equals the settlement refusal's
//!   ("backup settlement uncertain", from a test-only RAISE) keeps its generic reply on every route.
//!   Only the typed refusal at a pending check is BUSY.
//! - SOL1-01: a primary replaced while a write waits for its reservation is refused before any
//!   journal effect (generic 500, never BUSY, nothing marked or transferred). The existing
//!   lifecycle exclusion (the native fence that Python's tenant deletion takes before rmtree)
//!   cannot pass while a write is in flight in its reservation.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

const TOKEN: &str = "synthetic-service-token";
const SECRET: &str = "synthetic-forward-secret";
const UID: &str = "0b5f6c1e-3a2d-4c8e-9f10-1a2b3c4d5e6f";
/// The settlement refusal's message, raised here as an ordinary SQLite failure.
const SAME_TEXT: &str = "backup settlement uncertain";
const INTERNAL: (u16, Option<&str>, Option<&str>, &str) = (
    500,
    None,
    Some("text/plain; charset=utf-8"),
    "Internal Server Error",
);

fn tenant_root(root: &Path) -> PathBuf {
    root.join("users").join(UID)
}

fn sha256(data: &[u8]) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn managed_tenant(root: &Path, files: &[(&str, &str)]) {
    let t = tenant_root(root);
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
        let version = sha256(d.as_bytes());
        db.execute(
            "INSERT INTO files VALUES (?,?,?,1.0)",
            rusqlite::params![p, d.as_bytes(), version],
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

async fn start(root: &Path) -> std::net::SocketAddr {
    let cfg = serde_json::json!({
        "port": 0, "control_token": "ctl", "forward_secret": SECRET, "auth_token": TOKEN, "tenant_key": "k",
        "data_root": root.to_str().unwrap(), "legacy_user_id": "",
        "corpus": {"entry_layout": "daily", "entries_prefix": "Entries", "index_enabled": false,
                   "month_file_template": "{year}-{month02}.md", "monthly_prefix": "", "index_file": "INDEX.md"},
        "llm": {"main": {"base_url": "http://127.0.0.1:9/v1", "chat_model": "chat-a", "setting_key": "LLM_CHAT_MODEL",
                          "fallback_setting_key": "LLM_CHAT_FALLBACK_MODEL"},
                "aux": {"base_url": "http://127.0.0.1:9/v1", "chat_model": "aux-a", "setting_key": "LLM_AUX_MODEL",
                         "fallback_setting_key": "LLM_AUX_FALLBACK_MODEL"},
                "aux_prefers_main": true, "configured": "chat-a"},
        "corpus_write_impl": "rust",
    })
    .to_string();
    let cfg = noevia_diary::config::parse(&cfg).unwrap();
    let app = Arc::new(noevia_diary::server::App::new(cfg));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(noevia_diary::serve(listener, app));
    addr
}

fn writer() -> Vec<(&'static str, String)> {
    vec![
        ("authorization", format!("Bearer {TOKEN}")),
        ("x-cowork-user-id", UID.to_string()),
        ("x-noevia-diary-forward", SECRET.to_string()),
        ("x-noevia-diary-asserted", "1".to_string()),
        ("x-noevia-diary-recovered", "1".to_string()),
        ("x-noevia-diary-writer", "1".to_string()),
        ("content-type", "application/json".to_string()),
    ]
}

/// (status, retry-after, content-type, body) of one request.
async fn send_wire(
    addr: std::net::SocketAddr,
    method: &str,
    target: &str,
    headers: &[(&str, String)],
    body: &serde_json::Value,
) -> (u16, Option<String>, Option<String>, String) {
    let mut req = reqwest::Client::new()
        .request(method.parse().unwrap(), format!("http://{addr}{target}"))
        .body(body.to_string());
    for (k, v) in headers {
        req = req.header(*k, v);
    }
    let resp = req.send().await.unwrap();
    let get = |name: &str| {
        resp.headers()
            .get(name)
            .map(|v| v.to_str().unwrap().to_string())
    };
    let (retry, content_type) = (get("retry-after"), get("content-type"));
    (
        resp.status().as_u16(),
        retry,
        content_type,
        resp.text().await.unwrap(),
    )
}

fn wire_of(
    w: &(u16, Option<String>, Option<String>, String),
) -> (u16, Option<&str>, Option<&str>, &str) {
    (w.0, w.1.as_deref(), w.2.as_deref(), w.3.as_str())
}

fn primary(root: &Path) -> rusqlite::Connection {
    rusqlite::Connection::open(tenant_root(root).join("managed-diary.db")).unwrap()
}

fn index(root: &Path) -> rusqlite::Connection {
    rusqlite::Connection::open(tenant_root(root).join("managed-index.db")).unwrap()
}

fn column(db: &rusqlite::Connection, sql: &str) -> Vec<String> {
    let mut stmt = db.prepare(sql).unwrap();
    stmt.query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

fn outbox(root: &Path) -> Vec<String> {
    column(
        &primary(root),
        "SELECT path FROM workspace_index_outbox ORDER BY path",
    )
}

fn dirty(root: &Path) -> Vec<String> {
    column(
        &index(root),
        "SELECT document FROM dirty_documents ORDER BY document",
    )
}

fn has_file(root: &Path, path: &str) -> bool {
    primary(root)
        .query_row("SELECT 1 FROM files WHERE path=?", [path], |_| Ok(()))
        .is_ok()
}

fn meta(root: &Path, key: &str) -> String {
    primary(root)
        .query_row("SELECT value FROM meta WHERE key=?", [key], |r| r.get(0))
        .unwrap()
}

/// Imported Markdown queued for indexing (not yet marked dirty).
fn queued_import(root: &Path) {
    managed_tenant(
        root,
        &[
            ("Imports/x.md", "# x\n"),
            ("Imports/y.md", "# y\n"),
            ("Imports/n.txt", "n"),
        ],
    );
    primary(root)
        .execute_batch(
            "INSERT INTO workspace_index_outbox VALUES ('Imports/x.md');
             INSERT INTO workspace_index_outbox VALUES ('Imports/y.md');",
        )
        .unwrap();
}

/// A holder of the tenant's primary write reservation on another thread, released on `go`.
fn hold_primary(root: &Path) -> (std::sync::mpsc::Sender<()>, std::thread::JoinHandle<()>) {
    let path = tenant_root(root).join("managed-diary.db");
    let (held_tx, held) = std::sync::mpsc::channel();
    let (go, go_rx) = std::sync::mpsc::channel::<()>();
    let holder = std::thread::spawn(move || {
        let mut c = rusqlite::Connection::open(&path).unwrap();
        let t = c
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .unwrap();
        held_tx.send(()).unwrap();
        go_rx.recv().unwrap();
        t.rollback().unwrap();
    });
    held.recv().unwrap();
    (go, holder)
}

fn now_iso() -> String {
    chrono::Utc::now()
        .format("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string()
}

// ------------------------------------------------------------------ SOL1-02 same-text controls

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_same_text_sqlite_failure_of_a_file_mark_stays_500() {
    let dir = tempfile::tempdir().unwrap();
    managed_tenant(dir.path(), &[]);
    index(dir.path())
        .execute_batch(&format!(
            "CREATE TRIGGER synthetic_same_text BEFORE INSERT ON dirty_documents
             BEGIN SELECT RAISE(ABORT, '{SAME_TEXT}'); END;"
        ))
        .unwrap();
    let addr = start(dir.path()).await;
    let b = serde_json::json!({"path": "a.md", "content": "x", "version": null});
    let w = send_wire(addr, "PUT", "/api/file", &writer(), &b).await;
    assert_eq!(wire_of(&w), INTERNAL);
    assert!(!has_file(dir.path(), "a.md") && dirty(dir.path()).is_empty());
    assert_eq!(meta(dir.path(), "generation"), "1");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_same_text_sqlite_failure_after_the_append_row_commits_stays_500() {
    let dir = tempfile::tempdir().unwrap();
    managed_tenant(dir.path(), &[]);
    let rid = "cccccccc-dddd-4eee-8fff-000000000002";
    // A diary with history (no first-entry scaffold), and a completion step that fails.
    index(dir.path())
        .execute_batch(&format!(
            "INSERT INTO journal (id, created_at, kind, payload, applied) VALUES ('old','2030','exchange','{{}}',1);
             CREATE TRIGGER synthetic_same_text BEFORE UPDATE OF applied ON journal WHEN NEW.applied = 1
             BEGIN SELECT RAISE(ABORT, '{SAME_TEXT}'); END;"
        ))
        .unwrap();
    let addr = start(dir.path()).await;
    let body = serde_json::json!({"text": "Synthetic same-text note.", "title": "t", "requestId": rid,
                                  "entryTime": now_iso()});
    let w = send_wire(addr, "POST", "/api/entries/append", &writer(), &body).await;
    assert_eq!(wire_of(&w), INTERNAL);
    // The row is durably queued and was not marked failed; it is not a BUSY "try again".
    let row: (i64, i64, Option<String>) = index(dir.path())
        .query_row(
            "SELECT applied, attempts, last_error FROM journal WHERE id<>'old'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(row, (0, 0, None));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_same_text_sqlite_failure_of_a_recovery_drain_mark_stays_500() {
    let dir = tempfile::tempdir().unwrap();
    queued_import(dir.path());
    index(dir.path())
        .execute_batch(&format!(
            "CREATE TRIGGER synthetic_same_text BEFORE INSERT ON dirty_documents
             WHEN NEW.document LIKE 'Imports/%'
             BEGIN SELECT RAISE(ABORT, '{SAME_TEXT}'); END;"
        ))
        .unwrap();
    let addr = start(dir.path()).await;
    let b = serde_json::json!({"path": "a.md", "content": "x", "version": null});
    let w = send_wire(addr, "PUT", "/api/file", &writer(), &b).await;
    assert_eq!(wire_of(&w), INTERNAL);
    // The drain's reservation rolled back: nothing transferred, nothing marked, no write.
    assert_eq!(outbox(dir.path()), ["Imports/x.md", "Imports/y.md"]);
    assert!(dirty(dir.path()).is_empty() && !has_file(dir.path(), "a.md"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_same_text_sqlite_failure_of_a_directory_insert_stays_500() {
    let dir = tempfile::tempdir().unwrap();
    managed_tenant(dir.path(), &[]);
    primary(dir.path())
        .execute_batch(&format!(
            "CREATE TRIGGER synthetic_same_text BEFORE INSERT ON directories
             BEGIN SELECT RAISE(ABORT, '{SAME_TEXT}'); END;"
        ))
        .unwrap();
    let addr = start(dir.path()).await;
    let b = serde_json::json!({"path": "D"});
    let w = send_wire(addr, "POST", "/api/directory", &writer(), &b).await;
    assert_eq!(wire_of(&w), INTERNAL);
    let dirs: i64 = primary(dir.path())
        .query_row("SELECT COUNT(*) FROM directories", [], |r| r.get(0))
        .unwrap();
    assert_eq!((dirs, meta(dir.path(), "generation").as_str()), (0, "1"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_same_text_sqlite_failure_of_an_edit_stays_502() {
    let dir = tempfile::tempdir().unwrap();
    let xid = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";
    let day = format!("## Sunday, January 6, 2030\n\n### 09:15 — walk\n\n**Me:** Walked.\n\n**Assistant:** Noted.\n\n<!-- xid:{xid} -->\n");
    managed_tenant(
        dir.path(),
        &[("Entries/2030/January/January 6, 2030.md", &day)],
    );
    let addr = start(dir.path()).await;
    // A stale hash answers the current one (409) without queueing anything.
    let w = send_wire(
        addr,
        "POST",
        "/api/entries/edit",
        &writer(),
        &serde_json::json!({"xid": xid, "me": "Walked far.", "base_hash": "0".repeat(64)}),
    )
    .await;
    assert_eq!(w.0, 409);
    let v: serde_json::Value = serde_json::from_str(&w.3).unwrap();
    let current = v["current_hash"].as_str().unwrap().to_string();
    index(dir.path())
        .execute_batch(&format!(
            "CREATE TRIGGER synthetic_same_text BEFORE INSERT ON journal WHEN NEW.kind = 'exchange_edit'
             BEGIN SELECT RAISE(ABORT, '{SAME_TEXT}'); END;"
        ))
        .unwrap();
    let w = send_wire(
        addr,
        "POST",
        "/api/entries/edit",
        &writer(),
        &serde_json::json!({"xid": xid, "me": "Walked far.", "assistant": "Noted.", "month": "2030-01",
                            "base_hash": current}),
    )
    .await;
    // api_entries_edit's `except Exception`: 502 for every enqueue failure, as before.
    assert_eq!(
        (w.0, w.1.as_deref(), w.3.as_str()),
        (502, None, r#"{"error":"edit failed"}"#)
    );
    let edits: i64 = index(dir.path())
        .query_row(
            "SELECT COUNT(*) FROM journal WHERE kind='exchange_edit'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(edits, 0);
}

// ------------------------------------------------------------------ SOL1-01 replacement + lifecycle

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_primary_replaced_while_the_write_waits_for_its_reservation_is_refused_before_any_journal_effect(
) {
    let dir = tempfile::tempdir().unwrap();
    queued_import(dir.path());
    let path = tenant_root(dir.path()).join("managed-diary.db");
    // A complete, intent-free replacement primary with a different outbox and generation.
    let fresh = tenant_root(dir.path()).join("fresh.db");
    std::fs::copy(&path, &fresh).unwrap();
    rusqlite::Connection::open(&fresh)
        .unwrap()
        .execute_batch(
            "DELETE FROM workspace_index_outbox;
             INSERT INTO workspace_index_outbox VALUES ('Fresh/z.md');
             UPDATE meta SET value='7' WHERE key='generation';",
        )
        .unwrap();
    let addr = start(dir.path()).await;
    let (go, holder) = hold_primary(dir.path());
    let b = serde_json::json!({"path": "a.md", "content": "x", "version": null});
    let request =
        tokio::spawn(async move { send_wire(addr, "PUT", "/api/file", &writer(), &b).await });
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        !request.is_finished(),
        "the write must wait for the held primary"
    );
    // Raw replacement (not a sanctioned lifecycle path): rename over the waited-for primary.
    std::fs::rename(&fresh, &path).unwrap();
    go.send(()).unwrap();
    holder.join().unwrap();
    let w = tokio::time::timeout(Duration::from_secs(10), request)
        .await
        .unwrap()
        .unwrap();
    // Fail closed, never BUSY: no mark, no transfer, no write, and the new file is untouched.
    assert_eq!(wire_of(&w), INTERNAL);
    assert!(dirty(dir.path()).is_empty());
    assert_eq!(outbox(dir.path()), ["Fresh/z.md"]);
    assert!(!has_file(dir.path(), "a.md"));
    assert_eq!(meta(dir.path(), "generation"), "7");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fence_waits_for_a_write_in_flight_in_its_reservation_and_refuses_later_writes() {
    let dir = tempfile::tempdir().unwrap();
    queued_import(dir.path());
    let addr = start(dir.path()).await;
    let (go, holder) = hold_primary(dir.path());
    let b = serde_json::json!({"path": "a.md", "content": "x", "version": null});
    let request =
        tokio::spawn(async move { send_wire(addr, "PUT", "/api/file", &writer(), &b).await });
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!request.is_finished());
    // Python's tenant deletion takes this fence (prepare_delete) before any tombstone or rmtree.
    let fence = tokio::spawn(async move {
        reqwest::Client::new()
            .post(format!("http://{addr}/internal/fence?tenant={UID}"))
            .header("x-noevia-diary-control", "ctl")
            .send()
            .await
            .unwrap()
            .status()
            .as_u16()
    });
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        !fence.is_finished(),
        "the fence must wait for the write in its primary reservation"
    );
    go.send(()).unwrap();
    holder.join().unwrap();
    let w = tokio::time::timeout(Duration::from_secs(10), request)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(w.0, 200, "{}", w.3);
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(10), fence)
            .await
            .unwrap()
            .unwrap(),
        200
    );
    // The reserved primary took every effect of the write before the fence returned.
    assert!(outbox(dir.path()).is_empty() && has_file(dir.path(), "a.md"));
    assert_eq!(dirty(dir.path()), ["Imports/x.md", "Imports/y.md", "a.md"]);
    // Every later write declines (Python keeps it), and nothing is written natively.
    let b = serde_json::json!({"path": "b.md", "content": "y", "version": null});
    let mut req = reqwest::Client::new()
        .put(format!("http://{addr}/api/file"))
        .body(b.to_string());
    for (k, v) in writer() {
        req = req.header(k, v);
    }
    let resp = req.send().await.unwrap();
    assert_eq!(resp.status().as_u16(), 421);
    assert!(resp.headers().get("x-noevia-diary-decline").is_some());
    assert!(!has_file(dir.path(), "b.md"));
}
