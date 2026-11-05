//! D6: GET /api/external-sources and GET /api/workspace-trash over the real server on a synthetic
//! managed tenant (and, for the first, a synthetic source folder). The byte-for-byte comparison with
//! Python is services' tests/test_rust_forward_d6.py.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use std::sync::Arc;

const TOKEN: &str = "synthetic-service-token";
const SECRET: &str = "synthetic-forward-secret";
const UID: &str = "0b5f6c1e-3a2d-4c8e-9f10-1a2b3c4d5e6f";

fn managed_tenant(root: &std::path::Path) {
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
    let idx = rusqlite::Connection::open(t.join("managed-index.db")).unwrap();
    idx.execute_batch(
        "CREATE TABLE dirty_documents (document TEXT PRIMARY KEY);
         CREATE TABLE journal (id TEXT PRIMARY KEY, created_at TEXT NOT NULL, kind TEXT NOT NULL, payload TEXT NOT NULL,
             applied INTEGER NOT NULL DEFAULT 0, applied_at TEXT, attempts INTEGER NOT NULL DEFAULT 0, last_error TEXT);",
    )
    .unwrap();
    std::fs::write(t.join("managed-diary.active"), UID).unwrap();
}

fn config(root: &std::path::Path, sources: Option<Vec<String>>) -> String {
    let mut v = serde_json::json!({
        "port": 0, "control_token": "ctl", "forward_secret": SECRET, "auth_token": TOKEN, "tenant_key": "",
        "data_root": root.to_str().unwrap(), "legacy_user_id": "",
        "corpus": {"entry_layout": "daily", "entries_prefix": "Entries", "index_enabled": false,
                   "month_file_template": "{year}-{month02}.md", "monthly_prefix": "", "index_file": "INDEX.md"},
        "llm": {"main": {"base_url": "http://127.0.0.1:9/v1", "chat_model": "chat-a", "setting_key": "LLM_CHAT_MODEL",
                          "fallback_setting_key": "LLM_CHAT_FALLBACK_MODEL"},
                "aux": {"base_url": "http://127.0.0.1:9/v1", "chat_model": "aux-a", "setting_key": "LLM_AUX_MODEL",
                         "fallback_setting_key": "LLM_AUX_FALLBACK_MODEL"},
                "aux_prefers_main": true, "configured": "chat-a"}
    });
    if let Some(s) = sources {
        v["external_sources"] = serde_json::json!(s);
    }
    v.to_string()
}

async fn start(root: &std::path::Path, sources: Option<Vec<String>>) -> std::net::SocketAddr {
    let cfg = noevia_diary::config::parse(&config(root, sources)).unwrap();
    let app = Arc::new(noevia_diary::server::App::new(cfg));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(noevia_diary::serve(listener, app));
    addr
}

async fn get(addr: std::net::SocketAddr, token: &str, uid: &str) -> (u16, Option<String>, String) {
    let resp = reqwest::Client::new()
        .get(format!("http://{addr}/api/external-sources"))
        .header("authorization", format!("Bearer {token}"))
        .header("x-cowork-user-id", uid)
        .header("x-noevia-diary-forward", SECRET)
        .header("x-noevia-diary-recovered", "1")
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let decline = resp
        .headers()
        .get("x-noevia-diary-decline")
        .map(|v| v.to_str().unwrap().to_string());
    (status, decline, resp.text().await.unwrap())
}

fn write(path: &std::path::Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

#[tokio::test]
async fn lists_a_synthetic_folder_like_python() {
    let dir = tempfile::tempdir().unwrap();
    managed_tenant(dir.path());
    let src = dir.path().join("src");
    write(&src.join("2026-09-06 trip.md"), "abc");
    write(&src.join("sub/notes_20260907.txt"), "hello");
    write(&src.join("image.png"), "no");
    write(&src.join(".md"), "no");
    let missing = dir.path().join("missing");
    let addr = start(
        dir.path(),
        Some(vec![
            src.to_str().unwrap().to_string(),
            missing.to_str().unwrap().to_string(),
        ]),
    )
    .await;
    let (s, d, body) = get(addr, TOKEN, UID).await;
    assert_eq!((s, d), (200, None));
    let p = src.to_str().unwrap();
    let m = missing.to_str().unwrap();
    assert_eq!(
        body,
        format!(
            r#"{{"configured":true,"sources":[{{"path":"{p}","exists":true,"files":[{{"name":"2026-09-06 trip.md","rel_path":"2026-09-06 trip.md","size":3,"date":"2026-09-06","date_source":"filename"}},{{"name":"notes_20260907.txt","rel_path":"sub/notes_20260907.txt","size":5,"date":"2026-09-07","date_source":"filename"}}],"total":2,"truncated":false}},{{"path":"{m}","exists":false,"files":[],"total":0,"truncated":false}}],"total":2}}"#
        )
    );
    // The token check answers {"error": ...} on this route.
    let (s, _, body) = get(addr, "wrong", UID).await;
    assert_eq!((s, body.as_str()), (401, r#"{"error":"unauthorized"}"#));
    // The tenant is resolved after the token: a non-UUID tenant is Python's 400.
    let (s, _, body) = get(addr, TOKEN, "not-a-uuid").await;
    assert_eq!(
        (s, body.as_str()),
        (400, r#"{"detail":"missing or invalid X-Cowork-User-ID"}"#)
    );
}

#[tokio::test]
async fn declines_when_python_did_not_name_the_folders() {
    let dir = tempfile::tempdir().unwrap();
    managed_tenant(dir.path());
    let addr = start(dir.path(), None).await;
    let (s, d, _) = get(addr, TOKEN, UID).await;
    assert_eq!((s, d.as_deref()), (421, Some("1")));
    let addr = start(dir.path(), Some(vec![])).await;
    let (s, _, body) = get(addr, TOKEN, UID).await;
    assert_eq!(
        (s, body.as_str()),
        (200, r#"{"configured":false,"sources":[],"total":0}"#)
    );
}

async fn get_path(
    addr: std::net::SocketAddr,
    target: &str,
    token: &str,
) -> (u16, Option<String>, Option<String>, String) {
    let resp = reqwest::Client::new()
        .get(format!("http://{addr}{target}"))
        .header("authorization", format!("Bearer {token}"))
        .header("x-cowork-user-id", UID)
        .header("x-noevia-diary-forward", SECRET)
        .header("x-noevia-diary-recovered", "1")
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let h = |n: &str| {
        resp.headers()
            .get(n)
            .map(|v| v.to_str().unwrap().to_string())
    };
    let (decline, cache) = (h("x-noevia-diary-decline"), h("cache-control"));
    (status, decline, cache, resp.text().await.unwrap())
}

fn capsule(id: &str, data: &[u8]) -> Vec<u8> {
    use sha2::{Digest, Sha256};
    let version: String = Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    // base64 of "hello" (no padding needed for this test's bytes: 5 bytes -> "aGVsbG8=")
    assert_eq!(data, b"hello");
    format!(
        r#"{{"data":"aGVsbG8=","format":"noevia-trash-v1","id":"{id}","path":"Notes/a.md","state":"trashed","trashedAt":1730000000.5,"version":"{version}"}}"#
    )
    .into_bytes()
}

#[tokio::test]
async fn workspace_trash_lists_capsules_like_python() {
    let dir = tempfile::tempdir().unwrap();
    managed_tenant(dir.path());
    let ids = [
        "00000000-0000-4000-8000-000000000001",
        "00000000-0000-4000-8000-000000000002",
    ];
    let db =
        rusqlite::Connection::open(dir.path().join("users").join(UID).join("managed-diary.db"))
            .unwrap();
    for id in ids {
        db.execute(
            "INSERT INTO files VALUES (?,?,'v',1.0)",
            rusqlite::params![format!(".noevia-trash/{id}.json"), capsule(id, b"hello")],
        )
        .unwrap();
    }
    db.execute(
        "INSERT INTO files VALUES ('.noevia-trash/00000000-0000-4000-8000-000000000003.json',?,'v',1.0)",
        rusqlite::params![b"{not json".to_vec()],
    )
    .unwrap();
    let addr = start(dir.path(), None).await;
    let (s, d, cache, body) = get_path(addr, "/api/workspace-trash", TOKEN).await;
    assert_eq!((s, d, cache.as_deref()), (200, None, Some("no-store")));
    let v = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
    let rec = |n: u8| {
        format!(
            r#"{{"id":"00000000-0000-4000-8000-00000000000{n}","path":"Notes/a.md","version":"{v}","state":"trashed","trashedAt":1730000000.5}}"#
        )
    };
    assert_eq!(
        body,
        format!(
            r#"{{"records":[{},{},{{"id":"00000000-0000-4000-8000-000000000003","path":null,"version":null,"state":"invalid","trashedAt":null,"invalid":true}}],"next":null}}"#,
            rec(1),
            rec(2)
        )
    );
    // after: the records past that id
    let (s, _, _, body) = get_path(
        addr,
        "/api/workspace-trash?after=00000000-0000-4000-8000-000000000001",
        TOKEN,
    )
    .await;
    assert_eq!(s, 200);
    assert!(body.starts_with(&format!(r#"{{"records":[{},"#, rec(2))));
    // an id Python alone can read declines; one it refuses is its 409
    let (s, d, _, _) = get_path(addr, "/api/workspace-trash?after=not-a-uuid", TOKEN).await;
    assert_eq!((s, d.as_deref()), (421, Some("1")));
    let (s, _, cache, body) = get_path(addr, "/api/workspace-trash?after=abc", TOKEN).await;
    assert_eq!(
        (s, cache, body.as_str()),
        (
            409,
            None,
            r#"{"detail":"A valid recovery operation ID is required"}"#
        )
    );
    // HTTPException(401), like the D4b routes
    let (s, _, _, body) = get_path(addr, "/api/workspace-trash", "wrong").await;
    assert_eq!((s, body.as_str()), (401, r#"{"detail":"unauthorized"}"#));
}
