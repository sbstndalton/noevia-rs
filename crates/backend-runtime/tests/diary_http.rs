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
    db.execute_batch("CREATE TABLE meta(key TEXT PRIMARY KEY,value TEXT NOT NULL);CREATE TABLE files(path TEXT PRIMARY KEY,data BLOB NOT NULL,version TEXT NOT NULL,updated REAL NOT NULL);CREATE TABLE directories(path TEXT PRIMARY KEY);CREATE TABLE workspace_index_outbox(path TEXT PRIMARY KEY);")?;
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
    })?;
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
    let request = |method: &str, path: &str, body: Value| {
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
            "",
            body.to_string().as_bytes(),
            1_790_000_000_123,
        )
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
