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
        assert_eq!(
            Some(title::derived_title(&s(k.get("m"))).as_str()),
            k.get("out").as_str(),
            "{:?}",
            k.get("m")
        );
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
        assert_eq!(
            body.as_deref(),
            k.get("body").as_str(),
            "{}",
            wtf16::stringify(k).unwrap()
        );
        if body.is_some() {
            let reply =
                wtf16::parse(r#"{"choices":[{"message":{"content":"Train times"}}]}"#).unwrap();
            assert_eq!(
                title::title_from_reply(&reply).as_deref(),
                k.get("out").as_str()
            );
        }
    }
    for k in arr(c.get("first")) {
        let m = match k.get("m") {
            JValue::Str(x) if x == "__undefined" => JValue::Undefined,
            v => v.clone(),
        };
        assert_eq!(
            title::first_reply(k.get("h")),
            k.get("reply") == &JValue::Bool(true)
        );
        assert_eq!(
            Some(title::first_user_message(k.get("h"), &m).as_str()),
            k.get("user").as_str()
        );
    }
    assert_eq!(Some(title::TITLE_SYSTEM), c.get("system").as_str());
}

#[test]
fn write_fingerprints_match_node() {
    use chat_pipeline::writes;
    let c = wtf16::parse(include_str!("fixtures/writes-cases.json")).unwrap();
    let un = |v: &JValue| match v {
        JValue::Str(s) if s == "__undefined" => JValue::Undefined,
        v => v.clone(),
    };
    for k in arr(c.get("cases")) {
        let (n, t, a) = (un(k.get("name")), un(k.get("target")), un(k.get("args")));
        assert_eq!(
            Some(writes::fingerprint(&n, &t, &a).as_str()),
            k.get("fp").as_str(),
            "{}",
            wtf16::stringify(k).unwrap()
        );
        if let (Some(want), JValue::Str(raw)) = (k.get("canonical").as_str(), &a) {
            assert_eq!(writes::canonical(&wtf16::parse(raw).unwrap()), want);
        }
    }
}

#[test]
fn interrupted_turn_records_match_node() {
    use chat_pipeline::record::TurnRecord;
    let c = wtf16::parse(include_str!("fixtures/turn-cases.json")).unwrap();
    let show = |v: Option<JValue>| v.map_or("null".to_owned(), |v| wtf16::stringify(&v).unwrap());
    for k in arr(c.get("cases")) {
        // The oracle's clock: 1007 at creation, +7 at each later read (only `usage` reads it).
        let mut clock = 1000.0;
        let mut tick = || {
            clock += 7.0;
            clock
        };
        let mut rec = TurnRecord::new(tick());
        for ev in arr(k.get("events")) {
            rec.observe(ev, &mut tick);
        }
        let what = wtf16::stringify(k.get("events")).unwrap();
        assert_eq!(
            show(rec.assistant_entry()),
            wtf16::stringify(k.get("assistant")).unwrap(),
            "assistant {what}"
        );
        assert_eq!(
            show(rec.partial_entry()),
            wtf16::stringify(k.get("partial")).unwrap(),
            "partial {what}"
        );
    }
}

#[test]
fn sources_match_node() {
    use chat_pipeline::sources;
    let c = wtf16::parse(include_str!("fixtures/sources-cases.json")).unwrap();
    let show = |v: Vec<JValue>| wtf16::stringify(&JValue::Arr(v)).unwrap();
    for k in arr(c.get("build")) {
        assert_eq!(
            show(sources::build_sources(
                arr(k.get("entries")),
                k.get("files")
            )),
            wtf16::stringify(k.get("out")).unwrap(),
            "{}",
            wtf16::stringify(k).unwrap()
        );
    }
    for k in arr(c.get("files")) {
        assert_eq!(
            !sources::is_unreadable(k.get("f")),
            k.get("readable") == &JValue::Bool(true),
            "{:?}",
            k.get("f")
        );
    }
    let mut hits = 0;
    for k in arr(c.get("used")) {
        let got = sources::used_injected(
            k.get("injected"),
            k.get("placed"),
            k.get("answer"),
            k.get("question"),
        );
        hits += usize::from(!got.is_empty());
        assert_eq!(
            show(got),
            wtf16::stringify(k.get("out")).unwrap(),
            "{}",
            wtf16::stringify(k).unwrap()
        );
    }
    assert!(hits > 20, "{hits}");
}

#[test]
fn round_streams_match_node() {
    use chat_pipeline::stream::{Round, Step};
    let c = wtf16::parse(include_str!("fixtures/stream-cases.json")).unwrap();
    let mut sends = 0;
    for k in arr(c.get("cases")) {
        let mut r = Round::new(
            k.get("chatgpt") == &JValue::Bool(true),
            k.get("toolOffset").as_num().unwrap() as usize,
            k.get("shownBefore") == &JValue::Bool(true),
        );
        let mut calls = Vec::new();
        for chunk in arr(k.get("chunks")) {
            if r.ended {
                break;
            }
            for s in r.feed(&base64(chunk.as_str().unwrap())) {
                calls.push(match s {
                    Step::Send(e) => JValue::Arr(vec![JValue::Str("send".into()), e]),
                    Step::FirstOutput => JValue::Arr(vec![JValue::Str("first".into())]),
                    Step::Usage { usage, timings } => JValue::Arr(vec![
                        JValue::Str("usage".into()),
                        usage,
                        if timings.is_nullish() {
                            JValue::Null
                        } else {
                            timings
                        },
                    ]),
                    Step::End => JValue::Arr(vec![JValue::Str("end".into())]),
                });
            }
        }
        sends += calls.len();
        let what = wtf16::stringify(k.get("chunks")).unwrap();
        assert_eq!(
            wtf16::stringify(&JValue::Arr(calls)).unwrap(),
            wtf16::stringify(k.get("calls")).unwrap(),
            "{what}"
        );
        let st = k.get("state");
        assert_eq!(
            JValue::Bool(r.saw_anything),
            *st.get("sawAnything"),
            "{what}"
        );
        assert_eq!(
            Some(r.round_reasoning.as_str()),
            st.get("roundReasoning").as_str(),
            "{what}"
        );
        assert_eq!(
            JValue::Bool(r.round_has_content),
            *st.get("roundHasContent"),
            "{what}"
        );
        assert_eq!(JValue::Bool(r.shown_text), *st.get("shownText"), "{what}");
        assert_eq!(
            Some(r.round_content.as_str()),
            st.get("roundContent").as_str(),
            "{what}"
        );
        let tools: Vec<JValue> = r
            .tool_calls
            .iter()
            .map(|(i, s)| {
                let mut o = vec![
                    ("id".to_owned(), s.id.clone()),
                    ("idMade".to_owned(), JValue::Bool(s.id_made)),
                    ("name".to_owned(), JValue::Str(s.name.clone())),
                    ("args".to_owned(), JValue::Str(s.args.clone())),
                ];
                if s.too_large {
                    o.push(("tooLarge".into(), JValue::Bool(true)));
                }
                if s.name_cut {
                    o.push(("nameCut".into(), JValue::Bool(true)));
                }
                JValue::Arr(vec![
                    if i.is_nan() {
                        JValue::Null
                    } else {
                        JValue::Num(*i)
                    },
                    JValue::Obj(o),
                ])
            })
            .collect();
        assert_eq!(
            wtf16::stringify(&JValue::Arr(tools)).unwrap(),
            wtf16::stringify(st.get("tools")).unwrap(),
            "{what}"
        );
    }
    assert!(sends > 2000, "{sends}");
}

#[test]
fn usage_reports_match_node() {
    use chat_pipeline::usage::Exchange;
    let c = wtf16::parse(include_str!("fixtures/usage-cases.json")).unwrap();
    let s = |x: &str| JValue::Str(x.into());
    let model = s("m-synthetic");
    for k in arr(c.get("cases")) {
        let local = k.get("local") == &JValue::Bool(true);
        let mut x = Exchange::new(model.clone(), k.get("started").as_num().unwrap());
        let mut log = Vec::new();
        for (i, op) in arr(k.get("ops")).iter().enumerate() {
            let o = arr(op);
            let now = o[1].as_num().unwrap();
            match o[0].as_str().unwrap() {
                "round" => x.round(now),
                "first" => {
                    if let Some(e) = x.first_output(now) {
                        log.push(JValue::Arr(vec![s("send"), e]));
                    }
                }
                _ => {
                    if let Some(r) = x.report(&o[2], &o[3]) {
                        if let Some(rec) = r.record_usage {
                            log.push(JValue::Arr(vec![s("record"), model.clone(), rec]));
                        }
                        if let (true, Some(v)) = (local, r.mtp_value) {
                            log.push(JValue::Arr(vec![s("evidence"), model.clone(), v]));
                        }
                        if let Some((p, ms)) = r.prefill_sample {
                            log.push(JValue::Arr(vec![
                                s("prefill"),
                                model.clone(),
                                JValue::Num(p),
                                JValue::Num(ms),
                            ]));
                        }
                        log.push(JValue::Arr(vec![s("send"), r.event]));
                    }
                }
            }
            log.push(JValue::Arr(vec![s("op"), JValue::Num(i as f64)]));
        }
        assert_eq!(
            wtf16::stringify(&JValue::Arr(log)).unwrap(),
            wtf16::stringify(k.get("log")).unwrap(),
            "{}",
            wtf16::stringify(k.get("ops")).unwrap()
        );
    }
}

#[test]
fn turn_endings_match_node() {
    use chat_pipeline::end;
    let c = wtf16::parse(include_str!("fixtures/end-cases.json")).unwrap();
    let strs = |v: &JValue| -> Vec<String> {
        arr(v)
            .iter()
            .map(|x| x.as_str().unwrap().to_owned())
            .collect()
    };
    for k in arr(c.get("pauses")) {
        let n = k.get("applied").as_num().unwrap() as usize;
        let got = vec![
            end::declined_pause(n, &strs(k.get("names"))),
            end::supervision_pause(n),
            end::sensitive_pause(k.get("flag"), n),
        ];
        assert_eq!(
            wtf16::stringify(&JValue::Arr(got)).unwrap(),
            wtf16::stringify(k.get("sent")).unwrap()
        );
    }
    for k in arr(c.get("endings")) {
        let st = k.get("state");
        let b = |key: &str| st.get(key) == &JValue::Bool(true);
        let e = end::Ending {
            compact_only: st.get("body").get("compactOnly") == &JValue::Bool(true),
            paused: b("paused"),
            round_has_content: b("roundHasContent"),
            round_content: st.get("roundContent").as_str().unwrap(),
            round_reasoning: st.get("roundReasoning").as_str().unwrap(),
            final_pass: b("finalPass"),
            message: st.get("message"),
            injected: st.get("injectedEntries"),
            placed: arr(st.get("placedEntries")),
            readable_files: st.get("readableFiles"),
            reply_sources: st.get("replySources").as_num().unwrap() as usize,
            gathered_sources: &strs(st.get("gatheredSources")),
            model: st.get("model"),
            first_token_ms: st.get("exchangeFirstTokenMs").as_num(),
        };
        let (sent, replaced) = end::ending(&e);
        let what = wtf16::stringify(st).unwrap();
        assert_eq!(
            wtf16::stringify(&JValue::Arr(sent)).unwrap(),
            wtf16::stringify(k.get("sent")).unwrap(),
            "{what}"
        );
        assert_eq!(
            replaced.map(|r| wtf16::stringify(&JValue::Arr(r)).unwrap()),
            wtf16::stringify(k.get("replaced")).filter(|s| s != "null"),
            "{what}"
        );
    }
}

fn steps_log(steps: Vec<chat_pipeline::stream::Step>) -> Vec<JValue> {
    use chat_pipeline::stream::Step;
    steps
        .into_iter()
        .map(|s| match s {
            Step::Send(e) => JValue::Arr(vec![JValue::Str("send".into()), e]),
            Step::FirstOutput => JValue::Arr(vec![JValue::Str("first".into())]),
            Step::Usage { usage, timings } => JValue::Arr(vec![
                JValue::Str("usage".into()),
                if usage.is_nullish() {
                    JValue::Null
                } else {
                    usage
                },
                if timings.is_nullish() {
                    JValue::Null
                } else {
                    timings
                },
            ]),
            Step::End => JValue::Arr(vec![JValue::Str("end".into())]),
        })
        .collect()
}

fn slots_json(r: &chat_pipeline::stream::Round) -> String {
    let tools: Vec<JValue> = r
        .tool_calls
        .iter()
        .map(|(i, s)| {
            let mut o = vec![
                ("id".to_owned(), s.id.clone()),
                ("idMade".to_owned(), JValue::Bool(s.id_made)),
                ("name".to_owned(), JValue::Str(s.name.clone())),
                ("args".to_owned(), JValue::Str(s.args.clone())),
            ];
            if s.too_large {
                o.push(("tooLarge".into(), JValue::Bool(true)));
            }
            JValue::Arr(vec![JValue::Num(*i), JValue::Obj(o)])
        })
        .collect();
    wtf16::stringify(&JValue::Arr(tools)).unwrap()
}

#[test]
fn round_fallback_and_call_preparation_match_node() {
    use chat_pipeline::stream::{Round, Slot};
    let c = wtf16::parse(include_str!("fixtures/calls-cases.json")).unwrap();
    for k in arr(c.get("fallback")) {
        let b = k.get("before");
        let mut r = Round::new(
            false,
            b.get("toolOffset").as_num().unwrap() as usize,
            b.get("shownText") == &JValue::Bool(true),
        );
        r.round_content = b.get("roundContent").as_str().unwrap().to_owned();
        let log = steps_log(r.fallback(k.get("body")));
        let what = wtf16::stringify(k.get("body")).unwrap();
        assert_eq!(
            wtf16::stringify(&JValue::Arr(log)).unwrap(),
            wtf16::stringify(k.get("log")).unwrap(),
            "{what}"
        );
        let st = k.get("state");
        assert_eq!(
            Some(r.round_content.as_str()),
            st.get("roundContent").as_str(),
            "{what}"
        );
        assert_eq!(
            Some(r.round_reasoning.as_str()),
            st.get("roundReasoning").as_str(),
            "{what}"
        );
        assert_eq!(
            JValue::Bool(r.round_has_content),
            *st.get("roundHasContent"),
            "{what}"
        );
        assert_eq!(JValue::Bool(r.shown_text), *st.get("shownText"), "{what}");
        assert_eq!(
            slots_json(&r),
            wtf16::stringify(st.get("tools")).unwrap(),
            "{what}"
        );
    }
    for k in arr(c.get("prepare")) {
        let b = k.get("before");
        let mut r = Round::new(false, 0, false);
        r.round_content = b.get("roundContent").as_str().unwrap().to_owned();
        r.round_has_content = b.get("roundHasContent") == &JValue::Bool(true);
        for e in arr(k.get("slots")) {
            let e = arr(e);
            let s = &e[1];
            r.tool_calls.push((
                e[0].as_num().unwrap(),
                Slot {
                    id: s.get("id").clone(),
                    id_made: s.get("idMade") == &JValue::Bool(true),
                    name: s.get("name").as_str().unwrap().to_owned(),
                    args: s.get("args").as_str().unwrap().to_owned(),
                    too_large: s.get("tooLarge") == &JValue::Bool(true),
                    name_cut: false,
                },
            ));
        }
        let (steps, msg) = r.prepare_calls();
        let what = wtf16::stringify(k.get("slots")).unwrap();
        assert_eq!(
            wtf16::stringify(&JValue::Arr(steps_log(steps))).unwrap(),
            wtf16::stringify(k.get("log")).unwrap(),
            "{what}"
        );
        assert_eq!(
            wtf16::stringify(&msg).unwrap(),
            wtf16::stringify(k.get("assistantMsg")).unwrap(),
            "{what}"
        );
        assert_eq!(
            JValue::Bool(r.round_has_content),
            *k.get("roundHasContent"),
            "{what}"
        );
        assert_eq!(
            slots_json(&r),
            wtf16::stringify(k.get("after")).unwrap(),
            "{what}"
        );
    }
}

#[test]
fn tool_result_reduction_matches_node() {
    use chat_pipeline::reduce::reduce_tool_result;
    let c = wtf16::parse(include_str!("fixtures/reduce-cases.json")).unwrap();
    let texts = arr(c.get("texts"));
    let mut reduced = 0;
    for k in arr(c.get("cases")) {
        let text = texts[k.get("t").as_num().unwrap() as usize]
            .as_str()
            .unwrap();
        let r = reduce_tool_result(
            text,
            k.get("maxChars").as_num().unwrap() as usize,
            k.get("maxValueChars").as_num().unwrap() as usize,
        );
        let what = format!(
            "t={} max={} mv={}",
            k.get("t").as_num().unwrap(),
            k.get("maxChars").as_num().unwrap(),
            k.get("maxValueChars").as_num().unwrap()
        );
        assert_eq!(JValue::Bool(r.reduced), *k.get("reduced"), "{what}");
        assert_eq!(Some(r.note.as_str()), k.get("note").as_str(), "{what}");
        if r.reduced {
            reduced += 1;
            assert_eq!(Some(r.text.as_str()), k.get("text").as_str(), "{what}");
        } else {
            assert_eq!(r.text, text);
        }
    }
    assert!(reduced > 300, "{reduced}");
}

#[test]
fn original_noncharacters_beside_kelvin_and_long_s_are_kept_as_node_keeps_them() {
    // Review S5–S9 P2: U+FFFF/U+FFFE are ordinary input, not sentinels (fixtures/record-unicode.cjs).
    use chat_pipeline::{rounds, search, title};
    let c = wtf16::parse(include_str!("fixtures/unicode-cases.json")).unwrap();
    let t = |k: &JValue| k.get("t").as_str().unwrap().to_owned();
    for k in arr(c.get("queries")) {
        assert_eq!(
            search::clean_search_query(&t(k)),
            k.get("out").as_str().unwrap(),
            "{:?}",
            t(k)
        );
        let raw = JValue::Str(
            wtf16::stringify(&JValue::Obj(vec![
                ("query".into(), JValue::Str(t(k))),
                ("n".into(), JValue::Num(1.0)),
            ]))
            .unwrap(),
        );
        assert_eq!(
            search::clean_search_args(&JValue::Str("web_search".into()), &raw),
            *k.get("args"),
            "{:?}",
            t(k)
        );
    }
    for k in arr(c.get("announced")) {
        assert_eq!(
            rounds::announces_tool_action(&t(k)),
            k.get("out") == &JValue::Bool(true),
            "{:?}",
            t(k)
        );
    }
    for k in arr(c.get("links")) {
        let want: Vec<String> = arr(k.get("out"))
            .iter()
            .map(|u| u.as_str().unwrap().to_owned())
            .collect();
        assert_eq!(rounds::source_links(&t(k)), want, "{:?}", t(k));
    }
    for k in arr(c.get("titles")) {
        assert_eq!(
            title::clean_title(k.get("t")).as_deref(),
            k.get("out").as_str(),
            "{:?}",
            t(k)
        );
    }
}

mod route_stage {
    //! chat.cjs:900-1150 against core's own handleChat (fixtures/record-refusals.cjs).
    use chat_pipeline::route::{
        self, Answer, Cloud, Entry, Outcome, ProviderRow, Roles, Settings, Turn, Verdict,
    };
    use js_json::{wtf16, JValue};

    fn s(v: &JValue) -> Option<String> {
        v.as_str().map(str::to_owned)
    }
    fn arr(v: &JValue) -> &[JValue] {
        match v {
            JValue::Arr(a) => a,
            _ => &[],
        }
    }
    fn has(v: &JValue, k: &str) -> bool {
        matches!(v, JValue::Obj(o) if o.iter().any(|(key, _)| key == k))
    }
    fn roles(v: &JValue) -> Option<Roles> {
        matches!(v, JValue::Obj(_)).then(|| Roles {
            fast: s(v.get("fast")),
            smart: s(v.get("smart")),
            vision: s(v.get("vision")),
            code: s(v.get("code")),
            creative: s(v.get("creative")),
        })
    }
    fn provider(v: &JValue) -> ProviderRow {
        ProviderRow {
            id: s(v.get("id")).unwrap(),
            label: s(v.get("label")),
            kind: s(v.get("kind")),
            external: v.get("external") == &JValue::Bool(true),
            shared: v.get("shared") == &JValue::Bool(true),
            base_url: s(v.get("baseUrl")).unwrap_or_default(),
        }
    }

    struct Io<'a> {
        providers: &'a JValue,
        catalogue: Option<Vec<Entry>>,
        verdict: Option<Verdict>,
        answer: Option<Answer>,
    }

    impl route::RouteIo for Io<'_> {
        fn provider(&self, id: &str) -> Option<ProviderRow> {
            match self.providers.get(id) {
                JValue::Obj(_) => Some(provider(self.providers.get(id))),
                _ => None,
            }
        }
        async fn catalogue(&self) -> Option<Vec<Entry>> {
            self.catalogue.clone()
        }
        async fn check(&self) -> Option<Verdict> {
            self.verdict.clone()
        }
        async fn open_choice(&self) -> bool {
            true
        }
        async fn await_choice(&self) -> Option<Answer> {
            self.answer.clone()
        }
        fn aborted(&self) -> bool {
            false
        }
    }

    fn laya(name: &str) -> bool {
        let n = name.trim().to_ascii_lowercase();
        n.strip_prefix("laya")
            .is_some_and(|rest| rest.is_empty() || rest.starts_with(['_', '.', '-']))
    }

    fn block<F: std::future::Future>(f: F) -> F::Output {
        let mut f = std::pin::pin!(f);
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        match f.as_mut().poll(&mut cx) {
            std::task::Poll::Ready(v) => v,
            std::task::Poll::Pending => panic!("the oracle's io never waits"),
        }
    }

    #[test]
    fn routing_and_model_choice_match_node() {
        let c = wtf16::parse(include_str!("fixtures/refusals-cases.json")).unwrap();
        let providers = c.get("providers");
        let diary = s(c.get("diaryProjectId")).unwrap();
        let mut seen = std::collections::BTreeMap::<String, usize>::new();
        for k in arr(c.get("cases")) {
            let space = s(k.get("space")).unwrap();
            let explicit = ["routing", "model", "providerId"].iter().any(|f| has(k, f));
            let project = (space == "diaryProject" || space == "project" || explicit).then(|| {
                route::Project {
                    id: if space == "diaryProject" {
                        diary.clone()
                    } else {
                        "fixture-project".into()
                    },
                    routing: s(k.get("routing")),
                    model: s(k.get("model")),
                    provider: s(k.get("providerId")),
                }
            });
            let flags = k.get("chatFlags");
            let rm = k.get("rm");
            let settings = |v: &JValue| -> Option<Settings> {
                matches!(v, JValue::Obj(_)).then(|| Settings {
                    mode: s(v.get("mode")),
                    when_sensitive: s(v.get("whenSensitive")),
                    cloud: Cloud {
                        provider_id: s(v.get("cloud").get("providerId")),
                        fast: s(v.get("cloud").get("fast")),
                        smart: s(v.get("cloud").get("smart")),
                        code: s(v.get("cloud").get("code")),
                    },
                })
            };
            let chatgpt = k.get("chatgpt");
            let creative = k.get("verdicts").get("creative");
            let turn = Turn {
                body_project: project.is_some(),
                project,
                chat_id: Some("fixture-chat".into()),
                space_id: JValue::Undefined,
                compact_only: false,
                default_provider: "default".into(),
                diary_project_id: diary.clone(),
                roles_first: roles(k.get("roles")),
                roles_later: if k.get("rolesGone") == &JValue::Bool(true) {
                    None
                } else {
                    roles(k.get("roles"))
                },
                routing_modes: matches!(rm, JValue::Obj(_)).then(|| settings(rm.get("settings"))),
                force_local: flags.get("forceLocal") == &JValue::Bool(true),
                allow_cloud: flags.get("allowCloud") == &JValue::Bool(true),
                pre_flag: s(k.get("verdicts").get("preFlag")),
                router_role: s(k.get("role")).unwrap(),
                creative_for: ["fast", "smart", "code"]
                    .iter()
                    .filter(|r| creative.get(r) == &JValue::Bool(true))
                    .map(|r| (*r).to_owned())
                    .collect(),
                manager_enabled: k.get("managerOn") == &JValue::Bool(true),
                last_loaded: s(k.get("loaded")),
                route_user: s(k.get("workspaceUser")),
                owner: s(k.get("owner")),
                workspace_user: s(k.get("workspaceUser")),
                chatgpt: matches!(chatgpt, JValue::Obj(_)).then(|| route::ChatGpt {
                    enabled: chatgpt.get("enabled") == &JValue::Bool(true),
                    state: s(chatgpt.get("state")).unwrap(),
                }),
                system_model: laya,
            };
            let catalogue = match k.get("catalogue") {
                JValue::Arr(rows) => Some(
                    rows.iter()
                        .map(|r| Entry {
                            name: s(r.get("name")).unwrap(),
                            labels: arr(r.get("labels")).iter().filter_map(s).collect(),
                            missing_file: r.get("missingFile") == &JValue::Bool(true),
                            ..Entry::default()
                        })
                        .collect(),
                ),
                _ => None,
            };
            let v = rm.get("verdict");
            let a = rm.get("answer");
            let io = Io {
                providers,
                catalogue,
                verdict: matches!(v, JValue::Obj(_)).then(|| Verdict {
                    flagged: match v.get("flagged") {
                        JValue::Bool(b) => Some(*b),
                        _ => None,
                    },
                    flag: s(v.get("flag")),
                    cause: s(v.get("cause")),
                }),
                answer: matches!(a, JValue::Obj(_)).then(|| Answer {
                    choice: s(a.get("choice")),
                    remember: a.get("remember") == &JValue::Bool(true),
                }),
            };
            let got = block(route::decide(&turn, &io));
            let out = k.get("out");
            let what = wtf16::stringify(k).unwrap();
            let want_events: Vec<String> = arr(out.get("events"))
                .iter()
                .map(|e| wtf16::stringify(e).unwrap())
                .collect();
            let got_events: Vec<String> = got
                .events
                .iter()
                .map(|e| wtf16::stringify(e).unwrap())
                .collect();
            assert_eq!(got_events, want_events, "events {what}");
            let kind = if let JValue::Obj(_) = out.get("refused") {
                let r = out.get("refused");
                assert_eq!(
                    got.outcome,
                    Outcome::Refused {
                        status: match r.get("status") {
                            JValue::Num(n) => *n as u16,
                            _ => 0,
                        },
                        error: s(r.get("payload").get("error")).unwrap(),
                        streamed: false
                    },
                    "{what}"
                );
                "refused"
            } else if let Some(other) = s(out.get("other")) {
                let text = other
                    .split_once(",error::")
                    .or_else(|| other.split_once("error::"))
                    .map(|(_, t)| t.to_owned())
                    .unwrap_or_else(|| panic!("unrecorded outcome {what}"));
                assert!(other.starts_with("route_pending"), "{what}");
                match &got.outcome {
                    Outcome::Refused {
                        error,
                        streamed: true,
                        ..
                    } => assert_eq!(error, &text, "{what}"),
                    o => panic!("got {o:?}, Node streamed {text:?}: {what}"),
                }
                "streamed"
            } else {
                let up = out.get("upstream");
                let Outcome::Go(go) = &got.outcome else {
                    panic!("got {:?}, Node went on: {what}", got.outcome)
                };
                if let Some(m) = s(up.get("load")) {
                    assert_eq!(go.model, m, "{what}");
                    assert_eq!(go.provider.id, "default", "{what}");
                } else {
                    assert_eq!(Some(go.model.clone()), s(up.get("model")), "{what}");
                    let url = s(up.get("url")).unwrap();
                    assert!(
                        url.starts_with(&go.provider.base_url),
                        "{url} {:?}: {what}",
                        go.provider
                    );
                }
                "go"
            };
            *seen.entry(kind.into()).or_default() += 1;
        }
        assert!(
            seen["refused"] > 1500 && seen["go"] > 1000 && seen["streamed"] >= 10,
            "{seen:?}"
        );
    }

    /// An io whose `stop` await never completes (or, `abort`, whose client went away after the card).
    struct Stuck {
        stop: &'static str,
        abort: bool,
        reached: std::cell::Cell<bool>,
    }

    impl route::RouteIo for Stuck {
        fn provider(&self, id: &str) -> Option<ProviderRow> {
            Some(ProviderRow {
                id: id.into(),
                base_url: format!("http://{id}.invalid"),
                ..ProviderRow::default()
            })
        }
        async fn catalogue(&self) -> Option<Vec<Entry>> {
            self.at("catalogue").await;
            // Unreadable: no role is reported missing.
            None
        }
        async fn check(&self) -> Option<Verdict> {
            self.at("check").await;
            Some(Verdict {
                flagged: Some(true),
                flag: Some("health".into()),
                cause: None,
            })
        }
        async fn open_choice(&self) -> bool {
            self.at("open").await;
            true
        }
        async fn await_choice(&self) -> Option<Answer> {
            self.at("await").await;
            Some(Answer {
                choice: Some("cloud".into()),
                remember: true,
            })
        }
        fn aborted(&self) -> bool {
            self.abort
        }
    }

    impl Stuck {
        async fn at(&self, name: &str) {
            if name == self.stop {
                self.reached.set(true);
                std::future::pending::<()>().await;
            }
        }
    }

    fn hybrid_turn() -> Turn {
        let roles = Roles {
            fast: Some("fast-m".into()),
            smart: Some("smart-m".into()),
            ..Roles::default()
        };
        Turn {
            project: None,
            body_project: false,
            chat_id: Some("c".into()),
            space_id: JValue::Undefined,
            compact_only: false,
            default_provider: "default".into(),
            diary_project_id: "diary".into(),
            roles_first: Some(roles.clone()),
            roles_later: Some(roles),
            routing_modes: Some(Some(Settings {
                mode: Some("hybrid".into()),
                when_sensitive: None,
                cloud: Cloud {
                    provider_id: Some("ext".into()),
                    smart: Some("c-smart".into()),
                    ..Cloud::default()
                },
            })),
            force_local: false,
            allow_cloud: false,
            pre_flag: None,
            router_role: "fast".into(),
            creative_for: Vec::new(),
            manager_enabled: false,
            last_loaded: None,
            route_user: Some("u".into()),
            owner: Some("u".into()),
            workspace_user: Some("u".into()),
            chatgpt: None,
            system_model: laya,
        }
    }

    #[test]
    fn dropping_the_decision_at_each_await_leaves_nothing_and_a_client_gone_after_the_card_is_aborted(
    ) {
        for stop in ["check", "open", "await", "catalogue"] {
            let io = Stuck {
                stop,
                abort: false,
                reached: std::cell::Cell::new(false),
            };
            let turn = hybrid_turn();
            {
                let mut f = std::pin::pin!(route::decide(&turn, &io));
                let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
                assert!(
                    std::future::Future::poll(f.as_mut(), &mut cx).is_pending(),
                    "{stop}"
                );
            }
            assert!(io.reached.get(), "the decision never reached {stop}");
        }
        let io = Stuck {
            stop: "",
            abort: true,
            reached: std::cell::Cell::new(false),
        };
        let got = block(route::decide(&hybrid_turn(), &io));
        assert_eq!(got.outcome, Outcome::Aborted);
        assert!(got.streamed);
        assert_eq!(
            got.events.len(),
            1,
            "only the card went out: {:?}",
            got.events
        );
        // Not aborted: the person's remembered cloud choice is sent, the turn goes to the cloud row.
        let io = Stuck {
            stop: "",
            abort: false,
            reached: std::cell::Cell::new(false),
        };
        let got = block(route::decide(&hybrid_turn(), &io));
        assert_eq!(got.events.len(), 2);
        assert!(
            matches!(&got.outcome, Outcome::Go(g) if g.model == "c-smart" && g.provider.external && !g.hybrid_ungranted)
        );
    }
}

mod gate_stage {
    //! One tool round's calls against core's own handleChat (fixtures/record-gate.cjs).
    use chat_pipeline::gate::{
        self, Call, Decision, Executed, Mode, Redeemed, TurnTools, WriteTarget,
    };
    use js_json::{wtf16, JValue};
    use std::cell::{Cell, RefCell};

    fn s(v: &JValue) -> Option<String> {
        v.as_str().map(str::to_owned)
    }
    fn arr(v: &JValue) -> &[JValue] {
        match v {
            JValue::Arr(a) => a,
            _ => &[],
        }
    }
    fn mode(v: &JValue) -> Mode {
        match v.as_str() {
            Some("allow") => Mode::Allow,
            Some("ask") => Mode::Ask,
            _ => Mode::Block,
        }
    }

    struct Fence {
        open: bool,
        started: std::rc::Rc<Cell<usize>>,
    }
    impl gate::Fence for Fence {
        fn open(&self) -> bool {
            self.open
        }
        fn started(&self) {
            self.started.set(self.started.get() + 1);
        }
    }

    struct Io<'a> {
        sc: &'a JValue,
        writes: Vec<String>,
        toolboxes: JValue,
        decisions: Cell<usize>,
        results: Cell<usize>,
        targets: Cell<usize>,
        grants: Cell<usize>,
        applied: RefCell<Vec<String>>,
        exec: RefCell<Vec<String>>,
        audit: RefCell<Vec<String>>,
        opened: Cell<usize>,
        revoked: Cell<usize>,
        fences: Cell<usize>,
        started: std::rc::Rc<Cell<usize>>,
        /// The await that never completes ("" for none), and whether it was reached.
        stop: &'static str,
        reached: Cell<bool>,
    }

    impl Io<'_> {
        async fn at(&self, name: &str) {
            if name == self.stop {
                self.reached.set(true);
                std::future::pending::<()>().await;
            }
        }
    }

    impl gate::GateIo for Io<'_> {
        type Binding = usize;
        type Fence = Fence;
        fn user(&self) -> Option<String> {
            Some("synthetic-user".into())
        }
        fn chat_id(&self) -> Option<String> {
            // A body without one gets a new chat id from the handler before the tools run.
            Some(s(self.sc.get("chatId")).unwrap_or_else(|| "new-chat".into()))
        }
        fn revoked_skill_refusal(&self, _: &str) -> Option<String> {
            None
        }
        fn is_write(&self, name: &str) -> bool {
            self.writes.iter().any(|w| w == name)
        }
        fn mode(&self, name: &str, _: bool) -> Mode {
            let flip = self.sc.get("flip");
            if matches!(flip, JValue::Obj(_))
                && flip.get("tool").as_str() == Some(name)
                && matches!(flip.get("afterDecisions"), JValue::Num(n) if self.decisions.get() as f64 >= *n)
            {
                return mode(flip.get("to"));
            }
            mode(self.sc.get("modes").get(name))
        }
        fn egress_refusal(&self, _: &str, _: &str) -> Option<String> {
            None
        }
        fn edit_tool(&self, _: &str) -> bool {
            false
        }
        fn resolve_edit(&self, _: &str) -> gate::EditTarget {
            gate::EditTarget::default()
        }
        fn has_write_targets(&self) -> bool {
            matches!(self.sc.get("target"), JValue::Obj(_))
        }
        async fn write_target(&self, _: &str, _: &str) -> Result<Option<WriteTarget>, ()> {
            self.at("write_target").await;
            let t = self.sc.get("target");
            let n = self.targets.get();
            self.targets.set(n + 1);
            let v = if n == 0 || t.get("again").as_str() == Some("same") {
                t.get("first")
            } else {
                t.get("again")
            };
            match v {
                JValue::Obj(_) if v.get("throws") == &JValue::Bool(true) => Err(()),
                JValue::Obj(_) => Ok(Some(WriteTarget {
                    target: s(v.get("target")),
                    kind: s(v.get("kind")),
                    error: s(v.get("error")),
                })),
                _ => Ok(None),
            }
        }
        fn provenance(&self, _: &str) -> Option<Result<Vec<JValue>, ()>> {
            None
        }
        fn automatic_only(&self, _: &str) -> bool {
            false
        }
        async fn permission_context(&self) -> Result<Option<usize>, ()> {
            self.at("permission_context").await;
            if self.chat_id().is_none() {
                return Ok(None);
            }
            let n = self.grants.get() + 1;
            self.grants.set(n);
            match self.sc.get("grant").as_str() {
                Some("tombstoned") => Err(()),
                Some("moves") => Ok(Some(n)),
                _ => Ok(Some(0)),
            }
        }
        fn binding_matches(&self, a: &Option<usize>, b: &Option<usize>) -> bool {
            a.is_some() && a == b
        }
        async fn chat_wide_approved(&self, _: &Option<usize>) -> bool {
            self.at("chat_wide_approved").await;
            self.sc.get("chatWide") == &JValue::Bool(true)
        }
        fn repeat(&self, fp: &str) -> bool {
            self.applied.borrow().iter().any(|a| a == fp)
        }
        fn route(&self, name: &str) -> Option<String> {
            match name {
                "gdrive_update" => Some("gdrive".into()),
                "delete_note" => Some("notes".into()),
                _ => None,
            }
        }
        fn new_approval_id(&self) -> String {
            "ap-00000000-0000-0000-0000-000000000000".into()
        }
        async fn open_approval(&self, _: &str, _: &Option<usize>) -> bool {
            self.at("open_approval").await;
            self.opened.set(self.opened.get() + 1);
            self.sc.get("open") == &JValue::Bool(true)
        }
        async fn await_approval(&self, _: &str, _: &Option<usize>) -> Decision {
            self.at("await_approval").await;
            let n = self.decisions.get();
            self.decisions.set(n + 1);
            Decision::from_js(
                arr(self.sc.get("decisions"))
                    .get(n)
                    .and_then(|d| d.as_str())
                    .unwrap_or("deny"),
            )
        }
        fn aborted(&self) -> bool {
            false
        }
        fn workspace_ok(&self, _: &Option<String>) -> bool {
            true
        }
        async fn revoke_grant(&self) {
            self.at("revoke_grant").await;
            self.revoked.set(self.revoked.get() + 1);
        }
        fn has_redeem(&self) -> bool {
            self.sc.get("redeemWired") == &JValue::Bool(true)
        }
        async fn redeem(&self, _: &Option<usize>) -> Redeemed<Fence> {
            self.at("redeem").await;
            match self.sc.get("redeem").as_str() {
                Some("unavailable") => Redeemed::Unavailable,
                Some("invalid") => Redeemed::Invalid,
                r => {
                    self.fences.set(self.fences.get() + 1);
                    Redeemed::Fence(Fence {
                        open: r == Some("fence-open"),
                        started: self.started.clone(),
                    })
                }
            }
        }
        async fn execute(
            &self,
            name: &str,
            args: &str,
            fence: Option<&Fence>,
            _: Option<&(String, Option<String>)>,
        ) -> Result<Executed, String> {
            self.at("execute").await;
            let n = self.results.get();
            self.results.set(n + 1);
            self.exec
                .borrow_mut()
                .push(format!("{name}|{args}|{}", fence.is_some()));
            let r = arr(self.sc.get("results"))
                .get(n)
                .cloned()
                .unwrap_or(JValue::Str("ok".into()));
            match &r {
                JValue::Str(t) => Ok(Executed {
                    text: t.clone(),
                    ..Executed::default()
                }),
                _ if matches!(r.get("throws"), JValue::Str(_)) => Err(s(r.get("throws")).unwrap()),
                _ if matches!(r.get("notRun"), JValue::Str(_)) => Ok(Executed {
                    text: s(r.get("notRun")).unwrap(),
                    not_run: true,
                    failed: false,
                }),
                _ => Ok(Executed {
                    text: s(r.get("failed")).unwrap(),
                    not_run: false,
                    failed: true,
                }),
            }
        }
        fn audit(&self, event: &str, tool: &str, reason: Option<&str>) {
            self.audit
                .borrow_mut()
                .push(format!("{event}|{tool}|{}", reason.unwrap_or("null")));
        }
        fn record_tool_use(&self, _: &str) {}
        fn public_tool(&self, name: &str) -> bool {
            chat_pipeline::replay::is_public_tool_name(&JValue::Str(name.into()), &self.toolboxes)
        }
    }

    fn block<F: std::future::Future>(f: F) -> F::Output {
        let mut f = std::pin::pin!(f);
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        match std::future::Future::poll(f.as_mut(), &mut cx) {
            std::task::Poll::Ready(v) => v,
            std::task::Poll::Pending => panic!("the oracle's io never waits"),
        }
    }

    /// The first difference, also written to `$TMPDIR/gate-diff.txt` (the verify log keeps one line).
    fn same<T: PartialEq + std::fmt::Debug>(got: &T, want: &T, what: &str) {
        if got != want {
            let text = format!("{what}\n got: {got:?}\nwant: {want:?}\n");
            let _ = std::fs::write(std::env::temp_dir().join("gate-diff.txt"), &text);
            panic!("{text}");
        }
    }

    #[test]
    fn tool_calls_through_the_gate_match_node() {
        let c = wtf16::parse(include_str!("fixtures/gate-cases.json")).unwrap();
        let writes: Vec<String> = arr(c.get("writes")).iter().filter_map(s).collect();
        let tools: Vec<JValue> = [
            "web_search",
            "read_file",
            "write_file",
            "gdrive_update",
            "delete_note",
            "unknown_tool",
        ]
        .iter()
        .map(|n| {
            JValue::Obj(vec![
                ("type".into(), JValue::Str("function".into())),
                (
                    "function".into(),
                    JValue::Obj(vec![("name".into(), JValue::Str((*n).into()))]),
                ),
            ])
        })
        .collect();
        let toolboxes = JValue::Arr(vec![JValue::Obj(vec![
            ("id".into(), JValue::Str("core".into())),
            ("tools".into(), JValue::Arr(tools)),
        ])]);
        let mut paused = 0;
        for k in arr(c.get("cases")) {
            let what = wtf16::stringify(k).unwrap();
            let io = Io {
                sc: k,
                writes: writes.clone(),
                toolboxes: toolboxes.clone(),
                decisions: Cell::new(0),
                results: Cell::new(0),
                targets: Cell::new(0),
                grants: Cell::new(0),
                applied: RefCell::new(Vec::new()),
                exec: RefCell::new(Vec::new()),
                audit: RefCell::new(Vec::new()),
                opened: Cell::new(0),
                revoked: Cell::new(0),
                fences: Cell::new(0),
                started: std::rc::Rc::new(Cell::new(0)),
                stop: "",
                reached: Cell::new(false),
            };
            let mut st = TurnTools::new(true);
            let mut events = Vec::new();
            let mut messages = Vec::new();
            for (i, call) in arr(k.get("calls")).iter().enumerate() {
                let name = s(call.get("name")).unwrap();
                let raw = s(call.get("args")).unwrap();
                let args = chat_pipeline::search::clean_search_args(
                    &JValue::Str(name.clone()),
                    &JValue::Str(raw),
                );
                let call = Call {
                    index: i,
                    id: format!("id-{i}"),
                    name,
                    args: s(&args).unwrap(),
                    too_large: false,
                };
                let reduce = |t: &str| {
                    let u: Vec<u16> = t.encode_utf16().collect();
                    String::from_utf16_lossy(u.get(..120).unwrap_or(&u))
                };
                let done = block(gate::run_call(
                    &io,
                    &mut st,
                    &call,
                    true,
                    &reduce,
                    &mut |e| events.push(e),
                ));
                if let Some(fp) = done.applied.clone() {
                    io.applied.borrow_mut().push(fp);
                }
                events.push(done.tool_result.clone());
                messages.push(done.message);
            }
            if !st.declined_writes.is_empty() {
                events.push(chat_pipeline::end::declined_pause(
                    st.applied.len(),
                    &st.declined_writes,
                ));
                paused += 1;
            }
            let out = k.get("out");
            let want: Vec<String> = arr(out.get("events"))
                .iter()
                .filter(|e| {
                    matches!(
                        e.get("type").as_str(),
                        Some("tool_pending" | "tool_result" | "paused")
                    )
                })
                .map(|e| match e {
                    JValue::Obj(o) => wtf16::stringify(&JValue::Obj(
                        o.iter().filter(|(k, _)| k != "idShape").cloned().collect(),
                    ))
                    .unwrap(),
                    _ => unreachable!(),
                })
                .collect();
            let got: Vec<String> = events
                .iter()
                .map(|e| match e {
                    JValue::Obj(o) => wtf16::stringify(&JValue::Obj(
                        o.iter().filter(|(k, _)| k != "id").cloned().collect(),
                    ))
                    .unwrap(),
                    _ => unreachable!(),
                })
                .collect();
            same(&got, &want, &format!("events {what}"));
            let want_exec: Vec<String> = arr(out.get("exec"))
                .iter()
                .map(|e| {
                    format!(
                        "{}|{}|{}",
                        s(e.get("name")).unwrap(),
                        s(e.get("args")).unwrap(),
                        e.get("fence") == &JValue::Bool(true)
                    )
                })
                .collect();
            same(&*io.exec.borrow(), &want_exec, &format!("exec {what}"));
            let want_audit: Vec<String> = arr(out.get("audit"))
                .iter()
                .map(|a| {
                    let p = arr(a);
                    format!(
                        "{}|{}|{}",
                        s(&p[0]).unwrap(),
                        s(&p[1]).unwrap_or_default(),
                        s(&p[2]).unwrap_or_else(|| "null".into())
                    )
                })
                .collect();
            same(&*io.audit.borrow(), &want_audit, &format!("audit {what}"));
            assert_eq!(
                io.opened.get(),
                arr(out.get("opened")).len(),
                "opened {what}"
            );
            // Every fence redeemed for a call was closed exactly once, whatever the path.
            assert_eq!(io.started.get(), io.fences.get(), "fences {what}");
            assert_eq!(
                Some(io.revoked.get() as f64),
                match out.get("revoked") {
                    JValue::Num(n) => Some(*n),
                    _ => None,
                },
                "revoked {what}"
            );
            match out.get("toolMessages") {
                JValue::Arr(want) => {
                    let got: Vec<(String, String)> = messages
                        .iter()
                        .map(|m| {
                            (
                                s(m.get("tool_call_id")).unwrap(),
                                s(m.get("content")).unwrap(),
                            )
                        })
                        .collect();
                    let want: Vec<(String, String)> = want
                        .iter()
                        .map(|m| (s(m.get("id")).unwrap(), s(m.get("content")).unwrap()))
                        .collect();
                    same(&got, &want, &format!("messages {what}"));
                }
                _ => assert!(
                    !st.declined_writes.is_empty(),
                    "Node sent no second round: {what}"
                ),
            }
        }
        assert!(paused > 50, "{paused}");
    }

    #[test]
    fn dropping_a_call_at_each_await_closes_its_fence_and_runs_nothing_after() {
        let sc = |grant_wide: bool, target: bool| {
            let text = format!(
                r#"{{"calls":[],"modes":{{"write_file":"ask"}},"flip":null,"open":true,"decisions":["approve"],"chatWide":{grant_wide},"redeem":"fence-open","redeemWired":true,"results":["ok: done"],"target":{},"grant":"ok","chatId":"c"}}"#,
                if target {
                    r#"{"first":{"target":"/d","kind":"file"},"again":"same"}"#
                } else {
                    "null"
                }
            );
            wtf16::parse(&text).unwrap()
        };
        for (stop, wide, target) in [
            ("write_target", false, true),
            ("permission_context", false, false),
            ("chat_wide_approved", false, false),
            ("open_approval", false, false),
            ("await_approval", false, false),
            ("redeem", true, false),
            ("execute", true, false),
            ("execute", false, false),
        ] {
            let k = sc(wide, target);
            let io = Io {
                sc: &k,
                writes: vec!["write_file".into()],
                toolboxes: JValue::Arr(Vec::new()),
                decisions: Cell::new(0),
                results: Cell::new(0),
                targets: Cell::new(0),
                grants: Cell::new(0),
                applied: RefCell::new(Vec::new()),
                exec: RefCell::new(Vec::new()),
                audit: RefCell::new(Vec::new()),
                opened: Cell::new(0),
                revoked: Cell::new(0),
                fences: Cell::new(0),
                started: std::rc::Rc::new(Cell::new(0)),
                stop,
                reached: Cell::new(false),
            };
            let mut st = TurnTools::new(true);
            let call = Call {
                index: 0,
                id: "id-0".into(),
                name: "write_file".into(),
                args: "{}".into(),
                too_large: false,
            };
            let reduce = |t: &str| t.to_owned();
            let mut sent = Vec::new();
            {
                let mut send = |e| sent.push(e);
                let mut f = std::pin::pin!(gate::run_call(
                    &io, &mut st, &call, true, &reduce, &mut send
                ));
                let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
                assert!(
                    std::future::Future::poll(f.as_mut(), &mut cx).is_pending(),
                    "{stop}"
                );
            }
            assert!(io.reached.get(), "never reached {stop}");
            // Every fence redeemed was closed by the drop; nothing ran after the await that was dropped.
            assert_eq!(io.started.get(), io.fences.get(), "{stop}: fence left open");
            // Dropped at (or before) the executor's await: the executor never answered.
            assert!(io.exec.borrow().is_empty(), "{stop}");
            assert!(
                io.applied.borrow().is_empty() && st.applied.is_empty(),
                "{stop}"
            );
        }
    }
}

mod finish_stage {
    //! A turn's end (`done`, the title decision, `title`) against core's handleChat, turn after turn on one
    //! handler (fixtures/record-finish.cjs).
    use chat_pipeline::title::{self, AttemptCounter, TitleTurn};
    use js_json::{wtf16, JValue};

    fn arr(v: &JValue) -> &[JValue] {
        match v {
            JValue::Arr(a) => a,
            _ => &[],
        }
    }

    fn same<T: PartialEq + std::fmt::Debug>(got: &T, want: &T, what: &str) {
        if got != want {
            let text = format!("{what}\n got: {got:?}\nwant: {want:?}\n");
            let _ = std::fs::write(std::env::temp_dir().join("gate-diff.txt"), &text);
            panic!("{text}");
        }
    }

    #[test]
    fn turn_endings_and_titles_match_node() {
        let c = wtf16::parse(include_str!("fixtures/finish-cases.json")).unwrap();
        let (mut asked, mut titled) = (0, 0);
        for k in arr(c.get("cases")) {
            let mut attempts = AttemptCounter::new(3, 2000);
            let project = k.get("project") == &JValue::Bool(true);
            let provider = if project {
                k.get("provider").as_str().unwrap()
            } else {
                "default"
            };
            for (t, got) in arr(k.get("turns"))
                .iter()
                .zip(arr(k.get("out").get("turns")))
            {
                let tail = arr(got.get("tail"));
                if tail.first().and_then(|e| e.get("type").as_str()) != Some("telemetry") {
                    continue; // refused or failed before the end (earlier stages)
                }
                let what = format!(
                    "{} turn {}",
                    wtf16::stringify(k).unwrap(),
                    wtf16::stringify(t).unwrap()
                );
                let answer = t.get("answer").as_str().unwrap();
                let turn = TitleTurn {
                    titles: k.get("titles") == &JValue::Bool(true),
                    paused: false,
                    round_has_content: !answer.is_empty(),
                    round_content: answer,
                    chat_id: t.get("chatId"),
                    list_id: project.then_some("fixture-project"),
                    compact_only: k.get("compactOnly") == &JValue::Bool(true),
                    diary: false,
                    local: provider == "default",
                    machine: false,
                    off_box: provider != "default",
                    history: t.get("history"),
                    message: k.get("message"),
                    workspace_user: Some("synthetic-user"),
                };
                let mut named_asked = false;
                let mut auto_named = |_: Option<&str>, _: &str, _: &str| {
                    named_asked = true;
                    match t.get("autoNamed") {
                        JValue::Bool(b) => Ok(*b),
                        JValue::Str(_) => Err(()),
                        _ => Ok(false),
                    }
                };
                let mut busy = || t.get("busy") == &JValue::Bool(true);
                let ask = title::title_ask(&turn, &mut attempts, &mut auto_named, &mut busy);
                same(&ask.is_some(), &matches!(got.get("titleAsked"), JValue::Obj(_)), &format!("asked {what} got {}", wtf16::stringify(got).unwrap()));
                same(&named_asked, &matches!(got.get("autoNamedAsked"), JValue::Arr(_)), &format!("autoNamed {what} got {}", wtf16::stringify(got).unwrap()));
                let made = ask.as_ref().and_then(|_| match t.get("title") {
                    JValue::Str(e) if e == "error" => None,
                    v => title::clean_title(v),
                });
                asked += usize::from(ask.is_some());
                titled += usize::from(made.is_some());
                let events = title::finish_events(
                    "answer-model",
                    made.as_deref()
                        .zip(ask.as_ref())
                        .map(|(m, a)| (a.chat_id.as_str(), m)),
                );
                let want: Vec<String> = tail[1..]
                    .iter()
                    .map(|e| wtf16::stringify(e).unwrap())
                    .collect();
                let events: Vec<String> = events
                    .iter()
                    .map(|e| wtf16::stringify(e).unwrap())
                    .collect();
                same(&events, &want, &format!("tail {what}"));
                match (made.as_deref(), got.get("saved")) {
                    (Some(m), JValue::Arr(s)) => {
                        let a = ask.as_ref().unwrap();
                        assert_eq!(s[1].as_str(), Some(a.chat_id.as_str()), "{what}");
                        assert_eq!(s[2].as_str(), Some(m), "{what}");
                        assert_eq!(s[3].as_str(), Some(a.first_message.as_str()), "{what}");
                        assert_eq!(s[0].as_str(), a.list_id.as_deref(), "{what}");
                    }
                    (None, JValue::Null) => {}
                    (m, s) => panic!("saved {m:?} vs Node {s:?}: {what}"),
                }
            }
        }
        assert!(asked > 400 && titled > 40, "{asked} {titled}");
    }
}
