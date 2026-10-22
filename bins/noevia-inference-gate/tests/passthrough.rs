//! What the web server, the model manager and the release preflight read through the gate today
//! reaches the router unchanged (sbstndalton/noevia#2115): the listing, properties, metrics and
//! slots (which must never load or wake a model), explicit loads, unloads of idle models, preset
//! reloads, and streamed generations and embeddings without buffering.
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

async fn both(
    router: &FakeRouter,
    gate: &GateHandle,
    path: &str,
) -> ((u16, String), (u16, String)) {
    let direct = client()
        .get(format!("{}{path}", router.url()))
        .send()
        .await
        .unwrap();
    let direct = (direct.status().as_u16(), direct.text().await.unwrap());
    let through = client().get(gate.url(path)).send().await.unwrap();
    let through = (through.status().as_u16(), through.text().await.unwrap());
    (direct, through)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reads_are_byte_identical_and_never_load_or_wake() {
    let router = FakeRouter::start(
        &[
            (CODE, St::Loaded, false),
            (SMART, St::Unloaded, false),
            ("route-fast", St::Sleeping, false),
        ],
        FakeConfig::default(),
    )
    .await;
    let gate = start_gate(test_config(router.url())).await;
    for path in [
        "/models",
        "/v1/models",
        "/health",
        "/props?model=route-code&autoload=false",
        "/metrics?model=route-code&autoload=false",
        "/slots?model=route-code&autoload=false",
        "/props?model=route-smart&autoload=false",
        "/metrics?model=route-smart&autoload=false",
    ] {
        let (direct, through) = both(&router, &gate, path).await;
        assert_eq!(direct, through, "{path}");
    }
    assert_eq!(
        router.loads_of(SMART),
        0,
        "a read with autoload=false never loads"
    );
    assert!(
        !router.events().iter().any(|e| matches!(e, Event::Wake(_))),
        "nor wakes"
    );
    assert_eq!(router.status(SMART), St::Unloaded);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gate_headers_are_removed_and_the_rest_forwarded() {
    let router = FakeRouter::start(&[(CODE, St::Loaded, false)], FakeConfig::default()).await;
    let gate = start_gate(test_config(router.url())).await;
    let lease = acquire_lease(&gate, CODE).await.unwrap();
    let r = client()
        .post(gate.url("/v1/chat/completions"))
        .header("authorization", "Bearer synthetic-key")
        .header("x-noevia-lease", &lease)
        .header("x-noevia-client", "web")
        .header("x-noevia-max-wait-ms", "1000")
        .header("x-custom-trace", "kept")
        .body(chat_body(CODE, 1, 10))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    let _ = r.text().await;
    let seen = router.state.lock().unwrap().seen_headers.clone();
    let (_, headers) = seen
        .iter()
        .rev()
        .find(|(p, _)| p == "/v1/chat/completions")
        .unwrap();
    assert_eq!(headers["authorization"], "Bearer synthetic-key");
    assert_eq!(headers["x-custom-trace"], "kept");
    for gone in [
        "x-noevia-lease",
        "x-noevia-client",
        "x-noevia-max-wait-ms",
        "x-noevia-gate-admin",
    ] {
        assert!(headers.get(gone).is_none(), "{gone} forwarded");
    }
    // The body arrives byte for byte (the synthetic fields drive the fake).
    let r = client()
        .post(gate.url("/echo"))
        .body("{\"no\":\"model\",\"x\":[1,2,3]}")
        .send()
        .await
        .unwrap();
    assert_eq!(r.text().await.unwrap(), "{\"no\":\"model\",\"x\":[1,2,3]}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn streams_are_not_buffered() {
    use futures_util::StreamExt;
    let router = FakeRouter::start(&[(CODE, St::Loaded, false)], FakeConfig::default()).await;
    let gate = start_gate(test_config(router.url())).await;
    let started = Instant::now();
    let resp = client()
        .post(gate.url("/v1/chat/completions"))
        .body(chat_body(CODE, 3, 300))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.headers()["content-type"], "text/event-stream");
    let mut stream = resp.bytes_stream();
    let first = stream.next().await.unwrap().unwrap();
    let first_at = started.elapsed();
    assert!(String::from_utf8_lossy(&first).contains("t0"));
    assert!(
        first_at < Duration::from_millis(650),
        "first chunk after {first_at:?}"
    );
    let mut rest = Vec::new();
    while let Some(chunk) = stream.next().await {
        rest.extend_from_slice(&chunk.unwrap());
    }
    assert!(String::from_utf8_lossy(&rest).contains("[DONE]"));
    assert!(started.elapsed() >= Duration::from_millis(850));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn explicit_loads_idle_unloads_and_embeddings_pass() {
    let router = FakeRouter::start(
        &[(CODE, St::Unloaded, false), (SMART, St::Unloaded, false)],
        FakeConfig::default(),
    )
    .await;
    let gate = start_gate(test_config(router.url())).await;
    let r = client()
        .post(gate.url("/models/load"))
        .body(r#"{"model":"route-code"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    assert!(router.events().contains(&Event::Load(CODE.into())));
    tokio::time::sleep(Duration::from_millis(150)).await;
    let r = client()
        .post(gate.url("/models/load"))
        .body(r#"{"model":"route-code"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(
        r.status().as_u16(),
        400,
        "the router's own answer passes through"
    );
    let r = client()
        .post(gate.url("/models/unload"))
        .body(r#"{"model":"route-code"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(
        r.status().as_u16(),
        200,
        "an idle model unloads without the admin"
    );
    let r = client()
        .post(gate.url("/models/unload"))
        .body(r#"{"model":"route-smart"}"#)
        .send()
        .await
        .unwrap();
    assert!(
        matches!(r.status().as_u16(), 400 | 409),
        "not running, or the first unload still runs"
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    let r = client()
        .post(gate.url("/v1/embeddings"))
        .body(r#"{"model":"route-smart","input":"synthetic"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    assert!(router.cuts().is_empty());
}
