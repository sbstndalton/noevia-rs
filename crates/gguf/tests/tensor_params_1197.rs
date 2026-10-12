//! #1197: a GGUF without general.parameter_count still has a parameter count, the sum of its
//! tensor-info element counts (the same answer as gguf_meta.py's `_param_count`). Synthetic headers.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use gguf::{read_raw_bytes, summarize, Json};

fn s(text: &str) -> Vec<u8> {
    let mut out = (text.len() as u64).to_le_bytes().to_vec();
    out.extend_from_slice(text.as_bytes());
    out
}

fn kv_str(key: &str, value: &str) -> Vec<u8> {
    let mut out = s(key);
    out.extend(8u32.to_le_bytes());
    out.extend(s(value));
    out
}

fn kv_u32(key: &str, value: u32) -> Vec<u8> {
    let mut out = s(key);
    out.extend(4u32.to_le_bytes());
    out.extend(value.to_le_bytes());
    out
}

fn kv_u64(key: &str, value: u64) -> Vec<u8> {
    let mut out = s(key);
    out.extend(10u32.to_le_bytes());
    out.extend(value.to_le_bytes());
    out
}

fn kv_bool(key: &str, value: bool) -> Vec<u8> {
    let mut out = s(key);
    out.extend(7u32.to_le_bytes());
    out.push(u8::from(value));
    out
}

fn info(name: &str, dims: &[u64]) -> Vec<u8> {
    let mut out = s(name);
    out.extend((dims.len() as u32).to_le_bytes());
    for d in dims {
        out.extend(d.to_le_bytes());
    }
    out.extend(0u32.to_le_bytes());
    out.extend(0u64.to_le_bytes());
    out
}

/// A v3 header: `declared` tensors counted up front, `infos` written after the KV pairs.
fn header(kvs: &[Vec<u8>], declared: u64, infos: &[Vec<u8>]) -> Vec<u8> {
    let mut out = b"GGUF".to_vec();
    out.extend(3u32.to_le_bytes());
    out.extend(declared.to_le_bytes());
    out.extend((kvs.len() as u64).to_le_bytes());
    for kv in kvs {
        out.extend(kv);
    }
    for i in infos {
        out.extend(i);
    }
    out
}

fn base() -> Vec<Vec<u8>> {
    vec![
        kv_str("general.architecture", "llama"),
        kv_u32("llama.block_count", 1),
    ]
}

fn general_params(buf: &[u8]) -> (Json, Json) {
    let raw = read_raw_bytes(buf).unwrap();
    let summary = summarize(&raw).unwrap();
    let general = summary.get("general").unwrap();
    (
        general.get("params").unwrap().clone(),
        general.get("params_raw").unwrap().clone(),
    )
}

fn tensors() -> Vec<Vec<u8>> {
    vec![
        info("token_embd.weight", &[4096, 32000]),
        info("blk.0.attn_q.weight", &[4096, 4096]),
        info("output_norm.weight", &[4096]),
    ]
}

const TOTAL: i128 = 4096 * 32000 + 4096 * 4096 + 4096;

#[test]
fn params_are_the_sum_of_tensor_element_counts_when_none_is_declared() {
    let buf = header(&base(), 3, &tensors());
    let (params, raw) = general_params(&buf);
    assert_eq!(params, Json::Str("147.9 M".into()));
    assert_eq!(raw.to_string(), TOTAL.to_string());
}

#[test]
fn a_declared_parameter_count_still_wins() {
    let mut kvs = base();
    kvs.push(kv_u64("general.parameter_count", 7_000_000_000));
    let (params, _) = general_params(&header(&kvs, 3, &tensors()));
    assert_eq!(params, Json::Str("7.0 B".into()));
}

#[test]
fn a_zero_dimension_tensor_is_one_element_and_no_tensors_gives_no_count() {
    let (params, _) = general_params(&header(&base(), 1, &[info("scalar", &[])]));
    assert_eq!(params, Json::Str("1".into()));
    let (params, raw) = general_params(&header(&base(), 0, &[]));
    assert_eq!((params, raw), (Json::Null, Json::Null));
}

#[test]
fn a_truncated_tensor_block_gives_no_count() {
    let buf = header(&base(), 3, &tensors()[..1]);
    let (params, raw) = general_params(&buf);
    assert_eq!((params, raw), (Json::Null, Json::Null));
}

#[test]
fn hostile_tensor_blocks_give_no_count() {
    let (params, _) = general_params(&header(&base(), 2u64.pow(40), &[]));
    assert_eq!(params, Json::Null, "too many tensors");
    let (params, _) = general_params(&header(&base(), 1, &[info("odd", &[1, 1, 1, 1, 1])]));
    assert_eq!(params, Json::Null, "more than four dimensions");
    let (params, _) = general_params(&header(&base(), 1, &[info("wide", &[2u64.pow(53)])]));
    assert_eq!(params, Json::Null, "a dimension past 2^53-1");
    let (params, _) = general_params(&header(&base(), 1, &[info("huge", &[2u64.pow(40); 3])]));
    assert_eq!(params, Json::Null, "element overflow");
    let many = vec![info("a", &[u64::from(u32::MAX), u64::from(u32::MAX)]); 3];
    let (params, _) = general_params(&header(&base(), 3, &many));
    assert_eq!(
        params,
        Json::Null,
        "total past u64 would overflow the count"
    );
}

#[test]
fn a_boolean_declared_count_is_not_a_count() {
    let mut kvs = base();
    kvs.push(kv_bool("general.parameter_count", true));
    let (params, _) = general_params(&header(&kvs, 3, &tensors()));
    assert_eq!(params, Json::Str("147.9 M".into()));
}

/// The wasm-side reader (node.rs, the JS mirror): the same count, placed first as the JS object does.
#[test]
fn the_node_reader_gives_the_same_count_first_in_the_summary() {
    use gguf::node::{summary, Fault, Segment};
    let buf = header(&base(), 3, &tensors());
    let text = summary(
        buf.len() as u64,
        &[Segment {
            off: 0,
            bytes: &buf,
        }],
    )
    .unwrap();
    assert!(
        text.starts_with("{\"parameterCount\":147853312,\"arch\":\"llama\""),
        "{text}"
    );
    // The tensor-info bytes are not in the segments: the host must be asked for them, not answered without them.
    let kv_end = buf.len() - tensors().iter().map(Vec::len).sum::<usize>();
    let short = summary(
        buf.len() as u64,
        &[Segment {
            off: 0,
            bytes: &buf[..kv_end],
        }],
    );
    assert!(matches!(short, Err(Fault::Need { .. })), "{short:?}");
}

#[test]
fn the_node_reader_gives_no_count_for_a_cut_short_block() {
    use gguf::node::{summary, Segment};
    let buf = header(&base(), 3, &tensors()[..1]);
    let text = summary(
        buf.len() as u64,
        &[Segment {
            off: 0,
            bytes: &buf,
        }],
    )
    .unwrap();
    assert!(text.starts_with("{\"arch\":\"llama\""), "{text}");
    assert!(!text.contains("parameterCount"), "{text}");
}

fn kv_u16(key: &str, value: u16) -> Vec<u8> {
    let mut out = s(key);
    out.extend(2u32.to_le_bytes());
    out.extend(value.to_le_bytes());
    out
}

/// #1591: shard 1 of a split model lists only its own tensors, so the sum would be a fraction of the model.
#[test]
fn a_split_model_without_a_declared_count_has_no_count() {
    let mut kvs = base();
    kvs.push(kv_u16("split.no", 0));
    kvs.push(kv_u16("split.count", 3));
    let buf = header(&kvs, 3, &tensors());
    let (params, raw) = general_params(&buf);
    assert_eq!((params, raw), (Json::Null, Json::Null));
    use gguf::node::{summary, Segment};
    let text = summary(
        buf.len() as u64,
        &[Segment {
            off: 0,
            bytes: &buf,
        }],
    )
    .unwrap();
    assert!(!text.contains("parameterCount"), "{text}");
}

#[test]
fn a_split_model_keeps_a_declared_count_and_a_single_file_split_count_still_sums() {
    let mut kvs = base();
    kvs.push(kv_u16("split.count", 3));
    kvs.push(kv_u64("general.parameter_count", 7_000_000_000));
    let (params, _) = general_params(&header(&kvs, 3, &tensors()));
    assert_eq!(params, Json::Str("7.0 B".into()));
    // split.count of 1 (or 0) is a whole file.
    for count in [0u16, 1] {
        let mut kvs = base();
        kvs.push(kv_u16("split.count", count));
        let (params, _) = general_params(&header(&kvs, 3, &tensors()));
        assert_eq!(params, Json::Str("147.9 M".into()), "split.count {count}");
    }
}
