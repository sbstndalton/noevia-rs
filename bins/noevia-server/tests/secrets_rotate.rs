#![cfg(feature = "rust-storage-reads")]
//! Track R12: `POST /api/admin/secrets/rotate` through the real HTTP front with NOEVIA_RUST_SECRETS_ROTATE=1,
//! with the front's real provider, MCP and Google Drive passes, over synthetic keys and QA data: every kind
//! of stored credential sealed under the previous key ends up opening under the current key alone, to the
//! same plaintext, and the report and the audit row say so. Node's own part is replayed byte for byte on
//! recorded rotations in crates/secrets-rotate. A dead legacy upstream makes any proxied request answer 502.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
use noevia_server::{
    config::Config,
    serve::{serve, Limits},
    App,
};
use serde_json::Value;
use server_store::rusqlite::Connection;
use sha2::{Digest, Sha256};
use std::{collections::HashMap, path::Path, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

const A: &str = "aaaaaaaa-7777-4aaa-8aaa-aaaaaaaaaaaa";
const B: &str = "bbbbbbbb-7777-4bbb-8bbb-bbbbbbbbbbbb";
const CURRENT: [u8; 32] = [7; 32];
const PREVIOUS: [u8; 32] = [9; 32];
const ADMIN: &str = "cowork_session=admin-token; cowork_csrf=csrf";
const MEMBER: &str = "cowork_session=member-token; cowork_csrf=csrf";
const ROUTE: &str = "/api/admin/secrets/rotate";

fn hex(s: &str) -> String {
    Sha256::digest(s.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

async fn post_raw(addr: std::net::SocketAddr, cookie: &str) -> (u16, String) {
    let mut s = TcpStream::connect(addr).await.unwrap();
    let head = format!("POST {ROUTE} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nCookie: {cookie}\r\nOrigin: http://{addr}\r\nX-CSRF-Token: csrf\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{{}}");
    s.write_all(head.as_bytes()).await.unwrap();
    let mut bytes = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(60), s.read_to_end(&mut bytes)).await;
    let text = String::from_utf8(bytes).unwrap();
    let (h, b) = text.split_once("\r\n\r\n").unwrap();
    (h.split_whitespace().nth(1).unwrap().parse().unwrap(), b.to_string())
}

async fn post(addr: std::net::SocketAddr, cookie: &str) -> (u16, Value) {
    let (status, body) = post_raw(addr, cookie).await;
    (status, serde_json::from_str(&body).unwrap_or(Value::Null))
}

fn seal(key: &[u8], n: u8, plain: &str, user: Option<&str>) -> String {
    secret_envelope::seal(key, &[n; 12], plain.as_bytes(), user.map(str::as_bytes)).unwrap()
}

fn open_current(value: &str, user: Option<&str>) -> String {
    let units: Vec<u16> = value.encode_utf16().collect();
    let (used, plain) =
        secret_envelope::open(&CURRENT, None, &units, user.map(str::as_bytes)).unwrap();
    assert_eq!(used, secret_envelope::KeyUsed::Current);
    String::from_utf8(plain.to_vec()).unwrap()
}

fn seed(dir: &Path) -> Connection {
    std::fs::write(dir.join("secrets.key"), CURRENT).unwrap();
    std::fs::write(dir.join("secrets.key.previous"), PREVIOUS).unwrap();
    let db = Connection::open(dir.join("cowork.db")).unwrap();
    db.execute_batch(include_str!(
        "../../../crates/server-store/tests/fixtures/node-schema.sql"
    ))
    .unwrap();
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS chatgpt_oauth_tokens(user_id TEXT PRIMARY KEY, data_enc TEXT NOT NULL,
    state TEXT NOT NULL DEFAULT 'connected', updated_at INTEGER NOT NULL)",
    )
    .unwrap();
    let now = noevia_server::identity::now_ms();
    for (uid, role, t) in [(A, "admin", "admin-token"), (B, "member", "member-token")] {
        db.execute("INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,created_at,updated_at) VALUES(?1,?1,?1,'Synthetic QA',?2,'x',?1,1,1)", (uid, role)).unwrap();
        db.execute(
            "INSERT INTO sessions VALUES(?1,?2,?3,?4,?4,?5,'ua','127.0.0.1')",
            (hex(t), uid, hex("csrf"), now, now + 86400000),
        )
        .unwrap();
        std::fs::create_dir_all(dir.join("users").join(uid)).unwrap();
    }
    // Every kind of stored credential, sealed under the previous key (or not at all).
    db.execute(
        "INSERT INTO storage_connections(user_id,kind,secret,updated_at) VALUES(?1,'webdav',?2,1)",
        (A, seal(&PREVIOUS, 1, "qa-storage-a", Some(A))),
    )
    .unwrap();
    db.execute(
        "INSERT INTO storage_connections(user_id,kind,secret,updated_at) VALUES(?1,'webdav','qa-storage-b-plain',1)",
        [B],
    )
    .unwrap();
    db.execute(
        "INSERT INTO chatgpt_oauth_tokens(user_id,data_enc,updated_at) VALUES(?1,?2,1)",
        (
            B,
            seal(
                &PREVIOUS,
                2,
                r#"{"access":"qa-a","refresh":"qa-r"}"#,
                Some(B),
            ),
        ),
    )
    .unwrap();
    let shared = serde_json::json!({"providers": [{"id": "qa-shared", "label": "QA shared", "baseUrl": "http://qa-provider:1/v1", "apiKey": seal(&PREVIOUS, 3, "qa-provider-key", None)}]});
    std::fs::write(
        dir.join("shared-providers.json"),
        serde_json::to_string_pretty(&shared).unwrap(),
    )
    .unwrap();
    let drive_key = google_token_seal::derive_secret_key(&PREVIOUS, google_token_seal::USER_LABEL);
    let sealed = google_token_seal::Sealer::new(drive_key.as_slice())
        .unwrap()
        .seal(br#"{"refresh":"qa-drive"}"#, &[4; 12])
        .unwrap();
    std::fs::create_dir_all(dir.join("google-drive-users")).unwrap();
    std::fs::write(
        dir.join("google-drive-users").join(format!("{A}.sealed")),
        sealed,
    )
    .unwrap();
    db
}

async fn front(dir: &Path, extra: &[(&str, &str)]) -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut env: HashMap<&str, String> = HashMap::from([
        ("NOEVIA_LEGACY_UPSTREAM", "http://127.0.0.1:9".into()),
        ("UI_DATA_DIR", dir.display().to_string()),
        ("PUBLIC_ORIGIN", format!("http://{addr}")),
    ]);
    for (k, v) in extra {
        env.insert(k, (*v).to_string());
    }
    let app = App::new(Config::from_lookup(|k| env.get(k).cloned()).unwrap());
    tokio::spawn(serve(
        listener,
        Arc::clone(&app),
        Limits::default(),
        std::future::pending(),
    ));
    addr
}

fn table<'a>(report: &'a Value, name: &str) -> &'a Value {
    &report["tables"][name]
}

#[tokio::test]
async fn the_front_rotates_every_credential_in_one_request() {
    let d = tempfile::tempdir().unwrap();
    let db = seed(d.path());
    // Switch unset, or any value but 1: Node's (the dead upstream).
    for extra in [&[][..], &[("NOEVIA_RUST_SECRETS_ROTATE", "true")][..]] {
        let addr = front(d.path(), extra).await;
        assert_eq!(post(addr, ADMIN).await.0, 502);
    }
    let addr = front(d.path(), &[("NOEVIA_RUST_SECRETS_ROTATE", "1")]).await;
    assert_eq!(post(addr, "").await.0, 401);
    assert_eq!(
        post(addr, MEMBER).await,
        (403, serde_json::json!({"error": "administrator required"}))
    );
    let (status, report_text) = post_raw(addr, ADMIN).await;
    let report: Value = serde_json::from_str(&report_text).unwrap();
    assert_eq!(status, 200, "{report}");
    let counts = |c: [u64; 5]| serde_json::json!({"current": c[0], "rotated": c[1], "upgraded": c[2], "empty": c[3], "failed": c[4]});
    assert_eq!(
        table(&report, "storage_connections"),
        &counts([0, 1, 1, 0, 0])
    );
    assert_eq!(
        table(&report, "chatgpt_oauth_tokens"),
        &counts([0, 1, 0, 0, 0])
    );
    // Re-sealed by the Drive pass, then checked: counted as rotated, as in Node.
    assert_eq!(
        table(&report, "google_drive_users"),
        &counts([0, 1, 0, 0, 0])
    );
    assert_eq!(table(&report, "shared_providers"), &counts([0, 1, 0, 0, 0]));
    for t in [
        "mcp_oauth_tokens",
        "mcp_oauth_clients",
        "directory_mcp_user_keys",
        "directory_mcp_servers",
    ] {
        assert_eq!(table(&report, t), &counts([0; 5]), "{t}");
    }
    assert_eq!(report["failures"], serde_json::json!([]));
    assert_eq!(report["previousKey"], true);
    // Node's table order (read from the reply's text: serde_json's map would sort the keys).
    let order: Vec<String> = match js_json::wtf16::parse(&report_text).unwrap().get("tables") {
        js_json::JValue::Obj(items) => items.iter().map(|(k, _)| k.clone()).collect(),
        _ => panic!("tables"),
    };
    let at = |name: &str| order.iter().position(|n| n == name).unwrap();
    assert_eq!(order[..3], ["storage_connections", "chatgpt_oauth_tokens", "google_drive_users"]);
    assert!(at("google_drive_users") < at("shared_providers"));
    assert!(at("shared_providers") < at("mcp_oauth_tokens"));
    assert_eq!(order[order.len() - 4..], ["mcp_oauth_tokens", "mcp_oauth_clients", "directory_mcp_user_keys", "directory_mcp_servers"]);
    // Every value now opens under the current key alone, to what it held.
    let secret = |u: &str| -> String {
        db.query_row(
            "SELECT secret FROM storage_connections WHERE user_id=?1",
            [u],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(open_current(&secret(A), Some(A)), "qa-storage-a");
    assert_eq!(open_current(&secret(B), Some(B)), "qa-storage-b-plain");
    let tokens: String = db
        .query_row(
            "SELECT data_enc FROM chatgpt_oauth_tokens WHERE user_id=?1",
            [B],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        open_current(&tokens, Some(B)),
        r#"{"access":"qa-a","refresh":"qa-r"}"#
    );
    let shared: Value = serde_json::from_str(
        &std::fs::read_to_string(d.path().join("shared-providers.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        open_current(shared["providers"][0]["apiKey"].as_str().unwrap(), None),
        "qa-provider-key"
    );
    let drive_key = google_token_seal::derive_secret_key(&CURRENT, google_token_seal::USER_LABEL);
    let file = std::fs::read_to_string(
        d.path()
            .join("google-drive-users")
            .join(format!("{A}.sealed")),
    )
    .unwrap();
    let plain = google_token_seal::Sealer::new(drive_key.as_slice())
        .unwrap()
        .open(&file)
        .unwrap();
    assert_eq!(plain.as_slice(), br#"{"refresh":"qa-drive"}"#);
    // The audit row, as core's: actor and target the administrator, the totals and no secret.
    let (action, actor, target, detail): (String, String, String, String) = db
        .query_row(
            "SELECT action, actor_user_id, target_user_id, detail FROM audit_events WHERE action='secrets.rotate'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(
        (action.as_str(), actor.as_str(), target.as_str()),
        ("secrets.rotate", A, A)
    );
    let detail: Value = serde_json::from_str(&detail).unwrap();
    assert_eq!(detail["totals"], report["totals"]);
    assert_eq!(detail["failed"], serde_json::json!([]));
    for text in [report.to_string(), detail.to_string()] {
        assert!(
            !text.contains("qa-storage")
                && !text.contains("qa-provider-key")
                && !text.contains("enc:v")
        );
    }
    // A second rotation finds everything current.
    let (status, again) = post(addr, ADMIN).await;
    assert_eq!(status, 200);
    assert_eq!(again["totals"]["rotated"], 0, "{again}");
    assert_eq!(again["totals"]["failed"], 0, "{again}");
}

/// A client that goes away right after sending does not stop the rotation: it runs to its end, with
/// its audit row.
#[tokio::test]
async fn a_rotation_whose_client_leaves_still_finishes() {
    let d = tempfile::tempdir().unwrap();
    let db = seed(d.path());
    let addr = front(d.path(), &[("NOEVIA_RUST_SECRETS_ROTATE", "1")]).await;
    {
        let mut s = TcpStream::connect(addr).await.unwrap();
        let head = format!("POST {ROUTE} HTTP/1.1\r\nHost: {addr}\r\nCookie: {ADMIN}\r\nOrigin: http://{addr}\r\nX-CSRF-Token: csrf\r\nContent-Length: 0\r\n\r\n");
        s.write_all(head.as_bytes()).await.unwrap();
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        let rows: i64 = db
            .query_row("SELECT COUNT(*) FROM audit_events WHERE action='secrets.rotate'", [], |r| r.get(0))
            .unwrap();
        if rows == 1 {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "no audit row");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let secret: String = db
        .query_row("SELECT secret FROM storage_connections WHERE user_id=?1", [A], |r| r.get(0))
        .unwrap();
    assert_eq!(open_current(&secret, Some(A)), "qa-storage-a");
}
