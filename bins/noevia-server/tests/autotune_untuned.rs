//! #2295: real native auth/dispatch with synthetic raw owner facts, never an engine.
#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
#[path = "mcp_support/mod.rs"]
mod support;
use serde_json::{json, Value};
use support::*;
const PATH: &str = "/api/models/autotune/untuned";
const FACTS: &str = "/internal/autotune-untuned/facts";
fn facts() -> Value {
    let cases = js_json::wtf16::parse(include_str!(
        "../../../crates/autotune-plan/tests/fixtures/untuned-node.json"
    ))
    .unwrap();
    let js_json::JValue::Arr(cases) = cases else {
        panic!("cases")
    };
    let case = cases
        .iter()
        .find(|c| c.get("name").as_str() == Some("ordered-scan"))
        .unwrap();
    let js_json::JValue::Arr(rows) = case.get("input").get("rows") else {
        panic!("rows")
    };
    let row = rows
        .iter()
        .find(|r| r.get("id").as_str() == Some("new"))
        .unwrap();
    let row: Value = serde_json::from_str(&js_json::wtf16::stringify(row).unwrap()).unwrap();
    json!({"available":true,"version":1,"rows":[row]})
}
fn answer(r: &Seen) -> (std::time::Duration, String) {
    if j(&r.body)["scan"] == true {
        reply("200 OK", &facts().to_string())
    } else {
        reply("200 OK", r#"{"available":true}"#)
    }
}
async fn rig() -> (tempfile::TempDir, Front, Upstream) {
    let d = tempfile::tempdir().unwrap();
    seed(d.path());
    let up = upstream().await;
    up.set(|r| {
        if r.path == FACTS {
            answer(r)
        } else {
            reply("500 WrongPath", "{}")
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
async fn native_scan_matches_shared_policy_and_never_uses_browser_model_selector() {
    let (_d, f, up) = rig().await;
    assert_eq!(
        get(f.addr, &format!("{PATH}?model=other"), ADMIN).await,
        (200, json!({"models":["new"],"skipped":[]}))
    );
    let seen = up.seen();
    assert_eq!(seen.len(), 2);
    assert_eq!(j(&seen[0].body), json!({"scan":false}));
    assert_eq!(j(&seen[1].body), json!({"scan":true}));
    for r in seen {
        assert_eq!(r.path, FACTS);
        assert_eq!(r.header("cookie"), None);
        assert_eq!(r.header("x-noevia-client-ip"), None);
        assert_eq!(
            r.header("x-noevia-store-token").as_deref(),
            Some(STORE_TOKEN)
        );
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auth_role_csrf_before_facts_and_non_get_methods_only_check_availability() {
    let (_d, f, up) = rig().await;
    assert_eq!(get(f.addr, PATH, "").await.0, 401);
    assert_eq!(get(f.addr, PATH, MEMBER).await.0, 403);
    assert_eq!(write(f.addr, "POST", PATH, MEMBER, "").await.0, 403);
    assert_eq!(raw(f.addr, "POST", PATH, ADMIN, "", b"").await.0, 403);
    assert!(up.seen().is_empty());
    for m in ["HEAD", "POST", "PUT", "DELETE", "OPTIONS"] {
        assert_eq!(write(f.addr, m, PATH, ADMIN, "{malformed").await.0, 405);
    }
    assert!(up
        .seen()
        .iter()
        .all(|r| j(&r.body) == json!({"scan":false})));
    up.set(|_| reply("200 OK", r#"{"available":false}"#));
    assert_eq!(get(f.addr, PATH, ADMIN).await.0, 404);
    assert_eq!(write(f.addr, "POST", PATH, ADMIN, "").await.0, 404);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_future_and_unknown_identity_cannot_return_a_scan() {
    let (_d, f, up) = rig().await;
    for mode in [0, 1, 2, 3] {
        up.set(move |r| {
            if j(&r.body)["scan"] != true {
                return reply("200 OK", r#"{"available":true}"#);
            }
            let mut v = facts();
            match mode {
                0 => v["version"] = json!(2),
                1 => v["rows"][0]["identity"] = Value::Null,
                2 => v["rows"][0]["latest"] = json!({"version":4,"signature":"a".repeat(64)}),
                _ => v["rows"][0]["identity"]["extra"] = json!("private"),
            };
            reply("200 OK", &v.to_string())
        });
        assert_eq!(
            get(f.addr, PATH, ADMIN).await,
            (502, json!({"error":"Could not list untuned models."}))
        );
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn only_closed_owner_failure_reasons_are_public_and_transport_failure_is_generic() {
    let (_d, f, up) = rig().await;
    for (reason, message) in [
        ("router", "The model server is not responding."),
        ("identity", "Model identity could not be read."),
        (
            "http://private/?token=secret",
            "Could not list untuned models.",
        ),
    ] {
        up.set(move |r| {
            if j(&r.body)["scan"] == true {
                reply("503 Unavailable", &json!({"failure":reason}).to_string())
            } else {
                reply("200 OK", r#"{"available":true}"#)
            }
        });
        assert_eq!(
            get(f.addr, PATH, ADMIN).await,
            (502, json!({"error":message}))
        );
    }
}
