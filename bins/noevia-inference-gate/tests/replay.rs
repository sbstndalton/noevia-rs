//! A replay of the failure of sbstndalton/noevia#2115 and #2117, at test speed.
//!
//! Two chats on different routes at once, one chat model at a time:
//! - chat A, on the Code route, runs a web-search turn: several streamed rounds with tool calls
//!   between them;
//! - chat B, on the Smart route, sends a follow-up while A runs, and retries like a user pressing
//!   Retry whenever it gets an error;
//! - pages poll the model properties and metrics (with `autoload=false`) all the while.
//!
//! Before the gate, B's admission unloaded the Code-route model in the middle of A's answer
//! (killing it), A's next request loaded it again, and B failed with "still unloading" four
//! times. Through the gate: no stream is cut, A's turn completes, B waits and then completes
//! without a single error, and the models switch once per turn, never back and forth per request.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod support;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use support::*;

const CODE: &str = "route-code";
const SMART: &str = "route-smart";

struct Outcome {
    a_rounds: Vec<u16>,
    b_attempts: Vec<u16>,
    b_done_after_a: bool,
}

async fn replay(a_uses_lease: bool) -> (FakeRouter, GateHandle, Outcome) {
    let router = FakeRouter::start(
        &[(CODE, St::Unloaded, false), (SMART, St::Sleeping, false)],
        FakeConfig::default(),
    )
    .await;
    let gate = Arc::new(start_gate(test_config(router.url())).await);
    let stop = Arc::new(AtomicBool::new(false));
    let poller = {
        let gate = Arc::clone(&gate);
        let stop = Arc::clone(&stop);
        tokio::spawn(async move {
            while !stop.load(Ordering::SeqCst) {
                for path in [
                    "/props?model=route-code&autoload=false",
                    "/metrics?model=route-smart&autoload=false",
                    "/models",
                ] {
                    let _ = client().get(gate.url(path)).send().await;
                }
                tokio::time::sleep(Duration::from_millis(60)).await;
            }
        })
    };
    let a = {
        let gate = Arc::clone(&gate);
        tokio::spawn(async move {
            let lease = if a_uses_lease {
                Some(acquire_lease(&gate, CODE).await.unwrap())
            } else {
                None
            };
            let mut rounds = Vec::new();
            for round in 0..4 {
                if round > 0 {
                    // The tool (a web search) runs between rounds.
                    tokio::time::sleep(Duration::from_millis(if a_uses_lease { 120 } else { 50 }))
                        .await;
                }
                let (s, body) = chat(&gate, CODE, 8, 40, lease.as_deref()).await;
                rounds.push(if s == 200 && !body.contains("[DONE]") {
                    599
                } else {
                    s
                });
            }
            if let Some(lease) = lease {
                release_lease(&gate, &lease).await;
            }
            (rounds, Instant::now())
        })
    };
    tokio::time::sleep(Duration::from_millis(200)).await;
    let mut attempts = Vec::new();
    let mut b_done = Instant::now();
    for _ in 0..6 {
        let (s, body) = chat(&gate, SMART, 3, 30, None).await;
        let s = if s == 200 && !body.contains("[DONE]") {
            599
        } else {
            s
        };
        attempts.push(s);
        if s == 200 {
            b_done = Instant::now();
            break;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    let (a_rounds, a_done) = a.await.unwrap();
    stop.store(true, Ordering::SeqCst);
    let _ = poller.await;
    let gate = Arc::try_unwrap(gate).ok().unwrap();
    (
        router,
        gate,
        Outcome {
            a_rounds,
            b_attempts: attempts,
            b_done_after_a: b_done >= a_done,
        },
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn the_2026_10_22_timeline_with_a_leased_turn_has_no_kill_and_no_error() {
    let (router, gate, out) = replay(true).await;
    assert!(
        router.cuts().is_empty(),
        "no stream cut: {:?}",
        router.events()
    );
    assert_eq!(
        out.a_rounds,
        vec![200, 200, 200, 200],
        "every round of the turn completes"
    );
    assert_eq!(
        out.b_attempts,
        vec![200],
        "the other route waits, then completes first time"
    );
    assert!(out.b_done_after_a, "it ran after the turn");
    let s = status(&gate).await;
    let switches = s["scheduler"]["counters"]["switches"].as_u64().unwrap();
    assert!(
        switches <= 2,
        "a sleeping model out, the other route in: {switches}"
    );
    assert_eq!(
        router.loads_of(CODE),
        1,
        "the Code-route model loaded once, never reloaded"
    );
    assert_eq!(router.loads_of(SMART), 1);
    assert_eq!(
        s["scheduler"]["counters"]["rejected"]
            .as_object()
            .map(|m| m.len()),
        Some(0)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn the_timeline_without_a_lease_cuts_nothing_either() {
    // A client without a lease (the Diary, another relay client) is admitted per request; the
    // other route may get the engine between its requests, but nothing is ever cut.
    let (router, _gate, out) = replay(false).await;
    assert!(
        router.cuts().is_empty(),
        "no stream cut: {:?}",
        router.events()
    );
    assert!(out.a_rounds.iter().all(|s| *s == 200), "{:?}", out.a_rounds);
    assert_eq!(out.b_attempts, vec![200]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn without_the_gate_the_old_admission_cuts_the_stream() {
    // The control: what the replay would show without the gate. The other route's admission
    // unloads the busy model directly at the router, as the web server's admission did.
    let router = FakeRouter::start(
        &[(CODE, St::Loaded, false), (SMART, St::Unloaded, false)],
        FakeConfig::default(),
    )
    .await;
    let url = router.url();
    let a = tokio::spawn(async move {
        let r = client()
            .post(format!("{url}/v1/chat/completions"))
            .body(chat_body(CODE, 10, 50))
            .send()
            .await
            .unwrap();
        r.text().await.unwrap_or_default()
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let r = client()
        .post(format!("{}/models/unload", router.url()))
        .body(r#"{"model":"route-code"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    let body = a.await.unwrap();
    assert!(!body.contains("[DONE]"), "the answer was cut");
    assert!(router.events().contains(&Event::StreamCut(CODE.into())));
}
