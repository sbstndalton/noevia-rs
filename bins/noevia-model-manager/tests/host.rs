#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
use noevia_model_manager::host::{HostHistory, HostSampler, HISTORY_CAP, PROC_CAP};
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use tokio::sync::Mutex;

fn apply(history: &mut HostHistory, row: &Value) -> Option<Value> {
    let stat = row["stat"].as_str().ok_or("synthetic stat failure");
    let mem = row["mem"].as_str().ok_or("synthetic memory failure");
    history
        .sample(stat, mem, row["ts"].as_f64().unwrap())
        .ok()
        .map(|p| serde_json::to_value(p).unwrap())
}
#[test]
fn actual_python_samples_failure_state_rounding_and_retention() {
    let path = std::env::var("NOEVIA_HOST_FIXTURE").ok();
    let data = path
        .map(|p| std::fs::read_to_string(p).unwrap())
        .unwrap_or_else(|| include_str!("fixtures/host.json").into());
    let fixture: Value = serde_json::from_str(&data).unwrap();
    let mut history = HostHistory::default();
    for row in fixture["cases"].as_array().unwrap() {
        assert_eq!(
            apply(&mut history, row),
            if row["point"].is_null() {
                None
            } else {
                Some(row["point"].clone())
            }
        );
    }
    assert_eq!(
        serde_json::to_value(history.response()).unwrap(),
        fixture["samples_response"]
    );
    let mut history = HostHistory::default();
    for row in fixture["retention"]["inputs"].as_array().unwrap() {
        assert!(apply(&mut history, row).is_some());
    }
    assert_eq!(history.retained(), 450);
    assert_eq!(
        serde_json::to_value(history.response()).unwrap(),
        fixture["retention"]["response"]
    );
    // Meaningful negative controls: first sample cannot use absolute CPU totals;
    // and dropping CPU progress on a memory failure changes the next delta.
    let cases = fixture["cases"].as_array().unwrap();
    assert_eq!(cases[0]["point"]["cpu_pct"], json!(0.0));
    let mut mutant = HostHistory::default();
    apply(&mut mutant, &cases[0]);
    apply(&mut mutant, &cases[1]);
    let omitted_failure = apply(&mut mutant, &cases[3]).unwrap();
    assert_ne!(omitted_failure["cpu_pct"], cases[3]["point"]["cpu_pct"]);
    assert_ne!(history.response().history.len(), history.retained());
}
#[test]
fn malformed_caps_and_nonfinite_are_conservatively_refused() {
    for stat in [
        "cpu 1 2 3",
        "cpu 9007199254740993 0 0 0",
        "cpu NaN 0 0 0",
        "cpu 1 0 0 0 ",
    ]
    .into_iter()
    .take(3)
    {
        assert!(HostHistory::default()
            .sample(Ok(stat), Ok(""), 1.0)
            .is_err());
    }
    assert!(HostHistory::default()
        .sample(Ok(&" ".repeat(PROC_CAP + 1)), Ok(""), 1.0)
        .is_err());
    assert!(HostHistory::default()
        .sample(Ok("cpu 0 0 0 0"), Ok(&" ".repeat(PROC_CAP + 1)), 1.0)
        .is_err());
    assert!(HostHistory::default()
        .sample(Ok("cpu 0 0 0 0"), Ok(""), f64::NAN)
        .is_err());
    assert!(HostHistory::default()
        .sample(Ok(&format!("cpu {}", vec!["0"; 65].join(" "))), Ok(""), 1.0)
        .is_err());
}
proptest::proptest! {
    #[test]
    fn ring_bounds_and_current_are_preserved(n in 0usize..600) {
        let mut h=HostHistory::default();
        for i in 0..n { h.sample(Ok(&format!("cpu {i} 0 0 {i}")),Ok("MemTotal: 1048576 kB"),i as f64).unwrap(); }
        proptest::prop_assert_eq!(h.retained(),n.min(HISTORY_CAP));
        let response=h.response();
        proptest::prop_assert_eq!(response.history.len(),n.min(180));
        proptest::prop_assert_eq!(response.current,response.history.last().cloned());
    }
    #[test]
    fn hostile_bounded_proc_always_returns(stat in ".{0,1000}",mem in ".{0,1000}") {
        let _=HostHistory::default().sample(Ok(&stat),Ok(&mem),1.0);
    }
}
#[tokio::test(start_paused = true)]
async fn sampler_cadence_cancellation_and_timeout_bound_work() {
    let history = Arc::new(Mutex::new(HostHistory::default()));
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let sampler = HostSampler::start_with(history.clone(), move || {
        observed.fetch_add(1, Ordering::SeqCst);
        async { (Ok("cpu 0 0 0 0".into()), Ok("".into())) }
    });
    tokio::task::yield_now().await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    tokio::time::advance(std::time::Duration::from_secs(2)).await;
    tokio::task::yield_now().await;
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(history.lock().await.retained(), 2);
    drop(sampler);
    tokio::task::yield_now().await;
    tokio::time::advance(std::time::Duration::from_secs(100)).await;
    tokio::task::yield_now().await;
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let _sampler = HostSampler::start_with(history, move || {
        observed.fetch_add(1, Ordering::SeqCst);
        std::future::pending()
    });
    tokio::task::yield_now().await;
    tokio::time::advance(std::time::Duration::from_secs(1)).await;
    tokio::task::yield_now().await;
    tokio::time::advance(std::time::Duration::from_secs(100)).await;
    tokio::task::yield_now().await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}
