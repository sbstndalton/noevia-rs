#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::expect_used
)]
mod common;
use chat_mirror_sched::brain::{Brains, Timings};
use common::*;
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn m(n: u64) -> Duration {
    Duration::from_secs(n * 60)
}

type Built = Arc<Mutex<Vec<(String, String)>>>;

fn make() -> (Brains, Arc<BrainFake>, Built) {
    let exec = BrainFake::new();
    let built: Built = Arc::default();
    let b = built.clone();
    let brains = Brains::new(
        exec.clone(),
        Arc::new(move |u: &str, c: &str| b.lock().unwrap().push((u.into(), c.into()))),
        Arc::new(Lines::default()),
        Timings::default(),
    );
    (brains, exec, built)
}

#[tokio::test(start_paused = true)]
async fn an_idle_chat_is_built_once_and_the_mirror_nudged_later_changes_debounce() {
    let (b, exec, built) = make();
    exec.then("built");
    b.schedule(U1, "c1");
    advance(m(2)).await;
    b.schedule(U1, "c1");
    advance(m(2)).await;
    assert!(
        exec.calls().is_empty(),
        "each change restarts the idle timer"
    );
    advance(m(1)).await;
    assert_eq!(exec.calls(), vec![(U1.to_string(), "c1".to_string())]);
    assert_eq!(
        built.lock().unwrap().clone(),
        vec![(U1.to_string(), "c1".to_string())]
    );
    assert!(!b.pending(U1, "c1"));
}

#[tokio::test(start_paused = true)]
async fn only_a_retryable_failure_is_retried_once_after_fifteen_minutes() {
    let (b, exec, built) = make();
    exec.then("deadline");
    exec.then("deadline");
    b.schedule(U1, "c1");
    advance(m(3)).await;
    assert_eq!(exec.calls().len(), 1);
    advance(m(14)).await;
    assert_eq!(exec.calls().len(), 1);
    advance(m(1)).await;
    assert_eq!(exec.calls().len(), 2, "retried once");
    advance(m(60)).await;
    assert_eq!(exec.calls().len(), 2, "and not again");
    assert!(built.lock().unwrap().is_empty());
    for reason in ["no-model", "empty", "off", "gone", "current"] {
        exec.then(reason);
        b.schedule(U2, reason);
        advance(m(3)).await;
        advance(m(60)).await;
        assert_eq!(
            exec.calls().iter().filter(|(_, c)| c == reason).count(),
            1,
            "{reason} is not retried"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn builds_run_one_at_a_time_across_accounts_in_the_order_their_timers_fired() {
    let (b, exec, _) = make();
    *exec.work.lock().unwrap() = Duration::from_secs(30);
    b.schedule(U1, "a");
    advance(Duration::from_secs(1)).await;
    b.schedule(U2, "b");
    advance(Duration::from_secs(1)).await;
    b.schedule(U1, "c");
    // a's timer fires at 180 s and its build runs until 210 s; b (181 s) and c (182 s) wait their turn.
    advance(Duration::from_secs(178)).await;
    advance(Duration::from_secs(1)).await;
    advance(Duration::from_secs(1)).await;
    assert_eq!(exec.calls().len(), 1, "only the first build has started");
    for _ in 0..8 {
        advance(Duration::from_secs(30)).await;
    }
    assert_eq!(exec.peak(), 1, "the reasoner never runs twice at once");
    let order: Vec<String> = exec.calls().into_iter().map(|(_, c)| c).collect();
    assert_eq!(order, vec!["a", "b", "c"]);
}

#[tokio::test(start_paused = true)]
async fn a_newer_change_keeps_its_idle_timer_when_the_build_before_it_failed() {
    let (b, exec, _) = make();
    *exec.work.lock().unwrap() = Duration::from_secs(60);
    exec.then("error");
    b.schedule(U1, "c1");
    advance(m(3)).await;
    advance(Duration::from_secs(10)).await; // build in flight
    b.schedule(U1, "c1"); // a new change while it runs: idle timer starts now
    advance(Duration::from_secs(60)).await; // the build ends in error
    assert!(b.pending(U1, "c1"));
    advance(m(3)).await;
    assert_eq!(
        exec.calls().len(),
        2,
        "the newer change builds after its idle time, not 15 minutes later"
    );
}

#[tokio::test(start_paused = true)]
async fn a_panicking_executor_is_an_error_for_that_chat_only() {
    let (b, exec, _) = make();
    exec.then("panic");
    b.schedule(U1, "a");
    advance(Duration::from_secs(1)).await;
    b.schedule(U2, "b");
    advance(Duration::from_secs(179)).await; // a fires and panics
    advance(Duration::from_secs(1)).await; // b fires
    assert_eq!(exec.calls().len(), 2, "the next chat still builds");
    advance(m(15)).await;
    assert_eq!(
        exec.calls().iter().filter(|(_, c)| c == "a").count(),
        2,
        "the panicked build counts as an error and is retried once"
    );
    assert_eq!(exec.calls().iter().filter(|(_, c)| c == "b").count(), 1);
}

#[tokio::test(start_paused = true)]
async fn dropping_the_scheduler_stops_its_timers() {
    let (b, exec, _) = make();
    b.schedule(U1, "a");
    drop(b);
    advance(m(10)).await;
    assert!(exec.calls().is_empty());
}
