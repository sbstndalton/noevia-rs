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
            r#"{"kind":"listing","rows":[
              {"fields":{"id":0,"job_id":"one","model_name":false,"model":"","checkpoint":"\ud800","percent":0,"progress":0.9,"status":"","state":"running","private":"hidden"}},
              {"fields":{"id":"ignored","model_name":{},"progress":0.7,"state":true},"infinite":["id","percent"]},
              {"fields":{"percent":"50","progress":0.3}}
            ]}"#,
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
        (json!({"kind":"listing","rows":[]}), 200, json!([])),
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
            json!({"kind":"listing","rows":true}),
            500,
            json!({"error":"Internal error"}),
        ),
        (
            json!({"kind":"listing"}),
            500,
            json!({"error":"Internal error"}),
        ),
        (
            json!({"kind":"listing","rows":[null]}),
            500,
            json!({"error":"Internal error"}),
        ),
        (
            json!({"kind":"unknown"}),
            502,
            json!({"error":"Download status unavailable"}),
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_answer_past_the_bridge_cap_is_staged_whole_and_sent_exactly() {
    let (_d, front, up) = rig().await;
    // Core never capped this answer; the old 16 MiB bridge cap must not refuse it.
    let id = "i".repeat(17 * 1024 * 1024);
    let unchosen = "j".repeat(17 * 1024 * 1024);
    let facts = format!(
        r#"{{"kind":"listing","rows":[{{"fields":{{"id":"{id}","job_id":"{unchosen}","state":"done"}}}}]}}"#
    );
    up.set(move |_| reply("200 OK", &facts));
    let (status, headers, body) = raw_text(front.addr, "GET", PATH, MEMBER, "", b"").await;
    assert_eq!(status, 200);
    let expected = format!(r#"[{{"id":"{id}","model":"","progress":null,"status":"done"}}]"#);
    assert!(headers.contains(&format!("content-length: {}", expected.len())));
    assert!(headers.contains("cache-control: no-store"));
    assert!(body == expected, "the staged answer differs");
    assert_eq!(up.count(FACTS), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fault_while_staging_is_the_generic_500_with_nothing_of_the_listing() {
    let (_d, front, up) = rig().await;
    // The owner answers 200 and then breaks off mid-body.
    up.set(|_| {
        (
            std::time::Duration::ZERO,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 100000\r\nConnection: close\r\n\r\n{\"kind\":\"listing\",\"rows\":[{\"fields\":{\"id\":\"partial\"}}".to_owned(),
        )
    });
    let (status, headers, body) = raw_text(front.addr, "GET", PATH, MEMBER, "", b"").await;
    assert_eq!(status, 500);
    assert_eq!(j(&body), json!({"error":"Internal error"}));
    assert!(!body.contains("partial"));
    assert!(headers.contains("cache-control: no-store"));
    // A non-200 owner answer, or none at all, stays the unavailable 502.
    up.set(|_| reply("404 Not Found", r#"{"error":"Not found"}"#));
    let (status, _, body) = raw_text(front.addr, "GET", PATH, MEMBER, "", b"").await;
    assert_eq!(status, 502);
    assert_eq!(j(&body), json!({"error":"Download status unavailable"}));
    assert_eq!(up.count(FACTS), 2, "one owner call per request, no retry");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_client_that_leaves_mid_call_costs_one_owner_call_and_the_next_read_is_whole() {
    use tokio::io::AsyncWriteExt;
    let (_d, front, up) = rig().await;
    up.set(|_| {
        let mut slow = reply("200 OK", r#"{"kind":"listing","rows":[]}"#);
        slow.0 = std::time::Duration::from_secs(3);
        slow
    });
    let mut s = tokio::net::TcpStream::connect(front.addr).await.unwrap();
    s.write_all(
        format!(
            "GET {PATH} HTTP/1.1\r\nHost: {}\r\nCookie: {MEMBER}\r\nContent-Length: 0\r\n\r\n",
            front.addr
        )
        .as_bytes(),
    )
    .await
    .unwrap();
    let end = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while up.count(FACTS) == 0 && std::time::Instant::now() < end {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(up.count(FACTS), 1);
    drop(s);
    up.set(|_| {
        reply(
            "200 OK",
            r#"{"kind":"listing","rows":[{"fields":{"id":"after"}}]}"#,
        )
    });
    let (status, _, body) = raw_text(front.addr, "GET", PATH, MEMBER, "", b"").await;
    assert_eq!(status, 200);
    assert_eq!(
        j(&body),
        json!([{"id":"after","model":"","progress":null,"status":""}])
    );
    assert_eq!(
        up.count(FACTS),
        2,
        "no extra poll for the request that left"
    );
}
