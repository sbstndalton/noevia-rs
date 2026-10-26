use chrono::{NaiveDate, NaiveTime};
use diary_corpus_write as corpus;
use serde_json::Value;
use std::{error::Error, fs, path::Path};

type TestResult<T> = Result<T, Box<dyn Error>>;

fn field<'a>(value: &'a Value, name: &str) -> TestResult<&'a str> {
    let Some(field) = value.get(name).and_then(Value::as_str) else {
        return Err(format!("missing string field {name}").into());
    };
    Ok(field)
}

fn date(args: &Value) -> TestResult<NaiveDate> {
    Ok(NaiveDate::parse_from_str(field(args, "date")?, "%Y-%m-%d")?)
}

fn actual(op: &str, args: &Value) -> TestResult<Value> {
    let value = match op {
        "escape" => Value::String(corpus::escape_entry_text(field(args, "text")?)),
        "strip_markers" => Value::String(corpus::strip_markers(field(args, "text")?)),
        "has_marker" => Value::Bool(corpus::has_marker(
            field(args, "text")?,
            field(args, "xid")?,
        )),
        "canonical_day_header" => corpus::canonical_day_header(date(args)?).into(),
        "render_subsection_header" => Value::String(corpus::render_subsection_header(
            NaiveTime::parse_from_str(field(args, "time")?, "%H:%M:%S")?,
            field(args, "topic")?,
        )),
        "render_exchange" => Value::String(corpus::render_exchange(
            field(args, "me")?,
            field(args, "claude")?,
            field(args, "xid")?,
        )),
        "render_subsection_block" => Value::String(corpus::render_subsection_block(
            field(args, "header")?,
            field(args, "body")?,
        )),
        "exchange_text" => {
            corpus::exchange_text(Some(field(args, "text")?), field(args, "xid")?).into()
        }
        "exchange_hash" => {
            corpus::exchange_hash(Some(field(args, "text")?), field(args, "xid")?).into()
        }
        "replace_exchange_text" => corpus::replace_exchange_text(
            field(args, "text")?,
            field(args, "xid")?,
            field(args, "me")?,
            field(args, "claude")?,
        )
        .into(),
        "append_to_month_text" => corpus::append_to_month_text(
            args.get("month_text").and_then(Value::as_str),
            date(args)?,
            field(args, "header")?,
            field(args, "body")?,
        )
        .into(),
        unknown => return Err(format!("unknown operation {unknown}").into()),
    };
    Ok(value)
}

#[test]
fn python_312_differential_fixture() -> TestResult<()> {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus-write-fixtures.json");
    let data: Value = serde_json::from_slice(&fs::read(fixture)?)?;
    assert_eq!(
        data.get("unicode"),
        Some(&Value::String("15.0.0".to_owned()))
    );
    let Some(cases) = data.get("cases").and_then(Value::as_array) else {
        return Err("missing cases".into());
    };
    assert!(cases.len() >= 650, "fixture is unexpectedly sparse");
    for (index, case) in cases.iter().enumerate() {
        let args = case.get("args").ok_or("missing args")?;
        let expected = case.get("expected").ok_or("missing expected")?;
        let op = field(case, "op")?;
        let result = actual(op, args)?;
        assert_eq!(&result, expected, "case {index}: {op} {args}");
    }
    Ok(())
}
