//! What Node cannot be the oracle for (sbstndalton/noevia#1373): the flag seam before any row or
//! key, no DDL on a missing or older schema, byte budgets, unreadable keys, and no writes.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod support;
use server_chatgpt_status::{answer, DATA_ENC_BYTES, FAILED_MESSAGE, OWNER_BYTES};
use std::sync::atomic::{AtomicUsize, Ordering};
use support::*;

const A: &str = "aaaaaaaa-1111-4111-8111-aaaaaaaaaaaa";
const B: &str = "bbbbbbbb-2222-4222-8222-bbbbbbbbbbbb";
const KEY: [u8; 32] = [7; 32];
const FAILED: &str = "{\"error\":\"ChatGPT sign-in failed.\"}";
const OFF: &str = "{\"error\":\"Sign in with ChatGPT is turned off on this server.\"}";

fn run(
    d: &tempfile::TempDir,
    owner: &str,
    env: Option<bool>,
    keys_ok: bool,
) -> (u16, String, usize) {
    let calls = AtomicUsize::new(0);
    let keys = keys_ok.then(|| {
        write_key(d.path(), "secrets.key", &KEY);
        support::keys(d.path(), None)
    });
    let store = server_store::Store::open(d.path()).unwrap();
    let a = answer(&store, owner, env, &|| {
        calls.fetch_add(1, Ordering::SeqCst);
        keys.clone()
    });
    (a.status, a.body, calls.load(Ordering::SeqCst))
}

#[test]
fn flag_off_answers_404_before_any_row_schema_or_key() {
    // No ChatGPT table at all, and a corrupt row would be irrelevant: still Node's 404.
    let (d, db) = data_dir(None);
    assert_eq!(run(&d, A, None, true), (404, OFF.into(), 0));
    set_flag(&db, "false");
    assert_eq!(run(&d, A, None, true), (404, OFF.into(), 0));
    set_flag(&db, "True");
    assert_eq!(run(&d, A, None, true), (404, OFF.into(), 0));
    // The same bytes as "true" in a BLOB never enable Node's TEXT-only flag.
    db.execute_batch(
        "INSERT OR REPLACE INTO settings(key,value) VALUES('feature:chatgptOAuth',x'74727565')",
    )
    .unwrap();
    assert_eq!(run(&d, A, None, true), (404, OFF.into(), 0));
    set_flag(&db, "true");
    assert_eq!(run(&d, A, Some(false), true), (404, OFF.into(), 0));
}

#[test]
fn missing_or_older_schema_is_a_sanitized_502_and_never_migrated() {
    let (d, db) = data_dir(None);
    set_flag(&db, "true");
    assert_eq!(run(&d, A, None, true), (502, FAILED.into(), 0));
    assert_eq!(FAILED, format!("{{\"error\":\"{FAILED_MESSAGE}\"}}"));
    let exists = |db: &server_store::rusqlite::Connection| -> i64 {
        db.query_row(
            "SELECT count(*) FROM sqlite_master WHERE name='chatgpt_oauth_tokens'",
            [],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(exists(&db), 0, "no DDL ran");
    db.execute_batch("CREATE TABLE chatgpt_oauth_tokens(user_id TEXT PRIMARY KEY, data_enc TEXT NOT NULL, updated_at INTEGER NOT NULL)").unwrap();
    assert_eq!(run(&d, A, None, true), (502, FAILED.into(), 0));
    let (d, db) = data_dir(None);
    set_flag(&db, "true");
    db.execute_batch("CREATE TABLE chatgpt_oauth_tokens(user_id TEXT, data_enc TEXT, state TEXT, updated_at INTEGER)").unwrap();
    assert_eq!(
        run(&d, A, None, true),
        (502, FAILED.into(), 0),
        "no owner primary key"
    );
}

#[test]
fn keys_are_touched_only_for_a_connected_row_and_their_absence_is_502() {
    let (d, db) = data_dir(Some(&node_ddl()));
    set_flag(&db, "true");
    let (s, b, calls) = run(&d, A, None, false);
    assert_eq!((s, calls), (200, 0));
    assert!(b.contains("disconnected"));
    put_row(&db, A, "reconnect", &"enc:v2:x");
    let (s, b, calls) = run(&d, A, None, false);
    assert_eq!((s, calls), (200, 0));
    assert!(b.contains("\"reconnect\""));
    db.execute("UPDATE chatgpt_oauth_tokens SET state='connected'", [])
        .unwrap();
    assert_eq!(run(&d, A, None, false), (502, FAILED.into(), 1));
}

#[test]
fn foreign_rows_are_never_selected() {
    let (d, db) = data_dir(Some(&node_ddl()));
    set_flag(&db, "true");
    let sealed = seal(
        &KEY,
        br#"{"email":"b@example.test","plan":"team"}"#,
        Some(B),
    );
    put_row(&db, B, "connected", &sealed);
    let (s, b, calls) = run(&d, A, None, true);
    assert_eq!((s, calls), (200, 0));
    assert!(b.contains("disconnected") && !b.contains("example"));
    // A's id with a case or whitespace variant is not B's row either.
    let (s, b, _) = run(&d, &B.to_uppercase(), None, true);
    assert_eq!(s, 200);
    assert!(b.contains("disconnected"));
    let (s, b, _) = run(&d, B, None, true);
    assert_eq!(s, 200);
    assert!(b.contains("b…@example.test"));
}

#[test]
fn byte_budgets_refuse_before_copying_and_keep_nodes_answers_below_them() {
    let (d, db) = data_dir(Some(&node_ddl()));
    set_flag(&db, "true");
    // Legacy plaintext of exactly the cap is answered as Node does.
    let pad = |n: usize| {
        let head = r#"{"email":"p@example.test","plan":"plus","pad":""#;
        let tail = r#""}"#;
        format!("{head}{}{tail}", "x".repeat(n - head.len() - tail.len()))
    };
    put_row(&db, A, "connected", &pad(DATA_ENC_BYTES));
    let (s, b, _) = run(&d, A, None, true);
    assert_eq!(s, 200, "{b}");
    assert!(b.contains("p…@example.test"));
    db.execute(
        "UPDATE chatgpt_oauth_tokens SET data_enc=?1",
        [pad(DATA_ENC_BYTES + 1)],
    )
    .unwrap();
    assert_eq!(run(&d, A, None, true), (502, FAILED.into(), 0));
    // Oversized but not connected: Node's reconnect, the ciphertext never fetched.
    db.execute("UPDATE chatgpt_oauth_tokens SET state='reconnect'", [])
        .unwrap();
    let (s, b, _) = run(&d, A, None, true);
    assert_eq!(s, 200);
    assert!(b.contains("\"reconnect\""));
    // A 1 MiB row the same.
    db.execute(
        "UPDATE chatgpt_oauth_tokens SET state='connected', data_enc=?1",
        ["y".repeat(1 << 20)],
    )
    .unwrap();
    assert_eq!(run(&d, A, None, true), (502, FAILED.into(), 0));
    // An owner id over its cap is refused before any query.
    let long = "a".repeat(OWNER_BYTES + 1);
    assert_eq!(run(&d, &long, None, true), (502, FAILED.into(), 0));
}

#[test]
fn rows_node_never_writes_are_502_not_guesses() {
    let (d, db) = data_dir(Some(&node_ddl()));
    set_flag(&db, "true");
    put_row(&db, A, "connected", &vec![1u8, 2, 3]);
    assert_eq!(
        run(&d, A, None, true),
        (502, FAILED.into(), 0),
        "blob ciphertext"
    );
    db.execute("DELETE FROM chatgpt_oauth_tokens", []).unwrap();
    // JSON.parse accepts 1e400 (Infinity); this port refuses it rather than guess.
    put_row(
        &db,
        A,
        "connected",
        &seal(&KEY, br#"{"email":"p@x","n":1e400}"#, Some(A)),
    );
    assert_eq!(run(&d, A, None, true), (502, FAILED.into(), 1));
}

#[test]
fn the_read_writes_nothing() {
    let (d, db) = data_dir(Some(&node_ddl()));
    set_flag(&db, "true");
    put_row(
        &db,
        A,
        "connected",
        &seal(
            &KEY,
            br#"{"email":"p@example.test","plan":"plus","expiresAt":1}"#,
            Some(A),
        ),
    );
    let before = dump(&db);
    let (s, _, _) = run(&d, A, None, true);
    assert_eq!(s, 200);
    assert_eq!(dump(&db), before);
    assert!(!d.path().join("secrets.key.previous").exists());
}
