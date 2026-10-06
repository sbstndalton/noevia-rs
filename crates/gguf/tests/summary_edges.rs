//! Summary behaviour the JSON corpus cannot carry (non-finite floats are not JSON).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use gguf::{summarize, Json, Raw, Value};

fn raw(pairs: &[(&str, Value)]) -> Raw {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect()
}

#[test]
fn infinite_parameter_count_formats_like_python() {
    let r = raw(&[("general.parameter_count", Value::Float(f64::INFINITY))]);
    let s = summarize(&r).unwrap();
    let g = s.get("general").unwrap();
    assert_eq!(g.get("params"), Some(&Json::Str("inf T".into())));
    // Python's json.dumps would print the non-JSON token Infinity; we print null.
    assert!(s.to_string().contains("\"params_raw\":null"));
}

#[test]
fn nan_rope_freq_base_serialises_as_null() {
    let r = raw(&[
        ("general.architecture", Value::Str("llama".into())),
        ("llama.rope.freq_base", Value::Float(f64::NAN)),
    ]);
    let text = summarize(&r).unwrap().to_string();
    assert!(text.contains("\"rope_freq_base\":null"), "{text}");
    serde_json::from_str::<serde_json::Value>(&text).unwrap();
}

#[test]
fn nan_in_an_array_sample_raises_like_python() {
    let sample = vec![Value::Float(f64::NAN); 8];
    let r = raw(&[
        ("general.architecture", Value::Str("llama".into())),
        (
            "llama.block_count",
            Value::ArraySummary { count: 9, sample },
        ),
    ]);
    let e = summarize(&r).unwrap_err();
    assert_eq!(e.0, "cannot convert float NaN to integer");
}

#[test]
fn json_strings_escape_controls() {
    let r = raw(&[("general.name", Value::Str("a\"b\\c\n\u{1}\u{7f}é".into()))]);
    let text = summarize(&r).unwrap().to_string();
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["general"]["name"], "a\"b\\c\n\u{1}\u{7f}é");
}
