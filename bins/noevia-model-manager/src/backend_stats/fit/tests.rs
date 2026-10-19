#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
//! The actual Python differential: every case of `tests/fixtures/fit-python.v1.json` (recorded from
//! the services app by `tools/gen-fit-oracle.py`) is replayed against the same Docker world, the
//! same backend cache (seeded as the child's `hw._CACHE` held it) and the same meminfo. The
//! inventory, every size's chips (both renderings), the descriptors and the search budget must be
//! Python's, byte for byte. Every name is synthetic.
use super::super::tests::World;
use super::*;
use crate::backend_stats::{gpu_vram_map, Config};
use serde_json::Value as V;
use std::sync::{Arc, Mutex};

pub(crate) fn fixture() -> V {
    serde_json::from_str(include_str!("../../../tests/fixtures/fit-python.v1.json")).unwrap()
}

fn configured(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(|n| model_autoconfig::pyval::py_strip(n).to_owned())
        .filter(|n| !n.is_empty())
        .collect()
}

pub(crate) fn stats_for(case: &V, dir: &Path) -> BackendStats {
    let env = &case["env"];
    let meminfo = dir.join("meminfo");
    if let Some(text) = case["meminfo"].as_str() {
        std::fs::write(&meminfo, text).unwrap();
    }
    let stats = BackendStats::with_clock(
        Config {
            configured: configured(env["LLAMA_CONTAINERS"].as_str().unwrap_or("")),
            gpu_vram: gpu_vram_map(env["GPU_VRAM"].as_str().unwrap_or("")).unwrap(),
            default_port: Some(8080),
            drm_root: dir.join("drm"),
            meminfo,
            ram_reserve_gb: reserve_gb(env["HOST_RAM_RESERVE_GB"].as_str()),
        },
        Arc::new(World(Mutex::new(
            (!case["docker"].is_null()).then(|| case["docker"].clone()),
        ))),
        Arc::new(|| 1.0),
    )
    .unwrap();
    stats
        .seed_cache_json(case["cache"].as_str().unwrap())
        .unwrap();
    stats
}

#[tokio::test]
async fn the_fit_primitives_are_pythons() {
    let fx = fixture();
    let cases = fx["cases"].as_array().unwrap();
    assert!(cases.len() >= 12);
    for case in cases {
        let id = case["id"].as_str().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let stats = stats_for(case, dir.path());
        let inv = stats.inventory().await.expect(id);
        let want = &case["inventory"];
        let gpu: Vec<(String, String)> = inv
            .gpu
            .iter()
            .map(|(n, v)| (n.clone(), gguf::py_float_repr(*v)))
            .collect();
        let want_gpu: Vec<(String, String)> = want["gpu"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| {
                (
                    p[0].as_str().unwrap().to_owned(),
                    p[1].as_str().unwrap().to_owned(),
                )
            })
            .collect();
        assert_eq!(gpu, want_gpu, "{id}: fit backends");
        let want_cpu: Vec<String> = want["cpu"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n.as_str().unwrap().to_owned())
            .collect();
        assert_eq!(inv.cpu, want_cpu, "{id}: cpu backends");
        assert_eq!(
            gguf::py_float_repr(inv.ram),
            want["ram"].as_str().unwrap(),
            "{id}: usable RAM"
        );
        for chip in case["chips"].as_array().unwrap() {
            let size: u128 = chip["bytes"].as_str().unwrap().parse().unwrap();
            let text = |mode| chips(size, &inv, mode).unwrap_or_else(|_| "500".into());
            assert_eq!(
                text(Floats::Raise),
                chip["detail"].as_str().unwrap(),
                "{id}: chips of {size} (detail)"
            );
            assert_eq!(
                text(Floats::Null),
                chip["route"].as_str().unwrap(),
                "{id}: chips of {size} (route)"
            );
        }
        let descriptors = stats.descriptors().await.unwrap();
        let mut out = String::from("[");
        for (i, d) in descriptors.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            render_descriptor(&mut out, d, Floats::Raise).unwrap();
        }
        out.push(']');
        assert_eq!(
            out,
            case["descriptors"].as_str().unwrap(),
            "{id}: descriptors"
        );
        assert_eq!(
            gguf::py_float_repr(budget(&descriptors)),
            case["budget"].as_str().unwrap(),
            "{id}: budget"
        );
    }
}

#[tokio::test]
async fn an_unreadable_reserve_refuses_the_inventory() {
    let fx = fixture();
    let case = &fx["cases"][0];
    let dir = tempfile::tempdir().unwrap();
    let mut case = case.clone();
    case["env"]["HOST_RAM_RESERVE_GB"] = V::String("not-a-float".into());
    let stats = stats_for(&case, dir.path());
    assert_eq!(stats.usable_ram(), None);
    assert!(stats.inventory().await.is_none());
}

#[test]
fn every_accepted_reserve_is_pydantics_reading() {
    let fx = fixture();
    let forms = fx["reserve_forms"].as_object().unwrap();
    assert!(forms.len() >= 18);
    let mut accepted = 0;
    for (form, python) in forms {
        if let Some(native) = reserve_gb(Some(form)) {
            accepted += 1;
            assert_eq!(
                gguf::py_float_repr(native),
                python.as_str().unwrap(),
                "{form:?}"
            );
        }
    }
    assert!(accepted >= 8);
}

#[test]
fn reserve_and_ram_follow_python() {
    assert_eq!(reserve_gb(None), Some(DEFAULT_RESERVE_GB));
    assert_eq!(reserve_gb(Some("8")), Some(8.0));
    assert_eq!(reserve_gb(Some(" +8.25\n")), Some(8.25));
    for bad in ["", "inf", "nan", "0x10", "8 kB"] {
        assert_eq!(reserve_gb(Some(bad)), None, "{bad:?}");
    }
    assert_eq!(usable_ram_gb(0.0, -10.0), 0.0);
    assert_eq!(usable_ram_gb(16.0, 32.0), 0.0);
    assert_eq!(usable_ram_gb(62.5, 16.0), 46.5);
    assert_eq!(usable_ram_gb(8.0, 0.05), 8.0);
}

#[test]
fn round_text_is_pythons_round() {
    assert_eq!(round_text(12.5).unwrap(), "12");
    assert_eq!(round_text(37.5).unwrap(), "38");
    assert_eq!(round_text(-0.4).unwrap(), "0");
    assert_eq!(round_text(1e30).unwrap(), "1000000000000000019884624838656");
    assert_eq!(round_text(f64::INFINITY), Err(Fail::Raised));
    assert_eq!(round_text(f64::NAN), Err(Fail::Raised));
}

#[test]
fn non_finite_floats_follow_each_rendering() {
    let inv = Inventory {
        gpu: vec![("llama-a".into(), f64::INFINITY)],
        cpu: vec![],
        ram: 0.0,
    };
    assert_eq!(chips(0, &inv, Floats::Raise), Err(Fail::Raised));
    assert_eq!(
        chips(0, &inv, Floats::Null).unwrap(),
        "[{\"name\":\"llama-a\",\"vram_gb\":null,\"verdict\":\"fits\",\"ratio_pct\":0}]"
    );
}

/// A real Unix HTTP peer that accepts the native Docker request but never answers it.
#[cfg(feature = "native-section-autoconfig")]
pub(crate) fn hung_stats(
    dir: &Path,
) -> (
    BackendStats,
    Arc<std::sync::atomic::AtomicUsize>,
    tokio::task::JoinHandle<()>,
) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::AsyncReadExt;
    let socket = dir.join("qa-docker.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    let server = tokio::spawn(async move {
        let mut peers = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                connection = listener.accept() => {
                    let (mut stream, _) = connection.unwrap();
                    seen.fetch_add(1, Ordering::Relaxed);
                    peers.spawn(async move {
                        let mut bytes = [0; 1024];
                        while matches!(stream.read(&mut bytes).await, Ok(n) if n > 0) {}
                    });
                }
                _ = peers.join_next(), if !peers.is_empty() => {}
            }
        }
    });
    let mut stats = stats_for(&fixture()["repo"][0], dir);
    stats.docker = Arc::new(crate::docker::UnixDocker { socket });
    (stats, calls, server)
}

#[cfg(all(feature = "native-search", feature = "native-section-autoconfig"))]
#[tokio::test]
async fn authoritative_repository_timeout_retains_configured_cached_capacity_without_negative_cache(
) {
    use std::sync::atomic::Ordering;
    let dir = tempfile::tempdir().unwrap();
    let (mut stats, calls, server) = hung_stats(dir.path());
    stats.config.configured = vec!["llama-cuda".into()];
    // Existing seed holds a measured24GiB backend. Discovery failure must not erase it.
    for expected in 1..=2 {
        let started = std::time::Instant::now();
        let inv = stats.repository_inventory().await.unwrap();
        assert_eq!(inv.gpu, vec![("llama-cuda".to_owned(), 24.0)]);
        assert_eq!(inv.ram, 32.0);
        assert!(started.elapsed() >= std::time::Duration::from_secs(9));
        assert!(started.elapsed() < std::time::Duration::from_secs(15));
        assert_eq!(calls.load(Ordering::Relaxed), expected);
    }
    // Known declaration still wins a cached measurement after a failed discovery.
    stats.config.gpu_vram = vec![("llama-cuda".into(), 12.0)];
    let inv = stats.inventory_from(&[]).unwrap();
    assert_eq!(inv.gpu, vec![("llama-cuda".to_owned(), 12.0)]);
    server.abort();
    let _ = server.await;
}

#[cfg(all(feature = "native-search", feature = "native-section-autoconfig"))]
#[tokio::test]
async fn authoritative_repository_discovery_cancellation_drops_work_without_poisoning_reads() {
    use std::sync::atomic::Ordering;
    let dir = tempfile::tempdir().unwrap();
    let (stats, calls, server) = hung_stats(dir.path());
    let stats = Arc::new(stats);
    let worker = stats.clone();
    let task = tokio::spawn(async move { worker.repository_inventory().await });
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while calls.load(Ordering::Relaxed) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    // No optional negative-cache state was installed by the cancelled authoritative request.
    assert!(stats.fallback_reads.run(async { 7 }).await.is_ok());
    server.abort();
    let _ = server.await;
}

/// Native replay of the actual SDK malformed-daemon source oracle.
#[cfg(all(feature = "native-search", feature = "native-section-autoconfig"))]
pub(crate) fn stats_with_reference_fault(case: &V, fault: &str, dir: &Path) -> BackendStats {
    use crate::backend_stats::{DockerApi, Raw};
    struct FaultDaemon {
        base: World,
        world: V,
        fault: String,
    }
    impl DockerApi for FaultDaemon {
        fn get<'a>(&'a self, path: &'a str) -> crate::Job<'a, Raw> {
            Box::pin(async move {
                let bare = path.split('?').next().unwrap();
                let mut status = 200;
                let body = if bare == "/containers/json" {
                    match self.fault.as_str() {
                        "bounded-json" => Some(serde_json::to_vec(&serde_json::json!({"qa":"q".repeat(super::super::DOCKER_JSON_CAP)})).unwrap()),
                        "bounded-inventory" => Some(serde_json::to_vec(&vec![serde_json::json!({"Id":"qa"}); crate::INVENTORY_CAP + 1]).unwrap()),
                        "api500-numeric-message" => {
                            status = 500;
                            Some(br#"{"message":123}"#.to_vec())
                        }
                        "api404-numeric-message" => {
                            status = 404;
                            Some(br#"{"message":123}"#.to_vec())
                        }
                        "api500-missing-message" => {
                            status = 500;
                            Some(b"{}".to_vec())
                        }
                        "api500-invalid-json" => {
                            status = 500;
                            Some(b"qa".to_vec())
                        }
                        "api500-long-message" => {
                            status = 500;
                            Some(
                                serde_json::to_vec(
                                    &serde_json::json!({"message":"q".repeat(8192)}),
                                )
                                .unwrap(),
                            )
                        }
                        "invalid-json" => Some(b"{bad".to_vec()),
                        "wrong-list-type" => Some(br#"{"wrong":[]}"#.to_vec()),
                        "missing-list-id" => Some(b"[{}]".to_vec()),
                        "list-timeout" => return std::future::pending().await,
                        _ => None,
                    }
                } else if bare.starts_with("/containers/")
                    && bare.ends_with("/json")
                    && ["missing-name", "nonstring-name"].contains(&self.fault.as_str())
                {
                    let mut attrs = self.world["containers"][0].clone();
                    attrs.as_object_mut().unwrap().remove("Name");
                    if self.fault == "nonstring-name" {
                        attrs["Name"] = V::from(123);
                    }
                    Some(serde_json::to_vec(&attrs).unwrap())
                } else if bare.starts_with("/images/") && self.fault == "invalid-tags" {
                    let mut attrs = self.world["images"][0].clone();
                    attrs["RepoTags"] = serde_json::json!([1]);
                    Some(serde_json::to_vec(&attrs).unwrap())
                } else {
                    None
                };
                match body {
                    Some(body) => Ok(Raw {
                        status,
                        reason: "QA".into(),
                        body,
                    }),
                    None => self.base.get(path).await,
                }
            })
        }
    }
    let mut stats = stats_for(case, dir);
    stats.config.configured = vec!["llama-cuda".into()];
    stats.docker = Arc::new(FaultDaemon {
        base: World(Mutex::new(Some(case["docker"].clone()))),
        world: case["docker"].clone(),
        fault: fault.to_owned(),
    });
    stats
}

#[cfg(all(feature = "native-search", feature = "native-section-autoconfig"))]
#[tokio::test(start_paused = true)]
async fn authoritative_discovery_distinguishes_actual_reference_docker_exception_boundaries() {
    let fx: V = serde_json::from_str(include_str!(
        "../../../tests/fixtures/repo-runtime-python.v1.json"
    ))
    .unwrap();
    let shapes = &fx["docker_shapes"];
    assert_eq!(shapes["answers"].as_array().unwrap().len(), 12);
    for answer in shapes["answers"].as_array().unwrap() {
        let dir = tempfile::tempdir().unwrap();
        let stats = stats_with_reference_fault(
            &shapes["case"],
            answer["fault"].as_str().unwrap(),
            dir.path(),
        );
        let result = stats.repository_inventory().await;
        if answer["status"] == 500 {
            assert!(matches!(result, Err(Fail::Raised)), "{}", answer["fault"]);
        } else {
            let inv = result.unwrap();
            assert_eq!(
                inv.gpu,
                vec![("llama-cuda".to_owned(), 24.0)],
                "{}",
                answer["fault"]
            );
        }
    }
}

#[cfg(all(feature = "native-search", feature = "native-section-autoconfig"))]
#[tokio::test]
async fn authoritative_discovery_refuses_native_bounds_without_partial_inventory() {
    let fx = fixture();
    for fault in ["bounded-json", "bounded-inventory"] {
        let dir = tempfile::tempdir().unwrap();
        let stats = stats_with_reference_fault(&fx["repo"][0], fault, dir.path());
        assert!(
            matches!(stats.repository_inventory().await, Err(Fail::Refused)),
            "{fault}"
        );
    }
}
