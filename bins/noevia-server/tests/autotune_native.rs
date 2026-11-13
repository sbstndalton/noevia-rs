//! #2516 (R6): the native auto-tune runner through the real front, with `AUTOTUNE_RUNNER_IMPL=rust`.
//! Its start refusals are Node's own (the admission cases recorded from core `start()`), its facts
//! come from the native actor (Node's auto-tune bridges are never asked), and a run a restart
//! interrupted is marked so at boot. With the switch off the start is still Node's.
#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod calibration_support;
#[path = "mcp_support/mod.rs"]
mod support;
use calibration_support as cal;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::time::Duration;
use support::*;

const PATH: &str = "/api/models/autotune";

/// Node answers its load decision; anything else (an auto-tune bridge above all) is a 404 the
/// tests check was never asked.
fn node(r: &Seen) -> (Duration, String) {
    if r.method == "POST" && r.path == "/internal/model-maintenance/load-refusal" {
        reply("200 OK", r#"{"refusal":null}"#)
    } else {
        reply("404 Not Found", r#"{"error":"Not found"}"#)
    }
}

fn state_file(dir: &std::path::Path, e: &cal::Engine) -> std::path::PathBuf {
    let hash: String = Sha256::digest(e.base().as_bytes())
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect();
    dir.join(format!("native-autotune-{hash}.json"))
}

async fn rig(
    on: bool,
    state: Option<&str>,
) -> (tempfile::TempDir, Front, Upstream, Arc<cal::Engine>) {
    let d = tempfile::tempdir().unwrap();
    seed(d.path());
    let up = upstream().await;
    up.set(node);
    let (e, mut env) = cal::native(d.path(), true).await;
    if let Some(text) = state {
        std::fs::write(state_file(d.path(), &e), text).unwrap();
    }
    if on {
        env.push(("AUTOTUNE_RUNNER_IMPL", "rust".into()));
    }
    env.push(("NOEVIA_LEGACY_UPSTREAM", up.base()));
    env.push(("NOEVIA_PROJECT_STORE_TOKEN", STORE_TOKEN.into()));
    let f = front_env(d.path(), true, &env).await;
    (d, f, up, e)
}

fn no_bridge(up: &Upstream) {
    for r in up.seen() {
        assert!(
            !r.path.contains("autotune"),
            "Node asked: {} {}",
            r.method,
            r.path
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_start_refuses_as_node_does_without_asking_node() {
    let (_d, f, up, _e) = rig(true, None).await;
    let fixture: Value = serde_json::from_str(include_str!(
        "../../../crates/autotune-start/tests/fixtures/autotune-start.node.json"
    ))
    .unwrap();
    let mut checked = 0;
    for case in fixture["cases"].as_array().unwrap() {
        let node = &case["node"];
        // The admission's own 400s (a 202 would start a run; 409 needs a running job).
        if node["status"] != 400 || case["input"]["systemModel"] == true {
            continue;
        }
        let mut body = case["input"].clone();
        body.as_object_mut().unwrap().remove("systemModel");
        let (s, v) = write(f.addr, "POST", PATH, ADMIN, &body.to_string()).await;
        assert_eq!(s, 400, "{case}");
        assert_eq!(v["error"], node["error"], "{case}");
        checked += 1;
    }
    assert!(checked >= 40, "{checked}");
    let (s, v) = write(f.addr, "POST", PATH, ADMIN, "{not json").await;
    assert_eq!((s, v), (400, json!({"error":"invalid JSON"})));
    let (s, _) = write(f.addr, "POST", PATH, MEMBER, "{}").await;
    assert_eq!(s, 403);
    no_bridge(&up);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_status_and_cancel_facts_come_from_the_actor() {
    let (_d, f, up, _e) = rig(true, None).await;
    let (s, h, b) = raw_text(
        f.addr,
        "GET",
        &format!("{PATH}?model=synth"),
        ADMIN,
        "",
        b"",
    )
    .await;
    assert_eq!(s, 200, "{b}");
    assert!(h.contains("cache-control: no-store"));
    let body: Value = serde_json::from_str(&b).unwrap();
    assert_eq!(body["job"], Value::Null, "{body}");
    assert_eq!(body["kvCandidates"], json!(["bf16", "q8_0"]));
    // Nothing to cancel: the route's own answer, from the native facts.
    let (s, _) = write(f.addr, "POST", "/api/models/autotune/cancel", ADMIN, "{}").await;
    assert_ne!(s, 503);
    no_bridge(&up);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_run_a_restart_interrupted_is_marked_at_boot() {
    let running = json!({"history":{},"settings":{},"job":{"id":"qa-job","model":"synth","bulk":false,"mode":"fast",
        "promptBudgetSeconds":120,"status":"running","phase":"KV cache","startedAt":1,"log":[],
        "models":[{"model":"synth","status":"running","kv":["bf16","q8_0"],"phases":[{"id":"kv","label":"KV cache","status":"running","steps":[]}]}],
        "_revision":"0000000000000000000000000000000000000000000000000000000000000000","queueProgress":{"done":0,"total":1}}});
    let (d, f, up, e) = rig(true, Some(&running.to_string())).await;
    // As main.rs does at boot (the test front is built through the library).
    f.app.config.autotune.as_ref().unwrap().spawn_boot();
    let file = state_file(d.path(), &e);
    let mut job = Value::Null;
    for _ in 0..100 {
        let text = std::fs::read_to_string(&file).unwrap();
        job = serde_json::from_str::<Value>(&text).unwrap()["job"].clone();
        if job["status"] == "interrupted" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(job["status"], "interrupted", "{job}");
    assert_eq!(job["phase"], "Interrupted");
    assert_eq!(job["models"][0]["status"], "interrupted");
    assert_eq!(
        job["models"][0]["phases"][0]["reason"],
        "Restart interrupted this phase."
    );
    no_bridge(&up);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn with_the_switch_off_the_start_is_nodes() {
    let (_d, f, up, _e) = rig(false, None).await;
    let (s, _) = write(f.addr, "POST", PATH, ADMIN, "{}").await;
    // Node's answer (this synthetic Node knows no start: its 404).
    assert_eq!(s, 404);
    assert!(up
        .seen()
        .iter()
        .any(|r| r.method == "POST" && r.path == PATH));
}
