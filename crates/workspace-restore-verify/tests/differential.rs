//! Replays tests/fixtures/workspace-restore.v1.json (tools/gen-workspace-restore.py, generated
//! from noevia-services' diary/agent/workspace_restore.py) through the library.

use base64::Engine;
use serde_json::Value;
use workspace_restore_verify::verify;

fn decide(body: &[u8]) -> String {
    match verify(body) {
        Ok(v) => format!("accept {}", v.listing_sha256()),
        Err(r) => format!("reject: {}", r.code()),
    }
}

fn cases() -> Vec<Value> {
    let text = include_str!("fixtures/workspace-restore.v1.json");
    let doc: Value = serde_json::from_str(text).unwrap_or(Value::Null);
    doc.get("cases")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

#[test]
fn every_fixture_decides_as_recorded() {
    let cases = cases();
    assert!(cases.len() > 250, "fixture table missing or short");
    let mut failures = Vec::new();
    for c in &cases {
        let s = |k: &str| c.get(k).and_then(Value::as_str).unwrap_or("<missing>");
        let body = base64::engine::general_purpose::STANDARD
            .decode(s("archive_b64"))
            .unwrap_or_default();
        let got = decide(&body);
        if got != s("rust") {
            failures.push(format!("{}: {} != {}", s("name"), got, s("rust")));
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

#[test]
fn the_rust_side_never_accepts_more_and_differs_only_where_declared() {
    for c in cases() {
        let s = |k: &str| c.get(k).and_then(Value::as_str).unwrap_or("<missing>");
        let (py, rs, name) = (s("python"), s("rust"), s("name"));
        if rs.starts_with("accept") {
            assert_eq!(py, rs, "{name}: accepts what Python decides differently");
        }
        if py != rs {
            assert!(
                c.get("stricter").is_some()
                    || (name.starts_with("mutant/") && rs == "reject: invalid"),
                "{name}: undeclared disagreement {py} / {rs}"
            );
        }
    }
}

#[test]
fn every_python_refusal_class_is_covered() {
    let cases = cases();
    for code in [
        "invalid",
        "entries",
        "path",
        "entry_type",
        "entry_limits",
        "directory_not_empty",
        "type_mismatch",
        "expanded",
        "manifest_size",
        "manifest_object",
        "format",
        "counts",
        "duplicate_directory",
        "file_record",
        "overlap",
        "checksum",
        "parent_overlap",
        "entries_mismatch",
        "totals_invalid",
        "totals_mismatch",
    ] {
        let want = format!("reject: {code}");
        assert!(
            cases.iter().any(
                |c| c.get("python").and_then(Value::as_str) == Some(want.as_str())
                    && c.get("rust").and_then(Value::as_str) == Some(want.as_str())
            ),
            "no identical fixture for {code}"
        );
    }
}
