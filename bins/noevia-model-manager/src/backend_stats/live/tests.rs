#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
//! The actual Python differential of the live fields: every runtime case of
//! `tests/fixtures/fit-python.v1.json` (the services app's own private calls,
//! `main._model_detail_runtime`, `_models_runtime` and `_overview_runtime`, recorded by
//! `tools/gen-fit-oracle.py`) is replayed against the same Docker world, backend cache, restart
//! errors, meminfo, clock and llama-server answers. The native bodies must be the child's, byte
//! for byte: the routes splice them exactly as they spliced the child's. Every name is synthetic.
use super::super::tests::{llama, stop_llamas, World};
use crate::backend_stats::{fit::reserve_gb, gpu_vram_map, BackendStats, Config};
use serde_json::Value as V;
use std::sync::{Arc, Mutex};

fn fixture() -> V {
    serde_json::from_str(include_str!("../../../tests/fixtures/fit-python.v1.json")).unwrap()
}

fn configured(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(|n| model_autoconfig::pyval::py_strip(n).to_owned())
        .filter(|n| !n.is_empty())
        .collect()
}

#[tokio::test]
async fn the_live_fields_are_the_childs() {
    let fx = fixture();
    let cases = fx["runtime"].as_array().unwrap();
    assert_eq!(cases.len(), 3);
    for case in cases {
        let id = case["id"].as_str().unwrap();
        let env = &case["env"];
        let dir = tempfile::tempdir().unwrap();
        let meminfo = dir.path().join("meminfo");
        std::fs::write(&meminfo, case["meminfo"].as_str().unwrap()).unwrap();
        let now: f64 = case["now"].as_str().unwrap().parse().unwrap();
        let stats = BackendStats::with_clock(
            Config {
                configured: configured(env["LLAMA_CONTAINERS"].as_str().unwrap_or("")),
                gpu_vram: gpu_vram_map(env["GPU_VRAM"].as_str().unwrap_or("")).unwrap(),
                default_port: Some(8080),
                drm_root: dir.path().join("drm"),
                legacy_base: "http://127.0.0.1:9".into(),
                token: String::new(),
                meminfo,
                ram_reserve_gb: reserve_gb(env["HOST_RAM_RESERVE_GB"].as_str()),
            },
            Arc::new(World(Mutex::new(
                (!case["docker"].is_null()).then(|| case["docker"].clone()),
            ))),
            Arc::new(move || now),
        )
        .unwrap();
        stats
            .seed_cache_json(case["cache"].as_str().unwrap())
            .unwrap();
        let errors: Vec<(String, String)> = case["restart_errors"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_owned()))
            .collect();
        stats.seed_restart_errors(&errors);
        let mut servers = Vec::new();
        for (name, port) in case["ports"].as_object().unwrap() {
            let answer = &case["llama"][name];
            servers.push(
                llama(
                    port.as_u64().unwrap() as u32,
                    answer[0].as_u64().unwrap(),
                    answer[1].as_str().unwrap().to_owned(),
                )
                .await,
            );
        }
        for answer in case["answers"].as_array().unwrap() {
            assert_eq!(answer["status"], 200, "{id}");
            let args: Vec<&str> = answer["args"]
                .as_array()
                .unwrap()
                .iter()
                .map(|a| a.as_str().unwrap())
                .collect();
            let body = match answer["op"].as_str().unwrap() {
                "detail" => stats
                    .detail_runtime(args[0].parse().unwrap(), args[1] == "1")
                    .await
                    .unwrap(),
                "models" => {
                    let sizes: Vec<u128> = args.iter().map(|a| a.parse().unwrap()).collect();
                    stats.models_runtime(&sizes).await.unwrap()
                }
                "overview" => stats.overview_runtime(0).await.unwrap(),
                other => panic!("{other}"),
            };
            assert_eq!(
                String::from_utf8(body).unwrap(),
                answer["body"].as_str().unwrap(),
                "{id}: {} {args:?}",
                answer["op"]
            );
        }
        stop_llamas(servers).await;
    }
}

#[tokio::test]
async fn an_unread_reserve_refuses_only_the_chips() {
    let dir = tempfile::tempdir().unwrap();
    let stats = BackendStats::with_clock(
        Config {
            configured: vec!["llama-a".into()],
            gpu_vram: vec![("llama-a".into(), 12.0)],
            default_port: Some(8080),
            drm_root: dir.path().join("drm"),
            legacy_base: "http://127.0.0.1:9".into(),
            token: String::new(),
            meminfo: dir.path().join("meminfo"),
            ram_reserve_gb: None,
        },
        Arc::new(World(Mutex::new(None))),
        Arc::new(|| 1.0),
    )
    .unwrap();
    assert_eq!(
        stats.detail_runtime(1, true).await,
        Err(crate::backend_stats::Fail::Refused)
    );
    assert_eq!(
        stats.models_runtime(&[1]).await,
        Err(crate::backend_stats::Fail::Refused)
    );
    // Without a size to judge the reserve is never read.
    assert_eq!(
        stats.detail_runtime(1, false).await.unwrap(),
        b"{\"loaded\":{},\"fit\":[]}".to_vec()
    );
    assert_eq!(
        stats.models_runtime(&[]).await.unwrap(),
        b"{\"loaded\":{},\"fit\":[]}".to_vec()
    );
    let overview = String::from_utf8(stats.overview_runtime(3).await.unwrap()).unwrap();
    assert!(overview.ends_with("],\"activeDownloads\":3}"), "{overview}");
    assert!(overview.contains("\"status\":\"docker unreachable\""));
}
