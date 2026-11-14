#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
//! Parity of `docling_service::synthetic` with services `docling/synthetic_pdfs.py`, the reference it
//! replaces: every case is built by both and must be byte-equal (the Latin-1 refusal included).
//!
//! The reference runs only when `DOCLING_SYNTHETIC_ORACLE` names its file (python3 3.12, no
//! dependency beyond the standard library); `DOCLING_SYNTHETIC_ORACLE_REQUIRED=1` makes a missing
//! oracle a failure. `DOCLING_SYNTHETIC_PYTHON` picks the interpreter (default `python3`). Example
//! on the builder, with services exported under wt/:
//!
//!     n5run --with <services worktree> -- 'DOCLING_SYNTHETIC_ORACLE=<services>/docling/synthetic_pdfs.py \
//!       DOCLING_SYNTHETIC_ORACLE_REQUIRED=1 cargo test --locked -p docling-service --test synthetic_oracle'
use docling_service::synthetic::{build, default_lines, page_lines, Page};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    io::Write,
    process::{Command, Stdio},
};

/// services main 17d262f (file last changed in 9e60672b).
const ORACLE_SHA256: &str = "7190b8202cf95fc9565c8a36cd27df56573b54fb5f11398e6ff449a6889968d6";

/// Loads the reference by path (no import path, no current directory) and answers each request.
const DRIVER: &str = r#"
import importlib.util, json, sys
spec = importlib.util.spec_from_file_location("synthetic_pdfs", sys.argv[1])
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)
requests = json.load(sys.stdin)
out = []
for request in requests:
    if request["op"] == "lines":
        out.append(module.page_lines(request["paragraphs"], request["start"]))
        continue
    try:
        out.append(module.build([(kind, lines) for kind, lines in request["pages"]]).hex())
    except UnicodeEncodeError:
        out.append({"error": "not-latin1"})
json.dump(out, sys.stdout)
"#;

fn s(items: &[&str]) -> Vec<String> {
    items.iter().map(|i| i.to_string()).collect()
}

/// The documents both sides build: the reference's own `__main__` set, every page shape the
/// extraction tests use, and the edges (escapes, Latin-1, refusal, unknown kind, no page, many pages).
fn cases() -> Vec<(&'static str, Vec<Page>)> {
    let p = Page::new;
    let mut many = Vec::new();
    for index in 0..31 {
        let kind = ["text", "picture-text", "blank", "picture", "other"][index % 5];
        many.push(p(kind, page_lines(index % 4, index as i64 * 3)));
    }
    vec![
        (
            "main picture-text",
            vec![p("picture-text", default_lines())],
        ),
        ("main text", vec![p("text", default_lines())]),
        ("main blank", vec![p("blank", Vec::new())]),
        ("scan without text", vec![p("picture", Vec::new())]),
        (
            "page number only",
            vec![p("picture-text", s(&["Page 3 of 9"]))],
        ),
        (
            "mixed three",
            vec![
                p("text", default_lines()),
                p("picture-text", page_lines(12, 20)),
                p("blank", Vec::new()),
            ],
        ),
        (
            "text then blank",
            vec![p("text", default_lines()), p("blank", Vec::new())],
        ),
        (
            "escapes",
            vec![p("text", s(&["a\\b(c)d", "((nested))", "\\", ")(", ""]))],
        ),
        (
            "raw newline in a line",
            vec![p("text", s(&["line\nbreak", "tab\there"]))],
        ),
        (
            "latin-1",
            vec![p("picture-text", s(&["caf\u{e9} \u{fc} \u{ff} \u{a0}"]))],
        ),
        (
            "not latin-1 refused",
            vec![p("text", s(&["dash \u{2014}"]))],
        ),
        (
            "not latin-1 on a later page",
            vec![p("blank", Vec::new()), p("picture-text", s(&["\u{1f600}"]))],
        ),
        (
            "not latin-1 never drawn",
            vec![p("picture", s(&["\u{2014}"])), p("blank", s(&["\u{2014}"]))],
        ),
        ("unknown kind", vec![p("diagram", default_lines())]),
        ("no pages", Vec::new()),
        (
            "negative and large numbers",
            vec![
                p("text", page_lines(3, -2)),
                p("text", page_lines(2, 99_999)),
            ],
        ),
        ("a long page", vec![p("text", page_lines(200, 1))]),
        ("thirty-one pages", many),
    ]
}

fn oracle(requests: &Value) -> Option<Value> {
    let path = match std::env::var("DOCLING_SYNTHETIC_ORACLE") {
        Ok(path) if !path.is_empty() => path,
        _ => {
            assert!(
                std::env::var("DOCLING_SYNTHETIC_ORACLE_REQUIRED").as_deref() != Ok("1"),
                "DOCLING_SYNTHETIC_ORACLE_REQUIRED=1 but DOCLING_SYNTHETIC_ORACLE is unset"
            );
            eprintln!("skipped: DOCLING_SYNTHETIC_ORACLE unset");
            return None;
        }
    };
    let source = std::fs::read(&path).unwrap();
    assert_eq!(
        format!("{:x}", Sha256::digest(&source)),
        ORACLE_SHA256,
        "the reference changed: re-read it, port the change, then re-pin"
    );
    let python = std::env::var("DOCLING_SYNTHETIC_PYTHON").unwrap_or_else(|_| "python3".into());
    let mut child = Command::new(python)
        .args(["-I", "-c", DRIVER, &path])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(requests.to_string().as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "reference driver failed");
    Some(serde_json::from_slice(&output.stdout).unwrap())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn documents_are_byte_equal_to_the_reference() {
    let cases = cases();
    let requests: Vec<Value> = cases
        .iter()
        .map(|(_, pages)| {
            let pages: Vec<Value> = pages.iter().map(|p| json!([p.kind, p.lines])).collect();
            json!({"op": "build", "pages": pages})
        })
        .collect();
    let Some(answers) = oracle(&Value::Array(requests)) else {
        return;
    };
    let answers = answers.as_array().unwrap();
    assert_eq!(answers.len(), cases.len());
    for ((name, pages), expected) in cases.iter().zip(answers) {
        let ours = match build(pages) {
            Ok(bytes) => Value::String(hex(&bytes)),
            Err(_) => json!({"error": "not-latin1"}),
        };
        assert_eq!(&ours, expected, "case {name}");
    }
}

#[test]
fn filler_lines_equal_the_reference() {
    let shapes: Vec<(usize, i64)> =
        vec![(12, 1), (0, 1), (1, 0), (3, -2), (12, 20), (5, 1_000_000)];
    let requests: Vec<Value> = shapes
        .iter()
        .map(|(paragraphs, start)| json!({"op": "lines", "paragraphs": paragraphs, "start": start}))
        .collect();
    let Some(answers) = oracle(&Value::Array(requests)) else {
        return;
    };
    for ((paragraphs, start), expected) in shapes.iter().zip(answers.as_array().unwrap()) {
        assert_eq!(
            &json!(page_lines(*paragraphs, *start)),
            expected,
            "page_lines({paragraphs}, {start})"
        );
    }
}

/// Runs without the reference: the case list itself stays meaningful (each case builds or refuses
/// as its name says).
#[test]
fn case_list_is_well_formed() {
    for (name, pages) in cases() {
        assert_eq!(
            build(&pages).is_err(),
            name.contains("refused") || name.contains("later page"),
            "{name}"
        );
    }
}
