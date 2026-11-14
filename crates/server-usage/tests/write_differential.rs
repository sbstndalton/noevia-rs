#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
//! The usage writer replayed on recorded Node output (`fixtures/node-usage-write.v1.json`, written by
//! `record_write.cjs` from core's own usage.cjs): each case's calls, made at the recorded instant in the
//! recorded time zone, leave usage.json byte for byte as Node left it. Cases marked `difference` hold a
//! hand-edited value: the port leaves the file as it was.
use js_json::{wtf16, JValue};
use serde_json::Value;
use server_usage::write::{clock_in, record_tool, record_usage, Outcome};

const FIXTURE: &str = include_str!("fixtures/node-usage-write.v1.json");

#[test]
fn recorded_node_usage_writes() {
    let fixture: Value = serde_json::from_str(FIXTURE).unwrap();
    // The calls' values as Node's JSON holds them (key order and all).
    let ordered = wtf16::parse(FIXTURE).unwrap();
    let JValue::Arr(jcases) = ordered.get("cases") else { panic!("cases") };
    let cases = fixture["cases"].as_array().unwrap();
    assert!(cases.len() >= 40, "corpus shrank to {}", cases.len());
    let mut differences = 0;
    for (case, jcase) in cases.iter().zip(jcases) {
        let name = case["name"].as_str().unwrap();
        let mut file: Option<String> = case["start"].as_str().map(str::to_owned);
        let JValue::Arr(ops) = jcase.get("ops") else { panic!("{name}: ops") };
        let unusual = case["difference"] == "unusual";
        for (i, (op, after)) in ops.iter().zip(case["after"].as_array().unwrap()).enumerate() {
            let tz: chrono_tz::Tz = op.get("tz").as_str().unwrap().parse().unwrap();
            let now = op.get("now").as_num().unwrap() as i64;
            let clock = clock_in(now, &tz).unwrap();
            let outcome = match op.get("op").as_str() {
                Some("usage") => record_usage(file.as_deref().map(str::as_bytes), op.get("model"), op.get("usage"), &clock),
                _ => record_tool(file.as_deref().map(str::as_bytes), op.get("name"), &clock),
            };
            let want = after["file"].as_str();
            match outcome {
                Outcome::Write(text) => {
                    assert!(!unusual, "{name}#{i}: the port wrote a file it should leave alone");
                    assert_eq!(Some(text.as_str()), want, "{name}#{i}");
                    file = Some(text);
                }
                Outcome::Nothing => {
                    assert_eq!(want, file.as_deref(), "{name}#{i}: Node left the file as it was");
                }
                Outcome::Unusual => {
                    assert!(unusual, "{name}#{i}: unexpected refusal");
                    differences += 1;
                    break;
                }
            }
        }
    }
    assert_eq!(differences, 6, "the documented differences");
}

#[test]
fn the_recorded_sources_are_named() {
    let fixture: Value = serde_json::from_str(FIXTURE).unwrap();
    for f in ["server/usage.cjs", "server/workspace.cjs"] {
        assert_eq!(fixture["sources"][f].as_str().unwrap().len(), 64, "{f}");
    }
}
