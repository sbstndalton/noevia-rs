#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::type_complexity
)]

//! The [`Indexer`]'s queue as Node's promise chain orders it: runs of one file go first in, first out;
//! `afterIndex` fires only when an index run drains the queue; a run that panics never stalls the file;
//! the width warning fires when a batch's vectors do not fit the slot.

use project_index::embed::Vector;
use project_index::index::Embed;
use project_index::indexer::{Host, Indexer};
use project_index::slots::Target;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

struct TestHost {
    dir: PathBuf,
    after: AtomicUsize,
    /// Names of the threads that asked for the index path, in order.
    order: Mutex<Vec<String>>,
}

impl Host for TestHost {
    fn index_path(&self, project: &str, _user: Option<&str>) -> PathBuf {
        if let Some(name) = std::thread::current().name() {
            self.order.lock().unwrap().push(name.to_string());
        }
        self.dir.join(format!("{project}.db"))
    }
    fn user_active(&self, _user: &str) -> bool {
        true
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
    fn after_index(&self) {
        self.after.fetch_add(1, Ordering::SeqCst);
    }
}

fn indexer() -> (tempfile::TempDir, Arc<Indexer<TestHost>>) {
    sqlite_vec_loader::register().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let host = TestHost {
        dir: tmp.path().to_path_buf(),
        after: AtomicUsize::new(0),
        order: Mutex::new(Vec::new()),
    };
    (tmp, Arc::new(Indexer::new(host)))
}

fn long(word: &str, n: usize) -> String {
    format!("{word} sentence. ").repeat(n)
}

/// Distinct sentences, so no two chunks are the same (identical chunks are stored once).
fn numbered(n: usize) -> String {
    (0..n)
        .map(|i| format!("Line {i} of the QA notes. "))
        .collect()
}

fn vectors(texts: &[String]) -> Result<Vec<Vector>, String> {
    Ok(texts.iter().map(|_| Some(vec![1.0, 0.5, 0.25])).collect())
}

/// An embedder that tells the test it started, then waits for the test's go (first call only).
fn gated() -> (
    impl FnMut(&Target, &[String]) -> Result<Vec<Vector>, String> + Send,
    mpsc::Receiver<()>,
    mpsc::Sender<()>,
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
    (embed, started_rx, go_tx)
}

fn after(ix: &Indexer<TestHost>) -> usize {
    ix.host().after.load(Ordering::SeqCst)
}

fn spawn_tail(ix: &Arc<Indexer<TestHost>>, name: &str, head: &str) -> std::thread::JoinHandle<()> {
    let ix = Arc::clone(ix);
    let head = head.to_string();
    std::thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            let _ = ix.append_tail(
                "qa-proj",
                "a.md",
                &head,
                &[],
                None,
                (8, None, None, true),
                &mut |_: &Target, t: &[String]| vectors(t),
            );
        })
        .unwrap()
}

#[test]
fn queued_runs_of_one_file_go_first_in_first_out() {
    let (_tmp, ix) = indexer();
    let head = long("alpha", 400);
    let (mut slow, started, go) = gated();
    let a = Arc::clone(&ix);
    let h = head.clone();
    let first = std::thread::Builder::new()
        .name("index".into())
        .spawn(move || a.index_file("qa-proj", "a.md", &h, None, &mut slow))
        .unwrap();
    started.recv().unwrap();
    // Five appends queue behind the embedding run, each after the previous one joined.
    let names = ["t1", "t2", "t3", "t4", "t5"];
    let mut tails = Vec::new();
    for n in names {
        tails.push(spawn_tail(&ix, n, &head));
        std::thread::sleep(Duration::from_millis(30));
    }
    ix.host().order.lock().unwrap().clear();
    go.send(()).unwrap();
    assert!(first.join().unwrap().ok);
    for t in tails {
        t.join().unwrap();
    }
    let mut order: Vec<String> = ix
        .host()
        .order
        .lock()
        .unwrap()
        .iter()
        .filter(|n| n.starts_with('t'))
        .cloned()
        .collect();
    // A run may ask for the path more than once; what counts is the order of the runs.
    order.dedup();
    assert_eq!(order, names, "appends run in the order they were asked");
    assert_eq!(ix.tracked().0, 0, "the queue drained");
}

#[test]
fn after_index_fires_only_when_an_index_run_drains_the_queue() {
    let (_tmp, ix) = indexer();
    let mut quick = |_: &Target, t: &[String]| vectors(t);
    assert!(
        ix.index_file("qa-proj", "a.md", "small", None, &mut quick)
            .ok
    );
    assert_eq!(after(&ix), 1, "a lone index run");
    let _ = ix.append_tail(
        "qa-proj",
        "a.md",
        "small",
        &[],
        None,
        (8, None, None, true),
        &mut quick,
    );
    assert_eq!(after(&ix), 1, "a lone tail append never fires it");

    // Index run, then another index run queued behind it: fires once, for the last.
    let (mut slow, started, go) = gated();
    let a = Arc::clone(&ix);
    let one = std::thread::spawn(move || {
        a.index_file("qa-proj", "a.md", &long("alpha", 400), None, &mut slow)
    });
    started.recv().unwrap();
    let b = Arc::clone(&ix);
    let two = std::thread::spawn(move || {
        b.index_file(
            "qa-proj",
            "a.md",
            &long("bravo", 400),
            None,
            &mut |_: &Target, t: &[String]| vectors(t),
        )
    });
    std::thread::sleep(Duration::from_millis(50));
    go.send(()).unwrap();
    one.join().unwrap();
    two.join().unwrap();
    assert_eq!(
        after(&ix),
        2,
        "one more, when the second run drained the queue"
    );

    // Index run with a tail append queued behind it: the tail drains it, so nothing fires (Node's
    // index promise sees a newer queue tail and returns).
    let (mut slow, started, go) = gated();
    let a = Arc::clone(&ix);
    let head = long("charlie", 400);
    let h = head.clone();
    let one = std::thread::spawn(move || a.index_file("qa-proj", "a.md", &h, None, &mut slow));
    started.recv().unwrap();
    let tail = spawn_tail(&ix, "tail", &head);
    std::thread::sleep(Duration::from_millis(50));
    go.send(()).unwrap();
    one.join().unwrap();
    tail.join().unwrap();
    assert_eq!(after(&ix), 2);
    assert_eq!(ix.tracked(), (0, 0, 0));
}

#[test]
fn a_run_that_panics_does_not_stall_the_file() {
    let (_tmp, ix) = indexer();
    let a = Arc::clone(&ix);
    let crashed = std::thread::spawn(move || {
        a.index_file(
            "qa-proj",
            "a.md",
            &long("alpha", 400),
            None,
            &mut |_: &Target, _: &[String]| -> Result<Vec<Vector>, String> {
                panic!("embedder crashed")
            },
        )
    })
    .join();
    assert!(crashed.is_err());
    assert_eq!(ix.tracked().0, 0, "the crashed run left the queue");
    let (tx, rx) = mpsc::channel();
    let b = Arc::clone(&ix);
    std::thread::spawn(move || {
        let out = b.index_file(
            "qa-proj",
            "a.md",
            &long("bravo", 400),
            None,
            &mut |_: &Target, t: &[String]| vectors(t),
        );
        tx.send(out).unwrap();
    });
    let out = rx
        .recv_timeout(Duration::from_secs(20))
        .expect("the next run of the file must not wait for the crashed one");
    assert!(
        out.ok && out.embedded == out.stored && out.stored > 1,
        "{out:?}"
    );
}

/// Records Node's width warning; the first batch is 3 wide, later batches 4 wide.
struct Widths {
    calls: usize,
    warned: Vec<(String, Option<usize>, usize)>,
}

impl Embed for Widths {
    fn embed(&mut self, _t: &Target, texts: &[String]) -> Result<Vec<Vector>, String> {
        self.calls += 1;
        let w = if self.calls == 1 { 3 } else { 4 };
        Ok(texts.iter().map(|_| Some(vec![0.5; w])).collect())
    }
    fn width_skipped(&mut self, model: &str, got: Option<usize>, slot: usize) {
        self.warned.push((model.to_string(), got, slot));
    }
}

#[test]
fn vectors_that_do_not_fit_the_slot_are_skipped_with_the_warning() {
    let (_tmp, ix) = indexer();
    let mut widths = Widths {
        calls: 0,
        warned: Vec::new(),
    };
    let out = ix.index_file("qa-proj", "a.md", &numbered(1500), None, &mut widths);
    assert!(
        out.ok && widths.calls >= 2,
        "{out:?} calls={} warned={:?}",
        widths.calls,
        widths.warned
    );
    assert_eq!(
        out.embedded, 8,
        "only the first batch fits the slot it created"
    );
    assert!(out.stored > 8);
    assert_eq!(widths.warned.len(), widths.calls - 1);
    assert!(widths
        .warned
        .iter()
        .all(|w| *w == ("qa-model".to_string(), Some(4), 3)));
}
