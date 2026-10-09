//! Properties of `check` (input prep and values assembly): no input panics or runs unbounded,
//! refusals are typed, and every answer keeps the guarantees the Python rules make (so an
//! answer that broke one could never be "the same as Python's"). Agreement itself is pinned by
//! check_differential.rs; "never larger than Python" runs in noevia-services, where the Python
//! reference is live.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use model_autoconfig::{check_json, parse_value, Error, MAX_BLOCK_COUNT};
use model_files::json::{self, Value};
use proptest::prelude::*;

fn to_json(v: &Value) -> String {
    match v {
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Int(i) => i.to_string(),
        Value::Float(x) if x.is_nan() => "NaN".into(),
        Value::Float(x) if x.is_infinite() => {
            if *x > 0.0 { "Infinity" } else { "-Infinity" }.into()
        }
        Value::Float(x) => format!("{x:?}"),
        Value::Str(s) => {
            let mut out = String::from("\"");
            for c in s.chars() {
                match c {
                    '"' => out.push_str("\\\""),
                    '\\' => out.push_str("\\\\"),
                    c if c.is_ascii() && !c.is_ascii_control() => out.push(c),
                    c => {
                        let mut buf = [0u16; 2];
                        for u in c.encode_utf16(&mut buf) {
                            out.push_str(&format!("\\u{u:04x}"));
                        }
                    }
                }
            }
            out.push('"');
            out
        }
        Value::Arr(a) => format!("[{}]", a.iter().map(to_json).collect::<Vec<_>>().join(",")),
        Value::Obj(o) => format!(
            "{{{}}}",
            o.iter()
                .map(|(k, v)| format!("{k:?}:{}", to_json(v)))
                .collect::<Vec<_>>()
                .join(",")
        ),
    }
}

fn leaf() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("null".to_owned()),
        Just("true".to_owned()),
        Just("false".to_owned()),
        (-5i64..5000).prop_map(|n| n.to_string()),
        Just("1267650600228229401496703205376".to_owned()),
        (-1e6f64..1e6).prop_map(|x| format!("{x:?}")),
        Just("1e300".to_owned()),
        Just("NaN".to_owned()),
        Just("Infinity".to_owned()),
        prop::sample::select(vec![
            "\"\"",
            "\"8\"",
            "\" 32 \"",
            "\"x\"",
            "\"1_0\"",
            "\"gemma3\"",
            "\"\\u0663\"",
            "\"none\"",
            "\"\\u001c7\""
        ])
        .prop_map(str::to_owned),
    ]
}

/// Arbitrary JSON, nested a few levels, as text.
fn any_json() -> impl Strategy<Value = String> {
    leaf().prop_recursive(3, 24, 6, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..6).prop_map(|v| format!("[{}]", v.join(","))),
            prop::collection::vec(
                (
                    prop::sample::select(vec![
                        "_array",
                        "count",
                        "sample",
                        "name",
                        "vram_gb",
                        "gpu_count",
                        "a"
                    ]),
                    inner
                ),
                0..5
            )
            .prop_map(|kv| format!(
                "{{{}}}",
                kv.iter()
                    .map(|(k, v)| format!("\"{k}\":{v}"))
                    .collect::<Vec<_>>()
                    .join(",")
            )),
        ]
    })
}

fn model() -> impl Strategy<Value = String> {
    (
        prop_oneof![6 => Just("32".to_owned()), 1 => Just("42".to_owned()), 1 => any_json()],
        prop_oneof![6 => Just("32".to_owned()), 1 => Just("16".to_owned()), 1 => any_json()],
        prop_oneof![6 => Just("4096".to_owned()), 1 => any_json()],
        prop_oneof![6 => Just("8".to_owned()), 1 => any_json()],
        prop_oneof![6 => Just("131072".to_owned()), 1 => any_json()],
        prop_oneof![6 => Just("null".to_owned()), 1 => Just("512".to_owned()), 1 => any_json()],
        any_json(),
        any_json(),
        any_json(),
    )
        .prop_map(|(bc, h, e, kv, ctx, sw, swp, ex, fai)| {
            format!(
                "{{\"block_count\":{bc},\"attention_head_count\":{h},\"embedding_length\":{e},\
\"attention_head_count_kv\":{kv},\"context_length\":{ctx},\"sliding_window\":{sw},\
\"sliding_window_pattern\":{swp},\"expert_count\":{ex},\"full_attention_interval\":{fai}}}"
            )
        })
}

fn backends() -> impl Strategy<Value = String> {
    prop_oneof![
        6 => prop::collection::vec(
            (prop_oneof![6 => Just("\"a\"".to_owned()), 1 => Just("\"b\"".to_owned()), 1 => any_json()],
             prop_oneof![6 => Just("24.0".to_owned()), 1 => Just("0".to_owned()), 1 => any_json()],
             prop_oneof![6 => Just("1".to_owned()), 1 => Just("2".to_owned()), 1 => any_json()],
             prop_oneof![6 => Just("[]".to_owned()), 1 => any_json()]),
            0..4,
        )
        .prop_map(|bs| format!("[{}]", bs.iter().map(|(n, v, g, c)| format!(
            "{{\"name\":{n},\"vram_gb\":{v},\"gpu_count\":{g},\"card_vram_gb\":{c},\"host_ram_gb\":64}}"))
            .collect::<Vec<_>>().join(","))),
        1 => any_json(),
    ]
}

fn features() -> impl Strategy<Value = String> {
    prop_oneof![
        3 => Just("{}".to_owned()),
        3 => prop::collection::vec((prop::sample::select(vec!["accepts_enable_thinking", "accepts_reasoning_effort",
                                    "uses_think_tags", "uses_channel_thought", "accepts_preserve_thinking"]), leaf()), 0..5)
            .prop_map(|kv| format!("{{{}}}", kv.iter().map(|(k, v)| format!("\"{k}\":{v}")).collect::<Vec<_>>().join(","))),
        1 => any_json(),
    ]
}

fn rope() -> impl Strategy<Value = String> {
    prop_oneof![
        3 => Just("{}".to_owned()),
        3 => (any_json(), any_json(), any_json()).prop_map(|(a, t, f)| format!(
            "{{\"arch\":{a},\"rope_scaling_type\":{t},\"rope_scaling_factor\":{f}}}")),
        1 => any_json(),
    ]
}

prop_compose! {
    fn prep_request()(
        ns in prop_oneof![6 => Just("1".to_owned()), 1 => any_json()],
        arch in prop_oneof![3 => Just("\"llama\"".to_owned()), 3 => Just("\"gemma4\"".to_owned()), 1 => any_json()],
        m in prop_oneof![6 => model(), 1 => any_json()],
        size in prop_oneof![6 => Just("5046586572".to_owned()), 1 => any_json()],
        bs in backends(),
        has in any::<bool>(),
        mm in 0.0f64..3.0,
    ) -> String {
        format!("{{\"prep\":{{\"n_sessions\":{ns},\"arch\":{arch},\"model\":{m},\"file_size\":{size},\
\"backends\":{bs},\"projector\":{{\"has_mmproj\":{has},\"mmproj_gb\":{mm:?},\"mtp_gb\":0.0}}}}}}")
    }
}

prop_compose! {
    fn values_request()(
        n in 1i64..=8,
        initial in prop_oneof![Just(0i64), 4096i64..1_000_000],
        ctx in 4096i64..2_000_000,
        sized in any::<bool>(),
        fit in any::<bool>(),
        ngl in prop::option::of(0i64..999),
        cache in prop::option::of(0i64..100_000),
        has_mmproj in any::<bool>(),
        ub in prop_oneof![Just(String::new()), (0i64..100_000).prop_map(|n| n.to_string()), Just("x".to_owned())],
        b in prop_oneof![Just(String::new()), (0i64..100_000).prop_map(|n| n.to_string())],
        features in features(),
        rope in rope(),
        native in -5i64..300_000,
        gpus in prop::option::of(1i64..5),
        spec in prop::collection::vec((prop::sample::select(vec!["spec-type", "ngl", "batch-size", "tensor-split"]),
                                       prop::sample::select(vec!["", "4", "on"])), 0..4),
    ) -> (String, i64, i64, bool, bool, bool) {
        let opt = |v: Option<i64>| v.map_or("null".to_owned(), |n| n.to_string());
        let spec = spec.iter().map(|(k, v)| format!("[\"{k}\",\"{v}\"]")).collect::<Vec<_>>().join(",");
        let ctx = if sized { ctx } else { initial };
        (format!("{{\"values\":{{\"model_rel\":\"/models/m.gguf\",\"n_sessions\":{n},\"chat_template\":true,\
\"features\":{features},\"section\":\"m\",\"vision\":true,\"current\":{{\"ubatch-size\":\"{ub}\",\"batch-size\":\"{b}\"}},\
\"mmproj_rel\":\"\",\"has_mmproj\":{has_mmproj},\"spec\":[{spec}],\"plan\":{{\"initial_ctx\":{initial},\"sized\":{sized},\
\"ctx\":{ctx},\"ngl\":{},\"fit\":{fit},\"cache_ram\":{}}},\"rope\":{rope},\"native_ctx\":{native},\
\"rec_gpu_count\":{}}}}}", opt(ngl), opt(cache), opt(gpus)), n, ctx, sized, fit, has_mmproj)
    }
}

fn int(v: Option<&Value>) -> Option<i128> {
    match v {
        Some(Value::Int(i)) => i.to_string().parse().ok(),
        _ => None,
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 3000, ..ProptestConfig::default() })]

    #[test]
    fn prep_never_panics_and_answers_keep_python_guarantees(text in prep_request()) {
        match check_json(text.as_bytes()) {
            Err(e) => prop_assert!(!e.code().is_empty()),
            Ok(out) => {
                let v = json::parse(&out).expect("answer is JSON");
                let p = v.get("prep").expect("prep part");
                if p.get("refuse") == Some(&Value::Null) {
                    let layers = int(p.get("layers")).unwrap();
                    prop_assert!(0 < layers && layers <= MAX_BLOCK_COUNT);
                    let Some(Value::Arr(sized)) = p.get("sized") else { panic!("sized") };
                    let Some(Value::Arr(bs)) = p.get("backends") else { panic!("backends") };
                    prop_assert!(!sized.is_empty() && sized.len() == bs.len());
                    for (i, b) in bs.iter().enumerate() {
                        let same = int(b.get("same_as")).unwrap() as usize;
                        prop_assert!(same <= i);
                        prop_assert_eq!(int(bs[same].get("same_as")).unwrap() as usize, same);
                        prop_assert!(int(b.get("gpu_count")).unwrap() >= 1);
                    }
                    // The prep feeds the size core: its request must be well formed (or past a cap).
                    let req = format!("{{\"shape\":{},\"layers\":{layers},\"native_ctx\":{},\"model_gb_raw\":{},\
\"moe_ratio\":{},\"is_moe\":{},\"mmproj_vram_gb\":{},\"n_sessions\":{},\"backends\":{},\"preset\":\"\",\
\"prompt_tps\":0.0,\"prompt_budget_s\":120.0,\"verified_ctx\":0,\"cache_ram_cap_mib\":1024}}",
                        to_json(p.get("shape").unwrap()), to_json(p.get("native_ctx").unwrap()),
                        to_json(p.get("model_gb_raw").unwrap()), to_json(p.get("moe_ratio").unwrap()),
                        to_json(p.get("is_moe").unwrap()), to_json(p.get("mmproj_vram_gb").unwrap()),
                        to_json(p.get("n_sessions").unwrap()), to_json(p.get("backends").unwrap()));
                    match parse_value(&json::parse(&req).expect("request JSON")) {
                        Ok(_) | Err(Error::OutOfRange(_)) => {}
                        Err(e) => prop_assert!(false, "prep gave a request the size core rejects: {e} {req}"),
                    }
                }
            }
        }
    }

    #[test]
    fn values_never_panic_and_keep_python_guarantees(
        (text, n, ctx, sized, fit, has_mmproj) in values_request()
    ) {
        match check_json(text.as_bytes()) {
            Err(Error::Python(kind)) => prop_assert_eq!(kind, "AttributeError"), // features or rope not a dict
            Err(e) => prop_assert!(matches!(e, Error::Unsupported(_) | Error::OutOfRange(_)), "{}", e),
            Ok(out) => {
                let v = json::parse(&out).expect("answer is JSON");
                let Some(Value::Arr(pairs)) = v.get("values") else { panic!("values") };
                let mut keys = Vec::new();
                let get = |k: &str| pairs.iter().find_map(|p| match p {
                    Value::Arr(kv) if kv.first() == Some(&Value::Str(k.to_owned())) => match kv.get(1) {
                        Some(Value::Str(s)) => Some(s.clone()),
                        _ => None,
                    },
                    _ => None,
                });
                for p in pairs {
                    let Value::Arr(kv) = p else { panic!("pair") };
                    prop_assert_eq!(kv.len(), 2);
                    keys.push(kv[0].clone());
                }
                let mut unique = keys.iter().map(to_json).collect::<Vec<_>>();
                unique.sort();
                unique.dedup();
                prop_assert_eq!(keys.len(), unique.len(), "a key twice");
                prop_assert_eq!(get("parallel"), Some(n.to_string()));
                if sized || ctx > 0 {
                    prop_assert_eq!(get("ctx-size"), Some((i128::from(ctx) * i128::from(n)).to_string()));
                }
                if sized && fit {
                    for k in ["ngl", "cpu-moe", "n-cpu-moe", "tensor-split"] {
                        prop_assert!(get(k).is_none());
                    }
                    prop_assert_eq!(get("fit"), Some("on".to_owned()));
                }
                if has_mmproj {
                    prop_assert_eq!(get("image-max-tokens"), Some("1024".to_owned()));
                    let ub: i128 = get("ubatch-size").unwrap().parse().unwrap();
                    prop_assert!(ub >= 1024, "ubatch below the image bound");
                    if let Some(b) = get("batch-size") {
                        prop_assert!(b.parse::<i128>().map_or(true, |b| b >= 1024), "batch below ubatch bound");
                    }
                }
            }
        }
    }

    #[test]
    fn arbitrary_envelopes_never_panic(text in any_json()) {
        for wrapped in [text.clone(), format!("{{\"prep\":{text}}}"), format!("{{\"values\":{text}}}"),
                        format!("{{\"size\":{text}}}")] {
            let _ = check_json(wrapped.as_bytes());
        }
    }
}

/// The strategies above reach the answers, not only the refusals.
#[test]
fn the_strategies_reach_answers() {
    use proptest::strategy::ValueTree;
    use proptest::test_runner::TestRunner;
    let mut runner = TestRunner::deterministic();
    let (mut prep_ok, mut values_ok) = (0, 0);
    for _ in 0..2000 {
        let text = prep_request().new_tree(&mut runner).unwrap().current();
        if let Ok(out) = check_json(text.as_bytes()) {
            prep_ok += usize::from(out.contains("\"refuse\":null"));
        }
        let (text, ..) = values_request().new_tree(&mut runner).unwrap().current();
        values_ok += usize::from(check_json(text.as_bytes()).is_ok());
    }
    assert!(
        prep_ok > 150 && values_ok > 1000,
        "prep {prep_ok}/2000, values {values_ok}/2000"
    );
}

#[test]
fn huge_patterns_are_refused_by_the_work_budget_not_run() {
    // A long sample that never repeats: _period_of is quadratic, the budget bounds it.
    let sample: Vec<String> = (0..60_000).map(|i| i.to_string()).collect();
    let text = format!("{{\"prep\":{{\"n_sessions\":1,\"arch\":\"gemma4\",\"model\":{{\"block_count\":42,\
\"attention_head_count\":16,\"embedding_length\":4096,\"attention_head_count_kv\":8,\"sliding_window\":512,\
\"sliding_window_pattern\":[{}]}},\"file_size\":5000000000,\"backends\":[{{\"name\":\"a\",\"vram_gb\":24.0}}],\
\"projector\":{{\"has_mmproj\":false,\"mmproj_gb\":0.0,\"mtp_gb\":0.0}}}}}}", sample.join(","));
    let t0 = std::time::Instant::now();
    assert_eq!(check_json(text.as_bytes()), Err(Error::WorkLimit));
    assert!(t0.elapsed().as_secs() < 5, "{:?}", t0.elapsed());
}

#[test]
fn spec_entries_are_capped() {
    let values = |n: usize| {
        let spec = (0..n)
            .map(|i| format!("[\"spec-k{i}\",\"1\"]"))
            .collect::<Vec<_>>()
            .join(",");
        format!("{{\"values\":{{\"model_rel\":\"\",\"n_sessions\":1,\"chat_template\":true,\"features\":null,\
\"section\":\"m\",\"vision\":true,\"current\":{{}},\"mmproj_rel\":\"\",\"has_mmproj\":false,\"spec\":[{spec}],\
\"plan\":{{\"initial_ctx\":4096,\"sized\":false,\"ctx\":4096,\"ngl\":null,\"fit\":false,\"cache_ram\":null}},\
\"rope\":{{}},\"native_ctx\":8192,\"rec_gpu_count\":null}}}}")
    };
    assert!(check_json(values(model_autoconfig::values::MAX_SPEC).as_bytes()).is_ok());
    assert_eq!(
        check_json(values(model_autoconfig::values::MAX_SPEC + 1).as_bytes()),
        Err(Error::OutOfRange("spec"))
    );
    // A crafted near-4 MiB request is refused at once, not after a quadratic search.
    let t0 = std::time::Instant::now();
    assert_eq!(
        check_json(values(150_000).as_bytes()),
        Err(Error::OutOfRange("spec"))
    );
    assert!(t0.elapsed().as_secs() < 2, "{:?}", t0.elapsed());
}
