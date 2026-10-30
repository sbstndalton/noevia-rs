//! The provider registry through the real front (sbstndalton/noevia#2190 round 6): GET|POST
//! /api/providers, PUT|DELETE /api/providers/{id} and DELETE /api/providers/chatgpt, with the
//! Rust front the only writer of providers.json and shared-providers.json. Synthetic accounts,
//! keys and addresses only; the legacy upstream is dead, so a request that fell through to Node
//! would answer 502. MEMBER_OUTBOUND_ORIGINS names one synthetic origin for the whole binary, so a
//! member is refused every other. No real account, key or credential.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    dead_code
)]
use noevia_server::{
    config::Config,
    project_store,
    serve::{serve, Limits},
    App,
};
use server_store::rusqlite::Connection;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{Arc, Once};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const A: &str = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
const B: &str = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";
const ADMIN: &str = "cowork_session=admin-token; cowork_csrf=csrf";
const MEMBER: &str = "cowork_session=member-token; cowork_csrf=csrf";
const KEY: [u8; 32] = [7; 32];
const OTHER_KEY: [u8; 32] = [9; 32];
const STORE_TOKEN: &str = "synthetic-store-token-0123456789abcdef";

static ENV: Once = Once::new();

/// The deployment's default row (core index.cjs reads these from the process environment).
fn env_once() {
    ENV.call_once(|| {
        std::env::set_var("DEFAULT_PROVIDER_LABEL", "Synthetic engine");
        std::env::set_var("INFERENCE_BASE_URL", "http://127.0.0.1:9");
        std::env::set_var("INFERENCE_API_KEY", "sk-default-secret-0000");
        std::env::set_var("MODEL_MANAGER_KIND", "llamacpp");
        // The one origin the operator lets members connect (core ssrf.cjs): exact origins only.
        std::env::set_var("MEMBER_OUTBOUND_ORIGINS", "https://approved.example.com");
    });
}

fn hex(s: &str) -> String {
    Sha256::digest(s.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn j(s: &str) -> serde_json::Value {
    serde_json::from_str(s).unwrap_or_else(|_| panic!("not JSON: {s}"))
}

fn request(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    cookie: &str,
    body: &str,
) -> String {
    let csrf = if method == "GET" {
        String::new()
    } else {
        format!("Origin: http://{addr}\r\nX-CSRF-Token: csrf\r\n")
    };
    format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nCookie: {cookie}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{csrf}\r\n{body}", body.len())
}

async fn call(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    cookie: &str,
    body: &str,
) -> (u16, serde_json::Value) {
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(request(addr, method, path, cookie, body).as_bytes())
        .await
        .unwrap();
    let mut bytes = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(40), s.read_to_end(&mut bytes)).await;
    let text = String::from_utf8(bytes).unwrap();
    let (h, b) = text.split_once("\r\n\r\n").unwrap();
    let status = h.split_whitespace().nth(1).unwrap().parse().unwrap();
    (
        status,
        serde_json::from_str(b).unwrap_or(serde_json::Value::Null),
    )
}

struct Front {
    addr: std::net::SocketAddr,
    store: std::net::SocketAddr,
    dir: std::path::PathBuf,
}

fn seed(dir: &std::path::Path) -> Connection {
    std::fs::write(dir.join("secrets.key"), KEY).unwrap();
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
        db.execute("INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,created_at,updated_at) VALUES(?1,?1,?1,'Synthetic',?2,'x',?1,1,1)", (uid, role)).unwrap();
        db.execute(
            "INSERT INTO sessions VALUES(?1,?2,?3,?4,?4,?5,'ua','127.0.0.1')",
            (hex(t), uid, hex("csrf"), now, now + 86400000),
        )
        .unwrap();
        std::fs::create_dir_all(dir.join("users").join(uid)).unwrap();
    }
    db
}

async fn front(dir: &std::path::Path, flag: &str) -> Front {
    env_once();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let env = HashMap::from([
        ("NOEVIA_LEGACY_UPSTREAM", "http://127.0.0.1:9".to_string()),
        ("UI_DATA_DIR", dir.display().to_string()),
        ("PUBLIC_ORIGIN", format!("http://{addr}")),
        ("NOEVIA_FEATURE_CHATGPT_OAUTH", flag.to_string()),
    ]);
    let app = App::new(Config::from_lookup(|k| env.get(k).cloned()).unwrap());
    tokio::spawn(serve(
        listener,
        Arc::clone(&app),
        Limits::default(),
        std::future::pending(),
    ));
    let sl = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let store_addr = sl.local_addr().unwrap();
    let s = project_store::Store {
        data_dir: dir.to_path_buf(),
        token: STORE_TOKEN.into(),
        switch: server_store::RustProjects::owned(),
        start_id: "".into(),
    };
    tokio::spawn(project_store::serve_with_context(
        sl,
        s,
        None,
        Some(app),
        std::future::pending(),
    ));
    Front {
        addr,
        store: store_addr,
        dir: dir.to_path_buf(),
    }
}

/// One internal store call (`/v1/<op>`).
async fn store(addr: std::net::SocketAddr, op: &str, body: &str) -> (u16, serde_json::Value) {
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(format!("POST /v1/{op} HTTP/1.1\r\nhost: store\r\nconnection: close\r\ncontent-type: application/json\r\ncontent-length: {}\r\n{}: {STORE_TOKEN}\r\n\r\n{body}", body.len(), project_store::TOKEN_HEADER).as_bytes()).await.unwrap();
    let mut out = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(40), s.read_to_end(&mut out)).await;
    let text = String::from_utf8_lossy(&out).into_owned();
    let status = text[9..12].parse().unwrap();
    let body = text.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("");
    (
        status,
        serde_json::from_str(body).unwrap_or(serde_json::Value::Null),
    )
}

fn seal_with(key: &[u8; 32], plain: &str, n: u8) -> String {
    secret_envelope::seal(key, &[n; 12], plain.as_bytes(), None).unwrap()
}

fn seal_v1(plain: &str, n: u8) -> String {
    seal_with(&KEY, plain, n)
}

fn open_v1(value: &str) -> String {
    let units: Vec<u16> = value.encode_utf16().collect();
    let (_, plain) = secret_envelope::open(&KEY, None, &units, None).unwrap();
    String::from_utf8(plain.to_vec()).unwrap()
}

fn write_rows(path: &std::path::Path, rows: &serde_json::Value) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        path,
        serde_json::to_string_pretty(&serde_json::json!({ "providers": rows })).unwrap(),
    )
    .unwrap();
}

fn private_path(f: &Front, user: &str) -> std::path::PathBuf {
    f.dir.join("users").join(user).join("providers.json")
}

fn shared_path(f: &Front) -> std::path::PathBuf {
    f.dir.join("shared-providers.json")
}

fn read_rows(path: &std::path::Path) -> Vec<serde_json::Value> {
    match std::fs::read_to_string(path) {
        Ok(t) => j(&t)["providers"].as_array().cloned().unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

fn find<'a>(list: &'a serde_json::Value, id: &str) -> &'a serde_json::Value {
    list["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == id)
        .unwrap_or_else(|| panic!("no row {id} in {list}"))
}

fn token_row(db: &Connection, user: &str) -> Option<String> {
    db.query_row(
        "SELECT state FROM chatgpt_oauth_tokens WHERE user_id=?1",
        [user],
        |r| r.get(0),
    )
    .ok()
}

fn store_tokens(db: &Connection, user: &str, state: &str) {
    let json = r#"{"accessToken":"synthetic-access","refreshToken":"synthetic-refresh","accountId":"acct-synthetic","fedRamp":false,"expiresAt":4102444800000,"email":"ada@example.test","plan":"plus","connectedAt":1700000000000}"#;
    let sealed =
        secret_envelope::seal(&KEY, &[3; 12], json.as_bytes(), Some(user.as_bytes())).unwrap();
    db.execute(
        "INSERT OR REPLACE INTO chatgpt_oauth_tokens(user_id,data_enc,state,updated_at) VALUES(?1,?2,?3,1)",
        (user, sealed, state),
    )
    .unwrap();
}

fn audits(db: &Connection, user: &str) -> Vec<String> {
    let mut st = db
        .prepare("SELECT action FROM audit_events WHERE target_user_id=?1 AND action LIKE 'chatgpt.%' ORDER BY id")
        .unwrap();
    st.query_map([user], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_listing_is_the_merged_view_as_core_shows_it() {
    let d = tempfile::tempdir().unwrap();
    let db = seed(d.path());
    let f = front(d.path(), "0").await;
    write_rows(
        &shared_path(&f),
        &serde_json::json!([
            {"id":"default","label":"stale","baseUrl":"http://stale.test","apiKey":"","shared":true},
            {"id":"team","label":"Team","baseUrl":"http://127.0.0.1:1/v1","apiKey":seal_v1("sk-shared-aaaa1111", 1),
             "defaultModel":"team-model","shared":true,"contextTokens":8192,"capabilities":{"jsonSchemaParam":true}},
        ]),
    );
    write_rows(
        &private_path(&f, A),
        &serde_json::json!([
            {"id":"mine","label":"Mine","baseUrl":"https://api.openai.com/v1","apiKey":seal_v1("sk-synthetic-12345678", 2),"defaultModel":"","shared":false},
            {"id":"short","label":"Short","baseUrl":"http://127.0.0.1:2","apiKey":seal_v1("tiny", 3),"shared":false,"contextTokens":10},
            {"id":"local","label":"Local","baseUrl":"http://127.0.0.1:3","apiKey":seal_v1("local", 4),"shared":false,"capabilities":{}},
            {"id":"lost","label":"Lost","baseUrl":"http://127.0.0.1:4","apiKey":seal_with(&OTHER_KEY, "sk-from-another-key", 5),"shared":false},
            {"id":"trial","label":"Trial","baseUrl":"https://integrate.api.nvidia.com/v1","apiKey":"","shared":false},
            {"id":"lemonade","label":"Old name","baseUrl":"http://old.test","apiKey":"","shared":false},
            {"id":"mine","label":"Duplicate","baseUrl":"http://dup.test","apiKey":"","shared":false},
            {"id":"flagged","label":"Flagged shared","baseUrl":"http://127.0.0.1:5","apiKey":"","shared":true},
        ]),
    );
    write_rows(
        &private_path(&f, B),
        &serde_json::json!([{"id":"theirs","label":"Theirs","baseUrl":"http://127.0.0.1:6","apiKey":"","shared":false}]),
    );
    let (status, list) = call(f.addr, "GET", "/api/providers", ADMIN, "").await;
    assert_eq!(status, 200, "{list}");
    let ids: Vec<&str> = list["providers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["id"].as_str().unwrap())
        .collect();
    // The default first, the shared rows, then the account's own (a `lemonade` row is the
    // default and so dropped, the first of each id wins, a shared-flagged private row is not shown).
    assert_eq!(
        ids,
        ["default", "team", "mine", "short", "local", "lost", "trial"]
    );
    // The default row is the environment's: managed by llama.cpp, its key masked, the engine's
    // capabilities, never the key.
    assert_eq!(
        find(&list, "default"),
        &j(
            r#"{"id":"default","label":"Synthetic engine","baseUrl":"http://127.0.0.1:9","apiKeyMasked":"…0000","isDefault":true,"managed":true,"shared":true,"capabilities":{"jsonSchemaParam":true}}"#
        )
    );
    assert_eq!(
        find(&list, "team"),
        &j(
            r#"{"id":"team","label":"Team","baseUrl":"http://127.0.0.1:1/v1","apiKeyMasked":"…1111","isDefault":false,"managed":false,"shared":true,"defaultModel":"team-model","contextTokens":8192,"capabilities":{"jsonSchemaParam":true}}"#
        )
    );
    // A stored row without capabilities points at the OpenAI preset's address: its definition.
    assert_eq!(
        find(&list, "mine"),
        &j(
            r#"{"id":"mine","label":"Mine","baseUrl":"https://api.openai.com/v1","apiKeyMasked":"…5678","isDefault":false,"managed":false,"shared":false,"capabilities":{"tokenBudgetField":"max_completion_tokens","reasoningEffortParam":true,"reasoningEffortModels":["gpt-5.4"]}}"#
        )
    );
    // A short key shows no characters, a context window out of bounds is not shown, `local` is
    // no key, and capabilities of {} are never overridden.
    assert_eq!(find(&list, "short")["apiKeyMasked"], "••••");
    assert!(find(&list, "short").get("contextTokens").is_none());
    assert!(find(&list, "local")["apiKeyMasked"].is_null());
    assert!(find(&list, "local").get("capabilities").is_none());
    // A key no available secrets key opens: usable row, flagged, nothing to mask.
    assert_eq!(find(&list, "lost")["keyUnreadable"], true);
    assert!(find(&list, "lost")["apiKeyMasked"].is_null());
    assert_eq!(find(&list, "trial")["external"], true);
    assert!(find(&list, "mine").get("external").is_none());
    // The key never appears anywhere in the answer.
    let text = list.to_string();
    for secret in [
        "sk-shared-aaaa1111",
        "sk-synthetic-12345678",
        "sk-default-secret-0000",
        "enc:v1",
    ] {
        assert!(!text.contains(secret), "{secret} leaked");
    }
    // A member sees the shared rows and its own, never another account's.
    let (_, member) = call(f.addr, "GET", "/api/providers", MEMBER, "").await;
    let ids: Vec<&str> = member["providers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["default", "team", "theirs"]);
    // Signed out: the gate's 401, from the front.
    assert_eq!(call(f.addr, "GET", "/api/providers", "", "").await.0, 401);
    drop(db);
}

#[tokio::test]
async fn chatgpt_rows_follow_the_flag_and_the_brokers_state() {
    let d = tempfile::tempdir().unwrap();
    let db = seed(d.path());
    let off = front(d.path(), "0").await;
    write_rows(
        &private_path(&off, A),
        &serde_json::json!([
            {"id":"chatgpt-oauth","kind":"chatgpt-oauth","label":"ChatGPT","baseUrl":"https://chatgpt.com/backend-api/codex","apiKey":"","defaultModel":"","shared":false,"external":true},
        ]),
    );
    store_tokens(&db, A, "connected");
    // Flag off: the row is not listed.
    let (_, list) = call(off.addr, "GET", "/api/providers", ADMIN, "").await;
    assert!(list["providers"]
        .as_array()
        .unwrap()
        .iter()
        .all(|p| p["id"] != "chatgpt-oauth"));
    let on = front(d.path(), "1").await;
    let (_, list) = call(on.addr, "GET", "/api/providers", ADMIN, "").await;
    assert_eq!(
        find(&list, "chatgpt-oauth"),
        &j(
            r#"{"id":"chatgpt-oauth","label":"ChatGPT","baseUrl":"https://chatgpt.com/backend-api/codex","apiKeyMasked":null,"isDefault":false,"managed":false,"shared":false,"external":true,"kind":"chatgpt-oauth","connection":"connected"}"#
        )
    );
    store_tokens(&db, A, "reconnect");
    let (_, list) = call(on.addr, "GET", "/api/providers", ADMIN, "").await;
    assert_eq!(find(&list, "chatgpt-oauth")["connection"], "reconnect");
    // A shared row of that kind is never listed.
    write_rows(
        &shared_path(&on),
        &serde_json::json!([{"id":"chatgpt-oauth","kind":"chatgpt-oauth","label":"x","baseUrl":"http://x.test","apiKey":"","shared":true}]),
    );
    let (_, member) = call(on.addr, "GET", "/api/providers", MEMBER, "").await;
    assert!(member["providers"]
        .as_array()
        .unwrap()
        .iter()
        .all(|p| p["id"] != "chatgpt-oauth"));
}

#[tokio::test]
async fn a_token_row_without_a_provider_row_gets_it_at_the_listing() {
    let d = tempfile::tempdir().unwrap();
    let db = seed(d.path());
    let f = front(d.path(), "1").await;
    // No tokens, no row: nothing is added.
    let (_, list) = call(f.addr, "GET", "/api/providers", ADMIN, "").await;
    assert!(list["providers"]
        .as_array()
        .unwrap()
        .iter()
        .all(|p| p["id"] != "chatgpt-oauth"));
    assert!(read_rows(&private_path(&f, A)).is_empty());
    // A sign-in the broker saved whose row was not written (or predates this build).
    store_tokens(&db, A, "connected");
    let (_, list) = call(f.addr, "GET", "/api/providers", ADMIN, "").await;
    assert_eq!(find(&list, "chatgpt-oauth")["connection"], "connected");
    let rows = read_rows(&private_path(&f, A));
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0],
        j(
            r#"{"id":"chatgpt-oauth","kind":"chatgpt-oauth","label":"ChatGPT","baseUrl":"https://chatgpt.com/backend-api/codex","apiKey":"","defaultModel":"","shared":false,"external":true}"#
        )
    );
    // Listed again: still one row.
    call(f.addr, "GET", "/api/providers", ADMIN, "").await;
    assert_eq!(read_rows(&private_path(&f, A)).len(), 1);
    // The other account has none.
    assert!(read_rows(&private_path(&f, B)).is_empty());
}

#[tokio::test]
async fn connecting_follows_core_in_order_and_words() {
    let d = tempfile::tempdir().unwrap();
    let _db = seed(d.path());
    let f = front(d.path(), "0").await;
    let post = |cookie: &'static str, body: &'static str| {
        let addr = f.addr;
        async move { call(addr, "POST", "/api/providers", cookie, body).await }
    };
    let err = |s: u16, m: &str| (s, j(&format!(r#"{{"error":"{m}"}}"#)));
    assert_eq!(post(ADMIN, "{oops").await, err(400, "invalid JSON"));
    assert_eq!(post(ADMIN, "").await, err(400, "invalid JSON"));
    assert_eq!(
        post(ADMIN, "[]").await,
        err(400, "request body must be a JSON object")
    );
    assert_eq!(
        post(ADMIN, "null").await,
        err(400, "request body must be a JSON object")
    );
    // The shared rule comes before the fields.
    assert_eq!(
        post(MEMBER, r#"{"shared":true}"#).await,
        err(403, "administrator required for shared providers")
    );
    assert_eq!(
        post(ADMIN, r#"{"label":"  "}"#).await,
        err(400, "label required")
    );
    assert_eq!(
        post(ADMIN, r#"{"label":"x","contextTokens":5}"#).await,
        err(
            400,
            "contextTokens must be a whole number from 2048 to 2000000"
        )
    );
    assert_eq!(
        post(ADMIN, r#"{"label":"x","baseUrl":"ftp://x.test"}"#).await,
        err(400, "baseUrl must be an http(s) URL")
    );
    let long = format!(r#"{{"label":"x","baseUrl":"http://{}"}}"#, "a".repeat(2050));
    assert_eq!(
        call(f.addr, "POST", "/api/providers", ADMIN, &long).await,
        err(400, "baseUrl is too long")
    );
    // A member may not register an address the server reaches only from inside.
    assert_eq!(
        post(MEMBER, r#"{"label":"x","baseUrl":"http://10.0.0.1/v1"}"#).await,
        err(
            400,
            "Provider origin is not approved for member connections; contact an administrator."
        )
    );
    assert_eq!(
        post(ADMIN, r#"{"label":"x","baseUrl":"http://u:p@10.0.0.1/v1"}"#).await,
        err(
            400,
            "Provider origin is not approved for member connections; contact an administrator."
        )
    );
    assert_eq!(
        post(
            ADMIN,
            r#"{"label":"x","baseUrl":"http://10.0.0.1/v1","capabilities":{"nope":1}}"#
        )
        .await,
        err(400, "unknown capability: nope")
    );
    assert_eq!(
        post(
            ADMIN,
            r#"{"label":"x","baseUrl":"http://10.0.0.1/v1","capabilities":[]}"#
        )
        .await,
        err(400, "capabilities must be an object")
    );
    // Nothing was written by any refusal.
    assert!(!private_path(&f, A).exists());
    assert!(!shared_path(&f).exists());

    // The row: the address without trailing slashes, the key sealed, the answer without the key.
    let (status, made) = post(
        ADMIN,
        r#"{"label":"  Synthetic one  ","baseUrl":" http://127.0.0.1:11/v1// ","apiKey":" sk-synthetic-abcdef123456 ","defaultModel":" model-1 ","contextTokens":"16384","capabilities":{"reasoningEffortParam":true,"tokenBudgetField":"max_tokens"}}"#,
    )
    .await;
    assert_eq!(status, 200, "{made}");
    let id = made["id"].as_str().unwrap().to_owned();
    let (prefix, rest) = id.split_at(5);
    assert_eq!(prefix, "prov-");
    let (ms, random) = rest.split_once('-').unwrap();
    assert!(ms.parse::<u64>().unwrap() > 1_700_000_000_000);
    assert_eq!(random.len(), 6);
    assert!(random
        .bytes()
        .all(|b| b.is_ascii_digit() || b.is_ascii_lowercase()));
    assert_eq!(
        made,
        serde_json::json!({
            "id": id, "label": "Synthetic one", "baseUrl": "http://127.0.0.1:11/v1",
            "defaultModel": "model-1", "apiKeyMasked": "…3456", "contextTokens": 16384
        })
    );
    let rows = read_rows(&private_path(&f, A));
    assert_eq!(rows.len(), 1);
    let on_disk = rows[0].as_object().unwrap();
    // core's key order (serde_json sorts keys, so the order is read from the file's text).
    let text = std::fs::read_to_string(private_path(&f, A)).unwrap();
    let at: Vec<usize> = [
        "\"id\"",
        "\"label\"",
        "\"baseUrl\"",
        "\"apiKey\"",
        "\"defaultModel\"",
        "\"shared\"",
        "\"contextTokens\"",
        "\"capabilities\"",
    ]
    .iter()
    .map(|k| text.find(k).unwrap())
    .collect();
    assert!(at.windows(2).all(|w| w[0] < w[1]), "{text}");
    assert!(on_disk["apiKey"].as_str().unwrap().starts_with("enc:v1:"));
    assert!(!std::fs::read_to_string(private_path(&f, A))
        .unwrap()
        .contains("sk-synthetic-abcdef123456"));
    assert_eq!(
        open_v1(on_disk["apiKey"].as_str().unwrap()),
        "sk-synthetic-abcdef123456"
    );
    assert_eq!(on_disk["shared"], false);
    assert_eq!(on_disk["contextTokens"], 16384);
    assert_eq!(
        on_disk["capabilities"],
        j(r#"{"reasoningEffortParam":true,"tokenBudgetField":"max_tokens"}"#)
    );
    // It is listed for its owner, not for another account.
    let (_, list) = call(f.addr, "GET", "/api/providers", ADMIN, "").await;
    assert_eq!(find(&list, &id)["apiKeyMasked"], "…3456");
    let (_, member) = call(f.addr, "GET", "/api/providers", MEMBER, "").await;
    assert!(member["providers"]
        .as_array()
        .unwrap()
        .iter()
        .all(|p| p["id"] != id.as_str()));

    // A row with no key keeps '' on disk; an administrator may share one with everyone.
    let (status, shared) = post(
        ADMIN,
        r#"{"label":"Team","baseUrl":"http://10.0.0.1:8000/v1","shared":true,"apiKey":"sk-team-9999"}"#,
    )
    .await;
    assert_eq!(status, 200, "{shared}");
    let shared_rows = read_rows(&shared_path(&f));
    assert_eq!(shared_rows.len(), 1);
    assert_eq!(shared_rows[0]["shared"], true);
    assert_eq!(read_rows(&private_path(&f, A)).len(), 1);
    let (_, member) = call(f.addr, "GET", "/api/providers", MEMBER, "").await;
    assert_eq!(
        find(&member, shared["id"].as_str().unwrap())["apiKeyMasked"],
        "…9999"
    );
    let (status, keyless) = post(
        ADMIN,
        r#"{"label":"No key","baseUrl":"http://10.0.0.2/v1"}"#,
    )
    .await;
    assert_eq!(status, 200);
    assert!(keyless["apiKeyMasked"].is_null());
    assert!(keyless.get("contextTokens").is_none());
    let rows = read_rows(&private_path(&f, A));
    assert_eq!(rows[1]["apiKey"], "");
}

#[tokio::test]
async fn editing_follows_core() {
    let d = tempfile::tempdir().unwrap();
    let _db = seed(d.path());
    let f = front(d.path(), "1").await;
    write_rows(
        &shared_path(&f),
        &serde_json::json!([{"id":"team","label":"Team","baseUrl":"http://127.0.0.1:1/v1","apiKey":seal_v1("sk-team-1111", 1),"shared":true}]),
    );
    write_rows(
        &private_path(&f, A),
        &serde_json::json!([
            {"id":"mine","label":"Mine","baseUrl":"http://127.0.0.1:2/v1","apiKey":seal_v1("sk-mine-2222222222", 2),"defaultModel":"m","shared":false,"contextTokens":4096,"capabilities":{"jsonSchemaParam":true}},
            {"id":"other","label":"Other","baseUrl":"http://127.0.0.1:3/v1","apiKey":seal_v1("sk-other-3333333333", 3),"defaultModel":"o","shared":false},
            {"id":"lost","label":"Lost","baseUrl":"http://127.0.0.1:4/v1","apiKey":seal_with(&OTHER_KEY, "sk-lost-4444", 4),"shared":false},
            {"id":"chatgpt-oauth","kind":"chatgpt-oauth","label":"ChatGPT","baseUrl":"https://chatgpt.com/backend-api/codex","apiKey":"","shared":false,"external":true},
            {"id":"ext","label":"Ext","baseUrl":"http://127.0.0.1:5/v1","apiKey":"","shared":false,"external":true},
        ]),
    );
    write_rows(
        &private_path(&f, B),
        &serde_json::json!([{"id":"theirs","label":"Theirs","baseUrl":"http://127.0.0.1:6","apiKey":"","shared":false}]),
    );
    let put = |cookie: &'static str, id: &'static str, body: &'static str| {
        let addr = f.addr;
        async move { call(addr, "PUT", &format!("/api/providers/{id}"), cookie, body).await }
    };
    let err = |s: u16, m: &str| (s, j(&format!(r#"{{"error":"{m}"}}"#)));
    // Refused before the body is read: the same answer for a body that is not JSON.
    assert_eq!(
        put(ADMIN, "default", "{oops").await,
        err(400, "the default provider cannot be edited")
    );
    assert_eq!(
        put(ADMIN, "lemonade", "{oops").await,
        err(400, "the default provider cannot be edited")
    );
    assert_eq!(
        put(ADMIN, "nope", "{oops").await,
        err(404, "no such provider")
    );
    assert_eq!(
        put(ADMIN, "theirs", "{oops").await,
        err(404, "no such provider")
    );
    assert_eq!(
        put(ADMIN, "chatgpt-oauth", "{oops").await,
        err(
            400,
            "this provider is managed by its sign-in and cannot be edited"
        )
    );
    assert_eq!(
        put(ADMIN, "ext", "{oops").await,
        err(
            400,
            "this provider is managed by its sign-in and cannot be edited"
        )
    );
    assert_eq!(
        put(MEMBER, "team", "{oops").await,
        err(403, "administrator required")
    );
    assert_eq!(put(ADMIN, "test", "{}").await, err(404, "no such provider"));
    assert_eq!(
        put(ADMIN, "%E0%A4%A", "{}").await,
        err(400, "no such provider")
    );
    // Then the body.
    assert_eq!(put(ADMIN, "mine", "{oops").await, err(400, "invalid JSON"));
    assert_eq!(put(ADMIN, "mine", "[]").await, err(400, "invalid JSON"));
    assert_eq!(put(ADMIN, "mine", "null").await, err(400, "invalid JSON"));
    for field in ["label", "baseUrl", "apiKey", "defaultModel"] {
        let body = format!(r#"{{"{field}":5}}"#);
        assert_eq!(
            call(f.addr, "PUT", "/api/providers/mine", ADMIN, &body).await,
            err(400, &format!("{field} must be a string"))
        );
    }
    assert_eq!(
        put(ADMIN, "mine", r#"{"label":""}"#).await,
        err(400, "label required")
    );
    assert_eq!(
        put(ADMIN, "mine", r#"{"baseUrl":"gopher://x"}"#).await,
        err(400, "baseUrl must be an http(s) URL")
    );
    assert_eq!(
        put(ADMIN, "mine", r#"{"contextTokens":1}"#).await,
        err(
            400,
            "contextTokens must be a whole number from 2048 to 2000000"
        )
    );
    assert_eq!(
        put(ADMIN, "mine", r#"{"capabilities":{"x":1}}"#).await,
        err(400, "unknown capability: x")
    );
    // A stored key never follows the row to another origin.
    assert_eq!(
        put(ADMIN, "mine", r#"{"baseUrl":"http://127.0.0.1:99/v1"}"#).await,
        err(
            400,
            "Enter the API key again when moving a provider to a different address."
        )
    );
    // Every refusal left the file as it was.
    let before = read_rows(&private_path(&f, A));
    assert_eq!(before[0]["label"], "Mine");
    assert_eq!(before[0]["baseUrl"], "http://127.0.0.1:2/v1");

    // A new address with a new key: moved, resealed, answered masked.
    let (status, moved) = put(
        ADMIN,
        "mine",
        r#"{"label":"  Renamed ","baseUrl":"http://127.0.0.1:99/v1/","apiKey":"sk-new-key-0123456789","defaultModel":"m2"}"#,
    )
    .await;
    assert_eq!(status, 200, "{moved}");
    assert_eq!(
        moved,
        j(
            r#"{"id":"mine","label":"Renamed","baseUrl":"http://127.0.0.1:99/v1","apiKeyMasked":"…6789","isDefault":false,"managed":false,"shared":false,"defaultModel":"m2","contextTokens":4096,"capabilities":{"jsonSchemaParam":true}}"#
        )
    );
    let rows = read_rows(&private_path(&f, A));
    assert_eq!(
        open_v1(rows[0]["apiKey"].as_str().unwrap()),
        "sk-new-key-0123456789"
    );
    // The other rows are written back as they were.
    assert_eq!(rows[1], before[1]);
    assert_eq!(rows[2], before[2], "an unreadable key keeps its ciphertext");
    assert_eq!(rows[3], before[3]);

    // Omitted fields keep their values; the key is kept when none is typed (same origin).
    let (status, kept) = put(ADMIN, "mine", r#"{"label":"Again"}"#).await;
    assert_eq!(status, 200);
    assert_eq!(kept["label"], "Again");
    assert_eq!(kept["apiKeyMasked"], "…6789");
    assert_eq!(kept["contextTokens"], 4096);
    // The same origin, another path, keeps the key too.
    let (status, same) = put(ADMIN, "mine", r#"{"baseUrl":"http://127.0.0.1:99/other"}"#).await;
    assert_eq!(status, 200, "{same}");
    assert_eq!(same["apiKeyMasked"], "…6789");
    // null or '' clears the window; capabilities null returns to the preset default, {} replaces.
    let (_, cleared) = put(
        ADMIN,
        "mine",
        r#"{"contextTokens":null,"capabilities":null}"#,
    )
    .await;
    assert!(cleared.get("contextTokens").is_none());
    assert!(cleared.get("capabilities").is_none());
    let row = read_rows(&private_path(&f, A))[0].clone();
    assert!(row.get("contextTokens").is_none());
    assert!(row.get("capabilities").is_none());
    let (_, replaced) = put(
        ADMIN,
        "mine",
        r#"{"contextTokens":"2048","capabilities":{"tokenBudgetField":"max_completion_tokens"}}"#,
    )
    .await;
    assert_eq!(replaced["contextTokens"], 2048);
    assert_eq!(
        replaced["capabilities"],
        j(r#"{"tokenBudgetField":"max_completion_tokens"}"#)
    );
    // An empty key field does not clear the key (omitted and '' both keep it).
    let (_, still) = put(ADMIN, "mine", r#"{"apiKey":""}"#).await;
    assert_eq!(still["apiKeyMasked"], "…6789");

    // A key nothing opens is kept for its own origin and forgotten when the row moves.
    let (_, lost) = put(ADMIN, "lost", r#"{"label":"Lost too"}"#).await;
    assert_eq!(lost["keyUnreadable"], true);
    assert_eq!(
        read_rows(&private_path(&f, A))[2]["apiKey"],
        before[2]["apiKey"]
    );
    let (status, moved_lost) = put(ADMIN, "lost", r#"{"baseUrl":"http://127.0.0.1:98/v1"}"#).await;
    assert_eq!(status, 200, "{moved_lost}");
    assert!(moved_lost.get("keyUnreadable").is_none());
    assert_eq!(read_rows(&private_path(&f, A))[2]["apiKey"], "");

    // A shared row: an administrator edits it in the shared file; a member cannot.
    let (status, team) = put(ADMIN, "team", r#"{"label":"Team two"}"#).await;
    assert_eq!(status, 200, "{team}");
    assert_eq!(read_rows(&shared_path(&f))[0]["label"], "Team two");
    assert_eq!(
        open_v1(read_rows(&shared_path(&f))[0]["apiKey"].as_str().unwrap()),
        "sk-team-1111"
    );
    // Another account's private file is untouched by any of it.
    assert_eq!(read_rows(&private_path(&f, B))[0]["label"], "Theirs");
}

#[tokio::test]
async fn removing_follows_core_and_projects_fall_back() {
    let d = tempfile::tempdir().unwrap();
    let _db = seed(d.path());
    let f = front(d.path(), "1").await;
    write_rows(
        &shared_path(&f),
        &serde_json::json!([
            {"id":"team","label":"Team","baseUrl":"http://127.0.0.1:1/v1","apiKey":"","shared":true},
            {"id":"team2","label":"Team 2","baseUrl":"http://127.0.0.1:1/v1","apiKey":"","shared":true},
        ]),
    );
    write_rows(
        &private_path(&f, A),
        &serde_json::json!([
            {"id":"mine","label":"Mine","baseUrl":"http://127.0.0.1:2/v1","apiKey":seal_v1("sk-mine-2222222222", 2),"shared":false},
            {"id":"keep","label":"Keep","baseUrl":"http://127.0.0.1:3/v1","apiKey":seal_v1("sk-keep-3333333333", 3),"shared":false},
        ]),
    );
    let projects = r#"{"projects":[{"id":"p1","name":"On mine","provider":"mine","model":"m"},{"id":"p2","name":"On keep","provider":"keep","model":"k"},{"id":"p3","name":"Default"}]}"#;
    std::fs::write(f.dir.join("users").join(A).join("projects.json"), projects).unwrap();
    let del = |cookie: &'static str, id: &'static str| {
        let addr = f.addr;
        async move { call(addr, "DELETE", &format!("/api/providers/{id}"), cookie, "").await }
    };
    let err = |s: u16, m: &str| (s, j(&format!(r#"{{"error":"{m}"}}"#)));
    assert_eq!(
        del(ADMIN, "default").await,
        err(400, "the default provider cannot be removed")
    );
    assert_eq!(
        del(ADMIN, "lemonade").await,
        err(400, "the default provider cannot be removed")
    );
    assert_eq!(
        del(MEMBER, "team").await,
        err(403, "administrator required")
    );
    assert_eq!(del(ADMIN, "nope").await, err(404, "no such provider"));
    assert_eq!(del(ADMIN, "%zz").await, err(400, "no such provider"));
    // Not another account's row.
    write_rows(
        &private_path(&f, B),
        &serde_json::json!([{"id":"theirs","label":"T","baseUrl":"http://127.0.0.1:6","apiKey":"","shared":false}]),
    );
    assert_eq!(del(ADMIN, "theirs").await, err(404, "no such provider"));
    assert_eq!(read_rows(&private_path(&f, B)).len(), 1);
    // The row goes, the other stays byte for byte, and the projects that used it fall back.
    let keep_before = read_rows(&private_path(&f, A))[1].clone();
    assert_eq!(del(ADMIN, "mine").await, (200, j(r#"{"ok":true}"#)));
    let rows = read_rows(&private_path(&f, A));
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0], keep_before);
    let stored =
        j(&std::fs::read_to_string(f.dir.join("users").join(A).join("projects.json")).unwrap());
    assert_eq!(stored["projects"][0], j(r#"{"id":"p1","name":"On mine"}"#));
    assert_eq!(stored["projects"][1]["provider"], "keep");
    assert_eq!(stored["projects"][2], j(r#"{"id":"p3","name":"Default"}"#));
    assert_eq!(del(ADMIN, "mine").await, err(404, "no such provider"));
    // An administrator removes a shared row from the shared file.
    assert_eq!(del(ADMIN, "team").await, (200, j(r#"{"ok":true}"#)));
    let shared = read_rows(&shared_path(&f));
    assert_eq!(shared.len(), 1);
    assert_eq!(shared[0]["id"], "team2");
}

#[tokio::test]
async fn disconnecting_chatgpt_takes_the_tokens_the_row_and_the_fallbacks() {
    let d = tempfile::tempdir().unwrap();
    let db = seed(d.path());
    let f = front(d.path(), "1").await;
    write_rows(
        &private_path(&f, A),
        &serde_json::json!([
            {"id":"chatgpt-oauth","kind":"chatgpt-oauth","label":"ChatGPT","baseUrl":"https://chatgpt.com/backend-api/codex","apiKey":"","defaultModel":"","shared":false,"external":true},
            {"id":"mine","label":"Mine","baseUrl":"http://127.0.0.1:2/v1","apiKey":"","shared":false},
        ]),
    );
    store_tokens(&db, A, "connected");
    std::fs::write(
        f.dir.join("users").join(A).join("projects.json"),
        r#"{"projects":[{"id":"p1","provider":"chatgpt-oauth","model":"gpt-x"},{"id":"p2","provider":"mine"}]}"#,
    )
    .unwrap();
    // Another account's tokens are not touched.
    store_tokens(&db, B, "connected");
    assert_eq!(
        call(f.addr, "DELETE", "/api/providers/chatgpt", ADMIN, "").await,
        (200, j(r#"{"ok":true,"state":"disconnected"}"#))
    );
    assert_eq!(token_row(&db, A), None);
    assert_eq!(token_row(&db, B).as_deref(), Some("connected"));
    let rows = read_rows(&private_path(&f, A));
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["id"], "mine");
    let stored =
        j(&std::fs::read_to_string(f.dir.join("users").join(A).join("projects.json")).unwrap());
    assert_eq!(stored["projects"][0], j(r#"{"id":"p1"}"#));
    assert_eq!(stored["projects"][1]["provider"], "mine");
    assert!(audits(&db, A).contains(&"chatgpt.oauth.disconnect".to_string()));
    // Again (nothing to disconnect) is still a disconnect.
    assert_eq!(
        call(f.addr, "DELETE", "/api/providers/chatgpt", ADMIN, "")
            .await
            .0,
        200
    );
    // Removing the row by its id is disconnecting too, flag on or off.
    let off = front(d.path(), "0").await;
    write_rows(
        &private_path(&off, B),
        &serde_json::json!([{"id":"chatgpt-oauth","kind":"chatgpt-oauth","label":"ChatGPT","baseUrl":"https://chatgpt.com/backend-api/codex","apiKey":"","shared":false,"external":true}]),
    );
    assert_eq!(
        call(
            off.addr,
            "DELETE",
            "/api/providers/chatgpt-oauth",
            MEMBER,
            ""
        )
        .await,
        (200, j(r#"{"ok":true}"#))
    );
    assert_eq!(token_row(&db, B), None, "its tokens went first");
    assert!(read_rows(&private_path(&off, B)).is_empty());
    // The disconnect route itself is Node's 404 while the flag is off, before any change.
    store_tokens(&db, A, "connected");
    assert_eq!(
        call(off.addr, "DELETE", "/api/providers/chatgpt", ADMIN, "").await,
        (
            404,
            j(r#"{"error":"Sign in with ChatGPT is turned off on this server."}"#)
        )
    );
    assert_eq!(token_row(&db, A).as_deref(), Some("connected"));
}

#[tokio::test]
async fn the_siblings_stay_nodes() {
    let d = tempfile::tempdir().unwrap();
    let _db = seed(d.path());
    let f = front(d.path(), "1").await;
    // The legacy upstream is dead: what Node answers is a 502 from the front's proxy.
    for (m, p) in [
        ("PUT", "/api/providers"),
        ("DELETE", "/api/providers"),
        ("GET", "/api/providers/anything"),
        ("PUT", "/api/providers/chatgpt"),
        ("POST", "/api/providers/chatgpt"),
        ("DELETE", "/api/providers/x/y"),
        ("GET", "/api/providers/"),
        ("DELETE", "/api/providers/"),
    ] {
        let (status, _) = call(f.addr, m, p, ADMIN, "{}").await;
        assert_eq!(status, 502, "{m} {p} is Node's");
    }
}

#[tokio::test]
async fn nothing_is_written_by_the_old_node_path_any_more() {
    // The files change only through the front: a listing never rewrites a file it only reads.
    let d = tempfile::tempdir().unwrap();
    let _db = seed(d.path());
    let f = front(d.path(), "1").await;
    write_rows(
        &private_path(&f, A),
        &serde_json::json!([{"id":"plain","label":"Plain","baseUrl":"http://127.0.0.1:2/v1","apiKey":"sk-plaintext-legacy-0001","shared":false}]),
    );
    let before = std::fs::read(private_path(&f, A)).unwrap();
    let (_, list) = call(f.addr, "GET", "/api/providers", ADMIN, "").await;
    assert_eq!(find(&list, "plain")["apiKeyMasked"], "…0001");
    assert_eq!(std::fs::read(private_path(&f, A)).unwrap(), before);
    // The next write seals it (core's saveProviders sealed every row).
    let (status, _) = call(
        f.addr,
        "POST",
        "/api/providers",
        ADMIN,
        r#"{"label":"Second","baseUrl":"http://127.0.0.1:3/v1"}"#,
    )
    .await;
    assert_eq!(status, 200);
    let rows = read_rows(&private_path(&f, A));
    assert!(rows[0]["apiKey"].as_str().unwrap().starts_with("enc:v1:"));
    assert_eq!(
        open_v1(rows[0]["apiKey"].as_str().unwrap()),
        "sk-plaintext-legacy-0001"
    );
}

// ---------------------------------------------------------------------------------------------
// The project store's providers.json, which Node's seal-on-load migration saves through.

#[tokio::test]
async fn the_project_store_saves_providers_json_by_compare_and_swap() {
    let d = tempfile::tempdir().unwrap();
    let _db = seed(d.path());
    let f = front(d.path(), "0").await;
    let (status, read) = store(
        f.store,
        "read",
        &format!(r#"{{"user":"{A}","file":"providers.json"}}"#),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(read, j(r#"{"version":"","text":null}"#));
    let text = r#"{"providers":[{"id":"a","label":"A","baseUrl":"http://x.test","apiKey":"","shared":false}]}"#;
    let body =
        serde_json::json!({"user": A, "file": "providers.json", "version": "", "text": text});
    let (status, wrote) = store(f.store, "write", &body.to_string()).await;
    assert_eq!(status, 200, "{wrote}");
    let version = wrote["version"].as_str().unwrap().to_owned();
    assert_eq!(std::fs::read_to_string(private_path(&f, A)).unwrap(), text);
    // A stale version gets the current file back, and nothing is written.
    let stale = serde_json::json!({"user": A, "file": "providers.json", "version": "", "text": "{\"providers\":[]}"});
    let (status, conflict) = store(f.store, "write", &stale.to_string()).await;
    assert_eq!(status, 409);
    assert_eq!(conflict["version"], version.as_str());
    assert_eq!(conflict["text"], text);
    // Text that is not a provider file is refused.
    let bad =
        serde_json::json!({"user": A, "file": "providers.json", "version": version, "text": "[]"});
    assert_eq!(store(f.store, "write", &bad.to_string()).await.0, 400);
    assert_eq!(std::fs::read_to_string(private_path(&f, A)).unwrap(), text);
    // The file name is still checked.
    let other = serde_json::json!({"user": A, "file": "shared-providers.json", "version": "", "text": text});
    assert_eq!(store(f.store, "write", &other.to_string()).await.0, 400);
    // A Rust write moves the version Node's next save compares against.
    call(
        f.addr,
        "POST",
        "/api/providers",
        ADMIN,
        r#"{"label":"B","baseUrl":"http://127.0.0.1:3/v1"}"#,
    )
    .await;
    let again =
        serde_json::json!({"user": A, "file": "providers.json", "version": version, "text": text});
    assert_eq!(store(f.store, "write", &again.to_string()).await.0, 409);
}

// ---------------------------------------------------------------------------------------------
// A client that leaves must not strand a multi-step change.

#[tokio::test]
async fn a_client_that_leaves_mid_disconnect_still_gets_the_whole_disconnect() {
    let d = tempfile::tempdir().unwrap();
    let db = seed(d.path());
    let f = front(d.path(), "1").await;
    write_rows(
        &private_path(&f, A),
        &serde_json::json!([{"id":"chatgpt-oauth","kind":"chatgpt-oauth","label":"ChatGPT","baseUrl":"https://chatgpt.com/backend-api/codex","apiKey":"","shared":false,"external":true}]),
    );
    store_tokens(&db, A, "connected");
    std::fs::write(
        f.dir.join("users").join(A).join("projects.json"),
        r#"{"projects":[{"id":"p1","provider":"chatgpt-oauth","model":"gpt-x"}]}"#,
    )
    .unwrap();
    // The handler is between the tokens going and the row going (a debug-build pause) when the
    // tab closes.
    noevia_server::provider_routes::hooks::PAUSE_AFTER_TOKENS
        .lock()
        .unwrap()
        .push((d.path().to_path_buf(), 800));
    let mut s = TcpStream::connect(f.addr).await.unwrap();
    s.write_all(request(f.addr, "DELETE", "/api/providers/chatgpt", ADMIN, "").as_bytes())
        .await
        .unwrap();
    for _ in 0..100 {
        if token_row(&db, A).is_none() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(token_row(&db, A), None, "the first step ran");
    assert_eq!(
        read_rows(&private_path(&f, A)).len(),
        1,
        "the row is still there"
    );
    drop(s);
    for _ in 0..100 {
        if read_rows(&private_path(&f, A)).is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(token_row(&db, A), None, "the tokens went");
    assert!(
        read_rows(&private_path(&f, A)).is_empty(),
        "the row went after them although nobody was waiting"
    );
    let stored =
        j(&std::fs::read_to_string(f.dir.join("users").join(A).join("projects.json")).unwrap());
    assert_eq!(stored["projects"][0], j(r#"{"id":"p1"}"#));
}

#[tokio::test]
async fn a_removal_waits_for_the_files_lock_and_survives_the_client_leaving() {
    let d = tempfile::tempdir().unwrap();
    let _db = seed(d.path());
    let f = front(d.path(), "1").await;
    write_rows(
        &private_path(&f, A),
        &serde_json::json!([{"id":"mine","label":"Mine","baseUrl":"http://127.0.0.1:2/v1","apiKey":"","shared":false}]),
    );
    std::fs::write(
        f.dir.join("users").join(A).join("projects.json"),
        r#"{"projects":[{"id":"p1","provider":"mine","model":"m"}]}"#,
    )
    .unwrap();
    // Another writer holds the file's lock (flock on providers.json.lock) while the tab closes.
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(private_path(&f, A).with_extension("json.lock"))
        .unwrap();
    lock.lock().unwrap();
    let mut s = TcpStream::connect(f.addr).await.unwrap();
    s.write_all(request(f.addr, "DELETE", "/api/providers/mine", ADMIN, "").as_bytes())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    drop(s);
    tokio::time::sleep(Duration::from_millis(200)).await;
    lock.unlock().unwrap();
    for _ in 0..100 {
        if read_rows(&private_path(&f, A)).is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(read_rows(&private_path(&f, A)).is_empty());
    let stored =
        j(&std::fs::read_to_string(f.dir.join("users").join(A).join("projects.json")).unwrap());
    assert_eq!(stored["projects"][0], j(r#"{"id":"p1"}"#));
}

#[tokio::test]
async fn a_member_may_connect_an_operator_approved_origin_and_no_lookalike() {
    let d = tempfile::tempdir().unwrap();
    let _db = seed(d.path());
    let f = front(d.path(), "0").await;
    for (base, status) in [
        ("https://approved.example.com/v1", 200),
        ("https://approved.example.com.evil.test/v1", 400),
        ("https://approved.example.com:8443/v1", 400),
        ("http://approved.example.com/v1", 400),
    ] {
        let body = format!(r#"{{"label":"Approved","baseUrl":"{base}"}}"#);
        let (got, answer) = call(f.addr, "POST", "/api/providers", MEMBER, &body).await;
        assert_eq!(got, status, "{base}: {answer}");
    }
    assert_eq!(
        read_rows(&private_path(&f, B)).len(),
        1,
        "only the approved one was saved"
    );
    // Moving an own row to another origin is held to the same rule.
    let id = read_rows(&private_path(&f, B))[0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let (status, _) = call(
        f.addr,
        "PUT",
        &format!("/api/providers/{id}"),
        MEMBER,
        r#"{"baseUrl":"http://10.0.0.9/v1"}"#,
    )
    .await;
    assert_eq!(status, 400);
    let (status, _) = call(
        f.addr,
        "PUT",
        &format!("/api/providers/{id}"),
        MEMBER,
        r#"{"label":"Renamed"}"#,
    )
    .await;
    assert_eq!(status, 200);
}

#[tokio::test]
async fn simultaneous_changes_never_lose_each_others_rows() {
    let d = tempfile::tempdir().unwrap();
    let _db = seed(d.path());
    let f = front(d.path(), "0").await;
    let mut tasks = Vec::new();
    for n in 0..12 {
        let addr = f.addr;
        tasks.push(tokio::spawn(async move {
            let body = format!(r#"{{"label":"Row {n}","baseUrl":"http://127.0.0.1:{}/v1","apiKey":"sk-synthetic-key-{n:04}"}}"#, 2000 + n);
            call(addr, "POST", "/api/providers", ADMIN, &body).await
        }));
    }
    let mut ids = Vec::new();
    for t in tasks {
        let (status, made) = t.await.unwrap();
        assert_eq!(status, 200, "{made}");
        ids.push(made["id"].as_str().unwrap().to_owned());
    }
    let rows = read_rows(&private_path(&f, A));
    assert_eq!(rows.len(), 12, "every row survived");
    for id in &ids {
        assert!(rows.iter().any(|r| r["id"] == id.as_str()), "{id}");
    }
    // Removals in parallel with edits: each lands on its own row.
    let mut tasks = Vec::new();
    for (n, id) in ids.iter().enumerate() {
        let (addr, id) = (f.addr, id.clone());
        tasks.push(tokio::spawn(async move {
            if n % 2 == 0 {
                call(addr, "DELETE", &format!("/api/providers/{id}"), ADMIN, "")
                    .await
                    .0
            } else {
                call(
                    addr,
                    "PUT",
                    &format!("/api/providers/{id}"),
                    ADMIN,
                    r#"{"label":"Edited"}"#,
                )
                .await
                .0
            }
        }));
    }
    for t in tasks {
        assert_eq!(t.await.unwrap(), 200);
    }
    let rows = read_rows(&private_path(&f, A));
    assert_eq!(rows.len(), 6);
    assert!(rows.iter().all(|r| r["label"] == "Edited"));
    for r in &rows {
        assert!(open_v1(r["apiKey"].as_str().unwrap()).starts_with("sk-synthetic-key-"));
    }
}
