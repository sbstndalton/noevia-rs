//! D5 write routes end to end: the real server on an ephemeral port over a synthetic managed
//! tenant (no real data). Byte parity with Python is proven by the services differential tests
//! (diary/tests/test_rust_forward_d5.py); these pin the storage effects, the single-writer lock,
//! the fence and every decline.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

const TOKEN: &str = "synthetic-service-token";
const SECRET: &str = "synthetic-forward-secret";
const UID: &str = "0b5f6c1e-3a2d-4c8e-9f10-1a2b3c4d5e6f";

fn tenant_root(root: &Path) -> PathBuf {
    root.join("users").join(UID)
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

fn sha256(data: &[u8]) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

async fn start(root: &Path, corpus_write_impl: &str) -> std::net::SocketAddr {
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
        "corpus_write_impl": corpus_write_impl,
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

fn without(mut h: Vec<(&'static str, String)>, name: &str) -> Vec<(&'static str, String)> {
    h.retain(|(k, _)| *k != name);
    h
}

async fn send(
    addr: std::net::SocketAddr,
    method: &str,
    target: &str,
    headers: &[(&str, String)],
    body: &serde_json::Value,
) -> (u16, bool, String) {
    let mut req = reqwest::Client::new()
        .request(method.parse().unwrap(), format!("http://{addr}{target}"))
        .body(body.to_string());
    for (k, v) in headers {
        req = req.header(*k, v);
    }
    let resp = req.send().await.unwrap();
    let status = resp.status().as_u16();
    let declined = resp.headers().get("x-noevia-diary-decline").is_some();
    (status, declined, resp.text().await.unwrap())
}

async fn send_raw(
    addr: std::net::SocketAddr,
    method: &str,
    target: &str,
    headers: &[(&str, String)],
    body: &'static [u8],
) -> (u16, bool, String) {
    let mut req = reqwest::Client::new()
        .request(method.parse().unwrap(), format!("http://{addr}{target}"))
        .body(body);
    for (k, v) in headers {
        req = req.header(*k, v);
    }
    let resp = req.send().await.unwrap();
    let status = resp.status().as_u16();
    let declined = resp.headers().get("x-noevia-diary-decline").is_some();
    (status, declined, resp.text().await.unwrap())
}

fn file_row(root: &Path, path: &str) -> Option<(Vec<u8>, String)> {
    let db = rusqlite::Connection::open(tenant_root(root).join("managed-diary.db")).unwrap();
    db.query_row(
        "SELECT data, version FROM files WHERE path=?",
        [path],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .ok()
}

fn meta(root: &Path, key: &str) -> String {
    let db = rusqlite::Connection::open(tenant_root(root).join("managed-diary.db")).unwrap();
    db.query_row("SELECT value FROM meta WHERE key=?", [key], |r| r.get(0))
        .unwrap()
}

fn index(root: &Path) -> rusqlite::Connection {
    rusqlite::Connection::open(tenant_root(root).join("managed-index.db")).unwrap()
}

fn dirty(root: &Path) -> Vec<String> {
    let idx = index(root);
    let mut stmt = idx
        .prepare("SELECT document FROM dirty_documents ORDER BY document")
        .unwrap();
    stmt.query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

fn now_iso() -> String {
    chrono::Utc::now()
        .format("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string()
}

#[tokio::test]
async fn put_file_creates_replaces_and_refuses_like_python() {
    let dir = tempfile::tempdir().unwrap();
    managed_tenant(dir.path(), &[("AI Memory/memory.md", "old")]);
    let addr = start(dir.path(), "python").await;
    let old = sha256(b"old");

    // Stale base version: 409, nothing written.
    let (s, d, body) = send(addr, "PUT", "/api/file", &writer(),
        &serde_json::json!({"path": "AI Memory/memory.md", "content": "new", "version": "0".repeat(64)})).await;
    assert_eq!((s, d), (409, false));
    assert_eq!(
        body,
        r#"{"detail":"File changed elsewhere. Reopen it before saving; your draft has been kept."}"#
    );
    assert_eq!(
        file_row(dir.path(), "AI Memory/memory.md").unwrap().0,
        b"old"
    );

    // Replace.
    let (s, _, body) = send(
        addr,
        "PUT",
        "/api/file",
        &writer(),
        &serde_json::json!({"path": "AI Memory/memory.md", "content": "new é", "version": old}),
    )
    .await;
    assert_eq!(s, 200);
    let v = sha256("new é".as_bytes());
    assert_eq!(
        body,
        format!(r#"{{"path":"AI Memory/memory.md","content":"new é","version":"{v}"}}"#)
    );
    let (data, version) = file_row(dir.path(), "AI Memory/memory.md").unwrap();
    assert_eq!((data, version), ("new é".as_bytes().to_vec(), v.clone()));
    assert_eq!(meta(dir.path(), "generation"), "2");
    let due: f64 = meta(dir.path(), "due").parse().unwrap();
    let snap: f64 = meta(dir.path(), "snapshot_time").parse().unwrap();
    assert!((due - snap - 3.0).abs() < 1e-6);
    assert_eq!(dirty(dir.path()), vec!["AI Memory/memory.md".to_string()]);

    // Unchanged content: no write at all.
    let (s, _, _) = send(
        addr,
        "PUT",
        "/api/file",
        &writer(),
        &serde_json::json!({"path": "AI Memory/memory.md", "content": "new é", "version": v}),
    )
    .await;
    assert_eq!(s, 200);
    assert_eq!(meta(dir.path(), "generation"), "2");

    // Create (version null), then a file under a file is a path conflict and leaves no dirty mark.
    let (s, _, _) = send(
        addr,
        "PUT",
        "/api/file",
        &writer(),
        &serde_json::json!({"path": "Notes/a.md", "content": "x", "version": null}),
    )
    .await;
    assert_eq!(s, 200);
    let (s, _, body) = send(
        addr,
        "PUT",
        "/api/file",
        &writer(),
        &serde_json::json!({"path": "Notes/a.md/b.md", "content": "x", "version": null}),
    )
    .await;
    assert_eq!(s, 409);
    assert_eq!(
        body,
        r#"{"detail":"A directory or file already occupies this path; your draft has been kept"}"#
    );
    assert!(!dirty(dir.path()).contains(&"Notes/a.md/b.md".to_string()));

    // Validation: Python's messages.
    for (b, expected) in [
        (
            serde_json::json!({"path": 5, "content": "x", "version": null}),
            r#"{"detail":"Invalid path"}"#,
        ),
        (
            serde_json::json!({"path": "a.txt", "content": "x", "version": null}),
            r#"{"detail":"Only Markdown files are supported"}"#,
        ),
        (
            serde_json::json!({"path": ".x/a.md", "content": "x", "version": null}),
            r#"{"detail":"Choose a path inside the diary folder"}"#,
        ),
        (
            serde_json::json!({"path": "a.md", "content": "x"}),
            r#"{"detail":"Content and base version required; maximum 512 KiB"}"#,
        ),
        (
            serde_json::json!({"path": "a.md", "content": "x".repeat(512 * 1024 + 1), "version": null}),
            r#"{"detail":"Content and base version required; maximum 512 KiB"}"#,
        ),
    ] {
        let (s, d, body) = send(addr, "PUT", "/api/file", &writer(), &b).await;
        assert_eq!((s, d, body.as_str()), (400, false, expected));
    }
    // Not a JSON object: FastAPI's 422 stays with Python.
    let (s, d, _) = send(addr, "PUT", "/api/file", &writer(), &serde_json::json!([1])).await;
    assert_eq!((s, d), (421, true));
}

#[tokio::test]
async fn directory_create_matches_python() {
    let dir = tempfile::tempdir().unwrap();
    managed_tenant(dir.path(), &[("Notes/a.md", "x")]);
    let addr = start(dir.path(), "python").await;
    let cases = [
        (
            serde_json::json!({"path": "Projects"}),
            200,
            r#"{"path":"Projects","isDir":true}"#,
        ),
        (
            serde_json::json!({"path": "Projects/Sub"}),
            200,
            r#"{"path":"Projects/Sub","isDir":true}"#,
        ),
        (
            serde_json::json!({"path": "Projects"}),
            405,
            r#"{"detail":"A file or directory already exists at this path"}"#,
        ),
        (
            serde_json::json!({"path": "Notes"}),
            405,
            r#"{"detail":"A file or directory already exists at this path"}"#,
        ),
        (
            serde_json::json!({"path": "Missing/Sub"}),
            409,
            r#"{"detail":"Parent directory does not exist"}"#,
        ),
        (
            serde_json::json!({"path": "Notes/a.md/x"}),
            409,
            r#"{"detail":"Parent directory does not exist"}"#,
        ),
        (
            serde_json::json!({}),
            405,
            r#"{"detail":"The diary root already exists"}"#,
        ),
        (
            serde_json::json!({"path": 1}),
            400,
            r#"{"detail":"Invalid path"}"#,
        ),
    ];
    for (b, status, expected) in cases {
        let (s, _, body) = send(addr, "POST", "/api/directory", &writer(), &b).await;
        assert_eq!((s, body.as_str()), (status, expected), "{b}");
    }
    assert_eq!(meta(dir.path(), "generation"), "3");
}

#[tokio::test]
async fn append_journals_then_writes_once_and_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    managed_tenant(dir.path(), &[]);
    let addr = start(dir.path(), "rust").await;
    let rid = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";
    let b = serde_json::json!({"text": "Walked by the river.", "title": "walk", "requestId": rid, "entryTime": now_iso()});
    let (s, d, body) = send(addr, "POST", "/api/entries/append", &writer(), &b).await;
    assert_eq!((s, d), (200, false), "{body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["queued"], false);
    assert_eq!(v["xid"], rid);
    let document = v["document"].as_str().unwrap().to_string();
    let (text, _) = file_row(dir.path(), &document).unwrap();
    let text = String::from_utf8(text).unwrap();
    assert!(text.contains(&format!("<!-- xid:{rid} -->")));
    assert!(text.contains("**Me:** Walked by the river."));
    // First entry of a fresh diary: the scaffold seeds were created too.
    for seed in [
        "Entries/README.md",
        "AI Memory/README.md",
        "Raw Sources/README.md",
    ] {
        assert!(file_row(dir.path(), seed).is_some(), "{seed}");
    }
    // Journal: one applied exchange row with Python's payload form.
    let idx = index(dir.path());
    let (kind, payload, applied): (String, String, i64) = idx
        .query_row("SELECT kind, payload, applied FROM journal", [], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .unwrap();
    assert_eq!((kind.as_str(), applied), ("exchange", 1));
    assert!(
        payload.starts_with(r#"{"scaffold": true, "xid": ""#),
        "{payload}"
    );
    assert_eq!(dirty(dir.path()), vec![document.clone()]);

    // Same requestId again: same xid, nothing written.
    let generation = meta(dir.path(), "generation");
    let (s, _, body2) = send(addr, "POST", "/api/entries/append", &writer(), &b).await;
    assert_eq!((s, body2), (200, body));
    assert_eq!(meta(dir.path(), "generation"), generation);
    let rows: i64 = idx
        .query_row("SELECT COUNT(*) FROM journal", [], |r| r.get(0))
        .unwrap();
    assert_eq!(rows, 1);
}

#[tokio::test]
async fn append_invalidates_retrieval_before_it_writes() {
    // The process can be lost at any point after the corpus commit (before the journal row is
    // marked applied, before the reply). The retrieval invalidation must already be persisted:
    // here the corpus write itself fails at commit, and the document is still marked dirty.
    let dir = tempfile::tempdir().unwrap();
    managed_tenant(dir.path(), &[]);
    {
        let db =
            rusqlite::Connection::open(tenant_root(dir.path()).join("managed-diary.db")).unwrap();
        db.execute_batch(
            "CREATE TRIGGER synthetic_commit_failure BEFORE INSERT ON files \
             BEGIN SELECT RAISE(ABORT, 'synthetic commit failure'); END;",
        )
        .unwrap();
    }
    let addr = start(dir.path(), "rust").await;
    let rid = "aaaaaaaa-bbbb-4ccc-8ddd-ffffffffffff";
    let b = serde_json::json!({"text": "Synthetic note.", "title": "walk", "requestId": rid, "entryTime": now_iso()});
    let (s, _, body) = send(addr, "POST", "/api/entries/append", &writer(), &b).await;
    assert_ne!(s, 200, "{body}");
    let marked = dirty(dir.path());
    assert_eq!(marked.len(), 1, "{marked:?}");
    assert!(
        marked[0].starts_with("Entries/") && marked[0].ends_with(".md"),
        "{marked:?}"
    );
    assert!(
        file_row(dir.path(), &marked[0]).is_none(),
        "nothing was written"
    );
}

#[tokio::test]
async fn append_validation_and_impl_gate() {
    let dir = tempfile::tempdir().unwrap();
    managed_tenant(dir.path(), &[]);
    let addr = start(dir.path(), "rust").await;
    let rid = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";
    let cases = [
        (
            serde_json::json!({"text": " ", "requestId": rid}),
            400,
            r#"{"error":"text is required (at most 8000 characters)"}"#,
        ),
        (
            serde_json::json!({"text": "x", "title": "a#b", "requestId": rid}),
            400,
            r#"{"error":"title must be one line of at most 80 characters"}"#,
        ),
        (
            serde_json::json!({"text": "x", "requestId": "AAAAAAAA-bbbb-4ccc-8ddd-eeeeeeeeeeee"}),
            400,
            r#"{"error":"requestId must be a UUIDv4"}"#,
        ),
        (
            serde_json::json!({"text": "x", "requestId": rid, "entryDay": "2030-01-01"}),
            400,
            r#"{"error":"appends always go to today's entry"}"#,
        ),
        (
            serde_json::json!({"text": "a <!--xid:abc-->", "requestId": rid}),
            400,
            r#"{"error":"text must not contain diary markers or Markdown headings"}"#,
        ),
        (
            serde_json::json!({"text": "a\n # b", "requestId": rid}),
            400,
            r#"{"error":"text must not contain diary markers or Markdown headings"}"#,
        ),
        (
            serde_json::json!({"text": "x", "requestId": rid, "entryTime": "2030-01-06T09:00:00"}),
            400,
            r#"{"detail":"Invalid diary date/time; include a timezone offset"}"#,
        ),
        (
            serde_json::json!({"text": "x", "requestId": rid, "entryTime": 5}),
            400,
            r#"{"detail":"Invalid diary date/time; include a timezone offset"}"#,
        ),
        (
            serde_json::json!({"text": "x", "requestId": rid, "entryTime": "2020-01-06T09:00:00Z"}),
            400,
            r#"{"error":"entryTime must be the current time"}"#,
        ),
    ];
    for (b, status, expected) in cases {
        let (s, _, body) = send(addr, "POST", "/api/entries/append", &writer(), &b).await;
        assert_eq!((s, body.as_str()), (status, expected), "{b}");
    }
    // A form Python's fromisoformat might read differently: Python serves it.
    let (s, d, _) = send(
        addr,
        "POST",
        "/api/entries/append",
        &writer(),
        &serde_json::json!({"text": "x", "requestId": rid, "entryTime": "20300106T0900+0000"}),
    )
    .await;
    assert_eq!((s, d), (421, true));
    assert!(file_row(dir.path(), "Entries/README.md").is_none());

    // DIARY_CORPUS_WRITE_IMPL=python: Python's own transforms write the text, so it serves.
    let dir2 = tempfile::tempdir().unwrap();
    managed_tenant(dir2.path(), &[]);
    let addr2 = start(dir2.path(), "python").await;
    let (s, d, _) = send(
        addr2,
        "POST",
        "/api/entries/append",
        &writer(),
        &serde_json::json!({"text": "x", "requestId": rid, "entryTime": now_iso()}),
    )
    .await;
    assert_eq!((s, d), (421, true));
}

#[tokio::test]
async fn edit_rewrites_one_exchange_with_conflict_checks() {
    let dir = tempfile::tempdir().unwrap();
    let xid = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";
    let day = format!("## Sunday, January 6, 2030\n\n### 09:15 — walk\n\n**Me:** Walked.\n\n**Assistant:** Noted.\n\n<!-- xid:{xid} -->\n");
    managed_tenant(
        dir.path(),
        &[("Entries/2030/January/January 6, 2030.md", &day)],
    );
    let addr = start(dir.path(), "rust").await;
    let stale = "0".repeat(64);
    let (s, _, body) = send(
        addr,
        "POST",
        "/api/entries/edit",
        &writer(),
        &serde_json::json!({"xid": xid, "me": "Walked far.", "base_hash": stale}),
    )
    .await;
    assert_eq!(s, 409);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["conflict"], true);
    let current = v["current_hash"].as_str().unwrap().to_string();

    let (s, _, body) = send(addr, "POST", "/api/entries/edit", &writer(),
        &serde_json::json!({"xid": xid, "me": "Walked far.", "assistant": "Noted.", "month": "2030-01", "base_hash": current})).await;
    assert_eq!(s, 200, "{body}");
    assert!(body.starts_with(r#"{"ok":true,"document":"Entries/2030/January/January 6, 2030.md","day":"2030-01-06","hash":""#));
    let (text, _) = file_row(dir.path(), "Entries/2030/January/January 6, 2030.md").unwrap();
    assert!(String::from_utf8(text)
        .unwrap()
        .contains("**Me:** Walked far."));
    let payload: String = index(dir.path())
        .query_row(
            "SELECT payload FROM journal WHERE kind='exchange_edit'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        payload,
        format!(
            r#"{{"xid": "{xid}", "new_me": "Walked far.", "new_claude": "Noted.", "month": "2030-01"}}"#
        )
    );

    for (b, status, expected) in [
        (
            serde_json::json!({"xid": "zz", "me": "x"}),
            400,
            r#"{"error":"invalid xid"}"#,
        ),
        (
            serde_json::json!({"xid": xid, "me": " "}),
            400,
            r#"{"error":"me is required"}"#,
        ),
        (
            serde_json::json!({"xid": xid, "me": "x", "month": "2030-13"}),
            400,
            r#"{"error":"month must be YYYY-MM"}"#,
        ),
        (
            serde_json::json!({"xid": xid, "me": "x", "base_hash": "abc"}),
            400,
            r#"{"error":"invalid base_hash"}"#,
        ),
        (
            serde_json::json!({"xid": "bbbbbbbb-bbbb-4ccc-8ddd-eeeeeeeeeeee", "me": "x"}),
            404,
            r#"{"error":"no corpus document contains exchange bbbbbbbb-bbbb-4ccc-8ddd-eeeeeeeeeeee"}"#,
        ),
    ] {
        let (s, _, body) = send(addr, "POST", "/api/entries/edit", &writer(), &b).await;
        assert_eq!((s, body.as_str()), (status, expected), "{b}");
    }
    // Pydantic would refuse a non-string field (422): Python serves.
    let (s, d, _) = send(
        addr,
        "POST",
        "/api/entries/edit",
        &writer(),
        &serde_json::json!({"xid": xid, "me": 5}),
    )
    .await;
    assert_eq!((s, d), (421, true));
}

#[tokio::test]
async fn declines_without_writer_role_but_recovers_pending_work() {
    let dir = tempfile::tempdir().unwrap();
    managed_tenant(dir.path(), &[]);
    let addr = start(dir.path(), "rust").await;
    let b = serde_json::json!({"path": "a.md", "content": "x", "version": null});
    for h in [
        without(writer(), "x-noevia-diary-writer"),
        without(writer(), "x-cowork-user-id"),
    ] {
        let (s, d, _) = send(addr, "PUT", "/api/file", &h, &b).await;
        assert_eq!((s, d), (421, true));
    }
    // Pending journal entry: Python replays it first.
    index(dir.path())
        .execute(
            "INSERT INTO journal (id, created_at, kind, payload) VALUES ('j','t','exchange','{}')",
            [],
        )
        .unwrap();
    let (s, d, _) = send(addr, "PUT", "/api/file", &writer(), &b).await;
    assert_eq!((s, d), (200, false));
    rusqlite::Connection::open(tenant_root(dir.path()).join("managed-diary.db"))
        .unwrap()
        .execute("DELETE FROM files WHERE path='a.md'", [])
        .unwrap();
    index(dir.path())
        .execute("UPDATE journal SET applied=1", [])
        .unwrap();
    // Native replay quarantines the malformed entry and drains the index outbox.
    let db = rusqlite::Connection::open(tenant_root(dir.path()).join("managed-diary.db")).unwrap();
    db.execute("INSERT INTO workspace_index_outbox VALUES ('x.md')", [])
        .unwrap();
    let (s, d, _) = send(addr, "PUT", "/api/file", &writer(), &b).await;
    assert_eq!((s, d), (200, false));
    db.execute("DELETE FROM files WHERE path='a.md'", [])
        .unwrap();
    db.execute("DELETE FROM workspace_index_outbox", [])
        .unwrap();
    // Dirty documents alone do not stop a write (the index only lags).
    index(dir.path())
        .execute("INSERT INTO dirty_documents VALUES ('old.md')", [])
        .unwrap();
    let (s, d, _) = send(addr, "PUT", "/api/file", &writer(), &b).await;
    assert_eq!((s, d), (200, false));
    assert!(file_row(dir.path(), "a.md").is_some());
}

#[tokio::test]
async fn fence_waits_for_the_writer_and_refuses_later_writes() {
    let dir = tempfile::tempdir().unwrap();
    managed_tenant(dir.path(), &[]);
    let addr = start(dir.path(), "rust").await;
    let client = reqwest::Client::new();
    let r = client
        .post(format!("http://{addr}/internal/fence?tenant={UID}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 403);
    let r = client
        .post(format!("http://{addr}/internal/fence?tenant={UID}"))
        .header("x-noevia-diary-control", "ctl")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    let (s, d, _) = send(
        addr,
        "PUT",
        "/api/file",
        &writer(),
        &serde_json::json!({"path": "a.md", "content": "x", "version": null}),
    )
    .await;
    assert_eq!((s, d), (421, true));
    assert!(file_row(dir.path(), "a.md").is_none());
}

#[tokio::test]
async fn concurrent_appends_on_one_tenant_keep_journal_order() {
    let dir = tempfile::tempdir().unwrap();
    managed_tenant(dir.path(), &[]);
    let addr = start(dir.path(), "rust").await;
    let ids: Vec<String> = (0..12)
        .map(|i| format!("aaaaaaaa-bbbb-4ccc-8ddd-{i:012x}"))
        .collect();
    let mut tasks = Vec::new();
    for id in ids.clone() {
        tasks.push(tokio::spawn(async move {
            let b = serde_json::json!({"text": format!("note {id}"), "requestId": id, "entryTime": now_iso()});
            send(addr, "POST", "/api/entries/append", &writer(), &b).await
        }));
    }
    let mut document = String::new();
    for t in tasks {
        let (s, d, body) = t.await.unwrap();
        assert_eq!((s, d), (200, false), "{body}");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        document = v["document"].as_str().unwrap().to_string();
    }
    let text = String::from_utf8(file_row(dir.path(), &document).unwrap().0).unwrap();
    // Every note exactly once, in journal (rowid) order.
    let idx = index(dir.path());
    let mut stmt = idx
        .prepare("SELECT payload FROM journal WHERE kind='exchange' ORDER BY rowid")
        .unwrap();
    let order: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|p| {
            let v: serde_json::Value = serde_json::from_str(&p.unwrap()).unwrap();
            v["xid"].as_str().unwrap().to_string()
        })
        .collect();
    assert_eq!(order.len(), ids.len());
    let mut last = 0;
    for id in &order {
        let marker = format!("<!-- xid:{id} -->");
        assert_eq!(text.matches(&marker).count(), 1);
        let at = text.find(&marker).unwrap();
        assert!(at > last, "corpus order follows journal order");
        last = at;
    }
    let pending: i64 = idx
        .query_row("SELECT COUNT(*) FROM journal WHERE applied=0", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(pending, 0);
}

/// FastAPI validates the handler's body parameter (422) before the handler's `check_auth` (401):
/// with a bad or missing token, a body Python refuses with 422 must decline (Python answers),
/// never get Rust's 401. Semantic refusals come after auth, so they still get the 401.
#[tokio::test]
async fn body_shape_refusals_precede_auth_on_every_write_route() {
    let dir = tempfile::tempdir().unwrap();
    managed_tenant(dir.path(), &[]);
    let addr = start(dir.path(), "rust").await;
    let routes = [
        ("PUT", "/api/file"),
        ("POST", "/api/directory"),
        ("POST", "/api/entries/append"),
        ("POST", "/api/entries/edit"),
    ];
    let shape: [&'static [u8]; 5] = [b"{\"path\":", b"[1]", b"\"text\"", b"", b"{\"x\":NaN}"];
    let edit_model: [&'static [u8]; 4] = [
        br#"{"me":"x"}"#,
        br#"{"xid":5,"me":"x"}"#,
        br#"{"xid":"0123abcd","me":"x","month":3}"#,
        br#"{"xid":"0123abcd","me":"x","assistant":null}"#,
    ];
    let tokens = [
        without(writer(), "authorization"),
        without(writer(), "authorization")
            .into_iter()
            .chain([("authorization", "Bearer wrong-token".to_string())])
            .collect::<Vec<_>>(),
    ];
    for headers in &tokens {
        for (method, target) in routes {
            let bodies: Vec<&'static [u8]> = if target == "/api/entries/edit" {
                shape.iter().chain(edit_model.iter()).copied().collect()
            } else {
                shape.to_vec()
            };
            for body in bodies {
                let (status, declined, _) = send_raw(addr, method, target, headers, body).await;
                assert_eq!(
                    (status, declined),
                    (421, true),
                    "{method} {target} {:?}",
                    String::from_utf8_lossy(body)
                );
            }
            // A body the model accepts reaches check_auth.
            let valid: &'static [u8] = match target {
                "/api/entries/edit" => br#"{"xid":"zz","me":"x"}"#,
                _ => br#"{"path":"a.md","text":"x"}"#,
            };
            let (status, declined, _) = send_raw(addr, method, target, headers, valid).await;
            assert_eq!(
                (status, declined),
                (401, false),
                "{method} {target} valid shape"
            );
        }
    }
    assert!(file_row(dir.path(), "a.md").is_none());
}

#[tokio::test]
async fn native_replay_without_handshake_quarantines_dependents_and_preserves_row_order() {
    let dir = tempfile::tempdir().unwrap();
    managed_tenant(dir.path(), &[]);
    let idx = index(dir.path());
    for (id, kind, payload) in [
        ("bad", "unknown", serde_json::json!({"xid":"bad-xid"})),
        (
            "dependent",
            "exchange_edit",
            serde_json::json!({"xid":"bad-xid"}),
        ),
        (
            "first",
            "exchange",
            serde_json::json!({"day":"2030-01-06","xid":"abcdef12","sub_header":"12:00 | Synthetic","body":diary_corpus_write::render_exchange("first", "", "abcdef12")}),
        ),
        (
            "second",
            "exchange_edit",
            serde_json::json!({"xid":"abcdef12","new_me":"newest","new_claude":""}),
        ),
    ] {
        idx.execute(
            "INSERT INTO journal (id,created_at,kind,payload) VALUES (?,'reverse-clock',?,?)",
            rusqlite::params![id, kind, payload.to_string()],
        )
        .unwrap();
    }
    let addr = start(dir.path(), "rust").await;
    let body = serde_json::json!({"path":"incoming.md","content":"incoming","version":null});
    let (s, d, b) = send(
        addr,
        "PUT",
        "/api/file",
        &without(writer(), "x-noevia-diary-recovered"),
        &body,
    )
    .await;
    assert_eq!((s, d), (200, false), "{b}");
    let errors: Vec<(String,i64,String)> = idx.prepare("SELECT id,attempts,last_error FROM journal WHERE last_error IS NOT NULL ORDER BY rowid").unwrap().query_map([],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap().map(Result::unwrap).collect();
    assert_eq!(
        errors,
        vec![
            (
                "bad".into(),
                1,
                "quarantined: unknown journal kind: unknown".into()
            ),
            (
                "dependent".into(),
                1,
                "quarantined: depends on quarantined bad".into()
            )
        ]
    );
    let data = file_row(dir.path(), "Entries/2030/January/06 Sunday.md");
    // Discover the configured daily name rather than assuming a filename presentation.
    let db = rusqlite::Connection::open(tenant_root(dir.path()).join("managed-diary.db")).unwrap();
    let text: Vec<u8> = db
        .query_row(
            "SELECT data FROM files WHERE path LIKE 'Entries/%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let text = String::from_utf8(text).unwrap();
    assert!(text.contains("newest"), "{text}");
    assert_eq!(text.matches("<!-- xid:abcdef12 -->").count(), 1);
    drop(data);
    // A simulated crash after corpus commit but before mark_applied replays idempotently.
    idx.execute("UPDATE journal SET applied=0 WHERE id='second'", [])
        .unwrap();
    let generation = meta(dir.path(), "generation");
    let (s,d,b) = send(addr,"PUT","/api/file", &writer(),&serde_json::json!({"path":"incoming.md","content":"incoming","version":sha256(b"incoming")})).await;
    assert_eq!((s, d), (200, false), "{b}");
    assert_eq!(meta(dir.path(), "generation"), generation);
}

#[tokio::test]
async fn replay_outage_never_quarantines_and_retries_observe_cooldown() {
    let dir = tempfile::tempdir().unwrap();
    managed_tenant(dir.path(), &[]);
    let db = rusqlite::Connection::open(tenant_root(dir.path()).join("managed-diary.db")).unwrap();
    db.execute(
        "INSERT INTO directories VALUES ('Entries/2030/January/January 6, 2030.md')",
        [],
    )
    .unwrap();
    let idx = index(dir.path());
    let payload = serde_json::json!({"day":"2030-01-06","xid":"abcdef12","sub_header":"12:00 | Synthetic","body":"Synthetic"});
    idx.execute("INSERT INTO journal(id,created_at,kind,payload,attempts) VALUES ('outage','t','exchange',?,30)",[payload.to_string()]).unwrap();
    let addr = start(dir.path(), "rust").await;
    for n in 0..2 {
        let (s, d, b) = send(
            addr,
            "POST",
            "/api/directory",
            &writer(),
            &serde_json::json!({"path":format!("Synthetic {n}")}),
        )
        .await;
        assert_eq!((s, d), (200, false), "{b}");
    }
    let row: (i64, i64, String) = idx
        .query_row(
            "SELECT applied,attempts,last_error FROM journal WHERE id='outage'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        row,
        (0, 31, "Entries/2030/January/January 6, 2030.md".into())
    );
}

/// Y0-1/2/3: the reference date() / fromisoformat() refuse year 0, so a year-zero hint is left
/// with the reference applier before any native effect, while neighbouring valid years stay native.
#[tokio::test]
async fn year_zero_hints_decline_before_effects_and_adjacent_years_stay_native() {
    let dir = tempfile::tempdir().unwrap();
    managed_tenant(dir.path(), &[]);
    let addr = start(dir.path(), "rust").await;
    let xid = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";
    // Incoming edit (Y0-2): "0000-01" is declined (reference answers 502); the shape rule still
    // refuses "0000-13" itself, and years 1 and 9999 are decided natively (no such document).
    let (s, d, _) = send(
        addr,
        "POST",
        "/api/entries/edit",
        &writer(),
        &serde_json::json!({"xid": xid, "me": "x", "month": "0000-01"}),
    )
    .await;
    assert_eq!((s, d), (421, true));
    let not_found = format!(r#"{{"error":"no corpus document contains exchange {xid}"}}"#);
    for (month, status, expected) in [
        ("0000-13", 400, r#"{"error":"month must be YYYY-MM"}"#),
        ("0001-01", 404, not_found.as_str()),
        ("9999-12", 404, not_found.as_str()),
    ] {
        let (s, _, body) = send(
            addr,
            "POST",
            "/api/entries/edit",
            &writer(),
            &serde_json::json!({"xid": xid, "me": "x", "month": month}),
        )
        .await;
        assert_eq!((s, body.as_str()), (status, expected), "{month}");
    }
    // Queued legacy rows (Y0-1, Y0-3): any year-zero hint declines the whole request before replay.
    let idx = index(dir.path());
    for (id, kind, payload) in [
        (
            "edit0",
            "exchange_edit",
            serde_json::json!({"xid":"abcdef12","new_me":"x","new_claude":"","month":"0000-01"}),
        ),
        (
            "day0",
            "exchange",
            serde_json::json!({"day":"0000-01-01","xid":"abcdef34","sub_header":"12:00 | Synthetic","body":diary_corpus_write::render_exchange("zero", "", "abcdef34")}),
        ),
    ] {
        idx.execute(
            "INSERT INTO journal (id,created_at,kind,payload) VALUES (?,'2030',?,?)",
            rusqlite::params![id, kind, payload.to_string()],
        )
        .unwrap();
        let put = serde_json::json!({"path":"a.md","content":"x","version":null});
        let (s, d, _) = send(addr, "PUT", "/api/file", &writer(), &put).await;
        assert_eq!((s, d), (421, true), "{id}");
        let row: (i64, i64, Option<String>) = idx
            .query_row(
                "SELECT applied, attempts, last_error FROM journal WHERE id=?",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(row, (0, 0, None), "{id} must be untouched");
        assert!(file_row(dir.path(), "a.md").is_none(), "{id}");
        // The reference applier retires the row; native admission resumes afterwards.
        idx.execute("UPDATE journal SET applied=1 WHERE id=?", [id])
            .unwrap();
    }
    // Adjacent valid years are not declined (month hints 0001-01 and 9999-12, day 0001-01-01).
    for (id, kind, payload) in [
        (
            "edit1",
            "exchange_edit",
            serde_json::json!({"xid":"abcdef12","new_me":"x","new_claude":"","month":"0001-01"}),
        ),
        (
            "edit9999",
            "exchange_edit",
            serde_json::json!({"xid":"abcdef12","new_me":"x","new_claude":"","month":"9999-12"}),
        ),
        (
            "day1",
            "exchange",
            serde_json::json!({"day":"0001-01-01","xid":"abcdef56","sub_header":"12:00 | Synthetic","body":diary_corpus_write::render_exchange("one", "", "abcdef56")}),
        ),
    ] {
        idx.execute(
            "INSERT INTO journal (id,created_at,kind,payload) VALUES (?,'2030',?,?)",
            rusqlite::params![id, kind, payload.to_string()],
        )
        .unwrap();
        let put = serde_json::json!({"path":format!("{id}.md"),"content":"x","version":null});
        let (_, d, _) = send(addr, "PUT", "/api/file", &writer(), &put).await;
        assert!(!d, "{id} must stay native");
    }
}

// ------------------------------------------------- D6 recovery x R4 backup authority (union)

const INTENT: &str = r#"{"id":"synthetic-intent","method":"PUT","path":"/synthetic"}"#;

fn primary(root: &Path) -> rusqlite::Connection {
    rusqlite::Connection::open(tenant_root(root).join("managed-diary.db")).unwrap()
}

fn outbox(root: &Path) -> Vec<String> {
    let db = primary(root);
    let mut stmt = db
        .prepare("SELECT path FROM workspace_index_outbox ORDER BY path")
        .unwrap();
    stmt.query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

fn journal_rows(root: &Path) -> Vec<(String, i64, i64)> {
    let idx = index(root);
    let mut stmt = idx
        .prepare("SELECT id, applied, attempts FROM journal ORDER BY rowid")
        .unwrap();
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

/// Imported Markdown queued for indexing (not yet marked dirty), plus one pending index entry.
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_drain_borrows_its_held_primary_reservation_for_journal_marks() {
    let dir = tempfile::tempdir().unwrap();
    queued_import(dir.path());
    let addr = start(dir.path(), "rust").await;
    let b = serde_json::json!({"path": "a.md", "content": "x", "version": null});
    // A drain that opened a second primary reservation for each dirty mark waited for its own
    // (the 30 s busy timeout) and then failed the write with a 500.
    let (s, d, body) = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        send(addr, "PUT", "/api/file", &writer(), &b),
    )
    .await
    .expect("the outbox drain contended with its own primary reservation");
    assert_eq!((s, d), (200, false), "{body}");
    assert!(outbox(dir.path()).is_empty());
    assert_eq!(dirty(dir.path()), ["Imports/x.md", "Imports/y.md", "a.md"]);
    // The next write recovers again: nothing is transferred twice, every mark stays single.
    let b2 = serde_json::json!({"path": "b.md", "content": "y", "version": null});
    let (s, _, body) = send(addr, "PUT", "/api/file", &writer(), &b2).await;
    assert_eq!(s, 200, "{body}");
    assert!(outbox(dir.path()).is_empty());
    assert_eq!(
        dirty(dir.path()),
        ["Imports/x.md", "Imports/y.md", "a.md", "b.md"]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pending_backup_intent_stops_recovery_before_any_effect() {
    let dir = tempfile::tempdir().unwrap();
    queued_import(dir.path());
    index(dir.path())
        .execute(
            "INSERT INTO journal (id, created_at, kind, payload) VALUES ('m','2030','index_month',?)",
            [r#"{"label":"January 2030","month":"2030-01.md"}"#],
        )
        .unwrap();
    primary(dir.path())
        .execute(
            "INSERT INTO meta VALUES ('backup_effect_pending', ?)",
            [INTENT],
        )
        .unwrap();
    let addr = start(dir.path(), "rust").await;
    let b = serde_json::json!({"path": "a.md", "content": "x", "version": null});
    let (s, d, body) = send(addr, "PUT", "/api/file", &writer(), &b).await;
    assert_eq!((s, d), (503, false));
    assert!(body.contains("backup settlement is uncertain"), "{body}");
    // No replay, transfer, dirty mark or write while the intent is unsettled.
    assert_eq!(outbox(dir.path()), ["Imports/x.md", "Imports/y.md"]);
    assert!(dirty(dir.path()).is_empty());
    assert_eq!(journal_rows(dir.path()), [("m".to_string(), 0, 0)]);
    assert!(file_row(dir.path(), "a.md").is_none());
    assert!(file_row(dir.path(), "INDEX.md").is_none());
    // Synthetic settlement (test-only removal): the next write recovers exactly once.
    primary(dir.path())
        .execute("DELETE FROM meta WHERE key='backup_effect_pending'", [])
        .unwrap();
    let (s, _, body) = send(addr, "PUT", "/api/file", &writer(), &b).await;
    assert_eq!(s, 200, "{body}");
    assert!(outbox(dir.path()).is_empty());
    assert_eq!(journal_rows(dir.path()), [("m".to_string(), 1, 0)]);
    assert_eq!(dirty(dir.path()), ["Imports/x.md", "Imports/y.md", "a.md"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_intent_committed_while_the_drain_waits_refuses_every_transfer() {
    let dir = tempfile::tempdir().unwrap();
    queued_import(dir.path());
    let addr = start(dir.path(), "rust").await;
    // Another holder of the one primary reservation (as native backup intent insertion is).
    let path = tenant_root(dir.path()).join("managed-diary.db");
    let (held_tx, held) = std::sync::mpsc::channel();
    let (go, go_rx) = std::sync::mpsc::channel::<()>();
    let holder = std::thread::spawn(move || {
        let mut c = rusqlite::Connection::open(&path).unwrap();
        let t = c
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .unwrap();
        held_tx.send(()).unwrap();
        go_rx.recv().unwrap();
        t.execute(
            "INSERT INTO meta VALUES ('backup_effect_pending', ?)",
            [INTENT],
        )
        .unwrap();
        t.commit().unwrap();
    });
    held.recv().unwrap();
    let b = serde_json::json!({"path": "a.md", "content": "x", "version": null});
    let request = tokio::spawn(async move { send(addr, "PUT", "/api/file", &writer(), &b).await });
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert!(
        !request.is_finished(),
        "the drain must wait for the held reservation"
    );
    go.send(()).unwrap();
    holder.join().unwrap();
    let (s, _, _) = tokio::time::timeout(std::time::Duration::from_secs(10), request)
        .await
        .unwrap()
        .unwrap();
    // The drain got the reservation only after the intent committed: it refuses inside it,
    // rolls back and transfers nothing (the write itself is not attempted).
    assert_ne!(s, 200);
    assert_eq!(outbox(dir.path()), ["Imports/x.md", "Imports/y.md"]);
    assert!(dirty(dir.path()).is_empty());
    assert!(file_row(dir.path(), "a.md").is_none());
}

#[test]
fn borrowed_journal_marks_check_the_held_reservation_and_refuse_any_other() {
    let dir = tempfile::tempdir().unwrap();
    managed_tenant(dir.path(), &[]);
    let t = tenant_root(dir.path());
    let journal = noevia_diary::write::Journal::open(&t.join("managed-index.db")).unwrap();
    let mut held = primary(dir.path());
    let tx = held
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .unwrap();
    journal.mark_dirty_in(&tx, "a.md").unwrap();
    // The reservation is still held through the effect: no intent can commit beside it.
    let other = primary(dir.path());
    other
        .busy_timeout(std::time::Duration::from_millis(100))
        .unwrap();
    assert!(other
        .execute(
            "INSERT INTO meta VALUES ('backup_effect_pending', ?)",
            [INTENT]
        )
        .is_err());
    tx.commit().unwrap();
    other
        .execute(
            "INSERT INTO meta VALUES ('backup_effect_pending', ?)",
            [INTENT],
        )
        .unwrap();
    let tx = held
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .unwrap();
    assert!(journal.mark_dirty_in(&tx, "b.md").is_err());
    drop(tx);
    // A transaction on any other database is never accepted as the primary reservation.
    let mut foreign = rusqlite::Connection::open(dir.path().join("foreign.db")).unwrap();
    foreign
        .execute_batch("CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);")
        .unwrap();
    let ftx = foreign
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .unwrap();
    assert!(journal.mark_dirty_in(&ftx, "c.md").is_err());
    drop(ftx);
    assert_eq!(dirty(dir.path()), ["a.md"]);
}
