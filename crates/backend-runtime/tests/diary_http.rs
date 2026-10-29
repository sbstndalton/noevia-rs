#[path = "../../server-auth/tests/common/mod.rs"]
mod auth_fixture;
use backend_runtime::{
    diary::Corpus,
    diary_http::{DiaryHttp, Tenant},
    journal::Journal,
    Fault,
};
use serde_json::{json, Value};
use std::{collections::HashMap, sync::Arc};
const USER: &str = "01234567-89ab-4cde-8fab-0123456789ab";
fn replace(v: &mut Value) {
    match v {
        Value::String(s) if s == "u-member" => *s = USER.into(),
        Value::Array(a) => {
            for v in a {
                replace(v);
            }
        }
        Value::Object(o) => {
            for v in o.values_mut() {
                replace(v);
            }
        }
        _ => {}
    }
}
#[test]
fn original_editor_contract_and_revocation_fence() -> Result<(), Box<dyn std::error::Error>> {
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
    let raw = rusqlite::Connection::open(server_store::db_path(built.dir.path()))?;
    raw.execute_batch("CREATE TABLE IF NOT EXISTS storage_connections(user_id TEXT PRIMARY KEY,kind TEXT NOT NULL,base_url TEXT NOT NULL DEFAULT '',bucket TEXT NOT NULL DEFAULT '',username TEXT NOT NULL DEFAULT '',secret TEXT NOT NULL DEFAULT '',corpus_root TEXT NOT NULL DEFAULT '',updated_at INTEGER NOT NULL);")?;
    raw.execute(
        "UPDATE user_features SET diary_enabled=1 WHERE user_id=?",
        [USER],
    )?;
    let path = built.dir.path().join("managed.db");
    let db = rusqlite::Connection::open(&path)?;
    let schema: Value = serde_json::from_str(include_str!(
        "../../diary-workspace-transaction/tests/fixtures/managed-schema.json"
    ))?;
    for table in schema
        .get("tables")
        .and_then(Value::as_array)
        .ok_or("tables")?
    {
        db.execute_batch(table.get("sql").and_then(Value::as_str).ok_or("sql")?)?;
    }
    db.execute_batch("CREATE TABLE workspace_operation_receipts(id TEXT PRIMARY KEY,fingerprint TEXT NOT NULL,op TEXT NOT NULL,response BLOB NOT NULL,created REAL NOT NULL);INSERT INTO meta VALUES('workspace_operation_namespace','11234567-89ab-4cde-8fab-0123456789ab');")?;
    for (k, v) in [("tenant", USER), ("active", "1"), ("generation", "0")] {
        db.execute("INSERT INTO meta VALUES(?,?)", [k, v])?;
    }
    let corpus = Corpus::open(&path, USER, 3.0)?;
    let migration = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(built.dir.path().join("managed-diary.lock"))?;
    migration.lock()?;
    assert_eq!(
        corpus.read(
            "note.md",
            &backend_runtime::Control::new(std::time::Duration::from_millis(10))?
        ),
        Err(Fault::Deadline)
    );
    drop(migration);

    let journal = Arc::new(Journal::open(&built.dir.path().join("index.db"))?);
    let mut tenants = HashMap::new();
    tenants.insert(
        USER.into(),
        Tenant {
            corpus,
            journal: Arc::clone(&journal),
        },
    );
    let diary = DiaryHttp::new(tenants)?.with_replay(backend_runtime::replay::Layout {
        root: String::new(),
        monthly_prefix: "Monthly".into(),
        entries_prefix: "Entries".into(),
        month_file_template: "{year}-{month02}.md".into(),
        index_file: "INDEX.md".into(),
        daily: false,
        index_enabled: true,
    })?.with_workspace(serde_json::from_value(json!({"remoteRoot":"","entriesPrefix":"Entries","monthlyPrefix":"Monthly","monthFileTemplate":"{year}-{month02}.md","indexFile":"INDEX.md","monthNames":["January","February","March","April","May","June","July","August","September","October","November","December"]}))?);
    let writer = server_store::Writer::open(built.dir.path(), server_store::RustAuth::owned())?;
    let config = server_auth::AuthConfig::from_lookup(|key| {
        (key == "PUBLIC_ORIGIN").then(|| "https://noevia.example.test".into())
    })
    .map_err(|e| format!("{e:?}"))?;
    let auth = server_auth::Authenticator::new(config);
    let input = scenario
        .get("cases")
        .and_then(Value::as_array)
        .ok_or("cases")?
        .iter()
        .find(|c| {
            c.pointer("/expect/csrf") == Some(&json!(true))
                && c.pointer("/expect/authn/user/id") == Some(&json!(USER))
        })
        .and_then(|v| v.get("input"))
        .ok_or("credentials")?;
    let cookie = input
        .get("cookie")
        .and_then(Value::as_str)
        .ok_or("cookie")?;
    let csrf = input.get("csrf").and_then(Value::as_str).ok_or("csrf")?;
    let request_bytes = |method: &str, path: &str, query: &str, body: &[u8]| {
        let creds = server_auth::Creds::from_headers(
            method,
            [
                ("cookie", cookie.as_bytes()),
                ("x-csrf-token", csrf.as_bytes()),
                ("origin", b"https://noevia.example.test".as_slice()),
            ],
        );
        diary.handle(
            &writer,
            &auth,
            &creds,
            USER,
            method,
            path,
            query,
            body,
            1_790_000_000_123,
        )
    };
    let request = |method: &str, path: &str, body: Value| {
        request_bytes(method, path, "", body.to_string().as_bytes())
    };
    assert_eq!(
        request("POST", "/api/diary/file", json!({"path":"note.md"}))?,
        (200, json!({"path":"note.md","content":null,"version":null}))
    );
    let saved = request(
        "PUT",
        "/api/diary/file",
        json!({"path":"note.md","content":"original","version":null}),
    )?;
    assert_eq!(saved.0, 200);
    let version = saved.1.get("version").cloned().ok_or("version")?;
    journal.clear_dirty("note.md")?;
    assert_eq!(
        request(
            "PUT",
            "/api/diary/file",
            json!({"path":"note.md","content":"original","version":version})
        )?,
        saved
    );
    assert!(journal.dirty_documents()?.is_empty());
    assert_eq!(
        request(
            "PUT",
            "/api/diary/file",
            json!({"path":"note.md","content":"wrong","version":null})
        )?
        .0,
        409
    );
    assert!(journal.dirty_documents()?.is_empty());
    let listed = request("GET", "/api/diary/files", Value::Null)?;
    assert_eq!(
        listed.1.pointer("/files/0/modified"),
        Some(&json!(1_790_000_000.123))
    );
    assert_eq!(
        request("POST", "/api/diary/file", json!({"path":"/note.md"}))?.0,
        400
    );
    let trash_id = "51234567-89ab-4cde-8fab-0123456789ab";
    let trash = json!({"action":"trash","id":trash_id,"path":"note.md","version":saved.1.get("version").ok_or("version")?});
    let removed = request("POST", "/api/diary/workspace-trash", trash.clone())?;
    assert_eq!(removed.0, 200);
    assert_eq!(
        request("POST", "/api/diary/workspace-trash", trash.clone())?,
        removed
    );
    assert_eq!(
        request("POST", "/api/diary/file", json!({"path":"note.md"}))?
            .1
            .get("content"),
        Some(&Value::Null)
    );
    let listing = request("GET", "/api/diary/workspace-trash", Value::Null)?;
    assert_eq!(listing.1.pointer("/records/0/id"), Some(&json!(trash_id)));
    let restore = json!({"action":"restore","id":trash_id});
    let restored = request("POST", "/api/diary/workspace-trash", restore.clone())?;
    assert_eq!(restored.0, 200);
    assert_eq!(
        request("POST", "/api/diary/workspace-trash", restore)?,
        restored
    );
    assert_eq!(
        request("POST", "/api/diary/file", json!({"path":"note.md"}))?
            .1
            .get("content"),
        Some(&json!("original"))
    );
    let export_creds = server_auth::Creds::from_headers(
        "GET",
        [
            ("cookie", cookie.as_bytes()),
            ("origin", b"https://noevia.example.test".as_slice()),
        ],
    );
    let exported = diary.export(&writer, &auth, &export_creds, USER, 1_790_000_000_123)?;
    let verified =
        workspace_restore_verify::verify_archive(&exported).map_err(|e| format!("{e:?}"))?;
    assert!(verified
        .files()
        .any(|(f, b)| f.path == "note.md" && b == b"original"));
    assert!(verified
        .files()
        .any(|(f, _)| f.path.starts_with(".noevia-trash/")));
    let preview = request_bytes(
        "POST",
        "/api/diary/workspace-import",
        "action=preview&name=Restored",
        &exported,
    )?;
    assert_eq!(preview.0, 200);
    let fingerprint = preview
        .1
        .get("fingerprint")
        .and_then(Value::as_str)
        .ok_or("fingerprint")?;
    let apply_query = format!("action=apply&name=Restored&fingerprint={fingerprint}");
    let applied = request_bytes(
        "POST",
        "/api/diary/workspace-import",
        &apply_query,
        &exported,
    )?;
    assert_eq!(applied.0, 200);
    assert_eq!(applied.1.get("alreadyApplied"), Some(&json!(false)));
    let imported = request(
        "POST",
        "/api/diary/file",
        json!({"path":"Imports/Restored/note.md"}),
    )?;
    assert_eq!(imported.1.get("content"), Some(&json!("original")));
    assert_eq!(request("PUT","/api/diary/file",json!({"path":"Imports/Restored/note.md","content":"later edit","version":imported.1.get("version")}))?.0,200);
    let retried = request_bytes(
        "POST",
        "/api/diary/workspace-import",
        &apply_query,
        &exported,
    )?;
    assert_eq!(retried.1.get("alreadyApplied"), Some(&json!(true)));
    assert_eq!(
        request(
            "POST",
            "/api/diary/file",
            json!({"path":"Imports/Restored/note.md"})
        )?
        .1
        .get("content"),
        Some(&json!("later edit"))
    );
    let xid = "41234567-89ab-4cde-8fab-0123456789ab";
    journal.enqueue("exchange", &json!({"xid":xid,"day":"2026-10-28","sub_header":"12:00","body":diary_corpus_write::render_exchange("first","response",xid),"scaffold":true}))?;
    let edit_control = backend_runtime::Control::new(std::time::Duration::from_secs(5))?;
    assert_eq!(diary.replay_pending(&writer, &edit_control)?, 1);
    let current = request(
        "POST",
        "/api/diary/file",
        json!({"path":"Monthly/2026-10.md"}),
    )?;
    let hash =
        diary_corpus_write::exchange_hash(current.1.get("content").ok_or("content")?.as_str(), xid)
            .ok_or("hash")?;
    journal.enqueue(
        "exchange_edit",
        &json!({"xid":xid,"month":"2026-10","new_me":"queued","new_claude":"response"}),
    )?;
    let edit =
        json!({"xid":xid,"month":"2026-10","me":"corrected","assistant":"reply","base_hash":hash});
    assert_eq!(
        request("POST", "/api/diary/entries/edit", edit.clone())?.0,
        409
    );
    assert_eq!(journal.pending_count()?, 1);
    assert_eq!(diary.replay_pending(&writer, &edit_control)?, 1);
    assert_eq!(
        request("POST", "/api/diary/entries/edit", edit.clone())?.0,
        409
    );
    let current = request(
        "POST",
        "/api/diary/file",
        json!({"path":"Monthly/2026-10.md"}),
    )?;
    let mut edit = edit;
    *edit.get_mut("base_hash").ok_or("base_hash")? = json!(diary_corpus_write::exchange_hash(
        current.1.get("content").ok_or("content")?.as_str(),
        xid
    ));
    let corrected = request("POST", "/api/diary/entries/edit", edit.clone())?;
    assert_eq!(corrected.0, 200);
    assert_eq!(corrected.1.get("day").ok_or("day")?, "2026-10-01");
    let current = request(
        "POST",
        "/api/diary/file",
        json!({"path":"Monthly/2026-10.md"}),
    )?;
    assert!(current
        .1
        .get("content")
        .ok_or("content")?
        .as_str()
        .ok_or("content")?
        .contains("corrected"));
    assert_eq!(
        corrected.1.get("hash").ok_or("hash")?,
        &json!(diary_corpus_write::exchange_hash(
            current.1.get("content").ok_or("content")?.as_str(),
            xid
        ))
    );
    assert_eq!(request("POST", "/api/diary/entries/edit", edit)?.0, 409);
    raw.execute(
        "UPDATE user_features SET diary_enabled=0 WHERE user_id=?",
        [USER],
    )?;
    assert_eq!(
        request("POST", "/api/diary/file", json!({"path":"note.md"}))?.0,
        404
    );
    journal.enqueue(
        "index_month",
        &json!({"month":"2026-10.md","label":"October 2026"}),
    )?;
    let control = backend_runtime::Control::new(std::time::Duration::from_secs(5))?;
    assert_eq!(diary.replay_pending(&writer, &control)?, 0);
    assert_eq!(journal.pending_count()?, 1);
    raw.execute(
        "UPDATE user_features SET diary_enabled=1 WHERE user_id=?",
        [USER],
    )?;
    assert_eq!(diary.replay_pending(&writer, &control)?, 1);
    assert_eq!(journal.pending_count()?, 0);
    raw.execute("DELETE FROM sessions WHERE user_id=?", [USER])?;
    assert_eq!(
        request("POST", "/api/diary/file", json!({"path":"note.md"})),
        Err(Fault::Unauthorized)
    );
    Ok(())
}
