//! #2303: the public calibration routes over the native worker, through the real front. The job runs
//! in the front: Core is asked nothing about calibration (its bridges are gone), and a synthetic
//! router and gate stand in for the engine. Node's refusal fixture still decides the validation order.
#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod calibration_support;
#[path = "mcp_support/mod.rs"]
mod support;
use calibration_support as cal;
use serde_json::{json, Value};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use support::*;
const PATH: &str = "/api/models/calibration";
const ASK: &str = r#"{"model":"synth","confirmPause":true,"promptBudgetSeconds":60}"#;
const OVER_LIMIT: &str = "This calibration could take longer than the 24-hour maintenance limit. Choose a shorter prompt time limit.";

/// The front over a native llama.cpp manager; Node answers 404 to anything it is asked.
async fn rig_with(gate: bool) -> (tempfile::TempDir, Front, Upstream, Arc<cal::Engine>) {
    let d = tempfile::tempdir().unwrap();
    seed(d.path());
    let up = upstream().await;
    up.set(node);
    let (e, mut env) = cal::native(d.path(), gate).await;
    env.push(("NOEVIA_LEGACY_UPSTREAM", up.base()));
    env.push(("NOEVIA_PROJECT_STORE_TOKEN", STORE_TOKEN.into()));
    let f = front_env(d.path(), true, &env).await;
    (d, f, up, e)
}
async fn rig() -> (tempfile::TempDir, Front, Upstream, Arc<cal::Engine>) {
    rig_with(true).await
}
/// Node's load decision (core model-load-refusal.cjs), asked before every load.
const LOAD_DECISION: &str = "/internal/model-maintenance/load-refusal";
/// Node: its load decision lets every load go; anything else is the 404 of an unknown path.
fn node(r: &Seen) -> (Duration, String) {
    if r.method == "POST" && r.path == LOAD_DECISION {
        reply("200 OK", r#"{"refusal":null}"#)
    } else {
        reply("404 Not Found", r#"{"error":"Not found"}"#)
    }
}
/// No calibration operation reaches Core: it is asked only for its load decision, by the front.
fn core_untouched(up: &Upstream) {
    for r in up.seen() {
        if r.path == LOAD_DECISION {
            assert_eq!(r.method, "POST");
            assert_eq!(
                r.header("x-noevia-store-token").as_deref(),
                Some(STORE_TOKEN)
            );
            assert_eq!(
                serde_json::from_str::<Value>(&r.body).unwrap(),
                json!({"model": "synth"})
            );
            continue;
        }
        assert!(!r.path.contains("calibration"), "{}", r.path);
        assert!(!r.path.contains("model-maintenance"), "{}", r.path);
    }
}
async fn settled(f: &Front) -> Value {
    for _ in 0..300 {
        let (_, v) = get(f.addr, PATH, ADMIN).await;
        // Terminal status and finishedAt are published together after bounded cleanup.
        if v["job"]["status"] != "running" && v["job"].get("finishedAt").is_some() {
            return v["job"].clone();
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("the job never settled")
}

/// Completion is a public persistence/admission boundary, including when the last save fails.
/// A directory at the state-file destination deterministically refuses the final atomic rename.
///
/// #2427: the block goes in only once the cancelled run has put its profile back, while the
/// restore's router reload is held. Blocked earlier, it also refuses the restore's own intent save,
/// and that run (by design) publishes its terminal status with the restore owed to a recovery
/// under a fresh hold, which keeps admission closed and rewrites the job after the status is
/// terminal: a different contract (calibration_worker's owed-recovery tests), and the race this
/// test used to lose.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn finished_status_has_a_final_saved_record_and_admits_the_next_start() {
    let (d, f, _up, e) = rig().await;
    assert_eq!(write(f.addr, "POST", PATH, ADMIN, ASK).await.0, 202);
    let path = cal::state_file(d.path(), &e);
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let saved: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            if saved["job"]["originalText"].is_string() && e.stream_started.load(Ordering::Acquire)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    // The long prompt is in flight, so the next router reload is the cancelled run's restore.
    let held_reload = e.hold_next_reload();
    assert_eq!(
        write(f.addr, "POST", "/api/models/calibration/cancel", ADMIN, "")
            .await
            .0,
        202
    );
    tokio::time::timeout(Duration::from_secs(30), held_reload.entered())
        .await
        .unwrap();
    // The restore has saved its intent and written the original profile; only its reload is out.
    assert_eq!(cal::ini(d.path()), format!("{}{}", cal::INI, cal::LAYA_INI));
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    held_reload.release();
    // The raw outcome may be cancelled, but clients must see no terminal status until the
    // final saved record and admission release are published together.
    let until = tokio::time::Instant::now() + Duration::from_secs(1);
    while tokio::time::Instant::now() < until {
        let (_, v) = get(f.addr, PATH, ADMIN).await;
        assert_eq!(v["job"]["status"], "running", "{v}");
        assert!(v["job"].get("finishedAt").is_none(), "{v}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    std::fs::remove_dir(&path).unwrap();
    let job = settled(&f).await;
    assert_eq!(job["status"], "cancelled", "{job}");
    // Restored on the run's own hold: no recovery is owed after the terminal status.
    assert_eq!(job["restored"], true, "{job}");
    let saved: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(saved["job"]["id"], job["id"]);
    assert_eq!(saved["job"]["finishedAt"], job["finishedAt"]);
    assert!(saved["job"].get("originalText").is_none());
    // No sleep, task counter or private admission observation between status and public start.
    let (status, next) = write(f.addr, "POST", PATH, ADMIN, ASK).await;
    assert_eq!(status, 202, "{next}");
    assert_ne!(next["id"], job["id"]);
    assert_eq!(
        write(f.addr, "POST", "/api/models/calibration/cancel", ADMIN, "")
            .await
            .0,
        202
    );
    settled(&f).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_launch_runs_in_the_front_under_the_gate_and_its_state_is_private() {
    let (d, f, up, e) = rig().await;
    let (s, v) = write(f.addr, "POST", PATH, ADMIN, ASK).await;
    assert_eq!(s, 202, "{v}");
    assert_eq!(v["model"], "synth");
    assert_eq!(v["status"], "running");
    assert_eq!(v["promptBudgetSeconds"], 60);
    assert_eq!(v["native"], 32768);
    // The profile exists, so the plan carries no conservative base values.
    assert_eq!(v["base"], json!({}));
    assert!(v.get("originalText").is_none());
    assert_eq!(e.count("POST /gate/v1/maintenance"), 1);
    // A second launch while one runs is refused before another hold is asked for.
    let (s, v) = write(f.addr, "POST", PATH, ADMIN, ASK).await;
    assert_eq!(
        (s, v),
        (409, json!({"error":"A calibration is already running."}))
    );
    assert_eq!(e.count("POST /gate/v1/maintenance"), 1);
    // The state route shows the job (never its original profile text) and the model's history.
    let (s, v) = get(f.addr, &format!("{PATH}?model=synth"), ADMIN).await;
    assert_eq!(s, 200);
    assert_eq!(v["job"]["status"], "running");
    assert_eq!(v["history"], json!([]));
    assert!(v["job"].get("originalText").is_none());
    // Preparing is saved before the async unload and original-profile capture, as in Node.
    // Wait for that capture before examining the private running record.
    let file: Value = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let file: Value = serde_json::from_str(
                &std::fs::read_to_string(cal::state_file(d.path(), &e)).unwrap(),
            )
            .unwrap();
            if file["job"]["originalText"].is_string() {
                break file;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert!(file["job"]["originalText"].is_string());
    // Cancelled through the native cancel route; the profile goes back.
    let (s, _) = write(f.addr, "POST", "/api/models/calibration/cancel", ADMIN, "").await;
    assert_eq!(s, 202);
    let job = settled(&f).await;
    assert_eq!(job["status"], "cancelled", "{job}");
    // Restored if the run had written the profile; either way models.ini is the original.
    assert_ne!(job["restored"], false);
    assert_eq!(cal::ini(d.path()), format!("{}{}", cal::INI, cal::LAYA_INI));
    core_untouched(&up);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_selects_the_history_the_state_file_holds() {
    let d = tempfile::tempdir().unwrap();
    seed(d.path());
    let up = upstream().await;
    let (e, mut env) = cal::native(d.path(), true).await;
    std::fs::write(
        cal::state_file(d.path(), &e),
        json!({"job": null, "history": {"synth": [{"verifiedCtx": 8192}]}, "unknown": 1})
            .to_string(),
    )
    .unwrap();
    env.push(("NOEVIA_LEGACY_UPSTREAM", up.base()));
    let f = front_env(d.path(), true, &env).await;
    let (s, v) = get(f.addr, &format!("{PATH}?model=synth"), ADMIN).await;
    assert_eq!(
        (s, v),
        (
            200,
            json!({"job": null, "history": [{"verifiedCtx": 8192}]})
        )
    );
    let (_, v) = get(f.addr, &format!("{PATH}?model=other"), ADMIN).await;
    assert_eq!(v, json!({"job": null, "history": []}));
    let (_, v) = get(f.addr, PATH, ADMIN).await;
    assert_eq!(v, json!({"job": null}));
    assert!(e.seen().is_empty());
    core_untouched(&up);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auth_role_account_and_csrf_precede_every_fact_operation() {
    let (_d, f, up, e) = rig().await;
    assert_eq!(get(f.addr, PATH, "").await.0, 401);
    assert_eq!(get(f.addr, PATH, MEMBER).await.0, 403);
    assert_eq!(write(f.addr, "POST", PATH, MEMBER, ASK).await.0, 403);
    assert_eq!(raw(f.addr, "POST", PATH, ADMIN, "", b"").await.0, 403);
    assert!(up.seen().is_empty());
    assert!(e.seen().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn method_confirmation_and_budget_order_skip_all_metadata_and_launches() {
    let (_d, f, up, e) = rig().await;
    for method in ["HEAD", "PUT", "DELETE", "OPTIONS"] {
        assert_eq!(write(f.addr, method, PATH, ADMIN, "{bad").await.0, 405);
    }
    // Node's own refusals, in its order (tests/fixtures/calibration-start-node.json).
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/calibration-start-node.json")).unwrap();
    for case in fixture["cases"].as_array().unwrap() {
        if case["listingReads"] != 0 || case["result"]["status"] != 400 {
            continue;
        }
        let (s, v) = write(f.addr, "POST", PATH, ADMIN, &case["input"].to_string()).await;
        assert_eq!(s, 400, "{case}");
        assert_eq!(v["error"], case["result"]["body"]["error"], "{case}");
    }
    assert!(e.seen().is_empty());
    core_untouched(&up);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_a_native_llamacpp_manager_calibration_is_unavailable() {
    let d = tempfile::tempdir().unwrap();
    seed(d.path());
    let up = upstream().await;
    let f = front_env(d.path(), true, &[("NOEVIA_LEGACY_UPSTREAM", up.base())]).await;
    let missing = json!({"error":"Native calibration is unavailable"});
    assert_eq!(get(f.addr, PATH, ADMIN).await, (404, missing.clone()));
    assert_eq!(
        write(f.addr, "POST", PATH, ADMIN, "{bad").await,
        (404, missing.clone())
    );
    let cancel = "/api/models/calibration/cancel";
    assert_eq!(
        write(f.addr, "POST", cancel, ADMIN, "").await,
        (404, missing)
    );
    core_untouched(&up);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_system_and_backend_failures_do_not_launch() {
    let (d, f, up, e) = rig().await;
    for (model, status, message) in [
        ("gone", 404, "Choose an installed model."),
        ("laya-mini", 400, "System routing model — not tuned"),
    ] {
        let ask = json!({"model": model, "confirmPause": true}).to_string();
        let (s, v) = write(f.addr, "POST", PATH, ADMIN, &ask).await;
        assert_eq!((s, v), (status, json!({"error": message})), "{model}");
    }
    e.down.store(true, Ordering::Release);
    let (s, v) = write(f.addr, "POST", PATH, ADMIN, ASK).await;
    assert_eq!(
        (s, v),
        (502, json!({"error":"The model server is not responding."}))
    );
    assert_eq!(e.count("POST /gate/"), 0);
    assert_eq!(e.count("POST /models/load"), 0);
    assert_eq!(get(f.addr, PATH, ADMIN).await.1, json!({"job": null}));
    assert_eq!(cal::ini(d.path()), format!("{}{}", cal::INI, cal::LAYA_INI));
    core_untouched(&up);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_gate_hold_is_the_public_hold_failure_and_nothing_runs() {
    let (d, f, up, e) = rig().await;
    for (gate, status) in [(403, 503), (409, 409)] {
        *e.refuse.lock().unwrap() = Some((gate, "busy".into(), "synthetic refusal".into()));
        let (s, v) = write(f.addr, "POST", PATH, ADMIN, ASK).await;
        assert_eq!(s, status, "{v}");
        assert!(v["error"].as_str().unwrap().starts_with(if status == 409 {
            "Requests are in progress."
        } else {
            "Calibration could not pause chat: "
        }));
    }
    assert_eq!(e.count("POST /models/load"), 0);
    assert_eq!(get(f.addr, PATH, ADMIN).await.1, json!({"job": null}));
    assert_eq!(cal::ini(d.path()), format!("{}{}", cal::INI, cal::LAYA_INI));
    core_untouched(&up);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_the_gate_a_run_longer_than_any_lease_is_refused_before_one_is_asked_for() {
    let (d, f, up, e) = rig_with(false).await;
    e.native.store(131072, Ordering::Release);
    // A 128K model at the longest budget fits the 24-hour lease on its own; with 1000 other models
    // live, unloading them first could take over 16 hours more.
    e.extra_live.store(1000, Ordering::Release);
    let ask = r#"{"model":"synth","confirmPause":true,"promptBudgetSeconds":1800}"#;
    let (s, v) = write(f.addr, "POST", PATH, ADMIN, ask).await;
    assert_eq!((s, v), (400, json!({"error": OVER_LIMIT})));
    assert_eq!(e.count("POST /models/load"), 0);
    assert_eq!(get(f.addr, PATH, ADMIN).await.1, json!({"job": null}));
    assert_eq!(cal::ini(d.path()), format!("{}{}", cal::INI, cal::LAYA_INI));
    core_untouched(&up);
}

/// The availability regression of the 8-hour cap: a 1M-context model at the default 2-minute budget
/// (about 8.3 hours at worst) is not refused; Node is asked for a lease that long (here it has none
/// to give, so the hold is refused as any refused lease is).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_the_gate_a_1m_context_run_at_the_default_budget_asks_for_its_lease() {
    let (d, f, up, e) = rig_with(false).await;
    e.native.store(1_048_576, Ordering::Release);
    let ask = r#"{"model":"synth","confirmPause":true,"promptBudgetSeconds":120}"#;
    let (s, v) = write(f.addr, "POST", PATH, ADMIN, ask).await;
    assert_ne!(v, json!({"error": OVER_LIMIT}));
    // Node answers no lease (404 here): the hold is refused as busy.
    assert_eq!(
        (s, v),
        (
            409,
            json!({"error": "Requests are in progress. Wait for them to finish, then start calibration."})
        )
    );
    let asked: Vec<u64> = up
        .seen()
        .iter()
        .filter(|r| r.method == "POST" && r.path == "/internal/model-maintenance/lease")
        .map(|r| {
            serde_json::from_str::<Value>(&r.body).unwrap()["maxMs"]
                .as_u64()
                .unwrap()
        })
        .collect();
    assert_eq!(asked.len(), 1, "{asked:?}");
    assert!(
        asked[0] > 8 * 3_600_000 && asked[0] <= 24 * 3_600_000,
        "{asked:?}"
    );
    assert_eq!(e.count("POST /models/load"), 0);
    assert_eq!(get(f.addr, PATH, ADMIN).await.1, json!({"job": null}));
    assert_eq!(cal::ini(d.path()), format!("{}{}", cal::INI, cal::LAYA_INI));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_launch_that_outlives_the_bridge_deadline_is_unavailable_but_still_starts() {
    let (_d, f, up, e) = rig().await;
    // Each identity read takes 7.5 s: preparation (one read) stays inside the 14 s bound Core's
    // bridge had, the launch (it prepares again before and after the hold) does not. The client hears
    // 503; the launch, which is never cut in half, goes on and starts the job.
    e.props_delay_ms.store(7_500, Ordering::Release);
    let started = std::time::Instant::now();
    let (s, v) = write(f.addr, "POST", PATH, ADMIN, ASK).await;
    assert_eq!(
        (s, v),
        (
            503,
            json!({"error":"Calibration state is unavailable. Try again shortly."})
        )
    );
    let waited = started.elapsed();
    // Preparation's 7.5 s, then the launch's own 14 s bound.
    assert!(
        waited >= Duration::from_secs(21) && waited < Duration::from_secs(23),
        "{waited:?}"
    );
    let mut job = Value::Null;
    for _ in 0..200 {
        job = get(f.addr, PATH, ADMIN).await.1["job"].clone();
        if job["status"] == "running" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(job["status"], "running", "{job}");
    assert_eq!(e.count("POST /gate/v1/maintenance"), 1);
    let (s, _) = write(f.addr, "POST", "/api/models/calibration/cancel", ADMIN, "").await;
    assert_eq!(s, 202);
    assert_eq!(settled(&f).await["status"], "cancelled");
    core_untouched(&up);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn facts_over_the_bridge_cap_are_unavailable() {
    let d = tempfile::tempdir().unwrap();
    seed(d.path());
    let up = upstream().await;
    let (e, mut env) = cal::native(d.path(), true).await;
    // A history past Core's 8 MiB answer cap.
    let big = "x".repeat(8 * 1024 * 1024 + 1);
    std::fs::write(
        cal::state_file(d.path(), &e),
        json!({"job": null, "history": {"synth": [{"note": big}]}}).to_string(),
    )
    .unwrap();
    env.push(("NOEVIA_LEGACY_UPSTREAM", up.base()));
    let f = front_env(d.path(), true, &env).await;
    let unavailable = json!({"error":"Calibration state is unavailable. Try again shortly."});
    assert_eq!(get(f.addr, PATH, ADMIN).await, (503, unavailable.clone()));
    assert_eq!(
        write(f.addr, "POST", PATH, ADMIN, ASK).await,
        (503, unavailable)
    );
    assert_eq!(e.count("POST /gate/"), 0);
    core_untouched(&up);
}

/// Boot recovery through the front's own Config (main.rs calls this before it serves): a job a
/// restart left running is interrupted and its profile restored (the oracle's first case), and
/// nothing launches or cancels while the recovery runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn boot_recovery_through_config_interrupts_the_job_and_restores_its_profile() {
    let d = tempfile::tempdir().unwrap();
    seed(d.path());
    let up = upstream().await;
    let (e, mut env) = cal::native(d.path(), true).await;
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/calibration-recover-node.json")).unwrap();
    let case = &fixture["cases"][0];
    assert_eq!(
        case["name"],
        "models.ini still what the job last wrote: restored"
    );
    std::fs::write(d.path().join("models.ini"), case["ini"].as_str().unwrap()).unwrap();
    std::fs::write(
        cal::state_file(d.path(), &e),
        case["state"].as_str().unwrap(),
    )
    .unwrap();
    env.push(("NOEVIA_LEGACY_UPSTREAM", up.base()));
    let f = front_env(d.path(), true, &env).await;
    let held_reload = e.hold_next_reload();
    let recovery = f.app.config.spawn_calibration_recovery().unwrap();
    assert!(
        f.app.config.spawn_calibration_recovery().is_none(),
        "one recovery at a time"
    );
    // Recovery has published its replacement job before this restore reload. Hold that
    // actual phase, rather than racing the earlier generation changes before a delay starts.
    held_reload.entered().await;
    assert!(
        held_reload.is_held(),
        "the synthetic restore reload is held"
    );
    assert!(
        !recovery.is_finished(),
        "recovery cannot finish while reload is held"
    );
    // While it runs: the job is the file's, with no run behind it.
    let (s, v) = write(f.addr, "POST", "/api/models/calibration/cancel", ADMIN, "").await;
    assert_eq!(
        (s, v),
        (
            503,
            json!({"error":"Calibration state is unavailable. Try again shortly."})
        )
    );
    let (s, v) = write(f.addr, "POST", PATH, ADMIN, ASK).await;
    assert_eq!(
        (s, v),
        (409, json!({"error":"A calibration is already running."}))
    );
    assert_eq!(
        e.count("POST /models/load"),
        0,
        "recovery cannot launch a trial"
    );
    assert!(
        held_reload.is_held(),
        "route assertions did not release the reload"
    );
    held_reload.release();
    let outcome = tokio::time::timeout(Duration::from_secs(30), recovery)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(outcome, Some(Some(true)));
    let job = get(f.addr, PATH, ADMIN).await.1["job"].clone();
    assert_eq!(job["status"], "interrupted", "{job}");
    assert_eq!(job["restored"], true);
    let original: Value = serde_json::from_str(case["state"].as_str().unwrap()).unwrap();
    assert_eq!(
        cal::ini(d.path()),
        original["job"]["originalText"].as_str().unwrap()
    );
    assert_eq!(e.count("GET /models?reload=1"), 1);
    // Recovery loads nothing, so it asks Node for no load decision.
    assert_eq!(e.count("POST /models/load"), 0);
    assert!(!up.seen().iter().any(|r| r.path == LOAD_DECISION));
    core_untouched(&up);
}

/// The event hold precedes the reply, and only its owner releases it, including on unwind.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn synthetic_reload_hold_orders_the_reply_and_releases_on_drop_or_unwind() {
    let d = tempfile::tempdir().unwrap();
    let e = cal::engine(d.path().join("models.ini")).await;
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], e.port));
    for unwind in [false, true] {
        let held = e.hold_next_reload();
        assert!(
            !held.is_held(),
            "arming alone is not an observed reload entry"
        );
        let before = e.count("GET /models?reload=1");
        let response = tokio::spawn(async move { get(addr, "/models?reload=1", "").await });
        held.entered().await;
        assert!(held.is_held());
        assert_eq!(e.count("GET /models?reload=1"), before + 1);
        assert!(
            !response.is_finished(),
            "a held reload cannot publish its reply"
        );
        if unwind {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
                let _owned_hold = held;
                panic!("synthetic reload hold owner unwind");
            }));
            assert!(result.is_err());
        } else {
            drop(held);
        }
        assert_eq!(response.await.unwrap().0, 200);
        assert_eq!(e.count("GET /models?reload=1"), before + 1);
    }
}

/// Puts a directory's mode back when the test ends, however it ends.
#[cfg(unix)]
struct Mode(std::path::PathBuf);
#[cfg(unix)]
impl Drop for Mode {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o755));
    }
}

/// P2-2: a boot recovery whose save of the interrupted state fails. The profile is restored and
/// memory keeps the interrupted job (noted as unsaved), not a running phantom: nothing to cancel, and
/// a launch saves it first and is refused, before any hold, while that fails. The save is retried;
/// once storage is repaired it lands without the note, and a launch runs.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn boot_recovery_whose_save_fails_keeps_the_interrupted_job_and_saves_it_once_it_can() {
    use noevia_server::model_maint::calibration::STOPPED;
    use noevia_server::model_maint::calibration_worker::{Plan, Refusal, UNSAVED};
    use sha2::{Digest, Sha256};
    use std::os::unix::fs::PermissionsExt;
    let d = tempfile::tempdir().unwrap();
    seed(d.path());
    let up = upstream().await;
    let (e, mut env) = cal::native(d.path(), true).await;
    // models.ini lives outside the data directory: only the state file's directory breaks.
    let presets = d.path().join("presets");
    std::fs::create_dir_all(&presets).unwrap();
    let ini = presets.join("models.ini");
    let original = format!("{}{}", cal::INI, cal::LAYA_INI);
    let written = original.replace("ctx-size = 4096", "ctx-size = 8192");
    std::fs::write(&ini, &written).unwrap();
    for (k, v) in &mut env {
        if *k == "LLAMACPP_PRESET_PATH" {
            *v = ini.display().to_string();
        }
    }
    let rev = |t: &str| -> String {
        Sha256::digest(t.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    };
    let running = json!({"job": {"id": "boot-job", "model": "synth", "promptBudgetSeconds": 60,
        "native": 32768, "base": {}, "status": "running", "phase": "Long-prompt test at 8,192 tokens",
        "startedAt": 1, "steps": [], "memoryFloorGib": 2, "memoryGuard": "active",
        "originalText": original, "originalRevision": rev(&original), "lastRevision": rev(&written)},
        "history": {}})
    .to_string();
    let state = cal::state_file(d.path(), &e);
    std::fs::write(&state, &running).unwrap();
    up.set(node);
    env.push(("NOEVIA_LEGACY_UPSTREAM", up.base()));
    env.push(("NOEVIA_PROJECT_STORE_TOKEN", STORE_TOKEN.into()));
    let f = front_env(d.path(), true, &env).await;
    let worker = f.app.config.calibration.clone().unwrap();
    // This fixture fails calibration-state writes, while the mandatory evidence fence remains
    // writable in its independent child directory (as the preset file already does).
    let evidence_file = f.app.config.maint.evidence.as_ref().unwrap().file();
    let evidence_dir = evidence_file.parent().unwrap();
    std::fs::create_dir_all(evidence_dir).unwrap();
    std::fs::set_permissions(evidence_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    // Storage fails: the data directory takes no new file (the save's temp file).
    let mode = Mode(d.path().to_path_buf());
    std::fs::set_permissions(d.path(), std::fs::Permissions::from_mode(0o555)).unwrap();
    if std::fs::write(d.path().join("probe"), "").is_ok() {
        drop(mode);
        let _ = std::fs::remove_file(d.path().join("probe"));
        eprintln!("skipped: this user ignores directory modes, so no save failure can be injected");
        return;
    }
    let recovery = f.app.config.spawn_calibration_recovery().unwrap();
    let outcome = tokio::time::timeout(Duration::from_secs(30), recovery)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(outcome, Some(Some(true)));
    assert_eq!(std::fs::read_to_string(&ini).unwrap(), original);
    assert!(worker.unsaved());
    let job = worker.job();
    assert_eq!(job.get("status").as_str(), Some("interrupted"), "{job:?}");
    let noted = format!("{STOPPED}{UNSAVED}");
    assert_eq!(job.get("error").as_str(), Some(noted.as_str()));
    assert_eq!(std::fs::read_to_string(&state).unwrap(), running);
    // Not running: nothing to cancel. A launch saves first, and is refused while it cannot.
    assert!(worker.cancel().is_none());
    let plan = Plan {
        prompt_budget_seconds: 60.0,
        native: 32768.0,
        base: Vec::new(),
    };
    assert_eq!(
        worker.launch("synth", plan).await,
        Err(Refusal::Unavailable)
    );
    assert!(
        e.count("POST /gate/v1/maintenance") >= 1,
        "restart repair holds fresh exclusion"
    );
    assert!(worker.unsaved());
    assert_eq!(std::fs::read_to_string(&state).unwrap(), running);
    // Repaired: the retry lands, without the note.
    drop(mode);
    for _ in 0..300 {
        if !worker.unsaved() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(!worker.unsaved(), "the save was not retried");
    let file: Value = serde_json::from_str(&std::fs::read_to_string(&state).unwrap()).unwrap();
    assert_eq!(file["job"]["status"], "interrupted", "{file}");
    assert_eq!(file["job"]["error"], STOPPED);
    assert_eq!(file["job"]["restored"], true);
    assert!(file["job"].get("originalText").is_none());
    let (_, v) = get(f.addr, PATH, ADMIN).await;
    assert_eq!(v["job"]["status"], "interrupted");
    assert_eq!(v["job"]["error"], STOPPED);
    // And a launch runs.
    let (s, v) = write(f.addr, "POST", PATH, ADMIN, ASK).await;
    assert_eq!(s, 202, "{v}");
    // Cancellation's bounded compare-and-abort retries legitimately refuse a constantly
    // changing start snapshot. Wait for the fixture's pending long request, not a fixed delay.
    assert!(
        tokio::time::timeout(Duration::from_secs(10), async {
            while !e.stream_started.load(Ordering::Acquire) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .is_ok(),
        "the recovered worker starts the successor's long request"
    );
    let (s, _) = write(f.addr, "POST", "/api/models/calibration/cancel", ADMIN, "").await;
    assert_eq!(s, 202);
    let job = settled(&f).await;
    assert_eq!(job["status"], "cancelled", "{job}");
    core_untouched(&up);
}

/// P2: a load Node's decision refuses (here the watchdog's quarantine), or one it cannot decide
/// (Node answers 503), never reaches the engine through the real front: every step fails as Node's
/// refused load does and the original profile goes back.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_load_nodes_decision_refuses_never_reaches_the_engine() {
    let quarantined = json!({"refusal": {"error": "synth was unloaded by the memory safety net because measured inference memory ran more than the allowed margin over the 16 GiB budget. It stays unloaded until its settings or the budget change: lower its context or prompt cache, or raise the budget, in Settings → Models & routing.", "code": "inference_budget_unloaded", "budgetGib": 16, "unloadedAt": "2026-11-04T00:00:00.000Z"}})
    .to_string();
    for (status, answer) in [
        ("200 OK", quarantined),
        (
            "503 Service Unavailable",
            r#"{"error":"synthetic"}"#.to_owned(),
        ),
    ] {
        let (d, f, up, e) = rig().await;
        up.set(move |r| {
            if r.path == LOAD_DECISION {
                reply(status, &answer)
            } else {
                reply("404 Not Found", r#"{"error":"Not found"}"#)
            }
        });
        let (s, v) = write(f.addr, "POST", PATH, ADMIN, ASK).await;
        assert_eq!(s, 202, "{v}");
        let job = settled(&f).await;
        assert_eq!(job["status"], "failed", "{status}: {job}");
        let steps = job["steps"].as_array().unwrap();
        assert!(!steps.is_empty(), "{job}");
        for step in steps {
            assert_eq!(step["status"], "failed", "{step}");
            assert_eq!(
                step["reason"],
                "The engine refused to load the model at this size."
            );
            assert_eq!(step["cause"], "load");
        }
        assert_eq!(e.count("POST /models/load"), 0, "{status}");
        assert!(up.seen().iter().any(|r| r.path == LOAD_DECISION));
        assert_eq!(cal::ini(d.path()), format!("{}{}", cal::INI, cal::LAYA_INI));
        core_untouched(&up);
    }
}
