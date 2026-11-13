//! `POST /v1/gdrive/tool` and `/target` end to end (track R5, gdrive_tools.rs): the real front and its
//! internal store listener, the real token broker over a sealed account file, a synthetic Google (token and
//! Drive API) on 127.0.0.1. The switch (`NOEVIA_RUST_DRIVE_TOOLS`) keeps both a 404 when off; on, a tool
//! reaches Drive with the account's access token, and the read-before-update gate holds across calls. The
//! texts themselves are proven against core by `gdrive_tools_oracle.rs`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
mod gdrive_support;

use gdrive_support::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

// The golden account file of tests/gdrive.rs (core's gdrive.cjs sealer, key [7;32]) for account A.
const NODE_FILE: &str = "AQIDBAUGBwgJCgsMgIgxgDi+UwWrjb2pgZHTVk5Oe2S+sxp0cQhofG56mOyqJu4MKObbx2YjiOIp95XFlLRpoFsgDgJ1/IeS38BvXz/+MZVPwozLiOOvRCG99kgyZhbaVBN47NmtMEBpqgbKbH3pg0zMV/11xgL0BXiHW9DA/DYPs9ifgguMcyvGCrLfU3M/fg5aQqY2xrOOpQtYrhaJqw43qj4B6g==";

const FILE: &str =
    r#"{"id":"f1","name":"QA f1.txt","mimeType":"text/plain","size":"5","version":"7"}"#;

/// The synthetic Google. Refresh number N hands out `access-N`, except that refreshes past `grants`
/// answer `invalid_grant`. File `deny` refuses every token.
fn google_with(up: &Upstream, grants: usize) -> Arc<AtomicUsize> {
    let refreshes = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&refreshes);
    up.set(move |r: &Seen| {
        let path = r.path.split('?').next().unwrap_or("");
        match (r.method.as_str(), path) {
            ("POST", "/token") => {
                let n = counted.fetch_add(1, Ordering::SeqCst) + 1;
                if n > grants {
                    return reply("400 Bad Request", r#"{"error":"invalid_grant"}"#);
                }
                reply(
                    "200 OK",
                    &format!(r#"{{"access_token":"access-{n}","expires_in":3600}}"#),
                )
            }
            ("GET", "/drive/v3/files/deny") => reply("401 Unauthorized", "{}"),
            ("GET", "/drive/v3/files") => reply("200 OK", &format!(r#"{{"files":[{FILE}]}}"#)),
            ("GET", "/drive/v3/files/f1") if r.path.contains("alt=media") => (
                Duration::ZERO,
                b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nqa ok".to_vec(),
            ),
            ("GET", "/drive/v3/files/f1") => reply("200 OK", FILE),
            ("PATCH", "/upload/drive/v3/files/f1") => reply(
                "200 OK",
                r#"{"id":"f1","name":"QA f1.txt","mimeType":"text/plain","version":"8"}"#,
            ),
            _ => reply("404 Not Found", ""),
        }
    });
    refreshes
}

fn google(up: &Upstream) {
    google_with(up, usize::MAX);
}

async fn connected_front(on: bool) -> (tempfile::TempDir, Upstream, Front) {
    let d = tempfile::tempdir().unwrap();
    seed(d.path());
    let up = upstream().await;
    google(&up);
    let dir = d.path().join("google-drive-users");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(format!("{A}.sealed")), NODE_FILE).unwrap();
    let extra = if on {
        vec![("NOEVIA_RUST_DRIVE_TOOLS", "1".to_string())]
    } else {
        Vec::new()
    };
    let f = front(d.path(), &up.base(), &extra).await;
    (d, up, f)
}

fn tool(name: &str, args: &str, exchange: &str) -> String {
    format!(
        r#"{{"user":"{A}","admin":false,"name":"{name}","arguments":{args},"exchangeKey":"{exchange}","cap":8000}}"#
    )
}

#[tokio::test]
async fn both_operations_are_404s_while_the_switch_is_off() {
    let (_d, up, f) = connected_front(false).await;
    let (st, _) = store(
        f.store,
        "gdrive/tool",
        STORE_TOKEN,
        &tool("drive_list_recent", "{}", "x"),
    )
    .await;
    assert_eq!(st, 404);
    let (st, _) = store(
        f.store,
        "gdrive/target",
        STORE_TOKEN,
        &tool("drive_trash_file", r#"{"fileId":"f1"}"#, "x"),
    )
    .await;
    assert_eq!(st, 404);
    assert!(up.seen().is_empty(), "nothing was sent");
}

#[tokio::test]
async fn a_tool_reaches_drive_with_the_accounts_token_and_an_update_needs_a_full_read_in_this_exchange(
) {
    let (_d, up, f) = connected_front(true).await;
    let (st, body) = store(
        f.store,
        "gdrive/tool",
        STORE_TOKEN,
        &tool("drive_search_files", r#"{"query":"qa"}"#, "ex-1"),
    )
    .await;
    assert_eq!(st, 200, "{body}");
    assert_eq!(
        body["text"],
        "Found 1:\n- QA f1.txt (id f1) · text/plain · 5 bytes"
    );
    let list = up
        .seen()
        .into_iter()
        .find(|s| s.path.starts_with("/drive/v3/files?"))
        .unwrap();
    assert!(
        list.headers.contains("authorization: bearer access-1"),
        "{}",
        list.headers
    );
    // An update before a read is refused; after a full read in the same exchange it runs.
    let update = tool(
        "drive_update_file",
        r#"{"fileId":"f1","content":"qa new"}"#,
        "ex-1",
    );
    let (_, body) = store(f.store, "gdrive/tool", STORE_TOKEN, &update).await;
    assert!(
        body["text"]
            .as_str()
            .unwrap()
            .starts_with("ERROR: drive_update_file replaces the whole file"),
        "{body}"
    );
    let (_, body) = store(
        f.store,
        "gdrive/tool",
        STORE_TOKEN,
        &tool("drive_read_file", r#"{"fileId":"f1"}"#, "ex-1"),
    )
    .await;
    assert_eq!(body["text"], "QA f1.txt:\nqa ok");
    let (_, body) = store(f.store, "gdrive/tool", STORE_TOKEN, &update).await;
    assert_eq!(body["text"], "Updated QA f1.txt (id f1).");
    let patch = up.seen().into_iter().find(|s| s.method == "PATCH").unwrap();
    assert_eq!(patch.body, "qa new");
    assert!(patch
        .headers
        .contains("content-type: text/plain; charset=utf-8"));
    // Another exchange has read nothing.
    let other = tool(
        "drive_update_file",
        r#"{"fileId":"f1","content":"x"}"#,
        "ex-2",
    );
    let (_, body) = store(f.store, "gdrive/tool", STORE_TOKEN, &other).await;
    assert!(body["text"]
        .as_str()
        .unwrap()
        .starts_with("ERROR: drive_update_file replaces the whole file"));
}

#[tokio::test]
async fn the_approval_card_names_the_file_and_no_account_is_told_to_sign_in() {
    let (_d, _up, f) = connected_front(true).await;
    let (st, body) = store(
        f.store,
        "gdrive/target",
        STORE_TOKEN,
        &tool("drive_trash_file", r#"{"fileId":"f1"}"#, "x"),
    )
    .await;
    assert_eq!(
        (st, body),
        (
            200,
            j(r#"{"target":{"target":"QA f1.txt (id f1)","kind":"drive"}}"#)
        )
    );
    let (_, body) = store(
        f.store,
        "gdrive/target",
        STORE_TOKEN,
        &tool("drive_read_file", r#"{"fileId":"f1"}"#, "x"),
    )
    .await;
    assert_eq!(body, j(r#"{"target":null}"#));
    let nobody = r#"{"user":null,"name":"drive_list_recent","arguments":{}}"#;
    let (st, body) = store(f.store, "gdrive/tool", STORE_TOKEN, nobody).await;
    assert_eq!(
        (st, body["text"].as_str()),
        (200, Some("ERROR: Sign in first."))
    );
    // B has no connection.
    let other = format!(r#"{{"user":"{B}","name":"drive_list_recent","arguments":{{}}}}"#);
    let (_, body) = store(f.store, "gdrive/tool", STORE_TOKEN, &other).await;
    assert_eq!(body["text"], "ERROR: Google Drive is not connected for this account. Connect it in Settings → Connectors.");
    let (st, _) = store(
        f.store,
        "gdrive/tool",
        STORE_TOKEN,
        r#"{"user":null,"name":5}"#,
    )
    .await;
    assert_eq!(st, 400);
}

async fn front_with_grants(
    grants: usize,
) -> (tempfile::TempDir, Upstream, Front, Arc<AtomicUsize>) {
    let d = tempfile::tempdir().unwrap();
    seed(d.path());
    let up = upstream().await;
    let refreshes = google_with(&up, grants);
    let dir = d.path().join("google-drive-users");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(format!("{A}.sealed")), NODE_FILE).unwrap();
    let f = front(
        d.path(),
        &up.base(),
        &[("NOEVIA_RUST_DRIVE_TOOLS", "1".to_string())],
    )
    .await;
    (d, up, f, refreshes)
}

#[tokio::test]
async fn a_token_refused_twice_drops_that_sign_ins_token_and_the_next_call_refreshes_again() {
    let (_d, up, f, refreshes) = front_with_grants(usize::MAX).await;
    let deny = tool("drive_get_metadata", r#"{"fileId":"deny"}"#, "x");
    let (_, body) = store(f.store, "gdrive/tool", STORE_TOKEN, &deny).await;
    assert_eq!(body["text"], "ERROR: Google Drive answered 401.");
    // The first token, then one past the refused one.
    assert_eq!(refreshes.load(Ordering::SeqCst), 2);
    let tokens: Vec<String> = up
        .seen()
        .into_iter()
        .filter(|s| s.path.starts_with("/drive/v3/files/deny"))
        .map(|s| {
            s.headers
                .lines()
                .find(|l| l.starts_with("authorization:"))
                .unwrap_or("")
                .to_owned()
        })
        .collect();
    assert_eq!(
        tokens,
        [
            "authorization: bearer access-1",
            "authorization: bearer access-2"
        ]
    );
    // That sign-in's cached token was dropped: the next call asks Google again.
    let (_, body) = store(
        f.store,
        "gdrive/tool",
        STORE_TOKEN,
        &tool("drive_get_metadata", r#"{"fileId":"f1"}"#, "x"),
    )
    .await;
    assert!(
        body["text"]
            .as_str()
            .unwrap()
            .starts_with("- QA f1.txt (id f1)"),
        "{body}"
    );
    assert_eq!(refreshes.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn a_refresh_google_refuses_after_a_401_is_the_brokers_own_words() {
    let (_d, _up, f, _) = front_with_grants(1).await;
    let (_, body) = store(
        f.store,
        "gdrive/tool",
        STORE_TOKEN,
        &tool("drive_get_metadata", r#"{"fileId":"deny"}"#, "x"),
    )
    .await;
    assert_eq!(
        body["text"],
        "ERROR: Google no longer accepts this connection (access was removed in your Google account). Connect again."
    );
}

/// One store call, the answer as raw text (a lone surrogate escape survives).
async fn store_raw(f: &Front, op: &str, body: &str) -> (u16, String) {
    let mut s = tokio::net::TcpStream::connect(f.store).await.unwrap();
    s.write_all(format!("POST /v1/{op} HTTP/1.1\r\nhost: store\r\nconnection: close\r\ncontent-type: application/json\r\ncontent-length: {}\r\n{}: {STORE_TOKEN}\r\n\r\n{body}", body.len(), noevia_server::project_store::TOKEN_HEADER).as_bytes()).await.unwrap();
    let mut out = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(30), s.read_to_end(&mut out)).await;
    let text = String::from_utf8_lossy(&out).into_owned();
    let status = text[9..12].parse().unwrap();
    (
        status,
        text.split_once("\r\n\r\n")
            .map(|(_, b)| b.to_owned())
            .unwrap_or_default(),
    )
}

#[tokio::test]
async fn a_lone_surrogate_from_the_model_is_kept_as_core_keeps_it() {
    let (_d, up, f) = connected_front(true).await;
    // The raw arguments text, as Node passes it: {"name":"QA \ud83d note"} (a lone high surrogate).
    let raw = r#"{\"name\":\"QA \\ud83d note\"}"#;
    let body = format!(r#"{{"user":"{A}","name":"drive_create_file","arguments":"{raw}"}}"#);
    let (st, text) = store_raw(&f, "gdrive/target", &body).await;
    // The card names the new file with its broken emoji, written back as JSON.stringify writes it.
    assert_eq!(
        (st, text.as_str()),
        (
            200,
            r#"{"target":{"target":"QA \ud83d note","kind":"drive-new"}}"#
        )
    );
    // In a search query it cannot become an address (core's encodeURIComponent throws): nothing is sent.
    let before = up.seen().len();
    let (st, text) = store_raw(
        &f,
        "gdrive/tool",
        &tool("drive_search_files", r#"{"query":"qa \ud800"}"#, "x"),
    )
    .await;
    assert_eq!(
        (st, text.as_str()),
        (
            200,
            r#"{"text":"ERROR: Google Drive could not be reached."}"#
        )
    );
    assert_eq!(up.seen().len(), before);
}

#[tokio::test]
async fn a_tool_that_runs_out_of_its_time_answers_itself_and_sends_nothing_more() {
    let d = tempfile::tempdir().unwrap();
    seed(d.path());
    let up = upstream().await;
    // Each Drive answer takes 250 ms; the whole call may take 400 ms (two requests do not fit).
    up.set(|r: &Seen| {
        let path = r.path.split('?').next().unwrap_or("");
        match path {
            "/token" => reply("200 OK", r#"{"access_token":"access-1","expires_in":3600}"#),
            _ => {
                let (_, text) = reply("200 OK", FILE);
                (Duration::from_millis(250), text)
            }
        }
    });
    let dir = d.path().join("google-drive-users");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(format!("{A}.sealed")), NODE_FILE).unwrap();
    let f = front(
        d.path(),
        &up.base(),
        &[("NOEVIA_RUST_DRIVE_TOOLS", "1".to_string())],
    )
    .await;
    noevia_server::gdrive::of(&f.app)
        .unwrap()
        .set_budgets(Duration::from_secs(5), Duration::from_millis(400));
    let started = std::time::Instant::now();
    let (_, body) = store(
        f.store,
        "gdrive/tool",
        STORE_TOKEN,
        &tool("drive_read_file", r#"{"fileId":"f1"}"#, "x"),
    )
    .await;
    assert_eq!(body["text"], noevia_server::gdrive_tools::TOOK_TOO_LONG);
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
}

/// The core checkout with the native paths (`NOEVIA_R5_CORE`; CI: tests/fixtures/r5-live.core-ref). Unset:
/// the live test is skipped (it needs a real `node` and that checkout).
fn r5_core() -> Option<std::path::PathBuf> {
    std::env::var_os("NOEVIA_R5_CORE").map(std::path::PathBuf::from)
}

/// One phase of `r5_node_live.cjs` in a real `node` process against the front's internal store.
async fn node_phase(
    core: &std::path::Path,
    phase: &str,
    store: std::net::SocketAddr,
) -> serde_json::Value {
    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/r5_node_live.cjs");
    let mut command = std::process::Command::new("node");
    command
        .arg(script)
        .arg(phase)
        .env("NOEVIA_R5_CORE", core)
        .env("STORE_PORT", store.port().to_string())
        .env("STORE_TOKEN", STORE_TOKEN)
        .env("USER_ID", A);
    // The front serves on this runtime: wait for node on a thread of its own.
    let out = tokio::task::spawn_blocking(move || command.output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        out.status.success(),
        "node {phase} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_str(String::from_utf8(out.stdout).unwrap().trim()).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_hands_a_drive_tool_to_the_real_front() {
    let Some(core) = r5_core() else {
        return;
    };
    let (_d, up, f) = connected_front(true).await;
    let out = node_phase(&core, "drive", f.store).await;
    assert_eq!(out["list"], "- QA f1.txt (id f1) · text/plain · 5 bytes");
    assert!(out["updateBeforeRead"]
        .as_str()
        .unwrap()
        .starts_with("ERROR: drive_update_file replaces the whole file"));
    assert_eq!(out["read"], "QA f1.txt:\nqa ok");
    assert_eq!(out["update"], "Updated QA f1.txt (id f1).");
    assert_eq!(
        out["target"],
        j(r#"{"target":"QA f1.txt (id f1)","kind":"drive"}"#)
    );
    assert_eq!(
        up.seen()
            .into_iter()
            .filter(|s| s.method == "PATCH")
            .count(),
        1
    );
}
