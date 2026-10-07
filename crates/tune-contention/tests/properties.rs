//! Properties of the foreign-load decision (noevia#1062).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use proptest::prelude::*;
use tune_contention::{decide, decide_json, Action, Prev, Request, Row};

fn status() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("loaded".to_owned()),
        Just("sleeping".to_owned()),
        Just("loading".to_owned()),
        Just("unloaded".to_owned()),
        Just("failed".to_owned()),
        "[a-z]{0,8}",
    ]
}

fn rows() -> impl Strategy<Value = Vec<Row>> {
    prop::collection::btree_map("[a-c]{1,3}", (status(), prop::option::of(0u64..4)), 0..6).prop_map(
        |m| {
            m.into_iter()
                .map(|(id, (status, busy))| Row { id, status, busy })
                .collect()
        },
    )
}

fn request() -> impl Strategy<Value = Request> {
    (
        rows(),
        prop::option::of((".{0,20}", 0u64..2_000_000)),
        0u64..1_000_000,
        0u64..3_000_000,
        0u64..2_000_000,
        0u64..200_000,
    )
        .prop_map(
            |(rows, prev, started_at, now, max_wait_ms, quiet_ms)| Request {
                tuning: "a".to_owned(),
                rows,
                prev: prev.map(|(fingerprint, since)| Prev { fingerprint, since }),
                started_at,
                now,
                max_wait_ms,
                quiet_ms,
            },
        )
}

proptest! {
    /// Never asks to stop a model that is loading, busy or in an unknown state, nor the tuned one.
    #[test]
    fn unload_only_idle_foreign(r in request()) {
        let d = decide(&r);
        for id in &d.unload {
            prop_assert!(id != &r.tuning);
            let row = r.rows.iter().find(|x| &x.id == id).unwrap();
            prop_assert!(row.status == "loaded" || row.status == "sleeping");
            prop_assert!(row.busy.is_none_or(|b| b == 0));
        }
        prop_assert_eq!(d.action == Action::Unload, !d.unload.is_empty());
    }

    /// Proceeds exactly when nothing foreign is live; gives up only past the limit.
    #[test]
    fn proceed_and_give_up(r in request()) {
        let d = decide(&r);
        prop_assert_eq!(d.action == Action::Proceed, d.foreign.is_empty());
        if d.action == Action::GiveUp { prop_assert!(r.now.saturating_sub(r.started_at) >= r.max_wait_ms); }
        if !d.foreign.is_empty() && r.now.saturating_sub(r.started_at) >= r.max_wait_ms {
            prop_assert_eq!(d.action, Action::GiveUp);
        }
        prop_assert!(d.since <= r.now);
    }

    /// Feeding a reply back with no time passing never unloads earlier than the quiet window.
    #[test]
    fn quiet_window_holds(r in request()) {
        let first = decide(&Request { prev: None, ..r.clone() });
        if first.action == Action::Wait && r.quiet_ms > 0 {
            let again = decide(&Request {
                prev: Some(Prev { fingerprint: first.fingerprint.clone(), since: first.since }),
                ..r.clone()
            });
            prop_assert_ne!(again.action, Action::Unload);
        }
    }

    /// Arbitrary text is either refused with a fixed code or decided; never a panic.
    #[test]
    fn json_never_panics(s in ".{0,200}") {
        let (status, reply) = decide_json(&s);
        prop_assert!(status <= 1);
        let refusals = [r#"{"error":"input"}"#, r#"{"error":"too_large"}"#];
        if status == 1 {
            prop_assert!(refusals.contains(&reply.as_str()), "unexpected refusal");
        }
    }
}
