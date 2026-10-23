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
    assert!(parses.len() >= 300);
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
