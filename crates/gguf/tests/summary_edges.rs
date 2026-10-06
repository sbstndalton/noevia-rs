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
fn infinite_parameter_count_is_none_like_python() {
    // gguf_meta.py maps non-finite floats to None before formatting (noevia#901, #913).
    for f in [f64::INFINITY, f64::NEG_INFINITY, f64::NAN] {
        let r = raw(&[("general.parameter_count", Value::Float(f))]);
        let s = summarize(&r).unwrap();
        let g = s.get("general").unwrap();
        assert_eq!(g.get("params"), Some(&Json::Null), "{f}");
        assert_eq!(g.get("params_raw"), Some(&Json::Null), "{f}");
    }
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
fn non_finite_in_int_fields_is_none_not_an_error() {
    let mut sample = vec![Value::Float(f64::NAN); 7];
    sample.push(Value::Float(4.0));
    let r = raw(&[
        ("general.architecture", Value::Str("llama".into())),
        (
            "llama.block_count",
            Value::ArraySummary { count: 9, sample },
        ),
        ("llama.context_length", Value::Float(f64::INFINITY)),
        (
            "llama.embedding_length",
            Value::List(vec![Value::Float(f64::NEG_INFINITY), Value::Int(3)]),
        ),
        (
            "llama.feed_forward_length",
            Value::ArraySummary {
                count: 9,
                sample: vec![Value::Float(f64::NAN); 8],
            },
        ),
    ]);
    let s = summarize(&r).unwrap();
    let m = s.get("model").unwrap();
    assert_eq!(
        m.get("block_count"),
        Some(&Json::Int(gguf::PyInt::Small(4)))
    );
    assert_eq!(m.get("context_length"), Some(&Json::Null));
    assert_eq!(m.get("embedding_length"), Some(&Json::Null));
    assert_eq!(m.get("feed_forward_length"), Some(&Json::Null));
}

#[test]
fn non_finite_architecture_is_empty() {
    let r = raw(&[
        ("general.architecture", Value::Float(f64::NAN)),
        ("nan.block_count", Value::Int(3)),
    ]);
    let s = summarize(&r).unwrap();
    assert_eq!(s.get("arch"), Some(&Json::Str(String::new())));
    let m = s.get("model").unwrap();
    assert_eq!(m.get("block_count"), Some(&Json::Null));
}

#[test]
fn json_strings_escape_controls() {
    let r = raw(&[("general.name", Value::Str("a\"b\\c\n\u{1}\u{7f}é".into()))]);
    let text = summarize(&r).unwrap().to_string();
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["general"]["name"], "a\"b\\c\n\u{1}\u{7f}é");
}
