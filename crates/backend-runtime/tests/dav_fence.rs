#[path = "../../server-auth/tests/common/mod.rs"]
mod auth_fixture;
use backend_runtime::{dav::DavBackend, diary::Corpus};
use serde_json::{json, Value};
use server_dav::{Backend, Identity, Scope, WriteCondition};
use std::{collections::HashMap, sync::Arc};
fn replace(value: &mut Value) {
    match value {
        Value::String(s) if s == "u-member" => *s = "01234567-89ab-4cde-8fab-0123456789ab".into(),
        Value::Array(a) => {
            for v in a {
                replace(v)
            }
        }
        Value::Object(o) => {
            for v in o.values_mut() {
                replace(v)
            }
        }
        _ => {}
    }
}
#[tokio::test]
async fn real_auth_writer_fences_revoked_dav_credentials_at_corpus_sink(
) -> Result<(), Box<dyn std::error::Error>> {
    let fixture: Value = serde_json::from_str(include_str!(
        "../../server-auth/tests/fixtures/node-auth.v1.json"
    ))?;
    let mut scenario = fixture
        .get("scenarios")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .ok_or("scenario")?
        .clone();
    replace(&mut scenario);
    let built = auth_fixture::build(&scenario);
    let user = "01234567-89ab-4cde-8fab-0123456789ab";
    let raw = rusqlite::Connection::open(server_store::db_path(built.dir.path()))?;
    raw.execute_batch("CREATE TABLE IF NOT EXISTS dav_settings(user_id TEXT PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,scope TEXT NOT NULL CHECK(scope IN ('off','lan','public')),acknowledged_at INTEGER,updated_at INTEGER NOT NULL);CREATE TABLE IF NOT EXISTS storage_connections(user_id TEXT PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,kind TEXT NOT NULL,base_url TEXT NOT NULL DEFAULT '',bucket TEXT NOT NULL DEFAULT '',username TEXT NOT NULL DEFAULT '',secret TEXT NOT NULL DEFAULT '',corpus_root TEXT NOT NULL DEFAULT '',updated_at INTEGER NOT NULL);")?;
    raw.execute(
        "INSERT INTO dav_settings(user_id,scope,updated_at) VALUES(?,'lan',0)",
        [user],
    )?;
    raw.execute(
        "UPDATE user_features SET diary_enabled=1 WHERE user_id=?",
        [user],
    )?;
    let path = built.dir.path().join("managed.db");
    let db = rusqlite::Connection::open(&path)?;
    let layout: Value = serde_json::from_str(include_str!(
        "../../diary-workspace-transaction/tests/fixtures/managed-schema.json"
    ))?;
    for table in layout
        .get("tables")
        .and_then(Value::as_array)
        .ok_or("tables")?
    {
        db.execute_batch(table.get("sql").and_then(Value::as_str).ok_or("schema")?)?;
    }
    db.execute_batch("CREATE TABLE workspace_operation_receipts(id TEXT PRIMARY KEY,fingerprint TEXT NOT NULL,op TEXT NOT NULL,response BLOB NOT NULL,created REAL NOT NULL);")?;
    db.execute(
        "INSERT INTO meta VALUES('workspace_operation_namespace',?)",
        ["11234567-89ab-4cde-8fab-0123456789ab"],
    )?;
    for (key, value) in [("tenant", user), ("active", "1"), ("generation", "0")] {
        db.execute("INSERT INTO meta VALUES(?,?)", [key, value])?;
    }
    let corpus = Arc::new(Corpus::open(&path, user, 3.0)?);
    let mut tenants = HashMap::new();
    tenants.insert(user.to_owned(), corpus);
    let writer = Arc::new(server_store::Writer::open(
        built.dir.path(),
        server_store::RustAuth::owned(),
    )?);
    let config = serde_json::from_value(
        json!({"remoteRoot":"","entriesPrefix":"Entries","monthlyPrefix":"Monthly","monthFileTemplate":"{year}-{month02}.md","indexFile":"index.md","monthNames":["January","February","March","April","May","June","July","August","September","October","November","December"]}),
    )?;
    let backend =
        DavBackend::new(Arc::new(built.store), writer, tenants, Scope::Lan)?.with_workspace(config);
    let identity = Identity {
        user_id: user.into(),
        credential_id: "a".repeat(32),
    };
    assert!(backend
        .allowed(&identity, Scope::Lan)
        .await
        .map_err(|e| format!("{e:?}"))?);
    let version = backend
        .write(&identity, "note.md", "synthetic", WriteCondition::Create)
        .await
        .map_err(|e| format!("{e:?}"))?;
    assert_eq!(
        backend
            .read(&identity, "note.md")
            .await
            .map_err(|e| format!("{e:?}"))?
            .map(|f| f.content),
        Some("synthetic".into())
    );
    let copy = backend
        .workspace(
            &identity,
            json!({"op":"copy","path":"note.md","destination":"copy.md","overwrite":false}),
        )
        .await
        .map_err(|e| format!("{e:?}"))?;
    assert_eq!(copy.0, 200);
    assert_eq!(
        backend
            .read(&identity, "copy.md")
            .await
            .map_err(|e| format!("{e:?}"))?
            .ok_or("copy")?
            .content,
        "synthetic"
    );
    let stale = backend
        .workspace(
            &identity,
            json!({"op":"delete","path":"copy.md","version":"0".repeat(64)}),
        )
        .await
        .map_err(|e| format!("{e:?}"))?;
    assert_eq!(stale.0, 412);
    let deleted = backend
        .workspace(
            &identity,
            json!({"op":"delete","path":"copy.md","version":version}),
        )
        .await
        .map_err(|e| format!("{e:?}"))?;
    assert_eq!(deleted.0, 200);
    assert!(backend
        .read(&identity, "copy.md")
        .await
        .map_err(|e| format!("{e:?}"))?
        .is_none());
    let receipts: i64 = db.query_row(
        "SELECT count(*) FROM workspace_operation_receipts",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(receipts, 3);
    raw.execute(
        "DELETE FROM app_passwords WHERE id=?",
        [&identity.credential_id],
    )?;
    assert!(!backend
        .allowed(&identity, Scope::Lan)
        .await
        .map_err(|e| format!("{e:?}"))?);
    assert_eq!(
        backend
            .write(
                &identity,
                "note.md",
                "must not land",
                WriteCondition::Match(version)
            )
            .await,
        Err(server_dav::Fault::Forbidden)
    );
    let content: Vec<u8> =
        db.query_row("SELECT data FROM files WHERE path='note.md'", [], |r| {
            r.get(0)
        })?;
    assert_eq!(content, b"synthetic");
    Ok(())
}
