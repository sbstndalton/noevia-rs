//! Actual pinned Python operation results and complete synthetic SQL state.
//! Receipt identity is native-only and tested as an additive invariant.
use diary_workspace_transaction::{execute, Request};
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, error::Error};
mod common;
type TestResult<T> = Result<T, Box<dyn Error>>;
fn field<'a>(value: &'a Value, key: &str) -> TestResult<&'a Value> {
    value
        .get(key)
        .ok_or_else(|| format!("missing {key}").into())
}
fn text<'a>(value: &'a Value, key: &str) -> TestResult<&'a str> {
    field(value, key)?
        .as_str()
        .ok_or_else(|| format!("nontext {key}").into())
}
fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn ordered(value: Value) -> Value {
    match value {
        Value::Object(values) => Value::Object(
            values
                .into_iter()
                .map(|(key, value)| (key, ordered(value)))
                .collect::<BTreeMap<_, _>>()
                .into_iter()
                .collect(),
        ),
        Value::Array(values) => Value::Array(values.into_iter().map(ordered).collect()),
        other => other,
    }
}
fn snapshot(db: &Connection) -> TestResult<String> {
    let mut files = Vec::new();
    let mut query = db.prepare("SELECT path,data,version,updated FROM files ORDER BY path")?;
    for row in query.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, Vec<u8>>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, f64>(3)?,
        ))
    })? {
        let (path, data, version, updated) = row?;
        files.push(json!({"path":path,"bytes":data.len(),"dataSHA256":hash(&data),"version":version,"updated":updated}));
    }
    let strings = |table: &str| -> TestResult<Vec<String>> {
        let mut q = db.prepare(&format!("SELECT path FROM {table} ORDER BY path"))?;
        let rows = q.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?;
        Ok(rows)
    };
    let mut q = db.prepare("SELECT key,value FROM meta ORDER BY key")?;
    let meta = q
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        .collect::<Result<BTreeMap<_, _>, _>>()?;
    assert_eq!(
        db.query_row::<i64, _, _>("SELECT count(*) FROM workspace_imports", [], |r| r.get(0))?,
        0
    );
    assert_eq!(
        db.query_row::<i64, _, _>("SELECT count(*) FROM backups", [], |r| r.get(0))?,
        0
    );
    let value = json!({"files":files,"directories":strings("directories")?,"meta":meta,"outbox":strings("workspace_index_outbox")?,"imports":[],"backups":[]});
    Ok(hash(&serde_json::to_vec(&ordered(value))?))
}
#[test]
fn frozen_python_mutation_kernels_and_native_receipts() -> TestResult<()> {
    let fixture: Value = serde_json::from_str(include_str!("fixtures/mutation-contract.json"))?;
    let now = field(&fixture, "now")?.as_f64().ok_or("now")?;
    let cases = field(&fixture, "cases")?.as_array().ok_or("cases")?;
    assert_eq!(cases.len(), 135);
    for case in cases {
        let (_dir, path) = common::fixture()?;
        let db = Connection::open(&path)?;
        db.execute_batch("DELETE FROM files; DELETE FROM directories;")?;
        let root = text(case, "root")?;
        let input = field(case, "input")?;
        let body = field(case, "sourceBody")?;
        let profile = input.get("profile").and_then(Value::as_str).unwrap_or("");
        db.execute(
            "UPDATE meta SET value=? WHERE key='generation'",
            [input
                .get("generation")
                .and_then(Value::as_str)
                .unwrap_or("0")],
        )?;
        for file in field(&fixture, "files")?.as_array().ok_or("files")? {
            let relative = text(file, "path")?;
            let data = if profile == "large" && relative == "Notes/one.md" {
                vec![b'x'; 512 * 1024 + 1]
            } else {
                text(file, "data")?.as_bytes().to_vec()
            };
            db.execute(
                "INSERT INTO files VALUES(?,?,?,1.0)",
                params![
                    diary_workspace_rules::join(root, &[relative]),
                    data,
                    hash(&data)
                ],
            )?;
        }
        for directory in field(&fixture, "directories")?
            .as_array()
            .ok_or("directories")?
        {
            db.execute(
                "INSERT INTO directories VALUES(?)",
                [diary_workspace_rules::join(
                    root,
                    &[directory.as_str().ok_or("directory")?],
                )],
            )?;
        }
        if profile == "nonmarkdown" {
            db.execute(
                "INSERT INTO files VALUES(?,?,?,1.0)",
                params![
                    diary_workspace_rules::join(root, &["Notes/Folder/opaque.bin"]),
                    b"opaque".as_slice(),
                    hash(b"opaque")
                ],
            )?;
        }
        if profile == "wide" {
            for i in 0..501 {
                db.execute(
                    "INSERT INTO files VALUES(?,?,?,1.0)",
                    params![
                        diary_workspace_rules::join(root, &[&format!("Wide/{i}.md")]),
                        b"x".as_slice(),
                        hash(b"x")
                    ],
                )?;
            }
        }
        if let Some(raw) = case.get("seedCapsule").and_then(Value::as_str) {
            db.execute(
                "INSERT INTO files VALUES(?,?,?,?)",
                params![
                    format!(".noevia-trash/{}.json", text(&fixture, "capsuleId")?),
                    raw.as_bytes(),
                    hash(raw.as_bytes()),
                    now - 10.0
                ],
            )?;
            if input.get("prepare").and_then(Value::as_str) == Some("trashed") {
                db.execute(
                    "DELETE FROM files WHERE path=?",
                    [diary_workspace_rules::join(root, &["Notes/one.md"])],
                )?;
            }
        }
        let mut cfg = common::config();
        *cfg.get_mut("remoteRoot").ok_or("root")? = json!(root);
        let op = text(input, "op")?;
        let mut request = json!({"version":1,"dbPath":path,"expectedTenant":common::TENANT,"operationNamespace":common::NAMESPACE,"op":op,"operationId":text(&fixture,"operationId")?,"now":now,"debounce":3.0,"config":cfg});
        let object = request.as_object_mut().ok_or("request")?;
        for (old, new) in [
            ("path", "source"),
            ("destination", "destination"),
            ("version", "sourceVersion"),
            ("destinationVersion", "destinationVersion"),
            ("overwrite", "overwrite"),
        ] {
            if let Some(value) = body.get(old) {
                object.insert(new.into(), value.clone());
            }
        }
        if matches!(op, "preserve" | "trash" | "restore")
            || body.get("overwrite") == Some(&Value::Bool(true))
        {
            object.insert("capsuleId".into(), field(&fixture, "capsuleId")?.clone());
        }
        let request: Request = serde_json::from_value(request)?;
        let first = execute(&request)?;
        let expected = field(case, "expected")?;
        assert_eq!(
            u64::from(first.status()),
            field(expected, "status")?.as_u64().ok_or("status")?,
            "{case}"
        );
        if let Some(expected) = expected.get("body") {
            assert_eq!(&first.body().to_value()?, expected, "{case}");
        }
        assert_eq!(
            snapshot(&db)?,
            text(case, "expectedStateSHA256")?,
            "state {case}"
        );
        let replay = execute(&request)?;
        assert_eq!(replay, first, "retry {case}");
        assert_eq!(
            snapshot(&db)?,
            text(case, "expectedStateSHA256")?,
            "retry state {case}"
        );
        assert_eq!(
            db.query_row::<i64, _, _>(
                "SELECT count(*) FROM workspace_operation_receipts",
                [],
                |r| r.get(0)
            )?,
            if op == "trashList" { 0 } else { 1 },
            "receipts {case}"
        );
    }
    Ok(())
}

fn native_request(path: &str, op: &str, source: Option<&str>) -> TestResult<Request> {
    let mut value = json!({"version":1,"dbPath":path,"expectedTenant":common::TENANT,"operationNamespace":common::NAMESPACE,"operationId":common::OP_ID,"capsuleId":common::CAPSULE,"op":op,"now":2.0,"debounce":0.0,"config":common::config()});
    if let Some(source) = source {
        let object = value.as_object_mut().ok_or("request")?;
        object.insert("source".into(), json!(source));
        object.insert("sourceVersion".into(), json!(hash(b"synthetic note")));
    }
    Ok(serde_json::from_value(value)?)
}
#[test]
fn scoped_copy_preserves_unrelated_capsule_and_corrupt_body_rowids() -> TestResult<()> {
    let (_dir, path) = common::fixture()?;
    let db = Connection::open(&path)?;
    let sentinel = format!(".noevia-trash/{}.json", common::CAPSULE);
    db.execute(
        "INSERT INTO files VALUES(?,x'626164',?,1.0)",
        params![sentinel, "0".repeat(64)],
    )?;
    db.execute(
        "INSERT INTO files VALUES('Elsewhere/opaque.md',zeroblob(2000000),?,1.0)",
        ["0".repeat(64)],
    )?;
    let before = db.query_row::<(i64, Vec<u8>, String, f64), _, _>(
        "SELECT rowid,data,version,updated FROM files WHERE path=?",
        [&sentinel],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )?;
    let other_id: i64 = db.query_row(
        "SELECT rowid FROM files WHERE path='Elsewhere/opaque.md'",
        [],
        |r| r.get(0),
    )?;
    let mut request = native_request(&path, "copy", Some("Notes/one.md"))?;
    request.destination = Some("Notes/copied.md".into());
    assert_eq!(execute(&request)?.status(), 200);
    assert_eq!(
        db.query_row::<(i64, Vec<u8>, String, f64), _, _>(
            "SELECT rowid,data,version,updated FROM files WHERE path=?",
            [sentinel],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        )?,
        before
    );
    assert_eq!(
        db.query_row::<i64, _, _>(
            "SELECT rowid FROM files WHERE path='Elsewhere/opaque.md'",
            [],
            |r| r.get(0)
        )?,
        other_id
    );
    assert_eq!(
        db.query_row::<i64, _, _>("SELECT count(*) FROM workspace_index_outbox", [], |r| r
            .get(0))?,
        1
    );
    Ok(())
}
#[test]
fn restore_preserves_unknown_integer_fields_and_separates_capsule_and_receipt_identity(
) -> TestResult<()> {
    use diary_backup_format::{Limits, PythonJson};
    let (_dir, path) = common::fixture()?;
    let db = Connection::open(&path)?;
    let huge = "1234567890".repeat(10);
    let raw = format!(
        r#"{{"format":"noevia-trash-v1","id":"{}","path":"Notes/one.md","version":"{}","data":"c3ludGhldGljIG5vdGU=","state":"trashed","trashedAt":1.0,"extra":{huge}}}"#,
        common::CAPSULE,
        hash(b"synthetic note")
    );
    let key = format!(".noevia-trash/{}.json", common::CAPSULE);
    db.execute("DELETE FROM files WHERE path='Notes/one.md'", [])?;
    db.execute(
        "INSERT INTO files VALUES(?,?,?,1.0)",
        params![key, raw.as_bytes(), hash(raw.as_bytes())],
    )?;
    let request = native_request(&path, "restore", None)?;
    let first = execute(&request)?;
    assert_eq!(first.status(), 200);
    assert_eq!(
        first.body().to_value()?.get("alreadyApplied"),
        Some(&json!(false))
    );
    let encoded: Vec<u8> =
        db.query_row("SELECT data FROM files WHERE path=?", [key], |r| r.get(0))?;
    let parsed = PythonJson::parse(
        &encoded,
        Limits {
            manifest_bytes: 100000,
            files: 0,
            directories: 0,
            json_depth: 32,
            json_nodes: 1000,
            string_bytes: 100000,
            payload_bytes: 100000,
        },
    )
    .map_err(|_| "restored JSON")?;
    let PythonJson::Object(record) = parsed else {
        return Err("object".into());
    };
    assert_eq!(record.get("extra"), Some(&PythonJson::Integer(huge)));
    let mut fresh = native_request(&path, "restore", None)?;
    fresh.operation_id = Some("41234567-89ab-4cde-8fab-0123456789ab".into());
    assert_eq!(
        execute(&fresh)?.body().to_value()?.get("alreadyApplied"),
        Some(&json!(true))
    );
    assert_eq!(execute(&request)?, first);
    assert_eq!(
        db.query_row::<String, _, _>("SELECT value FROM meta WHERE key='generation'", [], |r| r
            .get(0))?,
        "1"
    );
    assert_eq!(
        db.query_row::<i64, _, _>(
            "SELECT count(*) FROM workspace_operation_receipts",
            [],
            |r| r.get(0)
        )?,
        2
    );
    Ok(())
}
#[test]
fn trash_pages_keep_invalid_capsules_visible_without_loading_oversized_payloads() -> TestResult<()>
{
    use diary_workspace_rules::{encode_writer, NewCapsule, TrashState};
    let (_dir, path) = common::fixture()?;
    let db = Connection::open(&path)?;
    for i in 0..101 {
        let id = format!("00000000-0000-4000-8000-{i:012x}");
        let raw = encode_writer(&NewCapsule {
            id: &id,
            path: "Notes/one.md",
            raw: b"synthetic note",
            state: TrashState::Trashed,
            trashed_at: 1.0,
            restored_at: None,
        })
        .map_err(|_| "capsule")?;
        db.execute(
            "INSERT INTO files VALUES(?,?,?,1.0)",
            params![format!(".noevia-trash/{id}.json"), raw, hash(&raw)],
        )?;
    }
    db.execute("UPDATE files SET data=x'626164' WHERE path='.noevia-trash/00000000-0000-4000-8000-000000000000.json'",[])?;
    db.execute("UPDATE files SET data=zeroblob(2000000) WHERE path='.noevia-trash/00000000-0000-4000-8000-000000000001.json'",[])?;
    let request = native_request(&path, "trashList", None)?;
    let first = execute(&request)?;
    let first_body = first.body().to_value()?;
    let rows = field(&first_body, "records")?.as_array().ok_or("records")?;
    assert_eq!(rows.len(), 100);
    assert_eq!(
        rows.iter()
            .filter(|r| r.get("invalid") == Some(&json!(true)))
            .count(),
        2
    );
    let mut next = native_request(&path, "trashList", None)?;
    next.cursor = Some(text(&first.body().to_value()?, "next")?.into());
    let last = execute(&next)?;
    assert_eq!(
        field(&last.body().to_value()?, "records")?
            .as_array()
            .ok_or("records")?
            .len(),
        1
    );
    assert_eq!(last.body().to_value()?.get("next"), Some(&Value::Null));
    assert_eq!(
        db.query_row::<i64, _, _>(
            "SELECT count(*) FROM workspace_operation_receipts",
            [],
            |r| r.get(0)
        )?,
        0
    );
    assert_eq!(
        db.query_row::<String, _, _>("SELECT value FROM meta WHERE key='generation'", [], |r| r
            .get(0))?,
        "0"
    );
    Ok(())
}

#[test]
fn late_capsule_collision_rolls_back_prior_capsule_before_cached_refusal() -> TestResult<()> {
    let (_dir, path) = common::fixture()?;
    let db = Connection::open(&path)?;
    for (key, data) in [
        ("Notes/Folder/a.md", b"a".as_slice()),
        ("Notes/Folder/b.md", b"b".as_slice()),
    ] {
        db.execute(
            "INSERT INTO files VALUES(?,?,?,1.0)",
            params![key, data, hash(data)],
        )?;
    }
    db.execute("INSERT INTO directories VALUES('Notes/Folder')", [])?;
    let fixture: Value = serde_json::from_str(include_str!("fixtures/mutation-contract.json"))?;
    let case = field(&fixture, "cases")?
        .as_array()
        .ok_or("cases")?
        .iter()
        .find(|c| {
            c.get("name").and_then(Value::as_str) == Some("delete-folder")
                && c.get("root").and_then(Value::as_str) == Some("")
        })
        .ok_or("delete case")?;
    let ids = field(field(field(case, "expected")?, "body")?, "trash")?
        .as_array()
        .ok_or("trash")?;
    let conflict = format!(
        ".noevia-trash/{}.json",
        ids.last().and_then(Value::as_str).ok_or("collision id")?
    );
    let prior = format!(
        ".noevia-trash/{}.json",
        ids.first().and_then(Value::as_str).ok_or("prior id")?
    );
    db.execute(
        "INSERT INTO files VALUES(?,?,?,1.0)",
        params![
            conflict,
            b"preserved collision".as_slice(),
            hash(b"preserved collision")
        ],
    )?;
    let version = text(
        &execute(&native_request(&path, "stat", Some("Notes/Folder"))?)?
            .body()
            .to_value()?,
        "version",
    )?
    .to_owned();
    let mut request = native_request(&path, "delete", Some("Notes/Folder"))?;
    request.source_version = diary_workspace_transaction::Condition::Value(version.clone());
    let before = snapshot(&db)?;
    let first = execute(&request)?;
    assert_eq!(first.status(), 409);
    assert_eq!(snapshot(&db)?, before);
    assert_eq!(
        db.query_row::<i64, _, _>("SELECT count(*) FROM files WHERE path=?", [prior], |r| r
            .get(0))?,
        0
    );
    db.execute("DELETE FROM files WHERE path=?", [conflict])?;
    let changed = snapshot(&db)?;
    assert_eq!(execute(&request)?, first);
    assert_eq!(snapshot(&db)?, changed);
    let mut fresh = native_request(&path, "delete", Some("Notes/Folder"))?;
    fresh.operation_id = Some("41234567-89ab-4cde-8fab-0123456789ab".into());
    fresh.source_version = diary_workspace_transaction::Condition::Value(version);
    assert_eq!(execute(&fresh)?.status(), 200);
    assert_eq!(
        db.query_row::<String, _, _>("SELECT value FROM meta WHERE key='generation'", [], |r| r
            .get(0))?,
        "1"
    );
    assert_eq!(
        db.query_row::<i64, _, _>(
            "SELECT count(*) FROM workspace_operation_receipts",
            [],
            |r| r.get(0)
        )?,
        2
    );
    Ok(())
}
#[test]
fn simultaneous_same_identity_commits_one_copy_and_one_receipt() -> TestResult<()> {
    let (_dir, path) = common::fixture()?;
    let raw = serde_json::to_string(
        &json!({"version":1,"dbPath":path,"expectedTenant":common::TENANT,"operationNamespace":common::NAMESPACE,"operationId":common::OP_ID,"op":"copy","source":"Notes/one.md","destination":"Notes/copied.md","now":2.0,"debounce":0.0,"config":common::config()}),
    )?;
    let mut handles = Vec::new();
    for _ in 0..2 {
        let raw = raw.clone();
        handles.push(std::thread::spawn(
            move || -> Result<diary_workspace_transaction::Reply, String> {
                let request: Request = serde_json::from_str(&raw).map_err(|e| e.to_string())?;
                execute(&request).map_err(|e| e.to_string())
            },
        ));
    }
    let mut results = Vec::new();
    for handle in handles {
        results.push(handle.join().map_err(|_| "thread panicked")??);
    }
    assert_eq!(results.first(), results.last());
    assert_eq!(results.first().ok_or("reply")?.status(), 200);
    let db = Connection::open(path)?;
    assert_eq!(
        db.query_row::<String, _, _>("SELECT value FROM meta WHERE key='generation'", [], |r| r
            .get(0))?,
        "1"
    );
    assert_eq!(
        db.query_row::<i64, _, _>("SELECT count(*) FROM files", [], |r| r.get(0))?,
        2
    );
    assert_eq!(
        db.query_row::<i64, _, _>(
            "SELECT count(*) FROM workspace_operation_receipts",
            [],
            |r| r.get(0)
        )?,
        1
    );
    Ok(())
}

#[test]
fn derived_folder_targets_never_commit_outside_the_admitted_key_domain() -> TestResult<()> {
    let mut accepted = Vec::new();
    for unicode in [false, true] {
        for op in ["copy", "move"] {
            let (_dir, path) = common::fixture()?;
            let db = Connection::open(&path)?;
            db.execute_batch("DELETE FROM files;DELETE FROM directories;")?;
            let root = if unicode {
                "\u{10000}".repeat(500)
            } else {
                String::new()
            };
            let scalar = if unicode { "\u{10000}" } else { "x" };
            let leaf = format!("{}.md", scalar.repeat(495));
            let destination = scalar.repeat(500);
            let source = diary_workspace_rules::join(&root, &[&format!("F/{leaf}")]);
            db.execute(
                "INSERT INTO files VALUES(?,?,?,1.0)",
                params![source, b"x".as_slice(), hash(b"x")],
            )?;
            db.execute(
                "INSERT INTO directories VALUES(?)",
                [diary_workspace_rules::join(&root, &["F"])],
            )?;
            let mut cfg = common::config();
            *cfg.get_mut("remoteRoot").ok_or("root")? = json!(root);
            let mut inspect = native_request(&path, "stat", Some("F"))?;
            inspect.config = Some(serde_json::from_value(cfg.clone())?);
            let tag = text(&execute(&inspect)?.body().to_value()?, "version")?.to_owned();
            let mut request = native_request(&path, op, Some("F"))?;
            request.config = Some(serde_json::from_value(cfg)?);
            request.destination = Some(destination);
            request.source_version = diary_workspace_transaction::Condition::Value(tag);
            let before = snapshot(&db)?;
            let result = execute(&request)?;
            if result.status() != 507 {
                accepted.push(format!("unicode={unicode}/{op}/status={}", result.status()));
            } else {
                assert_eq!(snapshot(&db)?, before);
                assert_eq!(
                    db.query_row::<i64, _, _>(
                        "SELECT count(*) FROM workspace_operation_receipts",
                        [],
                        |r| r.get(0)
                    )?,
                    1
                );
            }
        }
    }
    assert!(
        accepted.is_empty(),
        "derived targets escaped their read/client domain: {accepted:?}"
    );
    Ok(())
}
#[test]
fn oversized_lookahead_key_does_not_hide_the_valid_trash_page() -> TestResult<()> {
    use diary_workspace_rules::{encode_writer, NewCapsule, TrashState};
    let (_dir, path) = common::fixture()?;
    let db = Connection::open(&path)?;
    for i in 0..100 {
        let id = format!("00000000-0000-4000-8000-{i:012x}");
        let raw = encode_writer(&NewCapsule {
            id: &id,
            path: "Notes/one.md",
            raw: b"synthetic note",
            state: TrashState::Trashed,
            trashed_at: 1.0,
            restored_at: None,
        })
        .map_err(|_| "capsule")?;
        db.execute(
            "INSERT INTO files VALUES(?,?,?,1.0)",
            params![format!(".noevia-trash/{id}.json"), raw, hash(&raw)],
        )?;
    }
    let oversized = format!(".noevia-trash/{}.json", "z".repeat(10000));
    db.execute(
        "INSERT INTO files VALUES(?,x'626164',?,1.0)",
        params![oversized, "0".repeat(64)],
    )?;
    let request = native_request(&path, "trashList", None)?;
    let first = execute(&request)?;
    assert_eq!(
        first.status(),
        200,
        "lookahead must not be decoded as a returned row"
    );
    assert_eq!(
        field(&first.body().to_value()?, "records")?
            .as_array()
            .ok_or("records")?
            .len(),
        100
    );
    let mut next = native_request(&path, "trashList", None)?;
    next.cursor = Some(text(&first.body().to_value()?, "next")?.into());
    assert_eq!(
        execute(&next)?.status(),
        409,
        "included corrupt key remains visible on its own page"
    );
    assert_eq!(
        db.query_row::<i64, _, _>(
            "SELECT count(*) FROM workspace_operation_receipts",
            [],
            |r| r.get(0)
        )?,
        0
    );
    Ok(())
}
