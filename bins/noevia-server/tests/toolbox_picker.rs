//! #2281: real front/session/Drive state, synthetic raw Node facts; no discovery or model calls.
#![cfg(feature = "rust-storage-reads")]
#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
#[path = "mcp_support/mod.rs"]
mod support;
use serde_json::{json, Value};
use support::*;
const PATH: &str = "/api/toolboxes";
fn tool(name: &str) -> Value {
    json!({"name": name, "description": "synthetic", "write": false})
}
/// The catalogue the front is given: the chat process's built-in boxes and the front's own discovery (mcp_support write_snapshot).
fn facts() -> Value {
    json!({"connectorBoxes":["gdrive"],"boxes":[
        {"id":"core","label":"core","description":"d","source":"builtin","tools":[]},
        {"id":"gdrive","label":"gdrive","description":"d","source":"builtin","tools":[tool("drive_read")]},
        {"id":"curated","label":"curated","description":"d","source":"mcp","inApp":true,"server":"nextcloud","tools":[]}
    ], "prefill":{"targetMs":14000,"models":{"synthetic":{"rate":0.5}}},"mcpEnabled":false})
}
async fn rig() -> (tempfile::TempDir, Front, Upstream) {
    let d = tempfile::tempdir().unwrap();
    let _db = seed(d.path());
    let up = upstream().await;
    let f = front_env(
        d.path(),
        true,
        &[
            ("NOEVIA_LEGACY_UPSTREAM", up.base()),
            ("NOEVIA_PROJECT_STORE_TOKEN", STORE_TOKEN.into()),
            ("GOOGLE_OAUTH_CLIENT_ID", "synthetic-client".into()),
            ("GOOGLE_OAUTH_CLIENT_SECRET", "synthetic-secret".into()),
        ],
    )
    .await;
    write_snapshot(&f, d.path(), facts());
    (d, f, up)
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn owns_the_response_and_asks_node_for_nothing_no_matter_the_account_inputs() {
    let (d, f, up) = rig().await;
    let (s, v) = get(
        f.addr,
        "/api/toolboxes?userId=other&connected=gdrive",
        ADMIN,
    )
    .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(
        v["toolboxes"],
        json!([
            {"id":"core","label":"core","description":"d","source":"builtin","inApp":true,"toolCount":0,"estTokens":0},
            {"id":"curated","label":"curated","description":"d","source":"mcp","inApp":true,"toolCount":0,"estTokens":0}
        ])
    );
    assert_eq!(v["prefill"], facts()["prefill"]);
    assert_eq!(v["mcp"], json!({"configured":false}));
    // Nothing is asked of Node per request (the facts bridge is gone); the front only asked it to publish.
    assert!(up.seen().is_empty());
    assert!(d.path().join("snapshot-demand").exists());
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn not_provably_current_facts_are_not_ready_never_stale() {
    let (d, f, _up) = rig().await;
    assert_eq!(get(f.addr, PATH, ADMIN).await.0, 200);
    let file = d.path().join("toolbox-chat-facts.json");
    let stale = std::fs::read_to_string(&file)
        .unwrap()
        .replacen("\"writtenAt\":", "\"writtenAt\":1,\"was\":", 1);
    std::fs::write(&file, stale).unwrap();
    let (s, _) = get(f.addr, PATH, ADMIN).await;
    assert_eq!(s, 503);
    std::fs::remove_file(&file).unwrap();
    assert_eq!(get(f.addr, PATH, ADMIN).await.0, 503);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn authentication_and_csrf_precede_method_refusal_and_never_call_node() {
    let (_d, f, up) = rig().await;
    assert_eq!(get(f.addr, PATH, "").await.0, 401);
    assert_eq!(raw(f.addr, "POST", PATH, ADMIN, "", b"").await.0, 403);
    for method in ["POST", "DELETE", "PUT", "HEAD", "OPTIONS", "PATCH"] {
        assert_eq!(
            write(f.addr, method, PATH, ADMIN, "").await.0,
            405,
            "{method}"
        );
    }
    assert!(up.seen().is_empty());
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn current_account_drive_connection_is_read_each_time_and_never_shared() {
    let (d, f, _up) = rig().await;
    let dir = d.path().join("google-drive-users");
    std::fs::create_dir_all(&dir).unwrap();
    let sealed = google_token_seal::Sealer::new(
        google_token_seal::derive_secret_key(&KEY, "google-drive-user").as_slice(),
    )
    .unwrap()
    .seal(
        br#"{"refreshToken":"synthetic-refresh","email":"ada@example.test","connectedAt":1}"#,
        &[1; 12],
    )
    .unwrap();
    std::fs::write(dir.join(format!("{A}.sealed")), sealed).unwrap();
    let (_, a) = get(f.addr, PATH, ADMIN).await;
    assert!(a["toolboxes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|b| b["id"] == "gdrive" && b["connector"] == true));
    let (_, b) = get(
        f.addr,
        "/api/toolboxes?userId=aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
        MEMBER,
    )
    .await;
    assert!(!b["toolboxes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|b| b["id"] == "gdrive"));
    std::fs::remove_file(dir.join(format!("{A}.sealed"))).unwrap();
    let (_, a) = get(f.addr, PATH, ADMIN).await;
    assert!(!a["toolboxes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|b| b["id"] == "gdrive"));
    assert!(!a.to_string().contains("synthetic-refresh"));
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unavailable_or_malformed_facts_fail_closed_without_echoing_private_data() {
    let (d, f, _up) = rig().await;
    let file = d.path().join("toolbox-chat-facts.json");
    let good = std::fs::read_to_string(&file).unwrap();
    let now = noevia_server::identity::now_ms();
    for text in [
        "{not json private-token".to_owned(),
        "null".to_owned(),
        json!({"version": 1, "kind": "toolbox-chat-facts", "writtenAt": now, "everyMs": 3000, "builtin": "private-token"}).to_string(),
        good.replacen("\"connectorBoxes\":[\"gdrive\"]", "\"connectorBoxes\":[\"private-token\",5]", 1),
    ] {
        std::fs::write(&file, text).unwrap();
        let (s, v) = get(f.addr, PATH, ADMIN).await;
        assert_eq!(s, 503, "{v}");
        assert!(!v.to_string().contains("private-token"));
    }
}
