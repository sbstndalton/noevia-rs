//! Differential tests against the actual services Python (`tools/gen-model-updates-oracle.py`).
//! The tree, models.ini and store of every case are rebuilt here from the fixture declaration;
//! the synthetic root adapter stands in for the Linux-only kernel-contained opener (the
//! integration test runs the same cases through the real opener and router on Linux).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
use super::*;
use rusqlite::Connection;
use serde_json::Value;
use std::time::{SystemTime, UNIX_EPOCH};

/// sha256 of the services sources the fixture came from (noevia-services 42065fa0f173cd9f0d03d899b2a92e6b7db5a87b).
const ORACLE: [(&str, &str); 9] = [
    (
        "api.py",
        "800edae73e7f7764845510ddf4e108bc2ea0a639f721e06e825a4a4644600dd6",
    ),
    (
        "autoconfig_core.py",
        "d9da1c88fe6ef8db5a0f3784ca5e024436c110d338deed9ea04bffb7870e15ac",
    ),
    (
        "db.py",
        "7ac62912f2295d0c96387f55c21e6f42e5815800c44a3920b7ebc2f3a0217478",
    ),
    (
        "discover.py",
        "f92603267381ea40976fb9642034d18ee24048769f1dc2ab7e68aeac9d97b725",
    ),
    (
        "helpers.py",
        "fcf2ccb14e5e068f97bb7a4b7985ab7f4f4855b940eb6d9e3838f239ad97918d",
    ),
    (
        "ini.py",
        "0c674795902c74b5d1511363b08cf8dd69fe470152f0b6b847668a270076dda4",
    ),
    (
        "main.py",
        "023b48e9fdf3a209afcb961d7602f24227546413781de158aae7de5b378dc4b1",
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
        "../../tests/fixtures/model-updates-python.v1.json"
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

/// Builds one case like the generator does; returns (models, ini, data).
pub(crate) fn build(root: &Path, case: &Value) -> (PathBuf, PathBuf, PathBuf) {
    let models = root.join("models");
    let ini = models.join("models.ini");
    let data = root.join("data");
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
                let path = models.join(file[0].as_str().unwrap());
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(&path, b"").unwrap();
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
                let ns = file[1]
                    .as_i64()
                    .map(i128::from)
                    .or_else(|| file[1].as_u64().map(i128::from))
                    .unwrap();
                let f = std::fs::File::options()
                    .write(true)
                    .open(models.join(file[0].as_str().unwrap()))
                    .unwrap();
                let t = time(ns);
                f.set_times(std::fs::FileTimes::new().set_accessed(t).set_modified(t))
                    .unwrap();
            }
        }
        other => panic!("unknown models kind {other}"),
    }
    match &case["store"] {
        Value::Null => {}
        Value::Object(store) => {
            std::fs::create_dir(&data).unwrap();
            let db = data.join("model_loader.db");
            if let Some(bytes) = store.get("bytes") {
                std::fs::write(&db, hex(bytes.as_str().unwrap())).unwrap();
            } else {
                let c = Connection::open(&db).unwrap();
                let journal = store["journal"].as_str().unwrap().to_uppercase();
                let _: String = c
                    .query_row(&format!("PRAGMA journal_mode={journal}"), [], |r| r.get(0))
                    .unwrap();
                for statement in store["sql"].as_array().unwrap() {
                    c.execute_batch(statement.as_str().unwrap()).unwrap();
                }
            }
        }
        other => panic!("unknown store {other}"),
    }
    (models, ini, data)
}

/// Applies (or, with `restore`, undoes) the case's folder permission modes.
pub(crate) fn modes(models: &Path, case: &Value, restore: bool) {
    use std::os::unix::fs::PermissionsExt;
    for item in case["modes"].as_array().into_iter().flatten() {
        let mode = if restore {
            0o700
        } else {
            u32::try_from(item[1].as_u64().unwrap()).unwrap()
        };
        std::fs::set_permissions(
            models.join(item[0].as_str().unwrap()),
            std::fs::Permissions::from_mode(mode),
        )
        .unwrap();
    }
}

fn run(models: &Path, ini: &Path, data: &Path) -> Outcome {
    compute(
        models,
        ini,
        &data.join("model_loader.db"),
        Instant::now() + DEADLINE,
        Root::synthetic,
    )
}

#[test]
fn fixture_provenance_is_the_pinned_services_source() {
    let oracle = &fixture()["oracle"];
    for (name, sha) in ORACLE {
        assert_eq!(oracle[name], sha, "{name}");
    }
}

#[test]
fn actual_python_differential() {
    let cases = fixture()["cases"].as_array().unwrap().clone();
    assert!(cases.len() >= 40);
    for case in cases {
        let id = case["id"].as_str().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let (models, ini, data) = build(dir.path(), &case);
        let db = data.join("model_loader.db");
        let before = std::fs::read(&db).ok();
        let expected = case["body"].as_str().unwrap();
        modes(&models, &case, false);
        let outcome = run(&models, &ini, &data);
        modes(&models, &case, true);
        match (outcome, case["status"].as_u64().unwrap()) {
            (Outcome::Json(body), 200) => {
                assert_eq!(case["content_type"], "application/json", "{id}");
                assert_eq!(String::from_utf8(body).unwrap(), expected, "{id}");
            }
            (Outcome::Failed, 500) => {
                assert_eq!(case["content_type"], "text/plain; charset=utf-8", "{id}");
                assert_eq!(expected, "Internal Server Error", "{id}");
            }
            (other, status) => panic!("{id}: native {other:?}, Python {status}"),
        }
        // Read-only: the store is never created and its bytes never change.
        assert_eq!(std::fs::read(&db).ok(), before, "{id}");
    }
}

#[test]
fn fromisoformat_matches_cpython_corpus() {
    let corpus = fixture()["fromisoformat"].as_array().unwrap().clone();
    assert!(corpus.len() >= 3000);
    let mut accepted = 0;
    for item in corpus {
        let input = item["input"].as_str().unwrap();
        let got = fromisoformat(input).map(|p| {
            serde_json::json!([
                p.year,
                p.month,
                p.day,
                p.hour,
                p.minute,
                p.second,
                p.microsecond,
                p.offset_us
            ])
        });
        let want = (!item["parsed"].is_null()).then(|| item["parsed"].clone());
        accepted += usize::from(want.is_some());
        assert_eq!(got, want, "{input:?}");
    }
    assert!(accepted >= 200, "corpus must exercise accepted strings too");
}

#[test]
fn float_repr_matches_python() {
    for (value, repr) in [
        (0.0, "0.0"),
        (-0.0, "-0.0"),
        (1.0, "1.0"),
        (1_700_000_000.5, "1700000000.5"),
        (1e16, "1e+16"),
        (9.999_999_999_999_998e15, "9999999999999998.0"),
        (1.234_567_890_123_456_8e16, "1.2345678901234568e+16"),
        (1.5e-5, "1.5e-05"),
        (1e-4, "0.0001"),
        (0.1, "0.1"),
        (-2.5e-300, "-2.5e-300"),
        (1.797_693_134_862_315_7e308, "1.7976931348623157e+308"),
        (123_456.0, "123456.0"),
    ] {
        assert_eq!(py_float_repr(value), repr, "{value}");
    }
}

#[test]
fn timestamp_rounding_and_range() {
    assert_eq!(from_timestamp(0.0), Some(0));
    assert_eq!(from_timestamp(0.000_000_5), Some(0)); // half-even down
    assert_eq!(from_timestamp(0.000_001_5), Some(2)); // half-even up
    assert_eq!(from_timestamp(-0.000_001_5), Some(-2));
    assert_eq!(from_timestamp(1.999_999_9), Some(2_000_000));
    // datetime.fromtimestamp raises past year 9999 (Python answers 500).
    assert_eq!(from_timestamp(253_402_300_800.0), None);
    assert!(from_timestamp(253_402_300_799.0).is_some());
    assert_eq!(from_timestamp(-62_135_596_801.0), None);
    assert_eq!(from_timestamp(f64::INFINITY), None);
}

#[test]
fn mtime_past_year_9999_answers_500() {
    let dir = tempfile::tempdir().unwrap();
    let entries = [Entry {
        name: "a.gguf".into(),
        mtime: 253_402_300_800.0,
    }];
    let mut checks = HashMap::new();
    checks.insert(
        "a.gguf".to_owned(),
        (
            Stored::Text("2024-01-01T00:00:00Z".into()),
            Stored::Real(1.0),
        ),
    );
    assert!(render(&entries, &checks).is_err());
    // An unknown remote never computes the local time, as in Python.
    checks.insert("a.gguf".to_owned(), (Stored::Null, Stored::Real(1.0)));
    assert!(render(&entries, &checks).is_ok());
    drop(dir);
}

#[test]
fn companion_rule_is_pythons() {
    for (name, expected) in [
        ("mmproj-x.gguf", true),
        ("X-MMPROJ.gguf", true),
        ("x.imatrix.gguf", true),
        ("IMATRIX.gguf", true),
        ("imatr\u{0131}x.gguf", true),
        ("\u{0130}matrix.gguf", false),
        ("big-draft-Q4.gguf", true),
        ("mtp-head.gguf", true),
        ("model-MTP.gguf", true),
        ("nomtp-draft.gguf", false),
        ("plain-Q4_K_M.gguf", false),
    ] {
        assert_eq!(companion(name), expected, "{name}");
    }
}

#[test]
fn listing_cap_refuses_instead_of_reading_everything() {
    let dir = tempfile::tempdir().unwrap();
    let models = dir.path().join("models");
    std::fs::create_dir(&models).unwrap();
    for i in 0..=MODEL_CAP {
        std::fs::write(models.join(format!("m{i:05}.gguf")), b"").unwrap();
    }
    let data = dir.path().join("data");
    std::fs::create_dir(&data).unwrap();
    Connection::open(data.join("model_loader.db"))
        .unwrap()
        .execute_batch(
            "CREATE TABLE update_check (filename, repo_id, hf_last_modified, checked_at)",
        )
        .unwrap();
    assert_eq!(
        run(&models, &models.join("models.ini"), &data),
        Outcome::Refused
    );
    std::fs::remove_file(models.join("m00000.gguf")).unwrap();
    assert_eq!(
        run(&models, &models.join("models.ini"), &data),
        Outcome::Json(b"{\"status\":{}}".to_vec())
    );
}

#[test]
fn oversized_ini_and_store_values_answer_500() {
    let dir = tempfile::tempdir().unwrap();
    let models = dir.path().join("models");
    std::fs::create_dir(&models).unwrap();
    std::fs::write(models.join("a.gguf"), b"").unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir(&data).unwrap();
    let c = Connection::open(data.join("model_loader.db")).unwrap();
    c.execute_batch("CREATE TABLE update_check (filename TEXT PRIMARY KEY, repo_id TEXT NOT NULL, hf_last_modified TEXT, checked_at REAL NOT NULL)")
        .unwrap();
    let ini = models.join("models.ini");
    std::fs::write(&ini, format!("[a]\nx = {}\n", "y".repeat(INI_CAP))).unwrap();
    assert_eq!(run(&models, &ini, &data), Outcome::Failed);
    std::fs::write(&ini, "[a]\nx = 1\n").unwrap();
    let cap = usize::try_from(VALUE_CAP).unwrap();
    for (len, ok) in [(cap, true), (cap + 1, false)] {
        c.execute(
            "INSERT OR REPLACE INTO update_check VALUES('a.gguf', ?1, NULL, 1.0)",
            ["r".repeat(len)],
        )
        .unwrap();
        let outcome = run(&models, &ini, &data);
        assert_eq!(matches!(outcome, Outcome::Json(_)), ok, "{len}");
        assert_eq!(matches!(outcome, Outcome::Failed), !ok, "{len}");
    }
}

#[test]
fn fifo_ini_answers_500_without_blocking() {
    let dir = tempfile::tempdir().unwrap();
    let models = dir.path().join("models");
    std::fs::create_dir(&models).unwrap();
    let ini = models.join("models.ini");
    assert!(std::process::Command::new("mkfifo")
        .arg(&ini)
        .status()
        .unwrap()
        .success());
    let data = dir.path().join("data");
    std::fs::create_dir(&data).unwrap();
    let started = Instant::now();
    assert_eq!(run(&models, &ini, &data), Outcome::Failed);
    assert!(started.elapsed() < std::time::Duration::from_secs(2));
}

#[test]
fn total_output_is_capped() {
    let big = "x".repeat(60_000);
    let mut checks = HashMap::new();
    let mut entries = Vec::new();
    for i in 0..200 {
        let name = format!("m{i}.gguf");
        checks.insert(name.clone(), (Stored::Null, Stored::Text(big.clone())));
        entries.push(Entry { name, mtime: 0.0 });
    }
    assert!(render(&entries[..100], &checks).is_ok());
    assert!(render(&entries, &checks).is_err());
}

/// noevia#1922: the update store as Python leaves it, WAL with no -wal/-shm, reads (and stays WAL).
#[test]
fn closed_wal_store_is_read() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("model_loader.db");
    let c = Connection::open(&path).unwrap();
    let _: String = c
        .query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))
        .unwrap();
    c.execute_batch(
        "CREATE TABLE update_check (filename TEXT PRIMARY KEY, repo_id TEXT NOT NULL, hf_last_modified TEXT, checked_at REAL NOT NULL);
         INSERT INTO update_check VALUES('a.gguf', 'synthetic/repo', NULL, 1.0);",
    )
    .unwrap();
    drop(c);
    assert!(!dir.path().join("model_loader.db-shm").exists());
    let Ok(checks) = read_checks(
        &path,
        std::time::Instant::now() + std::time::Duration::from_secs(4),
    ) else {
        panic!("closed WAL store was not read");
    };
    assert_eq!(checks.len(), 1);
    let mode: String = Connection::open(&path)
        .unwrap()
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .unwrap();
    assert_eq!(mode, "wal");
}
