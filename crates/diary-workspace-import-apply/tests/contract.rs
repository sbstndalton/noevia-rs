#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
use diary_workspace_import_apply::{
    apply_with_clock, read_request, Error, Request, MAGIC, MAX_METADATA,
};
use rusqlite::{params, Connection};
use sha2::{Digest, Sha256};
const TENANT: &str = "00000000-0000-0000-0000-000000000001";
fn database() -> (tempfile::TempDir, Request) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("managed-diary.db");
    let db = Connection::open(&path).unwrap();
    db.execute_batch(include_str!("managed-schema.sql"))
        .unwrap();
    for (key, value) in [("tenant", TENANT), ("active", "1"), ("generation", "1")] {
        db.execute("INSERT INTO meta VALUES (?,?)", [key, value])
            .unwrap();
    }
    let body = include_bytes!("workspace.zip");
    let destination = "Imports/Copy".to_string();
    let mut sha = Sha256::new();
    sha.update(destination.as_bytes());
    sha.update([0]);
    sha.update(body);
    (
        dir,
        Request {
            version: 1,
            db_path: path.to_string_lossy().into(),
            expected_tenant: TENANT.into(),
            destination,
            reviewed_fingerprint: format!("{:x}", sha.finalize()),
            debounce: 3.0,
        },
    )
}
#[test]
fn timestamps_and_due_match_actual_python_text_independently() {
    let rows: Vec<(String, String, String, String)> =
        serde_json::from_str(include_str!("clock-cases.json")).unwrap();
    for (now, debounce, expected_now, expected_due) in rows {
        let (_dir, mut request) = database();
        request.debounce = f64::from_bits(u64::from_str_radix(&debounce, 16).unwrap());
        let now = f64::from_bits(u64::from_str_radix(&now, 16).unwrap());
        apply_with_clock(&request, include_bytes!("workspace.zip"), || Ok(now)).unwrap();
        let db = Connection::open(&request.db_path).unwrap();
        for (key, expected) in [("snapshot_time", expected_now), ("due", expected_due)] {
            let (actual, kind): (String, String) = db
                .query_row(
                    "SELECT value,typeof(value) FROM meta WHERE key=?",
                    [key],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            assert_eq!(actual, expected);
            assert_eq!(kind, "text");
        }
        let (actual, kind): (f64, String) = db
            .query_row("SELECT updated,typeof(updated) FROM files", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(actual, now);
        assert_eq!(kind, "real");
    }
}
#[test]
fn receipt_avoids_clock_and_later_conflicts() {
    let (_dir, request) = database();
    assert!(
        !apply_with_clock(&request, include_bytes!("workspace.zip"), || Ok(
            1700000000.0
        ))
        .unwrap()
        .already_applied
    );
    assert!(
        apply_with_clock(&request, include_bytes!("workspace.zip"), || Err(
            Error::Clock
        ))
        .unwrap()
        .already_applied
    );
}
#[test]
fn frame_requires_version_and_eof_before_database() {
    let (_dir, request) = database();
    let raw = serde_json::json!({"version":request.version,"dbPath":request.db_path,"expectedTenant":request.expected_tenant,
 "destination":request.destination,"reviewedFingerprint":request.reviewed_fingerprint,"debounce":request.debounce});
    let raw = serde_json::to_vec(&raw).unwrap();
    let body = include_bytes!("workspace.zip");
    let mut bytes = MAGIC.to_vec();
    bytes.extend((raw.len() as u32).to_le_bytes());
    bytes.extend(raw);
    bytes.extend((body.len() as u64).to_le_bytes());
    bytes.extend(body);
    assert!(read_request(bytes.as_slice()).is_ok());
    bytes.push(0);
    assert!(matches!(read_request(bytes.as_slice()), Err(Error::Frame)));
    let mut header = MAGIC.to_vec();
    header.extend(((MAX_METADATA + 1) as u32).to_le_bytes());
    assert!(matches!(read_request(header.as_slice()), Err(Error::Frame)));
}
#[test]
fn schema_and_clock_failures_rollback_without_repair() {
    let (_dir, request) = database();
    assert_eq!(
        apply_with_clock(&request, include_bytes!("workspace.zip"), || Err(
            Error::Clock
        )),
        Err(Error::Clock)
    );
    let db = Connection::open(&request.db_path).unwrap();
    assert_eq!(
        db.query_row("SELECT count(*) FROM files", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
    db.execute(
        "INSERT INTO files VALUES (?,?,?,?)",
        params!["Imports/Copy", b"x", "digest", 1.0],
    )
    .unwrap();
    assert_eq!(
        apply_with_clock(&request, include_bytes!("workspace.zip"), || Ok(1.0)),
        Err(Error::Conflict)
    );
}

#[test]
fn rejected_python_generations_preserve_the_complete_transaction() {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("generation-cases.json")).unwrap();
    let unsupported = fixture["unsupported_zeros"].as_array().unwrap();
    assert_eq!(unsupported.len(), 8);
    let mut invalid: Vec<String> = unsupported
        .iter()
        .map(|code| {
            char::from_u32(code.as_u64().unwrap() as u32)
                .unwrap()
                .to_string()
        })
        .collect();
    invalid.extend((0x1c..=0x1f).map(|code| format!("{}12", char::from_u32(code).unwrap())));
    invalid.extend([format!("1{}", "0".repeat(4300)), "9".repeat(4300)]);
    for generation in invalid {
        let (_dir, request) = database();
        let db = Connection::open(&request.db_path).unwrap();
        db.execute_batch(
            "INSERT INTO files VALUES ('keep.md',X'6b656570','digest',1.0);
             INSERT INTO directories VALUES ('Keep');
             INSERT INTO workspace_index_outbox VALUES ('keep.md');
             INSERT INTO workspace_imports VALUES ('prior-receipt','Imports/Prior');
             INSERT INTO backups(destination) VALUES ('synthetic-backup');",
        )
        .unwrap();
        db.execute(
            "UPDATE meta SET value=? WHERE key='generation'",
            [&generation],
        )
        .unwrap();
        let snapshot = || {
            [
                "files",
                "directories",
                "workspace_imports",
                "workspace_index_outbox",
                "meta",
                "backups",
            ]
            .map(|table| {
                let mut statement = db
                    .prepare(&format!("SELECT * FROM {table} ORDER BY 1"))
                    .unwrap();
                let columns = statement.column_count();
                statement
                    .query_map([], |row| {
                        (0..columns)
                            .map(|column| row.get::<_, rusqlite::types::Value>(column))
                            .collect::<Result<Vec<_>, _>>()
                    })
                    .unwrap()
                    .collect::<Result<Vec<_>, _>>()
                    .unwrap()
            })
        };
        let before = snapshot();
        assert_eq!(
            apply_with_clock(&request, include_bytes!("workspace.zip"), || Ok(1000.0)),
            Err(Error::Generation),
            "{generation:?}"
        );
        assert_eq!(snapshot(), before, "{generation:?}");
    }
}

#[test]
fn native_lifecycle_guard_refuses_commit_and_receipt_replay() {
    let (_dir, request) = database();
    let calls = std::cell::Cell::new(0);
    let refused = diary_workspace_import_apply::apply_guarded(
        &request,
        include_bytes!("workspace.zip"),
        || Ok(1700000000.0),
        || {
            calls.set(calls.get() + 1);
            if calls.get() == 3 {
                Err(Error::Inactive)
            } else {
                Ok(())
            }
        },
    );
    assert_eq!(refused, Err(Error::Inactive));
    let db = Connection::open(&request.db_path).unwrap();
    assert_eq!(
        db.query_row("SELECT count(*) FROM files", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        db.query_row("SELECT count(*) FROM workspace_imports", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        db.query_row("SELECT value FROM meta WHERE key='generation'", [], |r| {
            r.get::<_, String>(0)
        })
        .unwrap(),
        "1"
    );
    apply_with_clock(&request, include_bytes!("workspace.zip"), || {
        Ok(1700000000.0)
    })
    .unwrap();
    calls.set(0);
    let refused = diary_workspace_import_apply::apply_guarded(
        &request,
        include_bytes!("workspace.zip"),
        || Err(Error::Clock),
        || {
            calls.set(calls.get() + 1);
            if calls.get() == 3 {
                Err(Error::Inactive)
            } else {
                Ok(())
            }
        },
    );
    assert_eq!(refused, Err(Error::Inactive));
    assert_eq!(
        db.query_row("SELECT count(*) FROM workspace_imports", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
}
