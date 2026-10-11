#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
use super::*;
#[allow(dead_code)]
#[path = "../bin/support/prompts_process.rs"]
mod prompts_process;
fn root() -> tempfile::TempDir {
    // Production rejects symlink ancestors. macOS's usual /var temporary root is a symlink.
    let path = std::env::var_os("S1_PROMPTS_TEST_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap());
    tempfile::tempdir_in(path).unwrap()
}
fn open(root: &Path) -> Store {
    let mut s = Store::open(root).unwrap();
    s.clock = || Ok(1_700_000_000.125);
    s
}
fn run(s: &mut Store, op: Operation) -> Result<serde_json::Value> {
    s.budget(Instant::now() + DEADLINE, Arc::new(AtomicBool::new(false)))?;
    Ok(
        serde_json::from_slice(&s.run(op, Instant::now() + DEADLINE, &AtomicBool::new(false))?)
            .unwrap(),
    )
}
#[test]
fn differential_seed_crud_sequence_order_and_ids() {
    let root = root();
    let mut s = open(root.path());
    s.seed().unwrap();
    let oracle = crate::prompts::tests::oracle();
    assert_eq!(
        run(&mut s, Operation::List).unwrap()["prompts"],
        oracle["cases"][1]["expected"]["prompts"]
    );
    let mut last_id = 4;
    let mut expected_rows = 4;
    for case in oracle["cases"].as_array().unwrap().iter().skip(2) {
        let q = &case["request"];
        if q["method"] != "POST"
            || q["path"] != "/api/v1/prompts"
            || case["expected"]["status"] != 200
        {
            continue;
        }
        let (name, body) = crate::prompts::parse_fields(&crate::prompts::tests::body(q)).unwrap();
        assert_eq!(
            run(&mut s, Operation::Add(name, body)).unwrap(),
            case["expected"]["body"]
        );
        last_id = case["expected"]["body"]["id"].as_i64().unwrap();
        expected_rows = case["expected"]["body"]["prompts"]
            .as_array()
            .unwrap()
            .len();
    }
    assert_eq!(
        run(&mut s, Operation::Delete(1)).unwrap()["prompts"]
            .as_array()
            .unwrap()
            .len(),
        expected_rows - 1
    );
    assert_eq!(run(&mut s, Operation::Delete(1)), Err(Error::NotFound));
    let added = run(&mut s, Operation::Add("same".into(), "x".into())).unwrap();
    assert_eq!(added["id"], last_id + 1);
}
#[test]
fn independent_seed_commits_restart_and_deleted_seed_retention() {
    for boundary in 0..=4 {
        let root = root();
        let mut s = open(root.path());
        for (name, body) in SEED.iter().take(boundary) {
            s.insert(name, body).unwrap();
        }
        drop(s);
        let mut s = open(root.path());
        s.seed().unwrap();
        assert_eq!(s.list().unwrap().len(), 4);
        sql(s
            .db
            .execute("DELETE FROM prompts WHERE name=?", [SEED[0].0]))
        .unwrap();
        drop(s);
        let mut s = open(root.path());
        s.seed().unwrap();
        assert_eq!(s.list().unwrap().len(), 3);
    }
}

#[test]
fn actual_seed_insert_failure_at_each_boundary_preserves_commits_and_restart() {
    for boundary in 0..4 {
        let root = root();
        let mut s = open(root.path());
        s.db.execute_batch(&format!("CREATE TRIGGER fail_seed BEFORE INSERT ON prompts WHEN (SELECT count(*) FROM prompts)>={boundary} BEGIN SELECT RAISE(ABORT,'synthetic seed fault'); END;")).unwrap();
        assert_eq!(s.seed(), Err(Error::Store));
        let observer = Connection::open(root.path().join("model_loader.db")).unwrap();
        assert_eq!(
            observer
                .query_row("SELECT count(*) FROM prompts", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            boundary
        );
        assert_eq!(
            observer
                .query_row("SELECT count(*) FROM kv WHERE key=?", [SEED_KEY], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
        drop(observer);
        drop(s);
        let db = Connection::open(root.path().join("model_loader.db")).unwrap();
        db.execute_batch("DROP TRIGGER fail_seed").unwrap();
        drop(db);
        let mut s = open(root.path());
        s.seed().unwrap();
        assert_eq!(s.list().unwrap().len(), 4);
        assert_eq!(
            s.db.query_row("SELECT count(DISTINCT name) FROM prompts", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            4
        );
    }
}

#[test]
fn signed_ids_autoincrement_exhaustion_and_unencodable_text_are_not_rewritten() {
    let root = root();
    let mut s = open(root.path());
    s.db.execute(
        "INSERT INTO prompts(id,name,body,created_at) VALUES(?,?,?,?)",
        params![i64::MIN, "negative", "body", 1.25],
    )
    .unwrap();
    assert_eq!(s.list().unwrap().first().unwrap().id, i64::MIN);
    s.db.execute(
        "INSERT INTO prompts(id,name,body,created_at) VALUES(?,?,?,?)",
        params![i64::MAX, "maximum", "body", 1.25],
    )
    .unwrap();
    assert_eq!(s.insert("must fail", "body"), Err(Error::Store));
    assert_eq!(
        s.db.query_row("SELECT count(*) FROM prompts", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        2
    );
    s.db.execute_batch("DELETE FROM prompts;UPDATE sqlite_sequence SET seq=0 WHERE name='prompts';INSERT INTO prompts(name,body,created_at) VALUES(CAST(x'ff' AS TEXT),'body',1)").unwrap();
    assert_eq!(run(&mut s, Operation::List), Err(Error::Limit));
}

#[test]
fn schema_comments_strings_and_quoted_names_cannot_fake_autoincrement() {
    for extra in [
        "/* AUTOINCREMENT */",
        "-- AUTOINCREMENT\n",
        ",extra TEXT DEFAULT 'AUTOINCREMENT'",
        ",\"AUTOINCREMENT\" TEXT",
        ",[AUTOINCREMENT] TEXT",
        ",`AUTOINCREMENT` TEXT",
    ] {
        let root = root();
        let db = Connection::open(root.path().join("model_loader.db")).unwrap();
        db.execute_batch(&format!("CREATE TABLE prompts(id INTEGER PRIMARY KEY,name TEXT NOT NULL,body TEXT NOT NULL,created_at REAL NOT NULL {extra});")).unwrap();
        drop(db);
        assert!(
            matches!(Store::open(root.path()), Err(Error::Store)),
            "PROMPTS_ASSERT:schema-autoincrement {extra}"
        );
    }
    let root = root();
    let db = Connection::open(root.path().join("model_loader.db")).unwrap();
    db.execute_batch("CREATE TABLE prompts(id INTEGER PRIMARY KEY /* ignored */ AUTOINCREMENT,name TEXT NOT NULL,body TEXT NOT NULL,created_at REAL NOT NULL,extra TEXT DEFAULT 'AUTOINCREMENT')").unwrap();
    drop(db);
    assert!(Store::open(root.path()).is_ok());
}
#[test]
fn failed_flag_commit_keeps_independent_rows_and_restart_fills_only_missing() {
    let root = root();
    let mut s = open(root.path());
    s.db.execute_batch(
        "CREATE TRIGGER fail_flag BEFORE INSERT ON kv BEGIN SELECT RAISE(FAIL,'synthetic');END;",
    )
    .unwrap();
    assert_eq!(s.seed(), Err(Error::Store));
    assert_eq!(s.list().unwrap().len(), 4);
    s.db.execute_batch("DROP TRIGGER fail_flag").unwrap();
    s.seed().unwrap();
    assert_eq!(s.list().unwrap().len(), 4);
}
#[test]
fn unrelated_tables_extra_columns_and_kv_sentinels_survive() {
    let root = root();
    let mut s = open(root.path());
    s.db.execute_batch("CREATE TABLE other(id INTEGER PRIMARY KEY,value BLOB);INSERT INTO other VALUES(8,x'001122');INSERT INTO kv VALUES('other','sentinel');ALTER TABLE prompts ADD COLUMN extra TEXT DEFAULT 'kept';").unwrap();
    s.seed().unwrap();
    run(
        &mut s,
        Operation::Add("Robert'); DROP TABLE other;--".into(), "literal".into()),
    )
    .unwrap();
    run(&mut s, Operation::Delete(1)).unwrap();
    assert_eq!(
        s.db.query_row("SELECT value FROM kv WHERE key='other'", [], |r| r
            .get::<_, String>(0))
            .unwrap(),
        "sentinel"
    );
    assert_eq!(
        s.db.query_row("SELECT hex(value) FROM other WHERE id=8", [], |r| r
            .get::<_, String>(0))
            .unwrap(),
        "001122"
    );
    assert_eq!(
        s.db.query_row("SELECT extra FROM prompts LIMIT 1", [], |r| r
            .get::<_, String>(0))
            .unwrap(),
        "kept"
    );
}
#[test]
fn failed_readback_does_not_undo_committed_post_or_delete() {
    let root = root();
    let mut s = open(root.path());
    let observer = Connection::open(root.path().join("model_loader.db")).unwrap();
    s.db.execute_batch("CREATE TRIGGER too_large AFTER INSERT ON prompts BEGIN INSERT INTO prompts(name,body,created_at) VALUES('bad',replace(hex(zeroblob(140000)),'0','x'),1);END;").unwrap();
    assert_eq!(
        run(&mut s, Operation::Add("committed".into(), "x".into())),
        Err(Error::Limit)
    );
    assert_eq!(
        observer
            .query_row("SELECT name FROM prompts WHERE id=1", [], |r| r
                .get::<_, String>(0))
            .optional()
            .unwrap(),
        Some("committed".to_owned())
    );
    assert_eq!(run(&mut s, Operation::Delete(1)), Err(Error::Limit));
    assert_eq!(
        observer
            .query_row("SELECT count(*) FROM prompts WHERE id=1", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
}
#[test]
fn lock_symlink_writable_special_and_replacement_fail_closed() {
    let root = root();
    let s = open(root.path());
    assert!(matches!(Store::open(root.path()), Err(Error::Busy)));
    drop(s);
    let bad = root.path().join("alias");
    std::os::unix::fs::symlink(root.path(), &bad).unwrap();
    assert!(matches!(Store::open(&bad), Err(Error::Store)));
    let mut s = open(root.path());
    std::fs::rename(
        root.path().join("model_loader.db.prompts.lock"),
        root.path().join("old.lock"),
    )
    .unwrap();
    std::fs::write(root.path().join("model_loader.db.prompts.lock"), "").unwrap();
    assert_eq!(run(&mut s, Operation::List), Err(Error::Store));
}
#[test]
fn expired_or_cancelled_request_never_starts_a_write() {
    let root = root();
    let mut s = open(root.path());
    let cancel = Arc::new(AtomicBool::new(true));
    s.budget(Instant::now() + DEADLINE, cancel.clone()).unwrap();
    assert_eq!(
        s.run(
            Operation::Add("never".into(), "written".into()),
            Instant::now() + DEADLINE,
            &cancel
        ),
        Err(Error::Store)
    );
    s.budget(Instant::now() + DEADLINE, Arc::new(AtomicBool::new(false)))
        .unwrap();
    assert!(s.list().unwrap().is_empty());
}

#[test]
fn actual_python_seed_flags_case_variants_and_nocase_ties() {
    let oracle = crate::prompts::tests::oracle();
    for case in oracle["cases"].as_array().unwrap() {
        let q = &case["request"];
        if q["op"] != "ordering" && q["op"] != "seedcase" {
            continue;
        }
        let root = root();
        let mut s = open(root.path());
        for name in q["names"].as_array().unwrap() {
            s.insert(name.as_str().unwrap(), "synthetic").unwrap();
        }
        if q["indexed"] == true {
            s.db.execute_batch("CREATE INDEX nocase_desc ON prompts(name COLLATE NOCASE,id DESC)")
                .unwrap();
        }
        let before = s.list().unwrap().len();
        if q["op"] == "seedcase" {
            if let Some(flag) = q["flag"].as_str() {
                s.db.execute(
                    "INSERT INTO kv(key,value) VALUES(?,?)",
                    params![SEED_KEY, flag],
                )
                .unwrap();
            }
            s.seed().unwrap();
            let flag: String =
                s.db.query_row("SELECT value FROM kv WHERE key=?", [SEED_KEY], |r| r.get(0))
                    .optional()
                    .unwrap()
                    .unwrap_or_default();
            assert_eq!(
                serde_json::json!({"added":s.list().unwrap().len()-before,"prompts":s.list().unwrap(),"flag":flag}),
                case["expected"]
            );
        } else {
            assert_eq!(run(&mut s, Operation::List).unwrap(), case["expected"]);
        }
    }
}
#[test]
fn row_response_dynamic_type_and_vm_work_limits_refuse() {
    let root = root();
    let mut s = open(root.path());
    s.db.execute_batch("WITH RECURSIVE n(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM n WHERE i<4096) INSERT INTO prompts(name,body,created_at) SELECT 'same','x',1 FROM n;").unwrap();
    assert_eq!(s.insert("extra", "x"), Err(Error::Limit));
    s.db.execute(
        "INSERT INTO prompts(name,body,created_at) VALUES('extra','x',1)",
        [],
    )
    .unwrap();
    assert_eq!(run(&mut s, Operation::List), Err(Error::Limit));
    s.db.execute_batch("DELETE FROM prompts;INSERT INTO prompts VALUES(1,x'00','x',1)")
        .unwrap();
    assert_eq!(run(&mut s, Operation::List), Err(Error::Limit));
    s.db.execute_batch("DELETE FROM prompts;").unwrap();
    s.budget(Instant::now() + DEADLINE, Arc::new(AtomicBool::new(false)))
        .unwrap();
    assert!(s.db.query_row("WITH RECURSIVE n(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM n WHERE i<10000000) SELECT sum(i) FROM n",[],|r|r.get::<_,i64>(0)).is_err());
    s.budget(Instant::now() + DEADLINE, Arc::new(AtomicBool::new(false)))
        .unwrap();
    s.db.execute_batch("WITH RECURSIVE n(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM n WHERE i<80) INSERT INTO prompts(name,body,created_at) SELECT 'same',replace(hex(zeroblob(120000)),'0','x'),1 FROM n").unwrap();
    assert_eq!(run(&mut s, Operation::List), Err(Error::Limit));
}
#[test]
fn bounded_busy_wait_does_not_retry_or_write_later() {
    let root = root();
    let mut s = open(root.path());
    let other = Connection::open(root.path().join("model_loader.db")).unwrap();
    other.execute_batch("BEGIN IMMEDIATE").unwrap();
    let start = Instant::now();
    assert_eq!(s.insert("blocked", "x"), Err(Error::Store));
    assert!(start.elapsed() < Duration::from_secs(2));
    other.execute_batch("ROLLBACK").unwrap();
    assert!(s.list().unwrap().is_empty());
}

#[test]
fn response_encoding_observes_deadline_cancellation_and_exact_byte_cap() {
    let cancel = Arc::new(AtomicBool::new(false));
    let mut writer = Bytes {
        bytes: Vec::new(),
        deadline: Instant::now() + Duration::from_secs(5),
        cancel: cancel.clone(),
    };
    writer.write_all(&vec![b'x'; RESPONSE_CAP]).unwrap();
    assert!(writer.write_all(b"x").is_err());
    assert_eq!(writer.bytes.len(), RESPONSE_CAP);
    writer.bytes.clear();
    cancel.store(true, Ordering::Relaxed);
    assert!(writer.write_all(b"x").is_err());
    assert!(writer.bytes.is_empty());
    cancel.store(false, Ordering::Relaxed);
    writer.deadline = Instant::now() - Duration::from_secs(1);
    assert!(writer.write_all(b"x").is_err());
}
#[test]
fn final_files_and_directory_modes_reject_unsafe_roots() {
    use std::os::unix::fs::PermissionsExt;
    for suffix in ["", "-wal", "-shm", ".prompts.lock"] {
        let root = root();
        let outside = root.path().join("other");
        std::fs::write(&outside, "").unwrap();
        std::os::unix::fs::symlink(
            &outside,
            root.path().join(format!("model_loader.db{suffix}")),
        )
        .unwrap();
        assert!(matches!(Store::open(root.path()), Err(Error::Store)));
    }
    let root = root();
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o777)).unwrap();
    assert!(matches!(Store::open(root.path()), Err(Error::Store)));
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::create_dir(root.path().join("model_loader.db")).unwrap();
    assert!(matches!(Store::open(root.path()), Err(Error::Store)));
}

#[tokio::test]
async fn bounded_queue_and_cancelled_busy_write_cannot_commit_after_unlock() {
    let root = root();
    let prompts = Prompts::open_with_clock(root.path(), || Ok(1_700_000_000.125))
        .await
        .unwrap();
    let other = Connection::open(root.path().join("model_loader.db")).unwrap();
    other.execute_batch("BEGIN IMMEDIATE").unwrap();
    let mut requests = Vec::new();
    for index in 0..4 {
        let mut request =
            Box::pin(prompts.response(Operation::Add(format!("cancel-{index}"), "never".into())));
        assert!(futures_util::poll!(&mut request).is_pending());
        requests.push(request);
    }
    assert_eq!(prompts.response(Operation::List).await, Err(Error::Busy));
    // Give the worker time to enter SQLite's busy handler while its first INSERT is blocked.
    tokio::time::sleep(Duration::from_millis(40)).await;
    drop(requests);
    other.execute_batch("ROLLBACK").unwrap();
    let drained = tokio::time::timeout(
        Duration::from_secs(2),
        prompts.permits.clone().acquire_many_owned(4),
    )
    .await
    .unwrap()
    .unwrap();
    drop(drained);
    let listed = serde_json::from_slice::<serde_json::Value>(
        &prompts.response(Operation::List).await.unwrap(),
    )
    .unwrap();
    assert_eq!(
        listed["prompts"].as_array().unwrap().len(),
        4,
        "cancelled INSERT escaped the busy wait"
    );
}

#[tokio::test]
async fn cancelled_busy_delete_cannot_commit_after_unlock() {
    let root = root();
    let prompts = Prompts::open_with_clock(root.path(), || Ok(1_700_000_000.125))
        .await
        .unwrap();
    let other = Connection::open(root.path().join("model_loader.db")).unwrap();
    other.execute_batch("BEGIN IMMEDIATE").unwrap();
    let mut request = Box::pin(prompts.response(Operation::Delete(1)));
    assert!(futures_util::poll!(&mut request).is_pending());
    tokio::time::sleep(Duration::from_millis(40)).await;
    drop(request);
    other.execute_batch("ROLLBACK").unwrap();
    let drained = tokio::time::timeout(
        Duration::from_secs(2),
        prompts.permits.clone().acquire_many_owned(4),
    )
    .await
    .unwrap()
    .unwrap();
    drop(drained);
    let listed = serde_json::from_slice::<serde_json::Value>(
        &prompts.response(Operation::List).await.unwrap(),
    )
    .unwrap();
    assert_eq!(
        listed["prompts"].as_array().unwrap().len(),
        4,
        "cancelled DELETE escaped the busy wait"
    );
}

#[tokio::test]
async fn actual_python_indirect_readers_observe_native_commits_on_same_file() {
    use sha2::{Digest, Sha256};
    let Some(source) = std::env::var_os("NOEVIA_PROMPTS_APP") else {
        assert!(
            std::env::var_os("CI").is_none(),
            "CI must run actual indirect reader qualification"
        );
        return;
    };
    let source = std::path::PathBuf::from(source);
    assert!(
        source.is_absolute(),
        "actual Python source must be absolute"
    );
    let source = source
        .canonicalize()
        .expect("actual Python source must exist");
    let oracle = crate::prompts::tests::oracle();
    let bytes = std::fs::read(source.join("db.py")).unwrap();
    assert_eq!(
        format!("{:x}", Sha256::digest(bytes)),
        oracle["sources"]["db.py"]["sha256"]
    );
    let root = root();
    let prompts = Prompts::open_with_clock(root.path(), || Ok(1_700_000_000.125))
        .await
        .unwrap();
    let mut ids = Vec::new();
    for name in ["a", "Z", "A", "quote' ; DROP TABLE prompts;--"] {
        let result: serde_json::Value = serde_json::from_slice(
            &prompts
                .response(Operation::Add(name.into(), "visible body".into()))
                .await
                .unwrap(),
        )
        .unwrap();
        ids.push(result["id"].as_i64().unwrap());
    }
    const READ: &str = r#"
import importlib.util,json,sys,types
from pathlib import Path
source=Path(sys.argv[1]);root=Path(sys.argv[2])
app=types.ModuleType('app');app.__path__=[str(source)];sys.modules['app']=app
config=types.ModuleType('app.config');config.settings=types.SimpleNamespace(data_dir=root);sys.modules['app.config']=config
spec=importlib.util.spec_from_file_location('app.db',source/'db.py');db=importlib.util.module_from_spec(spec);spec.loader.exec_module(db)
print(json.dumps({'list':db.list_prompts(),'selected':[dict(r) for r in db.prompts_by_ids(json.loads(sys.argv[3]))]}))
"#;
    let read = |ids: &[i64]| {
        let mut command =
            std::process::Command::new(std::env::var_os("PYTHON").expect("actual Python required"));
        command
            .env_clear()
            .env("PYTHONDONTWRITEBYTECODE", "1")
            .args(["-c", READ])
            .arg(&source)
            .arg(root.path())
            .arg(serde_json::to_string(ids).unwrap());
        let output =
            prompts_process::bounded(command, Vec::new(), Duration::from_secs(12)).unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()
    };
    let actual = read(&ids);
    let listed: serde_json::Value =
        serde_json::from_slice(&prompts.response(Operation::List).await.unwrap()).unwrap();
    assert_eq!(actual["list"], listed["prompts"]);
    assert_eq!(
        actual["selected"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["name"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["A", "Z", "a", "quote' ; DROP TABLE prompts;--"]
    );
    assert!(actual["selected"]
        .as_array()
        .unwrap()
        .iter()
        .all(|p| p["body"] == "visible body"));
    prompts.response(Operation::Delete(ids[0])).await.unwrap();
    assert_eq!(read(&ids)["selected"].as_array().unwrap().len(), 3);
}
