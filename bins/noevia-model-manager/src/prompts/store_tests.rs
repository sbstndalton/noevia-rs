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
/// `Store::open` takes a non-blocking flock on `model_loader.db.prompts.lock`. The lock belongs to
/// the open file description, so while another test thread in this binary is between `fork` and
/// `exec` of a spawned process (`prompts_process::bounded`), that child still holds a copy of a
/// just-dropped store's lock descriptor and the next open answers `Busy` for a few microseconds
/// (noevia#1939). Retry only that transient answer, with a bound, so a store that is genuinely
/// held (the lock tests open it directly) still fails.
fn open_retrying(root: &Path) -> Result<Store> {
    const MAX_ATTEMPTS: u32 = 200;
    let mut attempts = 0;
    loop {
        attempts += 1;
        match Store::open(root) {
            Err(Error::Busy) if attempts < MAX_ATTEMPTS => {
                std::thread::sleep(Duration::from_millis(10));
            }
            other => return other,
        }
    }
}
fn open(root: &Path) -> Store {
    let mut s = open_retrying(root).unwrap();
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
fn open_retrying_waits_out_a_transient_lock_holder_but_not_a_real_one() {
    let root = root();
    let held = open(root.path());
    // Released after a short delay, like a forked child finishing its exec: the retry succeeds.
    let releaser = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        drop(held);
    });
    assert!(open_retrying(root.path()).is_ok());
    releaser.join().unwrap();
    // Still held: `Store::open` itself keeps answering Busy (the lock tests rely on it).
    let _held = open(root.path());
    assert!(matches!(Store::open(root.path()), Err(Error::Busy)));
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
            matches!(open_retrying(root.path()), Err(Error::Store)),
            "PROMPTS_ASSERT:schema-autoincrement {extra}"
        );
    }
    let root = root();
    let db = Connection::open(root.path().join("model_loader.db")).unwrap();
    db.execute_batch("CREATE TABLE prompts(id INTEGER PRIMARY KEY /* ignored */ AUTOINCREMENT,name TEXT NOT NULL,body TEXT NOT NULL,created_at REAL NOT NULL,extra TEXT DEFAULT 'AUTOINCREMENT')").unwrap();
    drop(db);
    assert!(open_retrying(root.path()).is_ok());
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
    assert!(matches!(open_retrying(&bad), Err(Error::Store)));
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
fn sqlite_companion_lifecycle_allows_checkpoint_removal_and_recreation() {
    let root = root();
    let mut s = open(root.path());
    let added = run(&mut s, Operation::Add("before".into(), "kept".into())).unwrap();
    let identities = s.identities.clone();
    // Keep old inodes allocated so recreation cannot merely reuse their numbers.
    let old_wal = File::open(root.path().join("model_loader.db-wal")).unwrap();
    let old_shm = File::open(root.path().join("model_loader.db-shm")).unwrap();
    // SQLite itself checkpoints and removes companions when leaving WAL mode.
    // No active SQLite companion is manually unlinked or replaced by this test.
    s.db.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); PRAGMA journal_mode=DELETE;")
        .unwrap();
    for suffix in ["-wal", "-shm"] {
        assert_eq!(
            std::fs::symlink_metadata(root.path().join(format!("model_loader.db{suffix}")))
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::NotFound
        );
    }
    assert_eq!(s.check(), Ok(()));
    assert_eq!(
        run(&mut s, Operation::List).unwrap()["prompts"],
        added["prompts"]
    );
    s.db.execute_batch("PRAGMA journal_mode=WAL;").unwrap();
    let after = run(&mut s, Operation::Add("after".into(), "visible".into())).unwrap();
    for (suffix, old) in [("-wal", old_wal), ("-shm", old_shm)] {
        let new = safe_file(&root.path().join(format!("model_loader.db{suffix}")), false)
            .unwrap()
            .unwrap();
        let old = old.metadata().unwrap();
        assert_ne!((new.dev(), new.ino()), (old.dev(), old.ino()));
    }
    assert_eq!(s.identities, identities);
    assert_eq!(after["prompts"].as_array().unwrap().len(), 2);
    assert_eq!(
        run(&mut s, Operation::Delete(added["id"].as_i64().unwrap())).unwrap()["prompts"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn sqlite_companion_lifecycle_rejects_unsafe_present_files_before_write() {
    use std::os::unix::fs::PermissionsExt;
    for suffix in ["-wal", "-shm"] {
        for kind in ["symlink", "directory", "hardlink", "writable"] {
            let root = root();
            let mut s = open(root.path());
            let path = root.path().join(format!("model_loader.db{suffix}"));
            let held = root.path().join("held-companion");
            std::fs::rename(&path, &held).unwrap();
            let outside = root.path().join("synthetic-other");
            std::fs::write(&outside, "").unwrap();
            match kind {
                "symlink" => std::os::unix::fs::symlink(&outside, &path).unwrap(),
                "directory" => std::fs::create_dir(&path).unwrap(),
                "hardlink" => std::fs::hard_link(&outside, &path).unwrap(),
                "writable" => {
                    std::fs::write(&path, "").unwrap();
                    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o620))
                        .unwrap();
                }
                _ => unreachable!(),
            }
            assert_eq!(s.check(), Err(Error::Store), "{suffix} {kind}");
            assert_eq!(
                run(&mut s, Operation::Add("never".into(), "written".into())),
                Err(Error::Store),
                "{suffix} {kind}"
            );
            // Restore the SQLite-owned companion before any SQL or connection drop.
            if kind == "directory" {
                std::fs::remove_dir(&path).unwrap();
            } else {
                std::fs::remove_file(&path).unwrap();
            }
            std::fs::rename(&held, &path).unwrap();
            assert!(run(&mut s, Operation::List).unwrap()["prompts"]
                .as_array()
                .unwrap()
                .is_empty());
        }
    }
}

#[test]
fn sqlite_companion_lifecycle_keeps_root_database_and_lock_identity_strict() {
    for name in ["model_loader.db", "model_loader.db.prompts.lock"] {
        let root = root();
        let mut s = open(root.path());
        let path = root.path().join(name);
        let held = root.path().join("held-core-file");
        std::fs::rename(&path, &held).unwrap();
        std::fs::write(&path, "").unwrap();
        assert_eq!(run(&mut s, Operation::List), Err(Error::Store), "{name}");
        std::fs::remove_file(&path).unwrap();
        std::fs::rename(&held, &path).unwrap();
        assert_eq!(s.check(), Ok(()));
    }
    let outer = root();
    let path = outer.path().join("stable-root");
    std::fs::create_dir(&path).unwrap();
    let mut s = open(&path);
    let held = outer.path().join("held-root");
    std::fs::rename(&path, &held).unwrap();
    std::fs::create_dir(&path).unwrap();
    assert_eq!(run(&mut s, Operation::List), Err(Error::Store));
    std::fs::remove_dir(&path).unwrap();
    std::fs::rename(&held, &path).unwrap();
    assert_eq!(s.check(), Ok(()));
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
        assert!(matches!(open_retrying(root.path()), Err(Error::Store)));
    }
    let root = root();
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o777)).unwrap();
    assert!(matches!(open_retrying(root.path()), Err(Error::Store)));
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::create_dir(root.path().join("model_loader.db")).unwrap();
    assert!(matches!(open_retrying(root.path()), Err(Error::Store)));
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
    fn snapshot(root: &Path, stage: &str) {
        for name in [
            ".",
            "model_loader.db",
            "model_loader.db-wal",
            "model_loader.db-shm",
            "model_loader.db.prompts.lock",
        ] {
            match std::fs::symlink_metadata(root.join(name)) {
                Ok(m) => eprintln!(
                    "prompts reader {stage} {name}: dev={} ino={} uid={} mode={:o} nlink={} len={} regular={} directory={} symlink={}",
                    m.dev(), m.ino(), m.uid(), m.mode(), m.nlink(), m.len(),
                    m.is_file(), m.is_dir(), m.file_type().is_symlink()
                ),
                Err(e) => eprintln!("prompts reader {stage} {name}: kind={:?}", e.kind()),
            }
        }
    }
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
import importlib.util,json,sqlite3,sys,types
from pathlib import Path
source=Path(sys.argv[1]);root=Path(sys.argv[2])
app=types.ModuleType('app');app.__path__=[str(source)];sys.modules['app']=app
config=types.ModuleType('app.config');config.settings=types.SimpleNamespace(data_dir=root);sys.modules['app.config']=config
spec=importlib.util.spec_from_file_location('app.db',source/'db.py');db=importlib.util.module_from_spec(spec);spec.loader.exec_module(db)
print(json.dumps({'list':db.list_prompts(),'selected':[dict(r) for r in db.prompts_by_ids(json.loads(sys.argv[3]))], 'runtime':{'python':sys.version.split()[0],'executable':sys.executable,'sqlite':sqlite3.sqlite_version}}))
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
    eprintln!("prompts reader native SQLite={}", rusqlite::version());
    snapshot(root.path(), "before-python");
    let python_start = Instant::now();
    let actual = read(&ids);
    eprintln!(
        "prompts reader actual runtime={} elapsed_ms={}",
        actual["runtime"],
        python_start.elapsed().as_millis()
    );
    snapshot(root.path(), "after-python");
    let native_start = Instant::now();
    let native = prompts.response(Operation::List).await;
    eprintln!(
        "prompts reader native list success={} elapsed_ms={}",
        native.is_ok(),
        native_start.elapsed().as_millis()
    );
    snapshot(root.path(), "after-native-list");
    let listed: serde_json::Value = serde_json::from_slice(&native.unwrap()).unwrap();
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

// Compile the probe on author Mac as well; execute it only on qualified Linux.
#[cfg_attr(target_os = "linux", test)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn sqlite_locks_survive_raw_startup_verification() {
    let Some(python) = std::env::var_os("PYTHON") else {
        assert!(
            std::env::var_os("CI").is_none(),
            "CI requires Python lock probe"
        );
        return;
    };
    let root = root();
    let s = open(root.path());
    const PROBE: &str = r#"
import errno,fcntl,os,sys
with open(sys.argv[1],'r+b') as f:
    try:
        fcntl.lockf(f,fcntl.LOCK_EX|fcntl.LOCK_NB,510,1073741826,os.SEEK_SET)
    except OSError as e:
        if e.errno not in (errno.EACCES,errno.EAGAIN): raise
        print('blocked')
    else:
        print('acquired')
"#;
    let mut command = std::process::Command::new(python);
    command.env_clear().args(["-c", PROBE]).arg(&s.path);
    let output = prompts_process::bounded(command, Vec::new(), Duration::from_secs(12)).unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    // A separate process cannot acquire SQLite's main-DB shared-byte range
    // exclusively while this native WAL connection is alive. Its close cannot
    // affect native process locks. A raw startup close would make this succeed.
    assert_eq!(output.stdout, b"blocked\n");
    assert_eq!(s.check(), Ok(()));
}

#[tokio::test]
async fn actual_python_concurrent_reader_snapshot_and_native_delete_visibility() {
    use sha2::{Digest, Sha256};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let Some(source) = std::env::var_os("NOEVIA_PROMPTS_APP") else {
        assert!(
            std::env::var_os("CI").is_none(),
            "CI requires actual concurrent reader"
        );
        return;
    };
    let source = PathBuf::from(source);
    assert!(source.is_absolute());
    let source = source.canonicalize().unwrap();
    assert_eq!(
        format!(
            "{:x}",
            Sha256::digest(std::fs::read(source.join("db.py")).unwrap())
        ),
        crate::prompts::tests::oracle()["sources"]["db.py"]["sha256"]
    );
    let root = root();
    let prompts = Prompts::open_with_clock(root.path(), || Ok(1_700_000_000.125))
        .await
        .unwrap();
    let added: serde_json::Value = serde_json::from_slice(
        &prompts
            .response(Operation::Add(
                "held snapshot".into(),
                "visible body".into(),
            ))
            .await
            .unwrap(),
    )
    .unwrap();
    let id = added["id"].as_i64().unwrap();
    let socket = root.path().join("reader.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    const READ: &str = r#"
import importlib.util,json,socket,sys,types
from pathlib import Path
source=Path(sys.argv[1]);root=Path(sys.argv[2]);pid=int(sys.argv[4])
app=types.ModuleType('app');app.__path__=[str(source)];sys.modules['app']=app
config=types.ModuleType('app.config');config.settings=types.SimpleNamespace(data_dir=root);sys.modules['app.config']=config
spec=importlib.util.spec_from_file_location('app.db',source/'db.py');db=importlib.util.module_from_spec(spec);spec.loader.exec_module(db)
c=db._conn();c.execute('BEGIN')
before=[dict(r) for r in c.execute('SELECT * FROM prompts WHERE id=?',[pid])]
with socket.socket(socket.AF_UNIX) as peer:
    peer.settimeout(7);peer.connect(sys.argv[3]);peer.sendall(b'R')
    if peer.recv(1)!=b'C': raise RuntimeError('reader handshake failed')
    held=[dict(r) for r in c.execute('SELECT * FROM prompts WHERE id=?',[pid])]
c.rollback();c.close()
print(json.dumps({'before':before,'held':held,'after':[dict(r) for r in db.prompts_by_ids([pid])],'list':db.list_prompts()}))
"#;
    let mut command =
        std::process::Command::new(std::env::var_os("PYTHON").expect("actual Python required"));
    command
        .env_clear()
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .args(["-c", READ])
        .arg(&source)
        .arg(root.path())
        .arg(&socket)
        .arg(id.to_string());
    let reader = tokio::task::spawn_blocking(move || {
        prompts_process::bounded(command, Vec::new(), Duration::from_secs(12))
    });
    // Collect the handshake result before asserting, then always join the owned
    // bounded subprocess. Dropping the socket on failure releases the reader;
    // the helper's finite deadline handles failures before it connects.
    let exchange: std::result::Result<serde_json::Value, String> = async {
        let (mut peer, _) = tokio::time::timeout(Duration::from_secs(3), listener.accept())
            .await
            .map_err(|_| "reader accept deadline")?
            .map_err(|_| "reader accept failed")?;
        let mut ready = [0];
        tokio::time::timeout(Duration::from_secs(3), peer.read_exact(&mut ready))
            .await
            .map_err(|_| "reader ready deadline")?
            .map_err(|_| "reader ready failed")?;
        if ready != *b"R" {
            return Err("reader ready marker".into());
        }
        let deleted = prompts
            .response(Operation::Delete(id))
            .await
            .map_err(|_| "native delete failed")?;
        tokio::time::timeout(Duration::from_secs(3), peer.write_all(b"C"))
            .await
            .map_err(|_| "reader release deadline")?
            .map_err(|_| "reader release failed")?;
        serde_json::from_slice(&deleted).map_err(|_| "native readback invalid".into())
    }
    .await;
    let output = reader.await.unwrap().unwrap();
    let deleted = exchange.unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let actual: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(actual["before"].as_array().unwrap().len(), 1);
    assert_eq!(actual["held"], actual["before"]);
    assert!(actual["after"].as_array().unwrap().is_empty());
    assert_eq!(actual["list"], deleted["prompts"]);
    assert_eq!(actual["before"][0]["name"], "held snapshot");
    assert_eq!(actual["before"][0]["body"], "visible body");
    let listed: serde_json::Value =
        serde_json::from_slice(&prompts.response(Operation::List).await.unwrap()).unwrap();
    assert_eq!(listed, deleted);
}

#[test]
fn sticky_temporary_parent_requires_private_owned_prompt_root() {
    use std::os::unix::fs::PermissionsExt;
    // Canonical /tmp is /private/tmp on author Mac, /tmp on qualified Linux.
    let parent = std::fs::canonicalize("/tmp").unwrap();
    let metadata = std::fs::symlink_metadata(&parent).unwrap();
    assert!(metadata.is_dir());
    assert_eq!(metadata.uid(), 0);
    assert_ne!(metadata.mode() & 0o1000, 0);
    assert_ne!(metadata.mode() & 0o022, 0);
    let root = tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir_in(parent)
        .unwrap();
    assert_eq!(
        std::fs::symlink_metadata(root.path()).unwrap().uid(),
        rustix::process::geteuid().as_raw()
    );
    // Explicit permissions make the negative independent of ambient umask.
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(matches!(open_retrying(root.path()), Err(Error::Store)));
    assert!(!root.path().join("model_loader.db").exists());
    assert!(!root.path().join("model_loader.db.prompts.lock").exists());
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut store = open(root.path());
    store.seed().unwrap();
    assert_eq!(
        run(&mut store, Operation::List).unwrap()["prompts"]
            .as_array()
            .unwrap()
            .len(),
        4
    );
}
