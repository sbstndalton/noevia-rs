#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
//! The actual Python differential: every case of `tests/fixtures/telemetry-python.v1.json`
//! (recorded from the services app's `telemetry.py` by `tools/gen-telemetry-oracle.py`) must come
//! out of this port identically, floats by their Python `repr`. Every line is synthetic.
use super::*;
use serde_json::Value as V;

fn fixture() -> V {
    serde_json::from_str(include_str!("../tests/fixtures/telemetry-python.v1.json")).unwrap()
}

fn f(x: f64) -> V {
    V::String(gguf::py_float_repr(x))
}

fn of(x: Option<f64>) -> V {
    x.map_or(V::Null, f)
}

fn pairs(d: &Dict) -> V {
    V::Array(
        d.iter()
            .map(|(k, v)| V::Array(vec![V::String(k.clone()), V::String(v.clone())]))
            .collect(),
    )
}

fn dict(v: &V) -> Dict {
    v.as_array()
        .unwrap()
        .iter()
        .map(|p| {
            (
                p[0].as_str().unwrap().to_owned(),
                p[1].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

fn float(v: &V) -> f64 {
    v.as_str().unwrap().parse().unwrap()
}

fn shown(result: Result<(Vec<Sample>, Vec<ServerConfig>), Error>) -> V {
    match result {
        Err(Error::Raised(_)) => serde_json::json!({"raised": "ValueError"}),
        Err(Error::Unsupported(why)) => panic!("unsupported: {why}"),
        Ok((samples, configs)) => serde_json::json!({
            "samples": samples.iter().map(|s| serde_json::json!({
                "ts": f(s.ts), "instance": s.instance, "task": s.task, "model_path": s.model_path,
                "alias": s.alias, "spec_type": s.spec_type, "prompt_tokens": s.prompt_tokens,
                "prompt_tps": f(s.prompt_tps), "gen_tokens": s.gen_tokens, "gen_tps": f(s.gen_tps),
                "draft_acc": of(s.draft_acc), "draft_len": of(s.draft_len),
            })).collect::<Vec<_>>(),
            "configs": configs.iter().map(|c| serde_json::json!({
                "instance": c.instance, "alias": c.alias, "model_path": c.model_path,
                "argv": pairs(&c.argv), "first_seen": f(c.first_seen),
            })).collect::<Vec<_>>(),
        }),
    }
}

fn strip(case: &V, keys: &[&str]) -> V {
    let mut c = case.clone();
    for k in keys {
        c.as_object_mut().unwrap().remove(*k);
    }
    c
}

#[test]
fn the_pure_parse_is_pythons() {
    let fx = fixture();
    let logs = fx["logs"].as_object().unwrap();
    assert!(logs.len() >= 5);
    for (id, case) in logs {
        let text = case["text"].as_str().unwrap();
        let (a, b) = log_span(text).unwrap();
        assert_eq!(V::Array(vec![f(a), f(b)]), case["span"], "{id}: span");
        assert_eq!(
            shown(parse_log(text, None)),
            strip(case, &["text", "span"]),
            "{id}"
        );
    }
    for (id, case) in fx["raising"].as_object().unwrap() {
        let text = case["text"].as_str().unwrap();
        assert_eq!(shown(parse_log(text, None)), strip(case, &["text"]), "{id}");
    }
}

#[test]
fn the_stored_attribution_is_pythons() {
    let fx = fixture();
    let stored = fx["stored"].as_object().unwrap();
    for (id, case) in fx["attributed"].as_object().unwrap() {
        let mut asked: Vec<String> = Vec::new();
        let mut lookup = |port: &str| -> Result<Vec<Stored>, ()> {
            asked.push(port.to_owned());
            match stored.get(port) {
                None => Ok(Vec::new()),
                Some(V::String(_)) => Err(()),
                Some(rows) => Ok(rows
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|r| Stored {
                        instance: r["instance"].as_str().unwrap().to_owned(),
                        alias: r["alias"].as_str().unwrap().to_owned(),
                        model_path: r["model_path"].as_str().unwrap().to_owned(),
                        argv: dict(&r["argv"]),
                        first_seen: float(&r["first_seen"]),
                    })
                    .collect()),
            }
        };
        let backend = Backend {
            covered_until: float(&case["covered_until"]),
            continuous_since: float(&case["continuous_since"]),
            lookup: &mut lookup,
        };
        let text = case["text"].as_str().unwrap();
        assert_eq!(
            shown(parse_log(text, Some(backend))),
            strip(case, &["text", "covered_until", "continuous_since"]),
            "{id}"
        );
        // Each port is looked up at most once per parse.
        let mut unique = asked.clone();
        unique.dedup();
        assert_eq!(unique, asked, "{id}");
    }
}

#[test]
fn the_helpers_are_pythons() {
    let fx = fixture();
    for case in fx["argv"].as_array().unwrap() {
        let tokens: Vec<&str> = case["tokens"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t.as_str().unwrap())
            .collect();
        assert_eq!(pairs(&argv_to_dict(&tokens)), case["dict"], "{tokens:?}");
    }
    for case in fx["differing"].as_array().unwrap() {
        let configs: Vec<Dict> = case["configs"]
            .as_array()
            .unwrap()
            .iter()
            .map(dict)
            .collect();
        let refs: Vec<&Dict> = configs.iter().collect();
        let keys: Vec<V> = differing_keys(&refs).into_iter().map(V::String).collect();
        assert_eq!(V::Array(keys), case["keys"], "{configs:?}");
    }
    for case in fx["pct"].as_array().unwrap() {
        let values: Vec<f64> = case["values"]
            .as_array()
            .unwrap()
            .iter()
            .map(float)
            .collect();
        assert_eq!(
            f(pct(&values, float(&case["q"]))),
            case["out"],
            "{values:?}"
        );
    }
    for case in fx["keys"].as_array().unwrap() {
        let key = instance_key(case["port"].as_str().unwrap(), float(&case["ts"])).unwrap();
        assert_eq!(key, case["key"].as_str().unwrap());
    }
}

fn rows(v: &V) -> Vec<Row> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|r| {
            let d: std::collections::HashMap<String, V> = r
                .as_array()
                .unwrap()
                .iter()
                .map(|p| (p[0].as_str().unwrap().to_owned(), p[1].clone()))
                .collect();
            let opt = |k: &str| d[k].as_str().map(|s| s.parse::<f64>().unwrap());
            Row {
                instance: d["instance"].as_str().unwrap().to_owned(),
                ts: float(&d["ts"]),
                gen_tps: float(&d["gen_tps"]),
                prompt_tps: float(&d["prompt_tps"]),
                draft_acc: opt("draft_acc"),
                draft_len: opt("draft_len"),
                spec_type: d["spec_type"].as_str().map(str::to_owned),
            }
        })
        .collect()
}

#[test]
fn stats_and_history_are_pythons() {
    let fx = fixture();
    for (id, case) in fx["stats"].as_object().unwrap() {
        let s = stats_for(&rows(&case["rows"]));
        let shown = serde_json::json!({
            "n": s.n, "gen_p50": f(s.gen_p50), "gen_p25": f(s.gen_p25), "gen_p75": f(s.gen_p75),
            "prompt_p50": f(s.prompt_p50), "draft_acc_p50": of(s.draft_acc_p50),
            "draft_len_p50": of(s.draft_len_p50), "draft_n": s.draft_n, "last_ts": f(s.last_ts),
            "spec_types": s.spec_types, "single_config": s.single_config,
            "config_count": s.config_count,
        });
        assert_eq!(shown, case["stats"], "{id}");
    }
    for (id, case) in fx["history"].as_object().unwrap() {
        let argvs: Option<Vec<(String, Dict)>> = case["argvs"].as_array().map(|a| {
            a.iter()
                .map(|p| (p[0].as_str().unwrap().to_owned(), dict(&p[1])))
                .collect()
        });
        let runs = config_history(&rows(&case["rows"]), argvs.as_deref(), 2, 3).unwrap();
        let shown: Vec<V> = runs
            .iter()
            .map(|r| {
                serde_json::json!({
                    "instance": r.instance, "n": r.n, "gen_p50": f(r.gen_p50),
                    "gen_p25": f(r.gen_p25), "gen_p75": f(r.gen_p75), "prompt_p50": f(r.prompt_p50),
                    "draft_acc_p50": of(r.draft_acc_p50), "draft_len_p50": of(r.draft_len_p50),
                    "last_ts": f(r.last_ts), "argv": pairs(&r.argv), "is_current": r.is_current,
                    "diff": pairs(&r.diff), "rel_pct": r.rel_pct,
                })
            })
            .collect();
        assert_eq!(V::Array(shown), case["runs"], "{id}");
    }
}

#[test]
fn shapes_python_reads_differently_are_refused() {
    // A naive timestamp is read in the local zone; a week date is a shape not followed here.
    for line in [
        "2026-10-18T10:00:00 x",
        "2026-10-18T10:00:00+00:00:00 x",
        "2026-10-18T10:00:00\u{0661}Z x",
    ] {
        assert!(
            matches!(parse_ts(line), Err(Error::Unsupported(_))),
            "{line}"
        );
    }
    assert_eq!(parse_ts("no stamp").unwrap(), 0.0);
    // Non-ASCII digits in a captured number, and a task past 64 bits.
    let eval = |task: &str, rate: &str| {
        format!("[1] x slot print_timing: id 0 | task {task} |  eval time = 1 ms / 20 tokens (1 ms per token, {rate} tokens per second)")
    };
    assert!(matches!(
        parse_log(&eval("\u{0661}", "1.0"), None),
        Err(Error::Unsupported(_))
    ));
    assert!(matches!(
        parse_log(&eval("99999999999999999999", "1.0"), None),
        Err(Error::Unsupported(_))
    ));
    assert!(parse_log(&eval("1", "1.0"), None).is_ok());
}

#[test]
fn lines_split_as_python_splits_them() {
    assert_eq!(splitlines(""), Vec::<&str>::new());
    assert_eq!(splitlines("a\n"), vec!["a"]);
    assert_eq!(
        splitlines("a\r\nb\rc\x0bd\x0ce\x1cf\x1dg\x1eh\u{85}i\u{2028}j\u{2029}k"),
        vec!["a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k"]
    );
    assert_eq!(splitlines("\n\n x"), vec!["", "", " x"]);
    assert_eq!(splitlines("a\x1fb"), vec!["a\x1fb"]);
}
