//! The recorded Node chat titles (`fixtures/node-titles.v1.json`, written by `record_titles.cjs` from
//! noevia-core's `server/chat-title.cjs`) replayed against `chat_turn::title`, and the four whole-turn title
//! cases of `node-turns.v1.json` checked against the same functions.
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
    title_from_reply, title_request_body, title_request_json, AskFacts, AttemptCounter,
    CHAT_TITLE_MAX, MAX_ATTEMPTS, TITLE_MAX, TITLE_SYSTEM,
};
use common::{case, fixture as turns, request_body, same_json};
use serde_json::{json, Value};

type Test = fn(char) -> bool;

fn titles() -> Value {
    serde_json::from_str(include_str!("fixtures/node-titles.v1.json")).unwrap()
}

fn s(v: &Value) -> &str {
    v.as_str().unwrap()
}

fn list(v: &Value) -> &Vec<Value> {
    v.as_array().unwrap()
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
    assert_eq!(f["version"], 1);
    assert_eq!(
        f["sources"]["server/chat-title.cjs"].as_str().map(str::len),
        Some(64)
    );
    // The whole-turn recording was made from the same source file.
    assert_eq!(
        f["sources"]["server/chat-title.cjs"],
        turns()["sources"]["server/chat-title.cjs"]
    );
    assert_eq!(f["constants"]["CHAT_TITLE_MAX"], CHAT_TITLE_MAX);
    assert_eq!(f["constants"]["TITLE_MAX"], TITLE_MAX);
    assert_eq!(f["constants"]["MAX_ATTEMPTS"], MAX_ATTEMPTS);
    assert_eq!(s(&f["constants"]["TITLE_SYSTEM"]), TITLE_SYSTEM);
    assert_eq!(f["counters"]["defaults"]["maxAttempts"], MAX_ATTEMPTS);
    assert_eq!(f["counters"]["defaults"]["evictedAfterOtherKeys"], 2000);
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
        let ranges = parse_ranges(s(&f["unicode"][name]));
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
}

#[test]
fn derived_titles_are_node_s() {
    let f = titles();
    assert!(list(&f["derived"]).len() >= 40);
    for c in list(&f["derived"]) {
        assert_eq!(
            derived_title(s(&c["text"])),
            s(&c["title"]),
            "{}",
            c["text"]
        );
    }
}

#[test]
fn is_derived_title_is_node_s() {
    let f = titles();
    let mut yes = 0;
    for c in list(&f["isDerived"]) {
        let title = if c["titleAbsent"] == true {
            None
        } else {
            Some(&c["title"])
        };
        let got = is_derived_title(title, s(&c["firstMessage"]));
        assert_eq!(got, c["result"], "{c}");
        yes += usize::from(got);
    }
    assert!(yes > 20 && yes < list(&f["isDerived"]).len() - 20);
    // The old bare cut that splits an astral character: Node can hold that lone-surrogate title and says yes.
    // Rust cannot hold it, and every well-formed title is judged as in Node.
    let split = &f["isDerivedSplitCut"];
    assert_eq!(split["node"], true);
    assert_eq!(split["wellFormedTitle"], false);
    let message = s(&split["firstMessage"]);
    let cut: Vec<u16> = message.encode_utf16().take(CHAT_TITLE_MAX).collect();
    let well_formed: String = char::decode_utf16(cut).filter_map(Result::ok).collect();
    assert_eq!(
        is_derived_title(Some(&json!(well_formed)), message),
        split["wellFormedTitle"]
    );
}

#[test]
fn the_first_reply_and_the_first_user_message_are_node_s() {
    let f = titles();
    for c in list(&f["firstOf"]) {
        let history = if c["historyAbsent"] == true {
            &Value::Null
        } else {
            &c["history"]
        };
        let message = s(&c["message"]);
        assert_eq!(first_reply(history), c["firstReply"], "{c}");
        assert_eq!(
            first_user_message(history, message),
            s(&c["firstUserMessage"]),
            "{c}"
        );
    }
}

#[test]
fn the_title_request_is_what_generate_title_sends() {
    let f = titles();
    let mut cut_pairs = 0;
    for c in list(&f["requests"]) {
        let (model, message, reply) = (s(&c["model"]), s(&c["message"]), s(&c["reply"]));
        assert!(can_request(model, message, reply));
        let calls = list(&c["calls"]);
        assert_eq!(calls.len(), 1);
        let call = &calls[0];
        assert_eq!(call["method"], "POST");
        assert_eq!(call["redirect"], "error");
        assert_eq!(call["signal"], true);
        assert_eq!(call["headers"]["Content-Type"], "application/json");
        let node: Value = serde_json::from_str(s(&call["bodyText"])).unwrap();
        assert!(
            same_json(&title_request_body(model, message, reply), &node),
            "{message:?}"
        );
        assert_eq!(
            title_request_json(model, message, reply),
            s(&call["bodyText"]),
            "{message:?}"
        );
        // The bytes parse back to the value.
        let parsed: Value =
            serde_json::from_str(&title_request_json(model, message, reply)).unwrap();
        assert!(same_json(&parsed, &node));
        cut_pairs += usize::from(call["bodyLoneSurrogates"].as_u64().unwrap() > 0);
        assert_eq!(c["title"], "QA Title");
    }
    assert!(
        cut_pairs >= 4,
        "the recording cuts astral characters at the clip"
    );
}

#[test]
fn a_blank_model_message_or_reply_makes_no_request() {
    let f = titles();
    assert!(list(&f["skipped"]).len() >= 8);
    for c in list(&f["skipped"]) {
        let (model, message, reply) = (
            c["model"].as_str().unwrap_or(""),
            c["message"].as_str().unwrap_or(""),
            c["reply"].as_str().unwrap_or(""),
        );
        assert_eq!(can_request(model, message, reply), c["called"], "{c}");
        assert_eq!(c["title"].is_null(), c["called"] == false, "{c}");
    }
}

#[test]
fn replies_are_read_and_cleaned_as_node_does() {
    let f = titles();
    let (mut some, mut none, mut lone) = (0, 0, 0);
    for c in list(&f["replies"]) {
        let got = title_from_reply(&c["response"]);
        let want = c["title"].as_str().map(str::to_owned);
        assert_eq!(got, want, "{}: {}", c["label"], c["response"]);
        if let Some(raw) = c["response"]["choices"][0]["message"]["content"].as_str() {
            assert_eq!(clean_title(raw), want, "{raw:?}");
        }
        if let Some(t) = got {
            some += 1;
            assert!(!t.is_empty() && t.encode_utf16().count() <= TITLE_MAX);
        } else {
            none += 1;
        }
        lone += c["titleLoneSurrogates"].as_u64().unwrap();
    }
    // Node's titles cut through an astral letter end in half of it; the recording holds them with it deleted.
    assert!(
        lone >= 5,
        "the recording cuts titles through astral letters"
    );
    assert!(some > 100 && none > 40, "{some} titles, {none} none");
}

#[test]
fn the_attempt_counter_is_node_s() {
    let f = titles();
    assert!(list(&f["counters"]["runs"]).len() >= 10);
    for run in list(&f["counters"]["runs"]) {
        let max = run["options"]["max"]
            .as_u64()
            .map_or(MAX_ATTEMPTS, |m| m as u32);
        let max_chats = run["options"]["maxChats"]
            .as_u64()
            .map_or(2000, |m| m as usize);
        let mut counter = AttemptCounter::new(max, max_chats);
        for (i, op) in list(&run["ops"]).iter().enumerate() {
            let key = s(&op["key"]);
            match s(&op["op"]) {
                "allowed" => assert_eq!(json!(counter.allowed(key)), op["result"], "{i}: {op}"),
                _ => assert_eq!(json!(counter.record(key)), op["result"], "{i}: {op}"),
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

/// The reply a turn streamed: every content delta of its first scripted round.
fn streamed_reply(case: &Value) -> String {
    list(&case["input"]["rounds"][0]["frames"])
        .iter()
        .filter_map(|f| f["choices"][0]["delta"]["content"].as_str())
        .collect()
}

#[test]
fn the_whole_turn_title_cases_agree_with_the_module() {
    let f = turns();
    for name in [
        "title",
        "title_renamed",
        "title_still_derived",
        "title_failed",
    ] {
        let c = case(name);
        let input = &c["input"];
        let (model, message) = (s(&input["project"]["model"]), s(&input["body"]["message"]));
        let history = &input["body"]["history"];
        let reply = streamed_reply(&c);
        let requests = list(&c["requests"]);
        let first = first_reply(history);
        let auto_named = input["titles"]["autoNamed"] == true;
        let attempts = AttemptCounter::default();
        let key = attempt_key(
            None,
            Some(s(&input["project"]["id"])),
            s(&input["body"]["chatId"]),
        );

        // Is a title asked for? The turn made a second engine request exactly then.
        let facts = AskFacts {
            store_wired: input["titles"].is_object(),
            paused: false,
            round_has_content: true,
            reply: &reply,
            chat_id: s(&input["body"]["chatId"]),
            compact_only: false,
            diary: false,
            local: true,
            machine: false,
        };
        let asked = ask_for_title(
            &facts,
            || title_due(first, || auto_named),
            || attempts.allowed(&key),
            || false,
        );
        assert_eq!(asked, requests.len() == 2, "{name}");

        // The store is read for the first message exactly when the history has an assistant turn.
        let ops = list(&c["stores"]);
        let reads: Vec<&Value> = ops
            .iter()
            .filter(|o| o["op"] == "chatTitles.autoNamed")
            .collect();
        assert_eq!(reads.len(), usize::from(!first), "{name}");
        let first_message = first_user_message(history, message);
        for op in ops
            .iter()
            .filter(|o| s(&o["op"]).starts_with("chatTitles."))
        {
            assert_eq!(s(&op["first"]), first_message, "{name}");
        }

        let event = list(&c["events"]).iter().find(|e| e["type"] == "title");
        let saves: Vec<&Value> = ops
            .iter()
            .filter(|o| o["op"] == "chatTitles.save")
            .collect();
        if asked {
            let request = &requests[1];
            assert!(can_request(model, message, &reply));
            assert!(
                same_json(
                    &title_request_body(model, message, &reply),
                    &request_body(&f, request)
                ),
                "{name}"
            );
            assert_eq!(request["headers"]["Content-Type"], "application/json");
            let scripted = &input["rounds"][1];
            let title = scripted.get("json").and_then(title_from_reply);
            assert_eq!(title.as_deref(), event.map(|e| s(&e["title"])), "{name}");
            assert_eq!(saves.len(), usize::from(title.is_some()), "{name}");
        } else {
            assert!(event.is_none() && saves.is_empty(), "{name}");
        }
    }
}

// ---- Rust-side checks of the named differences and the edges ----------------------------------------

#[test]
fn a_cut_through_an_astral_character_drops_the_half_pair() {
    // The clip of the message: unit 599 is the high half of the emoji, which Node sends as "\ud83d".
    let message = format!("{}😀tail", "m".repeat(599));
    let body = title_request_json("qa-model", &message, "QA reply");
    assert!(
        body.contains(&format!("{}…\\n\\nAssistant", "m".repeat(599))),
        "{body}"
    );
    assert!(!body.contains("\\ud83d") && !body.contains('😀'));
    // A pair that fits is kept whole.
    let message = format!("{}😀tail", "m".repeat(598));
    assert!(title_request_json("qa-model", &message, "QA reply").contains('😀'));
    // A title cut at 59 units through an astral letter (the pictographs are gone by then). With no space to cut
    // at, Node's trailing strip stops at the lone surrogate and keeps the `,;` before it; Rust deletes the
    // surrogate afterwards, so the punctuation stays too.
    let cut = clean_title(&format!(
        "{},;{}{}",
        "a".repeat(56),
        "\u{1d49c}",
        "b".repeat(10)
    ));
    assert_eq!(cut.as_deref(), Some(&*format!("{},;…", "a".repeat(56))));
    let cut = clean_title(&format!(
        "{}{}{}",
        "a".repeat(58),
        "\u{1d49c}",
        "b".repeat(10)
    ));
    assert_eq!(cut.as_deref(), Some(&*format!("{}…", "a".repeat(58))));
    // A pair the cut keeps whole stays.
    let cut = clean_title(&format!(
        "{}{}{}",
        "a".repeat(57),
        "\u{1d49c}",
        "b".repeat(10)
    ));
    assert_eq!(
        cut.as_deref(),
        Some(&*format!("{}\u{1d49c}…", "a".repeat(57)))
    );
    // derivedTitle repairs the pair itself, in Node and here.
    let derived = derived_title(&format!("{}😀{}", "a".repeat(78), "b".repeat(10)));
    assert_eq!(derived, format!("{}…", "a".repeat(78)));
}

#[test]
fn a_reply_with_a_lone_surrogate_escape_is_no_reply_here() {
    // serde_json refuses the escape, so the caller never has a value to read.
    assert!(
        serde_json::from_str::<Value>(r#"{"choices":[{"message":{"content":"QA \ud83d"}}]}"#)
            .is_err()
    );
}

#[test]
fn title_from_reply_reads_only_the_first_choice_and_a_string() {
    let reply = |c: Value| json!({ "choices": [{ "message": { "content": c } }] });
    assert_eq!(
        title_from_reply(&reply(json!("QA Topic"))).as_deref(),
        Some("QA Topic")
    );
    assert_eq!(title_from_reply(&reply(json!(["QA Topic"]))), None);
    assert_eq!(title_from_reply(&reply(json!(null))), None);
    assert_eq!(title_from_reply(&json!({})), None);
    assert_eq!(title_from_reply(&Value::Null), None);
    assert_eq!(title_from_reply(&json!({ "choices": "QA" })), None);
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
        let _ = clean_title(&text);
        let _ = title_request_json("qa-model", &text, &text);
        let _ = title_request_body("qa-model", &text, &text);
        let _ = is_derived_title(Some(&json!(derived_title(&text))), &text);
        let _ = is_derived_title(Some(&json!(text.clone())), &text);
        let _ = first_user_message(&json!([{"role": "user", "content": text}]), &text);
        let _ = can_request("m", &text, &text);
        assert!(
            derived_title(&text).encode_utf16().count()
                <= CHAT_TITLE_MAX.max(text.encode_utf16().count())
        );
        if let Some(t) = clean_title(&text) {
            assert!(t.encode_utf16().count() <= TITLE_MAX);
        }
    }
}
