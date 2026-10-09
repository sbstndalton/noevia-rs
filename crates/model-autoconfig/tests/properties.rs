//! Properties: no input panics or runs unbounded, refusals are typed, and every plan keeps the
//! limits the Python rules guarantee (so a plan that broke one could never be "the same as
//! Python's"). The "never larger than Python" check itself runs in noevia-services, where the
//! Python reference is live; here the fixtures pin agreement (see differential.rs).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use model_autoconfig::{size_plan_json, MAX_INPUT_BYTES};
use model_files::json::{self, Value};
use proptest::prelude::*;

fn int(v: Option<&Value>) -> Option<i128> {
    match v {
        Some(Value::Int(i)) => i.to_string().parse().ok(),
        _ => None,
    }
}

prop_compose! {
    fn request()(
        layers in 1i64..200,
        kv_heads in 1i64..64,
        k_dim in 1i64..1024,
        window in prop::option::of(1i64..8192),
        period in prop::collection::vec((any::<bool>(), 1.0f64..16.0), 0..12),
        hybrid in prop::option::of(2i64..8),
        native in prop_oneof![Just(0i64), Just(-5i64), 1i64..2_000_000],
        model_gb in 0.1f64..400.0,
        moe in prop::option::of(prop_oneof![Just(0.65f64), Just(0.78), Just(0.85), Just(0.9), Just(0.92)]),
        mmproj in prop_oneof![Just(0.0f64), 0.5f64..4.0],
        sessions in 1i64..=8,
        backends in prop::collection::vec((0.5f64..200.0, 1i64..5, prop::collection::vec(0.5f64..80.0, 0..6), 0.0f64..512.0), 1..4),
        preset in prop_oneof![Just(""), Just("fast"), Just("balanced"), Just("long-ctx")],
        tps in prop_oneof![Just(0.0f64), 1.0f64..5000.0],
        budget in 0.5f64..3600.0,
        verified in prop_oneof![Just(0i64), 1i64..500_000],
        cap in 0i64..100_000,
    ) -> (String, i64, i64) {
        let period_json = if window.is_some() && !period.is_empty() {
            format!("[{}]", period.iter().map(|(l, h)| format!("[{l},{h:?}]")).collect::<Vec<_>>().join(","))
        } else {
            "null".to_owned()
        };
        let (win, kswa, vswa, shared) = match (hybrid, window) {
            (None, Some(w)) => (w.to_string(), k_dim.to_string(), k_dim.to_string(), "0".to_owned()),
            _ => ("null".to_owned(), "null".to_owned(), "null".to_owned(), "0".to_owned()),
        };
        let hyb = hybrid.map_or("null".to_owned(), |h| h.to_string());
        let period_json = if hybrid.is_some() { "null".to_owned() } else { period_json };
        let bs = backends.iter().map(|(v, g, c, h)| format!(
            "{{\"vram_gb\":{v:?},\"gpu_count\":{g},\"cards\":[{}],\"host_ram_gb\":{h:?},\"same_as\":0}}",
            c.iter().map(|x| format!("{x:?}")).collect::<Vec<_>>().join(","))).collect::<Vec<_>>();
        // every backend its own name
        let bs = bs.iter().enumerate().map(|(i, b)| b.replace("\"same_as\":0", &format!("\"same_as\":{i}"))).collect::<Vec<_>>().join(",");
        (format!(
            "{{\"shape\":{{\"gemma\":false,\"layers\":{layers},\"kv_heads\":{kv_heads},\"k_dim\":{k_dim},\"v_dim\":{k_dim},\
\"hybrid_interval\":{hyb},\"window\":{win},\"k_swa\":{kswa},\"v_swa\":{vswa},\"shared\":{shared},\"period\":{period_json}}},\
\"layers\":{layers},\"native_ctx\":{native},\"model_gb_raw\":{model_gb:?},\"moe_ratio\":{:?},\"is_moe\":{},\
\"mmproj_vram_gb\":{mmproj:?},\"n_sessions\":{sessions},\"backends\":[{bs}],\"preset\":\"{preset}\",\
\"prompt_tps\":{tps:?},\"prompt_budget_s\":{budget:?},\"verified_ctx\":{verified},\"cache_ram_cap_mib\":{cap}}}",
            moe.unwrap_or(0.0), moe.is_some()), layers, cap)
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn arbitrary_bytes_never_panic(bytes in prop::collection::vec(any::<u8>(), 0..512)) {
        let _ = size_plan_json(&bytes);
    }

    #[test]
    fn plans_keep_the_python_limits((req, layers, cap) in request()) {
        let started = std::time::Instant::now();
        let Ok(text) = size_plan_json(req.as_bytes()) else { return Ok(()); };
        prop_assert!(started.elapsed().as_secs() < 20);
        let plan = json::parse(&text).unwrap();
        // The prompt cache never exceeds the configured cap.
        if let Some(c) = int(plan.get("cache_ram")) {
            prop_assert!(c <= i128::from(cap));
        }
        // A dense preset's ngl is 999 (everything on the GPU) or keeps at least one layer on
        // the GPU and moves at least one off.
        if let Some(n) = int(plan.get("ngl")) {
            prop_assert!(n == 999 || (n >= 1 && n < i128::from(layers)));
        }
        // The final context never exceeds what the fit sweep found, and a cap always binds.
        let ctx = int(plan.get("ctx")).unwrap();
        let estimated = int(plan.get("estimated_ctx")).unwrap();
        let capped = int(plan.get("capped_ctx")).unwrap();
        if let Some(Value::Str(cap_kind)) = plan.get("cap") {
            if !cap_kind.is_empty() {
                prop_assert!(ctx <= capped && capped <= estimated);
            }
        }
        // No recommendation, no context.
        if matches!(plan.get("recommended"), Some(Value::Null)) {
            prop_assert_eq!(ctx, 0);
            prop_assert!(matches!(plan.get("cache_ram"), Some(Value::Null)));
        }
    }

    #[test]
    fn mangled_requests_are_refused_not_panicked((req, _l, _c) in request(), cut in 0usize..2000, byte in any::<u8>()) {
        let mut bytes = req.into_bytes();
        if !bytes.is_empty() {
            let i = cut % bytes.len();
            bytes[i] = byte;
        }
        let _ = size_plan_json(&bytes);
    }
}

#[test]
fn oversized_input_is_refused() {
    let big = vec![b' '; MAX_INPUT_BYTES + 1];
    assert_eq!(size_plan_json(&big).unwrap_err().code(), "input_too_large");
}

#[test]
fn out_of_range_values_are_refused() {
    let base = r#"{"shape":{"gemma":false,"layers":32,"kv_heads":8,"k_dim":128,"v_dim":128,"hybrid_interval":null,"window":null,"k_swa":null,"v_swa":null,"shared":0,"period":null},"layers":32,"native_ctx":131072,"model_gb_raw":4.7,"moe_ratio":0.0,"is_moe":false,"mmproj_vram_gb":0.0,"n_sessions":1,"backends":[{"vram_gb":24.0,"gpu_count":1,"cards":[],"host_ram_gb":64.0,"same_as":0}],"preset":"","prompt_tps":0.0,"prompt_budget_s":120.0,"verified_ctx":0,"cache_ram_cap_mib":1024}"#;
    assert!(size_plan_json(base.as_bytes()).is_ok());
    for (from, to, code) in [
        (
            "\"kv_heads\":8",
            "\"kv_heads\":1267650600228229401496703205376",
            "out_of_range",
        ),
        (
            "\"layers\":32,\"native",
            "\"layers\":5000,\"native",
            "schema",
        ),
        ("\"n_sessions\":1", "\"n_sessions\":9", "out_of_range"),
        ("\"gpu_count\":1", "\"gpu_count\":65", "out_of_range"),
        ("\"prompt_tps\":0.0", "\"prompt_tps\":0", "schema"),
        ("\"preset\":\"\"", "\"preset\":\"pt3\"", "schema"),
        ("\"vram_gb\":24.0", "\"vram_gb\":NaN", "schema"),
        ("\"same_as\":0", "\"same_as\":1", "schema"),
        (
            "\"cache_ram_cap_mib\":1024",
            "\"cache_ram_cap_mib\":1024,\"extra\":1",
            "schema",
        ),
    ] {
        let req = base.replacen(from, to, 1);
        assert_ne!(req, base, "{from}");
        assert_eq!(
            size_plan_json(req.as_bytes()).unwrap_err().code(),
            code,
            "{to}"
        );
    }
}

#[test]
fn work_is_bounded() {
    // 4096 layers on 64 cards that pass the pooled budget but never the per-card one, so every
    // context searches every offload level with a per-card pass each: Python would grind
    // through that for minutes; the port refuses within its work budget instead.
    let period = vec!["[false,1.0]"; 4096].join(",");
    let cards = vec!["2.0"; 64].join(",");
    let req = format!(
        r#"{{"shape":{{"gemma":false,"layers":4096,"kv_heads":8,"k_dim":128,"v_dim":128,"hybrid_interval":null,"window":512,"k_swa":128,"v_swa":128,"shared":0,"period":[{period}]}},"layers":4096,"native_ctx":0,"model_gb_raw":400.0,"moe_ratio":0.92,"is_moe":true,"mmproj_vram_gb":0.0,"n_sessions":1,"backends":[{{"vram_gb":100000.0,"gpu_count":64,"cards":[{cards}],"host_ram_gb":64.0,"same_as":0}}],"preset":"","prompt_tps":0.0,"prompt_budget_s":120.0,"verified_ctx":0,"cache_ram_cap_mib":1024}}"#
    );
    let started = std::time::Instant::now();
    let got = size_plan_json(req.as_bytes());
    assert!(started.elapsed().as_secs() < 30);
    assert_eq!(got.unwrap_err().code(), "work_limit");
}
