//! The recorded Node chat titles (`fixtures/node-titles.v1.json`, written by `record_titles.cjs` from
//! noevia-core's `server/chat-title.cjs`) replayed against `chat_turn::title`, and the four whole-turn title
//! cases of `node-turns.v1.json` checked against the same functions. Both recordings are read with
//! `js_json::wtf16::parse`, so Node's key order and its lone surrogates survive; request bodies are
//! compared as the exact bytes `JSON.stringify` wrote.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

mod common;

use chat_turn::title::{
    ask_for_title, attempt_key, can_request, clean_title, derived_title, first_reply,
    first_user_message, is_derived_title, is_js_space, is_letter, is_pictographic, title_due,
    title_from_reply, title_from_reply_text, title_request_body, title_request_json, AskFacts,
    AttemptCounter, CHAT_TITLE_MAX, MAX_ATTEMPTS, TITLE_MAX, TITLE_SYSTEM,
};
use js_json::wtf16::{self, LONE_BASE};
use js_json::JValue;

type Test = fn(char) -> bool;

fn titles() -> JValue {
    wtf16::parse(include_str!("fixtures/node-titles.v1.json")).unwrap()
}

fn s(v: &JValue) -> &str {
    v.as_str().unwrap()
}

fn list(v: &JValue) -> &[JValue] {
    match v {
        JValue::Arr(items) => items,
        other => panic!("not an array: {other:?}"),
    }
}

fn at(v: &JValue, i: usize) -> &JValue {
    match v {
        JValue::Arr(items) => items.get(i).unwrap_or(&JValue::Undefined),
        _ => &JValue::Undefined,
    }
}

fn num(v: &JValue) -> u64 {
    v.as_num().unwrap() as u64
}

fn has_lone(text: &str) -> bool {
    text.chars().any(|c| u32::from(c) >= LONE_BASE)
}

/// The character a lone surrogate unit is held as.
fn lone(unit: u16) -> char {
    wtf16::lone_char(unit).unwrap()
}

/// `"a-b,c-d"` (hex) into code point ranges.
fn parse_ranges(text: &str) -> Vec<(u32, u32)> {
    text.split(',')
        .map(|r| {
            let (a, b) = r.split_once('-').unwrap();
            (
                u32::from_str_radix(a, 16).unwrap(),
                u32::from_str_radix(b, 16).unwrap(),
            )
        })
        .collect()
}

#[test]
fn the_fixture_names_its_node_source_and_constants() {
    let f = titles();
    assert_eq!(num(f.get("version")), 1);
    let sha = s(f.get("sources").get("server/chat-title.cjs"));
    assert_eq!(sha.len(), 64);
    // The whole-turn recording was made from the same source file.
    assert_eq!(
        sha,
        common::fixture()["sources"]["server/chat-title.cjs"]
            .as_str()
            .unwrap()
    );
    let k = f.get("constants");
    assert_eq!(num(k.get("CHAT_TITLE_MAX")) as usize, CHAT_TITLE_MAX);
    assert_eq!(num(k.get("TITLE_MAX")) as usize, TITLE_MAX);
    assert_eq!(num(k.get("MAX_ATTEMPTS")) as u32, MAX_ATTEMPTS);
    assert_eq!(s(k.get("TITLE_SYSTEM")), TITLE_SYSTEM);
    let d = f.get("counters").get("defaults");
    assert_eq!(num(d.get("maxAttempts")) as u32, MAX_ATTEMPTS);
    assert_eq!(num(d.get("evictedAfterOtherKeys")), 2000);
}

#[test]
fn whitespace_letters_and_pictographs_are_node_s_for_every_code_point() {
    let f = titles();
    let tables: [(&str, Test); 3] = [
        ("space", is_js_space),
        ("letter", is_letter),
        ("pictographic", is_pictographic),
    ];
    for (name, test) in tables {
        let ranges = parse_ranges(s(f.get("unicode").get(name)));
        let mut expected = 0;
        for cp in 0..=0x10ffff_u32 {
            let Some(c) = char::from_u32(cp) else {
                continue;
            };
            let node = ranges.iter().any(|&(a, b)| (a..=b).contains(&cp));
            expected += usize::from(node);
            assert_eq!(test(c), node, "{name} U+{cp:04X}");
        }
        assert!(expected > 5, "{name} has members");
    }
    // The two places the JavaScript sets differ from Rust's.
    assert!(!is_js_space('\u{85}') && '\u{85}'.is_whitespace());
    assert!(is_js_space('\u{feff}') && !'\u{feff}'.is_whitespace());
    // A mapped lone surrogate is none of them.
    assert!(
        !is_js_space(lone(0xd83d)) && !is_letter(lone(0xd83d)) && !is_pictographic(lone(0xd83d))
    );
}

#[test]
fn derived_titles_are_node_s() {
    let f = titles();
    assert!(list(f.get("derived")).len() >= 40);
    for c in list(f.get("derived")) {
        assert_eq!(derived_title(s(c.get("text"))), s(c.get("title")), "{c:?}");
    }
}

#[test]
fn is_derived_title_is_node_s() {
    let f = titles();
    let (mut yes, mut split) = (0, 0);
    for c in list(f.get("isDerived")) {
        // A missing `title` key is `undefined`.
        let got = is_derived_title(c.get("title"), s(c.get("firstMessage")));
        assert_eq!(Some(got), c.get("result").as_bool(), "{c:?}");
        yes += usize::from(got);
        // The old bare 80-unit cut of a message whose astral character straddles it: the stored title
        // ends in a lone surrogate and Node says yes, and so does this port.
        let cut = c.get("title").as_str().is_some_and(has_lone);
        split += usize::from(cut && got);
        assert!(!cut || got, "{c:?}");
    }
    assert!(yes > 20 && yes < list(f.get("isDerived")).len() - 20);
    assert!(split >= 1, "a stored split cut is a positive case");
    // The same case spelled out.
    let message = format!("{}😀{}", "a".repeat(79), "b".repeat(10));
    let cut = format!("{}{}", "a".repeat(79), lone(0xd83d));
    assert_eq!(wtf16::len(&cut), 80);
    assert!(is_derived_title(&JValue::Str(cut), &message));
    assert!(!is_derived_title(&JValue::Str("a".repeat(79)), &message));
}

#[test]
fn the_first_reply_and_the_first_user_message_are_node_s() {
    let f = titles();
    for c in list(f.get("firstOf")) {
        // A missing `history` key is `undefined`.
        let history = c.get("history");
        let message = s(c.get("message"));
        assert_eq!(
            Some(first_reply(history)),
            c.get("firstReply").as_bool(),
            "{c:?}"
        );
        assert_eq!(
            first_user_message(history, message),
            s(c.get("firstUserMessage")),
            "{c:?}"
        );
    }
}

#[test]
fn the_title_request_is_what_generate_title_sends_byte_for_byte() {
    let f = titles();
    let mut cut_pairs = 0;
    for c in list(f.get("requests")) {
        let (model, message, reply) = (s(c.get("model")), s(c.get("message")), s(c.get("reply")));
        assert!(can_request(model, message, reply));
        let calls = list(c.get("calls"));
        assert_eq!(calls.len(), 1);
        let call = &calls[0];
        assert_eq!(s(call.get("method")), "POST");
        assert_eq!(s(call.get("redirect")), "error");
        assert_eq!(call.get("signal").as_bool(), Some(true));
        assert_eq!(
            s(call.get("headers").get("Content-Type")),
            "application/json"
        );
        let node = s(call.get("bodyText"));
        let body = title_request_body(model, message, reply);
        assert_eq!(wtf16::stringify(&body).unwrap(), node, "{message:?}");
        assert_eq!(title_request_json(model, message, reply), node);
        // The recorded bytes read and written again are the same bytes (key order included).
        assert_eq!(
            wtf16::stringify(&wtf16::parse(node).unwrap()).unwrap(),
            node
        );
        cut_pairs += usize::from(node.contains("\\ud"));
        assert_eq!(s(c.get("title")), "QA Title");
    }
    assert!(
        cut_pairs >= 4,
        "the recording cuts astral characters at the clip"
    );
}

#[test]
fn a_blank_model_message_or_reply_makes_no_request() {
    let f = titles();
    assert!(list(f.get("skipped")).len() >= 8);
    for c in list(f.get("skipped")) {
        let text = |k: &str| c.get(k).as_str().unwrap_or("").to_owned();
        let called = c.get("called").as_bool().unwrap();
        assert_eq!(
            can_request(&text("model"), &text("message"), &text("reply")),
            called,
            "{c:?}"
        );
        assert_eq!(c.get("title").is_nullish(), !called, "{c:?}");
    }
}

#[test]
fn replies_are_read_and_cleaned_as_node_does() {
    let f = titles();
    let (mut some, mut none, mut lone_titles) = (0, 0, 0);
    for c in list(f.get("replies")) {
        let response = c.get("response");
        let want = c.get("title").as_str();
        // The crate function takes the reply's text.
        let text = wtf16::stringify(response).unwrap();
        let got = title_from_reply_text(&text);
        assert_eq!(got.as_deref(), want, "{:?}: {text}", c.get("label"));
        assert_eq!(title_from_reply(response).as_deref(), want);
        if let JValue::Arr(choices) = response.get("choices") {
            if let Some(raw) = choices
                .first()
                .and_then(|c| c.get("message").get("content").as_str())
            {
                assert_eq!(clean_title(raw).as_deref(), want, "{raw:?}");
            }
        }
        if let Some(t) = got {
            some += 1;
            lone_titles += usize::from(has_lone(&t));
            assert!(!t.is_empty() && wtf16::len(&t) <= TITLE_MAX, "{t:?}");
        } else {
            none += 1;
        }
    }
    // Node's titles cut through an astral letter, or holding one, end in or hold a lone surrogate.
    assert!(lone_titles >= 8, "{lone_titles}");
    assert!(some > 100 && none > 40, "{some} titles, {none} none");
}

#[test]
fn the_attempt_counter_is_node_s() {
    let f = titles();
    let runs = list(f.get("counters").get("runs"));
    assert!(runs.len() >= 10);
    for run in runs {
        let max = run
            .get("options")
            .get("max")
            .as_num()
            .map_or(MAX_ATTEMPTS, |m| m as u32);
        let max_chats = run
            .get("options")
            .get("maxChats")
            .as_num()
            .map_or(2000, |m| m as usize);
        let mut counter = AttemptCounter::new(max, max_chats);
        for (i, op) in list(run.get("ops")).iter().enumerate() {
            let key = s(op.get("key"));
            match s(op.get("op")) {
                "allowed" => {
                    assert_eq!(
                        Some(counter.allowed(key)),
                        op.get("result").as_bool(),
                        "{i}: {op:?}"
                    );
                }
                _ => assert_eq!(
                    f64::from(counter.record(key)),
                    op.get("result").as_num().unwrap(),
                    "{i}: {op:?}"
                ),
            }
        }
    }
    // The defaults: three tries, 2000 chats, the oldest forgotten first.
    let mut counter = AttemptCounter::default();
    for _ in 0..3 {
        counter.record("first");
    }
    assert!(!counter.allowed("first"));
    for i in 1..2000 {
        counter.record(&format!("other{i}"));
    }
    assert!(!counter.allowed("first"));
    counter.record("other2000");
    assert!(counter.allowed("first"));
}

#[test]
fn the_attempt_key_separates_account_list_and_chat() {
    // Not recorded: chat.cjs builds the key inline, so this is a check against its text, not a replay.
    assert_eq!(
        attempt_key(Some("qa-user"), Some("qa-list"), "qa-chat"),
        "qa-user\0qa-list\0qa-chat"
    );
    assert_eq!(attempt_key(None, None, "qa-chat"), "\0\0qa-chat");
    let mut counter = AttemptCounter::default();
    counter.record(&attempt_key(Some("qa-user"), None, "qa-chat"));
    assert_eq!(
        counter.record(&attempt_key(Some("qa-user-2"), None, "qa-chat")),
        1
    );
}

// ---- the whole-turn title cases -----------------------------------------------------------------

/// A recorded case, read with its key order.
fn turn_case(f: &JValue, name: &str) -> JValue {
    list(f.get("cases"))
        .iter()
        .find(|c| c.get("name").as_str() == Some(name))
        .unwrap()
        .clone()
}

/// The reply a turn streamed: every content delta of its first scripted round.
fn streamed_reply(case: &JValue) -> String {
    list(at(case.get("input").get("rounds"), 0).get("frames"))
        .iter()
        .filter_map(|f| at(f.get("choices"), 0).get("delta").get("content").as_str())
        .collect()
}

#[test]
fn the_whole_turn_title_cases_agree_with_the_module() {
    let f = wtf16::parse(common::FIXTURE).unwrap();
    for name in [
        "title",
        "title_renamed",
        "title_still_derived",
        "title_failed",
    ] {
        let c = turn_case(&f, name);
        let input = c.get("input");
        let (model, message) = (
            s(input.get("project").get("model")),
            s(input.get("body").get("message")),
        );
        let history = input.get("body").get("history");
        let reply = streamed_reply(&c);
        let requests = list(c.get("requests"));
        let first = first_reply(history);
        let auto_named = input.get("titles").get("autoNamed").as_bool() == Some(true);
        let events = list(c.get("events"));

        // Is a title asked for? The turn made a second engine request exactly then. The facts come from
        // the recorded input and events; the recording runs the default local provider on an idle engine
        // (not recorded, so fixed here). The order of the gates and the counter are not recorded from Node;
        // the unit tests below check them against chat.cjs's text.
        let diary = |v: &JValue| v.as_str().is_some_and(|id| id.starts_with("diary"));
        let facts = AskFacts {
            store_wired: input.get("titles").is_object(),
            paused: events
                .iter()
                .any(|e| e.get("type").as_str() == Some("paused")),
            round_has_content: !reply.is_empty(),
            reply: &reply,
            chat_id: input.get("body").get("chatId").as_str().unwrap_or(""),
            compact_only: input.get("body").get("compactOnly").as_bool() == Some(true),
            diary: diary(input.get("body").get("spaceId")) || diary(input.get("project").get("id")),
            local: true,
            machine: false,
        };
        let asked = ask_for_title(
            &facts,
            || title_due(first, || auto_named),
            || true,
            || false,
        );
        assert_eq!(asked, requests.len() == 2, "{name}");

        // The store is read for the first message exactly when the history has an assistant turn.
        let ops = list(c.get("stores"));
        let op_name = |o: &JValue| s(o.get("op")).to_owned();
        let reads = ops
            .iter()
            .filter(|o| op_name(o) == "chatTitles.autoNamed")
            .count();
        assert_eq!(reads, usize::from(!first), "{name}");
        let first_message = first_user_message(history, message);
        for op in ops.iter().filter(|o| op_name(o).starts_with("chatTitles.")) {
            assert_eq!(s(op.get("first")), first_message, "{name}");
        }

        let event = events
            .iter()
            .find(|e| e.get("type").as_str() == Some("title"));
        let saves = ops
            .iter()
            .filter(|o| op_name(o) == "chatTitles.save")
            .count();
        if asked {
            let request = &requests[1];
            assert!(can_request(model, message, &reply));
            // The recorded body, bytes in Node's key order, is what the module builds.
            assert_eq!(
                wtf16::stringify(request.get("body")).unwrap(),
                title_request_json(model, message, &reply),
                "{name}"
            );
            assert_eq!(
                s(request.get("headers").get("Content-Type")),
                "application/json"
            );
            let scripted = at(input.get("rounds"), 1).get("json");
            let title = wtf16::stringify(scripted).and_then(|t| title_from_reply_text(&t));
            assert_eq!(title.as_deref(), event.map(|e| s(e.get("title"))), "{name}");
            assert_eq!(saves, usize::from(title.is_some()), "{name}");
        } else {
            assert!(event.is_none() && saves == 0, "{name}");
        }
    }
}

// ---- Rust-side checks of lone surrogates and the edges --------------------------------------------------

#[test]
fn a_cut_through_an_astral_character_keeps_its_half_as_node_does() {
    // The clip of the message: unit 599 is the high half of the emoji, which Node sends as "\ud83d".
    let message = format!("{}😀tail", "m".repeat(599));
    let body = title_request_json("qa-model", &message, "QA reply");
    assert!(
        body.contains(&format!("{}\\ud83d…\\n\\nAssistant", "m".repeat(599))),
        "{body}"
    );
    assert!(!body.contains('😀'));
    // A pair that fits is kept whole.
    let message = format!("{}😀tail", "m".repeat(598));
    assert!(title_request_json("qa-model", &message, "QA reply").contains('😀'));
    // A title cut at 59 units through an astral letter (the pictographs are gone by then): the half stays,
    // and with no space to cut at Node's trailing strip stops at it, keeping the `,;` before it.
    let half = lone(0xd835);
    let cut = clean_title(&format!("{},;\u{1d49c}{}", "a".repeat(56), "b".repeat(10)));
    assert_eq!(cut, Some(format!("{},;{half}…", "a".repeat(56))));
    assert_eq!(wtf16::len(cut.as_deref().unwrap()), 60);
    let cut = clean_title(&format!("{}\u{1d49c}{}", "a".repeat(58), "b".repeat(10)));
    assert_eq!(cut, Some(format!("{}{half}…", "a".repeat(58))));
    // A pair the cut keeps whole stays.
    let cut = clean_title(&format!("{}\u{1d49c}{}", "a".repeat(57), "b".repeat(10)));
    assert_eq!(cut, Some(format!("{}\u{1d49c}…", "a".repeat(57))));
    // derivedTitle repairs the pair itself.
    let derived = derived_title(&format!("{}😀{}", "a".repeat(78), "b".repeat(10)));
    assert_eq!(derived, format!("{}…", "a".repeat(78)));
}

#[test]
fn a_lone_surrogate_in_the_reply_behaves_as_in_node_inside_and_outside_content() {
    let high = lone(0xd83d);
    // Outside `content`: a value, a key, the second choice. The title is read as without it.
    for text in [
        r#"{"id":"qa-\ud83d","choices":[{"message":{"content":"QA Topic"}}]}"#,
        r#"{"\ud83d":1,"choices":[{"message":{"role":"a\ude00","content":"QA Topic"}}]}"#,
        r#"{"choices":[{"message":{"content":"QA Topic"}},{"message":{"content":"\ud83d"}}]}"#,
    ] {
        assert_eq!(
            title_from_reply_text(text).as_deref(),
            Some("QA Topic"),
            "{text}"
        );
    }
    // Inside `content`: it is one unit, kept, and counted as one.
    let text =
        |content: &str| format!(r#"{{"choices":[{{"message":{{"content":"{content}"}}}}]}}"#);
    assert_eq!(
        title_from_reply_text(&text(r"QA \ud83d Topic")),
        Some(format!("QA {high} Topic"))
    );
    // A lone surrogate alone is no letter: no title, as in Node.
    assert_eq!(title_from_reply_text(&text(r"\ud83d")), None);
    // 58 + 1 + 10 units: cut at 59 units, the half stays; the 60-unit title is 58 + half + ellipsis.
    let long = format!("{}\\ud83d{}", "a".repeat(58), "b".repeat(10));
    let title = title_from_reply_text(&text(&long)).unwrap();
    assert_eq!(title, format!("{}{high}…", "a".repeat(58)));
    assert_eq!(wtf16::len(&title), 60);
    // The half counts as one unit at the limit: 59 + 1 is 60 units and fits whole.
    let fits = format!("{}\\ud83d", "a".repeat(59));
    assert_eq!(
        title_from_reply_text(&text(&fits)),
        Some(format!("{}{high}", "a".repeat(59)))
    );
    // Text Node refuses is no title; the named differences give none as well.
    assert_eq!(title_from_reply_text("{"), None);
    assert_eq!(title_from_reply_text(&text("QA \u{10f800} Topic")), None);
    let deep = format!("{}{}", "[".repeat(300), "]".repeat(300));
    assert_eq!(title_from_reply_text(&deep), None);
}

#[test]
fn title_from_reply_reads_only_the_first_choice_and_a_string() {
    let reply = |c: &str| format!(r#"{{"choices":[{{"message":{{"content":{c}}}}}]}}"#);
    assert_eq!(
        title_from_reply_text(&reply(r#""QA Topic""#)).as_deref(),
        Some("QA Topic")
    );
    assert_eq!(title_from_reply_text(&reply(r#"["QA Topic"]"#)), None);
    assert_eq!(title_from_reply_text(&reply("null")), None);
    assert_eq!(title_from_reply_text("{}"), None);
    assert_eq!(title_from_reply_text("null"), None);
    assert_eq!(title_from_reply_text(r#"{"choices":"QA"}"#), None);
    assert_eq!(title_from_reply(&JValue::Undefined), None);
}

#[test]
fn asking_for_a_title_stops_at_the_first_fact_that_fails() {
    let ok = AskFacts {
        store_wired: true,
        paused: false,
        round_has_content: true,
        reply: "QA reply",
        chat_id: "qa-chat",
        compact_only: false,
        diary: false,
        local: true,
        machine: false,
    };
    let never = || -> bool { panic!("read before an earlier fact held") };
    assert!(ask_for_title(&ok, || true, || true, || false));
    for bad in [
        AskFacts {
            store_wired: false,
            ..ok
        },
        AskFacts { paused: true, ..ok },
        AskFacts {
            round_has_content: false,
            ..ok
        },
        AskFacts {
            reply: " \u{a0}\n",
            ..ok
        },
        AskFacts { chat_id: "", ..ok },
        AskFacts {
            compact_only: true,
            ..ok
        },
        AskFacts { diary: true, ..ok },
        AskFacts { local: false, ..ok },
    ] {
        assert!(!ask_for_title(&bad, never, never, never));
    }
    assert!(!ask_for_title(&ok, || false, never, never));
    assert!(!ask_for_title(&ok, || true, || false, never));
    assert!(!ask_for_title(&ok, || true, || true, || true));
    // An owner's machine skips the busy check.
    assert!(ask_for_title(
        &AskFacts {
            machine: true,
            ..ok
        },
        || true,
        || true,
        never
    ));
    assert!(title_due(true, never) && !title_due(false, || false) && title_due(false, || true));
}

/// A deterministic spread of nasty strings through every function: none may panic.
#[test]
fn no_input_panics() {
    let alphabet: Vec<&str> = vec![
        "a",
        "Z",
        " ",
        "\t",
        "\n",
        "\r\n",
        "\u{a0}",
        "\u{85}",
        "\u{feff}",
        "\u{2028}",
        "😀",
        "🇳🇱",
        "👨\u{200d}👩",
        "\u{fe0f}",
        "日",
        "é",
        "\"",
        "'",
        "“",
        "”",
        "`",
        "«",
        "#",
        "*",
        "**",
        ".",
        ",",
        "…",
        ":",
        "-",
        "–",
        "Title:",
        "title",
        "Chat ",
        "<think>",
        "</think>",
        "<|",
        "|>",
        "|",
        ">",
        "<",
        "\u{0}",
        "ß",
        "İ",
        "ı",
        "\u{10f83d}",
        "\u{10fe00}",
        "\u{1d49c}",
    ];
    let mut state = 0x2545_f491_4f6c_dd1d_u64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for round in 0..6000 {
        let len = [3, 12, 40, 90, 200][round % 5];
        let text: String = (0..len)
            .map(|_| alphabet[(next() % alphabet.len() as u64) as usize])
            .collect();
        let _ = derived_title(&text);
        let _ = title_request_json("qa-model", &text, &text);
        let _ = is_derived_title(&JValue::Str(derived_title(&text)), &text);
        let _ = is_derived_title(&JValue::Str(text.clone()), &text);
        let turn = JValue::obj([("role", "user".into()), ("content", text.clone().into())]);
        let _ = first_user_message(&JValue::Arr(vec![turn]), &text);
        let _ = can_request("m", &text, &text);
        // The reply text: the spread as `content`, as a key and as a bare value, through the crate function.
        let quoted = JValue::Str(text.clone());
        let reply = JValue::obj([(
            "choices",
            JValue::Arr(vec![JValue::obj([(
                "message",
                JValue::obj([("content", quoted.clone())]),
            )])]),
        )]);
        let _ = title_from_reply_text(&wtf16::stringify(&reply).unwrap());
        let _ = title_from_reply_text(&wtf16::stringify(&JValue::Arr(vec![quoted])).unwrap());
        let _ = title_from_reply_text(&text);
        assert!(wtf16::len(&derived_title(&text)) <= CHAT_TITLE_MAX.max(wtf16::len(&text)));
        if let Some(t) = clean_title(&text) {
            assert!(wtf16::len(&t) <= TITLE_MAX);
        }
    }
}
