//! `answer_store` against recorded Python outcomes: every case in `answer-store-fixtures.json`
//! (made by `generate_answer_store_fixtures.py` from the frozen Python Diary store over synthetic
//! managed tenants) is replayed through the Rust functions over the same synthetic tenant, and
//! the results are compared. Synthetic data only.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use chrono::{NaiveDate, NaiveTime};
use diary_core::assembler::Store as ContextStore;
use diary_core::pipeline::Journal as ExchangeJournal;
use noevia_diary::answer_store::{self, AnswerStore, Undetermined};
use noevia_diary::corpus::{self, BaseCorpus, Layout};
use noevia_diary::managed::{self, Managed};
use noevia_diary::recovery::State;
use noevia_diary::write::{Journal, Store, Writers};
use noevia_diary::writes::Ctx;
use serde_json::Value;
use sha2::Digest;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const FIXTURES: &str = include_str!("answer-store-fixtures.json");

fn fixtures() -> Value {
    serde_json::from_str(FIXTURES).unwrap()
}

fn tenant_uid(f: &Value) -> String {
    f["tenant"].as_str().unwrap().to_string()
}

fn tenant_root(root: &Path, uid: &str) -> PathBuf {
    root.join("users").join(uid)
}

fn sha256(data: &[u8]) -> String {
    sha2::Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// A file's bytes from its fixture form: a string, `{"repeat": [unit, count]}` or `{"b64": ..}`.
fn data_of(spec: &Value) -> Vec<u8> {
    if let Some(s) = spec.as_str() {
        return s.as_bytes().to_vec();
    }
    if let Some(r) = spec.get("repeat") {
        let unit = r[0].as_str().unwrap();
        return unit.repeat(r[1].as_u64().unwrap() as usize).into_bytes();
    }
    let b64 = spec["b64"].as_str().unwrap();
    base64_decode(b64)
}

fn base64_decode(text: &str) -> Vec<u8> {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::new();
    let mut acc = 0u32;
    let mut bits = 0;
    for c in text.bytes().filter(|c| *c != b'=') {
        acc = (acc << 6) | ALPHABET.iter().position(|a| *a == c).unwrap() as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    out
}

/// A managed tenant with the fixture's files, directories and journal rows, as Python's
/// ManagedCorpusBackend and Journal lay them out.
fn build_tenant(root: &Path, uid: &str, case: &Value) {
    let t = tenant_root(root, uid);
    std::fs::create_dir_all(&t).unwrap();
    let db = rusqlite::Connection::open(t.join("managed-diary.db")).unwrap();
    db.execute_batch(
        "CREATE TABLE files(path TEXT PRIMARY KEY, data BLOB NOT NULL, version TEXT NOT NULL, updated REAL NOT NULL);
         CREATE TABLE directories(path TEXT PRIMARY KEY);
         CREATE TABLE workspace_imports(fingerprint TEXT PRIMARY KEY, destination TEXT NOT NULL);
         CREATE TABLE workspace_index_outbox(path TEXT PRIMARY KEY);
         CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
         CREATE TABLE backups(destination TEXT PRIMARY KEY, generation INTEGER NOT NULL DEFAULT -1,
             last_success REAL, retry_at REAL NOT NULL DEFAULT 0, failures INTEGER NOT NULL DEFAULT 0, error TEXT);",
    )
    .unwrap();
    let settings = case
        .get("settings")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({"entry_layout": "daily", "index_enabled": false}));
    for (k, v) in [
        ("tenant", uid.to_string()),
        ("active", "1".to_string()),
        ("generation", "1".to_string()),
        ("settings", settings.to_string()),
    ] {
        db.execute("INSERT INTO meta VALUES (?,?)", rusqlite::params![k, v])
            .unwrap();
    }
    let tree = &case["tree"];
    // Python's order: put() files, then raw rows, then directories: scan order is insertion order.
    for key in ["files", "raw"] {
        for pair in tree[key].as_array().unwrap() {
            let path = pair[0].as_str().unwrap();
            let data = data_of(&pair[1]);
            db.execute(
                "INSERT OR REPLACE INTO files VALUES (?,?,?,1.0)",
                rusqlite::params![path, data, sha256(&data)],
            )
            .unwrap();
        }
    }
    for dir in tree["dirs"].as_array().unwrap() {
        db.execute(
            "INSERT INTO directories VALUES (?)",
            [dir.as_str().unwrap()],
        )
        .unwrap();
    }
    let idx = rusqlite::Connection::open(t.join("managed-index.db")).unwrap();
    idx.execute_batch(
        "CREATE TABLE dirty_documents (document TEXT PRIMARY KEY);
         CREATE TABLE journal (id TEXT PRIMARY KEY, created_at TEXT NOT NULL, kind TEXT NOT NULL, payload TEXT NOT NULL,
             applied INTEGER NOT NULL DEFAULT 0, applied_at TEXT, attempts INTEGER NOT NULL DEFAULT 0, last_error TEXT);",
    )
    .unwrap();
    for (i, row) in case
        .get("journal_before")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
    {
        idx.execute(
            "INSERT INTO journal (id, created_at, kind, payload, applied) VALUES (?,?,?,?,?)",
            rusqlite::params![
                format!("before-{i}"),
                "2030-01-01T00:00:00",
                row["kind"].as_str().unwrap(),
                row["payload"].as_str().unwrap(),
                row["applied"].as_i64().unwrap()
            ],
        )
        .unwrap();
    }
    std::fs::write(t.join("managed-diary.active"), uid).unwrap();
}

fn break_files_table(root: &Path, uid: &str) {
    let db = rusqlite::Connection::open(tenant_root(root, uid).join("managed-diary.db")).unwrap();
    db.execute_batch("ALTER TABLE files RENAME TO files_gone")
        .unwrap();
}

fn open(root: &Path, uid: &str) -> (Managed, Layout) {
    let (managed, settings) = managed::ready_for_write(&tenant_root(root, uid), uid, false)
        .unwrap()
        .expect("synthetic tenant is ready");
    let layout = corpus::layout(&BaseCorpus::default(), &settings).expect("supported layout");
    (managed, layout)
}

/// Run `f` with a write context over the tenant, under its writer slot (as the server does).
fn with_ctx<T>(root: &Path, uid: &str, f: impl FnOnce(&Ctx<'_>) -> T) -> T {
    with_ctx_after(root, uid, || (), f)
}

/// `with_ctx`, with `damage` run once the tenant has been opened (a tenant that is already damaged
/// is not ready, so it cannot be opened).
fn with_ctx_after<T>(
    root: &Path,
    uid: &str,
    damage: impl FnOnce(),
    f: impl FnOnce(&Ctx<'_>) -> T,
) -> T {
    let (managed, layout) = open(root, uid);
    damage();
    let tenant = tenant_root(root, uid);
    Writers::default()
        .with_lock(uid, |held| {
            let journal = Journal::open(&tenant.join("managed-index.db"), held).unwrap();
            let recovery = Mutex::new(State::default());
            let ctx = Ctx {
                recovery: &recovery,
                reader: &managed,
                store: Store::new(&tenant.join("managed-diary.db")),
                journal,
                layout: &layout,
            };
            f(&ctx)
        })
        .unwrap()
}

fn day_of(text: &str) -> NaiveDate {
    NaiveDate::parse_from_str(text, "%Y-%m-%d").unwrap()
}

// ------------------------------------------------------------------ (a) context day text

#[test]
fn context_day_text_matches_python_for_every_recorded_case() {
    let f = fixtures();
    let uid = tenant_uid(&f);
    let mut checked = 0;
    for case in f["day_text"].as_array().unwrap() {
        let dir = tempfile::tempdir().unwrap();
        build_tenant(dir.path(), &uid, case);
        let (managed, layout) = open(dir.path(), &uid);
        if case.get("break_files_table").is_some() {
            break_files_table(dir.path(), &uid);
        }
        for q in case["queries"].as_array().unwrap() {
            let day = day_of(q["day"].as_str().unwrap());
            let cap = q["max_chars"].as_i64().unwrap();
            let name = format!("{} {} cap {cap}", case["name"], q["day"]);
            assert_eq!(
                answer_store::context_day_text(&managed, &layout, day, cap),
                q["expected"].as_str().unwrap(),
                "{name}"
            );
            // The /api/day form keeps its markers and is unchanged by the refactor.
            if let Ok(cap) = usize::try_from(cap) {
                assert_eq!(
                    corpus::day_text(&managed, &layout, day, cap),
                    q["expected_markers"].as_str().unwrap(),
                    "{name} (markers)"
                );
            }
            checked += 1;
        }
    }
    assert!(checked > 700, "{checked}");
}

#[test]
fn context_day_text_strips_markers_and_caps_by_code_points() {
    let dir = tempfile::tempdir().unwrap();
    let text = "## Sunday, January 6, 2030\n\n### 09:00 \u{2014} T\n\n**Me:** a\u{1F33F}b\n\n**Assistant:** c\n\n<!-- xid:00000000-0000-4000-8000-0000000000aa -->\n";
    let case = serde_json::json!({
        "settings": {"entry_layout": "monthly", "index_enabled": false},
        "tree": {"files": [["2030-01.md", text]], "raw": [], "dirs": []},
    });
    build_tenant(dir.path(), "u-1", &case);
    let (managed, layout) = open(dir.path(), "u-1");
    let day = day_of("2030-01-06");
    let plain = answer_store::context_day_text(&managed, &layout, day, 0);
    assert!(!plain.contains("xid"), "{plain}");
    assert!(corpus::day_text(&managed, &layout, day, 0).contains("<!-- xid:"));
    // The cap counts code points: 19 characters end just after the emoji, 20 take it whole.
    let notice = "(earlier exchanges today truncated)\n\u{2026}";
    assert_eq!(
        answer_store::context_day_text(&managed, &layout, day, 19),
        format!("{notice}b\n\n**Assistant:** c")
    );
    assert_eq!(
        answer_store::context_day_text(&managed, &layout, day, 20),
        format!("{notice}\u{1F33F}b\n\n**Assistant:** c")
    );
    // 0 is no cap; a text that fits is returned whole.
    assert_eq!(
        answer_store::context_day_text(&managed, &layout, day, 0),
        plain
    );
    assert_eq!(
        answer_store::context_day_text(&managed, &layout, day, plain.chars().count() as i64),
        plain
    );
}

// ------------------------------------------------------------------ (b) memory text

#[test]
fn memory_text_matches_python_for_every_recorded_tree() {
    let f = fixtures();
    let uid = tenant_uid(&f);
    let mut undetermined = 0;
    for case in f["memory_text"].as_array().unwrap() {
        let dir = tempfile::tempdir().unwrap();
        build_tenant(dir.path(), &uid, case);
        let (managed, _) = open(dir.path(), &uid);
        if case.get("break_files_table").is_some() {
            break_files_table(dir.path(), &uid);
        }
        let name = case["name"].as_str().unwrap();
        let got = answer_store::memory_text(&managed);
        if case["rust"].as_str() == Some("undetermined") {
            // A named known difference: the order Python used cannot be reproduced, so no text
            // is guessed at.
            assert_eq!(got, Err(Undetermined), "{name}");
            undetermined += 1;
        } else {
            assert_eq!(
                got.as_deref(),
                Ok(case["expected"].as_str().unwrap()),
                "{name}"
            );
        }
    }
    assert_eq!(undetermined, 4);
}

// ------------------------------------------------------------------ (c) log_exchange

fn files_of(root: &Path, uid: &str) -> BTreeMap<String, Vec<u8>> {
    let db = rusqlite::Connection::open(tenant_root(root, uid).join("managed-diary.db")).unwrap();
    let mut stmt = db.prepare("SELECT path, data FROM files").unwrap();
    stmt.query_map([], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?))
    })
    .unwrap()
    .map(Result::unwrap)
    .collect()
}

struct Row {
    kind: String,
    payload: String,
    applied: i64,
    attempts: i64,
    last_error: Option<String>,
}

fn journal_rows(root: &Path, uid: &str, skip: usize) -> Vec<Row> {
    let db = rusqlite::Connection::open(tenant_root(root, uid).join("managed-index.db")).unwrap();
    let mut stmt = db
        .prepare("SELECT kind, payload, applied, attempts, last_error FROM journal ORDER BY rowid")
        .unwrap();
    stmt.query_map([], |r| {
        Ok(Row {
            kind: r.get(0)?,
            payload: r.get(1)?,
            applied: r.get(2)?,
            attempts: r.get(3)?,
            last_error: r.get(4)?,
        })
    })
    .unwrap()
    .map(Result::unwrap)
    .skip(skip)
    .collect()
}

fn dirty(root: &Path, uid: &str) -> Vec<String> {
    let db = rusqlite::Connection::open(tenant_root(root, uid).join("managed-index.db")).unwrap();
    let mut stmt = db
        .prepare("SELECT document FROM dirty_documents ORDER BY document")
        .unwrap();
    stmt.query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

fn is_uuid4(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 36
        && b.iter().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => *c == b'-',
            14 => *c == b'4',
            19 => matches!(c, b'8' | b'9' | b'a' | b'b'),
            _ => c.is_ascii_digit() || (b'a'..=b'f').contains(c),
        })
}

/// A recorded stored file: text, or `{"b64": ..}` for bytes that are not UTF-8.
fn recorded_bytes(v: &Value) -> Vec<u8> {
    data_of(v)
}

#[test]
fn log_exchange_matches_python_for_every_recorded_case() {
    let f = fixtures();
    let uid = tenant_uid(&f);
    let (mut logged, mut pending, mut refused) = (0, 0, 0);
    for case in f["log_exchange"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let dir = tempfile::tempdir().unwrap();
        build_tenant(dir.path(), &uid, case);
        let args = &case["args"];
        let now = NaiveTime::parse_from_str(args["now"].as_str().unwrap(), "%H:%M").unwrap();
        let result = with_ctx(dir.path(), &uid, |c| {
            answer_store::log_exchange(
                c,
                day_of(args["day"].as_str().unwrap()),
                now,
                args["topic"].as_str().unwrap(),
                args["me"].as_str().unwrap(),
                args["claude"].as_str().unwrap(),
            )
        });
        let expected = &case["expected"];
        let skip = case["journal_before"].as_array().unwrap().len();
        let rows = journal_rows(dir.path(), &uid, skip);
        let files = files_of(dir.path(), &uid);
        // Python's xid is a counter value; ours is random: compare with ours mapped onto it.
        let py_xid = expected.get("xid").and_then(Value::as_str);
        let (our_xid, expect_pending) = match (&result, expected["pending"].as_bool().unwrap()) {
            (Ok(xid), false) => (Some(xid.clone()), false),
            (Err((true, msg)), true) => {
                assert!(msg.contains("pending"), "{name}: {msg}");
                // The xid is only in the journal row.
                let payload: Value = serde_json::from_str(&rows[0].payload).unwrap();
                (payload["xid"].as_str().map(str::to_string), true)
            }
            (Err((false, _)), false) if expected.get("raises").is_some() => {
                refused += 1;
                (None, false)
            }
            other => panic!("{name}: unexpected outcome {other:?}"),
        };
        if let Some(raised) = expected.get("raises") {
            // Python raised before it enqueued anything: nothing was written, kept or marked.
            assert!(raised.is_string());
            assert!(rows.is_empty(), "{name}");
            assert!(dirty(dir.path(), &uid).is_empty(), "{name}");
            assert_eq!(
                files.len(),
                expected["files"].as_object().unwrap().len(),
                "{name}"
            );
            continue;
        }
        let our_xid = our_xid.unwrap();
        let py_xid = py_xid.unwrap();
        assert!(is_uuid4(&our_xid), "{name}: {our_xid}");
        assert_ne!(our_xid, py_xid, "{name}: the xid must be fresh");
        let swap = |text: &str| text.replace(&our_xid, py_xid);

        // Every stored file, byte for byte (the xid mapped).
        let want = expected["files"].as_object().unwrap();
        assert_eq!(
            files.keys().collect::<Vec<_>>(),
            want.keys().collect::<Vec<_>>(),
            "{name}: stored paths"
        );
        for (path, bytes) in &files {
            assert_eq!(
                swap(&String::from_utf8(bytes.clone()).unwrap()).into_bytes(),
                recorded_bytes(&want[path]),
                "{name}: {path}"
            );
        }
        // The journal row: kind, the payload (same keys, same order, same text), applied.
        let want_rows = expected["journal"].as_array().unwrap();
        assert_eq!(rows.len(), want_rows.len(), "{name}: journal rows");
        for (row, want) in rows.iter().zip(want_rows) {
            assert_eq!(row.kind, want["kind"].as_str().unwrap(), "{name}");
            assert_eq!(
                swap(&row.payload),
                want["payload"].as_str().unwrap(),
                "{name}"
            );
            assert_eq!(row.applied, want["applied"].as_i64().unwrap(), "{name}");
            assert_eq!(row.attempts, want["attempts"].as_i64().unwrap(), "{name}");
            assert_eq!(
                row.last_error.as_deref(),
                want["last_error"].as_str(),
                "{name}"
            );
        }
        // Marked dirty before the write, as the append route does (Python reindexes afterwards).
        let payload: Value = serde_json::from_str(&rows[0].payload).unwrap();
        assert_eq!(
            dirty(dir.path(), &uid),
            vec![payload["document"].as_str().unwrap().to_string()],
            "{name}"
        );
        if expect_pending {
            pending += 1;
        } else {
            logged += 1;
        }
    }
    assert_eq!((logged, pending, refused), (22, 3, 1));
}

#[test]
fn a_fresh_exchange_scaffolds_once_and_an_existing_tenant_does_not() {
    let f = fixtures();
    let uid = tenant_uid(&f);
    let dir = tempfile::tempdir().unwrap();
    let case = serde_json::json!({
        "settings": {"entry_layout": "daily", "index_enabled": false},
        "tree": {"files": [], "raw": [], "dirs": []},
    });
    build_tenant(dir.path(), &uid, &case);
    let day = day_of("2030-01-06");
    let now = NaiveTime::from_hms_opt(9, 30, 0).unwrap();
    let first = with_ctx(dir.path(), &uid, |c| {
        answer_store::log_exchange(c, day, now, "Topic", "me", "assistant")
    })
    .unwrap();
    let files = files_of(dir.path(), &uid);
    assert!(files.contains_key("Entries/README.md"));
    assert!(files.contains_key("AI Memory/README.md"));
    assert!(files.contains_key("Raw Sources/README.md"));
    let rows = journal_rows(dir.path(), &uid, 0);
    assert!(rows[0]
        .payload
        .starts_with(r#"{"scaffold": true, "xid": ""#));
    // A user's edit of a seed survives the next exchange, which is not a scaffold.
    let db =
        rusqlite::Connection::open(tenant_root(dir.path(), &uid).join("managed-diary.db")).unwrap();
    db.execute("DELETE FROM files WHERE path='Raw Sources/README.md'", [])
        .unwrap();
    drop(db);
    let second = with_ctx(dir.path(), &uid, |c| {
        answer_store::log_exchange(c, day, now, "Topic", "me again", "assistant again")
    })
    .unwrap();
    assert_ne!(first, second);
    let files = files_of(dir.path(), &uid);
    assert!(!files.contains_key("Raw Sources/README.md"));
    let rows = journal_rows(dir.path(), &uid, 0);
    assert!(rows[1]
        .payload
        .starts_with(r#"{"scaffold": false, "xid": ""#));
    let doc = String::from_utf8(files["Entries/2030/January/January 6, 2030.md"].clone()).unwrap();
    assert!(doc.contains("**Me:** me\n") && doc.contains("**Me:** me again\n"));
    assert!(doc.contains(&first) && doc.contains(&second));
}

#[test]
fn a_journal_failure_before_the_row_exists_is_an_error_that_wrote_nothing() {
    let f = fixtures();
    let uid = tenant_uid(&f);
    let dir = tempfile::tempdir().unwrap();
    let case = serde_json::json!({
        "settings": {"entry_layout": "daily", "index_enabled": false},
        "tree": {"files": [], "raw": [], "dirs": []},
    });
    build_tenant(dir.path(), &uid, &case);
    let result = with_ctx_after(
        dir.path(),
        &uid,
        || {
            let idx =
                rusqlite::Connection::open(tenant_root(dir.path(), &uid).join("managed-index.db"))
                    .unwrap();
            idx.execute_batch("ALTER TABLE journal RENAME TO journal_gone")
                .unwrap();
        },
        |c| {
            answer_store::log_exchange(
                c,
                day_of("2030-01-06"),
                NaiveTime::from_hms_opt(9, 0, 0).unwrap(),
                "T",
                "me",
                "assistant",
            )
        },
    );
    let Err((native_unavailable, message)) = result else {
        panic!("expected an error");
    };
    assert!(!native_unavailable);
    assert!(!message.is_empty());
    assert!(files_of(dir.path(), &uid).is_empty());
}

// ------------------------------------------------------------------ the diary_core adapters

#[test]
fn the_adapters_serve_the_assembler_and_the_pipeline() {
    let f = fixtures();
    let uid = tenant_uid(&f);
    let dir = tempfile::tempdir().unwrap();
    let case = serde_json::json!({
        "settings": {"entry_layout": "daily", "index_enabled": false},
        "tree": {"files": [["AI Memory/notes.md", "Remember the canal.\n"]], "raw": [], "dirs": []},
    });
    build_tenant(dir.path(), &uid, &case);
    with_ctx(dir.path(), &uid, |c| {
        let now = NaiveTime::from_hms_opt(14, 32, 59).unwrap();
        let mut a = AnswerStore::new(c, now);
        assert!(!ExchangeJournal::index_enabled(&a));
        assert_eq!(
            ExchangeJournal::standing_sections_text(&mut a, 4000),
            Ok(String::new())
        );
        assert_eq!(
            a.update_standing_sections(&Value::Null, &Value::Null, "2030-01-06"),
            Ok(None)
        );
        assert_eq!(
            ContextStore::standing_sections_text(&a, 4000),
            Ok(String::new())
        );
        assert_eq!(
            ContextStore::memory_text(&a),
            "--- AI Memory/notes.md ---\nRemember the canal.\n"
        );
        assert!(!a.undetermined());
        let day = day_of("2030-01-06");
        assert_eq!(ContextStore::day_text(&a, day, 12000), Ok(String::new()));
        // The save runs through the adapter and takes its time from the exchange's clock.
        let xid = ExchangeJournal::log_exchange(
            &mut a,
            "2030-01-06",
            "Canal",
            "I walked.",
            "The user walked.",
        )
        .unwrap();
        let text = ContextStore::day_text(&a, day, 12000).unwrap();
        assert!(text.contains("### 14:32 \u{2014} Canal"), "{text}");
        assert!(
            text.contains("**Me:** I walked.") && text.contains("**Assistant:** The user walked.")
        );
        assert!(!text.contains(&xid), "markers are not model context");
        // A malformed day is an error, not a native write that may be pending.
        let bad = ExchangeJournal::log_exchange(&mut a, "06/01/2030", "T", "m", "c");
        assert!(matches!(bad, Err((false, _))));
    });
}

#[test]
fn the_adapter_records_a_listing_it_cannot_reproduce() {
    let f = fixtures();
    let uid = tenant_uid(&f);
    let dir = tempfile::tempdir().unwrap();
    let case = serde_json::json!({
        "settings": {"entry_layout": "daily", "index_enabled": false},
        "tree": {"files": [["AI Memory/Dup.md", "a\n"], ["AI Memory/dup.md", "b\n"]], "raw": [], "dirs": []},
    });
    build_tenant(dir.path(), &uid, &case);
    with_ctx(dir.path(), &uid, |c| {
        let a = AnswerStore::new(c, NaiveTime::from_hms_opt(1, 2, 0).unwrap());
        assert!(!a.undetermined());
        assert_eq!(ContextStore::memory_text(&a), "");
        assert!(a.undetermined());
    });
}
