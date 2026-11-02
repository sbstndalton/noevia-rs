//! #2281: real front/session/Drive state, synthetic raw Node facts; no discovery or model calls.
#![cfg(feature = "rust-storage-reads")]
#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
#[path = "mcp_support/mod.rs"]
mod support;
use serde_json::{json, Value};
use support::*;
const PATH: &str = "/api/toolboxes";
const FACTS: &str = "/internal/toolbox-picker/facts";
fn facts() -> Value {
    json!({"connectorBoxes":["gdrive"],"boxes":[
        {"id":"core","source":"builtin","tools":[]},
        {"id":"gdrive","source":"builtin","tools":[{"type":"function","function":{"name":"drive_read"}}]},
        {"id":"curated","source":"mcp","inApp":true,"tools":[]}
    ], "prefill":{"targetMs":14000,"models":{"synthetic":{"rate":0.5}}},"mcp":{"enabled":false}})
}
async fn rig() -> (tempfile::TempDir, Front, Upstream) {
    let d = tempfile::tempdir().unwrap();
    let _db = seed(d.path());
    let up = upstream().await;
    up.set(|_| reply("200 OK", &facts().to_string()));
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
    (d, f, up)
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn owns_the_response_and_only_requests_trusted_raw_facts_without_account_inputs() {
    let (_d, f, up) = rig().await;
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
            {"id":"core","source":"builtin","inApp":true,"toolCount":0,"estTokens":0},
            {"id":"curated","source":"mcp","inApp":true,"toolCount":0,"estTokens":0}
        ])
    );
    assert_eq!(v["prefill"], facts()["prefill"]);
    assert_eq!(v["mcp"], json!({"configured":false}));
    let calls = up.seen();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].path, FACTS);
    assert_eq!(calls[0].method, "POST");
    assert_eq!(j(&calls[0].body), json!({}));
    assert_eq!(
        calls[0].header("x-noevia-store-token").as_deref(),
        Some(STORE_TOKEN)
    );
    assert_eq!(calls[0].header("cookie"), None);
    assert_eq!(calls[0].header("x-noevia-client-ip"), None);
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
    let (_d, f, up) = rig().await;
    for (status, body) in [
        ("503 Unavailable", "private-token"),
        ("200 OK", "not JSON private-token"),
        ("200 OK", "{}"),
        ("200 OK", "null"),
    ] {
        up.set(move |_| reply(status, body));
        let (s, v) = get(f.addr, PATH, ADMIN).await;
        assert_eq!(s, 503, "{v}");
        assert!(!v.to_string().contains("private-token"));
    }
}
