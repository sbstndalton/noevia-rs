//! D8 verifier move: with `x-noevia-diary-verify: 1` this process is the only verifier of the
//! tenant assertion. Synthetic tenant and key only.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use std::path::Path;
use std::sync::Arc;

const KEY: &str = "synthetic-tenant-key";
const TOKEN: &str = "synthetic-service-token";
const SECRET: &str = "synthetic-forward-secret";
const UID: &str = "0b5f6c1e-3a2d-4c8e-9f10-1a2b3c4d5e6f";
const OTHER: &str = "9b5f6c1e-3a2d-4c8e-9f10-1a2b3c4d5e6f";
const UNVERIFIED: &str = r#"{"detail":"invalid tenant assertion"}"#;

fn managed_tenant(root: &Path) {
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

async fn start(root: &Path) -> std::net::SocketAddr {
    let cfg = serde_json::json!({
        "port": 0, "control_token": "ctl", "forward_secret": SECRET, "auth_token": TOKEN, "tenant_key": KEY,
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
    let app = Arc::new(noevia_diary::server::App::new(
        noevia_diary::config::parse(&cfg).unwrap(),
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(noevia_diary::serve(listener, app));
    addr
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

static NONCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn nonce() -> String {
    format!(
        "{:032x}",
        NONCE.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    )
}

/// The web signer for (method, path, query, body) as `uid`, at `ts`.
fn sign(uid: &str, method: &str, path: &str, query: &str, body: &[u8], ts: u64) -> String {
    let hash = noevia_diary::assertion::sha256_hex(body);
    let req = tenant_assertion::Request {
        key: KEY,
        user_id: uid,
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
    tenant_assertion::sign(&req, ts, &nonce()).unwrap()
}

/// What the forwarder sends for a request it delegates to this verifier.
fn delegated(assertion: &str) -> Vec<(&'static str, String)> {
    vec![
        ("authorization", format!("Bearer {TOKEN}")),
        ("x-cowork-user-id", UID.to_string()),
        ("x-cowork-tenant-assertion", assertion.to_string()),
        ("x-noevia-diary-forward", SECRET.to_string()),
        ("x-noevia-diary-asserted", "0".to_string()),
        ("x-noevia-diary-verify", "1".to_string()),
        ("x-noevia-diary-recovered", "1".to_string()),
    ]
}

struct Reply {
    status: u16,
    declined: bool,
    verified: bool,
    body: String,
}

async fn send(
    addr: std::net::SocketAddr,
    method: &str,
    target: &str,
    headers: &[(&'static str, String)],
    body: Vec<u8>,
) -> Reply {
    let mut req = reqwest::Client::new()
        .request(method.parse().unwrap(), format!("http://{addr}{target}"))
        .body(body);
    for (k, v) in headers {
        req = req.header(*k, v);
    }
    let resp = req.send().await.unwrap();
    let status = resp.status().as_u16();
    let declined = resp.headers().get("x-noevia-diary-decline").is_some();
    let verified = resp.headers().get("x-noevia-diary-verified").is_some();
    Reply {
        status,
        declined,
        verified,
        body: resp.text().await.unwrap(),
    }
}

#[tokio::test]
async fn valid_assertion_is_verified_here_once_and_replay_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    managed_tenant(dir.path());
    let addr = start(dir.path()).await;
    let a = sign(UID, "GET", "/api/months", "", b"", now());
    let r = send(addr, "GET", "/api/months", &delegated(&a), vec![]).await;
    assert_eq!((r.status, r.verified), (200, true), "{}", r.body);
    let r = send(addr, "GET", "/api/months", &delegated(&a), vec![]).await;
    assert_eq!(
        (r.status, r.verified, r.body.as_str()),
        (401, false, UNVERIFIED)
    );
}

#[tokio::test]
async fn refusals_have_pythons_401_bytes() {
    let dir = tempfile::tempdir().unwrap();
    managed_tenant(dir.path());
    let addr = start(dir.path()).await;
    let t = now();
    let cases: Vec<(&str, Vec<(&'static str, String)>)> = vec![
        ("missing header", {
            let mut h = delegated("");
            h.retain(|(k, _)| *k != "x-cowork-tenant-assertion");
            h
        }),
        ("malformed", delegated("v2.1.zz.x")),
        (
            "wrong tenant",
            delegated(&sign(OTHER, "GET", "/api/months", "", b"", t)),
        ),
        (
            "wrong path",
            delegated(&sign(UID, "GET", "/api/day", "", b"", t)),
        ),
        (
            "wrong query",
            delegated(&sign(UID, "GET", "/api/months", "x=1", b"", t)),
        ),
        (
            "expired",
            delegated(&sign(UID, "GET", "/api/months", "", b"", t - 3600)),
        ),
        (
            "future skew",
            delegated(&sign(UID, "GET", "/api/months", "", b"", t + 65)),
        ),
        (
            "past skew/restart",
            delegated(&sign(UID, "GET", "/api/months", "", b"", t - 65)),
        ),
    ];
    for (name, headers) in cases {
        let r = send(addr, "GET", "/api/months", &headers, vec![]).await;
        assert_eq!(
            (r.status, r.body.as_str(), r.verified),
            (401, UNVERIFIED, false),
            "{name}"
        );
    }
    // Inside the window (future side; the past side is bounded by the restart guard).
    let ok = sign(UID, "GET", "/api/months", "", b"", t + 55);
    assert_eq!(
        send(addr, "GET", "/api/months", &delegated(&ok), vec![])
            .await
            .status,
        200
    );
}

#[tokio::test]
async fn write_body_is_hashed_and_tampering_is_refused_before_any_write() {
    let dir = tempfile::tempdir().unwrap();
    managed_tenant(dir.path());
    let addr = start(dir.path()).await;
    let body = br#"{"path":"Notes/a.md","content":"synthetic"}"#.to_vec();
    let mut tampered = body.clone();
    tampered[35] = b'X';
    let a = sign(UID, "PUT", "/api/file", "", &body, now());
    let mut h = delegated(&a);
    h.push(("x-noevia-diary-writer", "1".into()));
    h.push(("content-type", "application/json".into()));
    let r = send(addr, "PUT", "/api/file", &h, tampered).await;
    assert_eq!((r.status, r.body.as_str()), (401, UNVERIFIED));
    // The refused attempt did not consume the nonce: the genuine body is accepted.
    let r = send(addr, "PUT", "/api/file", &h, body).await;
    assert!(r.verified, "{} {}", r.status, r.body);
    assert_ne!(r.status, 401);
}

#[tokio::test]
async fn oversize_body_gets_pythons_413_before_verification() {
    let dir = tempfile::tempdir().unwrap();
    let addr = start(dir.path()).await;
    let mut h = delegated("v2.1.zz.x");
    h.push(("content-type", "application/json".into()));
    let r = send(
        addr,
        "GET",
        "/api/months",
        &h,
        vec![b' '; 64 * 1024 * 1024 + 1],
    )
    .await;
    assert_eq!(
        (r.status, r.body.as_str()),
        (413, r#"{"detail":"request body too large"}"#)
    );
}

#[tokio::test]
async fn a_verified_decline_says_so_and_unverifiable_requests_are_not_marked() {
    let dir = tempfile::tempdir().unwrap();
    // No managed tenant: the read declines (Python serves _tenant_state's answer).
    let addr = start(dir.path()).await;
    let a = sign(UID, "GET", "/api/months", "", b"", now());
    let r = send(addr, "GET", "/api/months", &delegated(&a), vec![]).await;
    assert_eq!((r.status, r.declined, r.verified), (421, true, true));
    // No tenant header: nothing to verify (Python lets it through unverified).
    let mut h = delegated("");
    h.retain(|(k, _)| *k != "x-cowork-user-id" && *k != "x-cowork-tenant-assertion");
    let r = send(addr, "GET", "/api/months", &h, vec![]).await;
    assert!(!r.verified, "{} {}", r.status, r.body);
}

#[tokio::test]
async fn without_the_verify_header_the_d4_rule_holds() {
    let dir = tempfile::tempdir().unwrap();
    managed_tenant(dir.path());
    let addr = start(dir.path()).await;
    // Unasserted is refused, and nothing is verified (or consumed) here.
    let a = sign(UID, "GET", "/api/months", "", b"", now());
    let mut h = delegated(&a);
    h.retain(|(k, _)| *k != "x-noevia-diary-verify");
    let r = send(addr, "GET", "/api/months", &h, vec![]).await;
    assert_eq!(
        (r.status, r.verified, r.body.as_str()),
        (401, false, UNVERIFIED)
    );
    let r = send(addr, "GET", "/api/months", &delegated(&a), vec![]).await;
    assert_eq!((r.status, r.verified), (200, true));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_replays_of_one_nonce_are_accepted_exactly_once() {
    let dir = tempfile::tempdir().unwrap();
    managed_tenant(dir.path());
    let addr = start(dir.path()).await;
    let a = sign(UID, "GET", "/api/months", "", b"", now());
    let mut tasks = Vec::new();
    for _ in 0..32 {
        let h = delegated(&a);
        tasks.push(tokio::spawn(async move {
            send(addr, "GET", "/api/months", &h, vec![]).await.status
        }));
    }
    let mut ok = 0;
    for t in tasks {
        match t.await.unwrap() {
            200 => ok += 1,
            401 => {}
            other => panic!("unexpected {other}"),
        }
    }
    assert_eq!(ok, 1);
}
