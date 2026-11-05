#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::expect_used
)]
mod common;
use chat_mirror_sched::journal::{system_clock, Activity, Change, Consumer, ReadError};
use common::*;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn make(fake: &Arc<Fake>) -> (Consumer, Arc<Sink>, Arc<Lines>) {
    make_with(fake, None, system_clock())
}

fn make_with(
    fake: &Arc<Fake>,
    activity: Option<Activity>,
    now: chat_mirror_sched::journal::Clock,
) -> (Consumer, Arc<Sink>, Arc<Lines>) {
    let sink = Arc::new(Sink::default());
    let log = Arc::new(Lines::default());
    let c = Consumer::new(
        fake.clone(),
        sink.clone(),
        log.clone(),
        activity,
        now,
        Duration::from_secs(2),
        Duration::from_secs(15 * 60),
    );
    (c, sink, log)
}

/// A consumer that has finished its first (rescan) read of `user`.
async fn primed(fake: &Arc<Fake>, user: &str) -> (Consumer, Arc<Sink>, Arc<Lines>) {
    let (c, sink, log) = make(fake);
    c.poll(user).await;
    sink.take();
    (c, sink, log)
}

#[tokio::test]
async fn a_history_save_schedules_the_mirror_and_the_brain() {
    let fake = Fake::new();
    fake.set(U1, vec![]);
    let (c, sink, _) = primed(&fake, U1).await;
    fake.push(U1, save(1, "c1", "r1"));
    c.poll(U1).await;
    assert_eq!(sink.take(), vec![("c1".to_string(), true)]);
    assert_eq!(c.cursor(U1), Some(1));
}

#[tokio::test]
async fn the_same_chat_and_revision_is_scheduled_once_a_new_revision_again() {
    let fake = Fake::new();
    fake.set(U1, vec![]);
    let (c, sink, _) = primed(&fake, U1).await;
    fake.push(U1, save(1, "c1", "r1"));
    fake.push(U1, save(2, "c1", "r1"));
    fake.push(U1, save(3, "c1", "r2"));
    c.poll(U1).await;
    assert_eq!(
        sink.take(),
        vec![("c1".to_string(), true), ("c1".to_string(), true)]
    );
}

#[tokio::test]
async fn only_a_history_save_queues_a_brain() {
    let fake = Fake::new();
    fake.set(U1, vec![]);
    let (c, sink, _) = primed(&fake, U1).await;
    fake.push(U1, entry(1, "imp", "r", "write", "import"));
    fake.push(U1, entry(2, "app", "r", "append", "turn"));
    fake.push(U1, entry(3, "rem", "r", "remove", "remove"));
    fake.push(
        U1,
        json!({"seq": 4, "chatId": "old", "revision": "r", "op": "write", "start": START}),
    );
    fake.push(U1, entry(5, "oth", "r", "write", "other"));
    fake.push(U1, save(6, "hist", "r"));
    c.poll(U1).await;
    let got = sink.take();
    assert_eq!(
        got.iter().filter(|(_, b)| *b).collect::<Vec<_>>(),
        vec![&("hist".to_string(), true)]
    );
    assert_eq!(got.len(), 6, "every entry still schedules its mirror");
}

#[tokio::test]
async fn a_restart_rescans_every_chat_still_in_the_journal_once_each() {
    let fake = Fake::new();
    fake.set(
        U1,
        vec![save(1, "a", "r1"), save(2, "b", "r1"), save(3, "a", "r2")],
    );
    let (c, sink, _) = make(&fake);
    c.poll(U1).await;
    assert_eq!(
        sink.take(),
        vec![("b".to_string(), false), ("a".to_string(), false)],
        "newest revision per chat, mirror only, oldest-last-seen first"
    );
    assert_eq!(sink.rescans.lock().unwrap().clone(), vec![U1.to_string()]);
    assert_eq!(c.cursor(U1), Some(3));
    // a second consumer (a restart) does it again
    let (c2, sink2, _) = make(&fake);
    c2.poll(U1).await;
    assert_eq!(sink2.chats().len(), 2);
}

#[tokio::test]
async fn a_reader_behind_a_rotation_or_past_a_reset_journal_rescans() {
    let fake = Fake::new();
    fake.set(U1, vec![save(1, "a", "r")]);
    let (c, sink, _) = primed(&fake, U1).await;
    // rotation: entries 2..9 are gone, the journal now starts at 10
    fake.set(U1, vec![save(10, "x", "r"), save(11, "y", "r")]);
    c.poll(U1).await;
    assert_eq!(
        sink.take()
            .iter()
            .map(|(c, _)| c.clone())
            .collect::<Vec<_>>(),
        vec!["x", "y"]
    );
    assert_eq!(sink.rescans.lock().unwrap().len(), 2);
    // reset: the journal starts over below the cursor
    fake.set(U1, vec![save(1, "z", "r")]);
    c.poll(U1).await;
    assert_eq!(sink.take(), vec![("z".to_string(), false)]);
    assert_eq!(c.cursor(U1), Some(1));
}

#[tokio::test]
async fn a_failed_read_is_logged_once_retried_and_never_panics() {
    let fake = Fake::new();
    fake.set(U1, vec![]);
    let (c, sink, log) = primed(&fake, U1).await;
    fake.push(U1, save(1, "a", "r"));
    for _ in 0..3 {
        fake.fail
            .lock()
            .unwrap()
            .entry(U1.into())
            .or_default()
            .push_back(ReadError::Failed("down".into()));
        c.poll(U1).await;
    }
    assert!(sink.chats().is_empty());
    let failed = log
        .all()
        .into_iter()
        .filter(|l| l.contains("read failed"))
        .count();
    assert_eq!(failed, 1, "once per run of failures");
    c.poll(U1).await;
    assert_eq!(sink.take(), vec![("a".to_string(), true)]);
    assert!(log.all().iter().any(|l| l.contains("reading again")));
}

#[tokio::test]
async fn a_gone_account_is_forgotten_and_a_new_touch_starts_fresh() {
    let fake = Fake::new();
    fake.set(U1, vec![save(1, "a", "r")]);
    let (c, _sink, log) = primed(&fake, U1).await;
    c.touch(U1);
    settle().await; // the touch's own read finishes first
    fake.journals.lock().unwrap().remove(U1);
    c.poll(U1).await;
    assert!(!c.tracked(U1));
    assert!(log.all().iter().any(|l| l.contains("account removed")));
    let reads = fake.read_count(U1);
    c.tick().await;
    assert_eq!(fake.read_count(U1), reads, "no further reads");
    fake.set(U1, vec![save(1, "a", "r")]);
    let (c2, sink2, _) = make(&fake);
    c2.poll(U1).await;
    assert_eq!(sink2.chats().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn asked_again_while_a_read_runs_it_reads_once_more_after() {
    let fake = Fake::new();
    fake.set(U1, vec![]);
    let (c, sink, _) = primed(&fake, U1).await;
    let c = Arc::new(c);
    fake.push(U1, save(1, "a", "r"));
    let c1 = c.clone();
    let first = tokio::spawn(async move { c1.poll(U1).await });
    // a save lands while the first poll is running; the second ask must not be lost
    fake.push(U1, save(2, "b", "r"));
    c.poll(U1).await;
    first.await.unwrap();
    c.poll(U1).await;
    let mut got: Vec<String> = sink.take().into_iter().map(|(c, _)| c).collect();
    got.sort();
    assert_eq!(got, vec!["a", "b"]);
}

#[tokio::test]
async fn the_timer_reads_a_user_only_when_the_journal_stamp_changed() {
    let fake = Fake::new();
    fake.set(U1, vec![]);
    let (c, sink, _) = make(&fake);
    c.touch(U1);
    settle().await;
    let after_touch = fake.read_count(U1);
    assert!(after_touch >= 1);
    c.tick().await;
    c.tick().await;
    assert_eq!(
        fake.read_count(U1),
        after_touch,
        "an unchanged journal costs a stat, not a read"
    );
    fake.push(U1, save(1, "a", "r"));
    c.tick().await;
    assert!(fake.read_count(U1) > after_touch);
    assert_eq!(sink.take().last(), Some(&("a".to_string(), true)));
}

#[tokio::test]
async fn a_failed_read_makes_the_next_tick_read_again_whatever_the_stamp() {
    let fake = Fake::new();
    fake.set(U1, vec![]);
    let (c, _, _) = make(&fake);
    c.touch(U1);
    settle().await;
    fake.fail
        .lock()
        .unwrap()
        .entry(U1.into())
        .or_default()
        .push_back(ReadError::Failed("x".into()));
    c.poll(U1).await;
    let n = fake.read_count(U1);
    c.tick().await;
    assert!(fake.read_count(U1) > n);
}

#[tokio::test]
async fn only_users_seen_within_the_window_are_read_and_the_front_activity_counts() {
    let fake = Fake::new();
    fake.set(U1, vec![]);
    fake.set(U2, vec![]);
    let now = Arc::new(Mutex::new(1_000_000.0f64));
    let clock: chat_mirror_sched::journal::Clock = {
        let n = now.clone();
        Arc::new(move || *n.lock().unwrap())
    };
    let doc: Arc<Mutex<Option<HashMap<String, f64>>>> = Arc::new(Mutex::new(Some(HashMap::from(
        [(U2.to_string(), 1_000_000.0)],
    ))));
    let activity: Activity = {
        let d = doc.clone();
        Arc::new(move || d.lock().unwrap().clone())
    };
    let (c, sink, _) = make_with(&fake, Some(activity), clock);
    // U2 is known only from the front's file
    c.tick().await;
    assert_eq!(
        fake.read_count(U2),
        1,
        "a user who only uses the front's routes is read"
    );
    assert_eq!(fake.read_count(U1), 0, "nobody asked about U1");
    // 16 minutes later with the same file entry: the stop condition applies
    *now.lock().unwrap() += 16.0 * 60_000.0;
    fake.push(U2, save(1, "a", "r"));
    c.tick().await;
    assert_eq!(
        fake.read_count(U2),
        1,
        "idle past 15 minutes: no longer read"
    );
    assert!(sink.chats().is_empty());
    // a newer front time brings them back
    *doc.lock().unwrap() = Some(HashMap::from([(U2.to_string(), *now.lock().unwrap())]));
    c.tick().await;
    assert_eq!(sink.take(), vec![("a".to_string(), true)]);
}

fn mv(seq: u64, chat: &str, raw: &str, id: &str, delivery: &str) -> Value {
    json!({"seq": seq, "chatId": chat, "rawChatId": raw, "eventId": id, "generation": "7", "op": "move", "delivery": delivery, "start": START, "revision": null, "at": 1})
}
const E1: &str = "0123456789abcdef0123456789abcdef";
const E2: &str = "fedcba9876543210fedcba9876543210";

#[tokio::test]
async fn a_live_move_with_a_valid_identity_builds_the_brain_once_under_the_raw_id() {
    let fake = Fake::new();
    fake.set(U1, vec![]);
    let (c, sink, _) = primed(&fake, U1).await;
    fake.push(U1, mv(1, "chat1", "chat1", E1, "live"));
    fake.push(U1, mv(2, "chat1", "chat1", E1, "live"));
    c.poll(U1).await;
    let got = sink.changed.lock().unwrap().clone();
    assert_eq!(got.len(), 2, "the mirror hears every notice");
    assert_eq!(
        got[0].2,
        Change {
            brain: true,
            brain_chat: Some("chat1".into())
        }
    );
    assert!(!got[1].2.brain, "a duplicate event id never builds twice");
}

#[tokio::test]
async fn recovery_unknown_invalid_and_first_scan_moves_never_build() {
    let fake = Fake::new();
    fake.set(U1, vec![mv(1, "chat0", "chat0", E2, "live")]);
    let (c, sink, _) = make(&fake);
    c.poll(U1).await; // first scan: mirror only, whatever the entry says
    assert!(sink
        .changed
        .lock()
        .unwrap()
        .iter()
        .all(|(_, _, ch)| !ch.brain));
    sink.take();
    fake.push(U1, mv(2, "chat1", "chat1", E1, "recovery"));
    fake.push(U1, mv(3, "chat2", "chat2", "not-hex", "live"));
    fake.push(U1, mv(4, "chat3", "other", E1, "live"));
    let mut wrong_start = mv(
        5,
        "chat4",
        "chat4",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "live",
    );
    wrong_start["start"] = json!("ffffffffffffffffffffffffffffffff");
    fake.push(U1, wrong_start);
    let mut bad_gen = mv(
        6,
        "chat5",
        "chat5",
        "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        "live",
    );
    bad_gen["generation"] = json!("01");
    fake.push(U1, bad_gen);
    c.poll(U1).await;
    let got = sink.changed.lock().unwrap().clone();
    assert_eq!(got.len(), 5);
    assert!(got.iter().all(|(_, _, ch)| !ch.brain), "{got:?}");
}

#[tokio::test]
async fn a_rescan_seeing_a_move_marks_it_known_so_a_later_replay_cannot_build() {
    let fake = Fake::new();
    fake.set(U1, vec![mv(1, "chat1", "chat1", E1, "live")]);
    let (c, sink, _) = make(&fake);
    c.poll(U1).await;
    sink.take();
    fake.push(U1, mv(2, "chat1", "chat1", E1, "live"));
    c.poll(U1).await;
    assert!(sink
        .changed
        .lock()
        .unwrap()
        .iter()
        .all(|(_, _, ch)| !ch.brain));
}

#[tokio::test]
async fn mirror_and_brain_scheduling_is_idempotent_under_a_second_consumer() {
    let fake = Fake::new();
    fake.set(U1, vec![save(1, "a", "r")]);
    let (c, sink, _) = make(&fake);
    c.poll(U1).await;
    c.poll(U1).await;
    assert_eq!(
        sink.chats().len(),
        1,
        "a poll with nothing new schedules nothing"
    );
}
