//! The gate in front of the synthetic router (sbstndalton/noevia#2115, #2117): concurrency,
//! leases, failures, fairness, sleeping and exempt models.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod support;

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use support::*;

const CODE: &str = "route-code";
const SMART: &str = "route-smart";
const EMBED: &str = "embed";

async fn setup(models: &[(&str, St, bool)], fake: FakeConfig) -> (FakeRouter, GateHandle) {
    let router = FakeRouter::start(models, fake).await;
    let gate = start_gate(test_config(router.url())).await;
    (router, gate)
}

fn done(body: &str) -> bool {
    body.contains("[DONE]")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_routes_at_once_the_second_waits_and_nothing_is_cut() {
    let (router, gate) = setup(
        &[(CODE, St::Loaded, false), (SMART, St::Unloaded, false)],
        FakeConfig::default(),
    )
    .await;
    let gate = std::sync::Arc::new(gate);
    let a = {
        let gate = std::sync::Arc::clone(&gate);
        tokio::spawn(async move {
            let r = chat(&gate, CODE, 10, 50, None).await;
            (r, Instant::now())
        })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    let ((b_status, b_body), b_end) = {
        let r = chat(&gate, SMART, 2, 20, None).await;
        (r, Instant::now())
    };
    let ((a_status, a_body), a_end) = a.await.unwrap();
    assert_eq!(a_status, 200);
    assert!(done(&a_body), "the first chat completed");
    assert_eq!(b_status, 200, "{b_body}");
    assert!(done(&b_body));
    assert!(
        b_end > a_end,
        "the second route ran after the first finished"
    );
    assert!(
        router.cuts().is_empty(),
        "no stream cut: {:?}",
        router.events()
    );
    assert_eq!(router.loads_of(SMART), 1);
    let s = status(&gate).await;
    assert_eq!(s["scheduler"]["counters"]["switches"], 1);
    assert_eq!(s["scheduler"]["holder"], SMART);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unload_of_a_busy_model_is_refused_and_only_an_admin_force_cuts_it() {
    let (router, gate) = setup(&[(CODE, St::Loaded, false)], FakeConfig::default()).await;
    let gate = std::sync::Arc::new(gate);
    let a = {
        let gate = std::sync::Arc::clone(&gate);
        tokio::spawn(async move { chat(&gate, CODE, 20, 50, None).await })
    };
    tokio::time::sleep(Duration::from_millis(150)).await;
    let unload = |body: &'static str, admin: bool| {
        let mut req = client().post(gate.url("/models/unload")).body(body);
        if admin {
            req = req.header("x-noevia-gate-admin", ADMIN);
        }
        req
    };
    let r = unload(r#"{"model":"route-code"}"#, false)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 409);
    assert_eq!(json_of(r).await["error"]["code"], "model_in_use");
    let r = unload(r#"{"model":"route-code","force":true}"#, false)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 403);
    let wrong = client()
        .post(gate.url("/models/unload"))
        .header("x-noevia-gate-admin", "not-the-admin-token-000")
        .body(r#"{"model":"route-code","force":true}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(wrong.status().as_u16(), 403);
    assert!(
        router.cuts().is_empty(),
        "nothing cut without the admin force"
    );
    let r = unload(r#"{"model":"route-code","force":true}"#, true)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    let (a_status, a_body) = a.await.unwrap();
    assert!(
        a_status != 200 || !done(&a_body),
        "the forced unload ended the stream"
    );
    assert!(router.events().contains(&Event::Unload {
        model: CODE.into(),
        busy: true
    }));
    let metrics = client()
        .get(gate.url("/gate/v1/metrics"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        metrics.contains("noevia_gate_unloads_forced_total 1"),
        "{metrics}"
    );
    assert!(
        metrics.contains("noevia_gate_unloads_refused_total 1"),
        "{metrics}"
    );
    let gate2 = std::sync::Arc::clone(&gate);
    let mut drained = false;
    for _ in 0..100 {
        let s = status(&gate2).await;
        if s["scheduler"]["in_flight"]
            .as_object()
            .is_some_and(|m| m.is_empty())
        {
            drained = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(drained, "the cut request no longer counts");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lease_holds_the_model_across_tool_rounds_and_its_release_lets_the_next_in() {
    let (router, gate) = setup(
        &[(CODE, St::Loaded, false), (SMART, St::Unloaded, false)],
        FakeConfig::default(),
    )
    .await;
    let gate = std::sync::Arc::new(gate);
    let lease = acquire_lease(&gate, CODE).await.unwrap();
    let (s1, b1) = chat(&gate, CODE, 3, 30, Some(&lease)).await;
    assert_eq!(s1, 200, "{b1}");
    let b = {
        let gate = std::sync::Arc::clone(&gate);
        tokio::spawn(async move { chat(&gate, SMART, 2, 20, None).await })
    };
    for _ in 0..2 {
        // A tool runs between rounds: nothing in flight, the lease holds the model.
        tokio::time::sleep(Duration::from_millis(150)).await;
        let (s, body) = chat(&gate, CODE, 3, 30, Some(&lease)).await;
        assert_eq!(s, 200, "{body}");
        assert!(!b.is_finished(), "the other route waits for the turn");
    }
    assert!(!router
        .events()
        .iter()
        .any(|e| matches!(e, Event::Request { model, .. } if model == SMART)));
    let released_at = Instant::now();
    assert_eq!(release_lease(&gate, &lease).await, 204);
    let (bs, bb) = b.await.unwrap();
    assert_eq!(bs, 200, "{bb}");
    assert!(
        released_at.elapsed() < Duration::from_secs(2),
        "served promptly after the release"
    );
    assert_eq!(
        release_lease(&gate, &lease).await,
        404,
        "a second release finds nothing"
    );
    assert!(router.cuts().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lease_ends_when_idle_and_its_requests_are_then_refused() {
    let (_router, gate) = setup(&[(CODE, St::Loaded, false)], FakeConfig::default()).await;
    let lease = acquire_lease(&gate, CODE).await.unwrap();
    tokio::time::sleep(Duration::from_millis(700)).await;
    let r = client()
        .post(gate.url("/v1/chat/completions"))
        .header("x-noevia-lease", &lease)
        .body(chat_body(CODE, 1, 10))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 409);
    assert_eq!(json_of(r).await["error"]["code"], "lease_unknown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_queued_lease_reports_its_position_then_is_granted() {
    let (_router, gate) = setup(
        &[(CODE, St::Loaded, false), (SMART, St::Unloaded, false)],
        FakeConfig::default(),
    )
    .await;
    let gate = std::sync::Arc::new(gate);
    let a = {
        let gate = std::sync::Arc::clone(&gate);
        tokio::spawn(async move { chat(&gate, CODE, 10, 40, None).await })
    };
    tokio::time::sleep(Duration::from_millis(60)).await;
    let resp = client()
        .post(gate.url("/gate/v1/leases"))
        .body(r#"{"model":"route-smart","holder":"web"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.headers()["content-type"], "text/event-stream");
    let events = read_events(resp, |e| e == "granted" || e == "error").await;
    assert_eq!(events[0].0, "queued");
    assert_eq!(events[0].1["position"], 1);
    assert!(
        events.iter().filter(|(e, _)| e == "queued").count() >= 2,
        "{events:?}"
    );
    let (last, data) = events.last().unwrap();
    assert_eq!(last, "granted");
    assert_eq!(data["lease"].as_str().unwrap().len(), 32);
    assert!(data["waited_ms"].as_u64().unwrap() > 0);
    assert_eq!(a.await.unwrap().0, 200);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stuck_unload_loads_nothing_else_and_recovers() {
    let (router, gate) = setup(
        &[(CODE, St::Loaded, false), (SMART, St::Unloaded, false)],
        FakeConfig::default(),
    )
    .await;
    router.set_stuck(true);
    let started = Instant::now();
    let r = client()
        .post(gate.url("/v1/chat/completions"))
        .body(chat_body(SMART, 1, 10))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 503);
    assert_eq!(json_of(r).await["error"]["code"], "engine_switch_stuck");
    assert!(started.elapsed() < Duration::from_secs(3));
    assert_eq!(
        router.loads_of(SMART),
        0,
        "nothing loaded beside the stuck model"
    );
    let r = client()
        .post(gate.url("/v1/chat/completions"))
        .body(chat_body(SMART, 1, 10))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 503, "refused at once while stuck");
    router.set_stuck(false);
    let mut ok = false;
    for _ in 0..50 {
        let (s, _) = chat(&gate, SMART, 1, 10, None).await;
        if s == 200 {
            ok = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(ok, "served once the model is gone");
    assert!(router.cuts().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unreachable_router_fails_fast_drops_waiters_and_recovers() {
    let (router, gate) = setup(
        &[(CODE, St::Loaded, false), (SMART, St::Unloaded, false)],
        FakeConfig::default(),
    )
    .await;
    let gate = std::sync::Arc::new(gate);
    let a = {
        let gate = std::sync::Arc::clone(&gate);
        tokio::spawn(async move { chat(&gate, CODE, 30, 50, None).await })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    let b = {
        let gate = std::sync::Arc::clone(&gate);
        tokio::spawn(async move {
            let r = client()
                .post(gate.url("/v1/chat/completions"))
                .body(chat_body(SMART, 1, 10))
                .send()
                .await
                .unwrap();
            let code = r.status().as_u16();
            (code, json_of(r).await)
        })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    router.down.store(true, Ordering::SeqCst);
    let started = Instant::now();
    let (code, body) = b.await.unwrap();
    assert_eq!(code, 502, "{body}");
    assert_eq!(body["error"]["code"], "engine_unavailable");
    assert!(started.elapsed() < Duration::from_secs(3));
    let r = client()
        .post(gate.url("/v1/chat/completions"))
        .body(chat_body(CODE, 1, 10))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 502);
    router.down.store(false, Ordering::SeqCst);
    let _ = a.await;
    let mut ok = false;
    for _ in 0..60 {
        let (s, _) = chat(&gate, CODE, 1, 10, None).await;
        if s == 200 {
            ok = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(ok, "recovers without restarting the gate");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_client_that_leaves_while_waiting_leaves_the_queue_and_loads_nothing() {
    let (router, gate) = setup(
        &[(CODE, St::Loaded, false), (SMART, St::Unloaded, false)],
        FakeConfig::default(),
    )
    .await;
    let gate = std::sync::Arc::new(gate);
    let a = {
        let gate = std::sync::Arc::clone(&gate);
        tokio::spawn(async move { chat(&gate, CODE, 16, 50, None).await })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    let body = chat_body(SMART, 1, 10);
    let request = format!(
        "POST /v1/chat/completions HTTP/1.1\r\nhost: gate\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
        body.len(),
        body
    );
    // Waits 200 ms (queued), then the socket closes.
    let answer = raw(gate.addr, request.as_bytes(), 200).await;
    assert!(answer.is_empty(), "no answer while queued: {answer}");
    let mut left = false;
    for _ in 0..50 {
        let s = status(&gate).await;
        if s["scheduler"]["queue"]
            .as_array()
            .is_some_and(|q| q.is_empty())
        {
            left = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(left, "the waiter left the queue");
    assert_eq!(a.await.unwrap().0, 200);
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(router.loads_of(SMART), 0, "nobody waits for it: not loaded");
    assert_eq!(status(&gate).await["scheduler"]["holder"], CODE);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_client_that_leaves_mid_stream_stops_counting() {
    use futures_util::StreamExt;
    let (_router, gate) = setup(&[(CODE, St::Loaded, false)], FakeConfig::default()).await;
    let resp = client()
        .post(gate.url("/v1/chat/completions"))
        .body(chat_body(CODE, 40, 50))
        .send()
        .await
        .unwrap();
    let mut stream = resp.bytes_stream();
    let first = stream.next().await.unwrap().unwrap();
    assert!(!first.is_empty());
    assert_eq!(status(&gate).await["scheduler"]["in_flight"][CODE], 1);
    drop(stream);
    let mut cleared = false;
    for _ in 0..50 {
        if status(&gate).await["scheduler"]["in_flight"]
            .as_object()
            .is_some_and(|m| m.is_empty())
        {
            cleared = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(cleared);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn under_continuous_traffic_the_other_route_is_served_within_the_fair_hold() {
    let (router, gate) = setup(
        &[(CODE, St::Loaded, false), (SMART, St::Unloaded, false)],
        FakeConfig::default(),
    )
    .await;
    let gate = std::sync::Arc::new(gate);
    let stop_at = Instant::now() + Duration::from_millis(2_500);
    let mut workers = Vec::new();
    for _ in 0..2 {
        let gate = std::sync::Arc::clone(&gate);
        workers.push(tokio::spawn(async move {
            let mut statuses = Vec::new();
            while Instant::now() < stop_at {
                statuses.push(chat(&gate, CODE, 3, 30, None).await.0);
            }
            statuses
        }));
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    let asked = Instant::now();
    let (bs, bb) = chat(&gate, SMART, 2, 20, None).await;
    let waited = asked.elapsed();
    assert_eq!(bs, 200, "{bb}");
    // Fair hold 400 ms + the requests already running (~100 ms) + grace 100 ms + load and stop.
    assert!(
        waited < Duration::from_millis(1_500),
        "served after {waited:?}"
    );
    for w in workers {
        let statuses = w.await.unwrap();
        assert!(statuses.iter().all(|s| *s == 200), "{statuses:?}");
    }
    assert!(router.cuts().is_empty(), "{:?}", router.events());
    let switches = status(&gate).await["scheduler"]["counters"]["switches"]
        .as_u64()
        .unwrap();
    assert!(switches <= 2, "one switch there and one back: {switches}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_sleeping_model_is_woken_by_its_request_never_loaded_by_the_gate() {
    let (router, gate) = setup(
        &[(SMART, St::Sleeping, false), (CODE, St::Unloaded, false)],
        FakeConfig::default(),
    )
    .await;
    let (s, body) = chat(&gate, SMART, 1, 10, None).await;
    assert_eq!(s, 200, "{body}");
    let events = router.events();
    assert!(events.contains(&Event::Wake(SMART.into())));
    assert_eq!(router.loads_of(SMART), 0);
    // Switching away from it after it sleeps again: unloaded, then the other model is served.
    router.set_status(SMART, St::Sleeping);
    tokio::time::sleep(Duration::from_millis(250)).await;
    let (s, body) = chat(&gate, CODE, 1, 10, None).await;
    assert_eq!(s, 200, "{body}");
    assert!(router.cuts().is_empty());
    assert!(
        !router.events().iter().any(|e| matches!(e, Event::Load(_))),
        "the gate never sends a load"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn exempt_models_are_served_beside_the_chat_model_and_never_block_or_switch_it() {
    let fake = FakeConfig {
        models_max: 2,
        ..FakeConfig::default()
    };
    let (router, gate) = setup(
        &[
            (CODE, St::Loaded, false),
            (EMBED, St::Unloaded, true),
            (SMART, St::Unloaded, false),
        ],
        fake,
    )
    .await;
    let gate = std::sync::Arc::new(gate);
    let a = {
        let gate = std::sync::Arc::clone(&gate);
        tokio::spawn(async move { chat(&gate, CODE, 12, 50, None).await })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    let b = {
        let gate = std::sync::Arc::clone(&gate);
        tokio::spawn(async move { chat(&gate, SMART, 1, 10, None).await })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    let embed = r#"{"model":"embed","input":"synthetic"}"#;
    let started = Instant::now();
    let r = client()
        .post(gate.url("/v1/embeddings"))
        .body(embed)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    assert!(
        started.elapsed() < Duration::from_millis(400),
        "not queued behind the chat"
    );
    assert!(!a.is_finished(), "served while the chat model streams");
    assert_eq!(a.await.unwrap().0, 200);
    assert_eq!(b.await.unwrap().0, 200);
    assert!(router.cuts().is_empty());
    assert!(!router.events().contains(&Event::Unload {
        model: EMBED.into(),
        busy: false
    }));
}
