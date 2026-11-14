#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::type_complexity
)]

//! The [`Indexer`]'s queue across threads: a re-index requested while an earlier run embeds supersedes
//! it (the old run writes no vectors), a delete during an embed leaves no rows behind, an inactive
//! account gets nothing written, and a drained queue forgets the file.

use project_index::chunk::hash;
use project_index::embed::Vector;
use project_index::indexer::{Host, Indexer};
use project_index::slots::Target;
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

struct TestHost {
    dir: PathBuf,
    active: AtomicBool,
}

impl Host for TestHost {
    fn index_path(&self, project: &str, user: Option<&str>) -> PathBuf {
        self.dir
            .join(user.unwrap_or("shared"))
            .join("rag")
            .join(format!("{project}.db"))
    }
    fn user_active(&self, _user: &str) -> bool {
        self.active.load(Ordering::SeqCst)
    }
    fn lifecycle_generation(&self, _user: Option<&str>) -> u64 {
        0
    }
    fn deleted_since(&self, _project: &str, _user: Option<&str>, _seen: u64) -> bool {
        false
    }
    fn active_target(&self) -> Target {
        Target {
            profile: "qa-profile".into(),
            model: "qa-model".into(),
        }
    }
    fn embedding_off(&self, _t: &Target) -> bool {
        false
    }
    fn foreground_busy(&self) -> bool {
        false
    }
}

fn long(word: &str) -> String {
    format!("{word} sentence. ").repeat(400)
}

fn vectors(texts: &[String]) -> Result<Vec<Vector>, String> {
    Ok(texts
        .iter()
        .map(|t| Some(vec![t.len() as f64, 1.0, 0.5]))
        .collect())
}

fn rows(path: &Path) -> Vec<(String, Option<String>)> {
    let db = Connection::open(path).unwrap();
    let mut st = db
        .prepare("SELECT file, version FROM chunks ORDER BY id")
        .unwrap();
    st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn vector_count(path: &Path) -> i64 {
    let db = Connection::open(path).unwrap();
    let tables: Vec<String> = {
        let mut st = db
            .prepare("SELECT name FROM sqlite_master WHERE name LIKE 'vecmap_%'")
            .unwrap();
        st.query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    };
    tables
        .iter()
        .map(|t| {
            db.query_row(&format!("SELECT COUNT(*) FROM {t}"), [], |r| {
                r.get::<_, i64>(0)
            })
            .unwrap()
        })
        .sum()
}

fn indexer() -> (tempfile::TempDir, Arc<Indexer<TestHost>>) {
    sqlite_vec_loader::register().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let host = TestHost {
        dir: tmp.path().to_path_buf(),
        active: AtomicBool::new(true),
    };
    (tmp, Arc::new(Indexer::new(host)))
}

type Gate = (mpsc::Receiver<()>, mpsc::Sender<()>);

/// An embedder that tells the test it started, then waits for the test's go.
fn gated() -> (
    impl FnMut(&Target, &[String]) -> Result<Vec<Vector>, String> + Send,
    mpsc::Receiver<()>,
    mpsc::Sender<()>,
) {
    let (embed, (started, go)) = gated_parts();
    (embed, started, go)
}

fn gated_parts() -> (
    impl FnMut(&Target, &[String]) -> Result<Vec<Vector>, String> + Send,
    Gate,
) {
    let (started_tx, started_rx) = mpsc::channel();
    let (go_tx, go_rx) = mpsc::channel::<()>();
    let go_rx = Mutex::new(go_rx);
    let mut first = true;
    let embed = move |_: &Target, texts: &[String]| {
        if first {
            first = false;
            started_tx.send(()).unwrap();
            go_rx.lock().unwrap().recv().unwrap();
        }
        vectors(texts)
    };
    (embed, (started_rx, go_tx))
}

#[test]
fn a_reindex_during_an_embed_supersedes_the_older_run() {
    let (_tmp, ix) = indexer();
    let (mut slow, started, go) = gated();
    let a = Arc::clone(&ix);
    let old = std::thread::spawn(move || {
        a.index_file(
            "qa-proj",
            "a.md",
            &long("alpha"),
            Some("qa-user"),
            &mut slow,
        )
    });
    started.recv().unwrap();
    // The old run is embedding its first batch; a newer version is requested and waits for the lane.
    assert!(ix.is_current_version(
        "qa-proj",
        "a.md",
        Some("qa-user"),
        Some(&hash(&long("alpha")))
    ));
    let b = Arc::clone(&ix);
    let new = std::thread::spawn(move || {
        b.index_file(
            "qa-proj",
            "a.md",
            &long("bravo"),
            Some("qa-user"),
            &mut |_: &Target, t: &[String]| vectors(t),
        )
    });
    let bravo = hash(&long("bravo"));
    while !ix.is_current_version("qa-proj", "a.md", Some("qa-user"), Some(&bravo)) {
        std::thread::yield_now();
    }
    assert!(
        !ix.is_current_version(
            "qa-proj",
            "a.md",
            Some("qa-user"),
            Some(&hash(&long("alpha")))
        ),
        "search drops the old text"
    );
    std::thread::sleep(std::time::Duration::from_millis(50));
    go.send(()).unwrap();
    let old = old.join().unwrap();
    let new = new.join().unwrap();
    assert!(
        old.ok && old.embedded == 0,
        "the superseded run stores no vector: {old:?}"
    );
    assert!(
        new.ok && new.embedded == new.stored && new.stored > 1,
        "{new:?}"
    );
    let path = ix.host().index_path("qa-proj", Some("qa-user"));
    let want = hash(&long("bravo"));
    assert!(rows(&path)
        .iter()
        .all(|(f, v)| f == "a.md" && v.as_deref() == Some(want.as_str())));
    assert_eq!(vector_count(&path), new.stored as i64);
    assert_eq!(ix.tracked(), (0, 0, 0), "a drained queue forgets the file");
}

#[test]
fn a_delete_during_an_embed_leaves_nothing() {
    let (_tmp, ix) = indexer();
    let (mut slow, started, go) = gated();
    let a = Arc::clone(&ix);
    let run = std::thread::spawn(move || {
        a.index_file(
            "qa-proj",
            "a.md",
            &long("alpha"),
            Some("qa-user"),
            &mut slow,
        )
    });
    started.recv().unwrap();
    ix.delete_file("qa-proj", "a.md", Some("qa-user"), false)
        .unwrap();
    // The rows went, so (as in Node) no version needs filtering; the run is superseded by its generation.
    assert_eq!(ix.tracked(), (1, 1, 0));
    go.send(()).unwrap();
    let out = run.join().unwrap();
    assert_eq!(out.embedded, 0);
    let path = ix.host().index_path("qa-proj", Some("qa-user"));
    assert!(rows(&path).is_empty());
    assert_eq!(vector_count(&path), 0);
    assert_eq!(ix.tracked(), (0, 0, 0));
}

#[test]
fn an_inactive_account_gets_no_index() {
    let (_tmp, ix) = indexer();
    ix.host().active.store(false, Ordering::SeqCst);
    let out = ix.index_file(
        "qa-proj",
        "a.md",
        "small",
        Some("qa-user"),
        &mut |_: &Target, t: &[String]| vectors(t),
    );
    assert!(out.ok && out.superseded, "{out:?}");
    assert!(!ix.host().index_path("qa-proj", Some("qa-user")).exists());
    ix.host().active.store(true, Ordering::SeqCst);
    let out = ix.index_file(
        "qa-proj",
        "a.md",
        "small",
        Some("qa-user"),
        &mut |_: &Target, t: &[String]| vectors(t),
    );
    assert!(out.ok && out.direct && out.stored == 1);
    assert_eq!(ix.wanted_versions("qa-proj", Some("qa-user")), vec![]);
}

#[test]
fn a_failed_embed_keeps_the_text_and_the_queue_forgets() {
    let (_tmp, ix) = indexer();
    let out = ix.index_file(
        "qa-proj",
        "a.md",
        &long("alpha"),
        None,
        &mut |_: &Target, _: &[String]| Err("embeddings 500: down".to_string()),
    );
    assert!(out.ok && out.stored > 1 && out.embedded == 0);
    assert_eq!(out.embed_error.as_deref(), Some("embeddings 500: down"));
    let path = ix.host().index_path("qa-proj", None);
    assert_eq!(rows(&path).len(), out.stored);
    assert_eq!(ix.tracked(), (0, 0, 0));
}
