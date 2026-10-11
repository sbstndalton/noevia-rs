#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
use noevia_model_manager::diagnose::{
    Analyzer, Refusal, Work, ENTRY_CAP, JSON_CAP, LINE_CAP, LINE_COUNT, LOG_CAP,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    sync::atomic::AtomicBool,
    time::{Duration, Instant},
};
fn analyse(text: &str) -> Result<Value, Refusal> {
    let cancel = AtomicBool::new(false);
    let a = Analyzer::new().unwrap();
    let bytes = a.analyse_json(
        text,
        &Work {
            deadline: Instant::now() + Duration::from_secs(5),
            cancelled: &cancel,
        },
    )?;
    assert!(bytes.len() <= JSON_CAP);
    Ok(serde_json::from_slice(&bytes).unwrap())
}
fn cause(text: &str) -> Value {
    analyse(text).unwrap()["failures"][0]["cause"].clone()
}
fn log(line: &str) -> String {
    format!("spawning server instance with name=A on port 1\n[1] {line}\ninstance name=A exited with status 1")
}
#[test]
fn immutable_python_differential() {
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/diagnose-python.v1.json")).unwrap();
    assert_eq!(
        fixture["oracle"]["blob"],
        "17a4670f6189d215189b5a6f321cd82547dfe06e"
    );
    assert_eq!(
        fixture["generator_sha256"],
        format!(
            "{:x}",
            Sha256::digest(include_bytes!("../src/bin/generate-diagnose-oracle.rs"))
        )
    );
    assert_eq!(
        fixture["process_sha256"],
        format!(
            "{:x}",
            Sha256::digest(include_bytes!("../src/bin/support/diagnose_process.rs"))
        )
    );
    for c in fixture["cases"].as_array().unwrap() {
        let id = c["id"].as_str().unwrap();
        let text = c["text"].as_str().unwrap();
        let narrowing = c["narrowing"].as_str().unwrap();
        if narrowing == "status-dialect" {
            // Preserve Python arbitrary-size integer JSON verbatim in this string;
            // never parse it through serde_json's default finite numeric representation.
            assert!(c["expected_json"]
                .as_str()
                .unwrap()
                .contains("\"status\": "));
            assert_eq!(analyse(text), Err(Refusal::StatusDialect), "{id}");
            continue;
        }
        let expected: Value = serde_json::from_str(c["expected_json"].as_str().unwrap()).unwrap();
        let actual = analyse(text).unwrap();
        if narrowing.is_empty() {
            assert_eq!(actual["failures"], expected, "{id}");
        } else {
            let mut raw = expected.clone();
            let mut safe = actual["failures"].clone();
            for f in raw.as_array_mut().unwrap() {
                f.as_object_mut().unwrap().remove("evidence");
            }
            for f in safe.as_array_mut().unwrap() {
                f.as_object_mut().unwrap().remove("evidence");
            }
            assert_eq!(safe, raw, "{id}");
            if narrowing == "cross-line-evidence-suppression" {
                for f in actual["failures"].as_array().unwrap() {
                    assert_eq!(f["evidence"], json!([]), "{id}");
                }
            } else {
                let evidence = actual["failures"][0]["evidence"].to_string();
                assert!(evidence.contains("[redacted]"), "{id}");
                for marker in [
                    "synthetic-secret-only",
                    "abcdefghijklmnopqrst",
                    "abcdefghijklmnop",
                    "AKIAABCDEFGHIJKLMNOP",
                    "eyJabcdefgh.abcdefgh.abcdefgh",
                ] {
                    assert!(!evidence.contains(marker), "{id}");
                }
                if id.starts_with("secret-9-") || id.starts_with("secret-10-") {
                    assert!(!evidence.contains("12345"), "{id}");
                }
            }
        }
    }
}
#[test]
fn priority_semantics() {
    assert_eq!(
        cause(&log("out of memory failed to allocate kv")),
        json!("kv-cache")
    );
}
#[test]
fn forced_semantics() {
    let t = log("fatal").replace(
        "instance name=A exited",
        "force-killing model instance name=A\ninstance name=A exited",
    );
    assert_eq!(analyse(&t).unwrap()["failures"], json!([]));
}
#[test]
fn slot_semantics() {
    let v=analyse("instance name=A exited with status 1\ninstance name=B exited with status 2\ninstance name=A exited with status 3").unwrap();
    assert_eq!(
        v["failures"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["model"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["B", "A"]
    );
}
#[test]
fn distinct_port_semantics() {
    let t = format!(
        "{}\nspawning server instance with name=A on port 1\n[1] model loaded",
        log("fatal")
    );
    assert_eq!(
        analyse(&t).unwrap()["failures"].as_array().unwrap().len(),
        1
    );
}
#[test]
fn case_sensitive_success_semantics() {
    let t = format!(
        "{}\nspawning server instance with name=A on port 2\n[2] MODEL LOADED",
        log("fatal")
    );
    assert_eq!(
        analyse(&t).unwrap()["failures"].as_array().unwrap().len(),
        1
    );
}
#[test]
fn redaction_semantics() {
    let v = analyse(&log("fatal api_key=synthetic-secret-only")).unwrap();
    assert_eq!(
        v["failures"][0]["evidence"],
        json!(["fatal api_key=[redacted]"])
    );
}
#[test]
fn crossline_semantics() {
    let v=analyse("spawning server instance with name=A on port 1\nAuthorization:\n[1] fatal synthetic-secret-only\ninstance name=A exited with status 1").unwrap();
    assert_eq!(v["failures"][0]["evidence"], json!([]));
}
#[test]
fn caps_and_work_refuse_whole_result() {
    assert_eq!(analyse(&"x".repeat(LOG_CAP + 1)), Err(Refusal::LogCap));
    assert_eq!(analyse(&"x".repeat(LINE_CAP + 1)), Err(Refusal::LineCap));
    assert_eq!(
        analyse(&"\n".repeat(LINE_COUNT + 1)),
        Err(Refusal::LineCount)
    );
    assert_eq!(
        analyse(&format!(
            "instance name={} exited with status 1",
            "x".repeat(1025)
        )),
        Err(Refusal::NameCap)
    );
    assert_eq!(
        analyse(&format!("[{}] fatal", "1".repeat(65))),
        Err(Refusal::PortCap)
    );
    let ports = (0..=ENTRY_CAP)
        .map(|p| format!("[{p}] fatal\n"))
        .collect::<String>();
    assert_eq!(analyse(&ports), Err(Refusal::EntryCap));
    let names = (0..=ENTRY_CAP)
        .map(|p| format!("instance name=M{p} exited with status 1\n"))
        .collect::<String>();
    assert_eq!(analyse(&names), Err(Refusal::EntryCap));
    let a = Analyzer::new().unwrap();
    let cancel = AtomicBool::new(true);
    assert_eq!(
        a.analyse_json(
            "",
            &Work {
                deadline: Instant::now() + Duration::from_secs(1),
                cancelled: &cancel
            }
        ),
        Err(Refusal::Cancelled)
    );
    let cancel = AtomicBool::new(false);
    assert_eq!(
        a.analyse_json(
            "",
            &Work {
                deadline: Instant::now(),
                cancelled: &cancel
            }
        ),
        Err(Refusal::Deadline)
    );
    assert!(analyse(&"x".repeat(LINE_CAP)).is_ok());
    assert!(analyse(&"\n".repeat(LINE_COUNT)).is_ok());
}
#[test]
fn bounded_redactor_policy_matches_existing_policy() {
    // Every policy family and counter exception, including Python Unicode classes.
    let redactor = noevia_model_manager::Redactor::new().unwrap();
    for text in [
        "fatal api_key=x",
        "fatal token=123",
        "fatal n_tokens=123 tokens=23 token_count=33 session=1.2",
        "fatal Authorization: Basic synthetic-secret-only",
        "fatal authorızatıon: synthetic-secret-only",
        "fatal https://a:b@example.invalid",
        "fatal hf_abcdefghijklmnopqrst",
        "fatal sk-abcdefghijklmnop",
        "fatal ghp_abcdefghijklmnopqrst",
        "fatal AKIAABCDEFGHIJKLMNOP",
        "fatal eyJabcdefgh.abcdefgh.abcdefgh",
        "fatal password='x' cookie=x session=x credential=x",
        "fatal x_token=y",
        "fatal authorization: bearer x",
        "fatal Basiſ synthetic",
    ] {
        assert_eq!(
            analyse(&log(text)).unwrap()["failures"][0]["evidence"],
            json!([redactor.redact(text).unwrap()])
        );
    }
}
use proptest::prelude::*;
#[test]
fn snapshot_and_serialization_caps_are_distinct() {
    fn shared_output(names: usize) -> String {
        let mut text = (0..names)
            .map(|n| format!("spawning server instance with name=M{n} on port 1\n"))
            .collect::<String>();
        for _ in 0..4 {
            text.push_str("[1] ");
            text.push_str(&"x".repeat(LINE_CAP - 4));
            text.push('\n');
        }
        text.extend((0..names).map(|n| format!("instance name=M{n} exited with status 1\n")));
        text
    }
    assert_eq!(analyse(&shared_output(129)), Err(Refusal::RetainedCap));
    assert_eq!(analyse(&shared_output(65)), Err(Refusal::JsonCap));
}
proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]
    #[test]
    fn arbitrary_finite_utf8_no_panic(text in ".{0,4096}") { let a=Analyzer::new().unwrap(); let cancel=AtomicBool::new(false); if let Ok(bytes)=a.analyse_json(&text,&Work{deadline:Instant::now()+Duration::from_secs(5),cancelled:&cancel}) { prop_assert!(bytes.len()<=JSON_CAP); let v:Value=serde_json::from_slice(&bytes).unwrap(); for f in v["failures"].as_array().unwrap() { prop_assert!(f["evidence"].as_array().unwrap().len()<=4); } } }
    #[test]
    fn arbitrary_lossy_bytes_no_panic(bytes in prop::collection::vec(any::<u8>(),0..4096)) { let text=String::from_utf8_lossy(&bytes); prop_assert!(analyse(&text).is_ok()); }
    #[test]
    fn secret_output_and_four_line_invariant(lines in prop::collection::vec("[a-z]{0,24}",0..40)) { let text=log(&lines.iter().map(|s|format!("fatal {s} api_key=synthetic-secret-only\n[1] ")).collect::<String>()); let v=analyse(&text).unwrap(); prop_assert!(!v.to_string().contains("synthetic-secret-only")); prop_assert!(v["failures"][0]["evidence"].as_array().unwrap().len()<=4); }
}
