//! Corruption must be refused, not normalized into a successful mutation.
use diary_workspace_transaction::{execute, Condition, Fault, Request};
use rusqlite::{params, Connection};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::error::Error;
mod common;
type ResultOf<T> = Result<T, Box<dyn Error>>;
fn put(db: &Connection, path: &str, data: &[u8]) -> ResultOf<()> {
    db.execute(
        "INSERT INTO files VALUES(?,?,?,1.0)",
        params![path, data, diary_workspace_rules::sha256(data)],
    )?;
    Ok(())
}
fn snapshot(db: &Connection, receipts: bool) -> ResultOf<String> {
    let mut hash = Sha256::new();
    for (table, columns) in [
        ("files", vec!["path", "data", "version", "updated"]),
        ("directories", vec!["path"]),
        ("meta", vec!["key", "value"]),
        ("workspace_index_outbox", vec!["path"]),
        ("workspace_imports", vec!["fingerprint", "destination"]),
        (
            "backups",
            vec![
                "destination",
                "generation",
                "last_success",
                "retry_at",
                "failures",
                "error",
            ],
        ),
        (
            "workspace_operation_receipts",
            vec!["id", "fingerprint", "op", "response", "created"],
        ),
    ] {
        if !receipts && table == "workspace_operation_receipts" {
            continue;
        }
        hash.update(table.as_bytes());
        let expressions = columns
            .iter()
            .map(|c| format!("typeof({c})||':'||hex(CAST({c} AS BLOB))"))
            .collect::<Vec<_>>();
        let order = (1..=columns.len())
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let mut q = db.prepare(&format!(
            "SELECT {} FROM {table} ORDER BY {order}",
            expressions.join(",")
        ))?;
        let rows = q.query_map([], |r| {
            (0..columns.len())
                .map(|i| r.get::<_, String>(i))
                .collect::<Result<Vec<_>, _>>()
        })?;
        for row in rows {
            for value in row? {
                hash.update((value.len() as u64).to_le_bytes());
                hash.update(value.as_bytes());
            }
        }
    }
    Ok(format!("{:x}", hash.finalize()))
}
fn request(path: &str, op: &str) -> ResultOf<Request> {
    Ok(serde_json::from_value(
        json!({"version":1,"dbPath":path,"expectedTenant":common::TENANT,"operationNamespace":common::NAMESPACE,"operationId":common::OP_ID,"capsuleId":common::CAPSULE,"op":op,"source":"Notes/one.md","destination":"Notes/new.md","sourceVersion":diary_workspace_rules::sha256(b"synthetic note"),"config":common::config(),"now":2.0,"debounce":0.0}),
    )?)
}
#[test]
fn conflicting_hierarchies_cannot_hide_behind_an_exact_file_etag() -> ResultOf<()> {
    let mut unsafe_cases = Vec::new();
    for profile in ["directory", "child-file", "child-directory", "ancestor"] {
        for op in [
            "stat", "list", "copy", "move", "delete", "preserve", "trash",
        ] {
            let (_dir, path) = common::fixture()?;
            let db = Connection::open(&path)?;
            match profile {
                "directory" => {
                    db.execute("INSERT INTO directories VALUES('Notes/one.md')", [])?;
                }
                "child-file" => put(&db, "Notes/one.md/child.md", b"child")?,
                "child-directory" => {
                    db.execute("INSERT INTO directories VALUES('Notes/one.md/Child')", [])?;
                }
                "ancestor" => put(&db, "Notes", b"not a directory")?,
                _ => return Err("profile".into()),
            }
            let mut req = request(&path, op)?;
            if op == "list" {
                req.source = Some("Notes".into());
            }
            let before = snapshot(&db, true)?;
            let result = execute(&req);
            if result.err() != Some(Fault::Schema) || snapshot(&db, true)? != before {
                unsafe_cases.push((profile, op));
            }
        }
    }
    for profile in [
        "destination-directory",
        "destination-child",
        "ancestor-file",
        "grandparent-file",
    ] {
        for op in ["copy", "move"] {
            let (_dir, path) = common::fixture()?;
            let db = Connection::open(&path)?;
            db.execute("INSERT INTO directories VALUES('Target')", [])?;
            put(&db, "Target/new.md", b"old destination")?;
            match profile {
                "destination-directory" => {
                    db.execute("INSERT INTO directories VALUES('Target/new.md')", [])?;
                }
                "destination-child" => put(&db, "Target/new.md/child.md", b"child")?,
                "ancestor-file" => put(&db, "Target", b"not a directory")?,
                "grandparent-file" => {
                    put(&db, "Target", b"not a directory")?;
                    db.execute("INSERT INTO directories VALUES('Target/Sub')", [])?;
                }
                _ => return Err("profile".into()),
            }
            let mut req = request(&path, op)?;
            req.destination = Some(
                if profile == "grandparent-file" {
                    "Target/Sub/new.md"
                } else {
                    "Target/new.md"
                }
                .into(),
            );
            req.overwrite = Condition::Value(true);
            req.destination_version =
                Condition::Value(diary_workspace_rules::sha256(b"old destination"));
            let before = snapshot(&db, true)?;
            let result = execute(&req);
            if result.err() != Some(Fault::Schema) || snapshot(&db, true)? != before {
                unsafe_cases.push((profile, op));
            }
        }
    }
    assert!(
        unsafe_cases.is_empty(),
        "corrupt hierarchies admitted or changed: {unsafe_cases:?}"
    );
    Ok(())
}
fn capsule(db: &Connection, id: &str, path: &str) -> ResultOf<String> {
    let raw = diary_workspace_rules::encode_writer(&diary_workspace_rules::NewCapsule {
        id,
        path,
        raw: b"synthetic note",
        state: diary_workspace_rules::TrashState::Trashed,
        trashed_at: 1.0,
        restored_at: None,
    })
    .map_err(|_| "capsule")?;
    let key = format!(".noevia-trash/{id}.json");
    put(db, &key, &raw)?;
    Ok(key)
}
#[test]
fn outer_capsule_corruption_is_visible_without_recovery_or_repair() -> ResultOf<()> {
    let mut unsafe_cases = Vec::new();
    for profile in [
        "path-tamper",
        "version-mismatch",
        "version-blob",
        "version-nul",
        "version-size",
        "time-text",
        "time-infinite",
        "directory",
        "child-file",
        "child-directory",
        "ancestor",
    ] {
        for op in ["trashList", "trash", "restore"] {
            let (_dir, path) = common::fixture()?;
            let db = Connection::open(&path)?;
            db.execute("DELETE FROM files WHERE path='Notes/one.md'", [])?;
            let key = capsule(&db, common::CAPSULE, "Notes/one.md")?;
            let neighbor = "41234567-89ab-4cde-8fab-0123456789ab";
            capsule(&db, neighbor, "Notes/neighbor.md")?;
            match profile {
                "path-tamper" => {
                    let raw: Vec<u8> =
                        db.query_row("SELECT data FROM files WHERE path=?", [&key], |r| r.get(0))?;
                    let mut record: serde_json::Value = serde_json::from_slice(&raw)?;
                    record
                        .as_object_mut()
                        .ok_or("record")?
                        .insert("path".into(), json!("Notes/changed.md"));
                    db.execute(
                        "UPDATE files SET data=? WHERE path=?",
                        params![serde_json::to_vec(&record)?, key],
                    )?;
                }
                "version-mismatch" => {
                    db.execute(
                        "UPDATE files SET version=? WHERE path=?",
                        params!["0".repeat(64), key],
                    )?;
                }
                "version-blob" => {
                    db.execute(
                        "UPDATE files SET version=? WHERE path=?",
                        params![b"bad".as_slice(), key],
                    )?;
                }
                "version-nul" => {
                    db.execute(
                        "UPDATE files SET version=? WHERE path=?",
                        params![format!("0\0{}", "x".repeat(100_000)), key],
                    )?;
                }
                "version-size" => {
                    db.execute(
                        "UPDATE files SET version=? WHERE path=?",
                        params!["0".repeat(2 * 1024 * 1024), key],
                    )?;
                }
                "time-text" => {
                    db.execute("UPDATE files SET updated='bad' WHERE path=?", [&key])?;
                }
                "time-infinite" => {
                    db.execute(
                        "UPDATE files SET updated=? WHERE path=?",
                        params![f64::INFINITY, key],
                    )?;
                }
                "directory" => {
                    db.execute("INSERT INTO directories VALUES(?)", [&key])?;
                }
                "child-file" => put(&db, &format!("{key}/child.md"), b"child")?,
                "child-directory" => {
                    db.execute(
                        "INSERT INTO directories VALUES(?)",
                        [format!("{key}/Child")],
                    )?;
                }
                "ancestor" => put(&db, ".noevia-trash", b"bad namespace")?,
                _ => return Err("profile".into()),
            }
            let before = snapshot(&db, false)?;
            let req = request(&path, op)?;
            let result = execute(&req)?;
            let rejected = if op == "trashList" && profile == "child-file" {
                // The extra file is itself a malformed capsule filename; the
                // established included-bad-key policy is a page refusal.
                result.status() == 409
            } else if op == "trashList" {
                let body = result.body().to_value()?;
                let rows = body
                    .get("records")
                    .and_then(serde_json::Value::as_array)
                    .ok_or_else(|| {
                        format!(
                            "records missing for {profile}/{op}: status={}, body={body:?}",
                            result.status()
                        )
                    })?;
                let target = rows
                    .iter()
                    .find(|r| {
                        r.get("id").and_then(serde_json::Value::as_str) == Some(common::CAPSULE)
                    })
                    .ok_or("target")?;
                let healthy = rows
                    .iter()
                    .find(|r| r.get("id").and_then(serde_json::Value::as_str) == Some(neighbor))
                    .ok_or("neighbor")?;
                target.get("invalid") == Some(&json!(true))
                    && (profile == "ancestor" || healthy.get("invalid").is_none())
            } else {
                result.status() == 409
            };
            if !rejected || snapshot(&db, false)? != before {
                unsafe_cases.push((profile, op));
            }
            let count: i64 = db.query_row(
                "SELECT count(*) FROM workspace_operation_receipts",
                [],
                |r| r.get(0),
            )?;
            if op != "trashList" && rejected {
                assert_eq!(count, 1);
                assert_eq!(execute(&req)?, result);
            } else if op == "trashList" {
                assert_eq!(count, 0);
            }
        }
    }
    assert!(
        unsafe_cases.is_empty(),
        "unverified capsule trusted or repaired: {unsafe_cases:?}"
    );
    Ok(())
}
#[test]
fn unrelated_corruption_and_historical_receipts_keep_their_scope() -> ResultOf<()> {
    let (_dir, path) = common::fixture()?;
    let db = Connection::open(&path)?;
    put(&db, "Other/bad.md", b"unrelated")?;
    put(&db, "Other/bad.md/child.md", b"unrelated child")?;
    let mut req = request(&path, "copy")?;
    let first = execute(&req)?;
    assert_eq!(first.status(), 200);
    put(&db, "Notes/one.md/child.md", b"now corrupt live source")?;
    req.now = None;
    req.debounce = None;
    req.config = None;
    let before = snapshot(&db, true)?;
    assert_eq!(execute(&req)?, first);
    assert_eq!(snapshot(&db, true)?, before);
    let (_dir, path) = common::fixture()?;
    let db = Connection::open(&path)?;
    db.execute("DELETE FROM files WHERE path='Notes/one.md'", [])?;
    let key = capsule(&db, common::CAPSULE, "Notes/one.md")?;
    let mut req = request(&path, "restore")?;
    let first = execute(&req)?;
    assert_eq!(first.status(), 200);
    db.execute(
        "UPDATE files SET version=? WHERE path=?",
        params!["0".repeat(64), key],
    )?;
    req.now = None;
    req.debounce = None;
    req.config = None;
    let before = snapshot(&db, true)?;
    assert_eq!(execute(&req)?, first);
    assert_eq!(snapshot(&db, true)?, before);
    Ok(())
}

#[test]
fn missing_targets_below_valid_files_keep_normal_policy() -> ResultOf<()> {
    let (_dir, path) = common::fixture()?;
    let db = Connection::open(&path)?;
    let mut req = request(&path, "stat")?;
    req.source = Some("Notes/one.md/missing.md".into());
    let before = snapshot(&db, true)?;
    assert_eq!(execute(&req)?.status(), 404);
    assert_eq!(snapshot(&db, true)?, before);
    put(&db, "Target.md", b"valid file parent")?;
    let data_before = snapshot(&db, false)?;
    req = request(&path, "copy")?;
    req.destination = Some("Target.md/missing.md".into());
    assert_eq!(execute(&req)?.status(), 409);
    assert_eq!(snapshot(&db, false)?, data_before);
    Ok(())
}
