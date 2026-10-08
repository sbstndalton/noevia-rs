//! Properties of the gguf-meta.cjs port over generated and mutated headers: no panics; every
//! reply is one of the fixed shapes (a summary, a need, a known fail code with at most a number,
//! a fixed refusal); any window gives the answer the whole file gives; and numbers written as
//! JS writes them parse back to the same double.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use gguf::node::{call, js_number, summary, summary_prefix, Fault, Segment};
use proptest::prelude::*;

fn str_(s: &[u8]) -> Vec<u8> {
    let mut v = (s.len() as u64).to_le_bytes().to_vec();
    v.extend_from_slice(s);
    v
}

fn base() -> Vec<u8> {
    let mut f = b"GGUF".to_vec();
    f.extend(3u32.to_le_bytes());
    f.extend(0u64.to_le_bytes());
    f.extend(6u64.to_le_bytes());
    f.extend(str_(b"general.architecture"));
    f.extend(8u32.to_le_bytes());
    f.extend(str_(b"t"));
    f.extend(str_(b"t.block_count"));
    f.extend(4u32.to_le_bytes());
    f.extend(4u32.to_le_bytes());
    f.extend(str_(b"t.attention.head_count_kv"));
    f.extend(9u32.to_le_bytes());
    f.extend(5u32.to_le_bytes());
    f.extend(3u64.to_le_bytes());
    f.extend([2, 0, 0, 0, 4, 0, 0, 0, 4, 0, 0, 0]);
    f.extend(str_(b"tokens"));
    f.extend(9u32.to_le_bytes());
    f.extend(8u32.to_le_bytes());
    f.extend(1030u64.to_le_bytes());
    for i in 0..1030u32 {
        f.extend(str_(format!("t{i}").as_bytes()));
    }
    f.extend(str_(b"tokenizer.chat_template"));
    f.extend(8u32.to_le_bytes());
    f.extend(str_(b"{{x}}"));
    f.extend(str_(b"t.attention.sliding_window_pattern"));
    f.extend(9u32.to_le_bytes());
    f.extend(9u32.to_le_bytes());
    f.extend(2u64.to_le_bytes());
    f.extend(7u32.to_le_bytes());
    f.extend(1u64.to_le_bytes());
    f.push(1);
    f.extend(6u32.to_le_bytes());
    f.extend(1u64.to_le_bytes());
    f.extend(1.5f32.to_le_bytes());
    f
}

/// What the host does (gguf-meta.cjs readSummaryWasm): hold the first `first` bytes, then on
/// each need read from `at` to at least `end` (and at least as much again as is held), merged
/// with any segment it touches into one.
fn windowed(file: &[u8], first: usize) -> Result<String, Fault> {
    let size = file.len();
    let mut segs: Vec<(usize, usize)> = Vec::new();
    if first.min(size) > 0 {
        segs.push((0, first.min(size)));
    }
    for _round in 0..10_000 {
        let view: Vec<Segment<'_>> = segs
            .iter()
            .map(|&(a, b)| Segment {
                off: a as u64,
                bytes: &file[a..b],
            })
            .collect();
        match summary(size as u64, &view) {
            Err(Fault::Need { at, end }) => {
                let (at, end) = (at as usize, end as usize);
                assert!(at < end && end <= size, "need {at}..{end} of {size}");
                assert!(
                    !segs.iter().any(|&(a, b)| a <= at && end <= b),
                    "need {at}..{end} is held"
                );
                let held: usize = segs.iter().map(|&(a, b)| b - a).sum();
                let (mut lo, mut hi) = (at, size.min(end.max(at + held.max(first).max(1))));
                segs.retain(|&(a, b)| {
                    let touch = a <= hi && b >= lo;
                    if touch {
                        lo = lo.min(a);
                        hi = hi.max(b);
                    }
                    !touch
                });
                segs.push((lo, hi));
                segs.sort_unstable();
            }
            r => return r,
        }
    }
    panic!("no answer");
}

/// A request holding `file` as one segment at 0.
fn one(size: u64, file: &[u8]) -> Vec<u8> {
    let mut v = size.to_le_bytes().to_vec();
    v.extend(u32::from(!file.is_empty()).to_le_bytes());
    if !file.is_empty() {
        v.extend(0u64.to_le_bytes());
        v.extend((file.len() as u32).to_le_bytes());
        v.extend_from_slice(file);
    }
    v
}

const FAILS: [&str; 5] = ["not_gguf", "limit", "eof", "range", "nested"];
const REFUSALS: [&str; 4] = [
    r#"{"error":"input"}"#,
    r#"{"error":"too_large"}"#,
    r#"{"error":"depth"}"#,
    r#"{"error":"kept"}"#,
];

fn check_reply(status: u32, reply: &str) {
    assert!(reply.is_ascii());
    if status == 1 {
        assert!(REFUSALS.contains(&reply), "{reply}");
        return;
    }
    assert_eq!(status, 0);
    if reply.starts_with("{\"summary\":{\"arch\":") {
        serde_json::from_str::<serde_json::Value>(reply).unwrap();
        return;
    }
    let v: serde_json::Value = serde_json::from_str(reply).unwrap();
    let o = v.as_object().unwrap();
    if let Some(n) = o.get("need") {
        assert_eq!(o.len(), 1);
        let (at, end) = (n["at"].as_u64().unwrap(), n["end"].as_u64().unwrap());
        assert!(at < end && n.as_object().unwrap().len() == 2);
        return;
    }
    let code = o["fail"].as_str().unwrap();
    if code == "version" || code == "type" {
        assert_eq!(o.len(), 2);
        assert!(o["value"].as_u64().unwrap() <= u64::from(u32::MAX));
    } else {
        assert_eq!(o.len(), 1, "{reply}");
        assert!(FAILS.contains(&code), "{reply}");
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn arbitrary_bytes_never_panic(body in proptest::collection::vec(any::<u8>(), 0..512), gguf in any::<bool>(), extra in 0u64..4096) {
        let mut file = if gguf { b"GGUF\x03\x00\x00\x00".to_vec() } else { Vec::new() };
        file.extend(body);
        let (status, reply) = call(&one(file.len() as u64 + extra, &file));
        check_reply(status, &reply);
    }

    #[test]
    fn mutated_headers_are_window_independent(
        flips in proptest::collection::vec((any::<prop::sample::Index>(), any::<u8>()), 0..6),
        cut in any::<prop::sample::Index>(),
        truncate in any::<bool>(),
        w in any::<prop::sample::Index>(),
    ) {
        let mut file = base();
        for (i, b) in flips {
            let at = i.index(file.len());
            file[at] = b;
        }
        if truncate {
            let n = cut.index(file.len());
            file.truncate(n);
        }
        let whole = summary_prefix(file.len() as u64, &file);
        let needs = matches!(whole, Err(Fault::Need { .. }));
        prop_assert!(!needs);
        let w = w.index(file.len() + 1);
        prop_assert_eq!(windowed(&file, w), whole.clone());
        let (status, reply) = call(&one(file.len() as u64, &file));
        check_reply(status, &reply);
    }

    #[test]
    fn js_numbers_round_trip(bits in any::<u64>()) {
        let v = f64::from_bits(bits);
        prop_assume!(v.is_finite() && v != 0.0);
        let s = js_number(v);
        prop_assert_eq!(s.parse::<f64>().unwrap(), v, "{}", s);
        prop_assert!(s.len() <= 25);
    }
}

#[test]
fn base_summary() {
    let f = base();
    let s = summary_prefix(f.len() as u64, &f).unwrap();
    assert!(s.contains("\"blockCount\":4"), "{s}");
    assert!(s.contains("\"headCountKv\":[2,4,4]"), "{s}");
    assert!(s.contains("\"slidingWindowPattern\":[[true],[1.5]]"), "{s}");
    assert!(s.ends_with("\"hasChatTemplate\":true}"), "{s}");
}

#[test]
fn caps_are_refusals() {
    // MAX_KEPT: 300,000 kept bytes in nested lists of 1000 under a summary key.
    let mut f = b"GGUF".to_vec();
    f.extend(3u32.to_le_bytes());
    f.extend(0u64.to_le_bytes());
    f.extend(1u64.to_le_bytes());
    f.extend(str_(b".block_count"));
    f.extend(9u32.to_le_bytes());
    f.extend(9u32.to_le_bytes());
    f.extend(300u64.to_le_bytes());
    for _ in 0..300 {
        f.extend(0u32.to_le_bytes());
        f.extend(1000u64.to_le_bytes());
        f.extend([0u8; 1000]);
    }
    assert_eq!(summary_prefix(f.len() as u64, &f), Err(Fault::Kept));
    // The same values under a key the summary does not read are walked, not kept.
    let at = 4 + 4 + 8 + 8 + 8;
    f[at] = b'x';
    assert!(summary_prefix(f.len() as u64, &f).is_ok());
}
