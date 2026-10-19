#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
use super::*;
use crate::backend_stats::{Config, DockerApi, Raw};
use serde_json::Value as V;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Mutex,
};
fn fixture() -> V {
    serde_json::from_str(include_str!(
        "../../tests/fixtures/bench-observe-python.v1.json"
    ))
    .unwrap()
}
#[test]
fn actual_python_load_and_contention_oracles() {
    let fx = fixture();
    assert_eq!(fx["source_sha256"]["bench.py"].as_str().unwrap().len(), 64);
    for c in fx["load"].as_array().unwrap() {
        assert_eq!(
            load_ms(c["text"].as_str().unwrap(), c["alias"].as_str().unwrap()).unwrap(),
            c["want"].as_f64(),
            "{}",
            c["id"]
        );
    }
    for c in fx["contention"].as_array().unwrap() {
        let aliases = c["aliases"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a.as_str().unwrap().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            contention(
                c["text"].as_str().unwrap(),
                &aliases,
                c["since"].as_f64().unwrap()
            )
            .unwrap(),
            c["want"].as_bool().unwrap(),
            "{}",
            c["id"]
        );
    }
}
struct World {
    seen: Mutex<Vec<String>>,
    mode: u8,
    active: Arc<AtomicUsize>,
}
struct Active(Arc<AtomicUsize>);
impl Drop for Active {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}
impl DockerApi for World {
    fn get<'a>(&'a self, path: &'a str) -> crate::Job<'a, Raw> {
        Box::pin(async move {
            self.seen.lock().unwrap().push(path.to_owned());
            let body = match path {
                "/version" => br#"{"ApiVersion":"1.45"}"#.to_vec(),
                "/containers/json?all=true&limit=1025&size=false" => b"[]".to_vec(),
                "/containers/qa-backend/json" => {
                    br#"{"Id":"qa-id","Config":{"Tty":true}}"#.to_vec()
                }
                p if p.contains("/logs?") => {
                    if self.mode == 1 {
                        return Err("unreachable");
                    }
                    if self.mode == 2 {
                        return Err(crate::docker::EXCEEDED);
                    }
                    if self.mode == 4 {
                        return Ok(Raw {
                            status: 200,
                            reason: "OK".into(),
                            body: vec![b'x'; LOG_CAP + 1],
                        });
                    }
                    if self.mode == 3 {
                        self.active.fetch_add(1, Ordering::SeqCst);
                        let _active = Active(self.active.clone());
                        std::future::pending::<()>().await;
                    }
                    if p.ends_with("tail=800") {
                        b"2026-10-18T10:00:01Z ensure_model: model name=qa-a is not loaded\n2026-10-18T10:00:02Z llama_server: listening on http://127.0.0.1:18001".to_vec()
                    } else {
                        Vec::new()
                    }
                }
                _ => panic!("unexpected read {path}"),
            };
            Ok(Raw {
                status: 200,
                reason: "OK".into(),
                body,
            })
        })
    }
}
fn world(mode: u8) -> (Arc<BackendStats>, Arc<World>) {
    let w = Arc::new(World {
        seen: Mutex::new(Vec::new()),
        mode,
        active: Arc::new(AtomicUsize::new(0)),
    });
    let stats = BackendStats::new(
        Config {
            configured: vec!["qa-backend".into()],
            gpu_vram: Vec::new(),
            default_port: Some(8080),
            drm_root: "/nonexistent-qa-drm".into(),
            meminfo: "/nonexistent-qa-meminfo".into(),
            ram_reserve_gb: Some(0.0),
        },
        w.clone(),
    )
    .unwrap();
    (Arc::new(stats), w)
}
fn query(cold: bool) -> String {
    crate::bench_runner::observe_query("qa-backend", "qa-a", &["qa-a".into()], 0.0, cold)
}
#[tokio::test]
async fn separate_tails_cold_gating_and_best_effort_io_defaults() {
    for mode in [0, 1] {
        let (stats, w) = world(mode);
        let observe = observer(stats);
        let out = observe(query(true)).await.unwrap();
        assert_eq!(out.peak_vram_json, "[]");
        assert!(!out.contended);
        assert_eq!(out.load_ms, if mode == 0 { Some(1000.0) } else { None });
        let reads = w
            .seen
            .lock()
            .unwrap()
            .iter()
            .filter(|s| s.contains("/logs?"))
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(reads.len(), 2);
        assert!(reads[0].ends_with("tail=400"));
        assert!(reads[1].ends_with("tail=800"));
    }
    let (stats, w) = world(0);
    assert_eq!(observer(stats)(query(false)).await.unwrap().load_ms, None);
    assert_eq!(
        w.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|s| s.contains("/logs?"))
            .count(),
        1
    );
}
#[tokio::test]
async fn bounds_and_unknown_targets_are_explicit_errors() {
    let (stats, w) = world(2);
    assert!(observer(stats)(query(true)).await.is_err());
    assert_eq!(
        w.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|s| s.contains("/logs?"))
            .count(),
        1
    );
    let (large, _) = world(4);
    assert!(observer(large)(query(true)).await.is_err());
    let (stats, w) = world(0);
    assert!(observer(stats)("backend=unconfigured&since=0".into())
        .await
        .is_err());
    assert!(!w.seen.lock().unwrap().iter().any(|s| s.contains("/logs?")));
    assert!(checked(&"x".repeat(MAX_LINE + 1)).is_err());
    assert!(checked(&"\n".repeat(MAX_LINES + 1)).is_err());
    assert!(request("backend=qa&since=NaN").is_err());
}
#[tokio::test(start_paused = true)]
async fn total_deadline_and_cancellation_drop_the_pending_read() {
    let (stats, w) = world(3);
    let observe = observer(stats);
    let task = tokio::spawn(async move { observe(query(true)).await });
    for _ in 0..100 {
        if w.active.load(Ordering::SeqCst) > 0 {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(w.active.load(Ordering::SeqCst), 1);
    tokio::time::advance(DEADLINE + Duration::from_secs(1)).await;
    assert!(task.await.unwrap().is_err());
    assert_eq!(w.active.load(Ordering::SeqCst), 0);
    let (stats, w) = world(3);
    let observe = observer(stats);
    let task = tokio::spawn(async move { observe(query(true)).await });
    for _ in 0..100 {
        if w.active.load(Ordering::SeqCst) > 0 {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(w.active.load(Ordering::SeqCst), 1);
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert_eq!(w.active.load(Ordering::SeqCst), 0);
}

#[test]
fn production_log_allowlist_accepts_only_finite_exact_queries() {
    for tail in [400, 800] {
        assert!(crate::docker::allowed_backend_read(&format!(
            "/containers/qa-id/logs?stderr=1&stdout=1&timestamps=1&follow=0&tail={tail}"
        )));
    }
    for query in [
        "stderr=1&stdout=1&timestamps=1&follow=0&tail=401",
        "stderr=1&stdout=1&timestamps=1&follow=1&tail=400",
        "stderr=1&stdout=1&timestamps=1&follow=0&tail=all",
    ] {
        assert!(!crate::docker::allowed_backend_read(&format!(
            "/containers/qa-id/logs?{query}"
        )));
    }
}
