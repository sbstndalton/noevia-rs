//! Properties of the llamacpp-presets port: no input panics; a value is accepted only where a
//! direct transcription of the JS check (`/^\d+$/` or `/^\d+(\.\d{1,3})?$/` and `Number(v)` within
//! the bounds, as f64, the way V8 compares) accepts it too; a clamped cache-ram is never above the
//! hard maximum; `canonical_option` only ever names a listed field. The JS side of "never accepts
//! what the JS refuses" also runs against the real JS in noevia-core's differential test.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use llamacpp_presets::{
    call, canonical, canonical_option, checked_value, clamp_cache_ram, field, to_number, valid,
    FIELDS,
};
use prompt_framing::js::units;
use proptest::prelude::*;

/// The JS rules, transcribed: (pattern kind, min, max, words).
fn js_valid(name: &str, v: &str) -> bool {
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let integer = |min: f64, max: f64| {
        digits(v) && {
            let n: f64 = v.parse().unwrap();
            n >= min && n <= max
        }
    };
    let decimal = |min: f64, max: f64| {
        let shape = match v.split_once('.') {
            None => digits(v),
            Some((i, f)) => digits(i) && digits(f) && (1..=3).contains(&f.len()),
        };
        shape && {
            let n: f64 = v.parse().unwrap();
            n >= min && n <= max
        }
    };
    const CT: [&str; 9] = [
        "f32", "f16", "bf16", "q8_0", "q4_0", "q4_1", "iq4_nl", "q5_0", "q5_1",
    ];
    match name {
        "ctx-size" => integer(2048.0, 1_048_576.0),
        "parallel" => integer(1.0, 16.0),
        "n-gpu-layers" => integer(0.0, 999.0) || v == "auto" || v == "all",
        "cache-type-k" | "cache-type-v" => CT.contains(&v),
        "flash-attn" => ["on", "off", "auto"].contains(&v),
        "batch-size" | "ubatch-size" => integer(32.0, 8192.0),
        "cache-ram" => integer(0.0, 1_048_576.0),
        "image-max-tokens" => integer(64.0, 16384.0),
        "spec-type" => [
            "none",
            "draft-mtp",
            "ngram-simple",
            "draft-mtp,ngram-simple",
        ]
        .contains(&v),
        "spec-draft-n-max" => integer(1.0, 32.0),
        "spec-draft-p-min" => {
            let (i, f) = v.split_once('.').map_or((v, None), |(i, f)| (i, Some(f)));
            match (i, f) {
                ("0" | "1", None) => true,
                ("0", Some(f)) => (1..=3).contains(&f.len()) && digits(f),
                ("1", Some(f)) => (1..=3).contains(&f.len()) && f.bytes().all(|b| b == b'0'),
                _ => false,
            }
        }
        "temp" => decimal(0.0, 2.0),
        "top-p" | "min-p" => decimal(0.0, 1.0),
        "top-k" => integer(0.0, 100_000.0),
        "repeat-penalty" => decimal(0.0, 3.0),
        _ => panic!("unknown field {name}"),
    }
}

fn value() -> impl Strategy<Value = String> {
    prop_oneof![
        "[0-9]{1,9}",
        "0{0,30}[0-9]{1,8}",
        "[0-9]{1,4}\\.[0-9]{0,5}",
        "[0-9.+\\-eExa-f ]{0,12}",
        "(auto|all|on|off|q8_0|f16|none|draft-mtp|ngram-simple|draft-mtp,ngram-simple)",
        "\\PC{0,8}",
    ]
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 4000, ..ProptestConfig::default() })]

    #[test]
    fn never_accepts_what_the_js_refuses(i in 0..FIELDS.len(), v in value()) {
        let f = &FIELDS[i];
        prop_assert_eq!(valid(f, &units(&v)), js_valid(f.name, &v), "{} = {:?}", f.name, v);
    }

    #[test]
    fn a_checked_value_is_a_listed_field_and_valid(k in "[a-zA-Z_\\-]{0,24}", v in value(), hard in prop::option::of(0u64..5000)) {
        if let Some(out) = checked_value(&units(&k), &units(&v), hard) {
            let f = field(&units(&k)).expect("only field names are accepted");
            let out = String::from_utf16(&out).unwrap();
            prop_assert!(out.is_empty() || js_valid(f.name, &out));
            if f.name != "cache-ram" {
                prop_assert_eq!(out, v);
            }
        }
    }

    #[test]
    fn a_clamped_cache_ram_is_never_above_the_hard_maximum(v in "-?[0-9]{1,30}", hard in 0u64..(1 << 53)) {
        let out = String::from_utf16(&clamp_cache_ram(&units(&v), hard)).unwrap();
        let n: u128 = out.parse().unwrap();
        prop_assert!(n <= u128::from(hard));
        prop_assert!(out == "0" || !out.starts_with('0'));
    }

    #[test]
    fn canonical_names_only_listed_fields(k in "[ \\-]{0,3}[a-zA-Z_\\-]{0,24}[ ]{0,2}", v in prop::option::of(value())) {
        let (f, t) = canonical_option(&units(&k), v.as_deref().map(units).as_deref());
        if let Some(f) = f {
            prop_assert!(FIELDS.iter().any(|g| g.name == f.name));
            prop_assert!(canonical(&units(k.trim().trim_start_matches('-'))).is_some());
        }
        prop_assert_eq!(t.is_some(), v.is_some());
    }

    #[test]
    fn decimal_numbers_parse_as_rust_does(v in "[+\\-]?[0-9]{1,20}(\\.[0-9]{0,20})?([eE][+\\-]?[0-9]{1,3})?") {
        let n = to_number(&units(&v)).unwrap();
        prop_assert_eq!(n.to_bits(), v.parse::<f64>().unwrap().to_bits());
    }

    #[test]
    fn no_input_panics(op in 0u8..6, body in "\\PC{0,64}") {
        let mut input = vec![op];
        input.extend(body.as_bytes());
        let _ = call(&input);
    }

    #[test]
    fn no_well_formed_input_panics(pairs in prop::collection::vec(("\\PC{0,10}", "\\PC{0,10}"), 0..6), hard in prop::option::of(any::<f64>())) {
        let wire = |s: &str| {
            let mut out = String::from("\"");
            for u in s.encode_utf16() {
                if u < 0x20 || u == 0x22 || u == 0x5c || u > 0x7e {
                    out.push_str(&format!("\\u{u:04x}"));
                } else {
                    out.push(char::from(u as u8));
                }
            }
            out.push('"');
            out
        };
        let list: Vec<String> = pairs.iter().map(|(k, v)| format!("[{},{}]", wire(k), wire(v))).collect();
        let hard = hard.filter(|h| h.is_finite()).map_or("null".to_owned(), |h| h.to_string());
        for op in [1u8, 2] {
            let body = if op == 1 { format!("[[{}],{hard}]", list.join(",")) } else { format!("[[{}]]", list.join(",")) };
            let mut input = vec![op];
            input.extend(body.as_bytes());
            let _ = call(&input);
        }
    }
}
