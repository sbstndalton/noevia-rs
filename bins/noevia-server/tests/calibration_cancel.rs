//! #2292: the public cancel route over the native worker, through the real front: a job the front
//! launched is cancelled by its opaque token only (never a model selector), its profile goes back,
//! and Core is asked nothing. Synthetic router and gate only; no engine or model work.
#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod calibration_support;
#[path = "mcp_support/mod.rs"]
mod support;
use calibration_support as cal;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use support::*;
const PATH: &str = "/api/models/calibration/cancel";
const START: &str = "/api/models/calibration";
const ASK: &str = r#"{"model":"synth","confirmPause":true,"promptBudgetSeconds":60}"#;
/// The public job keys (Node's actor selection; calibration_cancel_routes.rs `public_job`).
const PUBLIC: &[&str] = &[
    "id",
    "model",
    "promptBudgetSeconds",
    "native",
    "base",
    "status",
    "phase",
    "startedAt",
    "steps",
    "memoryFloorGib",
    "memoryGuard",
    "originalRevision",
    "lastRevision",
    "result",
    "error",
    "restored",
    "finishedAt",
];
async fn rig() -> (tempfile::TempDir, Front, Upstream, Arc<cal::Engine>) {
    let d = tempfile::tempdir().unwrap();
    seed(d.path());
    let up = upstream().await;
    up.set(node);
    let (e, mut env) = cal::native(d.path(), true).await;
    env.push(("NOEVIA_LEGACY_UPSTREAM", up.base()));
    env.push(("NOEVIA_PROJECT_STORE_TOKEN", STORE_TOKEN.into()));
    let f = front_env(d.path(), true, &env).await;
    (d, f, up, e)
}
/// Node: its load decision (core model-load-refusal.cjs) lets every load go; anything else is the
/// 404 of an unknown path.
fn node(r: &Seen) -> (Duration, String) {
    if r.method == "POST" && r.path == "/internal/model-maintenance/load-refusal" {
        reply("200 OK", r#"{"refusal":null}"#)
    } else {
        reply("404 Not Found", r#"{"error":"Not found"}"#)
    }
}
fn core_untouched(up: &Upstream) {
    for r in up.seen() {
        assert!(!r.path.contains("calibration"), "{}", r.path);
    }
}
async fn settled(f: &Front) -> Value {
    for _ in 0..300 {
        let (_, v) = get(f.addr, START, ADMIN).await;
        if v["job"]["status"] != "running" && v["job"].get("finishedAt").is_some() {
            return v["job"].clone();
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("the job never settled")
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_running_job_is_cancelled_by_its_token_and_its_profile_goes_back() {
    let (d, f, up, _e) = rig().await;
    let (s, launched) = write(f.addr, "POST", START, ADMIN, ASK).await;
    assert_eq!(s, 202, "{launched}");
    // This path has never accepted a model selector; even malformed JSON is ignored.
    let (s, v) = write(
        f.addr,
        "POST",
        &format!("{PATH}?model=other"),
        ADMIN,
        "{malformed",
    )
    .await;
    assert_eq!(s, 202, "{v}");
    assert_eq!(v["id"], launched["id"]);
    assert_eq!(v["model"], "synth");
    assert_eq!(v["status"], "running");
    let keys: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
    assert!(
        keys.iter().all(|k| PUBLIC.contains(&k.as_str())),
        "{keys:?}"
    );
    assert!(v.get("originalText").is_none());
    let job = settled(&f).await;
    assert_eq!(job["status"], "cancelled", "{job}");
    assert_eq!(job["id"], launched["id"]);
    assert_eq!(cal::ini(d.path()), format!("{}{}", cal::INI, cal::LAYA_INI));
    // Nothing is left to cancel.
    let (s, v) = write(f.addr, "POST", PATH, ADMIN, "").await;
    assert_eq!((s, v), (409, json!({"error":"No calibration is running."})));
    core_untouched(&up);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auth_account_role_and_csrf_precede_every_actor_call() {
    let (_d, f, up, e) = rig().await;
    assert_eq!(get(f.addr, PATH, "").await.0, 401);
    assert_eq!(write(f.addr, "POST", PATH, MEMBER, "").await.0, 403);
    assert_eq!(get(f.addr, PATH, MEMBER).await.0, 403);
    assert_eq!(raw(f.addr, "POST", PATH, ADMIN, "", b"").await.0, 403);
    assert!(up.seen().is_empty());
    assert!(e.seen().is_empty());
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn method_precedence_then_no_running_job_policy() {
    let (_d, f, up, e) = rig().await;
    for m in ["GET", "HEAD", "PUT", "DELETE", "OPTIONS"] {
        assert_eq!(write(f.addr, m, PATH, ADMIN, "").await.0, 405);
    }
    let (s, v) = write(f.addr, "POST", PATH, ADMIN, "").await;
    assert_eq!((s, v), (409, json!({"error":"No calibration is running."})));
    assert!(e.seen().is_empty());
    core_untouched(&up);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_cancel_response_is_not_stored_and_names_the_feature() {
    let (_d, f, up, _e) = rig().await;
    assert_eq!(write(f.addr, "POST", START, ADMIN, ASK).await.0, 202);
    let (s, headers, body) = raw_text(
        f.addr,
        "POST",
        PATH,
        ADMIN,
        &format!("Origin: http://{}\r\nX-CSRF-Token: csrf\r\n", f.addr),
        b"",
    )
    .await;
    assert_eq!(s, 202, "{body}");
    assert!(headers.contains("cache-control: no-store"));
    assert!(!body.contains("originalText"));
    assert_eq!(settled(&f).await["status"], "cancelled");
    assert!(noevia_server::FEATURES.contains(&"rust-calibration-cancel"));
    core_untouched(&up);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancellation_facts_over_the_bridge_cap_are_unavailable() {
    let d = tempfile::tempdir().unwrap();
    seed(d.path());
    let up = upstream().await;
    let (e, mut env) = cal::native(d.path(), true).await;
    // A running job (as a restart leaves it, before recovery) whose public facts pass 8 MiB.
    let big = "x".repeat(8 * 1024 * 1024 + 1);
    std::fs::write(
        cal::state_file(d.path(), &e),
        json!({"job": {"id": "j", "model": "synth", "status": "running", "phase": big, "steps": []}, "history": {}})
            .to_string(),
    )
    .unwrap();
    env.push(("NOEVIA_LEGACY_UPSTREAM", up.base()));
    let f = front_env(d.path(), true, &env).await;
    let (s, v) = write(f.addr, "POST", PATH, ADMIN, "").await;
    assert_eq!(
        (s, v),
        (
            503,
            json!({"error":"Calibration state is unavailable. Try again shortly."})
        )
    );
    core_untouched(&up);
}
