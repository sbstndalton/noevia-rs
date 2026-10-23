//! Requests that cannot load never wait for a switch, and the hold of a detached session
//! generation ends when the session is stopped or its generation ends, and lasts while it runs
//! (sbstndalton/noevia#2133 items 1 and 2). Synthetic models only.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod support;

use futures_util::StreamExt;
use std::sync::Arc;
use std::time::{Duration, Instant};
use support::*;

const CODE: &str = "route-code";
const SMART: &str = "route-smart";

async fn setup(
    fake: FakeConfig,
    tweak: impl FnOnce(&mut noevia_inference_gate::GateConfig),
) -> (FakeRouter, Arc<GateHandle>) {
    let router = FakeRouter::start(
        &[(CODE, St::Loaded, false), (SMART, St::Unloaded, false)],
        fake,
    )
    .await;
    let mut cfg = test_config(router.url());
    tweak(&mut cfg);
    let gate = start_gate(cfg).await;
    (router, Arc::new(gate))
}

fn unloads_of(router: &FakeRouter, model: &str) -> usize {
    router
        .events()
        .iter()
        .filter(|e| matches!(e, Event::Unload { model: m, .. } if m == model))
        .count()
}

/// Starts a session generation on `model` and leaves after its first chunk; the router goes on.
async fn detach(router: &FakeRouter, gate: &GateHandle, conv_id: &str, chunks: u64, ms: u64) {
    let resp = client()
        .post(gate.url("/v1/chat/completions"))
        .header("x-conversation-id", conv_id)
        .body(chat_body(CODE, chunks, ms))
        .send()
        .await
        .unwrap();
    let mut stream = resp.bytes_stream();
    assert!(!stream.next().await.unwrap().unwrap().is_empty());
    drop(stream);
    assert!(
        eventually(2_000, || router
            .events()
            .contains(&Event::Detached(CODE.into())))
        .await
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn a_non_loading_request_for_the_resident_model_is_admitted_at_once_while_another_model_waits(
) {
    let (router, gate) = setup(FakeConfig::default(), |_| {}).await;
    // The resident model answers a long generation; another model queues behind it.
    let a = {
        let gate = Arc::clone(&gate);
        tokio::spawn(async move { chat(&gate, CODE, 40, 40, None).await })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    let b = {
        let gate = Arc::clone(&gate);
        tokio::spawn(async move { chat(&gate, SMART, 2, 20, None).await })
    };
    // Past the fair hold: new loading work for the resident model would queue now.
    tokio::time::sleep(Duration::from_millis(600)).await;
    let st = status(&gate).await;
    assert_eq!(st["scheduler"]["queue"][0]["model"], SMART, "{st}");
    let started = Instant::now();
    let r = client()
        .get(gate.url(&format!("/slots?model={CODE}&autoload=false")))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "admitted at once, not queued: {:?}",
        started.elapsed()
    );
    assert!(!a.is_finished(), "answered while the generation still ran");
    assert_eq!(a.await.unwrap().0, 200);
    assert_eq!(b.await.unwrap().0, 200);
    // The waiting model is served and is never unloaded for the read, nor is the read's model
    // loaded again.
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(unloads_of(&router, SMART), 0, "{:?}", router.events());
    assert_eq!(router.loads_of(CODE), 0, "{:?}", router.events());
    assert_eq!(router.status(SMART), St::Loaded);
    assert!(router.cuts().is_empty(), "{:?}", router.events());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn a_stop_releases_the_session_hold_at_once() {
    let (router, gate) = setup(FakeConfig::default(), |c| {
        c.admission.session_hold_ms = 20_000;
        // Only the stop can end the hold early here.
        c.session_check_interval = Duration::from_secs(3600);
    })
    .await;
    detach(&router, &gate, "conv-synthetic-stop", 500, 40).await;
    let b = {
        let gate = Arc::clone(&gate);
        tokio::spawn(async move { chat(&gate, SMART, 2, 20, None).await })
    };
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!b.is_finished(), "waits for the held generation");
    let started = Instant::now();
    let r = client()
        .delete(gate.url("/v1/stream?conv_id=conv-synthetic-stop"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 204);
    let (s, _) = tokio::time::timeout(Duration::from_secs(5), b)
        .await
        .expect("served well before the session hold ends")
        .unwrap();
    assert_eq!(s, 200);
    assert!(started.elapsed() < Duration::from_secs(3));
    assert!(router.events().contains(&Event::Stopped(CODE.into())));
    assert!(router.cuts().is_empty(), "{:?}", router.events());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn the_session_hold_is_released_when_the_detached_generation_ends() {
    let (router, gate) = setup(FakeConfig::default(), |c| {
        c.admission.session_hold_ms = 20_000;
    })
    .await;
    detach(&router, &gate, "conv-synthetic-end", 10, 40).await;
    let started = Instant::now();
    let (s, _) = tokio::time::timeout(Duration::from_secs(5), chat(&gate, SMART, 2, 20, None))
        .await
        .expect("served once the generation ended, well before the session hold ends");
    assert_eq!(s, 200);
    assert!(started.elapsed() < Duration::from_secs(3));
    assert!(router.cuts().is_empty(), "{:?}", router.events());
    assert!(router.state.lock().unwrap().lookups > 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn the_session_hold_is_released_when_a_resume_reaches_the_end() {
    let (router, gate) = setup(FakeConfig::default(), |c| {
        c.admission.session_hold_ms = 20_000;
        // Only the resume's end can end the hold early here.
        c.session_check_interval = Duration::from_secs(3600);
    })
    .await;
    detach(&router, &gate, "conv-synthetic-resume", 10, 40).await;
    let resumed = client()
        .get(gate.url("/v1/stream?conv_id=conv-synthetic-resume&from=1"))
        .send()
        .await
        .unwrap();
    assert!(resumed.text().await.unwrap().contains("[DONE]"));
    let (s, _) = tokio::time::timeout(Duration::from_secs(5), chat(&gate, SMART, 2, 20, None))
        .await
        .expect("served once the resumed answer ended");
    assert_eq!(s, 200);
    assert!(router.cuts().is_empty(), "{:?}", router.events());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn a_detached_generation_longer_than_the_session_hold_keeps_its_model_until_it_ends() {
    let (router, gate) = setup(FakeConfig::default(), |c| {
        c.admission.session_hold_ms = 400;
    })
    .await;
    // Runs about 1.6 s: four session holds.
    detach(&router, &gate, "conv-synthetic-long", 40, 40).await;
    let (s, _) = chat(&gate, SMART, 2, 20, None).await;
    assert_eq!(s, 200);
    assert!(router.cuts().is_empty(), "{:?}", router.events());
    assert!(router
        .state
        .lock()
        .unwrap()
        .done_sessions
        .contains("conv-synthetic-long"));
}

/// A chat on the other model, started now; it waits while the session hold lasts.
fn waiting_chat(gate: &Arc<GateHandle>) -> tokio::task::JoinHandle<(u16, String)> {
    let gate = Arc::clone(gate);
    tokio::spawn(async move { chat(&gate, SMART, 2, 20, None).await })
}

async fn stop(gate: &GateHandle, conv_id: &str) -> u16 {
    client()
        .delete(gate.url(&format!("/v1/stream?conv_id={conv_id}")))
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

async fn served_soon(b: tokio::task::JoinHandle<(u16, String)>) {
    let (s, _) = tokio::time::timeout(Duration::from_secs(5), b)
        .await
        .expect("served")
        .unwrap();
    assert_eq!(s, 200);
}

async fn lookup_without_running_rows_keeps_the_hold(mode: Lookup) {
    let (router, gate) = setup(
        FakeConfig {
            lookup: mode,
            ..FakeConfig::default()
        },
        |c| c.admission.session_hold_ms = 20_000,
    )
    .await;
    // Runs about 1.2 s; the lookup never lists it while it runs.
    detach(&router, &gate, "conv-synthetic-omit", 30, 40).await;
    let b = waiting_chat(&gate);
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(router.state.lock().unwrap().lookups > 3);
    assert!(!b.is_finished(), "still held while the generation runs");
    // Once it ends the lookup lists it done, which releases the hold.
    served_soon(b).await;
    assert!(router.cuts().is_empty(), "{:?}", router.events());
    assert!(router
        .state
        .lock()
        .unwrap()
        .done_sessions
        .contains("conv-synthetic-omit"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn a_lookup_that_leaves_out_a_running_session_keeps_its_hold_until_it_reports_it_done() {
    lookup_without_running_rows_keeps_the_hold(Lookup::OmitRunning).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn an_empty_lookup_answer_keeps_the_hold_until_the_session_is_reported_done() {
    lookup_without_running_rows_keeps_the_hold(Lookup::EmptyWhileRunning).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn a_failed_lookup_leaves_the_hold() {
    let (router, gate) = setup(
        FakeConfig {
            lookup: Lookup::Error,
            ..FakeConfig::default()
        },
        |c| c.admission.session_hold_ms = 20_000,
    )
    .await;
    detach(&router, &gate, "conv-synthetic-lookup-error", 10, 40).await;
    let b = waiting_chat(&gate);
    // Well past the generation's end: without an answer the hold stays.
    tokio::time::sleep(Duration::from_millis(1_200)).await;
    assert!(router.state.lock().unwrap().lookups > 3);
    assert!(!b.is_finished(), "a failed lookup releases nothing");
    assert_eq!(stop(&gate, "conv-synthetic-lookup-error").await, 204);
    served_soon(b).await;
    assert!(router.cuts().is_empty(), "{:?}", router.events());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn a_resume_answered_with_an_error_leaves_the_hold() {
    for status in [400u16, 404, 503] {
        let (router, gate) = setup(
            FakeConfig {
                resume_status: Some(status),
                ..FakeConfig::default()
            },
            |c| {
                c.admission.session_hold_ms = 20_000;
                c.session_check_interval = Duration::from_secs(3600);
            },
        )
        .await;
        detach(&router, &gate, "conv-synthetic-resume-error", 60, 40).await;
        let resumed = client()
            .get(gate.url("/v1/stream?conv_id=conv-synthetic-resume-error&from=1"))
            .send()
            .await
            .unwrap();
        assert_eq!(resumed.status().as_u16(), status);
        let _ = resumed.text().await;
        let b = waiting_chat(&gate);
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(!b.is_finished(), "a {status} resume ends nothing");
        assert_eq!(stop(&gate, "conv-synthetic-resume-error").await, 204);
        served_soon(b).await;
        assert!(router.cuts().is_empty(), "{status}: {:?}", router.events());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn a_stop_for_one_conversation_leaves_another_s_hold() {
    let (router, gate) = setup(FakeConfig::default(), |c| {
        c.admission.session_hold_ms = 20_000;
        c.session_check_interval = Duration::from_secs(3600);
    })
    .await;
    detach(&router, &gate, "conv-synthetic-a", 200, 40).await;
    detach(&router, &gate, "conv-synthetic-b", 200, 40).await;
    let b = waiting_chat(&gate);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(stop(&gate, "conv-synthetic-a").await, 204);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        !b.is_finished(),
        "the other conversation still holds the model"
    );
    assert_eq!(stop(&gate, "conv-synthetic-b").await, 204);
    served_soon(b).await;
    assert!(router.cuts().is_empty(), "{:?}", router.events());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn a_failed_stop_leaves_the_hold() {
    let (router, gate) = setup(
        FakeConfig {
            stop_status: Some(500),
            ..FakeConfig::default()
        },
        |c| {
            c.admission.session_hold_ms = 20_000;
            c.session_check_interval = Duration::from_secs(3600);
        },
    )
    .await;
    detach(&router, &gate, "conv-synthetic-failed-stop", 200, 40).await;
    let b = waiting_chat(&gate);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(stop(&gate, "conv-synthetic-failed-stop").await, 500);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!b.is_finished(), "a refused stop releases nothing");
    router.configure(|c| c.stop_status = None);
    assert_eq!(stop(&gate, "conv-synthetic-failed-stop").await, 204);
    served_soon(b).await;
    assert!(router.cuts().is_empty(), "{:?}", router.events());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn a_stop_answered_before_the_generation_stops_still_releases_the_hold() {
    // The router accepts the stop at once and cancels afterwards: a switch right after may cut
    // the last moments of the stopped generation, which is accepted.
    let (router, gate) = setup(
        FakeConfig {
            stop_answers_at_once: true,
            ..FakeConfig::default()
        },
        |c| {
            c.admission.session_hold_ms = 20_000;
            c.session_check_interval = Duration::from_secs(3600);
        },
    )
    .await;
    detach(&router, &gate, "conv-synthetic-async-stop", 500, 40).await;
    let b = waiting_chat(&gate);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(stop(&gate, "conv-synthetic-async-stop").await, 204);
    served_soon(b).await;
    assert!(
        eventually(2_000, || router
            .events()
            .iter()
            .any(|e| matches!(e, Event::Stopped(_) | Event::StreamCut(_))))
        .await
    );
}
