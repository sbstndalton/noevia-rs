//! GET-only status against raw synthetic in-memory facts. No engine/model/worker operations.
#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
#[path = "mcp_support/mod.rs"]
mod support;
use serde_json::{json, Value};
use support::*;
const PATH: &str = "/api/models/autotune";
const FACTS: &str = "/internal/autotune-read/facts";
fn facts(keys: &[Value]) -> Value {
    json!({"available":true,"jobPresent":true,"job":{"id":"j","model":"m","status":"running","models":[],"originalText":"never-public","unknown":"never-public"},"allowBelowQ5Kv":false,
 "entries":keys.iter().map(|key|json!({"key":key,"history":if key=="m"{json!([{"at":1,"context":8192}])}else if key=="m-long"{json!([{"at":2,"context":32768}])}else{Value::Null},"allowQ5Kv":key=="m"})).collect::<Vec<_>>()})
}
async fn rig() -> (tempfile::TempDir, Front, Upstream) {
    let d = tempfile::tempdir().unwrap();
    seed(d.path());
    let up = upstream().await;
    up.set(|r| {
        if r.path == FACTS {
            reply(
                "200 OK",
                &facts(j(&r.body)["keys"].as_array().unwrap()).to_string(),
            )
        } else {
            reply_with(
                "202 NodeMethod",
                "X-Node-Method: yes\r\n",
                r#"{"node":true}"#,
            )
        }
    });
    let f = front_env(
        d.path(),
        true,
        &[
            ("NOEVIA_LEGACY_UPSTREAM", up.base()),
            ("NOEVIA_PROJECT_STORE_TOKEN", STORE_TOKEN.into()),
        ],
    )
    .await;
    (d, f, up)
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_get_composes_raw_owner_data_and_requests_exact_model_keys() {
    let (_d, f, up) = rig().await;
    let (s, h, b) = raw_text(
        f.addr,
        "GET",
        &format!("{PATH}?model=m&model=ignored"),
        ADMIN,
        "",
        b"",
    )
    .await;
    assert_eq!(s, 200);
    assert!(h.contains("cache-control: no-store"));
    let body = j(&b);
    assert_eq!(
        body["job"],
        json!({"id":"j","model":"m","status":"running","models":[]})
    );
    assert_eq!(body["history"], json!([{"at":1,"context":8192}]));
    assert_eq!(body["longId"], "m-long");
    assert_eq!(body["longHistory"], json!([{"at":2,"context":32768}]));
    assert_eq!(
        body["kvCandidates"],
        json!(["bf16", "q8_0", "q5_1", "q5_0"])
    );
    assert_eq!(body["settings"], json!({"allowQ5Kv":true}));
    assert_eq!(body["modes"]["long"]["defaultSeconds"], 1800);
    assert!(!b.contains("never-public"));
    let seen = up.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].path, FACTS);
    assert_eq!(j(&seen[0].body), json!({"keys":["m","m-long"]}));
    assert_eq!(
        seen[0].header("x-noevia-store-token").as_deref(),
        Some(STORE_TOKEN)
    );
    for key in ["cookie", "x-noevia-client-ip", "x-forwarded-for"] {
        assert_eq!(seen[0].header(key), None);
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn post_start_and_every_other_verb_remain_node_owned() {
    let (_d, f, up) = rig().await;
    for method in ["POST", "HEAD", "PUT", "PATCH", "DELETE", "OPTIONS"] {
        let (s, h, _) = raw_text(
            f.addr,
            method,
            PATH,
            ADMIN,
            &format!("Origin: http://{}\r\nX-CSRF-Token: csrf\r\n", f.addr),
            br#"{"confirmPause":true,"model":"m"}"#,
        )
        .await;
        assert_eq!(s, 202, "{method}");
        assert!(h.contains("x-node-method: yes"), "{method}");
    }
    assert_eq!(up.count(FACTS), 0);
    assert_eq!(up.count(PATH), 6);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admin_auth_precedes_facts_and_disabled_actor_preserves_not_found() {
    let (_d, f, up) = rig().await;
    assert_eq!(get(f.addr, PATH, "").await.0, 401);
    assert_eq!(
        get(f.addr, PATH, MEMBER).await,
        (
            403,
            json!({"error":"Administrator required for shared model profiles"})
        )
    );
    assert!(up.seen().is_empty());
    up.set(|_| reply("200 OK", r#"{"available":false}"#));
    assert_eq!(
        get(f.addr, PATH, ADMIN).await,
        (404, json!({"error":"Auto-tune is unavailable"}))
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn empty_model_omits_long_fields_and_history_is_limited_natively() {
    let (_d, f, up) = rig().await;
    let (s, body) = get(f.addr, PATH, ADMIN).await;
    assert_eq!(s, 200);
    assert_eq!(body["history"], json!([]));
    assert!(body.get("longId").is_none());
    assert!(body.get("longHistory").is_none());
    assert_eq!(j(&up.seen()[0].body), json!({"keys":[""]}));
    up.set(|r| {
        let mut f = facts(j(&r.body)["keys"].as_array().unwrap());
        f["entries"][0]["history"] = json!((0..12)
            .map(|at| json!({"at":at,"context":8192}))
            .collect::<Vec<_>>());
        reply("200 OK", &f.to_string())
    });
    let (s, body) = get(f.addr, &format!("{PATH}?model=m"), ADMIN).await;
    assert_eq!(s, 200);
    assert_eq!(body["history"].as_array().unwrap().len(), 10);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_or_dead_owner_cannot_become_public_status_or_engine_fallback() {
    let (_d, f, up) = rig().await;
    for bad in [
        json!({}),
        json!({"available":true,"status":200,"body":{"job":{}}}),
        json!({"available":true,"jobPresent":false,"entries":[],"allowBelowQ5Kv":false}),
    ] {
        up.set(move |_| reply("200 OK", &bad.to_string()));
        assert_eq!(get(f.addr, &format!("{PATH}?model=m"), ADMIN).await.0, 503);
    }
    up.set(|r| {
        let mut f = facts(j(&r.body)["keys"].as_array().unwrap());
        f["entries"][0]["history"] = json!([{"quality":{"Access_Token":"never-public"}}]);
        reply("200 OK", &f.to_string())
    });
    let (s, body) = get(f.addr, &format!("{PATH}?model=m"), ADMIN).await;
    assert_eq!(s, 503);
    assert!(!body.to_string().contains("never-public"));
    up.set(|_| reply("503 Down", r#"{"error":"never-public"}"#));
    let (s, body) = get(f.addr, PATH, ADMIN).await;
    assert_eq!(s, 503);
    assert!(!body.to_string().contains("never-public"));
    assert_eq!(up.count(PATH), 0);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lone_utf16_in_status_data_survives_without_becoming_a_model_selector() {
    let (_d, f, up) = rig().await;
    up.set(|r| {
        let f = facts(j(&r.body)["keys"].as_array().unwrap());
        reply(
            "200 OK",
            &f.to_string()
                .replace("\"model\":\"m\"", "\"model\":\"m\\ud800\""),
        )
    });
    let (s, _, body) = raw_text(f.addr, "GET", &format!("{PATH}?model=m"), ADMIN, "", b"").await;
    assert_eq!(s, 200);
    let parsed = js_json::wtf16::parse(&body).unwrap();
    assert_eq!(
        js_json::wtf16::len(parsed.get("job").get("model").as_str().unwrap()),
        2
    );
    assert_eq!(j(&up.seen()[0].body), json!({"keys":["m","m-long"]}));
}
