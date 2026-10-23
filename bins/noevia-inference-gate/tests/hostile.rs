//! Hostile and malformed input at the gate (sbstndalton/noevia#2115): nothing invalid reaches the
//! router, every limit holds, and every refusal has a stable code.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod support;

use std::time::{Duration, Instant};
use support::*;

const CODE: &str = "route-code";
const SMART: &str = "route-smart";

async fn setup(
    tweak: impl FnOnce(&mut noevia_inference_gate::GateConfig),
) -> (FakeRouter, GateHandle) {
    let router = FakeRouter::start(
        &[(CODE, St::Loaded, false), (SMART, St::Unloaded, false)],
        FakeConfig::default(),
    )
    .await;
    let mut cfg = test_config(router.url());
    tweak(&mut cfg);
    let gate = start_gate(cfg).await;
    (router, gate)
}

fn reached(router: &FakeRouter, path: &str) -> usize {
    router
        .state
        .lock()
        .unwrap()
        .seen_headers
        .iter()
        .filter(|(p, _)| p == path)
        .count()
}

async fn post(
    gate: &GateHandle,
    path: &str,
    body: impl Into<reqwest::Body>,
) -> (u16, serde_json::Value) {
    let r = client()
        .post(gate.url(path))
        .body(body)
        .send()
        .await
        .unwrap();
    let status = r.status().as_u16();
    (status, json_of(r).await)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn invalid_bodies_never_reach_the_router() {
    let (router, gate) = setup(|c| c.max_body_bytes = 1024).await;
    let cases: &[(&str, u16, &str)] = &[
        ("{not json", 400, "invalid_json"),
        (r#"{"model":1}"#, 400, "invalid_model"),
        (r#"{"model":null}"#, 400, "invalid_model"),
        (
            r#"{"model":"route-code","model":"route-smart"}"#,
            400,
            "invalid_json",
        ),
        (r#"{"model":"a\u0007b"}"#, 400, "invalid_model"),
    ];
    for (body, status, code) in cases {
        let (s, v) = post(&gate, "/v1/chat/completions", *body).await;
        assert_eq!(s, *status, "{body}");
        assert_eq!(v["error"]["code"], *code, "{body}");
        assert_eq!(v["error"]["retryable"], false);
    }
    let big = format!(r#"{{"model":"route-code","pad":"{}"}}"#, "x".repeat(2000));
    let (s, v) = post(&gate, "/v1/chat/completions", big).await;
    assert_eq!(s, 413);
    assert_eq!(v["error"]["code"], "body_too_large");
    // Without a content length (chunked), the limit holds as the body arrives.
    let chunks: Vec<Result<bytes::Bytes, std::io::Error>> = (0..20)
        .map(|_| Ok(bytes::Bytes::from(vec![b' '; 200])))
        .collect();
    let r = client()
        .post(gate.url("/v1/chat/completions"))
        .body(reqwest::Body::wrap_stream(futures_util::stream::iter(
            chunks,
        )))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 413);
    assert_eq!(reached(&router, "/v1/chat/completions"), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hostile_paths_are_refused_before_routing() {
    let (router, gate) = setup(|_| {}).await;
    let body = r#"{"model":"route-code"}"#;
    for path in [
        "/models/%75nload",
        "/v1/../models/unload",
        "/models/./unload",
        "//models/unload",
        "/models/unload/",
    ] {
        let request = format!(
            "POST {path} HTTP/1.1\r\nhost: gate\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        let answer = raw(gate.addr, request.as_bytes(), 1000).await;
        assert!(answer.starts_with("HTTP/1.1 400"), "{path}: {answer}");
        assert!(answer.contains("invalid_path"), "{path}");
    }
    assert!(router.events().is_empty(), "{:?}", router.events());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn malformed_and_unknown_leases_are_refused() {
    let (_router, gate) = setup(|_| {}).await;
    for (lease, status, code) in [
        ("short", 400, "invalid_lease"),
        ("0123456789ABCDEF0123456789abcdef", 400, "invalid_lease"),
        ("0123456789abcdef0123456789abcdef", 409, "lease_unknown"),
    ] {
        let r = client()
            .post(gate.url("/v1/chat/completions"))
            .header("x-noevia-lease", lease)
            .body(chat_body(CODE, 1, 10))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), status, "{lease}");
        assert_eq!(json_of(r).await["error"]["code"], code);
    }
    let two = client()
        .post(gate.url("/v1/chat/completions"))
        .header("x-noevia-lease", "0123456789abcdef0123456789abcdef")
        .header("x-noevia-lease", "fedcba9876543210fedcba9876543210")
        .body(chat_body(CODE, 1, 10))
        .send()
        .await
        .unwrap();
    assert_eq!(two.status().as_u16(), 400);
    assert_eq!(release_lease(&gate, "not-a-token").await, 400);
    assert_eq!(
        release_lease(&gate, "0123456789abcdef0123456789abcdef").await,
        404
    );
    for body in [
        r#"{"model":""}"#,
        "{",
        r#"{"model":"route-code","max_wait_ms":-1}"#,
        r#"{"model":"route-code","max_wait_ms":0}"#,
        r#"{"model":"route-code","holder":"x","pad":"#,
    ] {
        let r = client()
            .post(gate.url("/gate/v1/leases"))
            .body(body)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 400, "{body}");
    }
    let big = format!(r#"{{"model":"route-code","pad":"{}"}}"#, "x".repeat(5000));
    let r = client()
        .post(gate.url("/gate/v1/leases"))
        .body(big)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 413);
    let r = client()
        .get(gate.url("/gate/v1/nothing"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 404);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wait_limits_are_validated_and_honoured() {
    let (_router, gate) = setup(|_| {}).await;
    for value in ["-5", "0", "abc", "999999999999"] {
        let r = client()
            .post(gate.url("/v1/chat/completions"))
            .header("x-noevia-max-wait-ms", value)
            .body(chat_body(CODE, 1, 10))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 400, "{value}");
        assert_eq!(json_of(r).await["error"]["code"], "invalid_wait");
    }
    let gate = std::sync::Arc::new(gate);
    let a = {
        let gate = std::sync::Arc::clone(&gate);
        tokio::spawn(async move { chat(&gate, CODE, 20, 50, None).await })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    let started = Instant::now();
    let r = client()
        .post(gate.url("/v1/chat/completions"))
        .header("x-noevia-max-wait-ms", "200")
        .body(chat_body(SMART, 1, 10))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 503);
    assert_eq!(r.headers()["retry-after"], "2");
    let v = json_of(r).await;
    assert_eq!(v["error"]["code"], "inference_queue_timeout");
    assert_eq!(v["error"]["retryable"], true);
    let waited = started.elapsed();
    assert!(
        waited >= Duration::from_millis(180) && waited < Duration::from_millis(1_000),
        "{waited:?}"
    );
    assert_eq!(a.await.unwrap().0, 200);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn the_queue_is_bounded() {
    let (_router, gate) = setup(|c| c.admission.max_waiters = 3).await;
    let gate = std::sync::Arc::new(gate);
    let a = {
        let gate = std::sync::Arc::clone(&gate);
        tokio::spawn(async move { chat(&gate, CODE, 20, 50, None).await })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let gate = std::sync::Arc::clone(&gate);
        tasks.push(tokio::spawn(async move {
            let r = client()
                .post(gate.url("/v1/chat/completions"))
                .body(chat_body(SMART, 1, 10))
                .send()
                .await
                .unwrap();
            let s = r.status().as_u16();
            (
                s,
                json_of(r).await["error"]["code"]
                    .as_str()
                    .unwrap_or("")
                    .to_owned(),
            )
        }));
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let mut full = 0;
    let mut ok = 0;
    for t in tasks {
        match t.await.unwrap() {
            (503, code) if code == "inference_queue_full" => full += 1,
            (200, _) => ok += 1,
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(full, 5);
    assert_eq!(ok, 3);
    assert_eq!(a.await.unwrap().0, 200);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_slow_body_times_out_and_the_buffer_is_bounded() {
    let (router, gate) = setup(|c| {
        c.max_body_bytes = 4 * 1024;
        c.max_buffered_bytes = 4 * 1024;
    })
    .await;
    let request = "POST /v1/chat/completions HTTP/1.1\r\nhost: gate\r\ncontent-length: 100\r\n\r\n{\"model\":";
    let started = Instant::now();
    let answer = raw(gate.addr, request.as_bytes(), 3_000).await;
    assert!(answer.starts_with("HTTP/1.1 408"), "{answer}");
    assert!(answer.contains("body_timeout"));
    assert!(started.elapsed() < Duration::from_secs(2));
    // A slow upload holding the whole buffer makes the next one wait, then refuse: the gate
    // never buffers more than its limit.
    let gate = std::sync::Arc::new(gate);
    let holder = {
        let addr = gate.addr;
        tokio::spawn(async move {
            let pad = "x".repeat(3_900);
            let head = format!("POST /v1/chat/completions HTTP/1.1\r\nhost: gate\r\ncontent-length: {}\r\n\r\n{pad}", pad.len() + 50);
            raw(addr, head.as_bytes(), 1_000).await
        })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    let s = status(&gate).await;
    assert!(s["buffer_free_kib"].as_u64().unwrap() <= 1, "{s}");
    let _ = holder.await;
    assert_eq!(reached(&router, "/v1/chat/completions"), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unloads_without_a_model_and_maintenance_while_busy_are_refused() {
    let (router, gate) = setup(|_| {}).await;
    let (s, v) = post(&gate, "/models/unload", "{}").await;
    assert_eq!(s, 400);
    assert_eq!(v["error"]["code"], "invalid_model");
    let r = client().delete(gate.url("/models")).send().await.unwrap();
    assert_eq!(r.status().as_u16(), 400);
    let gate = std::sync::Arc::new(gate);
    let a = {
        let gate = std::sync::Arc::clone(&gate);
        tokio::spawn(async move { chat(&gate, CODE, 10, 50, None).await })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    let r = client()
        .get(gate.url("/models?reload=1"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 409);
    assert_eq!(json_of(r).await["error"]["code"], "engine_busy");
    let r = client()
        .delete(gate.url("/models?model=route-code"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 409);
    let r = client()
        .get(gate.url("/models?reload=1"))
        .header("x-noevia-gate-admin", ADMIN)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    assert_eq!(a.await.unwrap().0, 200);
    assert!(router.events().contains(&Event::Reload));
    assert!(router.cuts().is_empty());
}
