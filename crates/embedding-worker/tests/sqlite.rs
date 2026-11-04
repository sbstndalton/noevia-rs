#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
use embedding_worker::{
    fill::{Row, Store},
    runtime,
    sqlite::*,
    Config, Control, Index, Target,
};
use rusqlite::{params, Connection};
use serde_json::json;
const USER: &str = "11111111-1111-4111-8111-111111111111";
fn target(model: &str) -> Target {
    Target {
        model: model.into(),
        profile: "nomic-v1".into(),
        base_url: None,
    }
}
fn setup() -> (tempfile::TempDir, SqlStore, Index) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let auth = Connection::open(root.join("cowork.db")).unwrap();
    auth.pragma_update(None, "journal_mode", "WAL").unwrap();
    auth.execute_batch(include_str!(
        "../../server-store/tests/fixtures/node-schema.sql"
    ))
    .unwrap();
    auth.execute("INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,created_at,updated_at) VALUES(?,'synthetic','synthetic','Synthetic','member','synthetic','synthetic',0,0)",[USER]).unwrap();
    let dir = root.join("users").join(USER);
    std::fs::create_dir_all(dir.join("rag")).unwrap();
    std::fs::write(
        dir.join("projects.json"),
        json!({"projects":[{"id":"p"}]}).to_string(),
    )
    .unwrap();
    // Reuse the front's actual outbox schema/initialization; no test-only approximation.
    server_projects::outbox::enqueue(&dir, &[], 0).unwrap();
    let index = Index {
        user: Some(USER.into()),
        project: "p".into(),
    };
    let db = Connection::open(dir.join("rag/p.db")).unwrap();
    db.execute_batch("PRAGMA journal_mode=WAL;CREATE TABLE chunks(id INTEGER PRIMARY KEY,file TEXT NOT NULL,chunk_no INTEGER NOT NULL,body TEXT NOT NULL,content_hash TEXT NOT NULL,updated_at TEXT NOT NULL DEFAULT (datetime('now')),version TEXT,direct INTEGER,kind TEXT,meta TEXT);CREATE UNIQUE INDEX idx_chunks_file_hash ON chunks(file,content_hash);CREATE TABLE vec_meta(key TEXT PRIMARY KEY,value TEXT)").unwrap();
    for (id, direct, kind) in [
        (1, Some(0), Some("text")),
        (2, Some(0), Some("text")),
        (3, Some(1), None),
        (4, Some(0), Some("image")),
        (5, None, None),
    ] {
        db.execute("INSERT INTO chunks(id,file,chunk_no,body,content_hash,direct,kind) VALUES(?,?,0,'synthetic chunk',?,?,?)",params![id,format!("f{id}"),format!("hash{id}"),direct,kind]).unwrap();
    }
    let seed = Config {
        version: 0,
        active: target("active"),
        next: Some(target("next")),
        previous: None,
    };
    let store = SqlStore {
        rag_dir: None,
        root: root.into(),
        seed,
        open_vec: open_vec_required,
    };
    (tmp, store, index)
}
#[test]
fn exact_node_slots_framing_coverage_and_legacy_compatibility() {
    let (_tmp, store, index) = setup();
    let rows = store
        .missing(&index, &target("active"), 4, &Default::default())
        .unwrap()
        .unwrap();
    assert_eq!(rows.iter().map(|r| r.id).collect::<Vec<_>>(), vec![1, 2]);
    assert_eq!(
        store
            .publish(
                &index,
                &target("active"),
                &rows,
                &[vec![1.0, 0.0], vec![0.0, 1.0]],
                0,
                &Control::default()
            )
            .unwrap(),
        2
    );
    let path = store.root.join("users").join(USER).join("rag/p.db");
    let db = open_vec_required(&path).unwrap();
    let meta: String = db
        .query_row(
            "SELECT value FROM vec_meta WHERE key=?",
            [format!("slot:{}", slot_id(&target("active")))],
            |r| r.get(0),
        )
        .unwrap();
    let meta: serde_json::Value = serde_json::from_str(&meta).unwrap();
    assert_eq!(meta["table"], "vec_items");
    assert!(meta["map"].is_null());
    let bytes: Vec<u8> = db
        .query_row(
            "SELECT embedding FROM vec_items WHERE chunk_id=1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        bytes,
        [1.0_f32.to_le_bytes(), 0.0_f32.to_le_bytes()].concat()
    );
    assert_eq!(store.remaining(&index, &target("active")).unwrap(), 0);
    assert_eq!(
        store
            .publish(
                &index,
                &target("next"),
                &rows,
                &[vec![0.2, 0.8], vec![0.8, 0.2]],
                0,
                &Control::default()
            )
            .unwrap(),
        2
    );
    let map = format!("vecmap_{}", slot_id(&target("next")));
    let hash: String = db
        .query_row(
            &format!("SELECT content_hash FROM {map} WHERE chunk_id=1"),
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(hash, "hash1");
    db.execute("UPDATE chunks SET content_hash='changed' WHERE id=1", [])
        .unwrap();
    assert_eq!(store.remaining(&index, &target("next")).unwrap(), 1);
    assert_eq!(
        store
            .publish(
                &index,
                &target("next"),
                &rows,
                &[vec![1.0, 0.0], vec![0.0, 1.0]],
                0,
                &Control::default()
            )
            .unwrap(),
        1
    );
    store.drop_slot(&index, &target("next")).unwrap();
    assert_eq!(store.remaining(&index, &target("next")).unwrap(), 2);
    assert_eq!(store.remaining(&index, &target("active")).unwrap(), 0);
}
#[test]
fn tombstone_removed_account_hash_width_stop_and_version_fences() {
    let (_tmp, store, index) = setup();
    let rows = store
        .missing(&index, &target("next"), 4, &Default::default())
        .unwrap()
        .unwrap();
    let vectors = vec![vec![1.0, 0.0], vec![0.0, 1.0]];
    let control = Control::default();
    control.stop();
    assert_eq!(
        store
            .publish(&index, &target("next"), &rows, &vectors, 0, &control)
            .unwrap(),
        0
    );
    assert_eq!(
        store
            .publish(
                &index,
                &target("next"),
                &rows,
                &vectors,
                1,
                &Control::default()
            )
            .unwrap(),
        0
    );
    assert_eq!(
        store
            .publish(
                &index,
                &target("next"),
                &rows,
                &vectors,
                0,
                &Control::default()
            )
            .unwrap(),
        2
    );
    assert_eq!(
        store
            .publish(
                &index,
                &target("next"),
                &rows,
                &[vec![1.0], vec![1.0]],
                0,
                &Control::default()
            )
            .unwrap(),
        0
    );
    let dir = store.root.join("users").join(USER);
    let outbox = Connection::open(dir.join("index-outbox.db")).unwrap();
    outbox
        .execute("INSERT INTO tombstones VALUES('p',1,0)", [])
        .unwrap();
    assert!(store
        .publish(
            &index,
            &target("next"),
            &rows,
            &vectors,
            0,
            &Control::default()
        )
        .is_err());
    outbox.execute("DELETE FROM tombstones", []).unwrap();
    let auth = Connection::open(store.root.join("cowork.db")).unwrap();
    auth.execute("DELETE FROM users WHERE id=?", [USER])
        .unwrap();
    assert!(store
        .publish(
            &index,
            &target("next"),
            &rows,
            &vectors,
            0,
            &Control::default()
        )
        .is_err());
}
#[test]
fn never_recreates_missing_indexes_and_never_acknowledges_outbox() {
    let (_tmp, store, index) = setup();
    let dir = store.root.join("users").join(USER);
    server_projects::outbox::enqueue(
        &dir,
        &[server_projects::outbox::Job {
            kind: "index",
            project: "p",
            file: Some("f1"),
            payload: None,
        }],
        0,
    )
    .unwrap();
    let db = Connection::open(dir.join("index-outbox.db")).unwrap();
    let before: Vec<(String, i64, i64)> = db
        .prepare("SELECT state,attempts,generation FROM jobs")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    store
        .missing(&index, &target("next"), 4, &Default::default())
        .unwrap();
    let after: Vec<(String, i64, i64)> = db
        .prepare("SELECT state,attempts,generation FROM jobs")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(before, after);
    std::fs::remove_file(dir.join("rag/p.db")).unwrap();
    assert!(store
        .missing(&index, &target("next"), 4, &Default::default())
        .is_err());
    assert!(!dir.join("rag/p.db").exists());
}
#[test]
fn loader_failure_prevents_publication_without_plain_table_fallback() {
    let (_tmp, mut store, index) = setup();
    store.open_vec = |_| Err("synthetic registration failure".into());
    assert!(store
        .publish(
            &index,
            &target("next"),
            &[Row {
                id: 1,
                hash: "hash1".into(),
                text: vec![]
            }],
            &[vec![1.0]],
            0,
            &Control::default()
        )
        .is_err());
    let db = Connection::open(store.root.join("users").join(USER).join("rag/p.db")).unwrap();
    let n: i64 = db
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name LIKE 'vec_%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 1);
}
#[test]
fn config_saved_state_wins_and_corruption_fails_closed() {
    let (_tmp, store, _) = setup();
    assert_eq!(store.config().unwrap().active, target("active"));
    let path = store.root.join("embedding-config.json");
    std::fs::write(&path,json!({"version":1,"active":{"model":"saved","profile":"embeddinggemma-2","baseUrl":null},"next":null,"previous":null}).to_string()).unwrap();
    let c = store.config().unwrap();
    assert_ne!(c.version, 0);
    assert_eq!(c.active.model, "saved");
    std::fs::write(path, "broken").unwrap();
    assert!(store.config().is_err());
    assert!(!store.current(c.version, &Control::default()));
    assert!(runtime::seed(|key| if key == "EMBEDDING_PROFILE" {
        Some("unknown".into())
    } else {
        None
    })
    .is_err());
}
#[test]
fn outbox_tombstone_committed_during_publication_wait_refuses_the_write() {
    let (_tmp, store, index) = setup();
    let rows = store
        .missing(&index, &target("next"), 4, &Default::default())
        .unwrap()
        .unwrap();
    let mut outbox =
        Connection::open(store.root.join("users").join(USER).join("index-outbox.db")).unwrap();
    let tx = outbox
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .unwrap();
    tx.execute("INSERT INTO tombstones VALUES('p',7,0)", [])
        .unwrap();
    let (send, recv) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let result = store.publish(
            &index,
            &target("next"),
            &rows,
            &[vec![1.0], vec![1.0]],
            0,
            &Control::default(),
        );
        send.send(result).unwrap();
    });
    assert!(recv
        .recv_timeout(std::time::Duration::from_millis(80))
        .is_err());
    tx.commit().unwrap();
    assert!(recv
        .recv_timeout(std::time::Duration::from_secs(6))
        .unwrap()
        .is_err());
    worker.join().unwrap();
}
#[test]
fn legacy_tenant_without_outbox_is_fenced_by_project_lock_and_never_creates_an_outbox() {
    let (_tmp, store, index) = setup();
    let dir = store.root.join("users").join(USER);
    std::fs::remove_file(dir.join("index-outbox.db")).unwrap();
    assert_eq!(
        store
            .missing(&index, &target("next"), 4, &Default::default())
            .unwrap()
            .unwrap()
            .len(),
        2
    );
    assert!(!dir.join("index-outbox.db").exists());
    std::fs::write(dir.join("projects.json"), "{\"projects\":[]}").unwrap();
    assert!(store
        .missing(&index, &target("next"), 4, &Default::default())
        .is_ok());
}
#[test]
fn historical_node_chunk_columns_are_added_without_changing_existing_rows() {
    let (_tmp, store, index) = setup();
    let db = Connection::open(store.root.join("users").join(USER).join("rag/p.db")).unwrap();
    db.execute_batch("DELETE FROM chunks WHERE id>2;UPDATE chunks SET file='same';ALTER TABLE chunks DROP COLUMN version;ALTER TABLE chunks DROP COLUMN direct;ALTER TABLE chunks DROP COLUMN kind;ALTER TABLE chunks DROP COLUMN meta").unwrap();
    let rows = store
        .missing(&index, &target("next"), 4, &Default::default())
        .unwrap()
        .unwrap();
    assert_eq!(rows.len(), 2);
    store
        .publish(
            &index,
            &target("next"),
            &rows,
            &[vec![1.0], vec![1.0]],
            0,
            &Control::default(),
        )
        .unwrap();
    let n:i64=db.query_row("SELECT COUNT(*) FROM chunks WHERE version IS NULL AND direct IS NULL AND kind IS NULL AND meta IS NULL",[],|r|r.get(0)).unwrap();
    assert_eq!(n, 2);
}
#[test]
fn regression_p2_orphan_index_without_project_catalogue_is_in_coverage() {
    let (_tmp, store, index) = setup();
    let dir = store.root.join("users").join(USER);
    std::fs::remove_file(dir.join("projects.json")).unwrap();
    assert!(store.indexes().contains(&index));
    let rows = store
        .missing(&index, &target("next"), 4, &Default::default())
        .unwrap()
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(
        store
            .publish(
                &index,
                &target("next"),
                &rows,
                &[vec![1.0], vec![1.0]],
                0,
                &Control::default()
            )
            .unwrap(),
        2
    );
}
#[test]
fn regression_p2_read_batches_do_not_wait_for_account_write_lock() {
    let (_tmp, store, index) = setup();
    let mut auth = Connection::open(store.root.join("cowork.db")).unwrap();
    let tx = auth
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .unwrap();
    let (send, recv) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let rows = store.missing(&index, &target("next"), 4, &Default::default());
        let count = store.remaining(&index, &target("next"));
        let _ = send.send((rows, count));
    });
    let answer = recv.recv_timeout(std::time::Duration::from_millis(300));
    tx.rollback().unwrap();
    worker.join().unwrap();
    let (rows, count) =
        answer.expect("read-only RAG queries must not wait behind cowork.db writer");
    assert_eq!(rows.unwrap().unwrap().len(), 2);
    assert_eq!(count.unwrap(), 2);
}
#[test]
fn regression_p3_orphan_slot_tables_are_reused() {
    let (_tmp, store, index) = setup();
    let db = open_vec_required(&store.root.join("users").join(USER).join("rag/p.db")).unwrap();
    let id = slot_id(&target("next"));
    // Existing active slot ensures that next uses its model-specific table.
    let rows = store
        .missing(&index, &target("active"), 4, &Default::default())
        .unwrap()
        .unwrap();
    store
        .publish(
            &index,
            &target("active"),
            &rows,
            &[vec![1.0], vec![1.0]],
            0,
            &Control::default(),
        )
        .unwrap();
    db.execute_batch(&format!("CREATE VIRTUAL TABLE vec_{id} USING vec0(chunk_id INTEGER PRIMARY KEY,embedding float[1]);CREATE TABLE vecmap_{id}(chunk_id INTEGER PRIMARY KEY,content_hash TEXT NOT NULL)")).unwrap();
    assert_eq!(
        store
            .publish(
                &index,
                &target("next"),
                &rows,
                &[vec![1.0], vec![1.0]],
                0,
                &Control::default()
            )
            .unwrap(),
        2
    );
}
#[test]
fn shared_rag_dir_covers_catalogue_free_indexes_outside_data_root() {
    let (_tmp, mut store, index) = setup();
    let shared = tempfile::tempdir().unwrap();
    std::fs::copy(
        store.index_path(&index).unwrap(),
        shared.path().join("legacy.db"),
    )
    .unwrap();
    store.rag_dir = Some(shared.path().into());
    let index = Index {
        user: None,
        project: "legacy".into(),
    };
    assert!(store.indexes().contains(&index));
    let rows = store
        .missing(&index, &target("next"), 4, &Default::default())
        .unwrap()
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(
        store
            .publish(
                &index,
                &target("next"),
                &rows,
                &[vec![1.0], vec![1.0]],
                0,
                &Control::default()
            )
            .unwrap(),
        2
    );
    assert_eq!(store.remaining(&index, &target("next")).unwrap(), 0);
}
#[test]
fn publication_rejects_a_proof_that_expired_while_waiting_for_fences() {
    let (_tmp, store, index) = setup();
    let rows = store
        .missing(&index, &target("next"), 4, &Default::default())
        .unwrap()
        .unwrap();
    let control = Control::default();
    control.confirm_ownership();
    std::thread::sleep(std::time::Duration::from_millis(270));
    assert!(store
        .publish(
            &index,
            &target("next"),
            &rows,
            &[vec![1.0], vec![1.0]],
            0,
            &control
        )
        .is_err());
    assert!(control.suspended());
    assert_eq!(store.remaining(&index, &target("next")).unwrap(), 2);
}
