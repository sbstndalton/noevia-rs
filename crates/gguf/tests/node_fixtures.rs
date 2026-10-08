//! The shared fixture table (`tests/fixtures/gguf-meta.v1.json`, byte-identical to noevia-core's,
//! printed by its `tools/gen-gguf-meta-fixtures.cjs` from gguf-meta.cjs itself).
//!
//! Every row must agree exactly: the canonical summary text byte for byte, or the JS's error
//! message. Rows nested past `MAX_NEST` (which the JS accepts) must be refused as `Depth`. Each
//! row also runs through segments that start small and grow to what the reader asks for (as
//! the host does), and through the wire call.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use gguf::node::{call, summary, summary_prefix, Fault, Segment, MAX_NEST};
use serde_json::Value;

fn hex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

fn unpack(parts: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    for p in parts.as_array().unwrap() {
        if let Some(s) = p.as_str() {
            out.extend(hex(s));
        } else {
            let b = hex(p[0].as_str().unwrap())[0];
            out.extend(std::iter::repeat_n(b, p[1].as_u64().unwrap() as usize));
        }
    }
    out
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

fn result_text(r: &Result<String, Fault>) -> String {
    match r {
        Ok(s) => s.clone(),
        Err(f) => format!("{f:?}"),
    }
}

#[test]
fn every_row_matches_the_js() {
    let f: Value = serde_json::from_str(include_str!("fixtures/gguf-meta.v1.json")).unwrap();
    assert_eq!(f["version"], 1);
    assert_eq!(f["limits"]["maxHeaderBytes"], gguf::node::MAX_HEADER_BYTES);
    assert_eq!(f["limits"]["maxArrayKept"], gguf::node::MAX_ARRAY_KEPT);
    assert_eq!(f["limits"]["maxStringKept"], gguf::node::MAX_STRING_KEPT);
    let rows = f["cases"].as_array().unwrap();
    assert!(rows.len() > 300);
    let (mut ok, mut err, mut stricter) = (0, 0, 0);
    for row in rows {
        let name = row["name"].as_str().unwrap();
        let file = unpack(&row["bytes"]);
        let got = summary_prefix(file.len() as u64, &file);
        let nest = row["nest"].as_u64().unwrap_or(0);
        if nest > u64::from(MAX_NEST) {
            assert_eq!(got, Err(Fault::Depth), "{name}");
            stricter += 1;
        } else if let Some(want) = row["summary"].as_str() {
            assert_eq!(got.as_deref(), Ok(want), "{name}");
            ok += 1;
        } else {
            let want = row["error"].as_str().unwrap();
            let msg = got.as_ref().err().and_then(Fault::js_message);
            assert_eq!(msg.as_deref(), Some(want), "{name}: {got:?}");
            err += 1;
        }
        for w in [0, 1, 4, 23, 24, 100, file.len() / 2] {
            assert_eq!(
                result_text(&windowed(&file, w)),
                result_text(&got),
                "{name} window {w}"
            );
        }
        // The wire: the same answer as JSON, or the same fault code.
        let mut input = (file.len() as u64).to_le_bytes().to_vec();
        let n = u32::from(!file.is_empty());
        input.extend(n.to_le_bytes());
        if n == 1 {
            input.extend(0u64.to_le_bytes());
            input.extend((file.len() as u32).to_le_bytes());
            input.extend_from_slice(&file);
        }
        let (status, reply) = call(&input);
        match &got {
            Ok(s) => assert_eq!(
                (status, reply),
                (0, format!("{{\"summary\":{s}}}")),
                "{name}"
            ),
            Err(Fault::Depth) => assert_eq!((status, reply.as_str()), (1, r#"{"error":"depth"}"#)),
            Err(_) => {
                assert_eq!(status, 0, "{name}");
                assert!(reply.starts_with("{\"fail\":\""), "{name}: {reply}");
            }
        }
    }
    assert!(
        ok > 80 && err > 250 && stricter == 2,
        "{ok} {err} {stricter}"
    );
}

fn wire(size: u64, segs: &[(u64, &[u8])]) -> Vec<u8> {
    let mut v = size.to_le_bytes().to_vec();
    v.extend((segs.len() as u32).to_le_bytes());
    for (off, b) in segs {
        v.extend(off.to_le_bytes());
        v.extend((b.len() as u32).to_le_bytes());
    }
    for (_, b) in segs {
        v.extend_from_slice(b);
    }
    v
}

#[test]
fn wire_refusals() {
    const INPUT: &str = r#"{"error":"input"}"#;
    assert_eq!(call(b""), (1, INPUT.into()));
    assert_eq!(call(&[0; 11]), (1, INPUT.into()));
    // No segments: ask for the magic.
    assert_eq!(
        call(&wire(100, &[])),
        (0, r#"{"need":{"at":0,"end":4}}"#.into())
    );
    // A segment past the file, overlapping, unsorted, empty, or bytes left over.
    assert_eq!(call(&wire(3, &[(0, b"GGUF")])), (1, INPUT.into()));
    assert_eq!(
        call(&wire(100, &[(0, b"GGUF"), (2, b"UF")])),
        (1, INPUT.into())
    );
    assert_eq!(
        call(&wire(100, &[(10, b"x"), (0, b"GGUF")])),
        (1, INPUT.into())
    );
    assert_eq!(call(&wire(100, &[(0, b"")])), (1, INPUT.into()));
    let mut extra = wire(100, &[(0, b"GGUF")]);
    extra.push(0);
    assert_eq!(call(&extra), (1, INPUT.into()));
    let mut short = wire(100, &[(0, b"GGUF")]);
    short.pop();
    assert_eq!(call(&short), (1, INPUT.into()));
    // Too many segments.
    let many: Vec<(u64, &[u8])> = (0..=gguf::node::MAX_SEGMENTS as u64)
        .map(|i| (i * 2, &b"x"[..]))
        .collect();
    assert_eq!(call(&wire(10_000, &many)), (1, INPUT.into()));
    let big = vec![0u8; gguf::node::MAX_INPUT_BYTES + 1];
    assert_eq!(call(&big), (1, r#"{"error":"too_large"}"#.into()));
    // A range straddling two segments is asked for whole.
    assert_eq!(
        call(&wire(1000, &[(0, b"GGUF\x03\x00"), (6, b"\x00\x00")])),
        (0, r#"{"need":{"at":4,"end":8}}"#.into())
    );
    // A skip lands in a gap: only the bytes after it are asked for.
    let mut f = b"GGUF".to_vec();
    f.extend(3u32.to_le_bytes());
    f.extend(0u64.to_le_bytes());
    f.extend(2u64.to_le_bytes());
    f.extend(1u64.to_le_bytes());
    f.push(b'v');
    f.extend(9u32.to_le_bytes());
    f.extend(0u32.to_le_bytes());
    f.extend(5000u64.to_le_bytes());
    let head = f.len();
    f.extend(vec![7u8; 5000]);
    let tail_at = f.len() as u64;
    f.extend(1u64.to_le_bytes());
    f.push(b'k');
    f.extend(0u32.to_le_bytes());
    f.push(1);
    assert_eq!(
        call(&wire(f.len() as u64, &[(0, &f[..head])])),
        (
            0,
            format!(r#"{{"need":{{"at":{tail_at},"end":{}}}}}"#, tail_at + 8)
        )
    );
    let segs: [(u64, &[u8]); 2] = [(0, &f[..head]), (tail_at, &f[tail_at as usize..])];
    assert!(call(&wire(f.len() as u64, &segs))
        .1
        .starts_with(r#"{"summary":"#));
}
