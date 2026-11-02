//! #2303: actual authenticated native dispatch with synthetic fact/launch owners only.
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
#[path = "mcp_support/mod.rs"]
mod support;
use serde_json::{json, Value};
use support::*;
const PATH: &str = "/api/models/calibration";
const PREFIX: &str = "/internal/calibration-routes/";
const ASK: &str = r#"{"model":"synthetic","confirmPause":true,"promptBudgetSeconds":60}"#;
fn job() -> Value {
    json!({"id":"j","model":"synthetic","status":"running","steps":[],"originalText":"private","unknown":"private"})
}
fn facts() -> Value {
    json!({"available":true,"token":"a".repeat(64),"starting":false,"job":null,"history":{"synthetic":[{"verifiedCtx":8192}]}})
}
fn listing() -> Value {
    json!({"available":true,"backend":true,"row":{"id":"synthetic","meta":{"n_ctx_train":32768}},"system":false,"rowToken":"b".repeat(64)})
}
fn prepared() -> Value {
    json!({"available":true,"row":{"id":"synthetic","meta":{"n_ctx_train":32768}},"system":false,"profile":{"exists":false,"fields":["parallel","cache-type-k","ctx-size"]},"conservative":{"native":65536,"values":{"parallel":"2","cache-type-k":"q8_0","ctx-size":"8192","unknown":"no"}},"ladderReady":true,"identityReady":true,"identity":{"arch":null,"quant":null,"artifact":"synthetic-artifact","build":null,"profile":"d".repeat(64),"hardware":null},"preparedToken":"c".repeat(64)})
}
async fn rig() -> (tempfile::TempDir, Front, Upstream) {
    let d = tempfile::tempdir().unwrap();
    seed(d.path());
    let up = upstream().await;
    up.set(|r| {
        let v = match r.path.strip_prefix(PREFIX) {
            Some("facts") => facts(),
            Some("listing") => listing(),
            Some("prepare") => prepared(),
            Some("launch") => json!({"available":true,"applied":true,"job":job()}),
            _ => return reply("500 WrongPath", "{}"),
        };
        reply("200 OK", &v.to_string())
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
async fn native_plan_and_private_response_are_not_node_public_handler_forwarding() {
    let (_d, f, up) = rig().await;
    let (s, v) = write(f.addr, "POST", PATH, ADMIN, ASK).await;
    assert_eq!(s, 202, "{v}");
    assert!(v.get("originalText").is_none());
    assert!(v.get("unknown").is_none());
    let seen = up.seen();
    let launch = seen
        .iter()
        .find(|r| r.path == format!("{PREFIX}launch"))
        .unwrap();
    assert_eq!(
        j(&launch.body),
        json!({"model":"synthetic","token":"a".repeat(64),"rowToken":"b".repeat(64),"preparedToken":"c".repeat(64),"plan":{"promptBudgetSeconds":60,"native":32768,"base":{"parallel":"2","cache-type-k":"q8_0"}}})
    );
    for r in seen {
        assert!(r.path.starts_with(PREFIX));
        assert_eq!(
            r.header("x-noevia-store-token").as_deref(),
            Some(STORE_TOKEN)
        );
        assert_eq!(r.header("cookie"), None);
        assert_eq!(r.header("x-forwarded-for"), None);
        assert_eq!(r.header("x-noevia-client-ip"), None);
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_selects_history_and_sanitizes_job_but_has_no_launch_or_metadata_reads() {
    let (_d, f, up) = rig().await;
    up.set(|_| {
        let mut v = facts();
        v["job"] = job();
        reply("200 OK", &v.to_string())
    });
    let (s, v) = get(f.addr, &format!("{PATH}?model=synthetic"), ADMIN).await;
    assert_eq!(s, 200);
    assert_eq!(v["history"], json!([{"verifiedCtx":8192}]));
    assert!(v["job"].get("originalText").is_none());
    let (_, v) = get(f.addr, PATH, ADMIN).await;
    assert!(v.get("history").is_none());
    assert_eq!(up.seen().len(), 2);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auth_role_account_and_csrf_precede_every_fact_operation() {
    let (_d, f, up) = rig().await;
    assert_eq!(get(f.addr, PATH, "").await.0, 401);
    assert_eq!(get(f.addr, PATH, MEMBER).await.0, 403);
    assert_eq!(write(f.addr, "POST", PATH, MEMBER, ASK).await.0, 403);
    assert_eq!(raw(f.addr, "POST", PATH, ADMIN, "", b"").await.0, 403);
    assert!(up.seen().is_empty());
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn availability_method_confirmation_and_budget_order_skip_all_metadata_and_launches() {
    let (_d, f, up) = rig().await;
    for method in ["HEAD", "PUT", "DELETE", "OPTIONS"] {
        assert_eq!(write(f.addr, method, PATH, ADMIN, "{bad").await.0, 405);
    }
    for (ask, message) in [
        (r#"{}"#, "Confirm that chat can pause"),
        (
            r#"{"confirmPause":true,"promptBudgetSeconds":14}"#,
            "Choose a prompt time limit",
        ),
    ] {
        let (s, v) = write(f.addr, "POST", PATH, ADMIN, ask).await;
        assert_eq!(s, 400);
        assert!(v["error"].as_str().unwrap().starts_with(message));
    }
    assert_eq!(up.count(&format!("{PREFIX}listing")), 0);
    assert_eq!(up.count(&format!("{PREFIX}launch")), 0);
    up.set(|_| reply("200 OK", r#"{"available":false}"#));
    assert_eq!(write(f.addr, "POST", PATH, ADMIN, "{bad").await.0, 404);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn busy_missing_system_and_backend_failures_do_not_prepare_or_launch() {
    let (_d, f, up) = rig().await;
    up.set(|_| {
        let mut v = facts();
        v["starting"] = json!(true);
        reply("200 OK", &v.to_string())
    });
    assert_eq!(write(f.addr, "POST", PATH, ADMIN, ASK).await.0, 409);
    for (change, status) in [
        (json!({"backend":false}), 502),
        (json!({"row":null}), 404),
        (json!({"system":true}), 400),
    ] {
        up.set(move |r| {
            let mut v = if r.path.ends_with("/facts") {
                facts()
            } else {
                listing()
            };
            if !r.path.ends_with("/facts") {
                for (k, x) in change.as_object().unwrap() {
                    v[k] = x.clone();
                }
            }
            reply("200 OK", &v.to_string())
        });
        assert_eq!(write(f.addr, "POST", PATH, ADMIN, ASK).await.0, status);
    }
    assert_eq!(up.count(&format!("{PREFIX}prepare")), 0);
    assert_eq!(up.count(&format!("{PREFIX}launch")), 0);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_launch_is_not_retried_and_gate_credentials_are_server_unavailable() {
    let (_d, f, up) = rig().await;
    for (result, status) in [
        (json!({"applied":false,"reason":"stale"}), 409),
        (
            json!({"applied":false,"reason":"hold","hold":{"status":403}}),
            503,
        ),
        (
            json!({"applied":false,"reason":"hold","hold":{"status":409}}),
            409,
        ),
    ] {
        up.set(move |r| {
            let v = if r.path.ends_with("/facts") {
                facts()
            } else if r.path.ends_with("/listing") {
                listing()
            } else if r.path.ends_with("/prepare") {
                prepared()
            } else {
                result.clone()
            };
            reply("200 OK", &v.to_string())
        });
        let before = up.count(&format!("{PREFIX}launch"));
        assert_eq!(write(f.addr, "POST", PATH, ADMIN, ASK).await.0, status);
        assert_eq!(up.count(&format!("{PREFIX}launch")) - before, 1);
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bad_actor_schema_and_lone_surrogate_selector_fail_closed() {
    let (_d, f, up) = rig().await;
    assert_eq!(
        write(
            f.addr,
            "POST",
            PATH,
            ADMIN,
            r#"{"model":"m\ud800","confirmPause":true}"#
        )
        .await
        .0,
        400
    );
    assert_eq!(up.count(&format!("{PREFIX}listing")), 0);
    up.set(|_| reply("200 OK", r#"{"available":true}"#));
    assert_eq!(get(f.addr, PATH, ADMIN).await.0, 503);
    assert_eq!(write(f.addr, "POST", PATH, ADMIN, ASK).await.0, 503);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unavailable_or_malformed_actual_identity_cannot_launch_a_worker() {
    let (_d, f, up) = rig().await;
    for identity in [Value::Null, json!({"artifact":"only-one-field"})] {
        up.set(move |r| {
            let mut v = if r.path.ends_with("/facts") {
                facts()
            } else if r.path.ends_with("/listing") {
                listing()
            } else {
                prepared()
            };
            if r.path.ends_with("/prepare") {
                v["identity"] = identity.clone();
            }
            reply("200 OK", &v.to_string())
        });
        assert_eq!(write(f.addr, "POST", PATH, ADMIN, ASK).await.0, 503);
    }
    assert_eq!(up.count(&format!("{PREFIX}launch")), 0);
}
