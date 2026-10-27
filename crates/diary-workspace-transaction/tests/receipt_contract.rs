use diary_workspace_transaction::{execute, Condition, Fault, Request};
use rusqlite::{params, Connection};
use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::error::Error;
mod common;
type TestResult<T> = Result<T, Box<dyn Error>>;
#[derive(Serialize)]
struct Wire<'a> {
    status: u16,
    body: &'a Value,
}
fn hash(data: &[u8]) -> String {
    format!("{:x}", Sha256::digest(data))
}
fn request(path: &str, op: &str) -> TestResult<Request> {
    let mut value = json!({"version":1,"dbPath":path,"expectedTenant":common::TENANT,"operationNamespace":common::NAMESPACE,"operationId":common::OP_ID,"capsuleId":common::CAPSULE,"op":op,"now":2.0,"debounce":0.0,"config":common::config()});
    if op != "restore" {
        let object = value.as_object_mut().ok_or("request")?;
        object.insert("source".into(), json!("Notes/one.md"));
        object.insert("sourceVersion".into(), json!(hash(b"synthetic note")));
    }
    if matches!(op, "copy" | "move") {
        value
            .as_object_mut()
            .ok_or("request")?
            .insert("destination".into(), json!("Notes/two.md"));
    }
    Ok(serde_json::from_value(value)?)
}
#[test]
fn canonical_but_wrong_operation_receipts_are_refused_without_repair() -> TestResult<()> {
    let mut accepted = Vec::new();
    for op in ["delete", "copy", "move", "preserve", "trash", "restore"] {
        let (_dir, path) = common::fixture()?;
        let db = Connection::open(&path)?;
        if op == "restore" {
            let raw = diary_workspace_rules::encode_writer(&diary_workspace_rules::NewCapsule {
                id: common::CAPSULE,
                path: "Notes/one.md",
                raw: b"synthetic note",
                state: diary_workspace_rules::TrashState::Trashed,
                trashed_at: 1.0,
                restored_at: None,
            })
            .map_err(|_| "capsule")?;
            db.execute("DELETE FROM files WHERE path='Notes/one.md'", [])?;
            db.execute(
                "INSERT INTO files VALUES(?,?,?,1.0)",
                params![
                    format!(".noevia-trash/{}.json", common::CAPSULE),
                    raw,
                    hash(&raw)
                ],
            )?;
        }
        let mut req = request(&path, op)?;
        let first = execute(&req)?;
        assert_eq!(first.status(), 200);
        let original: Vec<u8> = db.query_row(
            "SELECT response FROM workspace_operation_receipts WHERE id=?",
            [common::OP_ID],
            |r| r.get(0),
        )?;
        let generation: String =
            db.query_row("SELECT value FROM meta WHERE key='generation'", [], |r| {
                r.get(0)
            })?;
        let mut extra = first.body().to_value()?.clone();
        extra
            .as_object_mut()
            .ok_or("body")?
            .insert("extra".into(), json!(true));
        let mut missing = first.body().to_value()?.clone();
        let key = missing
            .as_object()
            .and_then(|o| o.keys().next())
            .ok_or("key")?
            .clone();
        missing.as_object_mut().ok_or("body")?.remove(&key);
        let mut wrong = first.body().to_value()?.clone();
        let object = wrong.as_object_mut().ok_or("body")?;
        if op == "delete" {
            object.insert("deleted".into(), json!("Other/private.md"));
        } else if matches!(op, "copy" | "move" | "preserve") {
            object.insert("path".into(), json!("Other/private.md"));
        } else {
            object.insert("id".into(), json!("41234567-89ab-4cde-8fab-0123456789ab"));
        }
        let mut cases = vec![
            ("empty", 200, json!({})),
            ("missing", 200, missing),
            ("extra", 200, extra),
            ("identity", 200, wrong),
            (
                "wrong success status",
                201,
                first.body().to_value()?.clone(),
            ),
            ("status/error mismatch", 409, json!({"error":"missing"})),
            ("uncached infrastructure", 503, json!({"error":"database"})),
        ];
        let mut semantic = first.body().to_value()?.clone();
        let fields = semantic.as_object_mut().ok_or("body")?;
        match op {
            "delete" => {
                let id = fields
                    .get("trash")
                    .and_then(Value::as_array)
                    .and_then(|v| v.first())
                    .ok_or("id")?
                    .clone();
                fields.insert("trash".into(), json!([id.clone(), id]));
            }
            "copy" | "move" => {
                fields.insert("replaced".into(), json!(true));
            }
            "preserve" => {
                fields.insert(
                    "trash".into(),
                    json!("41234567-89ab-4cde-8fab-0123456789ab"),
                );
            }
            "trash" => {
                fields.insert("version".into(), json!("0".repeat(64)));
            }
            "restore" => {
                fields.insert("state".into(), json!("trashed"));
            }
            _ => return Err("operation".into()),
        }
        cases.push(("semantic binding", 200, semantic));
        for (name, status, body) in cases {
            let bytes = serde_json::to_vec(&Wire {
                status,
                body: &body,
            })?;
            db.execute(
                "UPDATE workspace_operation_receipts SET response=? WHERE id=?",
                params![bytes, common::OP_ID],
            )?;
            if execute(&req).err() != Some(Fault::Schema) {
                accepted.push(format!("{op}/{name}"));
            }
            assert_eq!(
                db.query_row::<Vec<u8>, _, _>(
                    "SELECT response FROM workspace_operation_receipts WHERE id=?",
                    [common::OP_ID],
                    |r| r.get(0)
                )?,
                bytes
            );
            assert_eq!(
                db.query_row::<String, _, _>(
                    "SELECT value FROM meta WHERE key='generation'",
                    [],
                    |r| r.get(0)
                )?,
                generation
            );
        }
        db.execute(
            "UPDATE workspace_operation_receipts SET response=? WHERE id=?",
            params![original, common::OP_ID],
        )?;
        // Replay cannot depend on current config/clock or source existence.
        req.now = None;
        req.debounce = None;
        req.config = None;
        assert_eq!(execute(&req)?, first);
    }
    assert!(
        accepted.is_empty(),
        "canonical invalid receipts were replayed: {accepted:?}"
    );
    Ok(())
}
#[test]
fn malformed_mutation_shape_refuses_before_opening_a_database() -> TestResult<()> {
    let dir = tempfile::tempdir()?;
    let path = dir
        .path()
        .join("must-not-exist.sqlite3")
        .to_string_lossy()
        .into_owned();
    let mut accepted = Vec::new();
    for op in ["delete", "copy", "move", "preserve", "trash"] {
        let mut req = request(&path, op)?;
        req.source = None;
        if execute(&req).err() != Some(Fault::Invalid) {
            accepted.push(format!("{op}/missing source"));
        }
    }
    for op in ["copy", "move"] {
        let mut req = request(&path, op)?;
        req.destination = None;
        if execute(&req).err() != Some(Fault::Invalid) {
            accepted.push(format!("{op}/missing destination"));
        }
    }
    for op in ["preserve", "trash", "restore"] {
        let mut req = request(&path, op)?;
        req.capsule_id = None;
        if execute(&req).err() != Some(Fault::Invalid) {
            accepted.push(format!("{op}/missing capsule"));
        }
    }
    let mut req = request(&path, "trash")?;
    req.source = Some("/Notes/one.md/".into());
    if execute(&req).err() != Some(Fault::Invalid) {
        accepted.push("trash/invalid raw path".into());
    }
    let mut req = request(&path, "copy")?;
    req.source_version = Condition::Value("x".repeat(diary_workspace_transaction::MAX_REQUEST + 1));
    if execute(&req).err() != Some(Fault::Frame) {
        accepted.push("overlarge direct API scalar".into());
    }
    assert!(!std::path::Path::new(&path).exists());
    assert!(
        accepted.is_empty(),
        "invalid shape was not rejected at admission: {accepted:?}"
    );
    Ok(())
}

#[test]
fn forged_replacement_requires_the_original_destination_precondition() -> TestResult<()> {
    for op in ["copy", "move"] {
        for condition in [
            Condition::Absent,
            Condition::Null,
            Condition::Value("wrong".into()),
        ] {
            let (_dir, path) = common::fixture()?;
            let db = Connection::open(&path)?;
            let mut req = request(&path, op)?;
            req.overwrite = Condition::Value(true);
            req.destination_version = condition;
            let first = execute(&req)?;
            assert_eq!(first.status(), 200);
            assert_eq!(
                first.body().to_value()?.get("replaced"),
                Some(&json!(false))
            );
            let mut body = first.body().to_value()?.clone();
            body.as_object_mut()
                .ok_or("body")?
                .insert("replaced".into(), json!(true));
            let bytes = serde_json::to_vec(&Wire {
                status: 200,
                body: &body,
            })?;
            db.execute(
                "UPDATE workspace_operation_receipts SET response=?",
                [&bytes],
            )?;
            assert_eq!(execute(&req).err(), Some(Fault::Schema));
            assert_eq!(
                db.query_row::<Vec<u8>, _, _>(
                    "SELECT response FROM workspace_operation_receipts",
                    [],
                    |r| r.get(0)
                )?,
                bytes
            );
            assert_eq!(
                db.query_row::<String, _, _>(
                    "SELECT value FROM meta WHERE key='generation'",
                    [],
                    |r| r.get(0)
                )?,
                "1"
            );
        }
    }
    Ok(())
}

#[test]
fn new_trash_timestamp_binds_stored_creation_not_retry_clock() -> TestResult<()> {
    let (_dir, path) = common::fixture()?;
    let db = Connection::open(&path)?;
    let mut req = request(&path, "trash")?;
    let first = execute(&req)?;
    assert_eq!(first.status(), 200);
    let original: Vec<u8> = db.query_row(
        "SELECT response FROM workspace_operation_receipts",
        [],
        |r| r.get(0),
    )?;
    for timestamp in [json!(3.0), json!(2), Value::Null, json!({"arbitrary":true})] {
        let mut body = first.body().to_value()?.clone();
        body.as_object_mut()
            .ok_or("body")?
            .insert("trashedAt".into(), timestamp);
        let bytes = serde_json::to_vec(&Wire {
            status: 200,
            body: &body,
        })?;
        db.execute(
            "UPDATE workspace_operation_receipts SET response=?",
            [&bytes],
        )?;
        assert_eq!(execute(&req).err(), Some(Fault::Schema));
        assert_eq!(
            db.query_row::<Vec<u8>, _, _>(
                "SELECT response FROM workspace_operation_receipts",
                [],
                |r| r.get(0)
            )?,
            bytes
        );
    }
    db.execute(
        "UPDATE workspace_operation_receipts SET response=?,created=3.0",
        [&original],
    )?;
    assert_eq!(execute(&req).err(), Some(Fault::Schema));
    db.execute("UPDATE workspace_operation_receipts SET created=2.0", [])?;
    req.now = Some(999.0);
    req.debounce = None;
    req.config = None;
    assert_eq!(execute(&req)?, first);
    Ok(())
}

#[test]
fn corrupted_receipt_id_never_turns_a_replay_into_a_new_operation() -> TestResult<()> {
    let mut accepted = Vec::new();
    for sql in [
        "UPDATE workspace_operation_receipts SET id=NULL",
        "UPDATE workspace_operation_receipts SET id=CAST(id AS BLOB)",
    ] {
        let (_dir, path) = common::fixture()?;
        let db = Connection::open(&path)?;
        let req = request(&path, "copy")?;
        assert_eq!(execute(&req)?.status(), 200);
        db.execute(sql, [])?;
        if execute(&req).err() != Some(Fault::Schema) {
            accepted.push(sql);
        } else {
            assert_eq!(
                db.query_row::<i64, _, _>(
                    "SELECT count(*) FROM workspace_operation_receipts",
                    [],
                    |r| r.get(0)
                )?,
                1
            );
            assert_eq!(
                db.query_row::<String, _, _>(
                    "SELECT value FROM meta WHERE key='generation'",
                    [],
                    |r| r.get(0)
                )?,
                "1"
            );
        }
    }
    assert!(
        accepted.is_empty(),
        "corrupt identity was treated as a missing receipt: {accepted:?}"
    );
    Ok(())
}

#[test]
fn admission_namespace_identity_is_checked_before_database_access() -> TestResult<()> {
    let (_dir, existing) = common::fixture()?;
    let absent = tempfile::tempdir()?
        .path()
        .join("must-not-exist.sqlite3")
        .to_string_lossy()
        .into_owned();
    let mut failures = Vec::new();
    for path in [&existing, &absent] {
        for namespace in [
            None,
            Some("not-a-uuid".into()),
            Some(common::NAMESPACE.to_uppercase()),
            Some(format!("{{{}}}", common::NAMESPACE)),
            Some("x".repeat(diary_workspace_transaction::MAX_REQUEST + 1)),
        ] {
            let mut req = request(path, "copy")?;
            let expected = if namespace
                .as_ref()
                .is_some_and(|v: &String| v.len() > diary_workspace_transaction::MAX_REQUEST)
            {
                Fault::Frame
            } else {
                Fault::Invalid
            };
            req.operation_namespace = namespace;
            if execute(&req).err() != Some(expected) {
                failures.push((path == &existing, expected));
            }
        }
    }
    assert!(!std::path::Path::new(&absent).exists());
    let db = Connection::open(&existing)?;
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
    let mut req = request(&existing, "copy")?;
    req.operation_namespace = Some("41234567-89ab-4cde-8fab-0123456789ab".into());
    assert_eq!(execute(&req).err(), Some(Fault::Conflict));
    let mut read = request(&existing, "copy")?;
    read.op = "stat".into();
    read.operation_namespace = None;
    assert_eq!(execute(&read)?.status(), 200);
    assert!(
        failures.is_empty(),
        "malformed namespace reached live DB boundary: {failures:?}"
    );
    Ok(())
}

#[test]
fn admission_cached_refusal_domain_matches_the_operation() -> TestResult<()> {
    let mut accepted = Vec::new();
    for op in ["trash", "restore"] {
        let (_dir, path) = common::fixture()?;
        let req = request(&path, op)?;
        let first = execute(&req)?;
        let db = Connection::open(&path)?;
        let original: Vec<u8> = db.query_row(
            "SELECT response FROM workspace_operation_receipts",
            [],
            |r| r.get(0),
        )?;
        let generation: String =
            db.query_row("SELECT value FROM meta WHERE key='generation'", [], |r| {
                r.get(0)
            })?;
        for (status, code) in [
            (403, "protected"),
            (404, "missing"),
            (412, "precondition"),
            (428, "required"),
        ] {
            let bytes = serde_json::to_vec(&Wire {
                status,
                body: &json!({"error":code}),
            })?;
            db.execute(
                "UPDATE workspace_operation_receipts SET response=?",
                [&bytes],
            )?;
            if execute(&req).err() != Some(Fault::Schema) {
                accepted.push((op, status));
            }
            assert_eq!(
                db.query_row::<Vec<u8>, _, _>(
                    "SELECT response FROM workspace_operation_receipts",
                    [],
                    |r| r.get(0)
                )?,
                bytes
            );
            assert_eq!(
                db.query_row::<String, _, _>(
                    "SELECT value FROM meta WHERE key='generation'",
                    [],
                    |r| r.get(0)
                )?,
                generation
            );
        }
        db.execute(
            "UPDATE workspace_operation_receipts SET response=?",
            [&original],
        )?;
        assert_eq!(execute(&req)?, first);
    }
    assert!(
        accepted.is_empty(),
        "impossible refusal replayed: {accepted:?}"
    );
    Ok(())
}
