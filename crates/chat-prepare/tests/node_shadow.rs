//! The native preparation in shadow (slice P7a) replayed on recorded Node output
//! (`fixtures/node-shadow.v1.json`, written by `record_shadow.cjs`): whole free-chat turns
//! through core's `createChatHandler` with the native hand-off asked for. For every turn core
//! handed off, `shadow::compare` with the recorded inputs (the request, the account's files as
//! written, the free chat list, the zone, the toolbox catalogue) must say `match`, or skip with
//! the reason the turn is not covered; never `diff` or `failed`. Without the catalogue (the front
//! has none yet) a history that needs it is skipped and the rest still match.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use chat_prepare::shadow::{compare, Catalogue, Settings, ShadowTurn, Verdict};
use chat_prepare::toolboxes::Toolbox;
use chat_prepare::tools_mode::{is_public_tool_name, PRIVATE_CORE_TOOLS};
use js_json::{wtf16, JValue};

const FIXTURE: &str = include_str!("fixtures/node-shadow.v1.json");

fn fixture() -> JValue {
    wtf16::parse(FIXTURE).expect("the fixture is JSON")
}

fn items<'a>(v: &'a JValue, key: &str) -> &'a [JValue] {
    match v.get(key) {
        JValue::Arr(a) => a,
        _ => &[],
    }
}

/// The bytes Node's `fs.writeFileSync(path, text)` writes: UTF-8, a lone surrogate as U+FFFD.
fn file_bytes(v: &JValue) -> Option<Vec<u8>> {
    let text = v.as_str()?;
    let mut out = String::new();
    for c in text.chars() {
        out.push(if wtf16::lone_unit(c).is_some() {
            '\u{FFFD}'
        } else {
            c
        });
    }
    Some(out.into_bytes())
}

fn toolbox(v: &JValue) -> Toolbox {
    Toolbox {
        id: v.get("id").as_str().unwrap().to_string(),
        label: String::new(),
        source: String::new(),
        in_app: false,
        server: None,
        // Recorded as names; a box holds `{ type: 'function', function: { name } }`.
        tools: items(v, "tools")
            .iter()
            .map(|n| {
                JValue::Obj(vec![
                    ("type".into(), JValue::from("function")),
                    (
                        "function".into(),
                        JValue::Obj(vec![("name".into(), n.clone())]),
                    ),
                ])
            })
            .collect(),
        reads: Vec::new(),
    }
}

struct Replayed {
    name: String,
    /// Core's system message holds a chat frame block.
    framed: bool,
    verdict: Verdict,
    without_catalogue: Verdict,
}

fn replay(f: &JValue) -> Vec<Replayed> {
    let cap = f.get("historyCap").as_num().unwrap() as usize;
    let mut out = Vec::new();
    for t in items(f, "turns") {
        let frame = t.get("prepared");
        if !frame.is_object() {
            continue;
        }
        let input = t.get("input");
        let boxes: Vec<Toolbox> = items(input, "toolboxes").iter().map(toolbox).collect();
        let known =
            |name: &str| PRIVATE_CORE_TOOLS.contains(&name) || boxes.iter().any(|b| b.offers(name));
        let carry = |name: &str| is_public_tool_name(Some(name), &boxes);
        let instructions = file_bytes(input.get("instructions"));
        let memory = file_bytes(input.get("memory"));
        let empty = JValue::Arr(Vec::new());
        let free = match input.get("freeChats") {
            JValue::Null | JValue::Undefined => &empty,
            list => list,
        };
        let run = |catalogue: Option<Catalogue<'_>>| {
            compare(&ShadowTurn {
                body: input.get("body"),
                frame,
                instructions: instructions.as_deref(),
                memory: memory.as_deref(),
                free_chats: Some(free),
                time_zone: input.get("tz").as_str().unwrap(),
                chat_framing: input.get("chatFraming").as_bool(),
                brain_context: Some(false),
                history_cap: cap,
                titles: true,
                workspace: true,
                // No settings database is the same as no rows: both read `undefined`.
                settings: Some(Settings {
                    effort_default: input.get("settings").get("reasoning_effort_default"),
                    sampling_presets: input.get("settings").get("auto_sampling_presets_enabled"),
                }),
                catalogue,
            })
        };
        let system = items(frame, "messages")
            .first()
            .map(|m| m.get("content").as_str().unwrap_or_default().to_string())
            .unwrap_or_default();
        out.push(Replayed {
            name: t.get("name").as_str().unwrap_or_default().to_string(),
            framed: system.contains("Chat frame (set by the user)"),
            verdict: run(Some(Catalogue {
                known_tool: &known,
                carry_orphan_tool: &carry,
            })),
            without_catalogue: run(None),
        });
    }
    out
}

#[test]
fn the_fixture_names_its_node_sources() {
    let f = fixture();
    assert_eq!(f.get("version").as_num(), Some(1.0));
    for file in ["server/chat.cjs", "server/rust-chat-native.cjs"] {
        let sha = f.get("sources").get(file).as_str().unwrap_or_default();
        assert_eq!(sha.len(), 64, "{file}");
    }
}

#[test]
fn every_handed_off_free_chat_turn_is_core_s_or_named_as_not_covered() {
    let f = fixture();
    let replayed = replay(&f);
    assert!(replayed.len() >= 100, "{} handed off", replayed.len());
    let wrong: Vec<String> = replayed
        .iter()
        .filter(|r| {
            matches!(r.verdict, Verdict::Diff(_) | Verdict::Failed(_))
                || matches!(r.without_catalogue, Verdict::Diff(_) | Verdict::Failed(_))
        })
        .map(|r| {
            format!(
                "{}: {} / without the catalogue {}",
                r.name,
                r.verdict.line(),
                r.without_catalogue.line()
            )
        })
        .collect();
    assert!(
        wrong.is_empty(),
        "{} of {} differ from core:\n{}",
        wrong.len(),
        replayed.len(),
        wrong[..wrong.len().min(8)].join("\n")
    );
    let matched = replayed
        .iter()
        .filter(|r| r.verdict == Verdict::Match)
        .count();
    let mut counts = std::collections::BTreeMap::new();
    for r in &replayed {
        *counts.entry(r.verdict.line()).or_insert(0) += 1;
        *counts
            .entry(format!(
                "without the catalogue: {}",
                r.without_catalogue.line()
            ))
            .or_insert(0) += 1;
    }
    println!("{} handed off: {counts:?}", replayed.len());
    assert!(
        matched * 10 >= replayed.len() * 8,
        "only {matched} of {} matched",
        replayed.len()
    );
    // Without the catalogue only a history that needs it is skipped; everything else is the same.
    for r in &replayed {
        if r.without_catalogue != r.verdict {
            assert_eq!(
                r.without_catalogue,
                Verdict::Skip("tool-history"),
                "{}",
                r.name
            );
            assert_eq!(r.verdict, Verdict::Match, "{}", r.name);
        }
    }
    assert!(replayed
        .iter()
        .any(|r| r.without_catalogue == Verdict::Skip("tool-history")));
    // A confirmed frame with chat framing on: its block is built and matches.
    let framed: Vec<&Replayed> = replayed.iter().filter(|r| r.framed).collect();
    assert!(framed.len() >= 5, "{} framed", framed.len());
    assert!(framed.iter().all(|r| r.verdict == Verdict::Match));
}

/// A frame whose messages are not the native ones is reported where they first differ, and a
/// frame with a message more or less too.
#[test]
fn a_difference_is_located_without_its_text() {
    let f = fixture();
    let t = items(&f, "turns")
        .iter()
        .find(|t| {
            t.get("prepared").is_object()
                && items(t.get("prepared"), "messages").len() >= 2
                && t.get("input").get("instructions").is_nullish()
        })
        .unwrap();
    let input = t.get("input");
    let free = JValue::Arr(Vec::new());
    let memory = file_bytes(input.get("memory"));
    let check = |frame: &JValue| {
        compare(&ShadowTurn {
            body: input.get("body"),
            frame,
            instructions: None,
            memory: memory.as_deref(),
            free_chats: Some(&free),
            time_zone: input.get("tz").as_str().unwrap(),
            chat_framing: Some(false),
            brain_context: Some(false),
            history_cap: f.get("historyCap").as_num().unwrap() as usize,
            titles: true,
            workspace: true,
            settings: None,
            catalogue: None,
        })
    };
    let frame = t.get("prepared").clone();
    let n = items(&frame, "messages").len();
    let with = |messages: Vec<JValue>| {
        let JValue::Obj(mut items) = frame.clone() else {
            panic!()
        };
        wtf16::set(&mut items, "messages", JValue::Arr(messages));
        JValue::Obj(items)
    };
    let mut messages = items(&frame, "messages").to_vec();
    // The last message's text changed in its 5th unit.
    let last = messages.last().unwrap().clone();
    let content = last.get("content").as_str().unwrap().to_string();
    let changed: String = content
        .chars()
        .enumerate()
        .map(|(i, c)| if i == 4 { '\u{2603}' } else { c })
        .collect();
    let JValue::Obj(mut m) = last else { panic!() };
    wtf16::set(&mut m, "content", JValue::from(changed));
    *messages.last_mut().unwrap() = JValue::Obj(m);
    match check(&with(messages.clone())) {
        Verdict::Diff(d) => {
            assert_eq!((d.message, d.ours, d.core), (n - 1, n, n));
            assert!(d.unit > 4);
        }
        other => panic!("{other:?}"),
    }
    // One message fewer, one more.
    match check(&with(items(&frame, "messages")[..n - 1].to_vec())) {
        Verdict::Diff(d) => assert_eq!((d.message, d.ours, d.core), (n - 1, n, n - 1)),
        other => panic!("{other:?}"),
    }
    let mut more = items(&frame, "messages").to_vec();
    more.push(JValue::Null);
    match check(&with(more)) {
        Verdict::Diff(d) => assert_eq!((d.message, d.ours, d.core), (n, n, n + 1)),
        other => panic!("{other:?}"),
    }
    assert!(!check(&frame).line().contains(&content));
    // A `chat` or `turn` field that is not the native one is named by its field.
    for (field, key) in [("chat", "key"), ("turn", "streamIdleMs")] {
        let JValue::Obj(mut items) = frame.clone() else {
            panic!()
        };
        let JValue::Obj(mut inner) = frame.get(field).clone() else {
            panic!()
        };
        wtf16::set(&mut inner, key, JValue::from("qa-other"));
        wtf16::set(&mut items, field, JValue::Obj(inner));
        match check(&JValue::Obj(items)) {
            Verdict::Diff(d) => assert_eq!((d.field, d.message), (field, 0)),
            other => panic!("{field}: {other:?}"),
        }
    }
}

/// What the recording cannot show: an Automatic turn routed to the Code role (chat.cjs adds its route
/// note to the system message) and a free list that could not be read with chat framing on are
/// skipped, not compared.
#[test]
fn a_code_route_and_an_unreadable_free_list_are_skipped() {
    let f = fixture();
    let t = items(&f, "turns")
        .iter()
        .find(|t| t.get("prepared").is_object())
        .unwrap();
    let input = t.get("input");
    let empty = JValue::Arr(Vec::new());
    let check = |frame: &JValue, free: Option<&JValue>, framing: Option<bool>| {
        compare(&ShadowTurn {
            body: input.get("body"),
            frame,
            instructions: None,
            memory: None,
            free_chats: free,
            time_zone: "UTC",
            chat_framing: framing,
            brain_context: Some(false),
            history_cap: 1000,
            titles: true,
            workspace: true,
            settings: None,
            catalogue: None,
        })
    };
    let JValue::Obj(mut items) = t.get("prepared").clone() else {
        panic!()
    };
    wtf16::set(&mut items, "role", JValue::from("code"));
    let code = JValue::Obj(items);
    assert_eq!(
        check(&code, Some(&empty), Some(false)),
        Verdict::Skip("code-route")
    );
    let frame = t.get("prepared");
    assert_eq!(check(frame, None, Some(true)), Verdict::Skip("free-list"));
    assert_eq!(check(frame, None, None), Verdict::Skip("free-list"));
    assert_ne!(check(frame, None, Some(false)), Verdict::Skip("free-list"));
}

/// The effort, Fast thinking switch and sampling presets of the recorded frames (settings rows varied)
/// are the native ones, and one that is not is named by its field.
#[test]
fn the_effort_and_sampling_follow_the_settings_rows() {
    let f = fixture();
    let frames: Vec<&JValue> = items(&f, "turns")
        .iter()
        .map(|t| t.get("prepared"))
        .filter(|p| p.is_object())
        .collect();
    assert!(frames
        .iter()
        .any(|p| p.get("effort").as_str() == Some("high")));
    assert!(frames
        .iter()
        .any(|p| p.get("effort").as_str() == Some("low")));
    assert!(frames
        .iter()
        .any(|p| matches!(p.get("sampling"), JValue::Obj(items) if !items.is_empty())));
    assert!(frames
        .iter()
        .any(|p| matches!(p.get("sampling"), JValue::Obj(items) if items.is_empty())));
    // Automatic turns (recorded with roles and a classifier verdict): a Fast route at the account
    // default effort runs with thinking off and the effort lowered.
    assert!(frames
        .iter()
        .any(|p| p.get("thinkingOff") == &JValue::Bool(true)));
    assert!(frames.iter().any(|p| p.get("auto") == &JValue::Bool(true)
        && p.get("role").as_str() == Some("fast")
        && p.get("effort").as_str() == Some("low")));
    assert!(frames
        .iter()
        .any(|p| p.get("auto") == &JValue::Bool(true) && p.get("role").as_str() == Some("smart")));
    // The same write twice, and once with its arguments in another key order: one fingerprint.
    let dedupe = items(&f, "turns")
        .iter()
        .find(|t| t.get("name").as_str() == Some("history_applied_dedupe"))
        .unwrap();
    assert_eq!(
        items(dedupe.get("prepared").get("chat"), "recentWrites").len(),
        3
    );
    let t = items(&f, "turns")
        .iter()
        .find(|t| t.get("prepared").is_object() && t.get("input").get("settings").is_object())
        .unwrap();
    let input = t.get("input");
    let empty = JValue::Arr(Vec::new());
    let check = |frame: &JValue| {
        compare(&ShadowTurn {
            body: input.get("body"),
            frame,
            instructions: None,
            memory: None,
            free_chats: Some(&empty),
            time_zone: input.get("tz").as_str().unwrap(),
            chat_framing: Some(false),
            brain_context: Some(false),
            history_cap: 1000,
            titles: true,
            workspace: true,
            settings: Some(Settings {
                effort_default: input.get("settings").get("reasoning_effort_default"),
                sampling_presets: input.get("settings").get("auto_sampling_presets_enabled"),
            }),
            catalogue: None,
        })
    };
    let JValue::Obj(mut items) = t.get("prepared").clone() else {
        panic!()
    };
    wtf16::set(&mut items, "effort", JValue::from("qa-other"));
    match check(&JValue::Obj(items)) {
        Verdict::Diff(d) => assert_eq!(d.field, "effort"),
        other => panic!("{other:?}"),
    }
}
