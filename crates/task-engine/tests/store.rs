//! Refusals, state moves, limits and hostile input for the job store. Synthetic ids and data only.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use serde_json::{json, Value};
use task_engine::{parse_jsonl, EngineError, Event, JobStore, Limits, NewJob, Status};

const A: &str = "11111111-1111-4111-8111-111111111111";
const B: &str = "22222222-2222-4222-8222-222222222222";

fn caps(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
}

fn new_job<'a>(kind: &'a str, capabilities: &'a [String], parent: Option<&'a str>) -> NewJob<'a> {
    NewJob {
        kind,
        project_id: Some("qa-project"),
        parent_id: parent,
        capabilities,
    }
}

fn store_with(id: &str) -> JobStore {
    let mut s = JobStore::new(Limits::default());
    s.create(id, 1, new_job("code", &[], None)).unwrap();
    s
}

#[test]
fn bad_job_ids_are_refused_before_anything_is_read() {
    let mut s = JobStore::new(Limits::default());
    for bad in [
        "",
        "ABCDEF01-1111-4111-8111-111111111111",
        "short",
        "11111111-1111-4111-8111-11111111111g",
        "../../etc/passwd",
    ] {
        assert_eq!(
            s.create(bad, 1, new_job("code", &[], None)).unwrap_err(),
            EngineError::BadJobId,
            "{bad}"
        );
        assert_eq!(s.start(bad, 1).unwrap_err(), EngineError::BadJobId, "{bad}");
    }
    assert_eq!(s.start(A, 1).unwrap_err(), EngineError::NoSuchJob);
}

#[test]
fn server_only_and_unknown_types_are_refused_on_the_public_path() {
    let mut s = store_with(A);
    assert_eq!(
        s.append(A, 2, "task.stage", json!({})).unwrap_err(),
        EngineError::ServerOnly
    );
    assert_eq!(
        s.append(A, 2, "task.revision", json!({})).unwrap_err(),
        EngineError::ServerOnly
    );
    assert_eq!(
        s.append(A, 2, "job.exploded", json!({})).unwrap_err(),
        EngineError::UnknownType
    );
    assert_eq!(
        s.append(A, 2, "job.created", json!({})).unwrap_err().code(),
        "bad_input"
    );
    assert_eq!(
        s.append(A, 2, "progress", json!("not an object"))
            .unwrap_err()
            .code(),
        "bad_input"
    );
    assert_eq!(
        s.get(A).unwrap().event_count,
        1,
        "a refused event writes nothing"
    );
}

#[test]
fn oversized_event_data_is_refused() {
    let mut s = store_with(A);
    let big = "x".repeat(65 * 1024);
    let err = s
        .append(A, 2, "assistant.output", json!({ "text": big }))
        .unwrap_err();
    assert_eq!(err, EngineError::TooLarge("event data is over 64 KiB"));
    assert_eq!(err.status(), 413);
}

#[test]
fn a_child_cannot_widen_its_parents_capabilities() {
    let mut s = JobStore::new(Limits::default());
    let parent_caps = caps(&["read"]);
    s.create(A, 1, new_job("code", &parent_caps, None)).unwrap();
    let wider = caps(&["read", "write"]);
    assert_eq!(
        s.create(B, 2, new_job("code", &wider, Some(A)))
            .unwrap_err(),
        EngineError::WidenCapabilities
    );
    let narrower = caps(&["read", "read"]);
    let child = s.create(B, 3, new_job("code", &narrower, Some(A))).unwrap();
    assert_eq!(child.capabilities, caps(&["read"]));
    assert_eq!(child.parent_id.as_deref(), Some(A));
}

#[test]
fn a_missing_parent_is_refused() {
    let mut s = JobStore::new(Limits::default());
    assert_eq!(
        s.create(B, 1, new_job("code", &[], Some(A))).unwrap_err(),
        EngineError::NoSuchParent
    );
}

#[test]
fn start_is_only_legal_from_queued() {
    let mut s = store_with(A);
    s.start(A, 2).unwrap();
    assert_eq!(s.start(A, 3).unwrap_err().code(), "illegal_transition");
    assert_eq!(s.get(A).unwrap().status, Status::Running);
}

#[test]
fn an_action_approval_takes_the_classifier_name_and_skips_actions_that_never_need_one() {
    use code_actions::Action;
    let mut s = store_with(A);
    s.start(A, 2).unwrap();
    let read = s.request_action_approval(A, 3, "r", Action::Read, json!({}));
    assert_eq!(read.unwrap_err().code(), "bad_input");
    let none = s.request_action_approval(A, 3, "r", Action::None, json!({}));
    assert_eq!(none.unwrap_err().code(), "bad_input");
    let view = s
        .request_action_approval(A, 3, "d", Action::Delete, json!({ "action": "read" }))
        .unwrap();
    assert_eq!(view.pending_approval_id.as_deref(), Some("d"));
    assert_eq!(
        view.pending_approval.as_ref().unwrap()["action"],
        json!(Action::Delete.name())
    );
    assert_eq!(view.status, Status::WaitingApproval);
}

#[test]
fn approvals_need_a_running_job_and_the_matching_id() {
    let mut s = store_with(A);
    let card = json!({ "action": "write", "command": "touch x" });
    assert_eq!(
        s.request_approval(A, 2, "appr-1", card.clone())
            .unwrap_err()
            .code(),
        "illegal_transition"
    );
    s.start(A, 2).unwrap();
    assert_eq!(
        s.request_approval(A, 3, "", card.clone())
            .unwrap_err()
            .code(),
        "bad_input"
    );
    assert_eq!(
        s.request_approval(A, 3, "appr-1", json!("no"))
            .unwrap_err()
            .code(),
        "bad_input"
    );
    s.request_approval(A, 4, "appr-1", card).unwrap();
    assert_eq!(s.get(A).unwrap().status, Status::WaitingApproval);
    assert_eq!(
        s.get(A).unwrap().pending_approval_id.as_deref(),
        Some("appr-1")
    );
    assert_eq!(
        s.decide_approval(A, 5, "appr-9", "approve")
            .unwrap_err()
            .code(),
        "bad_input"
    );
    assert_eq!(
        s.decide_approval(A, 5, "appr-1", "maybe")
            .unwrap_err()
            .code(),
        "bad_input"
    );
    s.decide_approval(A, 5, "appr-1", "approve").unwrap();
    let view = s.get(A).unwrap();
    assert_eq!(view.status, Status::Running);
    assert_eq!(view.pending_approval, None);
    assert_eq!(
        s.decide_approval(A, 6, "appr-1", "approve")
            .unwrap_err()
            .code(),
        "illegal_transition"
    );
}

#[test]
fn a_finished_job_takes_no_event_but_cancel_is_a_no_op() {
    let mut s = store_with(A);
    s.start(A, 2).unwrap();
    s.complete(A, 3, json!({ "ok": true })).unwrap();
    let before = s.events(A).unwrap().len();
    assert_eq!(
        s.append(A, 4, "progress", json!({ "stage": "x" }))
            .unwrap_err(),
        EngineError::Finished
    );
    assert_eq!(s.cancel(A, 5).unwrap().status, Status::Completed);
    assert_eq!(s.events(A).unwrap().len(), before);
}

#[test]
fn lifecycle_follows_start_complete_and_failure() {
    let mut s = store_with(A);
    assert_eq!(s.get(A).unwrap().lifecycle, Some("planned"));
    s.start(A, 2).unwrap();
    assert_eq!(s.get(A).unwrap().lifecycle, Some("implementing"));
    s.append(A, 3, "progress", json!({ "stage": "verifying" }))
        .unwrap();
    assert_eq!(s.get(A).unwrap().lifecycle, Some("verifying"));
    s.complete(A, 4, Value::Null).unwrap();
    assert_eq!(s.get(A).unwrap().lifecycle, Some("verifying"));
}

#[test]
fn a_failed_task_is_blocked_and_a_queued_completion_ends_the_lifecycle() {
    let mut s = store_with(A);
    s.fail(A, 2, "no worktree").unwrap();
    assert_eq!(s.get(A).unwrap().lifecycle, Some("blocked"));
    assert_eq!(s.get(A).unwrap().error.as_deref(), Some("no worktree"));

    let mut t = store_with(B);
    t.complete(B, 2, json!(1)).unwrap();
    assert_eq!(t.get(B).unwrap().status, Status::Completed);
    assert_eq!(
        t.get(B).unwrap().lifecycle,
        None,
        "planned -> verifying is illegal, as in task-lifecycle.cjs"
    );
}

#[test]
fn failure_messages_are_clipped_to_500_characters() {
    let mut s = store_with(A);
    let long = "é".repeat(600);
    s.fail(A, 2, &long).unwrap();
    assert_eq!(
        s.get(A).unwrap().error.as_deref().unwrap().chars().count(),
        500
    );
}

#[test]
fn failure_clip_counts_utf16_units_and_never_splits_a_pair() {
    // `String(...).slice(0, 500)` counts UTF-16 units: 250 emoji fill it exactly.
    let mut s = store_with(A);
    s.fail(A, 2, &"😀".repeat(300)).unwrap();
    assert_eq!(
        s.get(A).unwrap().error.as_deref().unwrap(),
        "😀".repeat(250)
    );
    // A cut that would split a pair drops the character (JS keeps a lone surrogate).
    let mut s = store_with(A);
    s.fail(A, 2, &format!("a{}", "😀".repeat(300))).unwrap();
    let error = s.get(A).unwrap().error.clone().unwrap();
    assert_eq!(error.encode_utf16().count(), 499);
}

#[test]
fn recover_interrupts_only_unfinished_jobs_that_are_not_live() {
    let mut s = JobStore::new(Limits::default());
    s.create(A, 1, new_job("code", &[], None)).unwrap();
    s.start(A, 2).unwrap();
    s.request_approval(A, 3, "appr-1", json!({})).unwrap();
    s.create(B, 4, new_job("code", &[], None)).unwrap();
    s.start(B, 5).unwrap();
    let done = s.recover(6, &|id| id == B);
    assert_eq!(done, vec![A.to_string()]);
    let view = s.get(A).unwrap();
    assert_eq!(view.status, Status::Interrupted);
    assert_eq!(
        view.error.as_deref(),
        Some("The server restarted while waiting for approval; start again to be asked again.")
    );
    assert_eq!(s.get(B).unwrap().status, Status::Running);
}

#[test]
fn prune_drops_old_finished_jobs_and_keeps_the_newest_limit() {
    let mut s = JobStore::new(Limits::default());
    let ids = [
        "00000000-0000-4000-8000-000000000001",
        "00000000-0000-4000-8000-000000000002",
        "00000000-0000-4000-8000-000000000003",
    ];
    for (i, id) in ids.iter().enumerate() {
        s.create(id, 100 + i as u64, new_job("code", &[], None))
            .unwrap();
        s.cancel(id, 200 + i as u64).unwrap();
    }
    s.create(
        "00000000-0000-4000-8000-000000000004",
        150,
        new_job("code", &[], None),
    )
    .unwrap();
    let removed = s.prune(10_000, 1_000_000, 1);
    assert_eq!(
        removed.len(),
        2,
        "two of three finished jobs beyond the limit of one"
    );
    assert!(
        s.get("00000000-0000-4000-8000-000000000004").is_some(),
        "a queued job is never pruned"
    );
    let removed = s.prune(10_000_000, 5, 10);
    assert_eq!(
        removed.len(),
        1,
        "the last finished job is older than the retention"
    );
}

#[test]
fn count_limits_are_enforced() {
    let limits = Limits {
        max_jobs: 1,
        max_capabilities: 2,
        max_capability_bytes: 8,
        max_card_bytes: 64,
    };
    let mut s = JobStore::new(limits);
    assert_eq!(
        s.create(A, 1, new_job("code", &caps(&["a", "b", "c"]), None))
            .unwrap_err()
            .code(),
        "limit"
    );
    assert_eq!(
        s.create(A, 1, new_job("code", &caps(&["way-too-long-name"]), None))
            .unwrap_err()
            .code(),
        "bad_input"
    );
    s.create(A, 1, new_job("code", &[], None)).unwrap();
    assert_eq!(
        s.create(B, 1, new_job("code", &[], None))
            .unwrap_err()
            .code(),
        "limit"
    );
    let card = json!({ "pad": "y".repeat(100) });
    s.start(A, 2).unwrap();
    assert_eq!(
        s.request_approval(A, 3, "x", card).unwrap_err().code(),
        "too_large"
    );
}

#[test]
fn a_job_takes_at_most_4096_events() {
    let mut s = store_with(A);
    for i in 1..4096u64 {
        s.append(A, i + 1, "progress", json!({ "stage": "x" }))
            .unwrap();
    }
    assert_eq!(s.get(A).unwrap().event_count, 4096);
    assert_eq!(
        s.append(A, 9_999, "progress", json!({}))
            .unwrap_err()
            .code(),
        "limit"
    );
}

#[test]
fn loading_refuses_gaps_mixed_ids_authority_and_chains() {
    let good = vec![
        Event {
            job: A.into(),
            seq: 1,
            kind: "job.created".into(),
            at: 1,
            data: json!({ "kind": "code" }),
        },
        Event {
            job: A.into(),
            seq: 2,
            kind: "job.started".into(),
            at: 2,
            data: json!({}),
        },
    ];
    let mut s = JobStore::new(Limits::default());
    assert!(s.load(good.clone()).is_ok());

    let mut gap = good.clone();
    gap[1].seq = 3;
    assert_eq!(
        JobStore::new(Limits::default())
            .load(gap)
            .unwrap_err()
            .code(),
        "bad_journal"
    );

    let mut mixed = good.clone();
    mixed[1].job = B.into();
    assert_eq!(
        JobStore::new(Limits::default())
            .load(mixed)
            .unwrap_err()
            .code(),
        "bad_journal"
    );

    let mut no_created = good.clone();
    no_created.remove(0);
    assert_eq!(
        JobStore::new(Limits::default())
            .load(no_created)
            .unwrap_err()
            .code(),
        "bad_journal"
    );

    assert_eq!(
        JobStore::new(Limits::default())
            .load(Vec::new())
            .unwrap_err()
            .code(),
        "bad_journal"
    );

    let mut authority = good.clone();
    authority.push(Event {
        job: A.into(),
        seq: 3,
        kind: "task.stage".into(),
        at: 3,
        data: json!({}),
    });
    assert_eq!(
        JobStore::new(Limits::default())
            .load(authority)
            .unwrap_err()
            .code(),
        "bad_journal"
    );

    let chained = "{\"job\":\"11111111-1111-4111-8111-111111111111\",\"seq\":1,\"type\":\"job.created\",\"at\":1,\"data\":{\"kind\":\"code\"},\"hash\":\"00\",\"previous\":null}\n";
    assert_eq!(parse_jsonl(chained).unwrap_err().code(), "bad_journal");
    let torn = "{\"job\":\"11111111-1111-4111-8111-111111111111\",\"seq\":1,\"type\":\"job.cre";
    assert_eq!(parse_jsonl(torn).unwrap_err().code(), "bad_journal");
    assert_eq!(parse_jsonl("[1,2]\n").unwrap_err().code(), "bad_journal");
}

#[test]
fn list_is_newest_first() {
    let mut s = JobStore::new(Limits::default());
    s.create(A, 1, new_job("code", &[], None)).unwrap();
    s.create(B, 9, new_job("code", &[], None)).unwrap();
    let order: Vec<&str> = s.list().iter().map(|v| v.id.as_str()).collect();
    assert_eq!(order, vec![B, A]);
}

/// Random operation sequences over one job: nothing panics, a terminal job never changes, and
/// every refusal is one of the codes this crate defines.
#[test]
fn random_sequences_never_panic_and_terminal_jobs_stay_terminal() {
    let mut seed: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut next = move |n: u64| {
        seed = seed
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (seed >> 33) % n
    };
    for _ in 0..200 {
        let mut s = store_with(A);
        let mut was_terminal = false;
        for step in 0..40u64 {
            let at = 2 + step;
            let r = match next(9) {
                0 => s.start(A, at).map(|_| ()),
                1 => s.request_approval(A, at, "p", json!({})).map(|_| ()),
                2 => s.decide_approval(A, at, "p", "approve").map(|_| ()),
                3 => s.complete(A, at, json!(null)).map(|_| ()),
                4 => s.fail(A, at, "boom").map(|_| ()),
                5 => s.cancel(A, at).map(|_| ()),
                6 => s
                    .append(A, at, "progress", json!({ "stage": "verifying" }))
                    .map(|_| ()),
                7 => s.interrupt(A, at, "restart").map(|_| ()),
                _ => s.append(A, at, "task.stage", json!({})).map(|_| ()),
            };
            if let Err(e) = r {
                assert!(
                    [
                        "bad_input",
                        "illegal_transition",
                        "finished",
                        "server_only",
                        "no_such_job",
                        "unknown_type",
                        "limit",
                        "bad_journal",
                        "too_large",
                        "bad_job_id"
                    ]
                    .contains(&e.code()),
                    "{e}"
                );
            }
            let terminal = s.get(A).unwrap().status.is_terminal();
            assert!(!was_terminal || terminal, "a finished job changed status");
            was_terminal = terminal;
        }
        let events = s.events(A).unwrap().to_vec();
        let text = task_engine::to_jsonl(&events);
        assert_eq!(parse_jsonl(&text).unwrap(), events);
    }
}

#[test]
fn a_node_shaped_card_without_an_id_is_answered_by_the_host_id() {
    let mut s = store_with(A);
    s.start(A, 2).unwrap();
    let card = json!({ "taskId": A, "action": "execute", "command": "ls", "paths": [] });
    s.request_approval(A, 3, "host-7", card.clone()).unwrap();
    let view = s.get(A).unwrap();
    assert_eq!(
        view.pending_approval.as_ref(),
        Some(&card),
        "the card is kept as given"
    );
    assert_eq!(view.pending_approval_id.as_deref(), Some("host-7"));
    assert_eq!(
        s.decide_approval(A, 4, "host-7", "deny").unwrap().status,
        Status::Running
    );
}

#[test]
fn a_card_cannot_name_its_own_approval_id() {
    let mut s = store_with(A);
    s.start(A, 2).unwrap();
    // A model-shaped card that carries an `approvalId` of its own: the host's id wins.
    s.request_approval(A, 3, "host-8", json!({ "approvalId": "forged" }))
        .unwrap();
    assert_eq!(
        s.get(A).unwrap().pending_approval_id.as_deref(),
        Some("host-8")
    );
    assert_eq!(
        s.decide_approval(A, 4, "forged", "approve")
            .unwrap_err()
            .code(),
        "bad_input"
    );
    assert_eq!(
        s.get(A).unwrap().pending_approval.as_ref(),
        Some(&json!({}))
    );
}
