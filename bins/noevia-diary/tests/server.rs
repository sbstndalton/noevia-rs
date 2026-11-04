//! End-to-end: the real server on an ephemeral port over a synthetic managed tenant.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use std::sync::Arc;

const KEY: &str = "synthetic-tenant-key";
const TOKEN: &str = "synthetic-service-token";
const UID: &str = "0b5f6c1e-3a2d-4c8e-9f10-1a2b3c4d5e6f";

fn managed_tenant(root: &std::path::Path, files: &[(&str, &str)], settings: &str) {
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
        ("settings", settings),
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

fn config(root: &std::path::Path) -> String {
    serde_json::json!({
        "port": 0, "control_token": "ctl", "auth_token": TOKEN, "tenant_key": KEY,
        "data_root": root.to_str().unwrap(), "legacy_user_id": "",
        "corpus": {"entry_layout": "daily", "entries_prefix": "Entries", "index_enabled": false,
                   "month_file_template": "{year}-{month02}.md", "monthly_prefix": "", "index_file": "INDEX.md"},
        "llm": {"main": {"base_url": "http://127.0.0.1:9/v1", "chat_model": "chat-a", "setting_key": "LLM_CHAT_MODEL",
                          "fallback_setting_key": "LLM_CHAT_FALLBACK_MODEL"},
                "aux": {"base_url": "http://127.0.0.1:9/v1", "chat_model": "aux-a", "setting_key": "LLM_AUX_MODEL",
                         "fallback_setting_key": "LLM_AUX_FALLBACK_MODEL"},
                "aux_prefers_main": true, "configured": "chat-a"}
    })
    .to_string()
}

async fn start(root: &std::path::Path) -> std::net::SocketAddr {
    let cfg = noevia_diary::config::parse(&config(root)).unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64();
    let app = Arc::new(noevia_diary::server::App::new(cfg, now - 5.0));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(noevia_diary::serve(listener, app));
    addr
}

fn sign(method: &str, path: &str, query: &[u8], nonce: &str, ts: u64) -> String {
    use sha2::Digest;
    let empty: String = sha2::Sha256::digest(b"")
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    tenant_assertion::sign(
        &tenant_assertion::Request {
            key: KEY,
            user_id: UID,
            assertion: "",
            method,
            path,
            query,
            body_hash: &empty,
            storage: "",
            legacy_owner: "",
            blocked: "",
            now: 0.0,
        },
        ts,
        nonce,
    )
    .unwrap()
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

async fn get(
    addr: std::net::SocketAddr,
    target: &str,
    headers: &[(&str, String)],
) -> (u16, Option<String>, String) {
    let mut req = reqwest::Client::new().get(format!("http://{addr}{target}"));
    for (k, v) in headers {
        req = req.header(*k, v);
    }
    let resp = req.send().await.unwrap();
    let status = resp.status().as_u16();
    let decline = resp
        .headers()
        .get("x-noevia-diary-decline")
        .map(|v| v.to_str().unwrap().to_string());
    (status, decline, resp.text().await.unwrap())
}

fn signed_headers(path: &str, query: &[u8], nonce: &str) -> Vec<(&'static str, String)> {
    vec![
        ("authorization", format!("Bearer {TOKEN}")),
        ("x-cowork-user-id", UID.to_string()),
        (
            "x-cowork-tenant-assertion",
            sign("GET", path, query, nonce, now()),
        ),
    ]
}

#[tokio::test]
async fn months_and_day_for_a_ready_managed_tenant() {
    let dir = tempfile::tempdir().unwrap();
    let day = "## Sunday, January 6, 2030\n\n### 09:15 — walk\n\n**Me:** Walked by the river.\n\n**Assistant:** Noted.\n\n<!-- xid:aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee -->\n";
    managed_tenant(
        dir.path(),
        &[
            ("Entries/2030/January/January 6, 2030.md", day),
            ("Entries/2030/February/notes.txt", "x"),
        ],
        "{}",
    );
    let addr = start(dir.path()).await;

    let (s, d, body) = get(
        addr,
        "/api/months",
        &signed_headers("/api/months", b"", "00000000000000000000000000000001"),
    )
    .await;
    assert_eq!((s, d), (200, None));
    assert_eq!(
        body,
        r#"{"months":[{"id":"2030-01","label":"January 2030","file":"Entries/2030/January"}]}"#
    );

    let q = b"month=2030-01";
    let (s, _, body) = get(
        addr,
        "/api/day?month=2030-01",
        &signed_headers("/api/day", q, "00000000000000000000000000000002"),
    )
    .await;
    assert_eq!(s, 200);
    let expected = format!(
        r#"{{"month":"2030-01","log":{},"standing":""}}"#,
        serde_json::to_string(day.trim()).unwrap()
    );
    assert_eq!(body, expected);

    let (s, _, body) = get(
        addr,
        "/api/day?month=2030-13",
        &signed_headers(
            "/api/day",
            b"month=2030-13",
            "00000000000000000000000000000003",
        ),
    )
    .await;
    assert_eq!(
        (s, body.as_str()),
        (400, r#"{"error":"month out of range"}"#)
    );
}

#[tokio::test]
async fn replay_bad_auth_and_assertion_refusals() {
    let dir = tempfile::tempdir().unwrap();
    managed_tenant(dir.path(), &[], "{}");
    let addr = start(dir.path()).await;
    let h = signed_headers("/api/months", b"", "00000000000000000000000000000009");
    assert_eq!(get(addr, "/api/months", &h).await.0, 200);
    let (s, _, body) = get(addr, "/api/months", &h).await;
    assert_eq!(
        (s, body.as_str()),
        (401, r#"{"detail":"invalid tenant assertion"}"#)
    );

    let mut h = signed_headers("/api/months", b"", "0000000000000000000000000000000a");
    h[0].1 = "Bearer wrong".into();
    let (s, _, body) = get(addr, "/api/months", &h).await;
    assert_eq!((s, body.as_str()), (401, r#"{"error":"unauthorized"}"#));

    // Signed for another path: refused.
    let mut h = signed_headers("/api/day", b"", "0000000000000000000000000000000b");
    h[0].1 = format!("Bearer {TOKEN}");
    assert_eq!(get(addr, "/api/months", &h).await.0, 401);

    // No tenant at all: 400 after auth.
    let (s, _, body) = get(
        addr,
        "/api/months",
        &[("authorization", format!("Bearer {TOKEN}"))],
    )
    .await;
    assert_eq!(
        (s, body.as_str()),
        (400, r#"{"detail":"missing or invalid X-Cowork-User-ID"}"#)
    );
}

#[tokio::test]
async fn declines_before_verifying_anything_it_cannot_serve() {
    let dir = tempfile::tempdir().unwrap();
    let addr = start(dir.path()).await;
    // Unknown tenant (Python would build or refuse it): declined, and the nonce is NOT consumed.
    let h = signed_headers("/api/months", b"", "0000000000000000000000000000000c");
    let (s, d, _) = get(addr, "/api/months", &h).await;
    assert_eq!((s, d.as_deref()), (421, Some("1")));
    // A pending journal entry: declined too (Python replays first).
    managed_tenant(dir.path(), &[], "{}");
    let idx =
        rusqlite::Connection::open(dir.path().join("users").join(UID).join("managed-index.db"))
            .unwrap();
    idx.execute(
        "INSERT INTO journal(id,created_at,kind,payload) VALUES ('j','t','exchange','{}')",
        [],
    )
    .unwrap();
    assert_eq!(get(addr, "/api/months", &h).await.0, 421);
    idx.execute("UPDATE journal SET applied=1", []).unwrap();
    // Dirty documents only feed the index: they do not stop native reads.
    idx.execute("INSERT INTO dirty_documents VALUES ('Entries/x.md')", [])
        .unwrap();
    // Now served, with the same assertion: it was never recorded by the declines.
    assert_eq!(get(addr, "/api/months", &h).await.0, 200);
    // Unported routes and an enabled INDEX.md are declined.
    assert_eq!(get(addr, "/api/workspace-trash", &[]).await.0, 421);
    let db =
        rusqlite::Connection::open(dir.path().join("users").join(UID).join("managed-diary.db"))
            .unwrap();
    db.execute(
        "UPDATE meta SET value='{\"index_enabled\": true}' WHERE key='settings'",
        [],
    )
    .unwrap();
    let h = signed_headers("/api/months", b"", "0000000000000000000000000000000d");
    assert_eq!(get(addr, "/api/months", &h).await.0, 421);
}

#[tokio::test]
async fn health_process_part_and_control_floor() {
    let dir = tempfile::tempdir().unwrap();
    let addr = start(dir.path()).await;
    let (s, _, body) = get(
        addr,
        "/api/health",
        &[("authorization", format!("Bearer {TOKEN}"))],
    )
    .await;
    assert_eq!(s, 200);
    assert_eq!(
        body,
        r#"{"ok":true,"auth_required":true,"model":"chat-a","model_configured":"chat-a","model_source":"unreachable","model_warnings":[]}"#
    );
    // With a tenant and storage header, health stays with Python.
    let mut h = signed_headers("/api/health", b"", "0000000000000000000000000000000e");
    h.push(("x-cowork-storage", "e30".into()));
    assert_eq!(get(addr, "/api/health", &h).await.0, 421);
    // Control: wrong token refused; the floor refuses assertions signed at or before it.
    assert_eq!(get(addr, "/internal/ready", &[]).await.0, 403);
    assert_eq!(
        get(
            addr,
            "/internal/ready",
            &[("x-noevia-diary-control", "ctl".into())]
        )
        .await
        .0,
        200
    );
    managed_tenant(dir.path(), &[], "{}");
    let h = signed_headers("/api/months", b"", "0000000000000000000000000000000f");
    let resp = reqwest::Client::new()
        .post(format!("http://{addr}/internal/floor"))
        .header("x-noevia-diary-control", "ctl")
        .body(now().to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(get(addr, "/api/months", &h).await.0, 401);
}

#[tokio::test]
async fn d4b_files_and_status_are_native_and_pending_recovery_is_declined() {
    let dir = tempfile::tempdir().unwrap();
    let addr = start(dir.path()).await;
    managed_tenant(
        dir.path(),
        &[("memory.md", "# m"), ("Notes/a.md", "a")],
        "{}",
    );
    let h = signed_headers("/api/files", b"", "000000000000000000000000000000f1");
    let (s, _, body) = get(addr, "/api/files", &h).await;
    assert_eq!(
        (s, body.as_str()),
        (
            200,
            r#"{"files":[{"path":"memory.md","name":"memory.md","isDir":false,"modified":1.0},{"path":"Notes","name":"Notes","isDir":true,"modified":1.0}]}"#
        )
    );
    let h = signed_headers(
        "/api/storage-status",
        b"",
        "000000000000000000000000000000f2",
    );
    let (s, _, body) = get(addr, "/api/storage-status", &h).await;
    assert_eq!(
        (s, body.as_str()),
        (
            200,
            r#"{"mode":"managed","backup":"not_configured","lastBackedUp":null,"retryAt":null,"error":null}"#
        )
    );
    // The Python exporter (the default) stamps the clock into the ZIP: declined.
    let h = signed_headers(
        "/api/workspace-export",
        b"",
        "000000000000000000000000000000f3",
    );
    assert_eq!(get(addr, "/api/workspace-export", &h).await.0, 421);
    // Recovery Python still owes (a dirty document, an index outbox entry): declined.
    let t = dir.path().join("users").join(UID);
    let idx = rusqlite::Connection::open(t.join("managed-index.db")).unwrap();
    idx.execute("INSERT INTO dirty_documents VALUES ('memory.md')", [])
        .unwrap();
    let h = signed_headers("/api/files", b"", "000000000000000000000000000000f4");
    assert_eq!(get(addr, "/api/files", &h).await.0, 421);
    idx.execute("DELETE FROM dirty_documents", []).unwrap();
    let db = rusqlite::Connection::open(t.join("managed-diary.db")).unwrap();
    db.execute(
        "INSERT INTO workspace_index_outbox VALUES ('memory.md')",
        [],
    )
    .unwrap();
    assert_eq!(get(addr, "/api/files", &h).await.0, 421);
    db.execute("DELETE FROM workspace_index_outbox", [])
        .unwrap();
    // The same assertion was never consumed by the declines.
    assert_eq!(get(addr, "/api/files", &h).await.0, 200);
}
