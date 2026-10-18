#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
//! The actual Python differential: every repository case of `tests/fixtures/fit-python.v1.json`
//! (`main._search_repo_runtime` of the services app, recorded by `tools/gen-fit-oracle.py`) is
//! replayed against the same Docker world, backend cache and meminfo. The inventory, the estimate
//! backends and every answer (chips, estimates, reason, native context) must be Python's, byte for
//! byte. Every name and header is synthetic.
use super::*;
use crate::backend_stats::fit::tests::{fixture, stats_for};
use serde_json::Value as V;

/// The child's `JSONResponse({"groups": [...]})` of native groups.
fn render(live: &[Live]) -> String {
    let mut out = String::from("{\"groups\":[");
    for (i, g) in live.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&format!("{{\"fit\":{}", g.fit.get()));
        if let Some(e) = &g.estimates {
            out.push_str(&format!(
                ",\"estimates\":{},\"reason\":{},\"nativeCtx\":{}",
                e.get(),
                json_string(&g.reason),
                g.native_ctx.as_ref().unwrap().get()
            ));
        }
        out.push('}');
    }
    out.push_str("]}");
    out
}

#[tokio::test]
async fn the_repository_estimates_are_pythons() {
    let fx = fixture();
    let cases = fx["repo"].as_array().unwrap();
    assert!(cases.len() >= 5);
    let mut answers = 0;
    let mut fallbacks = 0;
    for case in cases {
        let id = case["id"].as_str().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let stats = stats_for(case, dir.path());
        let inv = stats.inventory().await.expect(id);
        let gpu: Vec<V> = inv
            .gpu
            .iter()
            .map(|(n, v)| serde_json::json!([n, gguf::py_float_repr(*v)]))
            .collect();
        assert_eq!(
            V::Array(gpu),
            case["inventory"]["gpu"],
            "{id}: fit backends"
        );
        let descriptors = stats.estimate_descriptors(&inv);
        let shown: Vec<V> = descriptors
            .iter()
            .map(|d| {
                let cards: Vec<String> = d
                    .card_vram_gb
                    .iter()
                    .map(|c| match c {
                        Num::Int(n) => n.to_string(),
                        Num::Float(f) => gguf::py_float_repr(*f),
                    })
                    .collect();
                assert_eq!(
                    gguf::py_float_repr(d.host_ram_gb),
                    case["host_ram"].as_str().unwrap()
                );
                serde_json::json!({
                    "name": d.name,
                    "vram": gguf::py_float_repr(d.vram_gb),
                    "gpu_count": d.gpu_count as i64,
                    "cards": format!("[{}]", cards.join(", ")),
                })
            })
            .collect();
        assert_eq!(V::Array(shown), case["backends"], "{id}: estimate backends");
        let cap = case["cache_cap"].as_i64().unwrap();
        for (n, ask) in case["asks"].as_array().unwrap().iter().enumerate() {
            assert_eq!(ask["status"], 200, "{id} #{n}");
            let asks: Vec<Ask> = ask["groups"]
                .as_array()
                .unwrap()
                .iter()
                .map(|g| Ask {
                    bytes: g[0].as_str().unwrap().parse().unwrap(),
                    estimate: g[1].as_bool().unwrap(),
                })
                .collect();
            let mm = ask["mmproj"].as_str().map(|b| b.parse().unwrap());
            let summary = ask["summary"].as_str().map(|s| super::summary(s).unwrap());
            let live = groups(&asks, mm, summary.as_ref(), &inv, &descriptors, cap);
            let child: Vec<u64> = case["child"]
                .as_array()
                .unwrap()
                .iter()
                .map(|i| i.as_u64().unwrap())
                .collect();
            if child.contains(&(n as u64)) {
                // Python raised: its reason names the exception type, so the child answers.
                assert!(
                    ask["body"].as_str().unwrap().contains("Autoconfig failed"),
                    "{id} #{n}"
                );
                assert!(live.is_none(), "{id} #{n}: answered natively");
                fallbacks += 1;
                continue;
            }
            let live = live.unwrap_or_else(|| panic!("{id} #{n}: not answered natively"));
            assert_eq!(render(&live), ask["body"].as_str().unwrap(), "{id} #{n}");
            answers += 1;
        }
    }
    assert!(answers >= 20);
    assert!(fallbacks >= 3);
    // The projector override changes the answer in the tight-budget world.
    let tight = cases
        .iter()
        .find(|c| c["id"] == "projector-budget")
        .unwrap();
    let bodies: Vec<&str> = tight["asks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["body"].as_str().unwrap())
        .collect();
    assert!(bodies[0].contains("\"ctx\":32768,\"ctx_h\":\"32K\",\"gpu_layers\":40"));
    assert!(bodies[1].contains("\"key\":\"fast\",\"label\":\"Fast\",\"ctx\":4096"));
    // A real chat template is past the default string cap, and is still read.
    let long = cases.iter().find(|c| c["id"] == "real-shapes").unwrap()["asks"][0]["summary"]
        .as_str()
        .unwrap();
    assert!(model_files::json::parse(long).is_err());
    assert!(super::summary(long).is_some());
}

#[test]
fn values_python_would_render_differently_are_left_to_the_child() {
    let parse = |s: &str| super::summary(s).unwrap();
    assert_eq!(
        native_ctx(&parse(r#"{"model":{"context_length":8192}}"#)).unwrap(),
        "8192"
    );
    assert_eq!(
        native_ctx(&parse(r#"{"model":{"context_length":0}}"#)).unwrap(),
        "0"
    );
    assert_eq!(native_ctx(&parse(r#"{"model":{}}"#)).unwrap(), "0");
    assert_eq!(native_ctx(&parse(r#"{"model":null}"#)).unwrap(), "0");
    assert_eq!(native_ctx(&parse(r#"{"arch":"x"}"#)).unwrap(), "0");
    assert_eq!(
        native_ctx(&parse(r#"{"model":{"context_length":"8K"}}"#)).unwrap(),
        "\"8K\""
    );
    assert_eq!(
        native_ctx(&parse(r#"{"model":{"context_length":2.5}}"#)).unwrap(),
        "2.5"
    );
    // `.get` of a list raises in Python; a nested value is the child's to render.
    assert!(native_ctx(&parse(r#"{"model":[1]}"#)).is_none());
    assert!(native_ctx(&parse(r#"{"model":{"context_length":[1]}}"#)).is_none());
    // No backend: Python's reason, without running the analysis.
    let (e, r) = estimates(
        &parse(r#"{"model":{}}"#),
        1,
        0.0,
        &Value::Arr(Vec::new()),
        1024,
    )
    .unwrap();
    assert_eq!((e.as_str(), r.as_str()), ("[]", NO_GPU));
    // A projector past 2**53 bytes is divided exactly by Python only: the child answers.
    let inv = Inventory {
        gpu: Vec::new(),
        cpu: Vec::new(),
        ram: 0.0,
    };
    let asks = [Ask {
        bytes: 1,
        estimate: false,
    }];
    assert!(groups(&asks, Some(1u128 << 53), None, &inv, &[], 1024).is_none());
    assert!(groups(&asks, Some((1u128 << 53) - 1), None, &inv, &[], 1024).is_some());
}

/// The repository read takes the native answer only while the section autoconfig is native (the
/// cache-ram cap is known), and never asks the child then (its address here is closed).
#[tokio::test]
async fn the_repository_read_answers_natively_only_with_the_cap() {
    use crate::search::{live::Source, Group, Search};
    let fx = fixture();
    let case = &fx["repo"][0];
    let ask = &case["asks"][0];
    let dir = tempfile::tempdir().unwrap();
    let groups: Vec<Group> = ask["groups"]
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
        .map(|(i, g)| Group {
            shard_base: format!("synthetic-{i}.gguf"),
            shards: None,
            bytes: g[0].as_str().unwrap().parse().unwrap(),
            quant: None,
            projector: !g[1].as_bool().unwrap(),
            is_gguf: true,
            files: Vec::new(),
        })
        .collect();
    let summary = ask["summary"].as_str().unwrap();
    let mut unreadable = case.clone();
    unreadable["env"]["HOST_RAM_RESERVE_GB"] = V::String("1e3".into());
    let cap = Some(case["cache_cap"].as_i64().unwrap());
    for (case, cap, native) in [
        (case, None, false),
        (&unreadable, cap, false),
        (case, cap, true),
    ] {
        let search = Search::new(dir.path(), "http://127.0.0.1:9", "").unwrap();
        search.set_live(Source {
            stats: std::sync::Arc::new(stats_for(case, dir.path())),
            models: dir.path().join("models"),
            ini: dir.path().join("models.ini"),
            trusted: Default::default(),
            estimates_cache_cap_mib: cap,
        });
        let live = search.native_repo_runtime(&groups, Some(summary)).await;
        if native {
            assert_eq!(render(&live.unwrap()), ask["body"].as_str().unwrap());
        } else {
            // No cap (the python section autoconfig), or an unreadable HOST_RAM_RESERVE_GB.
            assert!(live.is_none());
        }
    }
}
