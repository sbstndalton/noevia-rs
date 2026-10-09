//! Differential test of `check` (autoconfig slice 2: input prep and values assembly) against
//! the Python reference: every case in tests/fixtures/model-autoconfig-check.v1.json (generated
//! by tools/gen-model-autoconfig.py from noevia-services' autoconfig_core.py `check_reference`)
//! must give exactly Python's answer or, where Python raised, an error naming the same exception
//! type. Cases named "[stricter] ..." are inputs the port refuses on purpose (see pyval.rs): there
//! the answer must be a refusal that is not a Python exception, never a different answer.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use model_autoconfig::Error;
use model_files::json::{self, Value};

fn cases() -> Vec<Value> {
    let text = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/model-autoconfig-check.v1.json"
    ))
    .expect("fixture file");
    match json::parse(&text).expect("fixture JSON").get("cases") {
        Some(Value::Arr(cases)) => cases.clone(),
        other => panic!("no cases: {other:?}"),
    }
}

/// Equality as Python's json round trip sees it: objects as maps, Int and Float distinct,
/// NaN equal to NaN, -0.0 distinct from 0.0.
fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Float(x), Value::Float(y)) => {
            x.to_bits() == y.to_bits() || (x.is_nan() && y.is_nan())
        }
        (Value::Arr(x), Value::Arr(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(p, q)| same(p, q))
        }
        (Value::Obj(x), Value::Obj(y)) => {
            x.len() == y.len() && x.iter().all(|(k, v)| b.get(k).is_some_and(|w| same(v, w)))
        }
        _ => a == b,
    }
}

#[test]
fn rust_agrees_with_python_on_every_check_fixture() {
    let cases = cases();
    assert!(cases.len() >= 400, "fixture too small: {}", cases.len());
    let (mut raised, mut stricter, mut prep_ok, mut values_ok) = (0, 0, 0, 0);
    for case in &cases {
        let Some(Value::Str(name)) = case.get("name") else {
            panic!("unnamed case")
        };
        let Some(Value::Str(input)) = case.get("input") else {
            panic!("{name}: no input")
        };
        let expect = case.get("expect").expect("expect");
        let got = model_autoconfig::check_json(input.as_bytes());
        if name.starts_with("[stricter]") {
            match got {
                Err(Error::Python(kind)) => {
                    panic!("{name}: a Python exception {kind}, not a refusal")
                }
                Err(_) => stricter += 1,
                Ok(text) => panic!("{name}: answered {text}"),
            }
            continue;
        }
        match (expect.get("error"), got) {
            (Some(Value::Str(kind)), Err(e)) => {
                raised += 1;
                assert_eq!(e.code(), format!("python:{kind}"), "{name}");
            }
            (None, Ok(text)) => {
                let answer = json::parse(&text).expect("answer JSON");
                assert!(
                    same(&answer, expect),
                    "{name}:\n rust   {text}\n python {expect:?}"
                );
                if answer.get("prep").is_some() {
                    prep_ok += 1;
                } else {
                    values_ok += 1;
                }
            }
            (want, got) => panic!("{name}: python {want:?}, rust {got:?}"),
        }
    }
    assert!(
        raised > 20 && stricter >= 5 && prep_ok > 100 && values_ok > 150,
        "coverage: raised {raised}, stricter {stricter}, prep {prep_ok}, values {values_ok}"
    );
}

#[test]
fn check_rejects_bad_envelopes() {
    for bad in [
        &b"[]"[..],
        b"{\"other\": {}}",
        b"{\"prep\": 1}",
        b"{\"values\": {}}",
        b"{\"prep\": null, \"prep\": null}",
        b"\xff",
    ] {
        match model_autoconfig::check_json(bad) {
            Err(Error::Python(k)) => panic!("{bad:?}: python {k}"),
            Err(_) => {}
            Ok(t) => panic!("{bad:?}: answered {t}"),
        }
    }
    assert_eq!(model_autoconfig::check_json(b"{}").unwrap(), "{}");
    assert_eq!(
        model_autoconfig::check_json(b"{\"size\": null}").unwrap(),
        "{}"
    );
}
