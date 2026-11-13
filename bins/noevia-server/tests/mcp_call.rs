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
