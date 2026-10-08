//! The shared fixture table (`tests/fixtures/gguf-meta.v1.json`, byte-identical to noevia-core's,
//! printed by its `tools/gen-gguf-meta-fixtures.cjs` from gguf-meta.cjs itself).
//!
//! Every row must agree exactly: the canonical summary text byte for byte, or the JS's error
//! message. Rows nested past `MAX_NEST` (which the JS accepts) must be refused as `Depth`. Each
//! row also runs through windows that start small and grow to what the reader asks for, and
//! through the wire call.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use gguf::node::{call, summary, Fault, MAX_NEST};
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

/// What the host does: a window that grows to the offset the reader names.
fn windowed(file: &[u8], mut w: usize) -> Result<String, Fault> {
    loop {
        w = w.min(file.len());
        match summary(file.len() as u64, &file[..w]) {
            Err(Fault::Need(n)) => {
                assert!(
                    n as usize > w && n as usize <= file.len(),
                    "need {n} for window {w}"
                );
                w = (n as usize).max(w * 2);
            }
            r => return r,
        }
    }
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
        let got = summary(file.len() as u64, &file);
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
        input.extend_from_slice(&file);
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

#[test]
fn wire_refusals() {
    assert_eq!(call(b""), (1, r#"{"error":"input"}"#.into()));
    assert_eq!(call(&[0; 7]), (1, r#"{"error":"input"}"#.into()));
    // A window longer than the file it claims to be.
    let mut input = 3u64.to_le_bytes().to_vec();
    input.extend_from_slice(b"GGUF");
    assert_eq!(call(&input), (1, r#"{"error":"input"}"#.into()));
    let big = vec![0u8; gguf::node::MAX_INPUT_BYTES + 1];
    assert_eq!(call(&big), (1, r#"{"error":"too_large"}"#.into()));
    // A short window of a longer file asks for more, at most the file's size.
    let mut input = 1000u64.to_le_bytes().to_vec();
    input.extend_from_slice(b"GGUF\x03\x00");
    assert_eq!(call(&input), (0, r#"{"need":8}"#.into()));
}
