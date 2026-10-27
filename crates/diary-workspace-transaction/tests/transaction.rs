use diary_workspace_transaction::{execute, read_request, Fault, Request};
use rusqlite::Connection;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::io::Cursor;

mod common;
use common::{config, fixture, CAPSULE, NAMESPACE, OP_ID, TENANT};

fn request(path: String, source: &str) -> Result<Request, Box<dyn std::error::Error>> {
    Ok(serde_json::from_value(json!({
        "version":1,"dbPath":path,"expectedTenant":TENANT,
        "operationNamespace":NAMESPACE,"op":"delete","operationId":OP_ID,
        "source":source,"sourceVersion":format!("{:x}",Sha256::digest(b"synthetic note")),
        "capsuleId":CAPSULE,"config":config(),"now":1792760000.0,"debounce":3.0
    }))?)
}

#[test]
fn operation_id_is_required_before_opening_database() -> Result<(), Box<dyn std::error::Error>> {
    let bad = Request {
        version: 1,
        db_path: "/this/path/must/not/be/opened.sqlite".into(),
        expected_tenant: TENANT.into(),
        operation_namespace: None,
        op: "delete".into(),
        operation_id: None,
        capsule_id: None,
        source: Some("Notes/one.md".into()),
        destination: None,
        source_version: diary_workspace_transaction::Condition::Absent,
        destination_version: diary_workspace_transaction::Condition::Absent,
        overwrite: diary_workspace_transaction::Condition::Absent,
        now: Some(0.0),
        debounce: Some(0.0),
        config: Some(serde_json::from_value(config())?),
        limit: None,
        cursor: None,
    };
    assert_eq!(execute(&bad).err(), Some(Fault::Invalid));
    Ok(())
}

#[test]
fn exact_retry_replays_response_and_does_not_increment_generation_twice(
) -> Result<(), Box<dyn std::error::Error>> {
    let (_dir, path) = fixture()?;
    let req = request(path.clone(), "Notes/one.md")?;
    let first = execute(&req)?;
    assert_eq!(first.status(), 200);
    let retry = execute(&req)?;
    assert_eq!(retry, first);
    let changed = request(path.clone(), "Notes/two.md")?;
    let conflict = execute(&changed)?;
    assert_eq!(conflict.status(), 409);
    let db = Connection::open(path)?;
    let generation: String =
        db.query_row("SELECT value FROM meta WHERE key='generation'", [], |row| {
            row.get(0)
        })?;
    let receipts: i64 = db.query_row(
        "SELECT COUNT(*) FROM workspace_operation_receipts",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(generation, "1");
    assert_eq!(receipts, 1);
    Ok(())
}

#[test]
fn request_frame_rejects_duplicate_keys_and_trailing_bytes(
) -> Result<(), Box<dyn std::error::Error>> {
    let payload = br#"{"version":1,"version":1,"dbPath":"/tmp/x","expectedTenant":"01234567-89ab-4cde-8fab-0123456789ab","op":"stat"}"#;
    let mut frame = b"DWT1".to_vec();
    frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    frame.extend_from_slice(payload);
    assert_eq!(read_request(Cursor::new(frame)).err(), Some(Fault::Frame));

    let payload = br#"{"version":1,"dbPath":"/tmp/x","expectedTenant":"01234567-89ab-4cde-8fab-0123456789ab","op":"stat"}"#;
    let mut frame = b"DWT1".to_vec();
    frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    frame.extend_from_slice(payload);
    frame.push(0);
    assert_eq!(read_request(Cursor::new(frame)).err(), Some(Fault::Frame));
    Ok(())
}

fn framed(reply: &diary_workspace_transaction::Reply) -> Result<Vec<u8>, Fault> {
    let mut bytes = Vec::new();
    diary_workspace_transaction::write_reply(&mut bytes, reply)?;
    Ok(bytes)
}

#[test]
fn retry_identity_normalizes_paths_and_ignores_live_clock_config(
) -> Result<(), Box<dyn std::error::Error>> {
    let (_dir, path) = fixture()?;
    let req = request(path, "Notes/one.md")?;
    let original = execute(&req)?;
    assert_eq!(original.status(), 200);
    let original_bytes = framed(&original)?;
    let mut retry = req;
    retry.config = None;
    retry.now = None;
    retry.debounce = None;
    assert_eq!(framed(&execute(&retry)?)?, original_bytes);
    retry.source = Some("///Notes/one.md///".into());
    assert_eq!(framed(&execute(&retry)?)?, original_bytes);
    Ok(())
}

#[test]
fn receipt_identity_distinguishes_omitted_and_null_conditions(
) -> Result<(), Box<dyn std::error::Error>> {
    let (_dir, path) = fixture()?;
    let raw = json!({
        "version":1,"dbPath":path,"expectedTenant":TENANT,"operationNamespace":NAMESPACE,
        "op":"copy","operationId":OP_ID,"capsuleId":CAPSULE,
        "source":"Notes/one.md","destination":"Notes/two.md", "config":config(),"now":1.0,"debounce":0.0
    });
    let first: Request = serde_json::from_value(raw.clone())?;
    assert_eq!(execute(&first)?.status(), 200);
    let mut changed = raw;
    let object = changed.as_object_mut().ok_or("missing object")?;
    object.insert("sourceVersion".into(), serde_json::Value::Null);
    let retry: Request = serde_json::from_value(changed)?;
    assert_eq!(execute(&retry)?.status(), 409);
    Ok(())
}

#[test]
fn corrupted_receipt_operation_is_not_replayed() -> Result<(), Box<dyn std::error::Error>> {
    let (_dir, path) = fixture()?;
    let req = request(path.clone(), "Notes/one.md")?;
    assert_eq!(execute(&req)?.status(), 200);
    let db = Connection::open(path)?;
    db.execute("UPDATE workspace_operation_receipts SET op='copy'", [])?;
    drop(db);
    assert_eq!(execute(&req).err(), Some(Fault::Schema));
    Ok(())
}

#[test]
fn cli_reports_failed_stdout_instead_of_success() -> Result<(), Box<dyn std::error::Error>> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let (_dir, path) = fixture()?;
    let mut cfg = config();
    *cfg.get_mut("remoteRoot").ok_or("missing remoteRoot")? = json!("");
    let payload = serde_json::to_vec(
        &json!({"version":1,"dbPath":path,"expectedTenant":TENANT,"op":"stat","source":"Notes/one.md","config":cfg}),
    )?;
    let mut frame = b"DWT1".to_vec();
    frame.extend_from_slice(&u32::try_from(payload.len())?.to_le_bytes());
    frame.extend_from_slice(&payload);
    let mut child = Command::new(env!("CARGO_BIN_EXE_diary-workspace-transaction"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    drop(child.stdout.take().ok_or("missing child stdout")?);
    let sent = (|| -> Result<(), Box<dyn std::error::Error>> {
        let mut stdin = child.stdin.take().ok_or("missing child stdin")?;
        stdin.write_all(&frame)?;
        Ok(())
    })();
    let output = child.wait_with_output()?;
    sent?;
    assert_eq!(output.status.code(), Some(3));
    assert_eq!(output.stderr, b"workspace transaction reply unavailable\n");
    Ok(())
}

#[test]
fn explicit_null_copy_precondition_is_terminal_and_preserved(
) -> Result<(), Box<dyn std::error::Error>> {
    let (_dir, path) = fixture()?;
    let raw = json!({"version":1,"dbPath":path,"expectedTenant":TENANT,"operationNamespace":NAMESPACE,
        "op":"copy","operationId":OP_ID,"source":"Notes/one.md","destination":"Notes/two.md",
        "sourceVersion":null,"config":config(),"now":1.0,"debounce":0.0});
    let req: Request = serde_json::from_value(raw.clone())?;
    let refused = execute(&req)?;
    assert_eq!(refused.status(), 428);
    let mut retry = req;
    retry.config = None;
    retry.now = None;
    assert_eq!(framed(&execute(&retry)?)?, framed(&refused)?);
    let db = Connection::open(&path)?;
    assert_eq!(
        db.query_row::<i64, _, _>("SELECT COUNT(*) FROM files", [], |r| r.get(0))?,
        1
    );
    assert_eq!(
        db.query_row::<String, _, _>("SELECT value FROM meta WHERE key='generation'", [], |r| r
            .get(0))?,
        "0"
    );
    drop(db);
    let mut without = raw;
    let obj = without.as_object_mut().ok_or("missing object")?;
    obj.remove("sourceVersion");
    obj.insert(
        "operationId".into(),
        json!("41234567-89ab-4cde-8fab-0123456789ab"),
    );
    let new_attempt: Request = serde_json::from_value(without)?;
    assert_eq!(execute(&new_attempt)?.status(), 200);
    Ok(())
}

#[test]
fn noncanonical_or_duplicate_receipts_refuse_without_repair(
) -> Result<(), Box<dyn std::error::Error>> {
    let (_dir, path) = fixture()?;
    let req = request(path.clone(), "Notes/one.md")?;
    let first = execute(&req)?;
    let original_frame = framed(&first)?;
    let db = Connection::open(&path)?;
    let original: Vec<u8> = db.query_row(
        "SELECT response FROM workspace_operation_receipts",
        [],
        |r| r.get(0),
    )?;
    let mut spaced = vec![b' '];
    spaced.extend_from_slice(&original);
    for bad in [
        spaced,
        br#"{"status":200,"status":200,"body":{}}"#.to_vec(),
        br#"{"status":200,"body":{"x":1,"x":2}}"#.to_vec(),
        br#"{"status":600,"body":{}}"#.to_vec(),
        br#"{"status":200,"body":{},"unknown":true}"#.to_vec(),
        br#"{"status":200,"body":[]}"#.to_vec(),
    ] {
        db.execute("UPDATE workspace_operation_receipts SET response=?", [&bad])?;
        assert_eq!(execute(&req).err(), Some(Fault::Schema));
        assert_eq!(
            db.query_row::<Vec<u8>, _, _>(
                "SELECT response FROM workspace_operation_receipts",
                [],
                |r| r.get(0)
            )?,
            bad
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
    db.execute(
        "UPDATE workspace_operation_receipts SET response=?",
        [&original],
    )?;
    assert_eq!(framed(&execute(&req)?)?, original_frame);
    Ok(())
}

#[test]
fn lost_mutation_stdout_is_resolved_by_the_same_persisted_identity(
) -> Result<(), Box<dyn std::error::Error>> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let (_dir, path) = fixture()?;
    let raw = json!({"version":1,"dbPath":path,"expectedTenant":TENANT,"operationNamespace":NAMESPACE,
        "op":"delete","operationId":OP_ID,"source":"Notes/one.md","capsuleId":CAPSULE,
        "sourceVersion":format!("{:x}",Sha256::digest(b"synthetic note")),"config":config(),"now":1.0,"debounce":0.0});
    let payload = serde_json::to_vec(&raw)?;
    let mut frame = b"DWT1".to_vec();
    frame.extend_from_slice(&u32::try_from(payload.len())?.to_le_bytes());
    frame.extend_from_slice(&payload);
    let mut child = Command::new(env!("CARGO_BIN_EXE_diary-workspace-transaction"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    drop(child.stdout.take().ok_or("missing stdout")?);
    let sent = (|| -> Result<(), Box<dyn std::error::Error>> {
        let mut stdin = child.stdin.take().ok_or("missing stdin")?;
        stdin.write_all(&frame)?;
        Ok(())
    })();
    let failed = child.wait_with_output()?;
    sent?;
    assert_eq!(failed.status.code(), Some(3));
    let db = Connection::open(&path)?;
    let recorded: Vec<u8> = db.query_row(
        "SELECT response FROM workspace_operation_receipts WHERE id=?",
        [OP_ID],
        |r| r.get(0),
    )?;
    assert_eq!(
        db.query_row::<String, _, _>("SELECT value FROM meta WHERE key='generation'", [], |r| r
            .get(0))?,
        "1"
    );
    drop(db);
    let mut retry = Command::new(env!("CARGO_BIN_EXE_diary-workspace-transaction"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let sent = (|| -> Result<(), Box<dyn std::error::Error>> {
        let mut stdin = retry.stdin.take().ok_or("missing stdin")?;
        stdin.write_all(&frame)?;
        Ok(())
    })();
    let output = retry.wait_with_output()?;
    sent?;
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let mut expected = b"DWR1".to_vec();
    expected.extend_from_slice(&u32::try_from(recorded.len())?.to_le_bytes());
    expected.extend_from_slice(&recorded);
    expected.extend_from_slice(b"END1");
    assert_eq!(output.stdout, expected);
    let db = Connection::open(&path)?;
    assert_eq!(
        db.query_row::<i64, _, _>(
            "SELECT COUNT(*) FROM workspace_operation_receipts",
            [],
            |r| r.get(0)
        )?,
        1
    );
    assert_eq!(
        db.query_row::<String, _, _>("SELECT value FROM meta WHERE key='generation'", [], |r| r
            .get(0))?,
        "1"
    );
    Ok(())
}

#[test]
fn invalid_receipt_metadata_refuses_without_loading_or_repairing_payload(
) -> Result<(), Box<dyn std::error::Error>> {
    let (_dir, path) = fixture()?;
    let req = request(path.clone(), "Notes/one.md")?;
    assert_eq!(execute(&req)?.status(), 200);
    let db = Connection::open(&path)?;
    db.execute(
        "UPDATE workspace_operation_receipts SET response=zeroblob(?)",
        [i64::try_from(
            diary_workspace_transaction::MAX_RESPONSE + 1,
        )?],
    )?;
    assert_eq!(execute(&req).err(), Some(Fault::Schema));
    assert_eq!(
        db.query_row::<i64, _, _>(
            "SELECT length(response) FROM workspace_operation_receipts",
            [],
            |r| r.get(0)
        )?,
        i64::try_from(diary_workspace_transaction::MAX_RESPONSE + 1)?
    );
    db.execute(
        "UPDATE workspace_operation_receipts SET response=x'00',fingerprint=printf('%0100000d',0)",
        [],
    )?;
    assert_eq!(execute(&req).err(), Some(Fault::Schema));
    assert_eq!(
        db.query_row::<i64, _, _>(
            "SELECT length(fingerprint) FROM workspace_operation_receipts",
            [],
            |r| r.get(0)
        )?,
        100000
    );
    assert_eq!(
        db.query_row::<String, _, _>("SELECT value FROM meta WHERE key='generation'", [], |r| r
            .get(0))?,
        "1"
    );
    Ok(())
}

#[test]
fn altered_schema_refuses_without_repair() -> Result<(), Box<dyn std::error::Error>> {
    let original = common::base_schema()?;
    let cases=[
        ("data type",original.replace("data BLOB NOT NULL","data TEXT NOT NULL")),
        ("nullable version",original.replace("version TEXT NOT NULL","version TEXT")),
        ("missing files primary key",original.replace("files(path TEXT PRIMARY KEY","files(path TEXT")),
        ("nocase path",original.replace("files(path TEXT PRIMARY KEY","files(path TEXT COLLATE NOCASE PRIMARY KEY")),
        ("extra uniqueness",original.replace("version TEXT NOT NULL","version TEXT NOT NULL UNIQUE")),
        ("check constraint",original.replace("updated REAL NOT NULL)","updated REAL NOT NULL CHECK(updated>=0))")),
        ("receipt blob type",original.replace("response BLOB NOT NULL","response TEXT NOT NULL")),
        ("receipt nullable fingerprint",original.replace("fingerprint TEXT NOT NULL,op","fingerprint TEXT,op")),
        ("receipt primary key",original.replace("receipts(id TEXT PRIMARY KEY","receipts(id TEXT")),
        ("meta primary key",original.replace("meta(key TEXT PRIMARY KEY","meta(key TEXT")),
        ("trigger",format!("{original} CREATE TRIGGER side_effect AFTER DELETE ON files BEGIN UPDATE meta SET value='999' WHERE key='generation'; END;")),
        ("foreign side table",format!("{original} CREATE TABLE side(path TEXT REFERENCES files(path) ON DELETE CASCADE);")),
    ];
    let mut accepted = Vec::new();
    for (name, sql) in cases {
        assert_ne!(sql, original, "test mutation {name} must change the DDL");
        let (_dir, path) = common::fixture_with_schema(&sql)?;
        let req: Request = serde_json::from_value(
            json!({"version":1,"dbPath":path,"expectedTenant":TENANT,"op":"stat","source":"Notes/one.md"}),
        )?;
        if execute(&req).err() != Some(Fault::Schema) {
            accepted.push(name);
        }
        let db = Connection::open(&path)?;
        assert_eq!(
            db.query_row::<String, _, _>(
                "SELECT value FROM meta WHERE key='generation'",
                [],
                |r| r.get(0)
            )?,
            "0"
        );
        assert_eq!(
            db.query_row::<i64, _, _>("SELECT COUNT(*) FROM files", [], |r| r.get(0))?,
            1
        );
    }
    assert!(
        accepted.is_empty(),
        "unsupported schemas accepted: {accepted:?}"
    );
    Ok(())
}

#[test]
fn actual_schema_and_equivalent_quoted_definitions_are_supported(
) -> Result<(), Box<dyn std::error::Error>> {
    let original = common::base_schema()?;
    let file_ddl="files(path TEXT PRIMARY KEY, data BLOB NOT NULL, version TEXT NOT NULL, updated REAL NOT NULL)";
    assert!(original.contains(file_ddl));
    let compatible=original.replace(file_ddl,"\"FILES\"(\"PATH\" text, /* CHECK or DEFAULT here is only a comment. */ \"data\" blob not null, [version] text collate binary not null, `updated` real not null, primary key(\"PATH\" collate binary DESC))")
        .replace("DEFAULT -1","DEFAULT (-1)").replace("DEFAULT 0","DEFAULT '0'");
    for sql in [&original, &compatible] {
        let (_dir, path) = common::fixture_with_schema(sql)?;
        let db = Connection::open(&path)?;
        db.execute("INSERT INTO backups(destination,generation,last_success,retry_at,failures,error) VALUES('synthetic-target',7,1,2,3,'retain')",[])?;
        drop(db);
        let req = request(path.clone(), "Notes/one.md")?;
        assert_eq!(execute(&req)?.status(), 200);
        let db = Connection::open(&path)?;
        let preserved:(i64,f64,f64,i64,String)=db.query_row("SELECT generation,last_success,retry_at,failures,error FROM backups WHERE destination='synthetic-target'",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)))?;
        assert_eq!(preserved, (7, 1.0, 2.0, 3, "retain".into()));
    }
    Ok(())
}

#[test]
fn schema_validation_precedes_valid_receipt_disclosure() -> Result<(), Box<dyn std::error::Error>> {
    let (_dir, path) = fixture()?;
    let req = request(path.clone(), "Notes/one.md")?;
    let initial = framed(&execute(&req)?)?;
    let db = Connection::open(&path)?;
    let recorded: Vec<u8> = db.query_row(
        "SELECT response FROM workspace_operation_receipts",
        [],
        |r| r.get(0),
    )?;
    db.execute_batch("CREATE TRIGGER surprise AFTER INSERT ON files BEGIN UPDATE meta SET value='bad' WHERE key='generation'; END;")?;
    assert_eq!(execute(&req).err(), Some(Fault::Schema));
    assert_eq!(
        db.query_row::<i64, _, _>(
            "SELECT COUNT(*) FROM sqlite_schema WHERE name='surprise'",
            [],
            |r| r.get(0)
        )?,
        1
    );
    assert_eq!(
        db.query_row::<Vec<u8>, _, _>(
            "SELECT response FROM workspace_operation_receipts",
            [],
            |r| r.get(0)
        )?,
        recorded
    );
    assert_eq!(
        db.query_row::<String, _, _>("SELECT value FROM meta WHERE key='generation'", [], |r| r
            .get(0))?,
        "1"
    );
    // Only this fixture's explicit owner removes the test corruption; the engine did not.
    db.execute_batch("DROP TRIGGER surprise;")?;
    assert_eq!(framed(&execute(&req)?)?, initial);
    Ok(())
}

#[test]
fn missing_tables_and_invalid_meta_refuse_without_bootstrap(
) -> Result<(), Box<dyn std::error::Error>> {
    for statement in [
        "DROP TABLE backups",
        "DROP TABLE workspace_operation_receipts",
        "INSERT INTO meta(key,value) VALUES(NULL,'unexpected')",
        "UPDATE meta SET value=printf('%0100000d',0) WHERE key='tenant'",
    ] {
        let (_dir, path) = fixture()?;
        let db = Connection::open(&path)?;
        db.execute_batch(statement)?;
        let before: i64 = db.query_row("SELECT COUNT(*) FROM sqlite_schema", [], |r| r.get(0))?;
        let req = request(path.clone(), "Notes/one.md")?;
        assert_eq!(execute(&req).err(), Some(Fault::Schema), "{statement}");
        assert_eq!(
            db.query_row::<i64, _, _>("SELECT COUNT(*) FROM sqlite_schema", [], |r| r.get(0))?,
            before
        );
        assert_eq!(
            db.query_row::<i64, _, _>(
                "SELECT COUNT(*) FROM files WHERE path='Notes/one.md'",
                [],
                |r| r.get(0)
            )?,
            1
        );
    }
    let dir = tempfile::tempdir()?;
    let missing = dir.path().join("missing.sqlite");
    let req = request(missing.to_string_lossy().into_owned(), "Notes/one.md")?;
    assert_eq!(execute(&req).err(), Some(Fault::Database));
    assert!(!missing.exists());
    Ok(())
}

#[test]
fn frozen_python_read_contracts_match_configured_backend_keys(
) -> Result<(), Box<dyn std::error::Error>> {
    let oracle: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/read-contract.json"))?;
    for case in oracle
        .get("cases")
        .ok_or("missing cases")?
        .as_array()
        .ok_or("missing cases")?
    {
        let (_dir, path) = fixture()?;
        let root = case
            .get("root")
            .ok_or("missing root")?
            .as_str()
            .ok_or("missing root")?;
        let db = Connection::open(&path)?;
        db.execute_batch("DELETE FROM files; DELETE FROM directories;")?;
        for row in oracle
            .get("files")
            .ok_or("missing files")?
            .as_array()
            .ok_or("missing files")?
        {
            let key = diary_workspace_rules::join(
                root,
                &[row
                    .get("path")
                    .ok_or("missing path")?
                    .as_str()
                    .ok_or("path")?],
            );
            let data = row
                .get("data")
                .ok_or("missing data")?
                .as_str()
                .ok_or("data")?
                .as_bytes();
            let version = format!("{:x}", Sha256::digest(data));
            db.execute(
                "INSERT INTO files VALUES(?,?,?,?)",
                rusqlite::params![
                    key,
                    data,
                    version,
                    row.get("updated")
                        .ok_or("missing updated")?
                        .as_f64()
                        .ok_or("updated")?
                ],
            )?;
        }
        for directory in oracle
            .get("directories")
            .ok_or("missing directories")?
            .as_array()
            .ok_or("missing directories")?
        {
            db.execute(
                "INSERT INTO directories VALUES(?)",
                [diary_workspace_rules::join(
                    root,
                    &[directory.as_str().ok_or("directory")?],
                )],
            )?;
        }
        let mut cfg = config();
        *cfg.get_mut("remoteRoot").ok_or("missing remoteRoot")? = json!(root);
        let req: Request = serde_json::from_value(
            json!({"version":1,"dbPath":path,"expectedTenant":TENANT,"op":case.get("op").ok_or("missing op")?,"source":case.get("path").ok_or("missing path")?,"config":cfg}),
        )?;
        let result = execute(&req)?;
        assert_eq!(
            &json!(result.status()),
            case.get("expected")
                .ok_or("missing expected")?
                .get("status")
                .ok_or("missing status")?,
            "{case}"
        );
        if let Some(body) = case.get("expected").ok_or("missing expected")?.get("body") {
            assert_eq!(&result.body().to_value()?, body, "{case}");
        }
        assert_eq!(
            db.query_row::<String, _, _>(
                "SELECT value FROM meta WHERE key='generation'",
                [],
                |r| r.get(0)
            )?,
            "0"
        );
        assert_eq!(
            db.query_row::<i64, _, _>(
                "SELECT COUNT(*) FROM workspace_operation_receipts",
                [],
                |r| r.get(0)
            )?,
            0
        );
        assert_eq!(
            db.query_row::<i64, _, _>("SELECT COUNT(*) FROM workspace_index_outbox", [], |r| r
                .get(0))?,
            0
        );
    }
    Ok(())
}

mod unicode15_list {
    use super::*;
    use rusqlite::params;
    use serde_json::Value;
    use std::error::Error;
    type StoredFile = (String, String, String, f64);
    fn files(db: &Connection) -> Result<Vec<StoredFile>, Box<dyn Error>> {
        let mut q = db.prepare("SELECT path,hex(data),version,updated FROM files ORDER BY path")?;
        let rows = q
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }
    #[test]
    fn frozen_unicode15_file_list_order_matches_python_across_corpus_roots(
    ) -> Result<(), Box<dyn Error>> {
        let fixture: Value = serde_json::from_str(include_str!("fixtures/unicode15-list.json"))?;
        assert_eq!(
            fixture.get("unicode").and_then(Value::as_str),
            Some("15.0.0")
        );
        let cases = fixture
            .get("cases")
            .and_then(Value::as_array)
            .ok_or("cases")?;
        assert_eq!(cases.len(), 18);
        let mut wrong = Vec::new();
        for case in cases {
            let (_dir, path) = common::fixture()?;
            let db = Connection::open(&path)?;
            db.execute("DELETE FROM files", [])?;
            db.execute("DELETE FROM directories", [])?;
            let root = case.get("root").and_then(Value::as_str).ok_or("root")?;
            let prefix = if root.is_empty() {
                String::new()
            } else {
                format!("{root}/")
            };
            for name in case.get("files").and_then(Value::as_array).ok_or("files")? {
                let name = name.as_str().ok_or("name")?;
                let full = format!("{prefix}Notes/{name}");
                db.execute(
                    "INSERT INTO files VALUES(?,?,?,1.0)",
                    params![
                        full,
                        b"synthetic".as_slice(),
                        diary_workspace_rules::sha256(b"synthetic")
                    ],
                )?;
            }
            db.execute(
                "INSERT INTO directories VALUES(?)",
                [format!("{prefix}Notes")],
            )?;
            let before = files(&db)?;
            let mut cfg = common::config();
            cfg.as_object_mut()
                .ok_or("config")?
                .insert("remoteRoot".into(), json!(root));
            let r: Request = serde_json::from_value(
                json!({"version":1,"dbPath":path,"expectedTenant":common::TENANT,"op":"list","source":"Notes","config":cfg}),
            )?;
            let reply = execute(&r)?;
            assert_eq!(reply.status(), 200);
            if &reply.body().to_value()? != case.get("expected").ok_or("expected")? {
                wrong.push(format!(
                    "{root}/{}",
                    case.get("name").and_then(Value::as_str).ok_or("label")?
                ));
            }
            assert_eq!(files(&db)?, before);
            assert_eq!(
                db.query_row::<i64, _, _>(
                    "SELECT count(*) FROM workspace_operation_receipts",
                    [],
                    |r| r.get(0)
                )?,
                0
            );
            assert_eq!(
                db.query_row::<String, _, _>(
                    "SELECT value FROM meta WHERE key='generation'",
                    [],
                    |r| r.get(0)
                )?,
                "0"
            );
        }
        assert!(wrong.is_empty(), "Unicode15 list mismatches: {wrong:?}");
        Ok(())
    }
}
