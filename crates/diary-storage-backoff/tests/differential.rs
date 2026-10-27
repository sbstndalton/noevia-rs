use diary_storage_backoff::{parse_retry_after, GateKind, StorageGate};
use serde_json::Value;
use std::{error::Error, fs, path::Path};

type TestResult<T> = Result<T, Box<dyn Error>>;

fn field<'a>(value: &'a Value, key: &str) -> TestResult<&'a Value> {
    value
        .get(key)
        .ok_or_else(|| format!("missing field {key}").into())
}
fn str_field<'a>(value: &'a Value, key: &str) -> TestResult<&'a str> {
    field(value, key)?
        .as_str()
        .ok_or_else(|| format!("not string: {key}").into())
}
fn f64_field(value: &Value, key: &str) -> TestResult<f64> {
    field(value, key)?
        .as_f64()
        .ok_or_else(|| format!("not number: {key}").into())
}
fn kind_text(kind: Option<GateKind>) -> &'static str {
    kind.map_or("", GateKind::as_str)
}

#[test]
fn python_312_differential_fixture() -> TestResult<()> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/storage-backoff-fixtures.json");
    let data: Value = serde_json::from_slice(&fs::read(path)?)?;
    assert_eq!(field(&data, "unicode")?.as_str(), Some("15.0.0"));
    let parses = field(&data, "parse")?
        .as_array()
        .ok_or("missing parse cases")?;
    assert_eq!(parses.len(), 334);
    assert_eq!(str_field(&data, "python")?, "3.12.15");
    assert_eq!(
        str_field(&data, "source_sha256")?,
        "d2ba1b187d611be7766e523d4638f7a3b9a4f223e49c4847d8f960897f04f2af"
    );
    for (index, case) in parses.iter().enumerate() {
        let value = field(case, "value")?.as_str();
        let got = parse_retry_after(value, f64_field(case, "default")?, f64_field(case, "wall")?);
        assert_eq!(
            got.seconds,
            f64_field(case, "expected")?,
            "parse case {index}: {case}"
        );
    }
    let sequences = field(&data, "sequences")?
        .as_array()
        .ok_or("missing sequences")?;
    let mut count = 0;
    for seq in sequences {
        let mut gate = StorageGate::new();
        let steps = field(seq, "steps")?
            .as_array()
            .ok_or("missing sequence steps")?;
        for step in steps {
            count += 1;
            let action = field(step, "action")?;
            let mono = f64_field(action, "mono")?;
            let wall = f64_field(action, "wall")?;
            match str_field(action, "op")? {
                "record" => {
                    let status = field(action, "status")?.as_u64().ok_or("invalid status")? as u16;
                    gate.record(
                        mono,
                        wall,
                        status,
                        action.get("header").and_then(Value::as_str),
                        action.get("root").and_then(Value::as_bool).unwrap_or(false),
                    )?;
                }
                "reset" => {
                    let result = gate.reset_login(mono)?;
                    assert_eq!(
                        Some(result),
                        field(step, "reset_result")?.as_bool(),
                        "{seq}: {step}"
                    );
                }
                "sample" => {}
                other => return Err(format!("unknown operation {other}").into()),
            }
            let state = gate.state(mono)?;
            let expected = field(step, "state")?.as_array().ok_or("missing state")?;
            assert_eq!(
                Some(kind_text(state.kind)),
                expected.first().and_then(Value::as_str),
                "{seq}: {step}"
            );
            assert_eq!(
                Some(state.remaining),
                expected.get(1).and_then(Value::as_f64),
                "{seq}: {step}"
            );
            let blocked = gate.check(mono)?;
            match (blocked, field(step, "blocked")?) {
                (None, Value::Null) => {}
                (Some(got), Value::Object(expected)) => {
                    assert_eq!(
                        Some(kind_text(got.kind)),
                        expected.get("kind").and_then(Value::as_str),
                        "{seq}: {step}"
                    );
                    assert_eq!(
                        Some(got.retry_after),
                        expected.get("retry_after").and_then(Value::as_u64),
                        "{seq}: {step}"
                    );
                }
                other => return Err(format!("blocked mismatch {other:?} in {seq}: {step}").into()),
            }
        }
    }
    assert!(count >= 350);
    Ok(())
}

#[test]
fn shipped_parser_edge_cases() -> TestResult<()> {
    let data: Value = serde_json::from_str(include_str!("oracles/parser-edges.json"))?;
    assert_eq!(
        field(&data, "source_sha256")?,
        "d2ba1b187d611be7766e523d4638f7a3b9a4f223e49c4847d8f960897f04f2af"
    );
    let cases = field(&data, "cases")?
        .as_array()
        .ok_or("missing edge cases")?;
    for case in cases {
        let value = field(case, "value")?.as_str();
        let got = parse_retry_after(value, 7.25, f64_field(case, "wall")?);
        assert_eq!(got.seconds, f64_field(case, "expected")?, "{case}");
        assert_eq!(
            format!("{:?}", got.source),
            str_field(case, "source")?,
            "{case}"
        );
    }
    Ok(())
}

#[test]
fn supplied_local_date_resolution_and_gate_transitions() -> TestResult<()> {
    use diary_storage_backoff::{
        parse_retry_after_with_local_date, RecordEffect, RetryAfterSource,
    };
    let wall = 1445412420.0;
    let expected_date =
        chrono::NaiveDateTime::parse_from_str("2015-10-21 07:28:00", "%Y-%m-%d %H:%M:%S")?;
    for zone in ["", "unknown", "-0000", "-٠٠٠٠"] {
        let header = format!("21 Oct 2015 07:28:00 {zone}");
        let mut seen = Vec::new();
        let result = parse_retry_after_with_local_date(Some(&header), 7.25, wall, |date| {
            seen.push(date);
            Some(wall + 12.5)
        });
        assert_eq!(seen, vec![expected_date]);
        assert_eq!(result.source, RetryAfterSource::HttpDate);
        assert_eq!(result.seconds, 12.5);
        let mut gate = StorageGate::new();
        assert_eq!(
            gate.record_with_local_date(100.0, wall, 429, Some(&header), false, |_| Some(
                wall + 12.5
            ))?,
            RecordEffect::ThrottleCooldown
        );
        assert_eq!(gate.state(100.0)?.remaining, 12.5);
        for timestamp in [None, Some(f64::NAN), Some(f64::INFINITY)] {
            let refused =
                parse_retry_after_with_local_date(Some(&header), 7.25, wall, |_| timestamp);
            assert_eq!(refused.source, RetryAfterSource::Invalid);
            assert_eq!(refused.seconds, 7.25);
        }
    }
    let mut called = false;
    let aware =
        parse_retry_after_with_local_date(Some("21 Oct 2015 07:28:00 GMT"), 7.25, wall, |_| {
            called = true;
            None
        });
    assert!(!called);
    assert_eq!(aware.seconds, 60.0);
    let mut gate = StorageGate::new();
    let original = gate;
    assert!(gate
        .record_with_local_date(
            f64::NAN,
            wall,
            429,
            Some("21 Oct 2015 07:28:00"),
            false,
            |_| {
                called = true;
                None
            }
        )
        .is_err());
    assert!(!called);
    assert_eq!(gate, original);
    Ok(())
}

#[test]
fn shipped_gate_expiry_and_reset_boundaries() -> TestResult<()> {
    let data: Value = serde_json::from_str(include_str!("oracles/parser-edges.json"))?;
    let sequences = field(&data, "sequences")?
        .as_array()
        .ok_or("missing sequences")?;
    for sequence in sequences {
        let mut gate = StorageGate::new();
        for step in field(sequence, "steps")?
            .as_array()
            .ok_or("missing steps")?
        {
            let action = field(step, "action")?;
            let mono = f64::from_bits(
                field(action, "mono_bits")?
                    .as_u64()
                    .ok_or("missing mono bits")?,
            );
            match str_field(action, "op")? {
                "record" => {
                    gate.record(
                        mono,
                        1445412420.0,
                        u16::try_from(field(action, "status")?.as_u64().ok_or("invalid status")?)?,
                        action.get("header").and_then(Value::as_str),
                        false,
                    )?;
                }
                "reset" => {
                    assert_eq!(
                        Some(gate.reset_login(mono)?),
                        field(step, "reset")?.as_bool(),
                        "{sequence}: {step}"
                    );
                }
                "sample" => {}
                other => return Err(format!("unknown action {other}").into()),
            }
            let expected = field(step, "state")?.as_array().ok_or("missing state")?;
            let state = gate.state(mono)?;
            assert_eq!(
                Some(kind_text(state.kind)),
                expected.first().and_then(Value::as_str),
                "{sequence}: {step}"
            );
            assert_eq!(
                Some(state.remaining.to_bits()),
                field(step, "remaining_bits")?.as_u64(),
                "{sequence}: {step}"
            );
            match (gate.check(mono)?, field(step, "blocked")?) {
                (None, Value::Null) => {}
                (Some(got), expected) => {
                    assert_eq!(
                        kind_text(got.kind),
                        str_field(expected, "kind")?,
                        "{sequence}: {step}"
                    );
                    assert_eq!(
                        got.retry_after as f64,
                        f64_field(expected, "retry_after")?,
                        "{sequence}: {step}"
                    );
                }
                _ => return Err(format!("blocked mismatch: {sequence}: {step}").into()),
            }
        }
    }
    Ok(())
}
