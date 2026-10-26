use std::io::Write;
use std::process::{Command, Stdio};

type TestResult = Result<(), Box<dyn std::error::Error>>;
fn invoke(body: &[u8]) -> Result<std::process::Output, Box<dyn std::error::Error>> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_diary-score-gap"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child.stdin.take().ok_or("missing stdin")?.write_all(body)?;
    Ok(child.wait_with_output()?)
}

#[test]
fn success_is_one_versioned_record_with_positions() -> TestResult {
    for (body, expected) in [
        (
            "diary-score-gap-v1\t0.08\t0.1,0.9,0.82,null\n",
            "diary-score-gap-v1\t0.9\t1,2,3\t0\n",
        ),
        (
            "diary-score-gap-v1\tnull\t0.1,0.9\n",
            "diary-score-gap-v1\t0.9\t0,1\t\n",
        ),
        (
            "diary-score-gap-v1\t0.08\t\n",
            "diary-score-gap-v1\tnull\t\t\n",
        ),
        (
            "diary-score-gap-v1\t1\tnull,null\n",
            "diary-score-gap-v1\tnull\t0,1\t\n",
        ),
        (
            "diary-score-gap-v1\tnull\t-0,0\n",
            "diary-score-gap-v1\t-0\t0,1\t\n",
        ),
    ] {
        let output = invoke(body.as_bytes())?;
        assert_eq!(output.status.code(), Some(0));
        assert_eq!(output.stdout, expected.as_bytes());
        assert!(output.stderr.is_empty());
    }
    Ok(())
}

#[test]
fn rejects_bad_version_types_numbers_and_framing_without_echo() -> TestResult {
    for body in [
        "",
        "diary-score-gap-v2\t0.08\t1\n",
        "diary-score-gap-v1\t0.08\t1",
        "diary-score-gap-v1\t0.08\t1\r\n",
        "diary-score-gap-v1\t0.08\t1\n\n",
        "diary-score-gap-v1\t0.08\t1\nsecret",
        "diary-score-gap-v1\t0.08\t1\textra\n",
        "diary-score-gap-v1\t0\t1\n",
        "diary-score-gap-v1\t-0.1\t1\n",
        "diary-score-gap-v1\t1.1\t1\n",
        "diary-score-gap-v1\ttrue\t1\n",
        "diary-score-gap-v1\tnull\tnan\n",
        "diary-score-gap-v1\tnull\tinf\n",
        "diary-score-gap-v1\tnull\t1e309\n",
        "diary-score-gap-v1\tnull\t 1\n",
        "diary-score-gap-v1\tnull\t1,\n",
        "diary-score-gap-v1\tnull\t,1\n",
        "diary-score-gap-v1\tnull\tfalse\n",
        "diary-score-gap-v1\tnull\t\"1\"\n",
        "diary-score-gap-v1\tnull\t1\0\n",
        "diary-score-gap-v1\tnull\tSECRET\n",
    ] {
        let output = invoke(body.as_bytes())?;
        assert_eq!(output.status.code(), Some(2), "{body:?}");
        assert!(output.stdout.is_empty());
        assert_eq!(output.stderr, b"invalid request\n");
    }
    let output = invoke(&[0xff, 10])?;
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    Ok(())
}

#[test]
fn count_and_byte_limits() -> TestResult {
    let make = |n: usize| format!("diary-score-gap-v1\tnull\t{}\n", vec!["null"; n].join(","));
    assert_eq!(invoke(make(4096).as_bytes())?.status.code(), Some(0));
    let output = invoke(make(4097).as_bytes())?;
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    // Build a valid record exactly at the byte limit, then one byte above.
    let prefix = "diary-score-gap-v1\tnull\t0.";
    let exact = format!("{prefix}{}\n", "0".repeat(131_072 - prefix.len() - 1));
    assert_eq!(invoke(exact.as_bytes())?.status.code(), Some(0));
    let excess = format!("{prefix}{}\n", "0".repeat(131_072 - prefix.len()));
    let output = invoke(excess.as_bytes())?;
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    Ok(())
}

#[test]
fn unknown_arguments_are_rejected() -> TestResult {
    let output = Command::new(env!("CARGO_BIN_EXE_diary-score-gap"))
        .arg("--unexpected")
        .stdin(Stdio::null())
        .output()?;
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert_eq!(output.stderr, b"invalid request\n");
    Ok(())
}
