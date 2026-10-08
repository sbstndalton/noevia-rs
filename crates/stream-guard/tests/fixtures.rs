//! The shared fixture table (`tests/fixtures/stream-guard.v1.json`, byte-identical to
//! noevia-core's, printed by its `tools/gen-stream-guard-fixtures.cjs` from the JS
//! IncrementalValidator and buildCorrectionRequest themselves).
//!
//! Every case must agree exactly: the feed that first returns a violation, the violation's
//! message and path (unit for unit) and the rule it names, `isDone()` after `end()`; the same
//! text fed one unit at a time; every two-chunk cut of a short text. Each case also runs with
//! the state encoded and decoded between chunks, and a sample through the wire format.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use serde_json::Value;
use stream_guard::{call, correction, Schema, State, Validator, Violation};

fn fixtures() -> Value {
    serde_json::from_str(include_str!("fixtures/stream-guard.v1.json")).unwrap()
}

/// A fixture string: plain, `{ "units": [..] }` or `{ "seq": [[piece, count], ..] }`.
fn text(v: &Value) -> Vec<u16> {
    if let Some(s) = v.as_str() {
        return s.encode_utf16().collect();
    }
    if let Some(u) = v.get("units") {
        return u
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x.as_u64().unwrap() as u16)
            .collect();
    }
    let mut out = Vec::new();
    for run in v["seq"].as_array().unwrap() {
        let piece = text(&run[0]);
        for _ in 0..run[1].as_u64().unwrap() {
            out.extend_from_slice(&piece);
        }
    }
    out
}

type Got = Option<(Vec<u16>, Vec<u16>, String)>;

fn got(v: Option<&Violation>) -> Got {
    v.map(|v| {
        (
            v.message.clone(),
            v.path.clone(),
            v.reason.code().to_owned(),
        )
    })
}

fn want(v: &Value) -> Got {
    if v.is_null() {
        return None;
    }
    Some((
        text(&v["message"]),
        text(&v["path"]),
        v["reason"].as_str().unwrap().to_owned(),
    ))
}

fn options(row: &Value) -> (i64, i64) {
    let o = &row["o"];
    (
        o["maxDepth"]
            .as_i64()
            .unwrap_or(stream_guard::DEFAULT_MAX_DEPTH),
        o["maxBytes"]
            .as_i64()
            .unwrap_or(stream_guard::DEFAULT_MAX_BYTES),
    )
}

/// Feed `chunks`; `(first, violation, done)` as the generator records them. With `roundtrip`
/// the state is encoded and decoded around every call.
fn run(
    schema: &Schema,
    (d, b): (i64, i64),
    chunks: &[Vec<u16>],
    roundtrip: bool,
) -> (Option<usize>, Got, bool) {
    let mut st = State::new(d, b).unwrap();
    let mut first = None;
    let mut seen: Got = None;
    let cycle = |st: State, f: &mut dyn FnMut(&mut Validator<'_>)| -> State {
        let st = if roundtrip {
            State::decode(&st.encode(), schema).unwrap()
        } else {
            st
        };
        let mut v = Validator::new(schema, st);
        f(&mut v);
        v.into_state()
    };
    for (i, c) in chunks.iter().enumerate() {
        st = cycle(st, &mut |v| {
            v.feed(c);
        });
        if first.is_none() && st.violation().is_some() {
            first = Some(i);
            seen = got(st.violation());
        }
    }
    st = cycle(st, &mut |v| {
        v.end();
    });
    if first.is_none() && st.violation().is_some() {
        first = Some(chunks.len());
        seen = got(st.violation());
    }
    (first, seen, st.is_done())
}

#[test]
fn limits_and_whitespace_match() {
    let f = fixtures();
    assert_eq!(f["version"], 1);
    let l = &f["limits"];
    assert_eq!(l["defaultMaxDepth"], stream_guard::DEFAULT_MAX_DEPTH);
    assert_eq!(l["defaultMaxBytes"], stream_guard::DEFAULT_MAX_BYTES);
    assert_eq!(l["maxGuardBytes"], stream_guard::MAX_GUARD_BYTES);
    assert_eq!(l["maxDepthCap"], stream_guard::MAX_DEPTH_CAP);
    assert_eq!(l["maxSchemaBytes"], stream_guard::MAX_SCHEMA_BYTES);
    assert_eq!(l["maxStateBytes"], stream_guard::MAX_STATE_BYTES);
    assert_eq!(l["maxCorrectionUnits"], stream_guard::MAX_CORRECTION_UNITS);
    let ws: Vec<u16> = f["whitespace"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_u64().unwrap() as u16)
        .collect();
    let mine: Vec<u16> = (0..=u16::MAX)
        .filter(|&u| stream_guard::is_whitespace(u))
        .collect();
    assert_eq!(mine, ws);
}

#[test]
fn every_case_agrees_with_the_js() {
    let f = fixtures();
    let schemas: Vec<Schema> = f["schemas"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| Schema::parse(s.as_str().unwrap().as_bytes()).unwrap())
        .collect();
    let cases = f["cases"].as_array().unwrap();
    assert!(cases.len() >= 9000, "{}", cases.len());
    let (mut violations, mut splits) = (0, 0);
    for (n, row) in cases.iter().enumerate() {
        let schema = &schemas[row["s"].as_u64().unwrap() as usize];
        let opts = options(row);
        let chunks: Vec<Vec<u16>> = row["chunks"].as_array().unwrap().iter().map(text).collect();
        let first = row["first"].as_u64().map(|x| x as usize);
        let v = want(&row["v"]);
        let done = row["done"].as_bool().unwrap();
        let expected = (first, v.clone(), done);
        assert_eq!(run(schema, opts, &chunks, false), expected, "case {n}");
        if chunks.iter().map(Vec::len).sum::<usize>() < 4096 || n % 2 == 0 {
            assert_eq!(
                run(schema, opts, &chunks, true),
                expected,
                "case {n} (state round trip)"
            );
        }
        if v.is_some() {
            violations += 1;
        }

        // One unit at a time.
        let whole: Vec<u16> = chunks.concat();
        let unit = &row["unit"];
        let mut st = Validator::new(schema, State::new(opts.0, opts.1).unwrap());
        let mut at: Option<i64> = None;
        for (i, u) in whole.iter().enumerate() {
            if st.feed(std::slice::from_ref(u)).is_some() {
                at = Some(i as i64 + 1);
                break;
            }
        }
        if at.is_none() && st.end().is_some() {
            at = Some(-1);
        }
        assert_eq!(at, unit["at"].as_i64(), "case {n} (unit at)");
        assert_eq!(got(st.violation()), want(&unit["v"]), "case {n} (unit)");

        // Every two-chunk cut of a short single-chunk text.
        if chunks.len() == 1 && whole.len() <= 40 {
            let differ: Vec<(usize, Got)> = row["splits"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .map(|p| (p[0].as_u64().unwrap() as usize, want(&p[1])))
                        .collect()
                })
                .unwrap_or_default();
            splits += differ.len();
            for cut in 1..whole.len() {
                let two = vec![whole[..cut].to_vec(), whole[cut..].to_vec()];
                let (_, gv, gd) = run(schema, opts, &two, false);
                match differ.iter().find(|(c, _)| *c == cut) {
                    Some((_, dv)) => assert_eq!(gv, *dv, "case {n} cut {cut}"),
                    None => assert_eq!((gv, gd), (v.clone(), done), "case {n} cut {cut}"),
                }
            }
        }
    }
    assert!(violations > 7000, "{violations}");
    assert!(
        splits > 0,
        "the table records the JS's own split-dependence at maxBytes"
    );
}

fn le(u: &[u16]) -> Vec<u8> {
    u.iter().flat_map(|c| c.to_le_bytes()).collect()
}

/// An independent ASCII JSON string writer for the expected wire replies.
fn ascii(u: &[u16]) -> String {
    let mut s = String::from("\"");
    for &c in u {
        match c {
            0x22 => s.push_str("\\\""),
            0x5c => s.push_str("\\\\"),
            0x20..=0x7e => s.push(char::from(c as u8)),
            _ => s.push_str(&format!("\\u{c:04x}")),
        }
    }
    s.push('"');
    s
}

fn reply_json(v: &Got, done: bool) -> String {
    let vj = match v {
        None => "null".to_owned(),
        Some((m, p, r)) => format!(
            "{{\"message\":{},\"path\":{},\"reason\":\"{r}\"}}",
            ascii(m),
            ascii(p)
        ),
    };
    format!("{{\"violation\":{vj},\"done\":{done}}}")
}

fn split(reply: &[u8]) -> (String, Vec<u8>) {
    let j = u32::from_le_bytes(reply[..4].try_into().unwrap()) as usize;
    (
        String::from_utf8(reply[4..4 + j].to_vec()).unwrap(),
        reply[4 + j..].to_vec(),
    )
}

#[test]
fn the_wire_format_agrees_too() {
    let f = fixtures();
    let schemas: Vec<&str> = f["schemas"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap())
        .collect();
    let mut checked = 0;
    for (n, row) in f["cases"].as_array().unwrap().iter().enumerate() {
        let chunks: Vec<Vec<u16>> = row["chunks"].as_array().unwrap().iter().map(text).collect();
        if n % 5 != 0 && chunks.iter().map(Vec::len).sum::<usize>() < 1_000_000 {
            continue;
        }
        checked += 1;
        let schema = schemas[row["s"].as_u64().unwrap() as usize].as_bytes();
        let (d, b) = options(row);
        let v = want(&row["v"]);
        let done = row["done"].as_bool().unwrap();
        let head = |op: u8| {
            let mut r = vec![op];
            r.extend_from_slice(&(d as f64).to_le_bytes());
            r.extend_from_slice(&(b as f64).to_le_bytes());
            r.extend_from_slice(&(schema.len() as u32).to_le_bytes());
            r.extend_from_slice(schema);
            r
        };
        let (s, reply) = call(&head(0));
        assert_eq!(s, 0, "case {n}");
        let (_, mut state) = split(&reply);
        let body = |op: u8, state: &[u8]| {
            let mut r = vec![op];
            r.extend_from_slice(&(schema.len() as u32).to_le_bytes());
            r.extend_from_slice(schema);
            r.extend_from_slice(&(state.len() as u32).to_le_bytes());
            r.extend_from_slice(state);
            r
        };
        for c in &chunks {
            let mut r = body(1, &state);
            r.push(0);
            r.extend_from_slice(&le(c));
            let (s, reply) = call(&r);
            assert_eq!(s, 0, "case {n}");
            state = split(&reply).1;
        }
        let (s, reply) = call(&body(2, &state));
        assert_eq!(s, 0, "case {n}");
        assert_eq!(split(&reply).0, reply_json(&v, done), "case {n} (wire)");
        if chunks.len() == 1 {
            let mut r = head(3);
            r.push(0);
            r.extend_from_slice(&le(&chunks[0]));
            let (s, reply) = call(&r);
            assert_eq!(
                (s, split(&reply)),
                (0, (reply_json(&v, done), vec![])),
                "case {n} (check)"
            );
        }
    }
    assert!(checked > 1800, "{checked}");
}

#[test]
fn corrections_agree_with_the_js() {
    let f = fixtures();
    let rows = f["corrections"].as_array().unwrap();
    assert!(rows.len() >= 100);
    for (n, row) in rows.iter().enumerate() {
        let message = text(&row["message"]);
        let path = (!row["path"].is_null()).then(|| text(&row["path"]));
        let clip = row["clip"].as_u64().map(|c| c as usize);
        let out = &row["out"]["violation"];
        let op = if out["path"].is_null() {
            "null".to_owned()
        } else {
            ascii(&text(&out["path"]))
        };
        let expected = format!(
            "{{\"type\":\"schema_violation_correction\",\"violation\":{{\"message\":{},\"path\":{op}}}}}",
            ascii(&text(&out["message"]))
        );
        assert_eq!(row["out"]["type"], "schema_violation_correction");
        assert_eq!(
            String::from_utf8(correction(&message, path.as_deref(), clip)).unwrap(),
            expected,
            "row {n}"
        );
        let mut r = vec![4u8];
        r.extend_from_slice(&clip.map_or(u32::MAX, |c| c as u32).to_le_bytes());
        r.push(u8::from(path.is_some()));
        r.extend_from_slice(&(message.len() as u32).to_le_bytes());
        r.extend_from_slice(&le(&message));
        r.extend_from_slice(&le(path.as_deref().unwrap_or(&[])));
        assert_eq!(call(&r), (0, expected.into_bytes()), "row {n} (wire)");
    }
}
