//! Edge paths through the gate (sbstndalton/noevia#2115): aliases, session generations and stream
//! resumes, preset reloads, unloads whose client leaves or that the router refuses, unknown
//! models, requests that cannot load, a client without a lease making several calls, and the
//! connection limits. Synthetic models only.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod support;

use std::sync::Arc;
use std::time::Duration;
use support::*;

const CODE: &str = "route-code";
const SMART: &str = "route-smart";

async fn setup(
    models: &[(&str, St, bool)],
    fake: FakeConfig,
    tweak: impl FnOnce(&mut noevia_inference_gate::GateConfig),
) -> (FakeRouter, Arc<GateHandle>) {
    let router = FakeRouter::start(models, fake).await;
    let mut cfg = test_config(router.url());
    tweak(&mut cfg);
    let gate = start_gate(cfg).await;
    (router, Arc::new(gate))
}

async fn unload(gate: &GateHandle, model: &str, admin: bool) -> (u16, serde_json::Value) {
    let mut req = client()
        .post(gate.url("/models/unload"))
        .body(serde_json::json!({ "model": model }).to_string());
    if admin {
        req = req.header("x-noevia-gate-admin", ADMIN);
    }
    let r = req.send().await.unwrap();
    let s = r.status().as_u16();
    (s, json_of(r).await)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stream_named_by_an_alias_is_its_model_s_own_work() {
    let (router, gate) = setup(
        &[(CODE, St::Loaded, false), (SMART, St::Unloaded, false)],
        FakeConfig::default(),
        |_| {},
    )
    .await;
    router.set_aliases(CODE, &["code-alias"]);
    // Let the gate read the aliases.
    tokio::time::sleep(Duration::from_millis(250)).await;
    let a = {
        let gate = Arc::clone(&gate);
        tokio::spawn(async move { chat(&gate, "code-alias", 12, 50, None).await })
    };
    assert!(
        eventually(1_000, || router
            .events()
            .iter()
            .any(|e| matches!(e, Event::Request { .. })))
        .await
    );
    // Neither name unloads it without the admin while it streams.
    for name in [CODE, "code-alias"] {
        let (s, v) = unload(&gate, name, false).await;
        assert_eq!(s, 409, "{name}");
        assert_eq!(v["error"]["code"], "model_in_use");
    }
    // Another model waits; more work under the alias is not queued behind its own model.
    let b = {
        let gate = Arc::clone(&gate);
        tokio::spawn(async move { chat(&gate, SMART, 2, 20, None).await })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    let (s, body) = chat(&gate, "code-alias", 2, 20, None).await;
    assert_eq!(s, 200);
    assert!(body.contains("[DONE]"));
    let (s, body) = a.await.unwrap();
    assert_eq!(s, 200);
    assert!(body.contains("[DONE]"), "not cut");
    assert_eq!(b.await.unwrap().0, 200);
    assert!(router.cuts().is_empty(), "{:?}", router.events());
    assert_eq!(
        router.loads_of(CODE),
        0,
        "never unloaded and reloaded for its own alias"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn a_session_generation_whose_client_left_is_never_cut_and_its_resume_counts() {
    use futures_util::StreamExt;
    let (router, gate) = setup(
        &[(CODE, St::Loaded, false), (SMART, St::Unloaded, false)],
        FakeConfig::default(),
        |_| {},
    )
    .await;
    // A session request: its client leaves after the first chunk (a page reload).
    let resp = client()
        .post(gate.url("/v1/chat/completions"))
        .header("x-conversation-id", "conv-synthetic-1")
        .body(chat_body(CODE, 20, 40))
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
    // Another route asks meanwhile: it waits for the generation the router still runs.
    let b = {
        let gate = Arc::clone(&gate);
        tokio::spawn(async move { chat(&gate, SMART, 2, 20, None).await })
    };
    // The page comes back and resumes its answer through the gate.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let resumed = client()
        .get(gate.url("/v1/stream?conv_id=conv-synthetic-1&from=1"))
        .send()
        .await
        .unwrap();
    assert_eq!(resumed.status().as_u16(), 200);
    let text = resumed.text().await.unwrap();
    assert!(text.contains("[DONE]"), "the resumed answer ends: {text}");
    assert_eq!(b.await.unwrap().0, 200);
    assert!(router.cuts().is_empty(), "{:?}", router.events());
    assert!(router.events().contains(&Event::Resume(CODE.into())));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn a_detached_session_without_a_resume_holds_its_model_for_the_session_hold() {
    use futures_util::StreamExt;
    let (router, gate) = setup(
        &[(CODE, St::Loaded, false), (SMART, St::Unloaded, false)],
        FakeConfig::default(),
        |_| {},
    )
    .await;
    let resp = client()
        .post(gate.url("/v1/chat/completions"))
        .header("x-conversation-id", "conv-synthetic-2")
        .body(chat_body(CODE, 25, 40))
        .send()
        .await
        .unwrap();
    let mut stream = resp.bytes_stream();
    let _ = stream.next().await;
    drop(stream);
    let (s, _) = chat(&gate, SMART, 2, 20, None).await;
    assert_eq!(s, 200);
    assert!(router.cuts().is_empty(), "{:?}", router.events());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn nothing_is_admitted_while_a_preset_reload_runs_and_it_cuts_nothing() {
    let (router, gate) = setup(
        &[(CODE, St::Loaded, false), (SMART, St::Unloaded, false)],
        FakeConfig {
            reload_ms: 300,
            ..FakeConfig::default()
        },
        |_| {},
    )
    .await;
    let reload = {
        let gate = Arc::clone(&gate);
        tokio::spawn(async move {
            client()
                .get(gate.url("/v1/models?reload=1"))
                .send()
                .await
                .unwrap()
                .status()
                .as_u16()
        })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(status(&gate).await["scheduler"]["switch"], "maintenance");
    // A chat during the reload waits for it instead of being restarted by it.
    let (s, body) = chat(&gate, CODE, 4, 30, None).await;
    assert_eq!(s, 200);
    assert!(body.contains("[DONE]"));
    assert_eq!(reload.await.unwrap(), 200);
    assert!(router.events().contains(&Event::Reload));
    assert!(router.cuts().is_empty(), "{:?}", router.events());
    assert_eq!(status(&gate).await["scheduler"]["switch"], "idle");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reload_whose_client_leaves_still_holds_admission_until_the_router_answers() {
    let (router, gate) = setup(
        &[(CODE, St::Loaded, false)],
        FakeConfig {
            reload_ms: 400,
            ..FakeConfig::default()
        },
        |_| {},
    )
    .await;
    let quick = reqwest::Client::builder()
        .timeout(Duration::from_millis(50))
        .build()
        .unwrap();
    assert!(quick
        .get(gate.url("/models?reload=1"))
        .send()
        .await
        .is_err());
    assert_eq!(status(&gate).await["scheduler"]["switch"], "maintenance");
    let (s, _) = chat(&gate, CODE, 2, 20, None).await;
    assert_eq!(s, 200);
    assert!(router.cuts().is_empty(), "{:?}", router.events());
    assert_eq!(status(&gate).await["scheduler"]["switch"], "idle");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unload_whose_client_leaves_is_still_reported_and_never_wedges_the_gate() {
    let (router, gate) = setup(
        &[(CODE, St::Loaded, false), (SMART, St::Unloaded, false)],
        FakeConfig {
            unload_answer_ms: 300,
            ..FakeConfig::default()
        },
        |_| {},
    )
    .await;
    let quick = reqwest::Client::builder()
        .timeout(Duration::from_millis(30))
        .build()
        .unwrap();
    let left = quick
        .post(gate.url("/models/unload"))
        .body(r#"{"model":"route-code"}"#)
        .send()
        .await;
    assert!(left.is_err(), "the client gave up");
    // The unload goes on; once the router lists the model gone, the next model gets in.
    let (s, _) = chat(&gate, SMART, 2, 20, None).await;
    assert_eq!(s, 200);
    assert_eq!(status(&gate).await["scheduler"]["switch"], "idle");
    assert!(router.cuts().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refused_unload_keeps_the_router_up() {
    let (router, gate) = setup(
        &[(CODE, St::Loaded, false), (SMART, St::Unloaded, false)],
        FakeConfig {
            unload_error: true,
            ..FakeConfig::default()
        },
        |_| {},
    )
    .await;
    let (s, _) = unload(&gate, CODE, false).await;
    assert_eq!(s, 500, "the router's own answer");
    let st = status(&gate).await;
    assert_eq!(st["scheduler"]["router_up"], true, "{st}");
    assert_eq!(st["scheduler"]["switch"], "idle");
    // The holder still serves.
    assert_eq!(chat(&gate, CODE, 1, 10, None).await.0, 200);
    // A switch the router refuses is tried again later, not in a storm, and the router stays up.
    router.configure(|c| c.unload_error = false);
    assert_eq!(chat(&gate, SMART, 1, 10, None).await.0, 200);
    assert!(router.cuts().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unknown_models_downloads_and_requests_that_cannot_load_never_switch() {
    let (router, gate) = setup(
        &[(CODE, St::Loaded, false), (SMART, St::Unloaded, false)],
        FakeConfig::default(),
        |_| {},
    )
    .await;
    let r = client()
        .post(gate.url("/v1/chat/completions"))
        .body(chat_body("no-such-model", 1, 10))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 404);
    assert_eq!(json_of(r).await["error"]["code"], "model_not_found");
    // A download passes to the router as is.
    let r = client()
        .post(gate.url("/models"))
        .body(r#"{"model":"org/synthetic-repo:Q4"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(
        r.status().as_u16(),
        404,
        "the fake has no downloads: its own answer"
    );
    // A generation that asks the router not to load a model that is not resident.
    let r = client()
        .post(gate.url("/v1/chat/completions?autoload=false"))
        .body(chat_body(SMART, 1, 10))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 400, "the router's own answer");
    tokio::time::sleep(Duration::from_millis(300)).await;
    let st = status(&gate).await;
    assert_eq!(st["scheduler"]["counters"]["switches"], 0, "{st}");
    assert_eq!(st["scheduler"]["holder"], CODE);
    assert!(!router
        .events()
        .iter()
        .any(|e| matches!(e, Event::Unload { .. })));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn a_client_without_a_lease_keeps_its_model_between_close_calls() {
    // The Diary's pattern: several calls in a row without a lease, a short pause between them.
    let (router, gate) = setup(
        &[(CODE, St::Loaded, false), (SMART, St::Unloaded, false)],
        FakeConfig::default(),
        |c| {
            c.admission.residency_grace_ms = 300;
            c.admission.fair_hold_ms = 5_000;
        },
    )
    .await;
    let first = chat(&gate, CODE, 2, 20, None).await;
    assert_eq!(first.0, 200);
    let other = {
        let gate = Arc::clone(&gate);
        tokio::spawn(async move { chat(&gate, SMART, 1, 10, None).await })
    };
    for _ in 0..4 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(chat(&gate, CODE, 2, 20, None).await.0, 200);
    }
    assert_eq!(other.await.unwrap().0, 200);
    assert_eq!(
        router.loads_of(CODE),
        0,
        "never switched away between its calls"
    );
    assert_eq!(router.loads_of(SMART), 1);
    assert!(router.cuts().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn slow_headers_are_cut_off_and_connections_are_bounded() {
    let (_router, gate) = setup(&[(CODE, St::Loaded, false)], FakeConfig::default(), |c| {
        c.max_connections = 3
    })
    .await;
    // Headers that never finish: closed after the header read timeout.
    let started = std::time::Instant::now();
    let answer = raw(gate.addr, b"GET /health HTTP/1.1\r\nhost: gate\r\n", 3_000).await;
    assert!(started.elapsed() < Duration::from_millis(2_500), "{answer}");
    // Three idle connections fill the limit; the next is closed at once.
    let mut held = Vec::new();
    for _ in 0..3 {
        held.push(tokio::net::TcpStream::connect(gate.addr).await.unwrap());
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    let answer = raw(
        gate.addr,
        b"GET /gate/v1/health HTTP/1.1\r\nhost: gate\r\n\r\n",
        300,
    )
    .await;
    assert!(answer.is_empty(), "{answer}");
    drop(held);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let r = client()
        .get(gate.url("/gate/v1/health"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_gate_frames_forwarded_bodies_itself() {
    let (router, gate) = setup(&[(CODE, St::Loaded, false)], FakeConfig::default(), |c| {
        // A buffer exactly the size of the largest body, not a multiple of the 1 KiB unit.
        c.max_body_bytes = 5_000;
        c.max_buffered_bytes = 5_000;
    })
    .await;
    let body = format!("{{\"pad\":\"{}\"}}", "x".repeat(4_990));
    assert_eq!(body.len(), 5_000);
    let request = format!(
        "POST /echo HTTP/1.1\r\nhost: gate\r\nexpect: 100-continue\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    let answer = raw(gate.addr, request.as_bytes(), 2_000).await;
    assert!(answer.contains("200 OK"), "{answer}");
    assert!(answer.ends_with(&body), "the whole body, byte for byte");
    let seen = router.state.lock().unwrap().seen_headers.clone();
    let (_, headers) = seen.iter().rev().find(|(p, _)| p == "/echo").unwrap();
    assert!(headers.get("expect").is_none(), "expect is the gate's own");
    assert_eq!(headers["content-length"], "5000");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_model_hidden_behind_many_query_keys_is_refused_not_passed() {
    let (router, gate) = setup(
        &[(CODE, St::Loaded, false), (SMART, St::Unloaded, false)],
        FakeConfig::default(),
        |_| {},
    )
    .await;
    let keys: Vec<String> = (0..64).map(|i| format!("k{i}=1")).collect();
    let path = format!("/props?{}&model=route-smart", keys.join("&"));
    let r = client().get(gate.url(&path)).send().await.unwrap();
    assert_eq!(r.status().as_u16(), 400);
    assert_eq!(json_of(r).await["error"]["code"], "invalid_query");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(router.loads_of(SMART), 0);
    assert!(!router
        .state
        .lock()
        .unwrap()
        .seen_headers
        .iter()
        .any(|(p, _)| p == "/props"));
}
