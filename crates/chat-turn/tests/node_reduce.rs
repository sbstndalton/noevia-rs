//! The recorded Node reductions (`fixtures/node-reduce.v1.json`, written by `record_reduce.cjs`):
//! every case's `reduce_tool_result` output, as `JSON.stringify({ text, reduced, note })`, must be
//! the bytes noevia-core's `reduceToolResult` gave. The fixture is read with `js_json::wtf16`, so
//! the inputs' lone surrogates reach the port as Node held them.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use chat_turn::js;
use chat_turn::reduce::{reduce_tool_result, Reduced};
use js_json::{wtf16, JValue};

const FIXTURE: &str = include_str!("fixtures/node-reduce.v1.json");

fn fixture() -> JValue {
    wtf16::parse(FIXTURE).expect("the fixture is JSON")
}

fn items<'a>(v: &'a JValue, key: &str) -> &'a [JValue] {
    match v.get(key) {
        JValue::Arr(items) => items,
        other => panic!("{key} is not an array: {other:?}"),
    }
}

fn input<'a>(f: &'a JValue, case: &JValue) -> &'a str {
    let at = case.get("input").as_num().unwrap() as usize;
    items(f, "inputs").get(at).and_then(JValue::as_str).unwrap()
}

fn max_chars(case: &JValue) -> usize {
    case.get("maxChars").as_num().unwrap() as usize
}

fn wire(r: &Reduced) -> String {
    js::stringify(&js::obj([
        ("text", js::s(r.text.clone())),
        ("reduced", JValue::Bool(r.reduced)),
        ("note", js::s(r.note.clone())),
    ]))
}

#[test]
fn the_fixture_names_its_node_source() {
    let f = fixture();
    assert_eq!(f.get("version").as_num(), Some(1.0));
    let sha = f
        .get("sources")
        .get("server/tool-result-reduce.cjs")
        .as_str()
        .unwrap();
    assert_eq!(sha.len(), 64);
    assert!(items(&f, "cases").len() >= 400);
}

#[test]
fn every_recorded_reduction_is_reproduced_byte_for_byte() {
    let f = fixture();
    let mut failed = Vec::new();
    let cases = items(&f, "cases");
    for case in cases {
        let got = wire(&reduce_tool_result(input(&f, case), max_chars(case)));
        let want = case.get("expect").as_str().unwrap();
        if got != want {
            failed.push(format!(
                "{}\n  node: {want}\n  rust: {got}",
                case.get("name").as_str().unwrap()
            ));
        }
    }
    assert!(
        failed.is_empty(),
        "{} of {} cases differ:\n{}",
        failed.len(),
        cases.len(),
        failed
            .iter()
            .take(5)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
}

#[test]
fn the_recording_covers_every_branch() {
    let f = fixture();
    let notes: Vec<String> = items(&f, "cases")
        .iter()
        .map(|c| {
            let r = wtf16::parse(c.get("expect").as_str().unwrap()).unwrap();
            if r.get("reduced") == &JValue::Bool(false) {
                return "passed".into();
            }
            r.get("note").as_str().unwrap().to_string()
        })
        .collect();
    let count = |p: &dyn Fn(&str) -> bool| notes.iter().filter(|n| p(n)).count();
    assert!(count(&|n| n == "passed") >= 10);
    assert!(
        count(
            &|n| n.starts_with("[truncated: showing the first") && !n.contains("after compaction")
        ) >= 10
    );
    assert!(count(&|n| n.contains("after compaction")) >= 10);
    assert!(count(&|n| n.starts_with("[compacted")) >= 10);
    assert!(
        count(
            &|n| n.ends_with("tab-separated, first line is the column names]")
                || n.contains("column(s)")
        ) >= 10
    );
    assert!(count(&|n| n.starts_with("[showing") && !n.contains('\n')) >= 10);
    assert!(count(&|n| n.starts_with("[showing") && n.contains("\n[truncated")) >= 10);
}

/// The named differences: Node parsed these and tabulated or compacted them; the port's parser
/// refuses them (`wtf16::Refusal::Unsupported`), so they take the not-JSON branch, a plain cut
/// with its note.
#[test]
fn input_the_rust_parser_refuses_is_truncated_as_prose() {
    let f = fixture();
    let differences = items(&f, "differences");
    assert_eq!(differences.len(), 4);
    for d in differences {
        let name = d.get("name").as_str().unwrap();
        let text = input(&f, d);
        let max = max_chars(d);
        assert_eq!(
            wtf16::parse(text).err(),
            Some(wtf16::Refusal::Unsupported),
            "{name}"
        );
        let got = reduce_tool_result(text, max);
        let note = format!(
            "[truncated: showing the first {max} of {} characters. Ask for a narrower query if you need the rest.]",
            js::len(text)
        );
        let want = Reduced {
            text: format!("{}\n\n{note}", js::slice(text, max)),
            reduced: true,
            note,
        };
        assert_eq!(got, want, "{name}");
        let node = d.get("node").as_str().unwrap();
        assert_ne!(wire(&got), node, "{name}: no longer a difference");
        // Node tabulated or compacted it (the recording may hold what the port refuses, so it is read as text).
        assert!(
            node.contains(r#""note":"[2 records"#) || node.contains("after compaction"),
            "{name}: {node}"
        );
    }
}
