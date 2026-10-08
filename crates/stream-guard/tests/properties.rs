//! Properties over generated JSON-ish text, schemas and arbitrary bytes: no panics; refusals are
//! fixed codes that never echo input; any split of a text within maxBytes gives the same first
//! violation and `done` as the whole text (cutting a surrogate pair is harmless far from
//! maxBytes; over it, as in the JS, a split may meet an earlier violation first); the state
//! survives encode/decode at every boundary and stays linear in the input; and, from an oracle
//! independent of the validator (serde_json's writer), every JSON document is accepted under
//! `{}` and every truncation of a container is not.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use proptest::prelude::*;
use stream_guard::{
    call, utf8_len, Reason, Schema, State, Validator, Violation, DEFAULT_MAX_BYTES,
    DEFAULT_MAX_DEPTH,
};

const SCHEMAS: [&str; 6] = [
    "{}",
    r#"{"type":"object","required":["a"],"additionalProperties":false,"properties":{"a":{"type":"string","enum":["x","xy","\u00e9\ud800"]},"b":{"type":["integer","null"],"enum":[1,null,2]},"c":{"type":"array","maxItems":3,"items":{"type":"string","maxLength":4}}}}"#,
    r#"{"type":"array","items":{"type":"object","properties":{"k":{"enum":[true,false]}}}}"#,
    r#"{"type":["string","number"],"maxLength":5}"#,
    r#"{"properties":{"\ud800":{"type":"null"}},"additionalProperties":false}"#,
    r#"{"type":"integer","enum":[0,-0,10,1e2]}"#,
];

const FIXED: [&str; 5] = [
    r#"{"error":"too_large"}"#,
    r#"{"error":"input_shape"}"#,
    r#"{"error":"schema"}"#,
    r#"{"error":"state"}"#,
    r#"{"error":"options"}"#,
];

fn text_strategy() -> impl Strategy<Value = Vec<u16>> {
    let pieces: Vec<&'static [u16]> = vec![
        &[0x7b],
        &[0x7d],
        &[0x5b],
        &[0x5d],
        &[0x22],
        &[0x3a],
        &[0x2c],
        &[0x5c],
        &[0x20],
        &[0x75],
        &[0x30],
        &[0x31],
        &[0x2d],
        &[0x2e],
        &[0x65],
        &[0x61],
        &[0x62],
        &[0x63],
        &[0x6b],
        &[0x78],
        &[0x79],
        &[0x74],
        &[0x66],
        &[0x6e],
        &[0x6c],
        &[0xe9],
        &[0xd800],
        &[0xdc00],
        &[0xd83d, 0xde00],
        &[0x2028],
        &[0xa0],
        &[0xfeff],
        &[0x00],
    ];
    let words: Vec<&'static str> = vec![
        "{\"a\":",
        "\"x\"",
        "\"xy\"",
        "\"c\":[",
        "\"b\":",
        "true",
        "false",
        "null",
        "1.5e3",
        "-0",
        "\"\\u00e9\\ud800\"",
        "\"k\":",
        "\\ud800",
        "\"\\ud800\":",
        "\"abcd\"",
        "]}",
        "},{",
        "10",
        "1e2",
    ];
    let unit = prop_oneof![
        3 => proptest::sample::select(pieces).prop_map(|p| p.to_vec()),
        2 => proptest::sample::select(words).prop_map(|w| w.encode_utf16().collect()),
    ];
    proptest::collection::vec(unit, 0..40).prop_map(|v| v.concat())
}

fn run(
    schema: &Schema,
    max_bytes: i64,
    chunks: &[&[u16]],
    roundtrip: bool,
) -> (Option<Violation>, bool) {
    let mut st = State::new(DEFAULT_MAX_DEPTH, max_bytes).unwrap();
    for c in chunks {
        if roundtrip {
            let bytes = st.encode();
            st = State::decode(&bytes, schema).unwrap();
            assert_eq!(State::decode(&st.encode(), schema).unwrap(), st);
        }
        let mut v = Validator::new(schema, st);
        v.feed(c);
        st = v.into_state();
    }
    let mut v = Validator::new(schema, st);
    v.end();
    let st = v.into_state();
    (st.violation().cloned(), st.is_done())
}

/// Cut `text` at `cuts` (sorted, deduplicated), optionally moving each cut off a surrogate pair.
fn cut<'a>(text: &'a [u16], cuts: &[usize], keep_pairs: bool) -> Vec<&'a [u16]> {
    let mut points: Vec<usize> = cuts
        .iter()
        .map(|&c| {
            let c = c % (text.len() + 1);
            let inside_pair = c > 0
                && c < text.len()
                && (0xd800..=0xdbff).contains(&text[c - 1])
                && (0xdc00..=0xdfff).contains(&text[c]);
            if keep_pairs && inside_pair {
                c + 1
            } else {
                c
            }
        })
        .collect();
    points.sort_unstable();
    points.dedup();
    let mut out = Vec::new();
    let mut prev = 0;
    for p in points {
        out.push(&text[prev..p]);
        prev = p;
    }
    out.push(&text[prev..]);
    out
}

fn json_value() -> impl Strategy<Value = serde_json::Value> {
    let leaf = prop_oneof![
        Just(serde_json::Value::Null),
        any::<bool>().prop_map(serde_json::Value::Bool),
        any::<i64>().prop_map(|n| serde_json::json!(n)),
        any::<f64>()
            .prop_filter("finite", |f| f.is_finite())
            .prop_map(|f| serde_json::json!(f)),
        "\\PC{0,8}".prop_map(serde_json::Value::String),
    ];
    leaf.prop_recursive(6, 48, 6, |inner| {
        prop_oneof![
            proptest::collection::vec(inner.clone(), 0..6).prop_map(serde_json::Value::Array),
            proptest::collection::btree_map("\\PC{0,6}", inner, 0..6)
                .prop_map(|m| serde_json::Value::Object(m.into_iter().collect())),
        ]
    })
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 512, ..ProptestConfig::default() })]

    #[test]
    fn any_input_gives_a_reply_or_a_fixed_code(input in proptest::collection::vec(any::<u8>(), 0..512)) {
        let (status, reply) = call(&input);
        if status != 0 {
            prop_assert_eq!(status, 1);
            prop_assert!(FIXED.contains(&std::str::from_utf8(&reply).unwrap()));
        }
    }

    #[test]
    fn corrupt_states_are_refused_or_decoded(
        s in 0usize..SCHEMAS.len(),
        text in text_strategy(),
        flips in proptest::collection::vec((any::<usize>(), any::<u8>()), 1..4),
        more in text_strategy(),
    ) {
        let schema = Schema::parse(SCHEMAS[s].as_bytes()).unwrap();
        let mut v = Validator::new(&schema, State::new(DEFAULT_MAX_DEPTH, DEFAULT_MAX_BYTES).unwrap());
        v.feed(&text);
        let mut bytes = v.into_state().encode();
        for (at, x) in flips {
            let n = bytes.len();
            bytes[at % n] ^= x | 1;
        }
        let mut req = vec![1u8];
        req.extend_from_slice(&(SCHEMAS[s].len() as u32).to_le_bytes());
        req.extend_from_slice(SCHEMAS[s].as_bytes());
        req.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        req.extend_from_slice(&bytes);
        req.push(0);
        req.extend(more.iter().flat_map(|u| u.to_le_bytes()));
        let (status, reply) = call(&req);
        if status != 0 {
            prop_assert!(FIXED.contains(&std::str::from_utf8(&reply).unwrap()));
        }
        // A refused state never echoes what it held.
        if let Ok(state) = State::decode(&bytes, &schema) {
            let mut v = Validator::new(&schema, state);
            v.feed(&more);
            v.end();
        }
    }

    #[test]
    fn splits_off_surrogate_pairs_never_change_the_answer(
        s in 0usize..SCHEMAS.len(),
        text in text_strategy(),
        cuts in proptest::collection::vec(any::<usize>(), 0..6),
        max_bytes in prop_oneof![Just(DEFAULT_MAX_BYTES), 0i64..80],
    ) {
        let schema = Schema::parse(SCHEMAS[s].as_bytes()).unwrap();
        let whole = run(&schema, max_bytes, &[&text], false);
        let parts = cut(&text, &cuts, true);
        let split = run(&schema, max_bytes, &parts, false);
        prop_assert_eq!(&run(&schema, max_bytes, &parts, true), &split);
        if i128::from(utf8_len(&text)) <= i128::from(max_bytes) {
            prop_assert_eq!(&split, &whole);
        } else {
            // Over the cap the JS reports it as soon as the chunk that crosses it arrives, so a
            // split can first meet an earlier violation in a chunk under the cap; both refuse.
            prop_assert!(split.0.is_some() && whole.0.is_some());
            prop_assert_eq!(whole.0.unwrap().reason, Reason::MaxBytes);
        }
    }

    #[test]
    fn any_split_is_the_same_far_from_max_bytes(
        s in 0usize..SCHEMAS.len(),
        text in text_strategy(),
        cuts in proptest::collection::vec(any::<usize>(), 0..6),
    ) {
        let schema = Schema::parse(SCHEMAS[s].as_bytes()).unwrap();
        let whole = run(&schema, DEFAULT_MAX_BYTES, &[&text], false);
        let parts = cut(&text, &cuts, false);
        prop_assert_eq!(run(&schema, DEFAULT_MAX_BYTES, &parts, true), whole);
    }

    #[test]
    fn the_state_stays_linear_in_the_input(s in 0usize..SCHEMAS.len(), text in text_strategy()) {
        let schema = Schema::parse(SCHEMAS[s].as_bytes()).unwrap();
        let mut v = Validator::new(&schema, State::new(DEFAULT_MAX_DEPTH, DEFAULT_MAX_BYTES).unwrap());
        for i in 0..text.len() {
            v.feed(&text[i..=i]);
            prop_assert!(v.state().encode().len() <= 512 + 48 * (i + 1));
        }
    }

    #[test]
    fn every_json_document_is_accepted_under_the_empty_schema(
        value in json_value(),
        cuts in proptest::collection::vec(any::<usize>(), 0..5),
    ) {
        let schema = Schema::parse(b"{}").unwrap();
        let text: Vec<u16> = serde_json::to_string(&value).unwrap().encode_utf16().collect();
        let parts = cut(&text, &cuts, false);
        prop_assert_eq!(run(&schema, DEFAULT_MAX_BYTES, &parts, true), (None, true));
        if matches!(value, serde_json::Value::Array(_) | serde_json::Value::Object(_)) {
            // Any proper prefix of a container is not a document.
            let k = cuts.first().copied().unwrap_or(0) % text.len();
            let (v, done) = run(&schema, DEFAULT_MAX_BYTES, &[&text[..k]], false);
            prop_assert!(v.is_some() && !done);
        }
    }
}
