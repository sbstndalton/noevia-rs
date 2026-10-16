//! Differential tests against the actual services Python (`tools/gen-model-detail-oracle.py`).
//! Every case's tree and models.ini are rebuilt here from the fixture declaration; the synthetic
//! root adapter stands in for the Linux-only kernel-contained opener (`tests/model_detail.rs`
//! runs the same cases through the real opener, router and a child stand-in on Linux).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
use super::*;
use serde_json::Value;
use std::time::{SystemTime, UNIX_EPOCH};

/// sha256 of the services sources the fixture came from (noevia-services 419a72bdb3acc386baad6407ed02a634997d3abf).
pub(crate) const ORACLE: [(&str, &str); 10] = [
    (
        "api.py",
        "800edae73e7f7764845510ddf4e108bc2ea0a639f721e06e825a4a4644600dd6",
    ),
    (
        "autoconfig.py",
        "15cc464f19cb43a68752572f6fa91046fe34c37296decef348a84dcbe75dc1c6",
    ),
    (
        "autoconfig_core.py",
        "d9da1c88fe6ef8db5a0f3784ca5e024436c110d338deed9ea04bffb7870e15ac",
    ),
    (
        "config.py",
        "ab6e348127b65f224d58fa2f805fefdfc7a472e14eb9e9ff540120e51f21f57f",
    ),
    (
        "discover.py",
        "f92603267381ea40976fb9642034d18ee24048769f1dc2ab7e68aeac9d97b725",
    ),
    (
        "gguf_meta.py",
        "8ac984794fb9087fcd774cfe1a4e83eac847c1675224c8c09029fb9dcbd43b0a",
    ),
    (
        "ini.py",
        "0c674795902c74b5d1511363b08cf8dd69fe470152f0b6b847668a270076dda4",
    ),
    (
        "main.py",
        "cc01b06acd695c6de26d751414ac963da3c473c8a44aaa60af952a20f1a245da",
    ),
    (
        "services.py",
        "aadf227ddc3666cebb1631a0ae543c8090539a382e8ca52710fc7a5458f1d041",
    ),
    (
        "utils.py",
        "3f56fb6b04094ec19c09c191a0d4267e2d233e3461884be0648e4875e6ef6426",
    ),
];

pub(crate) fn fixture() -> Value {
    serde_json::from_str(include_str!(
        "../../tests/fixtures/model-detail-python.v1.json"
    ))
    .unwrap()
}

fn hex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap())
        .collect()
}

fn time(ns: i128) -> SystemTime {
    let magnitude = std::time::Duration::from_nanos(u64::try_from(ns.unsigned_abs()).unwrap());
    if ns >= 0 {
        UNIX_EPOCH + magnitude
    } else {
        UNIX_EPOCH - magnitude
    }
}

/// Builds one case like the generator does; returns (models, ini).
pub(crate) fn build(root: &Path, case: &Value) -> (PathBuf, PathBuf) {
    let models = root.join("models");
    let ini = models.join("models.ini");
    match case["models"].as_str().unwrap_or("dir") {
        "missing" => {}
        "file" => std::fs::write(&models, b"").unwrap(),
        kind @ ("dir" | "link") => {
            if kind == "link" {
                let real = root.join("real-models");
                std::fs::create_dir(&real).unwrap();
                std::os::unix::fs::symlink(&real, &models).unwrap();
            } else {
                std::fs::create_dir(&models).unwrap();
            }
            for dir in case["dirs"].as_array().into_iter().flatten() {
                std::fs::create_dir_all(models.join(dir.as_str().unwrap())).unwrap();
            }
            let files = case["files"].as_array().cloned().unwrap_or_default();
            for file in &files {
                let path = models.join(file["name"].as_str().unwrap());
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(&path, hex(file["hex"].as_str().unwrap())).unwrap();
                if let Some(size) = file["size"].as_u64() {
                    std::fs::File::options()
                        .write(true)
                        .open(&path)
                        .unwrap()
                        .set_len(size)
                        .unwrap();
                }
            }
            for link in case["links"].as_array().into_iter().flatten() {
                std::os::unix::fs::symlink(
                    link[1].as_str().unwrap(),
                    models.join(link[0].as_str().unwrap()),
                )
                .unwrap();
            }
            for link in case["abslinks"].as_array().into_iter().flatten() {
                let target = std::fs::canonicalize(&models)
                    .unwrap()
                    .join(link[1].as_str().unwrap());
                std::os::unix::fs::symlink(target, models.join(link[0].as_str().unwrap())).unwrap();
            }
            match &case["ini"] {
                Value::Null => {}
                Value::String(s) if s == "dir" => std::fs::create_dir(&ini).unwrap(),
                Value::Object(o) => {
                    let bytes = match o.get("text") {
                        Some(text) => text.as_str().unwrap().as_bytes().to_vec(),
                        None => hex(o["hex"].as_str().unwrap()),
                    };
                    std::fs::write(&ini, bytes).unwrap();
                }
                other => panic!("unknown ini {other}"),
            }
            for file in &files {
                let ns = file["mtime"]
                    .as_i64()
                    .map(i128::from)
                    .or_else(|| file["mtime"].as_u64().map(i128::from))
                    .unwrap();
                let f = std::fs::File::options()
                    .write(true)
                    .open(models.join(file["name"].as_str().unwrap()))
                    .unwrap();
                let t = time(ns);
                f.set_times(std::fs::FileTimes::new().set_accessed(t).set_modified(t))
                    .unwrap();
            }
        }
        other => panic!("unknown models kind {other}"),
    }
    (models, ini)
}

/// (status, body) of one native answer, with the recorded child answer for the live fields.
fn answer(models: &Path, ini: &Path, result: &Value) -> (u16, String) {
    let step = locate(
        models,
        ini,
        result["query"].as_str().unwrap(),
        Instant::now() + DEADLINE,
        Root::synthetic,
    );
    let outcome = match step {
        Step::Done(outcome) => {
            assert!(result["runtime"].is_null(), "{result}");
            outcome
        }
        Step::Located(located) => {
            let runtime = &result["runtime"];
            assert_eq!(
                runtime_query(&located),
                format!(
                    "bytes={}&fit={}",
                    runtime["bytes"],
                    if runtime["fit"].as_bool().unwrap() {
                        "1"
                    } else {
                        "0"
                    }
                ),
                "{result}"
            );
            splice(&located, runtime["body"].as_str().unwrap().as_bytes())
        }
    };
    match outcome {
        Outcome::Json(status, body) => (status, String::from_utf8(body).unwrap()),
        Outcome::Failed => (500, "Internal Server Error".into()),
        Outcome::Refused => panic!("refused {result}"),
        Outcome::Unavailable => panic!("unavailable {result}"),
    }
}

#[test]
fn fixture_provenance_is_the_pinned_services_source() {
    let oracle = &fixture()["oracle"];
    for (name, sha) in ORACLE {
        assert_eq!(oracle[name], sha, "{name}");
    }
    assert_eq!(oracle.as_object().unwrap().len(), ORACLE.len());
}

#[test]
fn every_case_matches_the_actual_python_route() {
    let fixture = fixture();
    let cases = fixture["cases"].as_array().unwrap();
    assert!(cases.len() >= 15);
    let mut checked = 0;
    for case in cases {
        let tmp = tempfile::tempdir().unwrap();
        let (models, ini) = build(tmp.path(), case);
        let prefix = serde_json::to_string(models.to_str().unwrap()).unwrap();
        let prefix = &prefix[1..prefix.len() - 1];
        for result in case["results"].as_array().unwrap() {
            let (status, body) = answer(&models, &ini, result);
            assert_eq!(
                (status, body.replace(prefix, "@MODELS@")),
                (
                    u16::try_from(result["status"].as_u64().unwrap()).unwrap(),
                    result["body"].as_str().unwrap().to_owned()
                ),
                "case {} query {}",
                case["id"],
                result["query"]
            );
            checked += 1;
        }
    }
    assert!(checked >= 86, "{checked}");
}

/// A GGUF v3 header with no tensors and no key/value pairs.
const EMPTY_HEADER: &str = "474755460300000000000000000000000000000000000000";

fn located(fit: bool) -> Located {
    Located {
        head: "{\"key\":\"a.gguf\",".into(),
        tail: ",\"badges\":[]}".into(),
        model_id: "short-id".into(),
        bytes: 7,
        fit,
    }
}

#[test]
fn splice_places_the_childs_fields_verbatim_and_refuses_malformed_answers() {
    let body = br#"{"loaded":{"short-id":["b","a"],"x":["c"]},"fit":[{"vram_gb":1e+16,"r":0.30000000000000004}]}"#;
    assert_eq!(
        splice(&located(true), body),
        Outcome::Json(
            200,
            br#"{"key":"a.gguf","loadedOn":["b","a"],"fit":[{"vram_gb":1e+16,"r":0.30000000000000004}],"badges":[]}"#.to_vec()
        )
    );
    assert_eq!(
        splice(&located(false), br#"{"loaded":{},"fit":[]}"#),
        Outcome::Json(
            200,
            br#"{"key":"a.gguf","loadedOn":[],"fit":[],"badges":[]}"#.to_vec()
        )
    );
    for bad in [
        &b""[..],
        b"[]",
        b"{\"loaded\":{}}",
        b"{\"fit\":[]}",
        b"{\"loaded\":[],\"fit\":[]}",
        b"{\"loaded\":{\"a\":[1]},\"fit\":[]}",
        b"{\"loaded\":{},\"fit\":{}}",
        b"{\"loaded\":{},\"fit\":[]",
        b"{\"loaded\":{},\"fit\":NaN}",
    ] {
        assert_eq!(splice(&located(true), bad), Outcome::Unavailable, "{bad:?}");
    }
    // A companion never shows chips, so a child that sends some is not trusted.
    assert_eq!(
        splice(&located(false), br#"{"loaded":{},"fit":[{}]}"#),
        Outcome::Unavailable
    );
}

#[test]
fn python_int_of_text() {
    for (text, want) in [
        (" 12 ", Some(12)),
        ("+0_4", Some(4)),
        ("-7", Some(-7)),
        ("\u{1c}9\u{1f}", Some(9)),
        ("1__2", Some(0)),
        ("_1", Some(0)),
        ("1_", Some(0)),
        ("", Some(0)),
        ("x", Some(0)),
        ("0x10", Some(0)),
        ("1.5", Some(0)),
        ("é", Some(0)),
        ("١٢", None),
        ("99999999999999999999999999999999999999999", None),
    ] {
        let got = int_of_str(text).ok();
        assert_eq!(got, want, "{text:?}");
    }
    assert_eq!(int_of_str(&"9".repeat(4301)).ok(), Some(0));
}

#[test]
fn human_sizes_round_like_python() {
    for (n, want) in [
        (0u128, "0 B"),
        (1023, "1023 B"),
        (1024, "1.0 KB"),
        (1280, "1.2 KB"),
        (1792, "1.8 KB"),
        (1_048_575, "1024.0 KB"),
        (5 * 1024u128.pow(3) + 7, "5.0 GB"),
        (1024u128.pow(6), "1.0 EB"),
        (1024u128.pow(7), "1024.0 EB"),
    ] {
        assert_eq!(human_bytes(n), want, "{n}");
    }
}

#[test]
fn duplicate_section_headers_follow_python_regex() {
    assert!(has_duplicate_sections("[a]\nx = 1\n[a]\n"));
    assert!(has_duplicate_sections(" [a] ; note\n[a]#x\n"));
    assert!(!has_duplicate_sections("[a]\n[b]\n"));
    assert!(!has_duplicate_sections("[a]\n[a]]\n"));
    assert!(!has_duplicate_sections("[a]\n[a] x\n"));
}

#[test]
fn model_paths_with_parent_segments_are_refused_not_guessed() {
    let tmp = tempfile::tempdir().unwrap();
    let models = tmp.path().join("models");
    std::fs::create_dir(&models).unwrap();
    std::fs::write(models.join("a.gguf"), hex(EMPTY_HEADER)).unwrap();
    let ini = models.join("models.ini");
    std::fs::write(&ini, "[a-id]\nmodel = sub/../a.gguf\n").unwrap();
    assert!(matches!(
        locate(
            &models,
            &ini,
            "key=a.gguf",
            Instant::now() + DEADLINE,
            Root::synthetic
        ),
        Step::Done(Outcome::Refused)
    ));
    // `.` and empty segments are pathlib-normalised, as Python's is_file() sees them.
    std::fs::write(&ini, "[a-id]\nmodel = .//a.gguf/\n").unwrap();
    let step = locate(
        &models,
        &ini,
        "key=a.gguf",
        Instant::now() + DEADLINE,
        Root::synthetic,
    );
    let found = match step {
        Step::Located(found) => found,
        Step::Done(outcome) => panic!("not located: {outcome:?}"),
    };
    // The section maps to the literal rel `.//a.gguf/`, which is not the file's own rel, so the
    // entry falls back to name matching and keeps its stem as the id.
    assert_eq!(found.model_id, "a");
}

#[test]
fn an_expired_deadline_refuses() {
    let tmp = tempfile::tempdir().unwrap();
    let models = tmp.path().join("models");
    std::fs::create_dir(&models).unwrap();
    std::fs::write(models.join("a.gguf"), hex(EMPTY_HEADER)).unwrap();
    assert!(matches!(
        locate(
            &models,
            &models.join("models.ini"),
            "key=a.gguf",
            Instant::now(),
            Root::synthetic
        ),
        Step::Done(Outcome::Refused)
    ));
}
