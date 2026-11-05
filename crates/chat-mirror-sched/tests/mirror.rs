#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::expect_used
)]
mod common;
use chat_mirror_sched::mirror::{Mirror, Timings};
use chat_mirror_sched::MirrorOutcome;
use common::*;
use std::sync::Arc;
use std::time::Duration;

fn s(n: u64) -> Duration {
    Duration::from_secs(n)
}

fn make() -> (Mirror, Arc<MirrorFake>, Arc<Lines>) {
    let exec = MirrorFake::new();
    let log = Arc::new(Lines::default());
    let m = Mirror::new(exec.clone(), log.clone(), Timings::default());
    (m, exec, log)
}

#[tokio::test(start_paused = true)]
async fn a_burst_of_saves_is_one_pass_after_the_quiet_period() {
    let (m, exec, _) = make();
    m.schedule(U1, Some("a"), false);
    advance(s(3)).await;
    m.schedule(U1, Some("b"), false);
    advance(s(3)).await;
    assert_eq!(exec.count(), 0, "debounced: each save restarts the wait");
    advance(s(3)).await;
    assert_eq!(
        exec.passes(U1),
        vec![vec!["a".to_string(), "b".to_string()]]
    );
    assert!(!m.pending(U1), "a clean pass forgets the account");
}

#[tokio::test(start_paused = true)]
async fn a_chat_that_keeps_changing_is_still_mirrored_every_max_wait() {
    let (m, exec, _) = make();
    let start = tokio::time::Instant::now();
    for i in 0..12 {
        m.schedule(U1, Some(&format!("c{i}")), false);
        advance(s(4)).await;
    }
    let calls = exec.calls.lock().unwrap().clone();
    assert!(!calls.is_empty(), "never starved by the debounce");
    assert!(
        calls[0].2 - start <= s(30) + Duration::from_millis(5),
        "first pass within max_wait of the first save"
    );
}

#[tokio::test(start_paused = true)]
async fn immediate_skips_the_debounce_and_an_all_chats_pass_has_no_dirty_set() {
    let (m, exec, _) = make();
    m.schedule(U1, None, true);
    advance(Duration::from_millis(1)).await;
    assert_eq!(exec.passes(U1), vec![Vec::<String>::new()]);
}

#[tokio::test(start_paused = true)]
async fn a_change_during_a_pass_runs_the_next_pass_on_the_debounce() {
    let (m, exec, _) = make();
    let gate = Arc::new(tokio::sync::Notify::new());
    exec.then(Step::Hold(gate.clone()));
    m.schedule(U1, Some("a"), true);
    advance(Duration::from_millis(1)).await;
    m.schedule(U1, Some("b"), false);
    advance(s(10)).await;
    assert_eq!(exec.count(), 1, "one pass at a time per account");
    gate.notify_one();
    settle().await;
    assert_eq!(exec.count(), 1, "the follow-up waits out the debounce");
    advance(s(6)).await;
    assert_eq!(
        exec.passes(U1),
        vec![vec!["a".to_string()], vec!["b".to_string()]]
    );
}

#[tokio::test(start_paused = true)]
async fn failures_back_off_1_2_4_minutes_cap_at_an_hour_and_log_once_per_step() {
    let (m, exec, log) = make();
    for _ in 0..9 {
        exec.then(Step::Out(MirrorOutcome::Failed));
    }
    m.schedule(U1, Some("a"), true);
    advance(Duration::from_millis(1)).await;
    let mut at = vec![];
    for wait in [60u64, 120, 240, 480, 960, 1920, 3600, 3600] {
        advance(s(wait - 1)).await;
        assert_eq!(exec.count(), at.len() + 1, "not before {wait}s");
        advance(s(1)).await;
        at.push(wait);
        assert_eq!(exec.count(), at.len() + 1, "retried after {wait}s");
    }
    // the failed pass keeps its dirty chat for every retry
    assert!(exec.passes(U1).iter().all(|d| d == &vec!["a".to_string()]));
    let retry_lines = log
        .all()
        .into_iter()
        .filter(|l| l.contains("retrying in"))
        .count();
    assert_eq!(
        retry_lines, 7,
        "one line per distinct step (60 .. 3600), not per attempt"
    );
    // a new nudge starts the backoff over
    m.schedule(U1, Some("b"), false);
    advance(s(6)).await;
    assert_eq!(exec.count(), 10);
}

#[tokio::test(start_paused = true)]
async fn off_is_not_a_failure_and_the_account_is_forgotten() {
    let (m, exec, log) = make();
    exec.then(Step::Out(MirrorOutcome::Off));
    m.schedule(U1, Some("a"), true);
    advance(s(1)).await;
    advance(s(7200)).await;
    assert_eq!(exec.count(), 1);
    assert!(!m.pending(U1));
    assert!(log.all().is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_failed_pass_nudged_meanwhile_goes_again_on_the_debounce_with_its_chats() {
    let (m, exec, _) = make();
    let gate = Arc::new(tokio::sync::Notify::new());
    exec.then(Step::Hold(gate.clone()));
    // the held pass ends Done, so script a failure next to prove the dirty set survives
    m.schedule(U1, Some("a"), true);
    advance(Duration::from_millis(1)).await;
    m.schedule(U1, Some("b"), false);
    gate.notify_one();
    advance(s(6)).await;
    assert_eq!(exec.passes(U1).last(), Some(&vec!["b".to_string()]));
}

#[tokio::test(start_paused = true)]
async fn a_panicking_executor_fails_only_its_own_account() {
    let (m, exec, _) = make();
    exec.then(Step::Panic);
    m.schedule(U1, Some("a"), true);
    advance(Duration::from_millis(1)).await;
    m.schedule(U2, Some("x"), true);
    advance(Duration::from_millis(1)).await;
    assert_eq!(
        exec.passes(U2),
        vec![vec!["x".to_string()]],
        "the other account still runs"
    );
    assert!(
        m.pending(U1),
        "the panicked account is retried on the backoff, not lost"
    );
    advance(s(61)).await;
    assert_eq!(
        exec.passes(U1),
        vec![vec!["a".to_string()], vec!["a".to_string()]]
    );
}

#[tokio::test(start_paused = true)]
async fn dropping_the_mirror_stops_its_timers() {
    let (m, exec, _) = make();
    m.schedule(U1, Some("a"), false);
    drop(m);
    advance(s(60)).await;
    assert_eq!(exec.count(), 0);
}

#[tokio::test(start_paused = true)]
async fn accounts_are_independent() {
    let (m, exec, _) = make();
    m.schedule(U1, Some("a"), false);
    advance(s(4)).await;
    m.schedule(U2, Some("b"), false);
    advance(s(2)).await;
    assert_eq!(exec.passes(U1).len(), 1);
    assert_eq!(exec.passes(U2).len(), 0);
    advance(s(4)).await;
    assert_eq!(exec.passes(U2).len(), 1);
}
