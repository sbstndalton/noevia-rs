#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

//! F15: the reranker of project search against noevia-core's own `rerankPool`, `createDecisions` and
//! rerank backends (`generate_rerank_fixtures.cjs`): the request each backend sent (address, key,
//! body; texts as UTF-16 units), how each recorded answer is read, and the pool that comes back.

use project_retrieval::hits::Hit;
use project_retrieval::policy::Backend;
use project_retrieval::rerank::{document, merge, needed, order, parse, request, Bench, Config};
use serde_json::Value;

fn fixture() -> Value {
    let path = format!(
        "{}/tests/fixtures/F15-rerank.json",
        env!("CARGO_MANIFEST_DIR")
    );
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn hits(v: &Value) -> Vec<Hit> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|h| Hit {
            file: h["file"].as_str().unwrap().to_owned(),
            body: h["body"].as_str().unwrap().to_owned(),
            score: h["score"].as_f64().unwrap(),
        })
        .collect()
}

fn config(c: &Value) -> Config {
    Config {
        backend: if c["backend"] == "tei" {
            Backend::Tei
        } else {
            Backend::Llamacpp
        },
        base_url: c["baseUrl"].as_str().unwrap().to_owned(),
        model: c["model"].as_str().map(str::to_owned),
        api_key: c["apiKey"].as_str().map(str::to_owned),
        pool: 12,
        keep: c["keep"].as_u64().unwrap() as usize,
        deadline_ms: c["deadlineMs"].as_u64().unwrap(),
    }
}

/// The JSON body with its text lists as UTF-16 units, as the recorder keeps them.
fn as_recorded(body: &Value) -> Value {
    let mut b = body.clone();
    for key in ["documents", "texts"] {
        if let Some(list) = b.get_mut(key) {
            *list = Value::Array(
                list.as_array()
                    .unwrap()
                    .iter()
                    .map(|s| {
                        Value::Array(
                            s.as_str()
                                .unwrap()
                                .encode_utf16()
                                .map(Value::from)
                                .collect(),
                        )
                    })
                    .collect(),
            );
        }
    }
    b
}

#[test]
fn every_recorded_rerank_matches_node() {
    let f = fixture();
    let mut checked = 0;
    for c in f["cases"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c.get("runs").is_none())
    {
        let name = c["name"].as_str().unwrap();
        let cfg = config(c);
        let pool = hits(&c["pool"]);
        let want = hits(&c["result"]);
        let sent = c["requests"].as_array().unwrap();
        if !needed(&pool) {
            assert!(sent.is_empty(), "{name}");
            assert_eq!(merge(pool, cfg.keep, None), want, "{name}");
            checked += 1;
            continue;
        }
        assert_eq!(sent.len(), 1, "{name}");
        let r = &sent[0];
        let mine = request(&cfg, c["query"].as_str().unwrap(), &pool);
        assert_eq!(mine.url, r["url"].as_str().unwrap(), "{name}");
        assert_eq!(
            mine.authorization.as_deref(),
            r["authorization"].as_str(),
            "{name}"
        );
        assert_eq!(r["redirect"], "error");
        let docs_key = if cfg.backend == Backend::Tei {
            "texts"
        } else {
            "documents"
        };
        for (h, units) in pool.iter().zip(r["body"][docs_key].as_array().unwrap()) {
            let units: Vec<u16> = units
                .as_array()
                .unwrap()
                .iter()
                .map(|u| u.as_u64().unwrap() as u16)
                .collect();
            assert_eq!(document(&h.body), units, "{name}");
        }
        match serde_json::from_str::<Value>(&mine.body) {
            Ok(body) => assert_eq!(as_recorded(&body), r["body"], "{name}"),
            // A label cut inside a surrogate pair: the one lone surrogate is written as JS writes it.
            Err(_) => assert!(mine.body.contains("\\ud83d\""), "{name}: {}", mine.body),
        }
        let answer = &c["answer"];
        let bytes = match &answer["body"] {
            Value::String(s) => s.clone().into_bytes(),
            other => serde_json::to_vec(other).unwrap(),
        };
        let status = answer["status"].as_u64().unwrap() as u16;
        let ranking = parse(cfg.backend, pool.len(), status, &bytes).map(|s| order(&s));
        assert_eq!(merge(pool, cfg.keep, ranking), want, "{name}");
        checked += 1;
    }
    assert_eq!(checked, 26);
}

/// A reranker that never answers: each search falls back at the deadline; the third miss benches it
/// for 60 s, so the fourth search sends nothing.
#[test]
fn deadline_misses_bench_the_reranker_as_node_does() {
    let f = fixture();
    let c = f["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c.get("runs").is_some())
        .unwrap();
    let pool = hits(&c["pool"]);
    let mut bench = Bench::default();
    let now = 1_000_000.0;
    for (i, run) in c["runs"].as_array().unwrap().iter().enumerate() {
        let t = now + i as f64 * 150.0;
        let sends = !bench.benched(t);
        assert_eq!(u64::from(sends), run["sent"].as_u64().unwrap(), "run {i}");
        if sends {
            bench.missed(t);
        }
        assert_eq!(
            merge(pool.clone(), 6, None),
            hits(&run["result"]),
            "run {i}"
        );
    }
    assert!(bench.benched(now + 600.0) && !bench.benched(now + 300.0 + 60_001.0));
    bench.answered();
}
