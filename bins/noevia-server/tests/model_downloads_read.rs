//! Synthetic download facts from a loopback owner; no engine or tracker process.
#![allow(clippy::unwrap_used)]
#[path = "mcp_support/mod.rs"]
mod support;
use serde_json::json;
use support::*;

const PATH: &str = "/api/models/downloads";
const FACTS: &str = "/internal/model-downloads-read/facts";

async fn rig() -> (tempfile::TempDir, Front, Upstream) {
    let d = tempfile::tempdir().unwrap();
    seed(d.path());
    let up = upstream().await;
    let front = front_env(
        d.path(),
        true,
        &[
            ("NOEVIA_LEGACY_UPSTREAM", up.base()),
            ("NOEVIA_PROJECT_STORE_TOKEN", STORE_TOKEN.into()),
        ],
    )
    .await;
    (d, front, up)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exact_get_accepts_members_and_applies_js_field_rules() {
    let (_d, front, up) = rig().await;
    up.set(|_| {
        reply(
            "200 OK",
            r#"{"kind":"listing","jobs":[
              {"fields":{"id":0,"job_id":"one","model_name":false,"model":"","checkpoint":"\ud800","percent":0,"progress":0.9,"status":"","state":"running","private":"hidden"}},
              {"fields":{"id":"ignored","model_name":{},"progress":0.7,"state":true},"infinite":["id","percent"]},
              {"fields":{"percent":"50","progress":0.3}}
            ],"downloads":[{"fields":{"id":"ignored listing"}}]}"#,
        )
    });
    assert_eq!(get(front.addr, PATH, "").await.0, 401);
    assert!(up.seen().is_empty());
    let (status, headers, body) = raw_text(front.addr, "GET", PATH, MEMBER, "", b"").await;
    assert_eq!(status, 200);
    assert!(headers.contains("cache-control: no-store"));
    assert_eq!(
        body,
        r#"[{"id":"one","model":"\ud800","progress":0,"status":"running"},{"id":null,"model":{},"progress":null,"status":true},{"id":"","model":"","progress":0.3,"status":""}]"#
    );
    let seen = up.seen();
    let [request] = seen.as_slice() else {
        panic!("expected exactly one owner call");
    };
    assert_eq!(request.path, FACTS);
    assert_eq!(
        request.header("x-noevia-store-token").as_deref(),
        Some(STORE_TOKEN)
    );
    assert_eq!(request.header("cookie"), None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn owner_shapes_and_failures_preserve_status() {
    let (_d, front, up) = rig().await;
    for (facts, status, expected) in [
        (json!({"kind":"disabled"}), 200, json!([])),
        (
            json!({"kind":"listing","direct":[{"fields":{"id":"direct","percent":40}}]}),
            200,
            json!([{"id":"direct","model":"","progress":0.4,"status":""}]),
        ),
        (
            json!({"kind":"listing","jobs":false,"downloads":[{"fields":{"id":"fallback"}}]}),
            200,
            json!([{"id":"fallback","model":"","progress":null,"status":""}]),
        ),
        (
            json!({"kind":"listing","jobs":[],"downloads":true}),
            200,
            json!([]),
        ),
        (
            json!({"kind":"failed"}),
            502,
            json!({"error":"Download status unavailable"}),
        ),
        (
            json!({"kind":"fault"}),
            500,
            json!({"error":"Internal error"}),
        ),
        (
            json!({"kind":"listing","jobs":true}),
            500,
            json!({"error":"Internal error"}),
        ),
        (
            json!({"kind":"listing","jobs":[null]}),
            500,
            json!({"error":"Internal error"}),
        ),
    ] {
        up.set(move |_| reply("200 OK", &facts.to_string()));
        let (actual, _, body) = raw_text(front.addr, "GET", PATH, ADMIN, "", b"").await;
        assert_eq!(actual, status);
        assert_eq!(j(&body), expected);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn head_other_verbs_and_trailing_path_stay_with_node() {
    let (_d, front, up) = rig().await;
    up.set(|_| reply_with("202 NodeMethod", "X-Node-Method: yes\r\n", "{}"));
    for (method, path) in [
        ("HEAD", PATH),
        ("POST", PATH),
        ("GET", "/api/models/downloads/"),
    ] {
        let (status, headers, _) = raw_text(front.addr, method, path, ADMIN, "", b"").await;
        assert_eq!(status, 202, "{method} {path}");
        assert!(headers.contains("x-node-method: yes"));
    }
    assert!(up.seen().iter().all(|req| req.path != FACTS));
}
