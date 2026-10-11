#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
use server_store::{
    bounded::{BoundedStore, BoundedUpkeepWriter, Budget, Fault, QualifiedRoot},
    rusqlite::{types::Value, Connection},
    RustAuth, Store, Writer,
};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
const SCHEMA: &str = include_str!("fixtures/node-schema.sql");
fn db() -> (tempfile::TempDir, Connection) {
    let d = tempfile::tempdir().unwrap();
    let c = Connection::open(server_store::db_path(d.path())).unwrap();
    c.execute_batch(SCHEMA).unwrap();
    c.execute_batch("INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,created_at,updated_at) VALUES('u','A','a','A','member','x','w',1,1); INSERT INTO sessions VALUES('s','u','c',1,1,9,'ua','ip');").unwrap();
    (d, c)
}
fn budget() -> Budget {
    Budget::new(Instant::now() + Duration::from_secs(2), 2_000_000).unwrap()
}
fn seed(c: &Connection, n: usize) {
    for i in 0..n {
        let id = format!("user{i}");
        c.execute("INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,created_at,updated_at) VALUES(?1,?1,?1,?1,'member','x',?1,1,1)",[id]).unwrap();
    }
}
fn auth() -> RustAuth {
    RustAuth::from_env_value(Some("1")).unwrap()
}
#[test]
fn bounded_reads_and_narrow_writer_preserve_ordinary_connections() {
    let (d, c) = db();
    let ordinary = Store::open(d.path()).unwrap();
    let b = budget();
    let root = QualifiedRoot::open(d.path(), &b).unwrap();
    let mut w = BoundedUpkeepWriter::open(&root, auth(), &b).unwrap();
    w.write(|t| t.execute("UPDATE sessions SET last_seen_at=5 WHERE id_hash='s'", []))
        .unwrap();
    w.close().unwrap();
    let mut r = BoundedStore::open(&root, &b).unwrap();
    let seen = r
        .read(|r| {
            r.row(
                "SELECT last_seen_at FROM sessions WHERE id_hash='s'",
                [],
                |r| r.get::<_, i64>(0),
            )
        })
        .unwrap();
    assert_eq!(seen, Some(5), "ROUTING_ASSERT/store/read");
    r.close().unwrap();
    assert_eq!(
        ordinary
            .read(|r| r.row("SELECT length(hex(randomblob(1)))", [], |r| r
                .get::<_, i64>(0)))
            .unwrap(),
        Some(2)
    );
    assert_eq!(
        c.query_row("SELECT last_seen_at FROM sessions", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        5
    );
}
#[test]
fn writer_refuses_other_columns_settings_ddl_and_rolls_back() {
    for sql in [
        "UPDATE users SET role='admin'",
        "UPDATE sessions SET csrf_hash='bad'",
        "UPDATE device_grants SET expires_at=99",
        "INSERT INTO settings VALUES('feature:chatgptOAuth','true')",
        "DELETE FROM device_grants",
        "INSERT INTO audit_events(action,created_at) VALUES('bad',1)",
        "CREATE TABLE bad(x)",
        "ATTACH ':memory:' AS bad",
    ] {
        let (d, c) = db();
        let b = budget();
        let root = QualifiedRoot::open(d.path(), &b).unwrap();
        let mut w = BoundedUpkeepWriter::open(&root, auth(), &b).unwrap();
        let result = w.write(|t| {
            t.execute("UPDATE sessions SET last_seen_at=5", [])?;
            t.execute(sql, [])
        });
        assert!(result.is_err(), "ROUTING_ASSERT/store/writer");
        w.close().unwrap();
        assert_eq!(
            c.query_row("SELECT last_seen_at FROM sessions", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            1
        );
    }
}
#[test]
fn precopy_all_columns_and_rows_before_mapper() {
    for (query, value) in [
        ("SELECT ?1 AS display_name", Value::Text("x".repeat(5000))),
        ("SELECT ?1 AS user_id", Value::Text("x".repeat(1100))),
        ("SELECT ?1 AS csrf_hash", Value::Text("x".repeat(300))),
        ("SELECT ?1 AS disabled_at", Value::Blob(vec![0; 5000])),
    ] {
        let (d, _c) = db();
        let b = budget();
        let root = QualifiedRoot::open(d.path(), &b).unwrap();
        let mut r = BoundedStore::open(&root, &b).unwrap();
        let called = AtomicBool::new(false);
        let result = r.read(|r| {
            r.rows(query, [value], |row| {
                called.store(true, Ordering::Release);
                row.get::<_, Value>(0)
            })
        });
        assert_eq!(
            result.err().map(|e| e.fault),
            Some(Fault::Cap),
            "ROUTING_ASSERT/store/precopy"
        );
        assert!(
            !called.load(Ordering::Acquire),
            "ROUTING_ASSERT/store/before_mapper"
        );
        r.close().unwrap();
    }
    let (d, c) = db();
    seed(&c, 65);
    let b = budget();
    let root = QualifiedRoot::open(d.path(), &b).unwrap();
    let mut r = BoundedStore::open(&root, &b).unwrap();
    let result = r.read(|r| r.rows("SELECT id FROM users", [], |r| r.get::<_, String>(0)));
    assert_eq!(
        result.unwrap_err().fault,
        Fault::Cap,
        "ROUTING_ASSERT/store/rows"
    );
    r.close().unwrap();
}
#[test]
fn cancellation_vm_busy_and_immutable_job_slots() {
    let (d, c) = db();
    seed(&c, 10);
    let b = budget();
    let root = QualifiedRoot::open(d.path(), &b).unwrap();
    let mut r = BoundedStore::open(&root, &b).unwrap();
    b.cancellation().cancel();
    assert_eq!(r.read(|_| Ok(())).unwrap_err().fault, Fault::Cancelled);
    r.close().unwrap();
    let fresh = budget();
    let root = QualifiedRoot::open(d.path(), &fresh).unwrap();
    let mut r = BoundedStore::open(&root, &fresh).unwrap();
    b.cancellation().cancel();
    assert!(
        r.read(|r| r.row("SELECT 1", [], |r| r.get::<_, i64>(0)))
            .is_ok(),
        "ROUTING_ASSERT/store/old_cancel"
    );
    let vm = r.read(|r| {
        r.row(
            "SELECT count(*) FROM users a, users b, users c, users d, users e, users f",
            [],
            |r| r.get::<_, i64>(0),
        )
    });
    assert_eq!(
        vm.err().map(|e| e.fault),
        Some(Fault::Cap),
        "ROUTING_ASSERT/store/vm"
    );
    r.close().unwrap();
    let bb = budget();
    let root = QualifiedRoot::open(d.path(), &bb).unwrap();
    c.execute_batch("BEGIN IMMEDIATE").unwrap();
    let mut w = BoundedUpkeepWriter::open(&root, auth(), &bb).unwrap();
    assert_eq!(
        w.write(|_| Ok(())).unwrap_err().fault,
        Fault::Busy,
        "ROUTING_ASSERT/store/busy"
    );
    w.close().unwrap();
    c.execute_batch("ROLLBACK").unwrap();
}
#[test]
fn root_schema_and_existing_lock_qualification() {
    let (d, c) = db();
    let ordinary = Writer::open(d.path(), auth()).unwrap();
    ordinary
        .write(|_| {
            let b = budget();
            let root = QualifiedRoot::open(d.path(), &b).unwrap();
            let mut r = BoundedStore::open(&root, &b).unwrap();
            r.read(|r| r.row("SELECT count(*) FROM users", [], |r| r.get::<_, i64>(0)))
                .unwrap();
            r.close().unwrap();
            c.busy_timeout(Duration::ZERO).unwrap();
            assert!(
                c.execute_batch("BEGIN IMMEDIATE").is_err(),
                "ROUTING_ASSERT/store/existing_lock"
            );
            Ok(())
        })
        .unwrap();
    c.execute_batch("INSERT INTO schema_migrations VALUES(6,0)")
        .unwrap();
    let b = budget();
    let root = QualifiedRoot::open(d.path(), &b).unwrap();
    let mut r = BoundedStore::open(&root, &b).unwrap();
    assert_eq!(r.read(|_| Ok(())).unwrap_err().fault, Fault::SchemaTooNew);
    r.close().unwrap();
    let empty = tempfile::tempdir().unwrap();
    assert!(QualifiedRoot::open(empty.path(), &budget()).is_err());
    assert!(!server_store::db_path(empty.path()).exists());
}
#[test]
fn cancellation_worker_ownership_and_close() {
    let (d, _c) = db();
    let b = budget();
    let root = QualifiedRoot::open(d.path(), &b).unwrap();
    let cancel = b.cancellation();
    let lease = Arc::new(AtomicBool::new(true));
    let worker_lease = lease.clone();
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(0);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel(0);
    let h = std::thread::spawn(move || {
        let mut r = BoundedStore::open(&root, &b).unwrap();
        ready_tx.send(()).unwrap();
        release_rx.recv().unwrap();
        assert_eq!(r.read(|_| Ok(())).unwrap_err().fault, Fault::Cancelled);
        r.close().unwrap();
        worker_lease.store(false, Ordering::Release);
    });
    ready_rx.recv().unwrap();
    cancel.cancel();
    assert!(
        lease.load(Ordering::Acquire),
        "ROUTING_ASSERT/store/retained_worker"
    );
    release_tx.send(()).unwrap();
    h.join().unwrap();
    assert!(!lease.load(Ordering::Acquire));
}

#[test]
fn schema_caps_null_collation_views_triggers_and_deadline() {
    for change in [
        "INSERT INTO sessions VALUES(NULL,'u','c',1,1,9,'ua','ip')",
        "CREATE TRIGGER bad AFTER UPDATE ON sessions BEGIN UPDATE users SET role='admin'; END",
        "ALTER TABLE users DROP COLUMN credential_epoch",
        "DROP TABLE sessions; CREATE VIEW sessions AS SELECT 1 AS id_hash",
    ] {
        let (d, c) = db();
        c.execute_batch(change).unwrap();
        let b = budget();
        let root = QualifiedRoot::open(d.path(), &b).unwrap();
        let mut r = BoundedStore::open(&root, &b).unwrap();
        assert!(
            r.read(|_| Ok(())).is_err(),
            "ROUTING_ASSERT/store/schema_negative"
        );
        r.close().unwrap();
    }
    for schema in [SCHEMA.replace(
        "id_hash TEXT PRIMARY KEY",
        "id_hash TEXT COLLATE NOCASE PRIMARY KEY",
    )] {
        if schema == SCHEMA {
            continue;
        }
        let d = tempfile::tempdir().unwrap();
        let c = Connection::open(server_store::db_path(d.path())).unwrap();
        c.execute_batch(&schema).unwrap();
        let b = budget();
        let root = QualifiedRoot::open(d.path(), &b).unwrap();
        let mut r = BoundedStore::open(&root, &b).unwrap();
        assert!(
            r.read(|_| Ok(())).is_err(),
            "ROUTING_ASSERT/store/collation"
        );
        r.close().unwrap();
    }
    let (d, _c) = db();
    let b = Budget::new(Instant::now() + Duration::from_millis(1), 2_000_000).unwrap();
    std::thread::sleep(Duration::from_millis(2));
    assert_eq!(
        QualifiedRoot::open(d.path(), &b).err().unwrap().fault,
        Fault::Deadline
    );
    let b = budget();
    b.cancellation().cancel();
    assert_eq!(
        QualifiedRoot::open(d.path(), &b).err().unwrap().fault,
        Fault::Cancelled
    );
    let link = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(
        server_store::db_path(d.path()),
        server_store::db_path(link.path()),
    )
    .unwrap();
    assert!(QualifiedRoot::open(link.path(), &budget()).is_err());
}

// Qualification only: exact source/location/body/footer certificate, bounded child wrapper.
// Explicit capture fields keep source binding and raw process evidence independently visible.
#[allow(clippy::too_many_arguments)]
fn certificate(
    source: &str,
    test: &str,
    anchor: &str,
    tag: &str,
    equal: bool,
    status: Option<i32>,
    out: &[u8],
    err: &[u8],
) -> Result<serde_json::Value, String> {
    if status != Some(101) || source.len() > 1048576 || out.len() > 8388608 || err.len() > 65536 {
        return Err("capture/status cap".into());
    }
    if source.matches(anchor).count() != 1 {
        return Err("unique complete assertion binding".into());
    }
    let pos = source.find(anchor).ok_or("anchor")?;
    let before = &source[..pos];
    if !before
        .rsplit_once("fn ")
        .is_some_and(|(_, rest)| rest.starts_with(&format!("{test}(")))
    {
        return Err("test owner".into());
    }
    let line = before.bytes().filter(|b| *b == b'\n').count() + 1;
    let column = before.rsplit('\n').next().ok_or("column")?.chars().count() + 1;
    let out = std::str::from_utf8(out).map_err(|_| "stdout UTF8")?;
    let err = std::str::from_utf8(err).map_err(|_| "stderr UTF8")?;
    let selected = format!("test {test} ... FAILED");
    let inventory = format!("    {test}");
    let filtered = source
        .lines()
        .filter(|line| line.trim() == "#[test]")
        .count()
        .checked_sub(1)
        .ok_or("test count")?;
    let summary=format!("test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; {filtered} filtered out; finished in ");
    let expected_prefix =
        format!("running 1 test\n{selected}\n\nfailures:\n\nfailures:\n{inventory}\n\n{summary}");
    let tail = out
        .trim_start_matches('\n')
        .strip_prefix(&expected_prefix)
        .ok_or("exact test/failure inventory")?;
    let duration = tail
        .trim_end_matches('\n')
        .strip_suffix('s')
        .ok_or("duration")?;
    if !duration.split_once('.').is_some_and(|(a, b)| {
        !a.is_empty() && !b.is_empty() && a.bytes().chain(b.bytes()).all(|c| c.is_ascii_digit())
    }) {
        return Err("duration syntax".into());
    }
    let lines: Vec<_> = err.lines().collect();
    let panics: Vec<_> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| l.starts_with("thread '"))
        .collect();
    if panics.len() != 1 || err.matches("panicked at").count() != 1 {
        return Err("sole panic".into());
    }
    let (at, h) = *panics.first().ok_or("panic")?;
    let prefix = format!("thread '{test}' (");
    let h = h.strip_prefix(&prefix).ok_or("panic test")?;
    let (pid, location) = h.split_once(") panicked at ").ok_or("panic grammar")?;
    if pid.is_empty()
        || !pid.bytes().all(|b| b.is_ascii_digit())
        || location != format!("crates/server-store/tests/bounded.rs:{line}:{column}:")
    {
        return Err("bound source location".into());
    }
    let prefix_text = lines[..at].join("\n");
    let warnings = r#"warning: fields `rows` and `bytes` are never read
  --> crates/server-store/src/bounded.rs:83:5
   |
78 | struct State {
   |        ----- fields in this struct
...
83 |     rows: AtomicUsize,
   |     ^^^^
84 |     bytes: AtomicUsize,
   |     ^^^^^
   |
   = note: `#[warn(dead_code)]` (part of `#[warn(unused)]`) on by default

warning: methods `fail_cap` and `row_check` are never used
   --> crates/server-store/src/bounded.rs:143:8
    |
104 | impl Budget {
    | ----------- methods in this implementation
...
143 |     fn fail_cap(&self) -> StoreError {
    |        ^^^^^^^^
...
151 |     pub(super) fn row_check(&self, row: &Row<'_>) -> Result<(), StoreError> {
    |                   ^^^^^^^^^

warning: `server-store` (lib) generated 2 warnings"#;
    let clean_prefix = if tag == "ROUTING_ASSERT/store/precopy"
        && test == "precopy_all_columns_and_rows_before_mapper"
        && prefix_text.matches(warnings).count() == 1
    {
        prefix_text.replacen(warnings, "", 1)
    } else {
        prefix_text
    };
    if clean_prefix.lines().any(|l| {
        !l.is_empty()
            && !l.starts_with("   Compiling ")
            && !l.starts_with("    Finished ")
            && !l.starts_with("     Running ")
    }) {
        return Err("setup/compiler prefix".into());
    }
    let message = if equal {
        format!("assertion `left == right` failed: {tag}")
    } else {
        tag.to_owned()
    };
    if lines.get(at + 1).copied() != Some(message.as_str())
        || err.matches("ROUTING_ASSERT/").count() != 1
    {
        return Err("exact assertion payload".into());
    }
    let footer = "error: test failed, to rerun pass `-p server-store --test bounded`";
    let errors: Vec<_> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| l.starts_with("error:"))
        .collect();
    if errors.len() != 1 {
        return Err("sole cargo footer".into());
    }
    let (end, e) = *errors.first().ok_or("cargo footer")?;
    if *e != footer || end <= at + 1 || lines.iter().skip(end + 1).any(|l| !l.is_empty()) {
        return Err("terminal cargo footer/order".into());
    }
    let body: Vec<_> = lines
        .iter()
        .skip(at + 2)
        .take(end - at - 2)
        .filter(|l| !l.is_empty() && !l.starts_with("note: run with `RUST_BACKTRACE=1`"))
        .copied()
        .collect();
    if equal {
        if body.len() != 2
            || !body.first().is_some_and(|l| l.starts_with("  left: "))
            || !body.get(1).is_some_and(|l| l.starts_with(" right: "))
        {
            return Err("equal assertion body".into());
        }
    } else if !body.is_empty() {
        return Err("boolean body".into());
    }
    Ok(
        serde_json::json!({"test":test,"line":line,"column":column,"tag":tag,"exit":101,"source_bytes":source.len()}),
    )
}
#[test]
#[ignore = "explicit finite mutant matrix only"]
fn focused_mutant_certificates() -> Result<(), String> {
    use std::{path::PathBuf, process::Command};
    struct Restore(PathBuf, Vec<u8>);
    impl Drop for Restore {
        fn drop(&mut self) {
            if std::fs::write(&self.0, &self.1).is_err() {
                eprintln!("MUTATION RESTORE FAILED");
            }
        }
    }
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let dir =
        PathBuf::from(std::env::var("NOEVIA_BOUNDED_EVIDENCE").map_err(|_| "evidence directory")?);
    let resume = std::env::var_os("NOEVIA_BOUNDED_RESUME").is_some();
    if !resume {
        std::fs::create_dir(&dir).map_err(|_| "new evidence directory")?;
    }
    let source = std::fs::read_to_string(root.join("crates/server-store/tests/bounded.rs"))
        .map_err(|_| "test source")?;
    if source.len() > 1048576 {
        return Err("source cap".into());
    }
    let matrix=[
        ("precopy","crates/server-store/src/lib.rs","budget.row_check(row)?;","budget.store_check()?;","precopy_all_columns_and_rows_before_mapper","assert_eq!(\n            result.err().map(|e| e.fault),\n            Some(Fault::Cap),\n            \"ROUTING_ASSERT/store/precopy\"\n        );","ROUTING_ASSERT/store/precopy",true,2),
        ("writer","crates/server-store/src/bounded.rs","writer\n                && matches!(","writer\n                || matches!(","writer_refuses_other_columns_settings_ddl_and_rolls_back","assert!(result.is_err(), \"ROUTING_ASSERT/store/writer\");","ROUTING_ASSERT/store/writer",false,1),
        ("busy","crates/server-store/src/bounded.rs","c.busy_timeout(Duration::ZERO)","c.busy_timeout(Duration::from_secs(5))","cancellation_vm_busy_and_immutable_job_slots","assert_eq!(\n        w.write(|_| Ok(())).unwrap_err().fault,\n        Fault::Busy,\n        \"ROUTING_ASSERT/store/busy\"\n    );","ROUTING_ASSERT/store/busy",true,1),
        ("vm","crates/server-store/src/bounded.rs","progress.0.ticks.fetch_add(1, Ordering::AcqRel);","progress.0.ticks.load(Ordering::Acquire);","cancellation_vm_busy_and_immutable_job_slots","assert_eq!(\n        vm.err().map(|e| e.fault),\n        Some(Fault::Cap),\n        \"ROUTING_ASSERT/store/vm\"\n    );","ROUTING_ASSERT/store/vm",true,1),
    ];
    let mut proofs = Vec::new();
    for (tag, file, old, new, test, anchor, payload, equal, count) in matrix {
        let path = root.join(file);
        let original = std::fs::read(&path).map_err(|_| "production source")?;
        if original.len() > 1048576 {
            return Err("production source cap".into());
        }
        let text = std::str::from_utf8(&original).map_err(|_| "source UTF8")?;
        if text.matches(old).count() != count {
            return Err(format!("explicit mutation count {tag}"));
        }
        let changed = text.replace(old, new);
        let restore = Restore(path.clone(), original.clone());
        std::fs::write(&path, &changed).map_err(|_| "mutation write")?;
        let (capture_source, status, stdout, stderr) = if resume
            && dir.join(format!("{tag}.status")).exists()
        {
            if std::fs::read(dir.join(format!("{tag}.source.rs"))).map_err(|_| "retained source")?
                != changed.as_bytes()
            {
                return Err("retained mutation source mismatch".into());
            }
            let captured = std::fs::read_to_string(dir.join(format!("{tag}.assertions.rs")))
                .map_err(|_| "retained assertions")?;
            let status = std::fs::read_to_string(dir.join(format!("{tag}.status")))
                .map_err(|_| "retained status")?;
            if status != "Some(101)" {
                return Err("retained status".into());
            }
            (
                captured,
                Some(101),
                std::fs::read(dir.join(format!("{tag}.stdout"))).map_err(|_| "retained stdout")?,
                std::fs::read(dir.join(format!("{tag}.stderr"))).map_err(|_| "retained stderr")?,
            )
        } else {
            let output = Command::new("/tmp/m6-status-bounded-command")
            .args([
                "/Users/sebastiandalton/.rustup/toolchains/1.99.0-aarch64-apple-darwin/bin/cargo",
                "test",
                "--offline",
                "-p",
                "server-store",
                "--test",
                "bounded",
                "--target-dir",
                "/tmp/m6-bounded-store-mutant-target",
                "--",
                test,
                "--exact",
                "--nocapture",
            ])
            .current_dir(&root)
            .output()
            .map_err(|_| "bounded child wrapper")?;
            for (suffix, bytes) in [
                ("source.rs", changed.as_bytes()),
                ("assertions.rs", source.as_bytes()),
                ("stdout", output.stdout.as_slice()),
                ("stderr", output.stderr.as_slice()),
            ] {
                std::fs::write(dir.join(format!("{tag}.{suffix}")), bytes)
                    .map_err(|_| "capture write")?;
            }
            std::fs::write(
                dir.join(format!("{tag}.status")),
                format!("{:?}", output.status.code()),
            )
            .map_err(|_| "status capture")?;
            (
                source.clone(),
                output.status.code(),
                output.stdout,
                output.stderr,
            )
        };
        let proof = certificate(
            &capture_source,
            test,
            anchor,
            payload,
            equal,
            status,
            &stdout,
            &stderr,
        )?;
        for forged in [
            format!("{}trailing record\n", String::from_utf8_lossy(&stderr)),
            String::from_utf8_lossy(&stderr).replace(
                "panicked at crates/server-store/tests/bounded.rs:",
                "panicked at wrong.rs:",
            ),
            format!(
                "error[E0308]: compiler\n{}",
                String::from_utf8_lossy(&stderr)
            ),
        ] {
            if certificate(
                &capture_source,
                test,
                anchor,
                payload,
                equal,
                Some(101),
                &stdout,
                forged.as_bytes(),
            )
            .is_ok()
            {
                return Err("forged certificate accepted".into());
            }
        }
        proofs.push(serde_json::json!({"tag":tag,"file":file,"witness":proof}));
        drop(restore);
        if std::fs::read(&path).map_err(|_| "restore read")? != original {
            return Err("restore byte mismatch".into());
        }
    }
    std::fs::write(
        dir.join("certificate.json"),
        serde_json::to_vec_pretty(&proofs).map_err(|_| "certificate encoding")?,
    )
    .map_err(|_| "certificate write")?;
    Ok(())
}

#[test]
#[ignore = "explicit retained local evidence controls only"]
fn retained_certificate_forged_controls() -> Result<(), String> {
    let dir = std::path::Path::new("/tmp/m6-bounded-store-stage1-mutants-run1");
    let matrix = [
        ("precopy", "precopy_all_columns_and_rows_before_mapper", "assert_eq!(\n            result.err().map(|e| e.fault),\n            Some(Fault::Cap),\n            \"ROUTING_ASSERT/store/precopy\"\n        );", "ROUTING_ASSERT/store/precopy", true),
        ("writer", "writer_refuses_other_columns_settings_ddl_and_rolls_back", "assert!(result.is_err(), \"ROUTING_ASSERT/store/writer\");", "ROUTING_ASSERT/store/writer", false),
        ("busy", "cancellation_vm_busy_and_immutable_job_slots", "assert_eq!(\n        w.write(|_| Ok(())).unwrap_err().fault,\n        Fault::Busy,\n        \"ROUTING_ASSERT/store/busy\"\n    );", "ROUTING_ASSERT/store/busy", true),
        ("vm", "cancellation_vm_busy_and_immutable_job_slots", "assert_eq!(\n        vm.err().map(|e| e.fault),\n        Some(Fault::Cap),\n        \"ROUTING_ASSERT/store/vm\"\n    );", "ROUTING_ASSERT/store/vm", true),
    ];
    for (name, test, anchor, tag, equal) in matrix {
        let source = std::fs::read_to_string(dir.join(format!("{name}.assertions.rs")))
            .map_err(|_| "source")?;
        let out = std::fs::read(dir.join(format!("{name}.stdout"))).map_err(|_| "stdout")?;
        let err =
            std::fs::read_to_string(dir.join(format!("{name}.stderr"))).map_err(|_| "stderr")?;
        certificate(
            &source,
            test,
            anchor,
            tag,
            equal,
            Some(101),
            &out,
            err.as_bytes(),
        )?;
        let footer = "error: test failed, to rerun pass `-p server-store --test bounded`";
        for forged in [
            format!("{err}trailing record\n"),
            err.replace(
                "panicked at crates/server-store/tests/bounded.rs:",
                "panicked at wrong.rs:",
            ),
            err.replace(&format!("thread '{test}'"), "thread 'wrong_test'"),
            err.replace(tag, "ROUTING_ASSERT/wrong/policy"),
            err.replace("panicked at", "unwrap failed at"),
            format!("setup failed\n{err}"),
            format!("error[E0308]: compile failed\n{err}"),
            format!("{footer}\n{}", err.replace(footer, "")),
            format!("{err}thread 'extra' (1) panicked at wrong.rs:1:1:\n"),
            err.replace(tag, "Deadline"),
        ] {
            if certificate(
                &source,
                test,
                anchor,
                tag,
                equal,
                Some(101),
                &out,
                forged.as_bytes(),
            )
            .is_ok()
            {
                return Err(format!("accepted forged stderr {name}"));
            }
        }
        for forged in [
            String::from_utf8_lossy(&out).replace("running 1 test", "running 2 tests"),
            String::from_utf8_lossy(&out).replace("1 failed", "2 failed"),
            String::from_utf8_lossy(&out).replace(test, "wrong_test"),
        ] {
            if certificate(
                &source,
                test,
                anchor,
                tag,
                equal,
                Some(101),
                forged.as_bytes(),
                err.as_bytes(),
            )
            .is_ok()
            {
                return Err("accepted forged inventory".into());
            }
        }
        for status in [None, Some(0), Some(1), Some(137)] {
            if certificate(
                &source,
                test,
                anchor,
                tag,
                equal,
                status,
                &out,
                err.as_bytes(),
            )
            .is_ok()
            {
                return Err("accepted wrong status".into());
            }
        }
        for forged in [
            source.replace(anchor, "/* missing binding */"),
            format!("{source}\n{anchor}"),
        ] {
            if certificate(
                &forged,
                test,
                anchor,
                tag,
                equal,
                Some(101),
                &out,
                err.as_bytes(),
            )
            .is_ok()
            {
                return Err("accepted source binding forgery".into());
            }
        }
        if certificate(
            &source,
            test,
            anchor,
            tag,
            equal,
            Some(101),
            &vec![b'x'; 8388609],
            err.as_bytes(),
        )
        .is_ok()
            || certificate(
                &source,
                test,
                anchor,
                tag,
                equal,
                Some(101),
                &out,
                &vec![b'x'; 65537],
            )
            .is_ok()
        {
            return Err("accepted cap violation".into());
        }
    }
    Ok(())
}
