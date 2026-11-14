//! Replays what core's own decision-log.cjs and context-log.cjs (and their CLIs) answered, recorded
//! by `tests/record_turn_logs.cjs` into `tests/fixtures/node-turn-logs.v1.json`. Until the fixture is
//! recorded these tests say so and pass; `TURN_LOGS_ORACLE_REQUIRED=1` (CI) makes a missing fixture
//! fail. Known, intended differences (the port's fixes) are asserted as such, never skipped quietly.
#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
use js_json::{wtf16, JValue};
use std::path::PathBuf;
use std::process::Command;
use turn_logs::context::{self, Round};
use turn_logs::decision::{self, DecisionLog};

fn fixture() -> Option<JValue> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/node-turn-logs.v1.json");
    match std::fs::read_to_string(&path) {
        Ok(text) => Some(wtf16::parse(&text).unwrap()),
        Err(_) if std::env::var("TURN_LOGS_ORACLE_REQUIRED").as_deref() == Ok("1") => {
            panic!("{} is missing: record it with tests/record_turn_logs.cjs <core>", path.display())
        }
        Err(_) => {
            eprintln!("turn-logs oracle: {} not recorded yet; skipped", path.display());
            None
        }
    }
}

fn arr(v: &JValue) -> &[JValue] {
    match v {
        JValue::Arr(a) => a,
        other => panic!("not an array: {other:?}"),
    }
}

fn strs(v: &JValue) -> Vec<&str> {
    arr(v).iter().map(|l| l.as_str().unwrap()).collect()
}

fn s(v: &JValue) -> String {
    wtf16::stringify(v).unwrap_or_else(|| "undefined".into())
}

/// Core throws a TypeError on a line holding `null` (council p12-2 issue); the port skips it.
fn has_null_line(lines: &[&str]) -> bool {
    lines.iter().any(|l| l.trim() == "null")
}

#[test]
fn summarize_matches_core() {
    let Some(f) = fixture() else { return };
    let cases = arr(f.get("summarize"));
    assert!(cases.len() >= 6, "{} cases", cases.len());
    for (i, case) in cases.iter().enumerate() {
        let lines = strs(case.get("lines"));
        let ours = decision::summarize(lines.iter().copied());
        if case.get("threw").is_nullish() {
            assert_eq!(s(&ours), s(case.get("out")), "summarize case {i}");
        } else {
            assert!(has_null_line(&lines), "core threw on case {i} without a null line");
            let rest: Vec<&str> = lines.iter().copied().filter(|l| l.trim() != "null").collect();
            assert_eq!(s(&ours), s(&decision::summarize(rest)), "case {i}: the null line is skipped");
        }
    }
}

#[test]
fn records_match_core() {
    let Some(f) = fixture() else { return };
    let cases = arr(f.get("records"));
    assert!(cases.len() >= 4);
    for (i, case) in cases.iter().enumerate() {
        let input = case.get("input");
        let messages = arr(input.get("messages"));
        let tools = arr(input.get("tools"));
        let ours = context::record(&Round {
            chat_id: input.get("chatId"),
            model: input.get("model"),
            limit: input.get("limit"),
            round: input.get("round"),
            compacted: input.get("compacted").truthy(),
            messages,
            tools,
            now: input.get("now").as_num().unwrap(),
        });
        assert!(case.get("threw").is_nullish(), "core threw on record {i}");
        assert_eq!(s(&ours), s(case.get("out")), "record {i}");
    }
}

#[test]
fn reports_match_core() {
    let Some(f) = fixture() else { return };
    let cases = arr(f.get("reports"));
    assert!(cases.len() >= 6);
    for (i, case) in cases.iter().enumerate() {
        let lines = strs(case.get("lines"));
        let ours = context::report(lines.iter().copied());
        if case.get("threw").is_nullish() {
            assert_eq!(s(&ours), s(case.get("out")), "report {i}");
        } else {
            assert!(has_null_line(&lines), "core threw on report {i} without a null line");
        }
    }
}

#[test]
fn written_lines_match_core() {
    let Some(f) = fixture() else { return };
    let tmp = tempfile::tempdir().unwrap();
    let writes = arr(f.get("writes"));
    assert!(writes.len() >= 6);
    for (i, w) in writes.iter().enumerate() {
        let log = DecisionLog::new(tmp.path()).with_clock(Box::new(|| "2026-09-22T08:00:00.000Z".into())).with_echo(None);
        let kind = w.get("kind").as_str().unwrap();
        assert_eq!(log.line(kind, w.get("entry")), w.get("written").as_str().unwrap(), "write {i}");
    }
    // Rotation: ASCII lines, so core's UTF-16 count and the port's byte count agree.
    let rot = f.get("rotation");
    let dir = tmp.path().join("rot");
    std::fs::create_dir(&dir).unwrap();
    let max = rot.get("maxBytes").as_num().unwrap() as u64;
    let log = DecisionLog::new(&dir).with_max_bytes(max).with_clock(Box::new(|| "2026-09-22T08:00:00.000Z".into())).with_echo(None);
    let rounds = rot.get("rounds").as_num().unwrap() as usize;
    for i in 0..rounds {
        let entry = wtf16::parse(&format!(r#"{{"selected":"smart","options":3,"margin":0.3,"ms":500,"fellBack":null,"i":{i}}}"#)).unwrap();
        log.record("route", &entry);
    }
    assert_eq!(std::fs::read_to_string(log.file()).unwrap(), rot.get("current").as_str().unwrap());
    assert_eq!(std::fs::read_to_string(decision::rotated(log.file())).unwrap(), rot.get("rotated").as_str().unwrap());
}

#[test]
fn cli_output_matches_core() {
    let Some(f) = fixture() else { return };
    let cli = f.get("cli");
    let bin = env!("CARGO_BIN_EXE_turn-logs");
    let tmp = tempfile::tempdir().unwrap();
    let d = cli.get("decisions");
    let join = |v: &JValue| strs(v).join("\n");
    std::fs::write(tmp.path().join("system-one-decisions.jsonl.1"), join(d.get("lines1")) + "\n").unwrap();
    std::fs::write(tmp.path().join("system-one-decisions.jsonl"), join(d.get("lines")) + "\n\n").unwrap();
    let out = Command::new(bin).arg("decisions").arg(tmp.path()).output().unwrap();
    assert_eq!(out.status.code(), d.get("status").as_num().map(|n| n as i32));
    assert_eq!(String::from_utf8(out.stdout).unwrap(), d.get("stdout").as_str().unwrap());

    let c = cli.get("context");
    let file = tmp.path().join("context-log.jsonl");
    std::fs::write(&file, join(c.get("lines")) + "\n").unwrap();
    let out = Command::new(bin).arg("context").arg(&file).output().unwrap();
    assert_eq!(out.status.code(), c.get("status").as_num().map(|n| n as i32));
    assert_eq!(String::from_utf8(out.stdout).unwrap(), c.get("stdout").as_str().unwrap());

    let u = cli.get("contextUsage");
    let out = Command::new(bin).arg("context").output().unwrap();
    assert_eq!(out.status.code(), u.get("status").as_num().map(|n| n as i32));
    assert_eq!(String::from_utf8(out.stdout).unwrap(), u.get("stdout").as_str().unwrap());
}
