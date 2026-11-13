//! `POST /v1/mcp/call` end to end (track R5, mcp_call.rs): the real front, its real internal store listener,
//! a synthetic MCP server on 127.0.0.1. The switch (`NOEVIA_RUST_MCP_CALLS`) keeps it a 404 when off; on, a
//! call reaches the server with the right credential and answers core's text; a caller that goes away
//! still gets its session closed. The texts themselves are proven against core by `mcp_call_oracle.rs`.
#![cfg(feature = "rust-storage-reads")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
mod mcp_support;

use mcp_support::*;
use std::time::Duration;
use tokio::io::AsyncWriteExt;

/// A request body as JSON (`null` for an empty one, such as the session close).
fn body_of(s: &Seen) -> serde_json::Value {
    serde_json::from_str(&s.body).unwrap_or(serde_json::Value::Null)
}

/// The synthetic server: the MCP handshake, and `tools/call` answered with `answer`.
fn serve_tools(up: &Upstream, answer: &'static str, delay: Duration) {
    up.set(move |r: &Seen| {
        let body = body_of(r);
        if r.method == "POST" && body["method"] == "tools/call" {
            let id = body["id"].clone();
            let (_, text) = reply(
                "200 OK",
                &format!(r#"{{"jsonrpc":"2.0","id":{id},"result":{answer}}}"#),
            );
            return (delay, text);
        }
        mcp_reply(r, None).unwrap_or_else(|| reply("404 Not Found", ""))
    });
}

fn env(up: &Upstream, on: bool) -> Vec<(&'static str, String)> {
    let mut e = vec![("MCP_SERVERS", format!("open|{}/mcp|none", up.base()))];
    if on {
        e.push(("NOEVIA_RUST_MCP_CALLS", "1".into()));
    }
    e
}

const CALL: &str = r#"{"server":"open","name":"one","arguments":{"q":"qa é"},"user":"aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa","write":false}"#;

#[tokio::test]
async fn the_operation_is_a_404_while_the_switch_is_off() {
    let d = tempfile::tempdir().unwrap();
    seed(d.path());
    let up = upstream().await;
    serve_tools(&up, r#"{"content":[]}"#, Duration::ZERO);
    let f = front_env(d.path(), true, &env(&up, false)).await;
    let (st, _) = store(f.store, "call", STORE_TOKEN, CALL).await;
    assert_eq!(st, 404);
    assert!(up.seen().is_empty(), "nothing was sent");
    // The store secret is checked first, as for every operation.
    let (st, _) = store(f.store, "call", "wrong-token-wrong-token-wrong-token", CALL).await;
    assert_eq!(st, 401);
}

#[tokio::test]
async fn a_call_reaches_the_server_and_answers_cores_text() {
    let d = tempfile::tempdir().unwrap();
    seed(d.path());
    let up = upstream().await;
    serve_tools(
        &up,
        r#"{"content":[{"type":"text","text":"qa answer"}]}"#,
        Duration::ZERO,
    );
    let f = front_env(d.path(), true, &env(&up, true)).await;
    let (st, body) = store(f.store, "call", STORE_TOKEN, CALL).await;
    assert_eq!(
        (st, body),
        (200, j(r#"{"text":"qa answer","reduce":true}"#))
    );
    up.wait_for("/mcp", 4).await;
    let seen = up.seen();
    let order: Vec<String> = seen
        .iter()
        .map(|s| {
            format!(
                "{} {}",
                s.method,
                body_of(s)["method"].as_str().unwrap_or("")
            )
        })
        .collect();
    assert_eq!(
        order,
        [
            "POST initialize",
            "POST notifications/initialized",
            "POST tools/call",
            "DELETE "
        ]
    );
    assert_eq!(
        body_of(&seen[2])["params"],
        j(r#"{"name":"one","arguments":{"q":"qa é"}}"#)
    );
    // A server that is no longer configured, and a body that is not a call.
    let (st, body) = store(
        f.store,
        "call",
        STORE_TOKEN,
        r#"{"server":"gone","name":"one"}"#,
    )
    .await;
    assert_eq!(st, 200);
    assert_eq!(
        body["text"],
        "ERROR: tool \"one\" belongs to MCP server \"gone\", which is no longer configured"
    );
    let (st, _) = store(f.store, "call", STORE_TOKEN, r#"{"name":"one"}"#).await;
    assert_eq!(st, 400);
}

#[tokio::test]
async fn a_personal_server_gets_the_callers_own_key_and_no_one_elses() {
    let d = tempfile::tempdir().unwrap();
    let db = seed(d.path());
    let up = upstream().await;
    up.set(|r: &Seen| {
        let body = body_of(r);
        if body["method"] == "tools/call" {
            let id = body["id"].clone();
            let key = r.header("x-api-key").unwrap_or_default();
            return reply(
                "200 OK",
                &format!(r#"{{"jsonrpc":"2.0","id":{id},"result":{{"content":[{{"type":"text","text":"key {key}"}}]}}}}"#),
            );
        }
        mcp_reply(r, None).unwrap_or_else(|| reply("404 Not Found", ""))
    });
    add_server(
        &db,
        "dir-keyed",
        &format!("{}/mcp", up.base()),
        false,
        true,
        DECLARED,
    );
    db.execute(
        "INSERT INTO directory_mcp_user_keys VALUES(?1,'dir-keyed',?2,1)",
        (A, KEYS_GOLDEN),
    )
    .unwrap();
    let f = front_env(d.path(), true, &env(&up, true)).await;
    let ask = |user: &str| {
        format!(r#"{{"server":"dir-keyed","name":"one","arguments":{{}},"user":"{user}"}}"#)
    };
    let (st, body) = store(f.store, "call", STORE_TOKEN, &ask(A)).await;
    assert_eq!(
        (st, body["text"].as_str()),
        (200, Some("key synthetic-key"))
    );
    // B holds no key: told to add one, and nothing is sent.
    let before = up.seen().len();
    let (st, body) = store(f.store, "call", STORE_TOKEN, &ask(B)).await;
    assert_eq!(st, 200);
    assert_eq!(
        body["text"],
        "ERROR: one needs your own key for Server dir-keyed: Plugins → Connected → Add key."
    );
    assert_eq!(up.seen().len(), before);
}

#[tokio::test]
async fn a_caller_that_goes_away_still_gets_its_session_closed() {
    let d = tempfile::tempdir().unwrap();
    seed(d.path());
    let up = upstream().await;
    serve_tools(&up, r#"{"content":[]}"#, Duration::from_secs(30));
    let f = front_env(d.path(), true, &env(&up, true)).await;
    let mut s = tokio::net::TcpStream::connect(f.store).await.unwrap();
    s.write_all(format!("POST /v1/mcp/call HTTP/1.1\r\nhost: store\r\ncontent-type: application/json\r\ncontent-length: {}\r\n{}: {STORE_TOKEN}\r\n\r\n{CALL}", CALL.len(), noevia_server::project_store::TOKEN_HEADER).as_bytes()).await.unwrap();
    up.wait_for("/mcp", 3).await;
    let gone = std::time::Instant::now();
    drop(s);
    up.wait_for("/mcp", 4).await;
    assert_eq!(up.seen().last().unwrap().method, "DELETE");
    assert!(
        gone.elapsed() < Duration::from_secs(10),
        "{:?}",
        gone.elapsed()
    );
}

/// A sign-in server whose `tools/call` refuses `refused` tokens; `/token` hands out `qa-at-2`.
fn serve_sign_in(up: &Upstream, refused: &'static [&'static str]) {
    up.set(move |r: &Seen| {
        if r.path.starts_with("/token") {
            return reply(
                "200 OK",
                r#"{"access_token":"qa-at-2","refresh_token":"qa-refresh-2","expires_in":3600}"#,
            );
        }
        let body = body_of(r);
        if r.method == "POST" && body["method"] == "tools/call" {
            let token = r.header("authorization").unwrap_or_default();
            if refused.iter().any(|t| token == format!("bearer {t}")) {
                return reply("401 Unauthorized", r#"{"error":"token"}"#);
            }
            let id = body["id"].clone();
            return reply(
                "200 OK",
                &format!(r#"{{"jsonrpc":"2.0","id":{id},"result":{{"content":[{{"type":"text","text":"ok with {token}"}}]}}}}"#),
            );
        }
        mcp_reply(r, None).unwrap_or_else(|| reply("404 Not Found", ""))
    });
}

async fn signed_in_front(refused: &'static [&'static str]) -> (tempfile::TempDir, Upstream, Front) {
    let d = tempfile::tempdir().unwrap();
    let db = seed(d.path());
    let up = upstream().await;
    serve_sign_in(&up, refused);
    add_server(
        &db,
        "dir-sign",
        &format!("{}/mcp", up.base()),
        true,
        false,
        "[]",
    );
    store_client(
        &db,
        "dir-sign",
        &up.base(),
        "http://noevia.test/api/mcp-oauth/callback",
        "",
    );
    store_tokens(
        &db,
        A,
        "dir-sign",
        "qa-at-1",
        Some("qa-refresh"),
        Some(now() + 3_600_000),
        3,
    );
    let f = front_env(d.path(), true, &env(&up, true)).await;
    (d, up, f)
}

const SIGN_CALL: &str = r#"{"server":"dir-sign","name":"one","arguments":{},"user":"aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"}"#;

#[tokio::test]
async fn a_refused_sign_in_is_refreshed_once_and_the_call_runs_with_the_new_token() {
    let (_d, up, f) = signed_in_front(&["qa-at-1"]).await;
    let (st, body) = store(f.store, "call", STORE_TOKEN, SIGN_CALL).await;
    assert_eq!(
        (st, body["text"].as_str()),
        (200, Some("ok with bearer qa-at-2"))
    );
    assert_eq!(up.count("/token"), 1);
}

#[tokio::test]
async fn a_fresh_token_refused_again_marks_that_sign_in_and_the_next_call_asks_to_sign_in() {
    let (_d, up, f) = signed_in_front(&["qa-at-1", "qa-at-2"]).await;
    let (st, body) = store(f.store, "call", STORE_TOKEN, SIGN_CALL).await;
    assert_eq!(
        (st, body["text"].as_str()),
        (200, Some("ERROR calling one: tool call failed (HTTP 401)"))
    );
    assert_eq!(up.count("/token"), 1);
    let (_, body) = store(f.store, "call", STORE_TOKEN, SIGN_CALL).await;
    assert_eq!(
        body["text"],
        "ERROR: one needs you to sign in to Server dir-sign first: Plugins → Connected → Sign in."
    );
}

#[tokio::test]
async fn a_key_that_no_longer_opens_asks_for_the_key_not_to_try_again() {
    let d = tempfile::tempdir().unwrap();
    let db = seed(d.path());
    let up = upstream().await;
    serve_tools(&up, r#"{"content":[]}"#, Duration::ZERO);
    add_server(
        &db,
        "dir-keyed",
        &format!("{}/mcp", up.base()),
        false,
        true,
        DECLARED,
    );
    // A's sealed key copied under B's id: it opens for no one but A (the broker's `reconnect`).
    db.execute(
        "INSERT INTO directory_mcp_user_keys VALUES(?1,'dir-keyed',?2,1)",
        (B, KEYS_GOLDEN),
    )
    .unwrap();
    let f = front_env(d.path(), true, &env(&up, true)).await;
    let ask = format!(r#"{{"server":"dir-keyed","name":"one","user":"{B}"}}"#);
    let (st, body) = store(f.store, "call", STORE_TOKEN, &ask).await;
    assert_eq!(st, 200);
    assert_eq!(
        body["text"],
        "ERROR: one needs your own key for Server dir-keyed: Plugins → Connected → Add key."
    );
    assert!(up.seen().is_empty());
}

fn b64url_decode(s: &str) -> Vec<u8> {
    let val = |c: u8| match c {
        b'A'..=b'Z' => c - b'A',
        b'a'..=b'z' => c - b'a' + 26,
        b'0'..=b'9' => c - b'0' + 52,
        b'-' => 62,
        _ => 63,
    };
    let mut out = Vec::new();
    let (mut acc, mut bits) = (0u32, 0);
    for c in s.bytes() {
        acc = (acc << 6) | u32::from(val(c));
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    out
}

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    use sha2::{Digest, Sha256};
    let mut block = [0u8; 64];
    block[..key.len()].copy_from_slice(key);
    let inner = Sha256::new()
        .chain_update(block.map(|b| b ^ 0x36))
        .chain_update(data)
        .finalize();
    Sha256::new()
        .chain_update(block.map(|b| b ^ 0x5c))
        .chain_update(inner)
        .finalize()
        .to_vec()
}

#[tokio::test]
async fn the_in_process_server_gets_a_capability_token_signed_with_the_derived_key() {
    let d = tempfile::tempdir().unwrap();
    seed(d.path());
    let up = upstream().await;
    serve_tools(
        &up,
        r#"{"content":[{"type":"text","text":"edited"}]}"#,
        Duration::ZERO,
    );
    let extra = vec![
        ("MCP_SERVERS", format!("noevia|{}/mcp|internal", up.base())),
        ("NOEVIA_RUST_MCP_CALLS", "1".to_string()),
    ];
    let f = front_env(d.path(), true, &extra).await;
    let ask = format!(
        r#"{{"server":"noevia","name":"project_edit_file","arguments":{{}},"user":"{A}","workspaceUser":"{A}","project":"proj-1","editTarget":"digest-1","write":true}}"#
    );
    let started = now();
    let (st, body) = store(f.store, "call", STORE_TOKEN, &ask).await;
    assert_eq!((st, body["text"].as_str()), (200, Some("edited")));
    let call = up
        .seen()
        .into_iter()
        .find(|s| body_of(s)["method"] == "tools/call")
        .unwrap();
    let token = call
        .raw_headers
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case("authorization")
                .then(|| v.trim().to_owned())
        })
        .unwrap();
    let token = token.strip_prefix("Bearer ").unwrap();
    let (payload, signature) = token.split_once('.').unwrap();
    // core mcp-internal.cjs verifyToken: HMAC-SHA256 over the base64url payload, key derive('mcp-internal-token').
    let key = google_token_seal::derive_secret_key(&KEY, "mcp-internal-token");
    assert_eq!(
        b64url_decode(signature),
        hmac(key.as_slice(), payload.as_bytes())
    );
    let claims: serde_json::Value = serde_json::from_slice(&b64url_decode(payload)).unwrap();
    // core's payload, field for field and in core's order.
    let text = String::from_utf8(b64url_decode(payload)).unwrap();
    let want = format!(
        r#"{{"v":1,"uid":"{A}","pid":"proj-1","cid":null,"jti":"{}","exp":{},"w":1,"d":0,"t":"digest-1"}}"#,
        claims["jti"].as_str().unwrap(),
        claims["exp"]
    );
    assert_eq!(text, want);
    assert_eq!(claims["v"], 1);
    assert_eq!(claims["uid"], A);
    assert_eq!(claims["pid"], "proj-1");
    assert_eq!(claims["cid"], serde_json::Value::Null);
    assert_eq!(claims["jti"].as_str().unwrap().len(), 36);
    assert_eq!(
        (&claims["w"], &claims["d"]),
        (&serde_json::json!(1), &serde_json::json!(0))
    );
    assert_eq!(claims["t"], "digest-1");
    let exp = claims["exp"].as_i64().unwrap();
    assert!(exp >= started + 30_000 && exp <= now() + 30_000, "{exp}");
}
