//! The pipeline stages against the answers core's own code gave (tests/fixtures/record.cjs, at the
//! Core head in `replay-cases.json`).
#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
use chat_pipeline::{notes, replay, Thrown};
use js_json::{wtf16, JValue};

fn cases() -> JValue {
    wtf16::parse(include_str!("fixtures/replay-cases.json")).unwrap()
}

fn arr(v: &JValue) -> &[JValue] {
    match v {
        JValue::Arr(a) => a,
        _ => &[],
    }
}

fn same(got: Result<Vec<JValue>, Thrown>, want: &JValue, what: &str) {
    match (got, want.get("throws")) {
        (Err(Thrown), JValue::Str(_)) => {}
        (Ok(v), JValue::Undefined) => assert_eq!(
            wtf16::stringify(&JValue::Arr(v)).unwrap(),
            wtf16::stringify(want.get("out")).unwrap(),
            "{what}"
        ),
        (got, _) => panic!("{what}: got {got:?}, Node {want:?}"),
    }
}

#[test]
fn history_mapping_and_normalization_match_node() {
    let c = cases();
    let mut normalized = 0;
    let mut mapped = 0;
    for (i, k) in arr(c.get("cases")).iter().enumerate() {
        let what = format!("case {i}: {}", wtf16::stringify(k.get("history")).unwrap());
        if !matches!(k.get("normalized"), JValue::Undefined) {
            let boxes = k.get("boxes").clone();
            let carry = |n: &JValue| replay::is_public_tool_name(n, &boxes);
            let known = |n: &JValue| replay::is_known_tool(n, &boxes);
            let o = replay::ReplayOptions {
                carry_orphan_tool: &carry,
                off_box: k.get("offBox") == &JValue::Bool(true),
                known_tool: &known,
            };
            let message = if k.get("messageUndefined") == &JValue::Bool(true) {
                JValue::Undefined
            } else {
                k.get("message").clone()
            };
            same(
                replay::normalize_replay_history(arr(k.get("history")), &message, &o),
                k.get("normalized"),
                &what,
            );
            normalized += 1;
        } else {
            let cap = k.get("cap").as_num().unwrap() as i64;
            same(
                Ok(replay::map_history(k.get("history"), cap)),
                k.get("mapped"),
                &what,
            );
            mapped += 1;
        }
    }
    assert!(normalized > 800 && mapped > 400, "{normalized} {mapped}");
}

#[test]
fn public_tools_and_failed_turns_match_node() {
    let c = cases();
    for k in arr(c.get("public")) {
        assert_eq!(
            replay::is_public_tool_name(k.get("name"), k.get("boxes")),
            k.get("out") == &JValue::Bool(true),
            "{:?}",
            k
        );
    }
    for k in arr(c.get("failed")) {
        assert_eq!(
            replay::is_failed_turn(k.get("e")),
            k.get("out") == &JValue::Bool(true),
            "{:?}",
            k
        );
    }
}

#[test]
fn system_notes_match_node() {
    let c = cases();
    for k in arr(c.get("notes")) {
        let m = arr(k.get("m"));
        let n = k.get("n").as_str().unwrap();
        same(Ok(notes::with_system_note(m, n)), k.get("with"), n);
        same(Ok(notes::without_system_note(m, n)), k.get("without"), n);
    }
}

#[test]
fn the_oracle_is_the_paired_core_and_names_its_notes() {
    let c = cases();
    assert_eq!(c.get("core").as_str().map(str::len), Some(40));
    assert!(notes::CODE_ROUTE_NOTE.starts_with("For commands and code"));
}

#[test]
fn validation_matches_node() {
    use chat_pipeline::validate::{check, Account, Early};
    let c = wtf16::parse(include_str!("fixtures/validate-cases.json")).unwrap();
    let mut seen = std::collections::HashSet::new();
    for k in arr(c.get("cases")) {
        let a = Account {
            diary_enabled: k.get("diaryEnabled") == &JValue::Bool(true),
            extras_project: k.get("extrasProject") == &JValue::Bool(true),
        };
        let got = check(k.get("body"), a);
        let want = k.get("outcome");
        let what = wtf16::stringify(k).unwrap();
        match (got, want) {
            (Ok(Early::Continue(_)), JValue::Str(s)) if s == "continue" => {}
            (Ok(Early::StartPreparation), JValue::Str(s)) if s == "prepare" => {}
            (Ok(Early::Refuse(status, error)), w) => {
                assert_eq!(
                    f64::from(status),
                    w.get("status").as_num().unwrap(),
                    "{what}"
                );
                assert_eq!(
                    Some(error),
                    w.get("payload").get("error").as_str(),
                    "{what}"
                );
                assert_eq!(
                    wtf16::stringify(w.get("payload")).unwrap().len(),
                    error.len() + 12,
                    "{what}"
                );
            }
            (Err(Thrown), w) if w.get("throws").as_str().is_some() => {}
            (got, _) => panic!("{what}: got {got:?}"),
        }
        seen.insert(wtf16::stringify(want).unwrap());
    }
    assert!(seen.len() >= 8, "{seen:?}");
}
