//! End-to-end: the real server on an ephemeral port over a synthetic managed tenant. Python is the
//! only verifier; this side trusts only its forward secret.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use std::sync::Arc;

const KEY: &str = "synthetic-tenant-key";
const TOKEN: &str = "synthetic-service-token";
const SECRET: &str = "synthetic-forward-secret";
const UID: &str = "0b5f6c1e-3a2d-4c8e-9f10-1a2b3c4d5e6f";
const UNVERIFIED: &str = r#"{"detail":"invalid tenant assertion"}"#;

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
        "port": 0, "control_token": "ctl", "forward_secret": SECRET, "auth_token": TOKEN, "tenant_key": KEY,
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
    let app = Arc::new(noevia_diary::server::App::new(cfg));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(noevia_diary::serve(listener, app));
    addr
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

/// What Python's forwarder sends for a request its middleware verified.
fn forwarded() -> Vec<(&'static str, String)> {
    vec![
        ("authorization", format!("Bearer {TOKEN}")),
        ("x-cowork-user-id", UID.to_string()),
        ("x-noevia-diary-forward", SECRET.to_string()),
        ("x-noevia-diary-asserted", "1".to_string()),
        ("x-noevia-diary-recovered", "1".to_string()),
    ]
}

fn without(mut h: Vec<(&'static str, String)>, name: &str) -> Vec<(&'static str, String)> {
    h.retain(|(k, _)| *k != name);
    h
}

fn with(
    mut h: Vec<(&'static str, String)>,
    name: &'static str,
    v: &str,
) -> Vec<(&'static str, String)> {
    h.retain(|(k, _)| *k != name);
    h.push((name, v.to_string()));
    h
}

#[tokio::test]
async fn months_and_day_for_a_recovered_managed_tenant() {
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
    let (s, d, body) = get(addr, "/api/months", &forwarded()).await;
    assert_eq!((s, d), (200, None));
    assert_eq!(
        body,
        r#"{"months":[{"id":"2030-01","label":"January 2030","file":"Entries/2030/January"}]}"#
    );
    let (s, _, body) = get(addr, "/api/day?month=2030-01", &forwarded()).await;
    assert_eq!(s, 200);
    let expected = format!(
        r#"{{"month":"2030-01","log":{},"standing":""}}"#,
        serde_json::to_string(day.trim()).unwrap()
    );
    assert_eq!(body, expected);
    let (s, _, body) = get(addr, "/api/day?month=2030-13", &forwarded()).await;
    assert_eq!(
        (s, body.as_str()),
        (400, r#"{"error":"month out of range"}"#)
    );
}

#[tokio::test]
async fn a_missing_or_forged_forward_secret_gets_pythons_401() {
    let dir = tempfile::tempdir().unwrap();
    managed_tenant(dir.path(), &[], "{}");
    let addr = start(dir.path()).await;
    for h in [
        without(forwarded(), "x-noevia-diary-forward"),
        with(
            forwarded(),
            "x-noevia-diary-forward",
            "synthetic-forward-secreT",
        ),
        with(forwarded(), "x-noevia-diary-forward", ""),
    ] {
        for target in ["/api/months", "/api/day", "/api/health", "/api/files"] {
            let (s, d, body) = get(addr, target, &h).await;
            assert_eq!((s, d, body.as_str()), (401, None, UNVERIFIED), "{target}");
        }
    }
    // A forwarded tenant request Python did not verify (key set): refused the same way.
    let (s, _, body) = get(
        addr,
        "/api/months",
        &without(forwarded(), "x-noevia-diary-asserted"),
    )
    .await;
    assert_eq!((s, body.as_str()), (401, UNVERIFIED));
}

#[tokio::test]
async fn auth_and_tenant_refusals_follow_python_order() {
    let dir = tempfile::tempdir().unwrap();
    managed_tenant(dir.path(), &[], "{}");
    let addr = start(dir.path()).await;
    let (s, _, body) = get(
        addr,
        "/api/months",
        &with(forwarded(), "authorization", "Bearer wrong"),
    )
    .await;
    assert_eq!((s, body.as_str()), (401, r#"{"error":"unauthorized"}"#));
    let h = without(
        without(forwarded(), "x-cowork-user-id"),
        "x-noevia-diary-asserted",
    );
    let (s, _, body) = get(addr, "/api/months", &h).await;
    assert_eq!(
        (s, body.as_str()),
        (400, r#"{"detail":"missing or invalid X-Cowork-User-ID"}"#)
    );
    let (s, _, body) = get(
        addr,
        "/api/months",
        &with(forwarded(), "x-cowork-user-id", "not-a-uuid"),
    )
    .await;
    assert_eq!(
        (s, body.as_str()),
        (400, r#"{"detail":"missing or invalid X-Cowork-User-ID"}"#)
    );
}

#[tokio::test]
async fn declines_until_python_has_recovered_and_while_recovery_is_outstanding() {
    let dir = tempfile::tempdir().unwrap();
    let addr = start(dir.path()).await;
    // Unknown tenant: Python builds or refuses it.
    assert_eq!(get(addr, "/api/months", &forwarded()).await.0, 421);
    managed_tenant(dir.path(), &[], "{}");
    assert_eq!(get(addr, "/api/months", &forwarded()).await.0, 200);
    // Python has not recovered this tenant in this process: it serves (and recovers) first.
    let not_recovered = without(forwarded(), "x-noevia-diary-recovered");
    assert_eq!(get(addr, "/api/months", &not_recovered).await.0, 421);
    let t = dir.path().join("users").join(UID);
    let idx = rusqlite::Connection::open(t.join("managed-index.db")).unwrap();
    let db = rusqlite::Connection::open(t.join("managed-diary.db")).unwrap();
    for (conn, add, clear) in [
        (
            &idx,
            "INSERT INTO journal(id,created_at,kind,payload) VALUES ('j','t','exchange','{}')",
            "UPDATE journal SET applied=1",
        ),
        (
            &idx,
            "INSERT INTO dirty_documents VALUES ('Entries/x.md')",
            "DELETE FROM dirty_documents",
        ),
        (
            &db,
            "INSERT INTO workspace_index_outbox VALUES ('Entries/x.md')",
            "DELETE FROM workspace_index_outbox",
        ),
    ] {
        conn.execute(add, []).unwrap();
        assert_eq!(get(addr, "/api/months", &forwarded()).await.0, 421, "{add}");
        conn.execute(clear, []).unwrap();
        assert_eq!(
            get(addr, "/api/months", &forwarded()).await.0,
            200,
            "{clear}"
        );
    }
    assert_eq!(get(addr, "/api/files", &forwarded()).await.0, 421);
    db.execute(
        "UPDATE meta SET value='{\"index_enabled\": true}' WHERE key='settings'",
        [],
    )
    .unwrap();
    assert_eq!(get(addr, "/api/months", &forwarded()).await.0, 421);
}

#[tokio::test]
async fn health_process_part_and_control() {
    let dir = tempfile::tempdir().unwrap();
    let addr = start(dir.path()).await;
    let h = vec![
        ("authorization", format!("Bearer {TOKEN}")),
        ("x-noevia-diary-forward", SECRET.to_string()),
    ];
    let (s, _, body) = get(addr, "/api/health", &h).await;
    assert_eq!(s, 200);
    assert_eq!(
        body,
        r#"{"ok":true,"auth_required":true,"model":"chat-a","model_configured":"chat-a","model_source":"unreachable","model_warnings":[]}"#
    );
    let tenant = with(forwarded(), "x-cowork-storage", "e30");
    assert_eq!(get(addr, "/api/health", &tenant).await.0, 421);
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
    // The floor protocol is gone.
    assert_eq!(
        get(
            addr,
            "/internal/floor",
            &[("x-noevia-diary-control", "ctl".into())]
        )
        .await
        .0,
        404
    );
}
