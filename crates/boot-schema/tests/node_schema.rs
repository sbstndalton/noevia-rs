#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
//! cowork.db's boot replayed on recorded Node boots (`fixtures/node-boot-schema.v1.json`, written by
//! `record_schema.cjs` from core's own start-up modules): from the same starting database, files and
//! environment, sqlite_master, the journal mode, the migrations, the settings, the backfilled rows and
//! the first-run setup code come out as Node left them, and a second run changes nothing.
use boot_schema::{ensure, Context};
use rusqlite::types::ValueRef;
use rusqlite::Connection;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

const FIXTURE: &str = include_str!("fixtures/node-boot-schema.v1.json");

fn value(v: ValueRef<'_>) -> Value {
    match v {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) => json!(i),
        ValueRef::Real(f) => json!(f),
        ValueRef::Text(t) => json!(String::from_utf8_lossy(t)),
        ValueRef::Blob(b) => json!(b.iter().map(|x| format!("{x:02x}")).collect::<String>()),
    }
}

fn rows(db: &Connection, sql: &str) -> Vec<Value> {
    let mut stmt = db.prepare(sql).unwrap();
    let names: Vec<String> = stmt
        .column_names()
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    let mut out = Vec::new();
    let mut q = stmt.query([]).unwrap();
    while let Some(row) = q.next().unwrap() {
        let mut m = Map::new();
        for (i, n) in names.iter().enumerate() {
            m.insert(n.clone(), value(row.get_ref(i).unwrap()));
        }
        out.push(Value::Object(m));
    }
    out
}

fn dump(dir: &Path) -> Value {
    let db = Connection::open(dir.join("cowork.db")).unwrap();
    let journal: String = db
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .unwrap();
    let versions: Vec<Value> = rows(
        &db,
        "SELECT version FROM schema_migrations ORDER BY version",
    )
    .into_iter()
    .map(|r| r["version"].clone())
    .collect();
    let settings: Vec<Value> = rows(&db, "SELECT key, value FROM settings ORDER BY key")
        .into_iter()
        .map(|r| {
            let (k, v) = (
                r["key"].as_str().unwrap().to_string(),
                r["value"].as_str().unwrap().to_string(),
            );
            let uuid = v.len() == 36 && v.as_bytes()[14] == b'4' && v.split('-').count() == 5;
            if k == "instance_id" && uuid {
                json!([k, "<uuid>"])
            } else if k == "setup_code_hash" && v.len() == 64 {
                json!([k, "<sha256>"])
            } else {
                json!([k, v])
            }
        })
        .collect();
    let features: Vec<Value> = rows(&db, "SELECT * FROM user_features ORDER BY user_id")
        .into_iter()
        .map(|mut r| {
            if r["updated_at"]
                .as_i64()
                .is_some_and(|t| t > 1_000_000_000_000)
            {
                r["updated_at"] = json!("<now>");
            }
            r
        })
        .collect();
    let hash: Option<String> = db
        .query_row(
            "SELECT value FROM settings WHERE key='setup_code_hash'",
            [],
            |r| r.get(0),
        )
        .ok();
    let file = dir.join("first-run-setup-code");
    let setup = std::fs::read_to_string(&file).ok().map(|text| {
        let code = text.strip_suffix('\n').unwrap_or(&text).to_string();
        let shape = if text.len() == 23
            && text.ends_with('\n')
            && code.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            "base64url-16-bytes"
        } else if text == "qa-kept\n" {
            "kept"
        } else {
            "other"
        };
        json!({
            "shape": shape,
            "mode": format!("{:o}", std::fs::metadata(&file).unwrap().permissions().mode() & 0o777),
            "hashMatches": hash.as_deref() == Some(format!("{:x}", Sha256::digest(code.as_bytes())).as_str()),
        })
    });
    json!({
        "master": rows(&db, "SELECT type, name, tbl_name, sql FROM sqlite_master ORDER BY type, name"),
        "journal": journal,
        "versions": versions,
        "settings": settings,
        "features": features,
        "passkeys": rows(&db, "SELECT id, rp_id FROM passkeys ORDER BY id"),
        "storage": rows(&db, "SELECT * FROM storage_connections ORDER BY user_id"),
        "directory": rows(&db, "SELECT * FROM directory_mcp_servers ORDER BY id"),
        "setupFile": setup.unwrap_or(Value::Null),
    })
}

#[test]
fn recorded_node_boots() {
    let fixture: Value = serde_json::from_str(FIXTURE).unwrap();
    let cases = fixture["cases"].as_array().unwrap();
    assert!(cases.len() >= 15, "corpus shrank to {}", cases.len());
    for case in cases {
        let name = case["name"].as_str().unwrap();
        assert_eq!(
            case["idempotent"], true,
            "{name}: Node's second start changed something"
        );
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("ui-data");
        std::fs::create_dir(&dir).unwrap();
        if let Some(seed) = case["seed"].as_str() {
            Connection::open(dir.join("cowork.db"))
                .unwrap()
                .execute_batch(seed)
                .unwrap();
        }
        if case["keepSetupFile"] == true {
            std::fs::write(dir.join("first-run-setup-code"), "qa-kept\n").unwrap();
            std::fs::set_permissions(
                dir.join("first-run-setup-code"),
                std::fs::Permissions::from_mode(0o600),
            )
            .unwrap();
        }
        let env = case["env"].clone();
        let ctx = Context::from_lookup(|k| env[k].as_str().map(str::to_owned));
        let mut n = 0u8;
        let mut random = |buf: &mut [u8]| {
            for b in buf.iter_mut() {
                n = n.wrapping_add(37);
                *b = n;
            }
        };
        let first = ensure(&dir, &ctx, &mut random).unwrap();
        assert_eq!(
            u64::from(first.setup_code.is_some()),
            case["warned"].as_u64().unwrap(),
            "{name}: setup code"
        );
        let got = dump(&dir);
        assert_eq!(got, case["expected"], "{name}");
        let second = ensure(&dir, &ctx, &mut random).unwrap();
        assert!(second.setup_code.is_none(), "{name}");
        assert_eq!(dump(&dir), got, "{name}: a second run changed something");
    }
}

/// A run that fails after the setup code's file was made leaves neither the file nor anything else:
/// the next run makes a code whose hash is saved (Node's start could always be restarted; once it is
/// gone, a stuck first-run setup could only be repaired by hand).
#[test]
fn a_failed_run_leaves_nothing_half_done() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().join("ui-data");
    std::fs::create_dir(&dir).unwrap();
    let db = Connection::open(dir.join("cowork.db")).unwrap();
    db.execute_batch(
        "CREATE TABLE settings(key TEXT PRIMARY KEY, value TEXT NOT NULL);
         CREATE TRIGGER qa_refuse BEFORE INSERT ON settings WHEN NEW.key='setup_code_hash'
           BEGIN SELECT RAISE(ABORT,'qa'); END;",
    )
    .unwrap();
    drop(db);
    let mut fill = |buf: &mut [u8]| buf.fill(7);
    assert!(ensure(&dir, &Context::default(), &mut fill).is_err());
    assert!(
        !dir.join("first-run-setup-code").exists(),
        "the file went with the failed run"
    );
    let db = Connection::open(dir.join("cowork.db")).unwrap();
    let tables: i64 = db
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(tables, 1, "nothing of the failed run is left");
    db.execute_batch("DROP TRIGGER qa_refuse").unwrap();
    drop(db);
    let booted = ensure(&dir, &Context::default(), &mut fill).unwrap();
    let code = booted.setup_code.unwrap();
    let db = Connection::open(dir.join("cowork.db")).unwrap();
    let hash: String = db
        .query_row(
            "SELECT value FROM settings WHERE key='setup_code_hash'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(hash, format!("{:x}", Sha256::digest(code.as_bytes())));
    assert_eq!(
        std::fs::read_to_string(dir.join("first-run-setup-code")).unwrap(),
        format!("{}\n", code.as_str())
    );
}

/// A directory table with `personal` but not `declared_json` (a pair half added elsewhere) is never
/// passed as complete: either the column is added or the run fails.
#[test]
fn a_half_added_directory_pair_is_not_passed_silently() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().join("ui-data");
    std::fs::create_dir(&dir).unwrap();
    Connection::open(dir.join("cowork.db"))
        .unwrap()
        .execute_batch(
            "CREATE TABLE directory_mcp_servers(id TEXT PRIMARY KEY, registry_name TEXT NOT NULL UNIQUE,
               title TEXT NOT NULL, url TEXT NOT NULL, added_by TEXT, added_at INTEGER NOT NULL,
               headers_enc TEXT, oauth INTEGER NOT NULL DEFAULT 0, personal INTEGER NOT NULL DEFAULT 0);",
        )
        .unwrap();
    let mut fill = |buf: &mut [u8]| buf.fill(7);
    let result = ensure(&dir, &Context::default(), &mut fill);
    let db = Connection::open(dir.join("cowork.db")).unwrap();
    let has: bool = db
        .query_row(
            "SELECT count(*) FROM pragma_table_info('directory_mcp_servers') WHERE name='declared_json'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .unwrap()
        == 1;
    assert!(
        has || result.is_err(),
        "declared_json missing after a run that passed"
    );
}
