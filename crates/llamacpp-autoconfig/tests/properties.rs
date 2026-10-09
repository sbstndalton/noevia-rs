//! Properties of the port over random input: no panics on any bytes; every reply is JSON; a
//! suggestion only offers a qualified context at or below the model's native one whose row fits
//! the budget, a prompt cache at most the cap, and never shrinks when the budget grows; the load
//! gate's footprint of a suggestion's own settings is never above that suggestion's estimate (so
//! the gate counts at least what the suggestion was sized with); large metadata stays linear.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use llamacpp_autoconfig::{call, CTX_CANDIDATES};
use prompt_framing::json::{self, Value};
use proptest::prelude::*;
use std::time::Instant;

fn run(op: u8, wire: &str) -> (u32, String) {
    let mut input = vec![op];
    input.extend(wire.as_bytes());
    call(&input)
}

fn parse(text: &str) -> Value {
    json::parse_utf8(text.as_bytes(), 16).expect("reply is JSON")
}

fn num(v: Option<&Value>) -> Option<f64> {
    match v {
        Some(Value::Num(n)) => Some(*n),
        _ => None,
    }
}

fn str_num(v: Option<&Value>) -> Option<f64> {
    v.and_then(Value::as_str)
        .and_then(|s| String::from_utf16(s).ok())
        .and_then(|s| s.parse::<f64>().ok())
}

fn meta_strategy() -> impl Strategy<Value = String> {
    let n = prop_oneof![
        Just("null".to_owned()),
        (0u32..200).prop_map(|v| v.to_string()),
        prop::sample::select(vec![
            "0", "1", "2", "4", "6", "8", "32", "64", "128", "256", "512", "1024", "4096", "2048",
            "8192", "131072", "262144", "1048576", "-3", "0.5", "1e300", "2.5",
        ])
        .prop_map(str::to_owned),
    ];
    let heads = prop_oneof![
        n.clone(),
        prop::collection::vec(0u8..9, 0..50).prop_map(|v| format!(
            "[{}]",
            v.iter().map(u8::to_string).collect::<Vec<_>>().join(",")
        )),
        Just(r#"["8",true,null,"x"]"#.to_owned()),
    ];
    let pattern = prop_oneof![
        Just("null".to_owned()),
        prop::collection::vec(any::<bool>(), 0..50).prop_map(|v| format!(
            "[{}]",
            v.iter().map(bool::to_string).collect::<Vec<_>>().join(",")
        )),
    ];
    (
        prop::collection::vec(n, 13),
        heads,
        pattern,
        any::<bool>(),
        prop::sample::select(vec!["llama", "qwen35", "gemma4", "nomic-bert", ""]),
    )
        .prop_map(|(f, h, p, chat, arch)| {
            format!(
                r#"{{"arch":"{arch}","contextLength":{},"embeddingLength":{},"blockCount":{},"headCount":{},"keyLength":{},"valueLength":{},"keyLengthSwa":{},"valueLengthSwa":{},"slidingWindow":{},"sharedKvLayers":{},"fullAttentionInterval":{},"expertCount":{},"nextnPredictLayers":{},"headCountKv":{h},"slidingWindowPattern":{p},"hasChatTemplate":{chat}}}"#,
                f[0], f[1], f[2], f[3], f[4], f[5], f[6], f[7], f[8], f[9], f[10], f[11], f[12]
            )
        })
}

/// A realistic chat model: every sizing field set, so suggestions succeed often.
fn chat_meta_strategy() -> impl Strategy<Value = String> {
    (
        prop::sample::select(vec![4096u32, 8192, 32768, 40960, 131072, 262144, 1048576]),
        prop::sample::select(vec![2048u32, 2560, 4096, 5120]),
        8u32..64,
        prop::sample::select(vec![8u32, 16, 32]),
        prop::sample::select(vec![1u32, 2, 4, 8]),
        prop::sample::select(vec!["null", "4", "1"]),
        prop::sample::select(vec!["null", "512", "1024"]),
        prop::sample::select(vec!["null", "1"]),
    )
        .prop_map(|(ctx, emb, layers, heads, kv, fai, sw, nextn)| {
            format!(
                r#"{{"arch":"synthetic","contextLength":{ctx},"embeddingLength":{emb},"blockCount":{layers},"headCount":{heads},"headCountKv":{kv},"fullAttentionInterval":{fai},"slidingWindow":{sw},"nextnPredictLayers":{nextn},"hasChatTemplate":true}}"#
            )
        })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    #[test]
    fn any_bytes_never_panic(op in 0u8..8, body in prop::collection::vec(any::<u8>(), 0..300)) {
        let mut input = vec![op];
        input.extend(body);
        let (status, reply) = call(&input);
        prop_assert!(status <= 1);
        let _ = parse(&reply);
    }

    #[test]
    fn random_metadata_answers_or_refuses(meta in meta_strategy(), op in 1u8..4, budget in 0u32..40, cache in 0u32..5000,
                                          bytes in 0u64..(40u64 << 30), mmproj in 0u64..(2u64 << 30)) {
        let wire = format!(r#"{{"meta":{meta},"modelBytes":{bytes},"mmprojBytes":{mmproj},"budgetGib":{budget},"cacheRamMaxMib":{cache},"current":{{}},"options":{{}}}}"#);
        let (status, reply) = run(op, &wire);
        prop_assert!(status <= 1);
        let v = parse(&reply);
        if status == 1 {
            prop_assert!(reply == r#"{"error":"ambiguous"}"#, "{reply}");
        } else {
            prop_assert!(matches!(v, Value::Obj(_)));
        }
    }

    #[test]
    fn suggestions_fit_and_grow_with_the_budget(meta in chat_meta_strategy(), budget in 2u32..64, extra in 0u32..32,
                                                bytes in (1u64 << 28)..(20u64 << 30), cache in 0u32..4096,
                                                mmproj in prop::sample::select(vec![0u64, 600 << 20])) {
        let ask = |b: u32| {
            let wire = format!(r#"{{"meta":{meta},"modelBytes":{bytes},"mmprojBytes":{mmproj},"budgetGib":{b},"cacheRamMaxMib":{cache}}}"#);
            let (status, reply) = run(1, &wire);
            assert_eq!(status, 0, "{reply}");
            parse(&reply)
        };
        let small = ask(budget);
        let large = ask(budget + extra);
        let m = parse(&meta);
        let native = num(m.get("contextLength")).unwrap();
        if let Some(values) = small.get("values") {
            let ctx = str_num(values.get("ctx-size")).unwrap();
            prop_assert!(CTX_CANDIDATES.contains(&ctx) && ctx <= native);
            // The chosen row is one the budget fits.
            let Some(Value::Arr(rows)) = small.get("rows") else { panic!() };
            let row = rows.iter().find(|r| num(r.get("ctx")) == Some(ctx)).unwrap();
            prop_assert_eq!(row.get("fits"), Some(&Value::Bool(true)));
            prop_assert!(num(small.get("estimateGib")).unwrap() <= f64::from(budget) + 0.005);
            // The prompt cache stays at the cap (256 MiB is the floor).
            let cache_ram = str_num(values.get("cache-ram")).unwrap();
            prop_assert!(cache_ram <= f64::from(cache).max(256.0));
            prop_assert_eq!(str_num(values.get("n-gpu-layers")), Some(999.0));
            // A larger budget never suggests less context.
            let large_ctx = str_num(large.get("values").and_then(|v| v.get("ctx-size"))).unwrap();
            prop_assert!(large_ctx >= ctx);
            // The load gate's footprint of these very settings is never above the suggestion's
            // estimate when the cap is at least the 256 MiB floor.
            if cache >= 256 {
                let Value::Obj(members) = values else { panic!() };
                let opts: Vec<String> = members.iter().map(|(k, v)| format!(
                    "{}:{}", json_str(k), json_str(v.as_str().unwrap()))).collect();
                let wire = format!(r#"{{"meta":{meta},"modelBytes":{bytes},"mmprojBytes":{mmproj},"options":{{{}}}}}"#, opts.join(","));
                let (status, reply) = run(3, &wire);
                prop_assert_eq!(status, 0);
                let fp = parse(&reply);
                let total = num(fp.get("totalGib")).unwrap();
                prop_assert!(total <= num(small.get("estimateGib")).unwrap(), "{} vs {}", reply, num(small.get("estimateGib")).unwrap());
            }
        }
    }
}

fn json_str(units: &[u16]) -> String {
    let mut out = Vec::new();
    json::push_str(&mut out, units);
    String::from_utf8(out).unwrap()
}

#[test]
fn large_metadata_stays_linear() {
    // 200,000-item per-layer arrays (no small period): mode, period and sizing run in linear time.
    let n = 200_000;
    let heads: Vec<String> = (0..n).map(|i| ((i % 7) + 1).to_string()).collect();
    let pattern: Vec<&str> = (0..n)
        .map(|i| if i == n - 1 { "false" } else { "true" })
        .collect();
    let meta = format!(
        r#"{{"arch":"x","hasChatTemplate":true,"contextLength":131072,"embeddingLength":4096,"blockCount":{n},"headCount":32,"headCountKv":[{}],"slidingWindow":512,"slidingWindowPattern":[{}]}}"#,
        heads.join(","),
        pattern.join(",")
    );
    let start = Instant::now();
    // The period (200,000) is past MAX_PATTERN_PERIOD: refused, after a linear scan.
    let (status, reply) = run(
        1,
        &format!(r#"{{"meta":{meta},"modelBytes":1,"budgetGib":14}}"#),
    );
    assert_eq!((status, reply.as_str()), (1, r#"{"error":"ambiguous"}"#));
    // With a period-6 pattern the same size is answered.
    let pattern: Vec<&str> = (0..n)
        .map(|i| if i % 6 == 5 { "false" } else { "true" })
        .collect();
    let meta = format!(
        r#"{{"arch":"x","hasChatTemplate":true,"contextLength":131072,"embeddingLength":4096,"blockCount":{n},"headCount":32,"headCountKv":[{}],"slidingWindow":512,"slidingWindowPattern":[{}]}}"#,
        heads.join(","),
        pattern.join(",")
    );
    for op in 1..=3 {
        let (status, reply) = run(
            op,
            &format!(r#"{{"meta":{meta},"modelBytes":1,"budgetGib":1e9,"options":{{}}}}"#),
        );
        assert_eq!(status, 0, "{reply}");
    }
    assert!(start.elapsed().as_secs() < 20, "{:?}", start.elapsed());
}

#[test]
fn deep_input_is_bounded() {
    let deep = format!(
        r#"{{"meta":{{"arch":"x","headCountKv":{}1{}}},"budgetGib":14}}"#,
        "[".repeat(100_000),
        "]".repeat(100_000)
    );
    let (status, reply) = run(1, &deep);
    assert_eq!((status, reply.as_str()), (1, r#"{"error":"ambiguous"}"#));
}
