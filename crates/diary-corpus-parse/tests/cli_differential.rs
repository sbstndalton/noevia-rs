use serde_json::{json, Value};
use std::io::Write;
use std::process::{Command, Stdio};

#[test]
fn framed_cli_matches_actual_python_days_and_chunks() -> Result<(), Box<dyn std::error::Error>> {
    let oracle: Value = serde_json::from_str(include_str!("python-corpus.json"))?;
    let fixtures = oracle
        .get("fixtures")
        .and_then(Value::as_array)
        .ok_or("fixtures")?;
    assert_eq!(fixtures.len(), 455);
    for fixture in fixtures {
        let input = fixture
            .get("input")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .as_bytes();
        let mut child = Command::new(env!("CARGO_BIN_EXE_diary-corpus-parse"))
            .arg("parse-v1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let mut frame = Vec::with_capacity(input.len() + 16);
        frame.extend_from_slice(b"DCP1");
        frame.extend_from_slice(&(input.len() as u64).to_be_bytes());
        frame.extend_from_slice(input);
        frame.extend_from_slice(b"END1");
        child.stdin.take().ok_or("stdin")?.write_all(&frame)?;
        let output = child.wait_with_output()?;
        assert!(output.status.success(), "fixture {:?}", fixture["name"]);
        assert!(output.stderr.is_empty(), "fixture {:?}", fixture["name"]);
        let reply = output.stdout;
        assert!(reply.len() >= 16, "fixture {:?}", fixture["name"]);
        assert_eq!(reply.get(..4), Some(b"DCR1".as_slice()));
        assert_eq!(reply.get(reply.len() - 4..), Some(b"END1".as_slice()));
        let declared = u64::from_be_bytes(reply.get(4..12).ok_or("length")?.try_into()?);
        assert_eq!(declared as usize + 16, reply.len());
        let actual: Value =
            serde_json::from_slice(reply.get(12..reply.len() - 4).ok_or("payload")?)?;
        assert_eq!(actual.get("version").and_then(Value::as_u64), Some(1));
        assert_eq!(
            actual.get("days"),
            fixture.get("days"),
            "fixture {:?}",
            fixture["name"]
        );
        let chunks = actual
            .get("chunks")
            .and_then(Value::as_array)
            .ok_or("chunks")?;
        let projection = chunks
            .iter()
            .map(|row| json!([row["day"], row["header"], row["text"], row["content_hash"]]))
            .collect::<Vec<_>>();
        assert_eq!(
            &Value::Array(projection),
            fixture.get("chunks").ok_or("expected chunks")?,
            "fixture {:?}",
            fixture["name"]
        );
    }
    Ok(())
}
