//! Per-layer arrays (sbstndalton/noevia#1186): a top-level numeric array whose length equals
//! `<arch>.block_count` (at most MAX_PER_LAYER_KEPT) is kept whole; everything else keeps the
//! 8-element sample.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use gguf::{read_raw_bytes, Value, MAX_ARRAY_ELEMENTS_KEPT, MAX_PER_LAYER_KEPT};

fn raw_of(name: &str) -> gguf::Raw {
    let bytes = std::fs::read(common::fixtures_dir().join(format!("{name}.gguf"))).unwrap();
    read_raw_bytes(&bytes).unwrap()
}

fn list_len(raw: &gguf::Raw, key: &str) -> Option<usize> {
    match raw.get(key) {
        Some(Value::List(v)) => Some(v.len()),
        _ => None,
    }
}

fn summary_count(raw: &gguf::Raw, key: &str) -> Option<(u64, usize)> {
    match raw.get(key) {
        Some(Value::ArraySummary { count, sample }) => Some((*count, sample.len())),
        _ => None,
    }
}

#[test]
fn numeric_arrays_of_block_count_length_are_kept_whole() {
    let raw = raw_of("per_layer_hybrid_kv_kept_whole");
    for key in [
        "lfm2.attention.head_count_kv",
        "lfm2.feed_forward_length",
        "lfm2.rope.freq_base",
    ] {
        assert_eq!(list_len(&raw, key), Some(30), "{key}");
    }
    assert_eq!(
        list_len(
            &raw_of("per_layer_floats_kept_whole"),
            "lfm2.attention.scale"
        ),
        Some(30)
    );
    let max = raw_of("per_layer_block_count_max");
    assert_eq!(
        list_len(&max, "lfm2.attention.head_count_kv"),
        Some(usize::try_from(MAX_PER_LAYER_KEPT).unwrap())
    );
}

#[test]
fn every_other_long_array_keeps_the_sample() {
    let k = usize::try_from(MAX_ARRAY_ELEMENTS_KEPT).unwrap();
    let cases = [
        (
            "per_layer_len_mismatch_summarised",
            "lfm2.attention.head_count_kv",
            29,
        ),
        ("per_layer_len_mismatch_summarised", "lfm2.x", 31),
        (
            "per_layer_bool_summarised",
            "lfm2.attention.sliding_window_pattern",
            30,
        ),
        ("per_layer_string_summarised", "lfm2.names", 30),
        (
            "per_layer_before_block_count",
            "lfm2.attention.head_count_kv",
            30,
        ),
        (
            "per_layer_other_arch_block_count",
            "lfm2.attention.head_count_kv",
            30,
        ),
        ("per_layer_no_arch", "lfm2.attention.head_count_kv", 30),
        (
            "per_layer_block_count_float",
            "lfm2.attention.head_count_kv",
            30,
        ),
        (
            "per_layer_block_count_negative",
            "lfm2.attention.head_count_kv",
            30,
        ),
        (
            "per_layer_block_count_over_max",
            "lfm2.attention.head_count_kv",
            4097,
        ),
        (
            "per_layer_cut_off_keeps_sample",
            "lfm2.attention.head_count_kv",
            30,
        ),
    ];
    for (name, key, count) in cases {
        assert_eq!(
            summary_count(&raw_of(name), key),
            Some((count, k)),
            "{name} {key}"
        );
    }
    // nested arrays never take the per-layer path
    assert!(raw_of("per_layer_nested_not_kept").contains_key("_error"));
}

#[test]
fn a_cut_off_per_layer_array_ends_the_parse_without_losing_it() {
    let raw = raw_of("per_layer_cut_off_keeps_sample");
    assert_eq!(
        raw.get("_error"),
        Some(&Value::Str(
            "stopped at KV read: array runs past the end of the data".into()
        ))
    );
}

#[test]
fn a_hostile_header_of_many_per_layer_arrays_stays_cheap() {
    // block_count 4096 and 2000 uint8 arrays of 4096: only the budget is kept whole.
    let mut kvs: Vec<Vec<u8>> = vec![];
    let s = |k: &str| [&(k.len() as u64).to_le_bytes()[..], k.as_bytes()].concat();
    kvs.push(
        [
            s("general.architecture"),
            8u32.to_le_bytes().to_vec(),
            s("lfm2"),
        ]
        .concat(),
    );
    kvs.push(
        [
            s("lfm2.block_count"),
            4u32.to_le_bytes().to_vec(),
            4096u32.to_le_bytes().to_vec(),
        ]
        .concat(),
    );
    for i in 0..2000 {
        kvs.push(
            [
                s(&format!("k{i}")),
                9u32.to_le_bytes().to_vec(),
                0u32.to_le_bytes().to_vec(),
                4096u64.to_le_bytes().to_vec(),
                vec![1u8; 4096],
            ]
            .concat(),
        );
    }
    let mut buf = [
        b"GGUF".to_vec(),
        3u32.to_le_bytes().to_vec(),
        0u64.to_le_bytes().to_vec(),
    ]
    .concat();
    buf.extend((kvs.len() as u64).to_le_bytes());
    for kv in &kvs {
        buf.extend(kv);
    }
    let t = std::time::Instant::now();
    let raw = read_raw_bytes(&buf).unwrap();
    assert!(t.elapsed().as_secs() < 5);
    let kept: u64 = raw
        .values()
        .map(|v| match v {
            Value::List(items) => items.len() as u64,
            _ => 0,
        })
        .sum();
    assert_eq!(kept, gguf::MAX_PER_LAYER_VALUES);
    assert!(!raw.contains_key("_error"));
}
