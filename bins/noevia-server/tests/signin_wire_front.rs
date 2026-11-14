//! The sign-in provider's chat call through the real front: its broker (a sealed QA token row in a
//! seeded data directory), the broker's change signal, and a synthetic backend on 127.0.0.1 (the
//! loopback QA override). No real account, token or credential. Nothing in the shipped front calls
//! src/signin_wire.rs yet; this pins what it does once a caller exists.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
use noevia_server::chatgpt_oauth_routes::generation_of;
use noevia_server::signin_wire::{client, fetch_for, into_response, FrontAccounts, SigninWire, SESSIONS};
use noevia_server::{config::Config, App};
use http_body_util::BodyExt;
use server_store::rusqlite::Connection;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const A: &str = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
const KEY: [u8; 32] = [7; 32];
const DEADLINE: Duration = Duration::from_secs(20);
const BODY: &str = r#"{"model":"m-large","messages":[{"role":"user","content":"q"}],"stream":true}"#;
const STREAM: &str = concat!(
    "data: {\"type\":\"response.output_text.delta\",\"item_id\":\"m1\",\"delta\":\"Hel\"}\n\n",
    "data: {\"type\":\"response.output_text.delta\",\"item_id\":\"m1\",\"delta\":\"lo\"}\n\n",
    "data: {\"type\":\"response.completed\",\"response\":{\"output\":[],\"usage\":{\"input_tokens\":3,\"output_tokens\":2,\"total_tokens\":5}}}\n\n",
);

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
    db.execute("INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,created_at,updated_at) VALUES(?1,?1,?1,'Synthetic','member','x',?1,1,1)", [A]).unwrap();
    std::fs::create_dir_all(dir.join("users").join(A)).unwrap();
    db
}

/// A stored connected row; its ciphertext.
fn store_tokens(db: &Connection, access: &str, expires: i64) -> String {
    let json = format!(
        r#"{{"accessToken":"{access}","refreshToken":"synthetic-refresh","accountId":"acct-synthetic","fedRamp":true,"expiresAt":{expires},"email":"qa@example.test","plan":"plus","connectedAt":1700000000000}}"#
    );
    let sealed =
        secret_envelope::seal(&KEY, &[3; 12], json.as_bytes(), Some(A.as_bytes())).unwrap();
    db.execute(
        "INSERT OR REPLACE INTO chatgpt_oauth_tokens(user_id,data_enc,state,updated_at) VALUES(?1,?2,'connected',1)",
        (A, &sealed),
    )
    .unwrap();
    sealed
}

/// The synthetic backend: every `/responses` call answers the stream; anything else 404. It keeps
/// each request's path and lower-case headers.
async fn backend() -> (String, Arc<Mutex<Vec<(String, Vec<(String, String)>)>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let log = Arc::clone(&log);
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 8192];
                let end = loop {
                    if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break i;
                    }
                    match sock.read(&mut tmp).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => buf.extend_from_slice(&tmp[..n]),
                    }
                };
                let head = String::from_utf8_lossy(&buf[..end]).into_owned();
                let mut lines = head.split("\r\n");
                let path = lines
                    .next()
                    .and_then(|l| l.split(' ').nth(1))
                    .unwrap_or("")
                    .to_owned();
                let headers: Vec<(String, String)> = lines
                    .filter_map(|l| l.split_once(':'))
                    .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_owned()))
                    .collect();
                let len: usize = headers
                    .iter()
                    .find(|(k, _)| k == "content-length")
                    .and_then(|(_, v)| v.parse().ok())
                    .unwrap_or(0);
                let mut have = buf.len() - end - 4;
                while have < len {
                    match sock.read(&mut tmp).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => have += n,
                    }
                }
                let answer = if path.ends_with("/responses") {
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{STREAM}",
                        STREAM.len()
                    )
                } else {
                    "HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".into()
                };
                log.lock().unwrap().push((path, headers));
                let _ = sock.write_all(answer.as_bytes()).await;
                let _ = sock.shutdown().await;
            });
        }
    });
    (format!("http://{addr}"), seen)
}

async fn reply_text(answer: noevia_server::signin_wire::FrontAnswer) -> (u16, String) {
    let res = into_response(answer).expect("an answer, not a failed fetch");
    let status = res.status().as_u16();
    let bytes = tokio::time::timeout(DEADLINE, res.into_body().collect())
        .await
        .unwrap()
        .unwrap()
        .to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

#[tokio::test]
async fn a_sign_in_turn_through_the_front_broker() {
    let d = tempfile::tempdir().unwrap();
    let db = seed(d.path());
    let (base, seen) = backend().await;
    let env = HashMap::from([
        ("NOEVIA_LEGACY_UPSTREAM", "http://127.0.0.1:9".to_string()),
        ("UI_DATA_DIR", d.path().display().to_string()),
        ("PUBLIC_ORIGIN", "http://127.0.0.1:9".to_string()),
        ("NOEVIA_FEATURE_CHATGPT_OAUTH", "1".to_string()),
        ("NOEVIA_QA_CHATGPT_ISSUER", base.clone()),
        ("NOEVIA_QA_CHATGPT_CODEX_BASE", format!("{base}/backend-api/codex/")),
    ]);
    let app = App::new(Config::from_lookup(|k| env.get(k).cloned()).unwrap());
    let broker = Arc::clone(app.chatgpt_oauth.as_ref().expect("the front has a sign-in broker"));
    let transport = broker.transport();
    assert_eq!(transport.base_url, format!("{base}/backend-api/codex"));
    let mut wire = SigninWire {
        accounts: FrontAccounts::new(&app).unwrap(),
        client: client().unwrap(),
    };
    let url = "http://x/v1/chat/completions";

    // No sign-in yet: the broker's "not connected" refusal, as core's error answer.
    let answer = fetch_for(&mut wire, &SESSIONS, &transport, A, Some("chat-1"), url, BODY).await;
    let (status, text) = reply_text(answer).await;
    assert_eq!(status, 401, "{text}");
    assert!(text.contains("is not connected"), "{text}");
    assert!(seen.lock().unwrap().iter().all(|(p, _)| !p.ends_with("/responses")));

    // A stored sign-in: the turn streams, with the account's headers on the wire.
    let now = noevia_server::identity::now_ms();
    let sealed = store_tokens(&db, "synthetic-access-1", now + 3_600_000);
    let answer = fetch_for(&mut wire, &SESSIONS, &transport, A, Some("chat-1"), url, BODY).await;
    let (status, text) = reply_text(answer).await;
    assert_eq!(status, 200, "{text}");
    assert!(text.contains("\"content\":\"Hel\""), "{text}");
    assert!(text.ends_with("data: [DONE]\n\n"), "{text}");
    let calls: Vec<_> = seen
        .lock()
        .unwrap()
        .iter()
        .filter(|(p, _)| p.ends_with("/responses"))
        .cloned()
        .collect();
    assert_eq!(calls.len(), 1);
    let (path, headers) = &calls[0];
    assert_eq!(path, "/backend-api/codex/responses");
    let h = |n: &str| headers.iter().find(|(k, _)| k == n).map(|(_, v)| v.as_str());
    assert_eq!(h("authorization"), Some("Bearer synthetic-access-1"));
    assert_eq!(h("chatgpt-account-id"), Some("acct-synthetic"));
    assert_eq!(h("x-openai-fedramp"), Some("true"));
    assert_eq!(h("accept"), Some("text/event-stream"));
    assert!(h("user-agent").is_some_and(|v| v.starts_with("noevia/")));
    // The session the cache keeps names this sign-in (core's generation of its ciphertext).
    let cached = SESSIONS.lock().unwrap().lookup(A, wire_signal(&broker).as_deref(), false, now as f64);
    assert_eq!(
        cached.map(|s| s.generation),
        Some(js_json::JValue::Str(generation_of(&sealed)))
    );

    // A disconnect moves the broker's change signal: the cached session is not used again.
    broker.disconnect(&app, A, false).await.unwrap();
    let answer = fetch_for(&mut wire, &SESSIONS, &transport, A, Some("chat-1"), url, BODY).await;
    let (status, text) = reply_text(answer).await;
    assert_eq!(status, 401, "{text}");
    let responses = seen
        .lock()
        .unwrap()
        .iter()
        .filter(|(p, _)| p.ends_with("/responses"))
        .count();
    assert_eq!(responses, 1, "no backend call after the disconnect");
}

fn wire_signal(broker: &noevia_server::chatgpt_oauth_routes::Broker) -> Option<String> {
    noevia_server::signin_wire::file_signal(&broker.signal_path())
}
