//! The recorded #1793 attributions (`fixtures/node-sources.v1.json`, written by `record_sources.cjs`):
//! for every case, the files `used_injected` picks from core's prepared candidates are the ones
//! chat-sources.cjs `usedInjected` picked, and `reattributed` sends again exactly the list chat.cjs
//! sent (or nothing when Node sent nothing). The fixture is read with `js_json::wtf16`, so lone
//! surrogates reach the port as Node held them.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use chat_turn::js;
use chat_turn::sources::{reattributed, used_injected, Sources, ATTRIBUTE_MAX};
use js_json::{wtf16, JValue};

const FIXTURE: &str = include_str!("fixtures/node-sources.v1.json");

fn fixture() -> JValue {
    wtf16::parse(FIXTURE).expect("the fixture is JSON")
}

fn items<'a>(v: &'a JValue, key: &str) -> &'a [JValue] {
    match v.get(key) {
        JValue::Arr(items) => items,
        other => panic!("{key} is not an array: {other:?}"),
    }
}

#[test]
fn the_fixture_names_its_node_sources() {
    let f = fixture();
    assert_eq!(f.get("version").as_num(), Some(1.0));
    for name in [
        "server/chat-sources.cjs",
        "server/rust-chat-native.cjs",
        "server/source-readability.cjs",
    ] {
        assert_eq!(f.get("sources").get(name).as_str().unwrap().len(), 64);
    }
    assert_eq!(f.get("attributeMax").as_num(), Some(ATTRIBUTE_MAX as f64));
    assert!(items(&f, "cases").len() >= 200);
}

#[test]
fn every_recorded_attribution_is_reproduced() {
    let f = fixture();
    let mut failed = Vec::new();
    let mut hits = 0;
    for case in items(&f, "cases") {
        let name = case.get("name").as_str().unwrap();
        let sources = Sources::read(case.get("prepared")).expect("a readable prepared part");
        let answer = case.get("answer").as_str().unwrap();
        let question = case.get("question").as_str().unwrap();
        let used: Vec<JValue> = used_injected(&sources.injected, answer, question)
            .iter()
            .map(|e| js::s(e.file.clone()))
            .collect();
        let want_used = js::stringify(case.get("used"));
        let got_used = js::stringify(&JValue::Arr(used));
        let want = js::stringify(case.get("sources"));
        let got = js::stringify(
            &reattributed(&sources, answer, question).map_or(JValue::Null, JValue::Arr),
        );
        if got_used != want_used || got != want {
            failed.push(format!(
                "{name}:\n used  rust {got_used}\n       node {want_used}\n sent  rust {got}\n       node {want}"
            ));
        }
        if !matches!(case.get("sources"), JValue::Null) {
            hits += 1;
        }
    }
    assert!(
        hits >= 20,
        "the recording exercises the re-sent list ({hits})"
    );
    assert!(
        failed.is_empty(),
        "{} differ:\n{}",
        failed.len(),
        failed.join("\n")
    );
}

#[test]
fn a_bad_prepared_part_is_refused() {
    for bad in [
        r#"{"reply":{},"injected":[]}"#,
        r#"{"reply":[],"injected":[{"file":1,"text":"","item":null}]}"#,
        r#"{"reply":[],"injected":[{"file":"a","text":"","item":"x"}]}"#,
        r#"{"reply":[1],"injected":[]}"#,
    ] {
        assert!(
            Sources::read(&wtf16::parse(bad).unwrap()).is_none(),
            "{bad}"
        );
    }
}
