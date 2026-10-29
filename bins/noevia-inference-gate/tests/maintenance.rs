//! The maintenance hold (sbstndalton/noevia#2187): the gate is the one authority for the exclusion
//! a preset write, a calibration, an auto-tune or a throughput sweep needs. Admin only; refused
//! while work runs (optionally after a bounded wait); work without it waits for its end; it is
//! released by its token or expires when its holder stops renewing it. Synthetic models only.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod support;

use std::sync::Arc;
use std::time::{Duration, Instant};
use support::*;

const CODE: &str = "route-code";
const SMART: &str = "route-smart";

async fn setup() -> (FakeRouter, Arc<GateHandle>) {
    let router = FakeRouter::start(
        &[(CODE, St::Loaded, false), (SMART, St::Unloaded, false)],
        FakeConfig::default(),
    )
    .await;
    let gate = start_gate(test_config(router.url())).await;
    (router, Arc::new(gate))
}

/// `POST /gate/v1/maintenance` with `body`: (status, JSON answer).
async fn hold(
    gate: &GateHandle,
    admin: Option<&str>,
    body: serde_json::Value,
) -> (u16, serde_json::Value) {
    let mut req = client()
        .post(gate.url("/gate/v1/maintenance"))
        .body(body.to_string());
    if let Some(token) = admin {
        req = req.header("x-noevia-gate-admin", token);
    }
    let resp = req.send().await.unwrap();
    let status = resp.status().as_u16();
    (status, json_of(resp).await)
}

async fn take(gate: &GateHandle, reason: &str) -> String {
    let (status, body) = hold(gate, Some(ADMIN), serde_json::json!({ "reason": reason })).await;
    assert_eq!(status, 201, "{body}");
    body["hold"].as_str().unwrap().to_owned()
}

async fn release(gate: &GateHandle, admin: Option<&str>, token: &str) -> u16 {
    let mut req = client().delete(gate.url(&format!("/gate/v1/maintenance/{token}")));
    if let Some(admin) = admin {
        req = req.header("x-noevia-gate-admin", admin);
    }
    req.send().await.unwrap().status().as_u16()
}

async fn renew(gate: &GateHandle, token: &str, reason: Option<&str>) -> u16 {
    client()
        .post(gate.url(&format!("/gate/v1/maintenance/{token}/renew")))
        .header("x-noevia-gate-admin", ADMIN)
        .body(serde_json::json!({ "reason": reason }).to_string())
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_hold_api_needs_the_admin_token() {
    let (_router, gate) = setup().await;
    let (status, body) = hold(&gate, None, serde_json::json!({})).await;
    assert_eq!(status, 403);
    assert_eq!(body["error"]["code"], "admin_required");
    let (status, _) = hold(&gate, Some("wrong-admin-token-000"), serde_json::json!({})).await;
    assert_eq!(status, 403);
    assert!(!status_of_held(&gate).await);
    let token = take(&gate, "Tuning").await;
    assert_eq!(release(&gate, None, &token).await, 403);
    let renewed = client()
        .post(gate.url(&format!("/gate/v1/maintenance/{token}/renew")))
        .send()
        .await
        .unwrap()
        .status()
        .as_u16();
    assert_eq!(renewed, 403);
    assert!(
        status_of_held(&gate).await,
        "a refused release changes nothing"
    );
    assert_eq!(release(&gate, Some(ADMIN), &token).await, 204);
    assert_eq!(
        release(&gate, Some(ADMIN), &token).await,
        404,
        "released once"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_gate_without_an_admin_token_never_grants_a_hold() {
    let router = FakeRouter::start(&[(CODE, St::Loaded, false)], FakeConfig::default()).await;
    let mut cfg = test_config(router.url());
    cfg.admin_token = None;
    let gate = start_gate(cfg).await;
    let (status, _) = hold(&gate, Some(ADMIN), serde_json::json!({})).await;
    assert_eq!(status, 403);
}

async fn status_of_held(gate: &GateHandle) -> bool {
    status(gate).await["maintenance"]["held"].as_bool().unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_hold_is_refused_while_a_lease_is_live_and_granted_once_it_is_released() {
    let (_router, gate) = setup().await;
    let lease = acquire_lease(&gate, CODE).await.unwrap();
    let (status, body) = hold(
        &gate,
        Some(ADMIN),
        serde_json::json!({ "reason": "Applying" }),
    )
    .await;
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["error"]["code"], "engine_busy");
    assert!(!status_of_held(&gate).await);
    assert_eq!(release_lease(&gate, &lease).await, 204);
    let token = take(&gate, "Applying").await;
    assert_eq!(token.len(), 32);
    // Only one hold at a time.
    let (status, body) = hold(&gate, Some(ADMIN), serde_json::json!({})).await;
    assert_eq!(status, 503);
    assert_eq!(body["error"]["code"], "maintenance_held");
    assert_eq!(release(&gate, Some(ADMIN), &token).await, 204);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn a_hold_waits_for_a_running_request_when_asked_to_and_times_out_otherwise() {
    let (_router, gate) = setup().await;
    // A generation of about 600 ms.
    let running = {
        let gate = Arc::clone(&gate);
        tokio::spawn(async move { chat(&gate, CODE, 12, 50, None).await })
    };
    tokio::time::sleep(Duration::from_millis(150)).await;
    let (status, _) = hold(&gate, Some(ADMIN), serde_json::json!({})).await;
    assert_eq!(status, 409, "no wait asked: refused at once");
    let started = Instant::now();
    let (status, body) = hold(&gate, Some(ADMIN), serde_json::json!({ "wait_ms": 100 })).await;
    assert_eq!(status, 409, "{body}");
    assert!(
        started.elapsed() >= Duration::from_millis(100),
        "waited its limit"
    );
    let (status, body) = hold(&gate, Some(ADMIN), serde_json::json!({ "wait_ms": 5000 })).await;
    assert_eq!(status, 201, "{body}");
    let (code, text) = running.await.unwrap();
    assert_eq!(code, 200);
    assert!(text.contains("[DONE]"), "the running answer was not cut");
    let token = body["hold"].as_str().unwrap();
    assert_eq!(release(&gate, Some(ADMIN), token).await, 204);
    let (status, _) = hold(
        &gate,
        Some(ADMIN),
        serde_json::json!({ "wait_ms": 600_001 }),
    )
    .await;
    assert_eq!(status, 400, "an unbounded wait is refused");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn a_lease_waits_while_the_hold_is_held_and_is_granted_when_it_is_released() {
    let (_router, gate) = setup().await;
    let token = take(&gate, "Chat is paused while noevia calibrates route-code.").await;
    let st = status(&gate).await;
    assert_eq!(st["maintenance"]["held"], true);
    assert_eq!(
        st["maintenance"]["reason"],
        "Chat is paused while noevia calibrates route-code."
    );
    let waiting = {
        let gate = Arc::clone(&gate);
        tokio::spawn(async move { acquire_lease(&gate, CODE).await })
    };
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(!waiting.is_finished(), "no lease while the hold is held");
    assert_eq!(status(&gate).await["scheduler"]["leases"], 0);
    assert_eq!(release(&gate, Some(ADMIN), &token).await, 204);
    let lease = tokio::time::timeout(Duration::from_secs(3), waiting)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(release_lease(&gate, &lease).await, 204);
    assert!(!status_of_held(&gate).await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lease_that_outwaits_its_limit_is_refused_maintenance_held() {
    let (_router, gate) = setup().await;
    let token = take(&gate, "Tuning").await;
    let resp = client()
        .post(gate.url("/gate/v1/leases"))
        .body(serde_json::json!({ "model": CODE, "max_wait_ms": 300 }).to_string())
        .send()
        .await
        .unwrap();
    let events = read_events(resp, |e| e == "granted" || e == "error").await;
    assert_eq!(events[0].0, "queued");
    let (name, data) = events.last().unwrap();
    assert_eq!(name, "error");
    assert_eq!(data["code"], "maintenance_held");
    assert_eq!(data["retryable"], true);
    assert_eq!(release(&gate, Some(ADMIN), &token).await, 204);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn requests_without_the_hold_wait_and_the_holders_own_run_at_once() {
    let (router, gate) = setup().await;
    let token = take(&gate, "Calibrating").await;
    // The holder's request runs now.
    let resp = client()
        .post(gate.url("/v1/chat/completions"))
        .header("x-noevia-maintenance", &token)
        .body(chat_body(CODE, 2, 10))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert!(resp.text().await.unwrap().contains("[DONE]"));
    // Another client's request waits for the hold's end.
    let other = {
        let gate = Arc::clone(&gate);
        tokio::spawn(async move { chat(&gate, CODE, 2, 10, None).await })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!other.is_finished());
    let before = router
        .events()
        .iter()
        .filter(|e| matches!(e, Event::Request { .. }))
        .count();
    assert_eq!(release(&gate, Some(ADMIN), &token).await, 204);
    let (code, _) = tokio::time::timeout(Duration::from_secs(3), other)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(code, 200);
    let after = router
        .events()
        .iter()
        .filter(|e| matches!(e, Event::Request { .. }))
        .count();
    assert_eq!(
        after,
        before + 1,
        "it reached the router only after the release"
    );
    // The header is the gate's: never forwarded, and a stale or malformed one is refused.
    let stale = client()
        .post(gate.url("/v1/chat/completions"))
        .header("x-noevia-maintenance", &token)
        .body(chat_body(CODE, 1, 1))
        .send()
        .await
        .unwrap();
    assert_eq!(stale.status(), 409);
    assert_eq!(
        json_of(stale).await["error"]["code"],
        "maintenance_hold_unknown"
    );
    let bad = client()
        .post(gate.url("/v1/chat/completions"))
        .header("x-noevia-maintenance", "not-a-token")
        .body(chat_body(CODE, 1, 1))
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 400);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_request_that_outwaits_the_hold_is_refused_with_a_retryable_503() {
    let (_router, gate) = setup().await;
    let token = take(&gate, "Tuning").await;
    let resp = client()
        .post(gate.url("/v1/chat/completions"))
        .header("x-noevia-max-wait-ms", "250")
        .body(chat_body(CODE, 1, 1))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 503);
    assert_eq!(resp.headers()["x-noevia-gate-refusal"], "maintenance_held");
    assert_eq!(release(&gate, Some(ADMIN), &token).await, 204);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_preset_reload_needs_the_hold_while_one_is_held() {
    let (_router, gate) = setup().await;
    let token = take(&gate, "Applying").await;
    let refused = client()
        .get(gate.url("/models?reload=1"))
        .header("x-noevia-gate-admin", ADMIN)
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status(), 409, "even an admin force");
    let own = client()
        .get(gate.url("/models?reload=1"))
        .header("x-noevia-maintenance", &token)
        .send()
        .await
        .unwrap();
    assert_eq!(own.status(), 200);
    assert_eq!(release(&gate, Some(ADMIN), &token).await, 204);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn a_hold_whose_holder_stops_renewing_expires_and_new_work_runs() {
    let (_router, gate) = setup().await;
    let (code, body) = hold(
        &gate,
        Some(ADMIN),
        serde_json::json!({ "reason": "Tuning", "idle_ttl_ms": 1000 }),
    )
    .await;
    assert_eq!(code, 201);
    assert_eq!(body["idle_ttl_ms"], 1000);
    let token = body["hold"].as_str().unwrap().to_owned();
    // Renewed (with a new reason) it outlives its idle limit.
    for _ in 0..3 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(renew(&gate, &token, Some("Tuning: phase 2")).await, 200);
    }
    let st = status(&gate).await;
    assert_eq!(st["maintenance"]["held"], true);
    assert_eq!(st["maintenance"]["reason"], "Tuning: phase 2");
    assert!(st["maintenance"]["held_ms"].as_u64().unwrap() >= 1400);
    // A lease waits for it; once the holder stops renewing, the hold ends on its own.
    let waiting = {
        let gate = Arc::clone(&gate);
        tokio::spawn(async move { acquire_lease(&gate, CODE).await })
    };
    let lease = tokio::time::timeout(Duration::from_secs(4), waiting)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(!status_of_held(&gate).await);
    assert_eq!(
        renew(&gate, &token, None).await,
        404,
        "an expired hold is gone"
    );
    assert_eq!(release(&gate, Some(ADMIN), &token).await, 404);
    assert_eq!(release_lease(&gate, &lease).await, 204);
    let (code, _) = hold(&gate, Some(ADMIN), serde_json::json!({ "idle_ttl_ms": 10 })).await;
    assert_eq!(code, 400, "an idle limit below 1 s is refused");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn exempt_work_and_reads_that_load_nothing_run_while_a_hold_is_held() {
    let router = FakeRouter::start(
        &[(CODE, St::Loaded, false), ("embed-small", St::Loaded, true)],
        FakeConfig::default(),
    )
    .await;
    let gate = start_gate(test_config(router.url())).await;
    // The listing has been read (the resident chat model is the holder).
    tokio::time::sleep(Duration::from_millis(200)).await;
    let token = take(&gate, "Calibrating").await;
    // A small (exempt) model never takes the chat slot: it runs as under core's in-process hold.
    let started = Instant::now();
    let (code, _) = chat(&gate, "embed-small", 1, 1, None).await;
    assert_eq!(code, 200);
    assert!(
        started.elapsed() < Duration::from_millis(1500),
        "it did not wait for the hold"
    );
    // A read that loads nothing is answered too.
    let read = client()
        .get(gate.url(&format!("/slots?model={CODE}&autoload=false")))
        .send()
        .await
        .unwrap();
    assert_eq!(read.status(), 200);
    assert_eq!(release(&gate, Some(ADMIN), &token).await, 204);
}

/// A chat body of about `kib` KiB (the padding is a field the fake router ignores).
fn padded_body(model: &str, kib: usize) -> String {
    let mut v: serde_json::Value = serde_json::from_str(&chat_body(model, 1, 1)).unwrap();
    v["padding"] = "x".repeat(kib * 1024).into();
    v.to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn requests_waiting_out_a_hold_cannot_take_the_buffer_the_holders_requests_need() {
    let router = FakeRouter::start(&[(CODE, St::Loaded, false)], FakeConfig::default()).await;
    let mut cfg = test_config(router.url());
    // 64 KiB of body buffer: waiting requests may hold at most half of it.
    cfg.max_buffered_bytes = 64 * 1024;
    let gate = Arc::new(start_gate(cfg).await);
    let token = take(&gate, "Calibrating").await;
    let waiting = {
        let gate = Arc::clone(&gate);
        tokio::spawn(async move {
            client()
                .post(gate.url("/v1/chat/completions"))
                .body(padded_body(CODE, 20))
                .send()
                .await
                .unwrap()
                .status()
                .as_u16()
        })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!waiting.is_finished(), "the first one waits for the hold");
    // A second one would take more than the waiting share: refused at once.
    let second = client()
        .post(gate.url("/v1/chat/completions"))
        .body(padded_body(CODE, 20))
        .send()
        .await
        .unwrap();
    assert_eq!(second.status(), 503);
    assert_eq!(second.headers()["x-noevia-gate-refusal"], "gate_busy");
    // The holder's own large request still fits and runs now.
    let own = client()
        .post(gate.url("/v1/chat/completions"))
        .header("x-noevia-maintenance", &token)
        .body(padded_body(CODE, 20))
        .send()
        .await
        .unwrap();
    assert_eq!(own.status(), 200);
    assert_eq!(release(&gate, Some(ADMIN), &token).await, 204);
    assert_eq!(waiting.await.unwrap(), 200);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn requests_and_leases_waiting_out_a_hold_count_against_the_waiter_limit() {
    let router = FakeRouter::start(&[(CODE, St::Loaded, false)], FakeConfig::default()).await;
    let mut cfg = test_config(router.url());
    cfg.admission.max_waiters = 2;
    let gate = Arc::new(start_gate(cfg).await);
    let token = take(&gate, "Calibrating").await;
    let a = {
        let gate = Arc::clone(&gate);
        tokio::spawn(async move { chat(&gate, CODE, 1, 1, None).await.0 })
    };
    let b = {
        let gate = Arc::clone(&gate);
        tokio::spawn(async move { acquire_lease(&gate, CODE).await })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    let (code, _) = chat(&gate, CODE, 1, 1, None).await;
    assert_eq!(code, 503, "a third waiter is refused gate_busy");
    let refused = client()
        .post(gate.url("/gate/v1/leases"))
        .body(serde_json::json!({ "model": CODE }).to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status(), 503);
    assert_eq!(json_of(refused).await["error"]["code"], "gate_busy");
    // The holder is not a waiter.
    let resp = client()
        .post(gate.url("/v1/chat/completions"))
        .header("x-noevia-maintenance", &token)
        .body(chat_body(CODE, 1, 1))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(release(&gate, Some(ADMIN), &token).await, 204);
    assert_eq!(a.await.unwrap(), 200);
    let lease = b.await.unwrap().unwrap();
    assert_eq!(release_lease(&gate, &lease).await, 204);
}
