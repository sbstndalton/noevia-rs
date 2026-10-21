#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
use super::*;
use crate::prompts::store::{Store, OPERATION_DEADLINE};
use rusqlite::{types::ValueRef, Connection};
use sha2::{Digest, Sha256};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Instant,
};

fn private_root() -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir_in("/tmp").unwrap();
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    root
}
fn fixture() -> Value {
    let oracle: Value = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/db-init-python.v1.json"
    ))
    .unwrap();
    assert_eq!(
        oracle["source_sha256"],
        "6733da40e0ade14cf64ffed68814aa5dba5cb2d48e9bbeea91d2691c1186929a"
    );
    assert_eq!(
        oracle["services_ref"],
        "eefbcbaeebd3f806c62dd5aea48036da8b20bfbf"
    );
    assert_eq!(
        oracle["generator_sha256"],
        format!(
            "{:x}",
            Sha256::digest(include_bytes!(
                "../../../../../../tools/gen-db-init-oracle.py"
            ))
        )
    );
    assert_eq!(
        oracle["cases"][0]["schema"],
        serde_json::from_str::<Value>(EXPECTED).unwrap()
    );
    oracle
}
fn tables(db: &Connection) -> Vec<String> {
    db.prepare("SELECT name FROM sqlite_schema WHERE type='table' ORDER BY name")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}
fn data(db: &Connection) -> Value {
    let mut out = serde_json::Map::new();
    for table in tables(db) {
        let mut statement = db
            .prepare(&format!(r#"SELECT * FROM "{table}" ORDER BY rowid"#))
            .unwrap();
        let count = statement.column_count();
        let rows: Vec<Vec<Value>> = statement
            .query_map([], |row| {
                (0..count)
                    .map(|index| {
                        Ok(match row.get_ref(index)? {
                            ValueRef::Null => Value::Null,
                            ValueRef::Integer(value) => json!(value),
                            ValueRef::Real(value) => json!(value),
                            ValueRef::Text(value) => json!(String::from_utf8_lossy(value)),
                            ValueRef::Blob(_) => panic!("oracle uses only ordinary SQL values"),
                        })
                    })
                    .collect()
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        out.insert(table, json!(rows));
    }
    Value::Object(out)
}
#[test]
fn actual_python_fresh_legacy_repeated_schema_and_values_match() {
    for case in fixture()["cases"].as_array().unwrap() {
        let root = private_root();
        {
            let db = Connection::open(root.path().join("model_loader.db")).unwrap();
            db.execute_batch(case["input_sql"].as_str().unwrap())
                .unwrap();
        }
        let store = Store::open(root.path()).unwrap();
        initialize(&store).unwrap();
        validate(&store).unwrap();
        let actual = data(&store.db);
        assert_eq!(actual, case["rows"], "{}", case["name"]);
        initialize(&store).unwrap();
        assert_eq!(
            data(&store.db),
            actual,
            "repeated initialization changed stored values"
        );
    }
}
#[test]
fn incompatible_schema_rolls_back_full_schema_without_touching_existing_data() {
    for schema in [
        "CREATE TABLE req_timing(id INTEGER PRIMARY KEY, ts TEXT); INSERT INTO req_timing VALUES(7,'qa');",
        "CREATE TABLE bench_result(id INTEGER PRIMARY KEY AUTOINCREMENT,variant_id INTEGER,prompt_name TEXT,rep INTEGER);",
        "CREATE TABLE avatar_cache(owner TEXT PRIMARY KEY,url TEXT NOT NULL,fetched_at REAL NOT NULL CHECK(fetched_at>0));",
        "CREATE TABLE avatar_cache(owner TEXT PRIMARY KEY ON CONFLICT REPLACE,url TEXT NOT NULL,fetched_at REAL NOT NULL);",
        "CREATE TABLE download_history(id INTEGER PRIMARY KEY AUTOINCREMENT,repo_id TEXT NOT NULL,filename TEXT NOT NULL,dest_path TEXT NOT NULL,total_bytes INTEGER NOT NULL,status TEXT NOT NULL,error TEXT,started_at REAL NOT NULL,completed_at REAL); CREATE UNIQUE INDEX qa_extra_unique ON download_history(status);",
    ] {
        let root = private_root();
        let store = Store::open(root.path()).unwrap();
        store.db.execute_batch(schema).unwrap();
        let before = data(&store.db);
        assert!(initialize(&store).is_err());
        assert_eq!(data(&store.db), before);
    }
}
#[test]
fn cancel_or_fault_before_commit_rolls_back_created_tables_and_added_columns() {
    for cancel in [false, true] {
        let root = private_root();
        let case = fixture()["cases"][1].clone();
        {
            let db = Connection::open(root.path().join("model_loader.db")).unwrap();
            db.execute_batch(case["input_sql"].as_str().unwrap())
                .unwrap();
        }
        let store = Store::open(root.path()).unwrap();
        let before = data(&store.db);
        assert!(initialize_before_commit(&store, || {
            if cancel {
                store.cancel.store(true, Ordering::Relaxed);
                Ok(())
            } else {
                Err(Error::Store)
            }
        })
        .is_err());
        drop(store); // Closing a failed startup connection guarantees rollback even after an interrupt.
        let db = Connection::open(root.path().join("model_loader.db")).unwrap();
        assert_eq!(data(&db), before);
        let names: Vec<_> = columns(&db, "bench_result")
            .unwrap()
            .into_iter()
            .map(|v| v[0].as_str().unwrap().to_owned())
            .collect();
        assert!(!names.contains(&"response_text".to_owned()));
    }
}
#[test]
fn an_expired_or_cancelled_budget_initializes_nothing() {
    for cancelled in [false, true] {
        let root = private_root();
        let mut store = Store::open(root.path()).unwrap();
        let before = data(&store.db);
        store
            .budget(
                if cancelled {
                    Instant::now() + OPERATION_DEADLINE
                } else {
                    Instant::now()
                },
                Arc::new(AtomicBool::new(cancelled)),
            )
            .unwrap();
        assert!(initialize(&store).is_err());
        drop(store);
        let db = Connection::open(root.path().join("model_loader.db")).unwrap();
        assert_eq!(data(&db), before);
    }
}
#[tokio::test]
async fn the_worker_bootstraps_before_seed_and_retains_the_same_writer_lock() {
    let root = private_root();
    let prompts = crate::prompts::Prompts::open_with_schema(root.path(), true)
        .await
        .unwrap();
    let db = Connection::open(root.path().join("model_loader.db")).unwrap();
    assert_eq!(tables(&db).len(), 13); // Twelve application tables and SQLite's sequence.
    assert!(
        db.query_row("SELECT count(*) FROM prompts", [], |r| r.get::<_, i64>(0))
            .unwrap()
            > 0
    );
    assert!(
        Store::open(root.path()).is_err(),
        "a second initializer must not obtain the lifetime writer lock"
    );
    drop(prompts);
    assert!(Store::open(root.path()).is_ok());
}

#[test]
fn defaults_unique_keys_and_named_indexes_match_the_python_contract() {
    let root = private_root();
    let store = Store::open(root.path()).unwrap();
    initialize(&store).unwrap();
    store
        .db
        .execute(
            "INSERT INTO bench_run(backend,status,started_at) VALUES('qa','done',1)",
            [],
        )
        .unwrap();
    let defaults: (i64, i64, String) = store
        .db
        .query_row("SELECT reps,max_tokens,note FROM bench_run", [], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .unwrap();
    assert_eq!(defaults, (1, 256, String::new()));
    store
        .db
        .execute(
            "INSERT INTO req_timing(ts,backend,instance,task) VALUES(1,'qa','qa',1)",
            [],
        )
        .unwrap();
    assert!(store
        .db
        .execute(
            "INSERT INTO req_timing(ts,backend,instance,task) VALUES(2,'qa','qa',1)",
            []
        )
        .is_err());
    let fields: (String, i64, f64) = store
        .db
        .query_row(
            "SELECT model_path,prompt_tokens,gen_tps FROM req_timing",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(fields, (String::new(), 0, 0.0));
}
#[test]
fn wrong_defaults_or_a_repurposed_named_index_refuse() {
    for script in [
        SQL.replace("DEFAULT 256", "DEFAULT 257"),
        SQL.replace(
            "ON req_timing(model_path, ts DESC)",
            "ON req_timing(ts, model_path)",
        ),
    ] {
        let root = private_root();
        let store = Store::open(root.path()).unwrap();
        store.db.execute_batch(&script).unwrap();
        assert!(initialize(&store).is_err());
    }
}

#[test]
fn sqlite_work_budget_interrupts_and_rolls_back_schema_before_commit() {
    let root = private_root();
    let store = Store::open(root.path()).unwrap();
    let before = data(&store.db);
    let started = Instant::now();
    assert!(initialize_before_commit(&store, || {
        sql(store.db.query_row("WITH RECURSIVE qa(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM qa WHERE x<100000000) SELECT sum(x) FROM qa",
            [], |row| row.get::<_, i64>(0))).map(|_| ())
    }).is_err());
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
    drop(store);
    let db = Connection::open(root.path().join("model_loader.db")).unwrap();
    assert_eq!(data(&db), before);
}

#[test]
fn unrelated_tables_values_and_extra_nonunique_indexes_are_preserved() {
    let root = private_root();
    let store = Store::open(root.path()).unwrap();
    store.db.execute_batch("CREATE TABLE qa_extra(id INTEGER PRIMARY KEY,value BLOB); INSERT INTO qa_extra VALUES(7,NULL); CREATE INDEX qa_extra_prompt_name ON prompts(name);").unwrap();
    let before = data(&store.db);
    let index: String = store
        .db
        .query_row(
            "SELECT sql FROM sqlite_schema WHERE name='qa_extra_prompt_name'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    initialize(&store).unwrap();
    assert_eq!(data(&store.db)["qa_extra"], before["qa_extra"]);
    let after: String = store
        .db
        .query_row(
            "SELECT sql FROM sqlite_schema WHERE name='qa_extra_prompt_name'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(after, index);
}
