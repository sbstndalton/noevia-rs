//! Property tests and caps for mcp-frame (noevia#980): no panics on any input, output bounded by
//! the budget, and linear time on large hostile inputs.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use mcp_frame::schema::{resolve_schema_refs, InputError, SchemaError, MAX_SCHEMA_UNITS};
use mcp_frame::{parse_rpc_body, rpc_reply, Expected, Outcome, TooLarge, MAX_BODY_UNITS};
use proptest::prelude::*;
use std::time::{Duration, Instant};

fn u(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

/// JSON-ish text built from tokens that matter, plus raw units.
fn jsonish() -> impl Strategy<Value = Vec<u16>> {
    let tok = prop_oneof![
        Just("{".to_owned()),
        Just("}".to_owned()),
        Just("[".to_owned()),
        Just("]".to_owned()),
        Just(",".to_owned()),
        Just(":".to_owned()),
        Just("\"id\"".to_owned()),
        Just("\"method\"".to_owned()),
        Just("\"$ref\"".to_owned()),
        Just("\"#/$defs/A\"".to_owned()),
        Just("\"$defs\"".to_owned()),
        Just("\"A\"".to_owned()),
        Just("\"__proto__\"".to_owned()),
        Just("1".to_owned()),
        Just("-0".to_owned()),
        Just("1e400".to_owned()),
        Just("null".to_owned()),
        Just("\"\\ud800\"".to_owned()),
        Just("data: ".to_owned()),
        Just("\n".to_owned()),
        Just("\r\n".to_owned()),
        "[ -~]{0,4}".prop_map(|s| s),
    ];
    (
        proptest::collection::vec(tok, 0..60),
        proptest::collection::vec(any::<u16>(), 0..4),
    )
        .prop_map(|(t, raw)| {
            let mut v = u(&t.concat());
            v.extend(raw);
            v
        })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(4000))]

    #[test]
    fn rpc_never_panics_and_is_deterministic(text in jsonish(), sse in any::<bool>(), id in prop_oneof![Just(Expected::Number(1.0)), Just(Expected::Null), Just(Expected::Never), Just(Expected::String(vec![0x31]))]) {
        let a = parse_rpc_body(sse, &text, &id).unwrap();
        prop_assert_eq!(&a, &parse_rpc_body(sse, &text, &id).unwrap());
        let r = rpc_reply(&text, &a);
        prop_assert!(std::str::from_utf8(&r[1..]).is_ok());
        if let Outcome::Reply(s) | Outcome::Mismatch(s) = &a {
            prop_assert!(s.end <= text.len());
        }
    }

    #[test]
    fn rpc_raw_units(text in proptest::collection::vec(any::<u16>(), 0..200), sse in any::<bool>()) {
        let _ = parse_rpc_body(sse, &text, &Expected::Number(1.0)).unwrap();
    }

    #[test]
    fn schema_never_panics(text in jsonish()) {
        match resolve_schema_refs(&text) {
            Ok(Ok(out)) => prop_assert!(std::str::from_utf8(&out).is_ok()),
            Ok(Err(e)) => prop_assert!(std::str::from_utf8(&e.json()).is_ok()),
            Err(e) => prop_assert_eq!(e, InputError::NotJson),
        }
    }

    /// Any fan-out of refs ends within the budget: an error or an output whose size the budget bounds.
    #[test]
    fn schema_budget_respected(levels in 1usize..8, fan in 1usize..60, desc in 0usize..3000) {
        let mut defs = String::new();
        for i in 0..levels {
            let props: Vec<String> = (0..fan).map(|j| format!("\"p{j}\":{{\"$ref\":\"#/$defs/D{}\",\"description\":\"{}\"}}", i + 1, "d".repeat(desc))).collect();
            defs.push_str(&format!("\"D{i}\":{{\"properties\":{{{}}}}},", props.join(",")));
        }
        defs.push_str(&format!("\"D{levels}\":{{\"type\":\"string\"}}"));
        let t = u(&format!("{{\"$ref\":\"#/$defs/D0\",\"$defs\":{{{defs}}}}}"));
        let start = Instant::now();
        match resolve_schema_refs(&t).unwrap() {
            Ok(out) => prop_assert!(out.len() < 4 * 1024 * 1024),
            Err(e) => prop_assert!(matches!(e, SchemaError::Nodes | SchemaError::Chars | SchemaError::RefDepth)),
        }
        prop_assert!(start.elapsed() < Duration::from_secs(5));
    }
}

#[test]
fn caps() {
    let big = vec![0x20u16; MAX_BODY_UNITS + 1];
    assert_eq!(parse_rpc_body(true, &big, &Expected::Never), Err(TooLarge));
    let big = vec![0x20u16; MAX_SCHEMA_UNITS + 1];
    assert_eq!(resolve_schema_refs(&big), Err(InputError::TooLarge));
}

/// Large hostile inputs finish in time linear in their size (generous bounds for debug builds).
#[test]
fn linear_time() {
    let n = MAX_BODY_UNITS / 2 - 2;
    let cases: Vec<(bool, Vec<u16>)> = vec![
        (false, u(&("[".repeat(n) + &"]".repeat(n)))),
        (
            true,
            u(&"data: {\"id\":1,\"x\":[[[[\n".repeat(MAX_BODY_UNITS / 24)),
        ),
        (true, u(&"data: {\"id\":2}\n".repeat(MAX_BODY_UNITS / 16))),
        (
            true,
            u(&("data: \"".to_owned() + &"\\u0041".repeat(MAX_BODY_UNITS / 6 - 2))),
        ),
        (
            false,
            u(&format!("{{\"id\":{}}}", "9".repeat(MAX_BODY_UNITS - 10))),
        ),
        (true, vec![0x0a; MAX_BODY_UNITS]),
    ];
    for (sse, text) in cases {
        assert!(text.len() <= MAX_BODY_UNITS);
        let start = Instant::now();
        let _ = parse_rpc_body(sse, &text, &Expected::Number(1.0)).unwrap();
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "{:?}",
            start.elapsed()
        );
    }
    let m = MAX_SCHEMA_UNITS / 2 - 40;
    let schemas = [
        format!("{{\"$defs\":{{\"a\":{}{}}}}}", "[".repeat(m), "]".repeat(m)),
        format!("{{\"$defs\":{{\"a\":[{}0]}}}}", "0,".repeat(m - 4)),
        format!(
            "{{\"a\":{{\"$ref\":\"#/$defs/A\",\"x\":{}{}}},\"$defs\":{{\"A\":1}}}}",
            "[".repeat(m - 20),
            "]".repeat(m - 20)
        ),
    ];
    for s in schemas {
        let t = u(&s);
        assert!(t.len() <= MAX_SCHEMA_UNITS);
        let start = Instant::now();
        let _ = resolve_schema_refs(&t).unwrap();
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "{:?}",
            start.elapsed()
        );
    }
}
