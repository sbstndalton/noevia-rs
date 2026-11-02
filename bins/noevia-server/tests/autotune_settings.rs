//! #2287: the real front/identity gate with synthetic live actor facts and conditional commits.
#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
#[path = "mcp_support/mod.rs"]
mod support;
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use support::*;
const PATH: &str = "/api/models/autotune/settings";
const FACTS: &str = "/internal/autotune-settings/facts";
const COMMIT: &str = "/internal/autotune-settings/commit";
fn sample() -> Value {
    let cases: Value = serde_json::from_str(include_str!(
        "../../../crates/server-projects/tests/fixtures/autotune-settings-node.json"
    ))
    .unwrap();
    cases[1].clone()
}
async fn rig() -> (tempfile::TempDir, Front, Upstream) {
    let d = tempfile::tempdir().unwrap();
    seed(d.path());
    let up = upstream().await;
    up.set(|r| {
        let case = sample();
        if r.path == FACTS {
            let mut body = json!({"available":true,"settingsAvailable":true});
            if j(&r.body).get("model").is_some() {
                body["facts"] = case["facts"].clone();
            }
            reply("200 OK", &body.to_string())
        } else if r.path == COMMIT {
            reply(
                "200 OK",
                &json!({"applied":true,"facts":case["finalFacts"]}).to_string(),
            )
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
async fn public_response_is_native_and_commit_receives_only_the_checked_map_change() {
    let (_d, f, up) = rig().await;
    let (s, v) = write(
        f.addr,
        "POST",
        PATH,
        ADMIN,
        r#"{"model":"synthetic","allowQ5Kv":true}"#,
    )
    .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v, sample()["expected"]["body"]);
    let seen = up.seen();
    assert_eq!(seen.len(), 3);
    assert_eq!(seen[0].path, FACTS);
    assert_eq!(j(&seen[0].body), json!({}));
    assert_eq!(j(&seen[1].body), json!({"model":"synthetic"}));
    assert_eq!(
        j(&seen[2].body),
        json!({"model":"synthetic","allowQ5Kv":true,"token":sample()["facts"]["token"]})
    );
    for call in seen {
        assert_eq!(
            call.header("x-noevia-store-token").as_deref(),
            Some(STORE_TOKEN)
        );
        assert_eq!(call.header("cookie"), None);
        assert_eq!(call.header("x-noevia-client-ip"), None);
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn account_role_and_csrf_are_checked_before_any_actor_call() {
    let (_d, f, up) = rig().await;
    assert_eq!(get(f.addr, PATH, "").await.0, 401);
    assert_eq!(write(f.addr, "POST", PATH, MEMBER, "{}").await.0, 403);
    assert_eq!(raw(f.addr, "POST", PATH, ADMIN, "", b"{}").await.0, 403);
    assert!(up.seen().is_empty());
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn method_availability_body_and_unfinished_job_precedence_are_preserved() {
    let (_d, f, up) = rig().await;
    for method in ["GET", "HEAD", "PUT", "DELETE", "OPTIONS"] {
        assert_eq!(write(f.addr, method, PATH, ADMIN, "").await.0, 405);
    }
    for body in ["{broken", r#"{"model":"synthetic","allowQ5Kv":"true"}"#] {
        assert_eq!(write(f.addr, "POST", PATH, ADMIN, body).await.0, 400);
    }
    let huge = format!(
        r#"{{"model":"synthetic","allowQ5Kv":true,"padding":"{}"}}"#,
        "x".repeat(1024 * 1024)
    );
    assert_eq!(write(f.addr, "POST", PATH, ADMIN, &huge).await.0, 413);
    assert_eq!(up.count(COMMIT), 0);
    up.set(|_| reply("200 OK", r#"{"available":false,"settingsAvailable":false}"#));
    assert_eq!(
        get(f.addr, PATH, ADMIN).await.0,
        404,
        "unavailable precedes method guard"
    );
    up.set(|_| reply("200 OK", r#"{"available":true,"settingsAvailable":false}"#));
    assert_eq!(get(f.addr, PATH, ADMIN).await.0, 405);
    assert_eq!(write(f.addr, "POST", PATH, ADMIN, "{}").await.0, 404);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_job_start_between_facts_and_commit_is_revalidated_not_overwritten() {
    let (_d, f, up) = rig().await;
    let commits = Arc::new(AtomicUsize::new(0));
    let c = Arc::clone(&commits);
    up.set(move|r|{
        if r.path==COMMIT {c.fetch_add(1,Ordering::SeqCst);return reply("409 Changed",r#"{"applied":false}"#);}
        let mut answer=json!({"available":true,"settingsAvailable":true});
        if j(&r.body).get("model").is_some() {let mut facts=sample()["facts"].clone();if c.load(Ordering::SeqCst)>0 {facts["guard"]["job"]=json!({"id":"new-job","status":"running","models":[{"model":"synthetic-long","base":"synthetic","status":"pending"}]});}answer["facts"]=facts;}
        reply("200 OK",&answer.to_string())
    });
    let (s, v) = write(
        f.addr,
        "POST",
        PATH,
        ADMIN,
        r#"{"model":"synthetic","allowQ5Kv":true}"#,
    )
    .await;
    assert_eq!(s, 409, "{v}");
    assert!(v["error"].as_str().unwrap().contains("unfinished tune"));
    assert_eq!(commits.load(Ordering::SeqCst), 1);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn conflicting_snapshots_are_bounded_and_backend_failures_do_not_echo_details() {
    let (_d, f, up) = rig().await;
    up.set(|r| {
        if r.path == COMMIT {
            reply("409 Changed", r#"{"applied":false}"#)
        } else {
            let mut answer = json!({"available":true,"settingsAvailable":true});
            if j(&r.body).get("model").is_some() {
                answer["facts"] = sample()["facts"].clone();
            }
            reply("200 OK", &answer.to_string())
        }
    });
    assert_eq!(
        write(
            f.addr,
            "POST",
            PATH,
            ADMIN,
            r#"{"model":"synthetic","allowQ5Kv":true}"#
        )
        .await
        .0,
        409
    );
    assert_eq!(up.count(COMMIT), 3);
    up.set(|_| {
        reply(
            "503 Unavailable",
            r#"{"error":"never-expose-private-state"}"#,
        )
    });
    let (s, v) = write(f.addr, "POST", PATH, ADMIN, "{}").await;
    assert_eq!(s, 503);
    assert!(!v.to_string().contains("never-expose"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lone_surrogates_cannot_alias_a_configured_literal_private_use_model() {
    let (_d, f, up) = rig().await;
    // A hypothetical actor has a literal U+10F800 model. The escaped surrogate is a different
    // JavaScript ID and must be rejected before serde_json can turn its sentinel into that model.
    up.set(|r| {
        let mut answer = json!({"available": true, "settingsAvailable": true});
        if j(&r.body).get("model").is_some() {
            answer["facts"] = sample()["facts"].clone();
        }
        reply("200 OK", &answer.to_string())
    });
    let (status, value) = write(
        f.addr,
        "POST",
        PATH,
        ADMIN,
        r#"{"model":"\ud800","allowQ5Kv":true}"#,
    )
    .await;
    assert_eq!(status, 400, "{value}");
    assert_eq!(value["error"], "Choose a model.");
    let seen = up.seen();
    assert_eq!(seen.len(), 1, "only availability facts may be requested");
    assert_eq!(j(&seen[0].body), json!({}));
    assert_eq!(up.count(COMMIT), 0);
}
