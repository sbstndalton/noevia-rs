//! sbstndalton/noevia#2641: Node's lose-at-margin coverage for the front's leases on Node's gate
//! (`NOEVIA_RUST_LEASE_COVERAGE=node`), over HTTP against a scripted Node latch and router: a renewal
//! late within the margin lets the preset write and its reload go on; a lapse within the margin ends
//! the reload's router call in flight with Node's coverage error, the release owes the settle, and the
//! settle driver runs it under a fresh recovery lease and completes the recovery. With the switch off
//! the old whole-step rule stands. Synthetic data only; short TTLs with a 500 ms margin.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
mod maint_support;
use maint_support::*;
use noevia_server::model_maint::gate::Coverage;
use noevia_server::model_maint::recovery::Registry;
use noevia_server::model_maint::Maint;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const SECRET: &str = "synthetic-store-token-0123456789abcdef";
const LATCH: &str = "/internal/model-maintenance/lease";
/// Renewed every 100 ms; unconfirmed for 1.5 s it ends (the margin point: 1 s).
const GRANT: &str = r#"{"lease":"0123456789abcdef0123456789abcdef","generation":1,"ttlMs":1500,"renewMs":100,"hardMs":3600000}"#;
const LOST: &str =
    "The maintenance lease could not be renewed, so this work stopped before changing anything else.";

/// How Node answers renewals.
#[derive(Default)]
struct NodeScript {
    /// Each renewal answered this late.
    late: Mutex<Duration>,
    /// Renewals are not answered (until the recovery barrier arrives).
    silent: AtomicBool,
}

fn node(script: Arc<NodeScript>) -> Handler {
    Arc::new(move |req: &Req| {
        assert_eq!(
            req.headers.get("x-noevia-store-token").map(String::as_str),
            Some(SECRET)
        );
        if req.target.contains("/recovery/") {
            // The barrier is up: Node's chat is closed; renewals of the fresh lease answer again.
            script.silent.store(false, Ordering::SeqCst);
            return Answer::Reply(204, String::new());
        }
        if req.method == "POST" && req.target == LATCH {
            return Answer::Reply(201, GRANT.into());
        }
        if req.method == "POST" {
            if script.silent.load(Ordering::SeqCst) {
                return Answer::After(Duration::from_secs(30), 200, "{}".into());
            }
            let late = *script.late.lock().unwrap();
            return Answer::After(late, 200, "{}".into());
        }
        Answer::Reply(204, String::new())
    })
}

/// The router: `qa-a` loaded until it is asked to unload, loaded again by the reload, which stalls
/// when `stall` is set.
fn router(loaded: Arc<AtomicBool>, stall: bool) -> Handler {
    Arc::new(move |req: &Req| {
        if req.target.starts_with("/gate/v1/") {
            return Answer::Reply(404, r#"{"error":"not found"}"#.into());
        }
        match (req.method.as_str(), req.target.as_str()) {
            ("GET", "/models") => {
                let state = if loaded.load(Ordering::SeqCst) {
                    "loaded"
                } else {
                    "unloaded"
                };
                Answer::Reply(
                    200,
                    format!(r#"{{"data":[{{"id":"qa-a","status":{{"value":"{state}"}}}}]}}"#),
                )
            }
            ("POST", "/models/unload") => {
                loaded.store(false, Ordering::SeqCst);
                Answer::Reply(200, "{}".into())
            }
            ("GET", "/models?reload=1") => {
                loaded.store(true, Ordering::SeqCst);
                if stall {
                    Answer::After(Duration::from_secs(30), 200, r#"{"success":true}"#.into())
                } else {
                    Answer::Reply(200, r#"{"success":true}"#.into())
                }
            }
            _ => Answer::Reply(404, "{}".into()),
        }
    })
}

async fn maint(
    node_port: u16,
    router_port: u16,
    switch: Option<&str>,
) -> (Arc<Maint>, tempfile::TempDir) {
    let d = tempfile::tempdir().unwrap();
    let ini = d.path().join("models.ini");
    std::fs::write(&ini, "[qa-a]\nmodel = /models/qa-a.gguf\n").unwrap();
    let mut env: Vec<(&str, String)> = vec![
        ("MODEL_MANAGER_KIND", "llamacpp".into()),
        (
            "MODEL_MANAGER_BASE_URL",
            format!("http://127.0.0.1:{router_port}/v1"),
        ),
        ("LLAMACPP_PRESET_PATH", ini.display().to_string()),
        (
            "NOEVIA_LEGACY_UPSTREAM",
            format!("http://127.0.0.1:{node_port}"),
        ),
        ("NOEVIA_PROJECT_STORE_TOKEN", SECRET.into()),
        ("UI_DATA_DIR", d.path().display().to_string()),
    ];
    if let Some(v) = switch {
        env.push(("NOEVIA_RUST_LEASE_COVERAGE", v.into()));
    }
    let get = |k: &str| env.iter().find(|(n, _)| *n == k).map(|(_, v)| v.clone());
    let engine = Arc::new(server_stats::Engine::from_lookup(&get).unwrap());
    let sizing = Arc::new(noevia_server::model_sizing::Sizing::from_lookup(
        engine.kind(),
        Arc::clone(&engine),
        &get,
    ));
    let mut m = Maint::from_lookup(engine.kind(), &engine, sizing, &get).unwrap();
    m.authority.node_session("b", 1).unwrap();
    if m.coverage.node {
        m.coverage = Coverage {
            node: true,
            margin: Duration::from_millis(500),
            recoveries: Arc::new(Registry::with_timing(
                Duration::from_millis(200),
                Duration::from_secs(2),
            )),
        };
    }
    (Arc::new(m), d)
}

async fn until(what: &str, limit: Duration, f: impl Fn() -> bool) {
    let deadline = Instant::now() + limit;
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn the_switch_is_read_as_node_only() {
    let (n, _) = engine(node(Arc::default())).await;
    let (r, _) = engine(router(Arc::default(), false)).await;
    for (value, on) in [
        (None, false),
        (Some("1"), false),
        (Some("Node"), false),
        (Some("node"), true),
    ] {
        let (m, _d) = maint(n, r, value).await;
        assert_eq!(m.coverage.node, on, "{value:?}");
    }
}

// A renewal late within the margin: Node's rule lets the 150 s preset step start on a 1.5 s lease
// that renewal keeps confirming, so the write and its reload go on; the old whole-step rule
// (switch off) refuses it before writing.
#[tokio::test]
async fn a_renewal_late_within_the_margin_lets_the_write_and_its_reload_go_on() {
    let script = Arc::new(NodeScript::default());
    *script.late.lock().unwrap() = Duration::from_millis(600);
    let (n, _) = engine(node(Arc::clone(&script))).await;
    let loaded = Arc::new(AtomicBool::new(false));
    let (r, seen) = engine(router(Arc::clone(&loaded), false)).await;
    let (m, _d) = maint(n, r, Some("node")).await;
    // The write takes longer than the grant's first confirmation lasts: renewals answered 600 ms late
    // keep it covered.
    let wrote = m
        .write_presets_held(
            || async {
                tokio::time::sleep(Duration::from_millis(1600)).await;
                "written"
            },
            |_| true,
        )
        .await;
    let (written, reloaded) = wrote.unwrap_or_else(|o| panic!("{} {}", o.status, o.body));
    assert_eq!(written, "written");
    assert!(reloaded, "the reload ran under the renewed lease");
    assert!(seen
        .lock()
        .unwrap()
        .iter()
        .any(|q| q.target == "/models?reload=1"));
    assert!(!m.authority.held_by_front(), "nothing was lost");
    assert!(!m.coverage.recoveries.pending());

    let (m, _d) = maint(n, r, None).await;
    let Err(refused) = m.write_presets_held(|| async { "written" }, |_| true).await else {
        panic!("the whole-step rule refuses the write");
    };
    assert_eq!(refused.status, 503);
    assert!(
        refused.body.contains("stopped confirming"),
        "{}",
        refused.body
    );
}

// A lapse within the margin while the reload's router call is in flight: the call ends at the margin
// point with Node's coverage error (not when the router answers), the lease goes back owing the
// recovery (Node's barrier), chat admission stays closed, and the settle driver runs the settle under
// a fresh recovery lease (the reloaded model unloaded and confirmed) and completes the recovery.
#[tokio::test]
async fn a_lapse_within_the_margin_ends_the_reload_in_flight_and_the_driver_settles() {
    let script = Arc::new(NodeScript::default());
    let (n, node_seen) = engine(node(Arc::clone(&script))).await;
    let loaded = Arc::new(AtomicBool::new(true));
    let (r, router_seen) = engine(router(Arc::clone(&loaded), true)).await;
    let (m, _d) = maint(n, r, Some("node")).await;
    m.spawn_recoveries();
    let task = {
        let m = Arc::clone(&m);
        tokio::spawn(async move { m.reload_presets(true).await })
    };
    until("the reload in flight", Duration::from_secs(5), || {
        router_seen
            .lock()
            .unwrap()
            .iter()
            .any(|q| q.target == "/models?reload=1")
    })
    .await;
    assert!(
        router_seen
            .lock()
            .unwrap()
            .iter()
            .any(|q| q.target == "/models/unload"),
        "the reload unloaded first"
    );
    script.silent.store(true, Ordering::SeqCst);
    let started = Instant::now();
    let outcome = tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .expect("the reload ended at the margin")
        .unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(outcome.status, 503, "{}", outcome.body);
    assert!(outcome.body.contains(LOST), "{}", outcome.body);
    // The barrier went up before the latch was given back; chat stays closed meanwhile.
    until("the recovery barrier", Duration::from_secs(5), || {
        node_seen
            .lock()
            .unwrap()
            .iter()
            .any(|q| q.method == "POST" && q.target.contains("/recovery/"))
    })
    .await;
    let unloads = || {
        router_seen
            .lock()
            .unwrap()
            .iter()
            .filter(|q| q.target == "/models/unload")
            .count()
    };
    // The settle: the model the reload loaded is unloaded under the fresh lease, then the recovery
    // is proven to Node and completed.
    // (`held_by_front`: the front holds a lease or a recovery is required.)
    until("the settle and completion", Duration::from_secs(10), || {
        !m.authority.held_by_front() && !m.coverage.recoveries.pending()
    })
    .await;
    assert!(unloads() >= 2, "the settle unloaded the reloaded model");
    assert!(!loaded.load(Ordering::SeqCst));
    let seen = node_seen.lock().unwrap().clone();
    let recovery_acquire = seen
        .iter()
        .any(|q| q.method == "POST" && q.target == LATCH && q.body.contains("\"recovery\":true"));
    assert!(recovery_acquire, "a fresh recovery latch was taken");
    let proof = seen
        .iter()
        .find(|q| q.method == "DELETE" && q.target.contains("/recovery/"))
        .expect("the recovery was proven to Node");
    assert!(proof.body.contains("\"lease\""), "{}", proof.body);
    until("chat admission open again", Duration::from_secs(5), || {
        !m.maintenance_held() && !m.local_held()
    })
    .await;
}

// A loss at the margin point (nobody renews) is Node's "could not be renewed": the lease goes back
// owing the recovery, chat admission stays closed, and the release owes the settle. Switch off, the
// admission view is the authority's alone.
#[tokio::test]
async fn admission_counts_a_pending_recovery_only_with_the_switch() {
    let script = Arc::new(NodeScript::default());
    let (n, _) = engine(node(Arc::clone(&script))).await;
    let (r, _) = engine(router(Arc::default(), false)).await;
    let (m, _d) = maint(n, r, Some("node")).await;
    let hold = m
        .acquire("qa reload", Duration::from_secs(300), false)
        .await
        .unwrap();
    assert!(hold.node_coverage());
    assert!(hold.assert_covered(Duration::from_secs(150)).is_ok());
    script.silent.store(true, Ordering::SeqCst);
    tokio::time::timeout(Duration::from_secs(5), hold.lost_wait())
        .await
        .expect("lost at the margin point");
    assert_eq!(hold.assert_covered(Duration::ZERO).unwrap_err(), LOST);
    assert!(!hold.excluded());
    until(
        "the lease given back owing recovery",
        Duration::from_secs(5),
        || m.authority.held_by_front() && !m.authority.held(),
    )
    .await;
    assert!(
        m.maintenance_held(),
        "the owed recovery keeps admission closed"
    );
    hold.release().await;
    assert!(
        m.coverage.recoveries.pending(),
        "the release owes the settle"
    );

    // A run's hold (calibration, the auto-tune runner) owes the settle too: otherwise a loss with no
    // restore of the run's own would leave the authority's recovery required with nobody to run it.
    script.silent.store(false, Ordering::SeqCst);
    let (m, _d) = maint(n, r, Some("node")).await;
    let run = m
        .acquire_for_run("qa calibration", Duration::from_secs(600))
        .await
        .unwrap();
    script.silent.store(true, Ordering::SeqCst);
    tokio::time::timeout(Duration::from_secs(5), run.lost_wait())
        .await
        .expect("lost at the margin point");
    run.release().await;
    assert!(m.coverage.recoveries.pending(), "the run's release owes the settle");
    script.silent.store(false, Ordering::SeqCst);

    let (m, _d) = maint(n, r, None).await;
    let hold = m
        .acquire("qa reload", Duration::from_secs(300), false)
        .await
        .unwrap();
    assert!(!hold.node_coverage());
    assert!(m.maintenance_held());
    hold.release().await;
    assert!(
        !m.maintenance_held(),
        "without the switch admission is the authority's held() alone"
    );
}
