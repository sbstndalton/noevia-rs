//! Both load gates of the joined calibration worker (#2303 generation 2) and budget watch (#697)
//! stay active over one engine. The memory watchdog's fence, in the shared maintenance router that
//! every native loader uses, refuses a fenced model. Core's own load decision, asked by the
//! calibration worker before each load (`Worker::request`), refuses a Core-refused model. Neither
//! load reaches the engine. Synthetic router, gate and Node only; no engine or model is touched.
#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod calibration_support;
#[path = "mcp_support/mod.rs"]
mod support;
use calibration_support as cal;
use noevia_server::model_maint::Maint;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use support::*;

const PATH: &str = "/api/models/calibration";
const ASK: &str = r#"{"model":"synth","confirmPause":true,"promptBudgetSeconds":60}"#;
const LOAD_DECISION: &str = "/internal/model-maintenance/load-refusal";
const FENCED: &str = "laya-mini";

/// The deployment's own maintenance state over `env`, as the front builds it.
fn maint(env: &[(&'static str, String)]) -> Arc<Maint> {
    let get = |k: &str| env.iter().find(|(n, _)| *n == k).map(|(_, v)| v.clone());
    let engine = Arc::new(server_stats::Engine::from_lookup(&get).unwrap());
    let sizing = Arc::new(noevia_server::model_sizing::Sizing::from_lookup(
        engine.kind(),
        Arc::clone(&engine),
        &get,
    ));
    Arc::new(Maint::from_lookup(engine.kind(), &engine, sizing, &get).unwrap())
}

/// A recorded watchdog fence for `model` at the current preset revision (budget_watch.rs `seed_fence`).
fn seed_fence(maint: &Maint, model: &str) {
    let dir = maint
        .data_dir
        .as_ref()
        .unwrap()
        .join(format!("budget-watch-{}", maint.suffix));
    std::fs::create_dir_all(&dir).unwrap();
    let revision = maint.ini.as_ref().map(|i| i.snapshot().unwrap().revision);
    let hash = noevia_server::model_maint::ini::sha(model);
    std::fs::write(
        dir.join(format!("{hash}.json")),
        json!({"model":model,"revision":revision,"budgetGib":16.0,"at":0,"recorded":true})
            .to_string(),
    )
    .unwrap();
    std::fs::write(
        dir.join(format!("{hash}.context")),
        json!({"revision":revision,"revisionKnown":true,"budgetGib":16.0}).to_string(),
    )
    .unwrap();
}

async fn settled(f: &Front) -> Value {
    for _ in 0..300 {
        let (_, v) = get(f.addr, PATH, ADMIN).await;
        if v["job"]["status"] != "running" && v["job"].get("finishedAt").is_some() {
            return v["job"].clone();
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("the job never settled")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fenced_model_and_a_core_refused_model_both_load_nothing() {
    let d = tempfile::tempdir().unwrap();
    seed(d.path());
    let up = upstream().await;
    let refused = json!({"refusal": {"error": "synth was unloaded by the memory safety net.", "code": "inference_budget_unloaded", "budgetGib": 16}}).to_string();
    up.set(move |r| {
        if r.method == "POST" && r.path == LOAD_DECISION {
            reply("200 OK", &refused)
        } else {
            reply("404 Not Found", r#"{"error":"Not found"}"#)
        }
    });
    let (e, mut env) = cal::native(d.path(), true).await;
    env.push(("NOEVIA_LEGACY_UPSTREAM", up.base()));
    env.push(("NOEVIA_PROJECT_STORE_TOKEN", STORE_TOKEN.into()));
    env.push(("UI_DATA_DIR", d.path().display().to_string()));

    // The watchdog's fence, in the router every native loader (calibration included) shares.
    let m = maint(&env);
    seed_fence(&m, FENCED);
    let fenced = m
        .router
        .as_ref()
        .unwrap()
        .request(
            reqwest::Method::POST,
            "/models/load",
            Some(&json!({ "model": FENCED })),
            None,
            Duration::from_secs(1),
        )
        .await
        .unwrap();
    assert_eq!(fenced.status, 409);
    assert_eq!(fenced.body.unwrap()["code"], "inference_budget_unloaded");
    assert_eq!(
        e.count("POST /models/load"),
        0,
        "a fenced load reached the engine"
    );

    // Core's load decision, asked by the calibration worker through the real front, before any load.
    let f = front_env(d.path(), true, &env).await;
    let (s, v) = write(f.addr, "POST", PATH, ADMIN, ASK).await;
    assert_eq!(s, 202, "{v}");
    let job = settled(&f).await;
    assert_eq!(job["status"], "failed", "{job}");
    let steps = job["steps"].as_array().unwrap();
    assert!(!steps.is_empty(), "{job}");
    for step in steps {
        assert_eq!(step["cause"], "load", "{step}");
    }
    assert!(up.seen().iter().any(|r| r.path == LOAD_DECISION));

    assert_eq!(
        e.count("POST /models/load"),
        0,
        "neither the fenced nor the Core-refused model may reach the engine"
    );
    assert_eq!(cal::ini(d.path()), format!("{}{}", cal::INI, cal::LAYA_INI));
}
