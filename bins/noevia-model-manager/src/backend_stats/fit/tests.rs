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
            legacy_base: "http://127.0.0.1:9".into(),
            token: String::new(),
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
    case["env"]["HOST_RAM_RESERVE_GB"] = V::String("1e3".into());
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
    for bad in ["", "1e3", "inf", "nan", "8.", ".5", "1_0", "0x10", "8 kB"] {
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
