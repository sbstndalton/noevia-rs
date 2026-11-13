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
use std::time::Duration;

// The golden account file of tests/gdrive.rs (core's gdrive.cjs sealer, key [7;32]) for account A.
const NODE_FILE: &str = "AQIDBAUGBwgJCgsMgIgxgDi+UwWrjb2pgZHTVk5Oe2S+sxp0cQhofG56mOyqJu4MKObbx2YjiOIp95XFlLRpoFsgDgJ1/IeS38BvXz/+MZVPwozLiOOvRCG99kgyZhbaVBN47NmtMEBpqgbKbH3pg0zMV/11xgL0BXiHW9DA/DYPs9ifgguMcyvGCrLfU3M/fg5aQqY2xrOOpQtYrhaJqw43qj4B6g==";

const FILE: &str =
    r#"{"id":"f1","name":"QA f1.txt","mimeType":"text/plain","size":"5","version":"7"}"#;

fn google(up: &Upstream) {
    up.set(|r: &Seen| {
        let path = r.path.split('?').next().unwrap_or("");
        match (r.method.as_str(), path) {
            ("POST", "/token") => {
                reply("200 OK", r#"{"access_token":"access-1","expires_in":3600}"#)
            }
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
