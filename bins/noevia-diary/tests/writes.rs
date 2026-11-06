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
async fn declines_without_writer_role_recovery_or_with_pending_work() {
    let dir = tempfile::tempdir().unwrap();
    managed_tenant(dir.path(), &[]);
    let addr = start(dir.path(), "rust").await;
    let b = serde_json::json!({"path": "a.md", "content": "x", "version": null});
    for h in [
        without(writer(), "x-noevia-diary-writer"),
        without(writer(), "x-noevia-diary-recovered"),
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
    assert_eq!((s, d), (421, true));
    index(dir.path())
        .execute("UPDATE journal SET applied=1", [])
        .unwrap();
    // Index outbox: Python drains it.
    let db = rusqlite::Connection::open(tenant_root(dir.path()).join("managed-diary.db")).unwrap();
    db.execute("INSERT INTO workspace_index_outbox VALUES ('x.md')", [])
        .unwrap();
    let (s, d, _) = send(addr, "PUT", "/api/file", &writer(), &b).await;
    assert_eq!((s, d), (421, true));
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

fn import_archive() -> Vec<u8> {
    let header = serde_json::json!({"settings": {}, "directories": ["empty"],
        "files": [{"path": "note.md", "bytes": 4}]})
    .to_string();
    let mut frame = diary_workspace_export::MAGIC.to_vec();
    frame.extend_from_slice(&(header.len() as u32).to_be_bytes());
    frame.extend_from_slice(header.as_bytes());
    frame.extend_from_slice(b"note");
    diary_workspace_export::export(frame.as_slice()).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn import_cancel_keeps_writer_until_apply_settles() {
    let dir = tempfile::tempdir().unwrap();
    managed_tenant(dir.path(), &[]);
    let root = tenant_root(dir.path());
    let body = import_archive();
    let mut digest = sha2::Sha256::new();
    use sha2::Digest;
    digest.update(b"Imports/Copy\0");
    digest.update(&body);
    let fingerprint = format!("{:x}", digest.finalize());
    let db = rusqlite::Connection::open(root.join("managed-diary.db")).unwrap();
    db.execute_batch("BEGIN IMMEDIATE").unwrap();
    let writers = Arc::new(noevia_diary::write::Writers::default());
    let w = writers.clone();
    let r = root.clone();
    let (entered, ready) = tokio::sync::oneshot::channel();
    let outer = tokio::spawn(async move {
        tokio::task::spawn_blocking(move || {
            w.with_lock(UID, || {
                entered.send(()).unwrap();
                noevia_diary::workspace_import::perform(
                    &r.join("managed-diary.db"),
                    &r.join("managed-index.db"),
                    UID,
                    &body,
                    noevia_diary::workspace_import::Options {
                        action: "apply",
                        name: "Copy",
                        reviewed: &fingerprint,
                        native_errors: false,
                    },
                )
            })
        })
        .await
        .unwrap()
    });
    ready.await.unwrap();
    outer.abort();
    let _ = outer.await;
    let w = writers.clone();
    let r = root.clone();
    let mut second = tokio::task::spawn_blocking(move || {
        w.with_lock(UID, || {
            let db = rusqlite::Connection::open(r.join("managed-diary.db")).unwrap();
            let count: i64 = db
                .query_row("SELECT count(*) FROM workspace_imports", [], |r| r.get(0))
                .unwrap();
            assert_eq!(count, 1, "next writer must observe the settled receipt");
        })
    });
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), &mut second)
            .await
            .is_err()
    );
    db.execute_batch("COMMIT").unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), second)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        file_row(dir.path(), "Imports/Copy/note.md").unwrap().0,
        b"note"
    );
    assert_eq!(dirty(dir.path()), vec!["Imports/Copy/note.md"]);
}

#[test]
fn import_failed_dirty_transfer_leaves_durable_outbox() {
    let dir = tempfile::tempdir().unwrap();
    managed_tenant(dir.path(), &[]);
    let root = tenant_root(dir.path());
    let body = import_archive();
    use sha2::Digest;
    let mut hash = sha2::Sha256::new();
    hash.update(b"Imports/Copy\0");
    hash.update(&body);
    let fingerprint = format!("{:x}", hash.finalize());
    let idx = index(dir.path());
    idx.execute_batch("CREATE TRIGGER synthetic_dirty_failure BEFORE INSERT ON dirty_documents BEGIN SELECT RAISE(ABORT, 'synthetic failure'); END;").unwrap();
    let result = noevia_diary::workspace_import::perform(
        &root.join("managed-diary.db"),
        &root.join("managed-index.db"),
        UID,
        &body,
        noevia_diary::workspace_import::Options {
            action: "apply",
            name: "Copy",
            reviewed: &fingerprint,
            native_errors: false,
        },
    );
    assert!(matches!(
        result,
        noevia_diary::workspace::Reply::Json(200, _)
    ));
    let db = rusqlite::Connection::open(root.join("managed-diary.db")).unwrap();
    let outbox: String = db
        .query_row("SELECT path FROM workspace_index_outbox", [], |r| r.get(0))
        .unwrap();
    assert_eq!(outbox, "Imports/Copy/note.md");
    assert_eq!(
        db.query_row("SELECT count(*) FROM workspace_imports", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert!(dirty(dir.path()).is_empty());
}

#[test]
fn import_default_generation_refusal_declines_before_apply_attempt() {
    use noevia_diary::{
        workspace::Reply,
        workspace_import::{perform, Options},
    };
    let dir = tempfile::tempdir().unwrap();
    managed_tenant(dir.path(), &[]);
    let root = tenant_root(dir.path());
    let body = import_archive();
    use sha2::Digest;
    let mut hash = sha2::Sha256::new();
    hash.update(b"Imports/Copy\0");
    hash.update(&body);
    let fingerprint = format!("{:x}", hash.finalize());
    let db = rusqlite::Connection::open(root.join("managed-diary.db")).unwrap();
    // An apply attempt would hit this trigger and return a database error instead of decline.
    db.execute_batch("CREATE TRIGGER synthetic_no_apply BEFORE INSERT ON files BEGIN SELECT RAISE(ABORT, 'apply attempted'); END;").unwrap();
    for generation in ["bad".into(), "1".repeat(4301), "9".repeat(4300)] {
        db.execute(
            "UPDATE meta SET value=? WHERE key='generation'",
            [&generation],
        )
        .unwrap();
        let response = perform(
            &root.join("managed-diary.db"),
            &root.join("managed-index.db"),
            UID,
            &body,
            Options {
                action: "apply",
                name: "Copy",
                reviewed: &fingerprint,
                native_errors: false,
            },
        );
        assert!(matches!(response, Reply::Decline));
        for table in [
            "files",
            "directories",
            "workspace_imports",
            "workspace_index_outbox",
        ] {
            assert_eq!(
                db.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r
                    .get::<_, i64>(0))
                    .unwrap(),
                0
            );
        }
        assert_eq!(meta(dir.path(), "generation"), generation);
        assert!(dirty(dir.path()).is_empty());
    }
}
