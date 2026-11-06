//! The MCP tool executor through the real front and its real internal store listener (`POST /v1/mcp/call`,
//! sbstndalton/noevia#2218): the credentials are the broker's own (sealed rows, per account), the MCP server is one
//! synthetic http server on 127.0.0.1. Core's chat tool runtime is this endpoint's client (rust-mcp-exec in
//! project-store-client.cjs). No real server, account, token or credential.
#![cfg(feature = "rust-storage-reads")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
mod mcp_support;
use base64::Engine as _;
use mcp_support::*;
use serde_json::{json, Value};
use std::time::Duration;
use tokio::io::AsyncWriteExt;

/// A credential shown back reversed: the executor redacts a credential a server repeats verbatim, and what
/// this test needs from the server's answer is which credential arrived, not the credential itself.
fn rev(v: Option<String>) -> Value {
    v.map_or(Value::Null, |s| json!(s.chars().rev().collect::<String>()))
}

/// The same, for a value the test expects.
fn reversed(s: &str) -> Value {
    rev(Some(s.to_owned()))
}

/// The synthetic MCP server: a session at `initialize`, and `tools/call` answers with the credential it was shown (reversed).
fn echo(r: &Seen) -> (Duration, String) {
    if r.path != "/mcp" {
        return reply("404 Not Found", "");
    }
    if r.method == "DELETE" {
        return reply("200 OK", "");
    }
    let body = j(&r.body);
    let id = body["id"].clone();
    match body["method"].as_str().unwrap_or("") {
        "initialize" => reply_with(
            "200 OK",
            "mcp-session-id: synthetic-session\r\n",
            &format!(
                r#"{{"jsonrpc":"2.0","id":{id},"result":{{"protocolVersion":"2025-06-18","serverInfo":{{"name":"synthetic"}}}}}}"#
            ),
        ),
        "notifications/initialized" => reply("202 Accepted", ""),
        "tools/call" => {
            let shown = json!({
                "key": rev(r.header("x-api-key")), "auth": rev(r.header("authorization")), "uid": r.header("x-cowork-user-id"),
                "args": body["params"]["arguments"],
            });
            reply(
                "200 OK",
                &json!({"jsonrpc":"2.0","id":id,"result":{"content":[{"type":"text","text":shown.to_string()}]}}).to_string(),
            )
        }
        _ => reply("400 Bad", ""),
    }
}

async fn call(f: &Front, body: Value) -> (u16, Value) {
    store(f.store, "call", STORE_TOKEN, &body.to_string()).await
}

/// The text of a 200 answer.
fn text(v: &(u16, Value)) -> String {
    assert_eq!(v.0, 200, "{:?}", v.1);
    v.1["text"].as_str().unwrap().to_owned()
}

fn shown(t: &str) -> Value {
    j(t)
}

struct World {
    _dir: tempfile::TempDir,
    up: Upstream,
    front: Front,
}

async fn world() -> World {
    let dir = tempfile::tempdir().unwrap();
    let db = seed(dir.path());
    let up = upstream().await;
    up.set(echo);
    let base = up.base();
    let url = format!("{base}/mcp");
    add_server(&db, "dir-p", &url, false, true, DECLARED);
    add_server(&db, "dir-o", &url, true, false, "[]");
    add_server(&db, "dir-s", &url, false, false, DECLARED);
    db.execute(
        "UPDATE directory_mcp_servers SET headers_enc=?1 WHERE id='dir-s'",
        [seal(r#"{"X-Api-Key":"shared-key"}"#, None, 5)],
    )
    .unwrap();
    // Account A: its own key, its own sign-in, a Nextcloud connection. Account B: none of them.
    db.execute(
        "INSERT INTO directory_mcp_user_keys VALUES(?1,'dir-p',?2,1)",
        (A, seal(KEYS_JSON, Some(A), 6)),
    )
    .unwrap();
    store_tokens(
        &db,
        A,
        "dir-o",
        "acc-of-a",
        None,
        Some(now() + 3_600_000),
        7,
    );
    db.execute(
        "INSERT INTO storage_connections(user_id,kind,base_url,username,secret,updated_at) VALUES(?1,'nextcloud',?2,'alice',?3,1)",
        (A, &base, seal("app-pass", Some(A), 8)),
    )
    .unwrap();
    let servers = format!("plain|{url}|none,nc|{url}|nextcloud");
    let front = front_env(
        dir.path(),
        true,
        &[("MCP_SERVERS", servers), ("MCP_NEXTCLOUD_ORIGINS", base)],
    )
    .await;
    World {
        _dir: dir,
        up,
        front,
    }
}

fn request(user: &str, name: &str, server: &str, offers: &[&str]) -> Value {
    json!({"name": name, "args": {"q": 1}, "server": server, "offers": offers, "caller": user, "workspace": user})
}

#[tokio::test]
async fn each_account_gets_its_own_credential_and_never_anothers() {
    let w = world().await;
    let f = &w.front;
    // A personal key: the account's own, asked of the broker for this call.
    let a = shown(&text(&call(f, request(A, "t", "dir-p", &["dir-p"])).await));
    assert_eq!(a["key"], reversed("synthetic-key"));
    assert_eq!(a["args"], json!({"q": 1}));
    assert_eq!(
        text(&call(f, request(B, "t", "dir-p", &["dir-p"])).await),
        "ERROR: t needs your own key for Server dir-p: Plugins → Connected → Add key."
    );
    // A sign-in: the account's own access token.
    assert_eq!(
        shown(&text(&call(f, request(A, "t", "dir-o", &["dir-o"])).await))["auth"],
        reversed("bearer acc-of-a")
    );
    assert_eq!(
        text(&call(f, request(B, "t", "dir-o", &["dir-o"])).await),
        "ERROR: t needs you to sign in to Server dir-o first: Plugins → Connected → Sign in."
    );
    // The account's Nextcloud connection, to the declared origin only.
    let basic = base64::engine::general_purpose::STANDARD.encode("alice:app-pass");
    let nc = shown(&text(&call(f, request(A, "t", "nc", &["nc"])).await));
    assert_eq!(
        (nc["auth"].clone(), nc["uid"].clone()),
        (reversed(&format!("Basic {basic}").to_lowercase()), json!(A))
    );
    assert!(text(&call(f, request(B, "t", "nc", &["nc"])).await)
        .starts_with("ERROR: this tool needs your Nextcloud account."));
    // The account named in `workspace` is the one whose connection is used, not `caller`.
    let mut crossed = request(B, "t", "nc", &["nc"]);
    crossed["workspace"] = json!(A);
    assert_eq!(shown(&text(&call(f, crossed).await))["uid"], A);
    // An administrator's shared key is every account's.
    for user in [A, B] {
        assert_eq!(
            shown(&text(
                &call(f, request(user, "t", "dir-s", &["dir-s"])).await
            ))["key"],
            reversed("shared-key")
        );
    }
    // No account in the call: nothing is invented.
    let mut anon = request(A, "t", "dir-p", &["dir-p"]);
    anon.as_object_mut().unwrap().remove("caller");
    assert!(text(&call(f, anon).await).starts_with("ERROR: t needs your own key for"));
    // The ordinary operator server needs none.
    let plain = shown(&text(&call(f, request(B, "t", "plain", &["plain"])).await));
    assert_eq!(
        (plain["auth"].clone(), plain["key"].clone()),
        (Value::Null, Value::Null)
    );
    // Every session was closed.
    let sent = w.up.seen();
    let opened = sent
        .iter()
        .filter(|r| r.body.contains("\"initialize\""))
        .count();
    assert_eq!(sent.iter().filter(|r| r.method == "DELETE").count(), opened);
}

#[tokio::test]
async fn an_approved_edit_goes_nowhere_but_noevias_own_server_and_a_removed_server_is_not_called() {
    let w = world().await;
    let f = &w.front;
    let before = w.up.seen().len();
    let mut edit = request(A, "project_edit", "dir-s", &["dir-s"]);
    edit["editTarget"] = json!(true);
    assert_eq!(
        text(&call(f, edit).await),
        "ERROR: project_edit was approved as a project file edit, but \"project_edit\" was offered by MCP server \"dir-s\"; it was not run."
    );
    assert_eq!(w.up.seen().len(), before, "the server received nothing");
    // Routing: a name two servers offer and no server named is refused, not guessed.
    let mut both = request(A, "t", "x", &["plain", "nc"]);
    both.as_object_mut().unwrap().remove("server");
    assert_eq!(
        text(&call(f, both).await),
        "ERROR: tool \"t\" is offered by more than one MCP server, so it was not run."
    );
    // The directory table gone (or unreadable): the server is not configured, so nothing is sent.
    let db = server_store::rusqlite::Connection::open(w._dir.path().join("cowork.db")).unwrap();
    db.execute_batch("DROP TABLE directory_mcp_servers")
        .unwrap();
    assert_eq!(
        text(&call(f, request(A, "t", "dir-p", &["dir-p"])).await),
        "ERROR: tool \"t\" belongs to MCP server \"dir-p\", which is no longer configured"
    );
    assert_eq!(w.up.seen().len(), before);
}

#[tokio::test]
async fn only_the_front_s_own_caller_with_a_well_formed_call_is_answered() {
    let w = world().await;
    let f = &w.front;
    let body = request(A, "t", "plain", &["plain"]).to_string();
    let (status, _) = store(f.store, "call", "wrong-token-0123456789abcdef012345", &body).await;
    assert_eq!(status, 401);
    let (status, _) = store(f.store, "call", "", &body).await;
    assert_eq!(status, 401);
    for bad in [
        "not json",
        r#"{"offers":[]}"#,
        r#"{"name":"","offers":[]}"#,
        r#"{"name":"t","offers":"plain"}"#,
        r#"{"name":"t","offers":[1]}"#,
        r#"{"name":"t"}"#,
    ] {
        assert_eq!(
            store(f.store, "call", STORE_TOKEN, bad).await.0,
            400,
            "{bad}"
        );
    }
    assert_eq!(w.up.seen().len(), 0, "a malformed call reached no server");
    // The answer is always a 200 with text, a failed call included (core returned failures as text).
    let (status, v) = call(f, request(A, "t", "plain", &["nobody"])).await;
    assert_eq!(status, 200);
    assert_eq!(
        v["text"],
        "ERROR: unknown tool \"t\" on MCP server \"plain\""
    );
}

/// A caller that goes away (the chat's Stop, a browser that left) drops the call, and the session it opened is
/// closed: the server sees a DELETE with the session id.
#[tokio::test]
async fn a_caller_that_leaves_ends_the_call_and_closes_its_session() {
    let w = world().await;
    w.up.set(|r| {
        if r.path == "/mcp" && r.body.contains("\"tools/call\"") {
            // The tool never answers within the test.
            return (Duration::from_secs(60), String::new());
        }
        echo(r)
    });
    let body = request(A, "t", "plain", &["plain"]).to_string();
    let mut s = tokio::net::TcpStream::connect(w.front.store).await.unwrap();
    s.write_all(
        format!(
            "POST /v1/mcp/call HTTP/1.1\r\nhost: store\r\nconnection: close\r\ncontent-type: application/json\r\ncontent-length: {}\r\n{}: {STORE_TOKEN}\r\n\r\n{body}",
            body.len(),
            noevia_server::project_store::TOKEN_HEADER
        )
        .as_bytes(),
    )
    .await
    .unwrap();
    w.up.wait_for("/mcp", 3).await;
    assert!(w
        .up
        .seen()
        .iter()
        .any(|r| r.body.contains("\"tools/call\"")));
    drop(s);
    // The close is not tied to the caller that left: it arrives promptly all the same.
    let mut closed = false;
    for _ in 0..200 {
        if w.up.seen().iter().any(|r| {
            r.method == "DELETE"
                && r.header("mcp-session-id").as_deref() == Some("synthetic-session")
        }) {
            closed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        closed,
        "the session was left open: {:?}",
        w.up.seen()
            .iter()
            .map(|r| (&r.method, &r.path))
            .collect::<Vec<_>>()
    );
}
