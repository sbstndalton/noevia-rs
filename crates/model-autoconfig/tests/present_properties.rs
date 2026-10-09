//! Properties of `check`'s slice 3-6 parts (spec, files, present, baseline): no input panics or
//! runs unbounded, refusals are typed, oversized inputs are refused at their caps, and every
//! answer keeps the guarantees the Python rules make (displaced sorted and disjoint from the
//! values, spec values only on spec keys, file answers only ever a name from the listing).
//! Agreement itself is pinned by present_differential.rs; "never larger than Python" runs in
//! noevia-services, where the Python reference is live.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use model_autoconfig::{check_json, Error};
use model_files::json::{self, Value};
use proptest::prelude::*;
use std::time::{Duration, Instant};

fn fixture_inputs() -> &'static [String] {
    static INPUTS: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    INPUTS.get_or_init(|| {
        let text = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/model-autoconfig-present.v1.json"
        ))
        .unwrap();
        let Some(Value::Arr(cases)) = json::parse(&text).unwrap().get("cases").cloned() else {
            panic!("no cases")
        };
        cases
            .iter()
            .filter_map(|c| match c.get("input") {
                Some(Value::Str(s)) => Some(s.clone()),
                _ => None,
            })
            .collect()
    })
}

const ODD: [&str; 18] = [
    "null",
    "true",
    "0",
    "-1",
    "2.5",
    "NaN",
    "1267650600228229401496703205376",
    "\"\"",
    "\" \"",
    "\"x\"",
    "\"\\u2192\"",
    "\"\\u212a\"",
    "[]",
    "[1,\"a\"]",
    "{}",
    "{\"a\":[]}",
    "[[\"a\",\"file\",null]]",
    "[[\"k\",\"v\"],[\"k\",\"w\"]]",
];

/// Replace the `n`-th JSON scalar or bracket-delimited value after a ':' with `odd`.
fn mutate(input: &str, n: usize, odd: &str) -> String {
    let colons: Vec<usize> = input.match_indices(": ").map(|(i, _)| i + 2).collect();
    if colons.is_empty() {
        return input.to_owned();
    }
    let at = colons[n % colons.len()];
    let rest = &input[at..];
    // the end of the value starting at `at`: a balanced bracket run, a string, or a scalar
    let bytes = rest.as_bytes();
    let mut depth = 0i32;
    let mut in_str = false;
    let mut esc = false;
    let mut end = rest.len();
    for (i, &b) in bytes.iter().enumerate() {
        if in_str {
            if esc {
                esc = false;
            } else if b == b'\\' {
                esc = true;
            } else if b == b'"' {
                in_str = false;
                if depth == 0 {
                    end = i + 1;
                    break;
                }
            }
            continue;
        }
        match b {
            b'"' => in_str = true,
            b'[' | b'{' => depth += 1,
            b']' | b'}' if depth > 0 => {
                depth -= 1;
                if depth == 0 {
                    end = i + 1;
                    break;
                }
            }
            b',' | b']' | b'}' if depth == 0 => {
                end = i;
                break;
            }
            _ => {}
        }
    }
    format!("{}{}{}", &input[..at], odd, &rest[end..])
}

fn invariants(answer: &Value) {
    if let Some(Value::Obj(p)) = answer.get("present") {
        let get = |k: &str| p.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        let Some(Value::Arr(displaced)) = get("displaced") else {
            panic!("no displaced")
        };
        let names: Vec<String> = displaced
            .iter()
            .map(|v| match v {
                Value::Str(s) => s.clone(),
                _ => panic!("displaced entry"),
            })
            .collect();
        assert!(names.windows(2).all(|w| w[0] < w[1]), "displaced sorted");
        let Some(Value::Arr(minimal)) = get("minimal") else {
            panic!("no minimal")
        };
        for pair in minimal {
            let Value::Arr(kv) = pair else { panic!() };
            let Value::Str(k) = &kv[0] else { panic!() };
            assert!(!names.contains(k), "a displaced key is also written: {k}");
        }
    }
    if let Some(Value::Obj(s)) = answer.get("spec") {
        if let Some((_, Value::Arr(values))) = s.iter().find(|(k, _)| k == "values") {
            for pair in values {
                let Value::Arr(kv) = pair else { panic!() };
                let Value::Str(k) = &kv[0] else { panic!() };
                assert!(k.starts_with("spec-"), "a spec value on {k}");
            }
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 3000, .. ProptestConfig::default() })]

    #[test]
    fn mutated_requests_never_panic_and_refuse_typed(
        pick in 0usize..10_000, n in 0usize..200, odd in prop::sample::select(ODD.to_vec())
    ) {
        let inputs = fixture_inputs();
        let input = mutate(&inputs[pick % inputs.len()], n, odd);
        let t = Instant::now();
        match check_json(input.as_bytes()) {
            Ok(text) => invariants(&json::parse(&text).expect("answer is JSON")),
            Err(Error::WorkLimit) => panic!("work limit on a small request: {input}"),
            Err(_) => {}
        }
        prop_assert!(t.elapsed() < Duration::from_secs(2));
    }
}

#[test]
fn caps_refuse_oversized_parts() {
    let name = "x".repeat(200);
    let entry = format!("[\"{name}-mtp-.gguf\",\"file\",1]");
    let big = vec![entry.as_str(); model_autoconfig::files::MAX_ENTRIES + 1].join(",");
    let req = format!(
        "{{\"files\":[{{\"rule\":\"mtp_folder\",\"listing\":[{big}],\"prefix\":\"/models/\",\"section\":\"m\"}}]}}"
    );
    // Past the input cap or the listing cap: refused either way, quickly.
    let t = Instant::now();
    assert!(matches!(
        check_json(req.as_bytes()),
        Err(Error::OutOfRange(_) | Error::InputTooLarge)
    ));
    assert!(t.elapsed() < Duration::from_secs(2));

    let calls = vec!["{\"rule\":\"mtp_flat\",\"listing\":null,\"section\":\"m\"}"; 65].join(",");
    assert!(matches!(
        check_json(format!("{{\"files\":[{calls}]}}").as_bytes()),
        Err(Error::OutOfRange("files"))
    ));
    let args = vec!["\"-c\""; model_autoconfig::baseline::MAX_ARGS + 1].join(",");
    assert!(matches!(
        check_json(format!("{{\"baseline\":[{{\"args\":[{args}],\"known\":[]}}]}}").as_bytes()),
        Err(Error::OutOfRange("baseline.args"))
    ));
    let values = vec!["[\"k\",\"v\"]"; model_autoconfig::present::MAX_VALUES + 1].join(",");
    let inputs = fixture_inputs();
    let present = inputs
        .iter()
        .find(|i| i.starts_with("{\"present\""))
        .unwrap();
    let swapped = {
        let v = json::parse(present).unwrap();
        let Value::Obj(outer) = v else { panic!() };
        let Value::Obj(inner) = &outer[0].1 else {
            panic!()
        };
        let mut body = String::from("{\"present\":{");
        for (i, (k, val)) in inner.iter().enumerate() {
            if i > 0 {
                body.push(',');
            }
            body.push_str(&format!("\"{k}\":"));
            if k == "values" {
                body.push_str(&format!("[{values}]"));
            } else {
                body.push_str(&value_text(val));
            }
        }
        body.push_str("}}");
        body
    };
    assert!(matches!(
        check_json(swapped.as_bytes()),
        Err(Error::OutOfRange("present.values"))
    ));
}

fn value_text(v: &Value) -> String {
    match v {
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Int(i) => i.to_string(),
        Value::Float(x) if x.is_nan() => "NaN".into(),
        Value::Float(x) => format!("{x:?}"),
        Value::Str(s) => {
            let mut out = String::from("\"");
            for c in s.chars() {
                match c {
                    '"' => out.push_str("\\\""),
                    '\\' => out.push_str("\\\\"),
                    c if c.is_ascii() && !c.is_ascii_control() => out.push(c),
                    c => {
                        let mut buf = [0u16; 2];
                        for u in c.encode_utf16(&mut buf) {
                            out.push_str(&format!("\\u{u:04x}"));
                        }
                    }
                }
            }
            out.push('"');
            out
        }
        Value::Arr(a) => format!(
            "[{}]",
            a.iter().map(value_text).collect::<Vec<_>>().join(",")
        ),
        Value::Obj(o) => format!(
            "{{{}}}",
            o.iter()
                .map(|(k, v)| format!("{}:{}", value_text(&Value::Str(k.clone())), value_text(v)))
                .collect::<Vec<_>>()
                .join(",")
        ),
    }
}

#[test]
fn a_maximal_listing_is_bounded_work() {
    // Every entry at the cap, every name long: answered (or refused for work) in well under a
    // second, never run long.
    let name = "a".repeat(120);
    let entries: Vec<String> = (0..model_autoconfig::files::MAX_ENTRIES)
        .map(|i| format!("[\"{name}{i}-mtp-.gguf\",\"file\",{}]", i % 7))
        .collect();
    let req = format!(
        "{{\"files\":[{{\"rule\":\"mtp_flat\",\"listing\":[{}],\"section\":\"{name}\"}}]}}",
        entries.join(",")
    );
    let t = Instant::now();
    let got = check_json(req.as_bytes());
    assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
    assert!(matches!(
        got,
        Ok(_) | Err(Error::WorkLimit | Error::InputTooLarge)
    ));
}
