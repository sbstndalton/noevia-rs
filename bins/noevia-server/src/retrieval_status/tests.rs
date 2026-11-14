//! Parity tests for retrieval_status.rs. The oracle is core `server/retrieval-status.test.cjs`
//! (#340): each of its cases is one test here (named after it), plus the publisher's build
//! (`rust-health.cjs` `ragState`/`ownerReady`), `embeddingOff`, `clampInt` and the wire shape of
//! both probes over a loopback server.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
use super::*;
use serde_json::json;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const START: &str = "start-qa";
const NOW: i64 = 1_800_000_000_000;

fn target(profile: &str, model: &str, base: Option<&str>) -> Target {
    Target {
        profile: profile.into(),
        model: model.into(),
        base_url: base.map(str::to_owned),
    }
}

fn config(t: Target) -> EmbeddingConfig {
    EmbeddingConfig {
        version: 0,
        active: t,
        next: None,
        previous: None,
    }
}

fn router() -> (String, Option<String>) {
    ("http://router.invalid/v1/models".into(), Some("qa-key".into()))
}

struct World {
    dir: tempfile::TempDir,
    clock: Arc<AtomicI64>,
    calls: Arc<AtomicUsize>,
    seen: Arc<Mutex<Vec<Request>>>,
}

impl World {
    fn new() -> World {
        World {
            dir: tempfile::tempdir().unwrap(),
            clock: Arc::new(AtomicI64::new(NOW)),
            calls: Arc::new(AtomicUsize::new(0)),
            seen: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn advance(&self, ms: i64) {
        self.clock.fetch_add(ms, Ordering::SeqCst);
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    /// One readiness read with `found` as the probe's answer.
    async fn read(
        &self,
        status: &Status,
        seed: &EmbeddingConfig,
        held: bool,
        found: Found,
    ) -> (bool, Word) {
        self.read_with(status, seed, held, true, true, found).await
    }

    async fn read_with(
        &self,
        status: &Status,
        seed: &EmbeddingConfig,
        held: bool,
        owner_ready: bool,
        deps_ok: bool,
        found: Found,
    ) -> (bool, Word) {
        let clock = Arc::clone(&self.clock);
        let tick = move || clock.load(Ordering::SeqCst);
        let gate = move || held;
        let (calls, seen) = (Arc::clone(&self.calls), Arc::clone(&self.seen));
        status
            .state(
                Inputs {
                    data_dir: self.dir.path(),
                    start: START,
                    clock: &tick,
                    owner_ready,
                    deps_ok,
                    seed: Some(seed),
                    service_url: Some("http://embed-service:8080"),
                    maintenance_held: &gate,
                    router: router(),
                },
                move |request| async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    lock(&seen).push(request);
                    found
                },
            )
            .await
    }
}

fn status(ttl_ms: i64) -> Status {
    Status::new(Knobs {
        timeout: Duration::from_millis(2500),
        ttl_ms,
    })
}

// ── clampInt and the knobs ──────────────────────────────────────────────────────────────────────

#[test]
fn clamp_int_is_parse_int_then_clamp() {
    assert_eq!(clamp_int(None, 2500, 200, 5000), 2500);
    assert_eq!(clamp_int(Some(""), 2500, 200, 5000), 2500);
    assert_eq!(clamp_int(Some("abc"), 2500, 200, 5000), 2500);
    assert_eq!(clamp_int(Some("80"), 2500, 200, 5000), 200);
    assert_eq!(clamp_int(Some("3000.9"), 2500, 200, 5000), 3000);
    assert_eq!(clamp_int(Some("  1200ms"), 2500, 200, 5000), 1200);
    assert_eq!(clamp_int(Some("+900"), 2500, 200, 5000), 900);
    assert_eq!(clamp_int(Some("-5"), 2500, 200, 5000), 200);
    assert_eq!(clamp_int(Some("- 5"), 2500, 200, 5000), 2500);
    assert_eq!(clamp_int(Some("0x10"), 2500, 200, 5000), 200);
    assert_eq!(
        clamp_int(Some("99999999999999999999999"), 2500, 200, 5000),
        5000
    );
    // `parseInt` skips JS whitespace, which includes the no-break space.
    assert_eq!(clamp_int(Some("\u{a0}700"), 2500, 200, 5000), 700);
}

#[test]
fn knobs_follow_rag_cjs_defaults_and_clamps() {
    let k = |pairs: &[(&str, &str)]| {
        let map: std::collections::HashMap<_, _> = pairs.iter().copied().collect();
        Knobs::from_lookup(&|key| map.get(key).map(|v| (*v).to_owned()))
    };
    assert_eq!(
        k(&[]),
        Knobs {
            timeout: Duration::from_millis(2500),
            ttl_ms: 45000
        }
    );
    // The tests' '80' and '100' are clamped to the module's floors.
    assert_eq!(
        k(&[
            ("RETRIEVAL_PROBE_TIMEOUT_MS", "80"),
            ("RETRIEVAL_PROBE_TTL_MS", "100")
        ]),
        Knobs {
            timeout: Duration::from_millis(200),
            ttl_ms: 200
        }
    );
    assert_eq!(
        k(&[
            ("RETRIEVAL_PROBE_TIMEOUT_MS", "60000"),
            ("RETRIEVAL_PROBE_TTL_MS", "999999")
        ]),
        Knobs {
            timeout: Duration::from_millis(5000),
            ttl_ms: 300_000
        }
    );
}

// ── The request each path sends ─────────────────────────────────────────────────────────────────

#[test]
fn router_path_lists_models_with_the_inference_bearer() {
    let r = request_for(&target("nomic-v1", "probe-model", None), &router());
    assert_eq!(
        r,
        Some(Request::Router {
            url: "http://router.invalid/v1/models".into(),
            bearer: Some("qa-key".into()),
            model: "probe-model".into(),
        })
    );
    // An empty address is the shared engine too (`if (target.baseUrl)`).
    assert!(matches!(
        request_for(&target("nomic-v1", "probe-model", Some("")), &router()),
        Some(Request::Router { .. })
    ));
}

#[test]
fn sidecar_path_posts_the_fixed_synthetic_text_and_the_model() {
    let r = request_for(
        &target("nomic-v1", "probe-model", Some("http://sidecar.invalid")),
        &router(),
    );
    assert_eq!(
        r,
        Some(Request::Sidecar {
            url: "http://sidecar.invalid/v1/embeddings".into(),
            body: r#"{"model":"probe-model","input":["noevia health probe"]}"#.into(),
        })
    );
    // Trailing slashes, then one `/v1`, go before the path; the query framing is the profile's.
    let r = request_for(
        &target(
            "embeddinggemma-2",
            "probe-model",
            Some("http://sidecar.invalid/v1//"),
        ),
        &router(),
    );
    assert_eq!(
        r,
        Some(Request::Sidecar {
            url: "http://sidecar.invalid/v1/embeddings".into(),
            body: r#"{"model":"probe-model","input":["task: search result | query: noevia health probe"]}"#.into(),
        })
    );
    let r = request_for(
        &target("nomic-v1", "probe-model", Some("http://sidecar.invalid/v1/v1")),
        &router(),
    );
    assert!(
        matches!(r, Some(Request::Sidecar { url, .. }) if url == "http://sidecar.invalid/v1/v1/embeddings")
    );
}

#[test]
fn an_unknown_profile_sends_nothing() {
    assert_eq!(
        request_for(&target("qa-unknown", "probe-model", None), &router()),
        None
    );
    assert_eq!(
        request_for(
            &target("qa-unknown", "probe-model", Some("http://sidecar.invalid")),
            &router()
        ),
        None
    );
}

#[test]
fn answers_are_read_as_core_reads_them() {
    let side = Request::Sidecar {
        url: String::new(),
        body: String::new(),
    };
    assert!(answer_up(
        &side,
        &json!({"data": [{"embedding": [0.1, 0.2, 0.3]}]})
    ));
    assert!(
        !answer_up(&side, &json!({"data": []})),
        "an empty data array is not a real vector"
    );
    assert!(!answer_up(&side, &json!({"data": [{"embedding": []}]})));
    assert!(!answer_up(&side, &json!({"data": [{"embedding": "x"}]})));
    assert!(!answer_up(&side, &json!({"data": {"embedding": [1]}})));
    assert!(!answer_up(&side, &json!([1])));
    // Only the first item counts (`items[0]?.embedding`).
    assert!(!answer_up(
        &side,
        &json!({"data": [{"x": 1}, {"embedding": [1]}]})
    ));
    let route = Request::Router {
        url: String::new(),
        bearer: None,
        model: "probe-model".into(),
    };
    assert!(answer_up(
        &route,
        &json!({"data": [{"id": "probe-model"}, {"id": "chat-model"}]})
    ));
    assert!(!answer_up(&route, &json!({"data": [{"id": "chat-model"}]})));
    assert!(!answer_up(&route, &json!({"data": []})));
    assert!(!answer_up(&route, &json!({"models": [{"id": "probe-model"}]})));
    // `ids.includes(target.model)` is strict equality: case counts, a null id is no id.
    assert!(!answer_up(&route, &json!({"data": [{"id": "Probe-Model"}, null]})));
}

// ── retrieval-status.test.cjs, router path ──────────────────────────────────────────────────────

#[tokio::test]
async fn router_path_embed_model_listed_reads_available_and_never_posts() {
    let w = World::new();
    let seed = config(target("nomic-v1", "probe-model", None));
    let s = status(45000);
    assert_eq!(
        w.read(&s, &seed, false, Found::Up).await,
        (true, Word::Available)
    );
    assert_eq!(w.calls(), 1);
    assert!(matches!(
        lock(&w.seen).first(),
        Some(Request::Router { url, .. }) if url.ends_with("/v1/models")
    ));
}

#[tokio::test]
async fn router_path_embed_model_missing_reads_degraded_not_unavailable() {
    let w = World::new();
    let seed = config(target("nomic-v1", "probe-model", None));
    assert_eq!(
        w.read(&status(45000), &seed, false, Found::Down).await,
        (true, Word::Degraded),
        "native deps are present; only the embed model is not loadable there"
    );
}

#[tokio::test]
async fn router_path_the_result_is_cached_for_the_ttl_and_shared_by_concurrent_callers() {
    let w = World::new();
    let seed = config(target("nomic-v1", "probe-model", None));
    let s = status(200);
    // Two callers at once: the second waits for the first's probe and reads what it found.
    let slow = |w: &World| {
        let (calls, seen) = (Arc::clone(&w.calls), Arc::clone(&w.seen));
        move |request| async move {
            calls.fetch_add(1, Ordering::SeqCst);
            lock(&seen).push(request);
            tokio::time::sleep(Duration::from_millis(50)).await;
            Found::Up
        }
    };
    let clock = Arc::clone(&w.clock);
    let tick = move || clock.load(Ordering::SeqCst);
    let free = || false;
    let inputs = || Inputs {
        data_dir: w.dir.path(),
        start: START,
        clock: &tick,
        owner_ready: true,
        deps_ok: true,
        seed: Some(&seed),
        service_url: None,
        maintenance_held: &free,
        router: router(),
    };
    let (a, b) = tokio::join!(s.state(inputs(), slow(&w)), s.state(inputs(), slow(&w)));
    assert_eq!(a, (true, Word::Available));
    assert_eq!(b, (true, Word::Available));
    assert_eq!(w.calls(), 1, "concurrent callers share one in-flight probe");
    assert_eq!(
        w.read(&s, &seed, false, Found::Down).await,
        (true, Word::Available)
    );
    assert_eq!(w.calls(), 1, "a repeat call inside the TTL does not re-probe");
    w.advance(250);
    assert_eq!(
        w.read(&s, &seed, false, Found::Up).await,
        (true, Word::Available)
    );
    assert_eq!(
        w.calls(),
        2,
        "the stale cache entry was not reused past its TTL"
    );
    // Exactly at the TTL the entry is stale (`now - at < TTL`).
    w.advance(200);
    w.read(&s, &seed, false, Found::Up).await;
    assert_eq!(w.calls(), 3);
}

#[tokio::test]
async fn a_reconfiguration_invalidates_whatever_the_last_probe_found() {
    let w = World::new();
    let s = status(45000);
    let first = config(target("nomic-v1", "probe-model", None));
    assert_eq!(
        w.read(&s, &first, false, Found::Up).await,
        (true, Word::Available)
    );
    assert_eq!(w.calls(), 1);
    // A new model, then a new address: each probes again (Node: a fresh `init()`).
    let model = config(target("nomic-v1", "probe-model-2", None));
    w.read(&s, &model, false, Found::Up).await;
    assert_eq!(w.calls(), 2);
    let address = config(target("nomic-v1", "probe-model-2", Some("http://sidecar2.invalid")));
    w.read(&s, &address, false, Found::Up).await;
    assert_eq!(w.calls(), 3);
    // The saved configuration wins over the environment's seed, as for the native search.
    std::fs::write(
        w.dir.path().join("embedding-config.json"),
        json!({"version": 1, "active": {"model": "saved-model", "profile": "nomic-v1"}}).to_string(),
    )
    .unwrap();
    w.read(&s, &address, false, Found::Down).await;
    assert_eq!(w.calls(), 4);
    assert!(matches!(
        lock(&w.seen).last(),
        Some(Request::Router { model, .. }) if model == "saved-model"
    ));
}

// ── Router path: the maintenance gate is not an outage ──────────────────────────────────────────

#[tokio::test]
async fn router_path_a_held_gate_reports_the_last_cached_status_not_degraded() {
    let w = World::new();
    let seed = config(target("nomic-v1", "probe-model", None));
    let s = status(200);
    assert_eq!(
        w.read(&s, &seed, false, Found::Up).await,
        (true, Word::Available)
    );
    assert_eq!(w.calls(), 1);
    w.advance(250);
    assert_eq!(
        w.read(&s, &seed, true, Found::Down).await,
        (true, Word::Available),
        "maintenance is not an outage: the last real finding wins"
    );
    assert_eq!(
        w.calls(),
        1,
        "no listing request is made while the gate is held"
    );
    w.advance(250);
    assert_eq!(
        w.read(&s, &seed, false, Found::Down).await,
        (true, Word::Degraded),
        "a real probe runs again once the gate is free"
    );
    assert_eq!(w.calls(), 2);
    // A held gate after a degraded finding keeps `degraded`.
    w.advance(250);
    assert_eq!(
        w.read(&s, &seed, true, Found::Up).await,
        (true, Word::Degraded)
    );
    assert_eq!(w.calls(), 2);
}

#[tokio::test]
async fn router_path_a_held_gate_with_nothing_cached_yet_reads_available() {
    let w = World::new();
    let seed = config(target("nomic-v1", "probe-model", None));
    let s = status(45000);
    assert_eq!(
        w.read(&s, &seed, true, Found::Down).await,
        (true, Word::Available),
        "no finding yet and the gate is held: assume available rather than alarm"
    );
    assert_eq!(w.calls(), 0);
    // That answer is cached like a finding (Node caches the fallback too): inside the TTL a free
    // gate does not probe.
    assert_eq!(
        w.read(&s, &seed, false, Found::Down).await,
        (true, Word::Available)
    );
    assert_eq!(w.calls(), 0);
}

// ── Sidecar path ────────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn sidecar_path_a_healthy_answer_reads_available_and_carries_only_the_probe_text() {
    let w = World::new();
    let seed = config(target(
        "nomic-v1",
        "probe-model",
        Some("http://sidecar.invalid"),
    ));
    assert_eq!(
        w.read(&status(45000), &seed, false, Found::Up).await,
        (true, Word::Available)
    );
    assert_eq!(w.calls(), 1);
    let seen = lock(&w.seen);
    let Some(Request::Sidecar { url, body }) = seen.first() else {
        panic!("expected a sidecar request, got {seen:?}");
    };
    assert!(url.ends_with("/v1/embeddings"));
    let body: serde_json::Value = serde_json::from_str(body).unwrap();
    assert_eq!(body["input"], json!(["noevia health probe"]));
    assert_eq!(body["model"], json!("probe-model"));
}

#[tokio::test]
async fn sidecar_path_an_unreachable_or_malformed_answer_reads_degraded() {
    let w = World::new();
    let seed = config(target(
        "nomic-v1",
        "probe-model",
        Some("http://sidecar.invalid"),
    ));
    assert_eq!(
        w.read(&status(45000), &seed, false, Found::Down).await,
        (true, Word::Degraded)
    );
}

#[tokio::test]
async fn sidecar_path_never_goes_through_the_maintenance_gate() {
    let w = World::new();
    let seed = config(target(
        "nomic-v1",
        "probe-model",
        Some("http://sidecar.invalid"),
    ));
    assert_eq!(
        w.read(&status(45000), &seed, true, Found::Up).await,
        (true, Word::Available),
        "the sidecar is not the router: its probe does not consult the router gate"
    );
    assert_eq!(w.calls(), 1);
}

// ── Shared behaviour ────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn missing_native_sqlite_modules_read_unavailable_and_never_probe() {
    let w = World::new();
    let seed = config(target("nomic-v1", "probe-model", None));
    assert_eq!(
        w.read_with(&status(45000), &seed, false, true, false, Found::Up)
            .await,
        (false, Word::Unavailable)
    );
    assert_eq!(w.calls(), 0, "no request is made when the index cannot run at all");
}

#[tokio::test]
async fn an_owner_that_is_not_ready_publishes_unavailable() {
    // `build: () => ownerReady() ? ragState(rag) : { ragAvailable: false, retrieval: 'unavailable' }`.
    let w = World::new();
    let seed = config(target("nomic-v1", "probe-model", None));
    assert_eq!(
        w.read_with(&status(45000), &seed, false, false, true, Found::Up)
            .await,
        (false, Word::Unavailable)
    );
    assert_eq!(w.calls(), 0);
}

#[tokio::test]
async fn the_embedding_service_turned_off_reads_off_without_a_probe() {
    let w = World::new();
    let s = status(45000);
    let status_file = |enabled: bool, at: i64| {
        std::fs::write(
            w.dir.path().join("embedding-worker-status.json"),
            json!({"protocol": "rust-embedding-worker-v1", "start": START, "at": (at - 1000) as f64,
                   "service": {"managed": true, "enabled": enabled}, "serviceAt": (at - 2000) as f64})
            .to_string(),
        )
        .unwrap();
    };
    status_file(false, NOW);
    // The shared engine: off whenever the managed service is off.
    let shared = config(target("nomic-v1", "probe-model", None));
    assert_eq!(
        w.read(&s, &shared, false, Found::Up).await,
        (true, Word::Off)
    );
    // A target on that service: off; on another address: probed.
    let on_service = config(target(
        "nomic-v1",
        "probe-model",
        Some("http://embed-service:8080/v1"),
    ));
    assert_eq!(
        w.read(&s, &on_service, false, Found::Up).await,
        (true, Word::Off)
    );
    assert_eq!(w.calls(), 0);
    let elsewhere = config(target(
        "nomic-v1",
        "probe-model",
        Some("http://sidecar.invalid"),
    ));
    assert_eq!(
        w.read(&s, &elsewhere, false, Found::Up).await,
        (true, Word::Available)
    );
    assert_eq!(w.calls(), 1);
    // Turned back on (a later observation): the shared engine is probed again.
    w.advance(5000);
    status_file(true, NOW + 5000);
    assert_eq!(
        w.read(&s, &shared, false, Found::Down).await,
        (true, Word::Degraded)
    );
    assert_eq!(w.calls(), 2);
}

#[tokio::test]
async fn an_unreadable_saved_configuration_reads_degraded_without_a_probe() {
    let w = World::new();
    std::fs::write(w.dir.path().join("embedding-config.json"), b"{not json").unwrap();
    let seed = config(target("nomic-v1", "probe-model", None));
    assert_eq!(
        w.read(&status(45000), &seed, false, Found::Up).await,
        (true, Word::Degraded)
    );
    assert_eq!(w.calls(), 0);
}

#[test]
fn words_are_cores_four() {
    let words: Vec<_> = [Word::Available, Word::Degraded, Word::Off, Word::Unavailable]
        .iter()
        .map(|w| w.as_str())
        .collect();
    assert_eq!(words, ["available", "degraded", "off", "unavailable"]);
    assert_eq!(
        crate::health_routes::shadow_line((true, "available"), (true, "available")),
        None
    );
    assert_eq!(
        crate::health_routes::shadow_line((true, "available"), (true, "degraded")).as_deref(),
        Some("differs (node ragAvailable=true retrieval=available, native ragAvailable=true retrieval=degraded)")
    );
}

// ── The wire: one loopback server per request ───────────────────────────────────────────────────

/// Serves one connection: waits `delay`, answers `reply`, and returns the raw request it read.
async fn one_reply(
    reply: Vec<u8>,
    delay: Duration,
) -> (String, tokio::task::JoinHandle<Vec<u8>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let Ok((mut sock, _)) = listener.accept().await else {
            return Vec::new();
        };
        let mut seen = Vec::new();
        let mut buf = [0u8; 4096];
        // Read the head, then the body by its Content-Length.
        loop {
            let Ok(n) = sock.read(&mut buf).await else { break };
            if n == 0 {
                break;
            }
            seen.extend_from_slice(&buf[..n]);
            let text = String::from_utf8_lossy(&seen).to_string();
            if let Some(end) = text.find("\r\n\r\n") {
                let length = text
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                    })
                    .unwrap_or(0);
                if seen.len() >= end + 4 + length {
                    break;
                }
            }
        }
        tokio::time::sleep(delay).await;
        let _ = sock.write_all(&reply).await;
        let _ = sock.shutdown().await;
        seen
    });
    (format!("http://{addr}"), task)
}

fn http(status: &str, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

fn router_request(base: &str) -> Request {
    Request::Router {
        url: format!("{base}/v1/models"),
        bearer: Some("qa-key".into()),
        model: "probe-model".into(),
    }
}

#[tokio::test]
async fn the_router_probe_is_a_get_with_the_bearer_and_no_body() {
    let (base, seen) = one_reply(
        http("200 OK", r#"{"data":[{"id":"probe-model"},{"id":"chat-model"}]}"#),
        Duration::ZERO,
    )
    .await;
    assert_eq!(
        send(router_request(&base), Duration::from_millis(2500)).await,
        Found::Up
    );
    let raw = String::from_utf8(seen.await.unwrap()).unwrap();
    let lower = raw.to_ascii_lowercase();
    assert!(raw.starts_with("GET /v1/models HTTP/1.1\r\n"), "{raw}");
    assert!(lower.contains("\r\nauthorization: bearer qa-key\r\n"), "{raw}");
    assert!(raw.ends_with("\r\n\r\n"), "GET /v1/models never carries a request body");
}

#[tokio::test]
async fn the_sidecar_probe_is_a_post_without_credentials() {
    let (base, seen) = one_reply(
        http("200 OK", r#"{"data":[{"embedding":[0.1,0.2,0.3]}]}"#),
        Duration::ZERO,
    )
    .await;
    let body = r#"{"model":"probe-model","input":["noevia health probe"]}"#;
    let request = Request::Sidecar {
        url: format!("{base}/v1/embeddings"),
        body: body.into(),
    };
    assert_eq!(send(request, Duration::from_millis(2500)).await, Found::Up);
    let raw = String::from_utf8(seen.await.unwrap()).unwrap();
    let lower = raw.to_ascii_lowercase();
    assert!(raw.starts_with("POST /v1/embeddings HTTP/1.1\r\n"), "{raw}");
    assert!(lower.contains("\r\ncontent-type: application/json\r\n"));
    assert!(!lower.contains("authorization:"));
    assert!(raw.ends_with(body));
}

#[tokio::test]
async fn an_unreachable_non_2xx_redirect_or_unreadable_answer_is_down() {
    // Nothing listens: a closed port.
    let closed = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let a = l.local_addr().unwrap();
        drop(l);
        format!("http://{a}")
    };
    assert_eq!(
        send(router_request(&closed), Duration::from_millis(2500)).await,
        Found::Down
    );
    for reply in [
        http("500 Internal Server Error", "boom"),
        http("503 Service Unavailable", r#"{"data":[{"id":"probe-model"}]}"#),
        b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:1/v1/models\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            .to_vec(),
        http("200 OK", "not json"),
        http("200 OK", r#"{"data":[{"id":"chat-model"}]}"#),
    ] {
        let (base, _seen) = one_reply(reply, Duration::ZERO).await;
        assert_eq!(
            send(router_request(&base), Duration::from_millis(2500)).await,
            Found::Down
        );
    }
}

#[tokio::test]
async fn an_answer_over_its_cap_is_down() {
    let big = format!(
        r#"{{"data":[{{"id":"probe-model"}}],"pad":"{}"}}"#,
        "x".repeat(ROUTER_CAP)
    );
    let (base, _seen) = one_reply(http("200 OK", &big), Duration::ZERO).await;
    assert_eq!(
        send(router_request(&base), Duration::from_millis(5000)).await,
        Found::Down
    );
    // Just under the cap is read.
    let pad = ROUTER_CAP - r#"{"data":[{"id":"probe-model"}],"pad":""}"#.len();
    let fits = format!(r#"{{"data":[{{"id":"probe-model"}}],"pad":"{}"}}"#, "x".repeat(pad));
    assert_eq!(fits.len(), ROUTER_CAP);
    let (base, _seen) = one_reply(http("200 OK", &fits), Duration::ZERO).await;
    assert_eq!(
        send(router_request(&base), Duration::from_millis(5000)).await,
        Found::Up
    );
}

#[tokio::test]
async fn a_slow_answer_is_bounded_by_the_probe_timeout() {
    let (base, _seen) = one_reply(
        http("200 OK", r#"{"data":[{"id":"probe-model"}]}"#),
        Duration::from_secs(5),
    )
    .await;
    let started = std::time::Instant::now();
    assert_eq!(
        send(router_request(&base), Duration::from_millis(200)).await,
        Found::Down
    );
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "the probe gave up near its own timeout"
    );
}
