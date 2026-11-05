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

fn base64(s: &str) -> Vec<u8> {
    let mut out = Vec::new();
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => continue,
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    out
}

#[test]
fn provider_failure_text_matches_node() {
    use chat_pipeline::failure::{
        capped_text, provider_failure_text, FailedReply, PROVIDER_ERROR_BODY_CAP,
    };
    let c = wtf16::parse(include_str!("fixtures/failure-cases.json")).unwrap();
    let bodies: Vec<String> = arr(c.get("bodies"))
        .iter()
        .map(|b| capped_text(&base64(b.as_str().unwrap()), PROVIDER_ERROR_BODY_CAP))
        .collect();
    let cases = arr(c.get("cases"));
    assert!(cases.len() > 2000);
    for k in cases {
        let detail = &bodies[k.get("body").as_num().unwrap() as usize];
        let reply = FailedReply {
            status: k.get("status").as_num().map(|n| n as i64),
            detail,
            gate_refusal: k.get("gate") == &JValue::Bool(true),
            provider_message: k.get("providerMessage") == &JValue::Bool(true),
        };
        let got = provider_failure_text(
            &reply,
            k.get("chatgpt") == &JValue::Bool(true),
            k.get("partial") == &JValue::Bool(true),
        );
        assert_eq!(
            Some(got.as_str()),
            k.get("text").as_str(),
            "{}",
            wtf16::stringify(k).unwrap()
        );
    }
}

#[test]
fn round_checks_match_node() {
    use chat_pipeline::rounds;
    let c = wtf16::parse(include_str!("fixtures/rounds-cases.json")).unwrap();
    let text = |v: &JValue| -> String {
        if v.truthy() {
            js_json::to_js_string(v).unwrap()
        } else {
            String::new()
        }
    };
    let mut yes = 0;
    for k in arr(c.get("announced")) {
        let want = k.get("out") == &JValue::Bool(true);
        yes += usize::from(want);
        assert_eq!(
            rounds::announces_tool_action(&text(k.get("t"))),
            want,
            "{:?}",
            k.get("t")
        );
    }
    assert!(yes > 300, "{yes}");
    for k in arr(c.get("links")) {
        let want: Vec<String> = arr(k.get("out"))
            .iter()
            .map(|u| u.as_str().unwrap().to_owned())
            .collect();
        assert_eq!(
            rounds::source_links(&text(k.get("t"))),
            want,
            "{:?}",
            k.get("t")
        );
    }
    for k in arr(c.get("pass")) {
        assert_eq!(
            rounds::needs_final_pass(
                k.get("round").as_num().unwrap() as u32,
                k.get("toolCalls").as_num().unwrap() as usize,
                k.get("hasContent") == &JValue::Bool(true)
            ),
            k.get("out") == &JValue::Bool(true)
        );
    }
    for k in arr(c.get("fallback")) {
        let s: Vec<String> = arr(k.get("sources"))
            .iter()
            .map(|u| u.as_str().unwrap().to_owned())
            .collect();
        assert_eq!(
            Some(rounds::fallback_text(&s).as_str()),
            k.get("out").as_str()
        );
    }
    assert_eq!(
        Some(rounds::ANNOUNCED_NOTE),
        c.get("notes").get("announced").as_str()
    );
    assert_eq!(
        Some(rounds::FINAL_ANSWER_NOTE),
        c.get("notes").get("final").as_str()
    );
}

#[test]
fn titles_match_node() {
    use chat_pipeline::title;
    let c = wtf16::parse(include_str!("fixtures/title-cases.json")).unwrap();
    let s = |v: &JValue| -> String {
        if v.is_nullish() {
            String::new()
        } else {
            js_json::to_js_string(v).unwrap()
        }
    };
    for k in arr(c.get("derived")) {
        assert_eq!(Some(title::derived_title(&s(k.get("m"))).as_str()), k.get("out").as_str(), "{:?}", k.get("m"));
    }
    for k in arr(c.get("isDerived")) {
        let t = match k.get("title") {
            JValue::Str(x) if x == "__undefined" => JValue::Undefined,
            v => v.clone(),
        };
        assert_eq!(
            title::is_derived_title(&t, k.get("m").as_str().unwrap()),
            k.get("out") == &JValue::Bool(true),
            "{}",
            wtf16::stringify(k).unwrap()
        );
    }
    for k in arr(c.get("clean")) {
        let got = title::clean_title(k.get("a"));
        assert_eq!(got.as_deref(), k.get("out").as_str(), "{:?}", k.get("a"));
    }
    for k in arr(c.get("requests")) {
        let body = title::title_request_body(
            k.get("model").as_str().unwrap(),
            k.get("message").as_str().unwrap(),
            k.get("reply").as_str().unwrap(),
        );
        assert_eq!(body.as_deref(), k.get("body").as_str(), "{}", wtf16::stringify(k).unwrap());
        if body.is_some() {
            let reply = wtf16::parse(r#"{"choices":[{"message":{"content":"Train times"}}]}"#).unwrap();
            assert_eq!(title::title_from_reply(&reply).as_deref(), k.get("out").as_str());
        }
    }
    for k in arr(c.get("first")) {
        let m = match k.get("m") {
            JValue::Str(x) if x == "__undefined" => JValue::Undefined,
            v => v.clone(),
        };
        assert_eq!(title::first_reply(k.get("h")), k.get("reply") == &JValue::Bool(true));
        assert_eq!(Some(title::first_user_message(k.get("h"), &m).as_str()), k.get("user").as_str());
    }
    assert_eq!(Some(title::TITLE_SYSTEM), c.get("system").as_str());
}
