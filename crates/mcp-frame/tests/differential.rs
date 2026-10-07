//! Differential fixtures (noevia#980): every case in `fixtures/mcp-frame.v1.json` was evaluated by
//! noevia-core's JS reference (`server/mcp.cjs` parseRpcBodyJs, resolveSchemaRefsJs) and encoded
//! as the exact reply this crate must give. The file is byte-identical to noevia-core's
//! `tests/fixtures/mcp-frame.v1.json` (noevia-core CI compares the two).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use mcp_frame::schema::{
    resolve_schema_refs, MAX_NODE_DEPTH, MAX_REF_DEPTH, MAX_SCHEMA_CHARS, MAX_SCHEMA_NODES,
    OBJECT_PROTOTYPE_NAMES,
};
use mcp_frame::{parse_rpc_body, rpc_reply, Expected, MAX_BODY_UNITS};
use serde_json::Value;

fn fixtures() -> Value {
    let text = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/mcp-frame.v1.json"
    ))
    .expect("fixture file");
    let f: Value = serde_json::from_str(&text).expect("fixture JSON");
    assert_eq!(f["version"], 1);
    let l = &f["limits"];
    assert_eq!(l["maxNodes"], MAX_SCHEMA_NODES);
    assert_eq!(l["maxChars"], MAX_SCHEMA_CHARS);
    assert_eq!(l["maxRefDepth"], MAX_REF_DEPTH);
    assert_eq!(l["maxNodeDepth"], MAX_NODE_DEPTH);
    assert_eq!(l["maxBodyBytes"], MAX_BODY_UNITS);
    let names: Vec<&str> = f["objectPrototype"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(names, OBJECT_PROTOTYPE_NAMES);
    f
}

fn unhex16(s: &str) -> Vec<u16> {
    (0..s.len())
        .step_by(4)
        .map(|i| u16::from_str_radix(&s[i..i + 4], 16).expect("hex"))
        .collect()
}

fn expected(id: &Value) -> Expected {
    match id["kind"].as_str().unwrap() {
        "never" => Expected::Never,
        "null" => Expected::Null,
        "true" => Expected::Bool(true),
        "false" => Expected::Bool(false),
        "num" => Expected::Number(f64::from_bits(
            u64::from_str_radix(id["bits"].as_str().unwrap(), 16).unwrap(),
        )),
        "str" => Expected::String(unhex16(id["u16"].as_str().unwrap())),
        k => panic!("id kind {k}"),
    }
}

const TAGS: [&str; 5] = ["reply", "mismatch", "none", "other", "invalid"];

#[test]
fn rpc_bodies_agree() {
    let f = fixtures();
    let cases = f["rpc"].as_array().unwrap();
    assert!(cases.len() >= 2500);
    let mut bad = Vec::new();
    for c in cases {
        let text: Vec<u16> = match c.get("text") {
            Some(t) => t.as_str().unwrap().encode_utf16().collect(),
            None => unhex16(c["textU16"].as_str().unwrap()),
        };
        let o = parse_rpc_body(c["sse"].as_bool().unwrap(), &text, &expected(&c["id"])).unwrap();
        let r = rpc_reply(&text, &o);
        let kind = TAGS[usize::from(r[0])];
        let out = String::from_utf8(r[1..].to_vec()).unwrap();
        let want_out = c["expect"]["out"].as_str().unwrap_or("");
        if kind != c["expect"]["kind"] || out != want_out {
            bad.push(c["name"].as_str().unwrap().to_owned());
        }
    }
    assert!(
        bad.is_empty(),
        "{} of {} rpc fixtures disagree:\n{}",
        bad.len(),
        cases.len(),
        bad.join("\n")
    );
}

#[test]
fn schemas_agree() {
    let f = fixtures();
    let cases = f["schema"].as_array().unwrap();
    assert!(cases.len() >= 800);
    let mut bad = Vec::new();
    for c in cases {
        let input: Vec<u16> = c["input"].as_str().unwrap().encode_utf16().collect();
        let (ok, out) = match resolve_schema_refs(&input).unwrap() {
            Ok(b) => (true, b),
            Err(e) => (false, e.json()),
        };
        let out = String::from_utf8(out).unwrap();
        if ok != c["expect"]["ok"].as_bool().unwrap() || out != c["expect"]["out"].as_str().unwrap()
        {
            bad.push(c["name"].as_str().unwrap().to_owned());
        }
    }
    assert!(
        bad.is_empty(),
        "{} of {} schema fixtures disagree:\n{}",
        bad.len(),
        cases.len(),
        bad.join("\n")
    );
}
