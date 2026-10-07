//! Property tests for sandbox-bridge (noevia#999): containment cannot be escaped, framing does not
//! depend on how a stream is chunked, and no input makes any rule panic or hand out more than it
//! should.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use proptest::prelude::*;
use sandbox_bridge::contain::contained;
use sandbox_bridge::frame::{Framer, Pushed, DEFAULT_LIMIT};
use sandbox_bridge::json::{parse, validate, write_value, Value};
use sandbox_bridge::start::{start_json, ALLOWED_ENV};
use sandbox_bridge::tool_call::tool_call_json;

/// An independent normaliser: `.` dropped, `..` pops (never above `/`), empty segments dropped.
fn normalise(p: &str) -> Vec<&str> {
    let mut out: Vec<&str> = Vec::new();
    for seg in p.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            s => out.push(s),
        }
    }
    out
}

fn segment() -> impl Strategy<Value = String> {
    prop_oneof![
        Just(".".to_owned()),
        Just("..".to_owned()),
        Just("...".to_owned()),
        Just("..x".to_owned()),
        Just("x..".to_owned()),
        Just("a.".to_owned()),
        Just("".to_owned()),
        Just("\\".to_owned()),
        Just("..\\..".to_owned()),
        Just("\0".to_owned()),
        Just("caf\u{e9}".to_owned()),
        Just("cafe\u{301}".to_owned()),
        Just("\u{ff0e}\u{ff0e}".to_owned()),
        Just("\u{2024}\u{2024}".to_owned()),
        Just("😀".to_owned()),
        Just("/".to_owned()),
        "[a-c]{1,2}",
        any::<char>().prop_map(|c| c.to_string()),
    ]
}

fn pathish() -> impl Strategy<Value = String> {
    (any::<bool>(), prop::collection::vec(segment(), 0..8)).prop_map(|(abs, segs)| {
        let body = segs.join("/");
        if abs {
            format!("/{body}")
        } else {
            body
        }
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(4000))]

    /// Whatever the candidate (`..`, absolute jumps, NUL, backslashes, NFC/NFD, trailing dots),
    /// "contained" means the normalised candidate is the normalised root or below it.
    #[test]
    fn containment_never_escapes(root in pathish(), tail in pathish(), join in any::<bool>()) {
        let candidate = if join { format!("{root}/{tail}") } else { tail.clone() };
        let (ok, _) = contained(&root, &candidate);
        if ok {
            prop_assert!(root.starts_with('/') && candidate.starts_with('/'));
            prop_assert!(!root.contains('\0') && !candidate.contains('\0'));
            let r = normalise(&root);
            let c = normalise(&candidate);
            prop_assert!(c.len() >= r.len() && c[..r.len()] == r[..], "{root:?} {candidate:?}");
        }
    }

    /// And it is not vacuous: a plain child (names not starting with `..`) is contained.
    #[test]
    fn plain_children_are_contained(root in "(/[a-z]{1,3}){0,4}", names in prop::collection::vec("[a-z][a-z.]{0,3}|\\.[a-z]{0,3}", 0..5)) {
        let root = if root.is_empty() { "/".to_owned() } else { root };
        let candidate = format!("{root}/{}", names.join("/"));
        prop_assert!(contained(&root, &candidate).0, "{root:?} {candidate:?}");
    }

    /// Chunking does not change the lines (below the limit).
    #[test]
    fn framing_is_chunking_invariant(
        parts in prop::collection::vec(prop_oneof![
            Just("{\"a\":1}"), Just("[1,2]"), Just("\"s\""), Just("\n"), Just("\r\n"), Just("\r"),
            Just(" "), Just("{"), Just("}"), Just("x"), Just("é"), Just("😀"), Just("\u{feff}"), Just("null"),
        ], 0..40),
        cuts in prop::collection::vec(any::<prop::sample::Index>(), 0..8),
    ) {
        let text: String = parts.concat();
        let mut whole = Framer::new(DEFAULT_LIMIT);
        let Pushed::Lines(expected) = whole.push(&text) else { panic!("overflow") };
        let bounds: Vec<usize> = text.char_indices().map(|(i, _)| i).chain([text.len()]).collect();
        let mut at: Vec<usize> = cuts.iter().map(|c| bounds[c.index(bounds.len())]).collect();
        at.sort_unstable();
        let mut framer = Framer::new(DEFAULT_LIMIT);
        let mut got = Vec::new();
        let mut prev = 0;
        for cut in at.into_iter().chain([text.len()]) {
            if let Pushed::Lines(l) = framer.push(&text[prev..cut]) { got.extend(l) } else { panic!("overflow") }
            prev = cut;
        }
        prop_assert_eq!(got, expected.clone());
        for line in &expected {
            prop_assert!(validate(line) && !line.contains('\n'));
        }
    }

    /// The buffer never holds more than the limit: after an overflow framing starts empty.
    #[test]
    fn overflow_resets(limit in 0usize..64, chunks in prop::collection::vec("[a\n{}1😀]{0,40}", 1..10)) {
        let mut framer = Framer::new(limit);
        let mut held = 0usize;
        for chunk in &chunks {
            let units: usize = chunk.encode_utf16().count();
            match framer.push(chunk) {
                Pushed::Overflow(size) => {
                    prop_assert_eq!(size, held + units);
                    prop_assert!(size > limit);
                    held = 0;
                }
                Pushed::Lines(_) => {
                    let after = chunk.rsplit_once('\n').map_or(held + units, |(_, rest)| rest.encode_utf16().count());
                    prop_assert!(after <= limit);
                    held = after;
                }
            }
        }
    }

    /// `parse` accepts exactly what `validate` accepts, and re-emitting is stable.
    #[test]
    fn parse_matches_validate(s in "[\\[\\]{}\":,0-9.eE+\\-a-z \\\\\u{e9}]{0,30}") {
        let v = parse(&s);
        prop_assert_eq!(v.is_some(), validate(&s));
        if let Some(v) = v {
            let mut out = String::new();
            write_value(&v, &mut out);
            prop_assert_eq!(parse(&out), Some(v));
        }
    }

    /// serde_json's output is always JSON.parse-able.
    #[test]
    fn serde_output_validates(v in json_value()) {
        prop_assert!(validate(&serde_json::to_string(&v).unwrap()));
    }

    /// No input panics; an outside-the-workspace call is always `other` and flagged.
    #[test]
    fn tool_call_total(v in json_value(), outside in any::<bool>()) {
        let mut payload = serde_json::json!({ "toolName": "read", "input": v });
        if outside { payload["outsideWorkspace"] = serde_json::Value::Bool(true); }
        let text = serde_json::to_string(&payload).unwrap();
        let out = tool_call_json(&text).unwrap();
        let reply = parse(&out).unwrap();
        prop_assert!(reply.get("toolCallId").and_then(Value::as_str16).is_some());
        if outside {
            prop_assert_eq!(reply.get("kind"), Some(&Value::Str("other".encode_utf16().collect())));
            prop_assert_eq!(reply.get("rawInput").and_then(|r| r.get("noeviaOutsideWorkspace")), Some(&Value::Bool(true)));
        }
    }

    #[test]
    fn arbitrary_text_never_panics(s in any::<String>()) {
        let _ = tool_call_json(&s);
        let _ = start_json(&s);
        let _ = contained(&s, &s);
        let _ = Framer::new(16).push(&s);
    }

    /// Only allowlisted variables with short string values come out of a start line.
    #[test]
    fn start_env_is_allowlisted(env in prop::collection::btree_map("[A-Za-z_]{1,8}|HOME|PATH|LD_PRELOAD", json_value(), 0..6)) {
        let line = serde_json::json!({ "noevia": "start", "cwd": "/w", "env": env }).to_string();
        let text = start_json(&line);
        let reply = parse(&text).unwrap();
        let Some(Value::Obj(fields)) = reply.get("env") else { panic!("no env") };
        for (k, v) in fields {
            let k = String::from_utf16(k).unwrap();
            prop_assert!(ALLOWED_ENV.contains(&k.as_str()));
            prop_assert!(matches!(v, Value::Str(s) if s.len() < 4096));
        }
    }
}

fn json_value() -> impl Strategy<Value = serde_json::Value> {
    let leaf = prop_oneof![
        Just(serde_json::Value::Null),
        any::<bool>().prop_map(serde_json::Value::Bool),
        any::<f64>()
            .prop_filter("finite", |f| f.is_finite())
            .prop_map(|f| serde_json::json!(f)),
        any::<i64>().prop_map(|i| serde_json::json!(i)),
        any::<String>().prop_map(serde_json::Value::String),
        prop_oneof![
            Just("path"),
            Just("file_path"),
            Just("command"),
            Just("__proto__"),
            Just("toString")
        ]
        .prop_map(|s| serde_json::Value::String(s.to_owned())),
    ];
    leaf.prop_recursive(4, 32, 6, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..6).prop_map(serde_json::Value::Array),
            prop::collection::btree_map(
                prop_oneof![
                    Just("path".to_owned()),
                    Just("command".to_owned()),
                    Just("toString".to_owned()),
                    Just("noeviaOutsideWorkspace".to_owned()),
                    "[a-z0-9]{0,4}"
                ],
                inner,
                0..6
            )
            .prop_map(|m| serde_json::Value::Object(m.into_iter().collect())),
        ]
    })
}
