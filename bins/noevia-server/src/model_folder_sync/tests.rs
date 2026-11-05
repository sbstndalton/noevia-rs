#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
use super::*;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicI64, AtomicUsize};

#[derive(Default)]
struct Shared {
    events: Mutex<Vec<String>>,
    taken: AtomicUsize,
    dropped: AtomicUsize,
    registers: AtomicUsize,
    lost: Arc<AtomicBool>,
}

impl Shared {
    fn push(&self, e: impl Into<String>) {
        self.events.lock().unwrap().push(e.into());
    }
    fn events(&self) -> Vec<String> {
        self.events.lock().unwrap().clone()
    }
}

struct FakeHold {
    sh: Arc<Shared>,
    unconfirmed: Arc<AtomicBool>,
}
impl Drop for FakeHold {
    fn drop(&mut self) {
        self.sh.dropped.fetch_add(1, Ordering::SeqCst);
    }
}
impl Hold for FakeHold {
    fn excluded(&self) -> bool {
        !self.sh.lost.load(Ordering::SeqCst)
    }
    fn may_start(&self, _: Duration) -> bool {
        self.excluded() && !self.unconfirmed.load(Ordering::SeqCst)
    }
    async fn release(self) {
        self.sh.push("release");
    }
}

#[derive(Clone)]
enum Out {
    Ok,
    MissingRevision,
    Status(u16),
}

struct Fake {
    sh: Arc<Shared>,
    files: Vec<String>,
    registered: Mutex<HashSet<String>>,
    outcomes: HashMap<String, Vec<Out>>,
    attempts: Mutex<HashMap<String, usize>>,
    busy: AtomicBool,
    refuse: AtomicBool,
    list_status: Option<u16>,
    reload_ok: AtomicBool,
    wrote: Mutex<String>,
    on_disk: Mutex<String>,
    lose_after: Option<usize>,
    unconfirmed_after: Option<usize>,
    unsettled: Mutex<HashSet<String>>,
    budget: Option<AtomicI64>,
    unconfirmed: Arc<AtomicBool>,
    list_calls: AtomicUsize,
    vanish_on_second_list: Mutex<HashSet<String>>,
}

impl Fake {
    fn new(files: &[&str]) -> Fake {
        Fake {
            sh: Arc::new(Shared::default()),
            files: files.iter().map(|s| (*s).to_owned()).collect(),
            registered: Mutex::default(),
            outcomes: HashMap::new(),
            attempts: Mutex::default(),
            busy: AtomicBool::new(false),
            refuse: AtomicBool::new(false),
            list_status: None,
            reload_ok: AtomicBool::new(true),
            wrote: Mutex::new("r".into()),
            on_disk: Mutex::new("r".into()),
            lose_after: None,
            unconfirmed_after: None,
            unsettled: Mutex::default(),
            budget: None,
            unconfirmed: Arc::default(),
            list_calls: AtomicUsize::new(0),
            vanish_on_second_list: Mutex::default(),
        }
    }
    async fn tick(&self) {
        if let Some(b) = &self.budget {
            if b.fetch_sub(1, Ordering::SeqCst) <= 0 {
                while b.load(Ordering::SeqCst) <= 0 {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            }
            tokio::task::yield_now().await;
        }
    }
}

impl Env for Arc<Fake> {
    type Hold = FakeHold;
    fn busy(&self) -> bool {
        self.busy.load(Ordering::SeqCst)
    }
    async fn take(&self) -> Result<FakeHold, Refusal> {
        self.tick().await;
        if self.refuse.load(Ordering::SeqCst) {
            return Err(Refusal::Busy);
        }
        self.sh.push("hold");
        self.sh.taken.fetch_add(1, Ordering::SeqCst);
        Ok(FakeHold {
            sh: Arc::clone(&self.sh),
            unconfirmed: Arc::clone(&self.unconfirmed),
        })
    }
    async fn list_unregistered(&self) -> Result<Vec<String>, Fault> {
        self.tick().await;
        self.sh.push("list");
        let n = self.list_calls.fetch_add(1, Ordering::SeqCst);
        if let Some(status) = self.list_status {
            return Err(Fault {
                status: Some(status),
                message: "models folder unreadable".into(),
            });
        }
        let reg = self.registered.lock().unwrap();
        let gone = self.vanish_on_second_list.lock().unwrap();
        let mut out: Vec<String> = self
            .files
            .iter()
            .filter(|f| !reg.contains(*f) && !(n % 2 == 1 && gone.contains(*f)))
            .cloned()
            .collect();
        out.sort();
        Ok(out)
    }
    async fn settled(&self, stem: &str) -> Settled {
        self.tick().await;
        if self.unsettled.lock().unwrap().contains(stem) {
            Settled::Not("a download or copy of it is still running".into())
        } else {
            Settled::Yes
        }
    }
    async fn register(&self, stem: &str) -> Result<Registered, Fault> {
        self.tick().await;
        self.sh.push(format!("register:{stem}"));
        let n = {
            let mut a = self.attempts.lock().unwrap();
            let n = a.entry(stem.to_owned()).or_default();
            *n += 1;
            *n
        };
        let script = self.outcomes.get(stem).cloned().unwrap_or(vec![Out::Ok]);
        let out = script[n.min(script.len()) - 1].clone();
        let count = self.sh.registers.fetch_add(1, Ordering::SeqCst) + 1;
        if self.lose_after == Some(count) {
            self.sh.lost.store(true, Ordering::SeqCst);
        }
        if self.unconfirmed_after == Some(count) {
            self.unconfirmed.store(true, Ordering::SeqCst);
        }
        match out {
            Out::Ok => {
                self.registered.lock().unwrap().insert(stem.to_owned());
                Ok(Registered {
                    revision: Some(self.wrote.lock().unwrap().clone()),
                })
            }
            Out::MissingRevision => Ok(Registered { revision: None }),
            Out::Status(s) => Err(Fault {
                status: Some(s),
                message: format!("scripted {s}"),
            }),
        }
    }
    async fn revision(&self) -> Option<String> {
        self.tick().await;
        Some(self.on_disk.lock().unwrap().clone())
    }
    async fn reload(&self, _: &FakeHold, _: &str) -> Reloaded {
        self.tick().await;
        self.sh.push("reload");
        if self.reload_ok.load(Ordering::SeqCst) {
            Reloaded::Yes
        } else {
            Reloaded::No
        }
    }
}

fn logger() -> (Log, Arc<Mutex<Vec<String>>>) {
    let logs: Arc<Mutex<Vec<String>>> = Arc::default();
    let sink = Arc::clone(&logs);
    (
        Arc::new(move |m: &str| sink.lock().unwrap().push(m.to_owned())),
        logs,
    )
}

fn worker(
    fake: &Arc<Fake>,
    state: Option<PathBuf>,
) -> (Worker<Arc<Fake>>, Arc<Mutex<Vec<String>>>) {
    let (log, logs) = logger();
    (Worker::new(Arc::clone(fake), state, log), logs)
}

// --- the Node oracle -------------------------------------------------------------------------------

fn compact(v: &Value) -> Value {
    match v {
        Value::Array(a) if a.len() > 20 => {
            json!({"count": a.len(), "first": a[0], "last": a[a.len()-1]})
        }
        other => other.clone(),
    }
}

/// Scenarios whose Node behaviour the move changes on purpose; each is covered by its own test below.
const INTENTIONAL: [&str; 2] = [
    // The hold excludes every tune, so the per-file busy check has nothing left to catch.
    "busy-flips-mid-scan",
    // Node reported a failing hold as a scan failure with the gate's message; here any refusal defers.
    "held-writer-other-error-is-a-scan-failure",
];

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replays_the_node_oracle() {
    let fixture: Value =
        serde_json::from_str(include_str!("../../tests/fixtures/folder-sync-node.json")).unwrap();
    let mut replayed = 0;
    for row in fixture["rows"].as_array().unwrap() {
        let name = row["name"].as_str().unwrap();
        if INTENTIONAL.contains(&name) {
            continue;
        }
        let spec = &row["spec"];
        let files: Vec<String> = match &spec["files"] {
            Value::Array(a) => a.iter().map(|s| s.as_str().unwrap().to_owned()).collect(),
            other => (0..other["count"].as_u64().unwrap())
                .map(|i| format!("draft-{i:03}"))
                .collect(),
        };
        let mut fake = Fake::new(&files.iter().map(String::as_str).collect::<Vec<_>>());
        match &spec["outcomes"] {
            Value::Object(m) => {
                for (stem, script) in m {
                    let one = |v: &Value| match v {
                        Value::Number(n) => Out::Status(n.as_u64().unwrap() as u16),
                        _ => Out::Ok,
                    };
                    let script: Vec<Out> = match script {
                        Value::Array(a) => a.iter().map(one).collect(),
                        v => vec![one(v)],
                    };
                    fake.outcomes.insert(stem.clone(), script);
                }
            }
            Value::String(_) => {
                for f in &files {
                    fake.outcomes.insert(f.clone(), vec![Out::Status(400)]);
                }
            }
            _ => {}
        }
        fake.busy.store(
            !spec["busy"].is_null() && spec["busy"] != json!(false),
            Ordering::SeqCst,
        );
        fake.list_status = spec["listStatus"].as_u64().map(|s| s as u16);
        fake.refuse
            .store(spec["writer"] == json!("refused"), Ordering::SeqCst);
        fake.reload_ok
            .store(spec["reloadOk"] == json!(true), Ordering::SeqCst);
        let fake = Arc::new(fake);
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("model-folder-sync.json");
        let (mut w, mut logs) = worker(&fake, Some(state.clone()));
        for (i, step) in spec["runs"].as_array().unwrap().iter().enumerate() {
            if let Some(b) = step.get("busy") {
                fake.busy.store(b == &json!(true), Ordering::SeqCst);
            }
            if let Some(wr) = step.get("writer") {
                fake.refuse.store(wr == &json!("refused"), Ordering::SeqCst);
            }
            if step.get("restart").is_some() {
                (w, logs) = worker(&fake, Some(state.clone()));
            }
            if let Some(d) = step.get("drop") {
                for s in d.as_array().unwrap() {
                    fake.registered
                        .lock()
                        .unwrap()
                        .insert(s.as_str().unwrap().to_owned());
                }
            }
            fake.sh.events.lock().unwrap().clear();
            logs.lock().unwrap().clear();
            let got = w.run().await.to_json();
            let want = &row["runs"][i];
            let want_result = want["result"].clone();
            let mut got_c = got.clone();
            if let Some(s) = got_c.get("skipped").cloned() {
                got_c["skipped"] = compact(&s);
            }
            assert_eq!(got_c, want_result, "{name} run {i}");
            // The events: Node's hold/release brackets and Rust's second scan under the hold are the
            // bookkeeping of the exclusion, not behaviour.
            let strip = |v: Vec<String>| {
                let mut seen_list = false;
                v.into_iter()
                    .filter(|e| e != "hold" && e != "release" && !e.starts_with('…'))
                    .filter(|e| {
                        if e == "list" {
                            let keep = !seen_list;
                            seen_list = true;
                            keep
                        } else {
                            true
                        }
                    })
                    .collect::<Vec<_>>()
            };
            let raw_want: Vec<String> = want["events"]
                .as_array()
                .unwrap()
                .iter()
                .map(|e| e.as_str().unwrap().to_owned())
                .collect();
            let got_events = strip(fake.sh.events());
            if let Some(more) = raw_want.last().and_then(|l| l.strip_prefix('…')) {
                // The recorder keeps the first six events and counts the rest.
                let rest: usize = more.split_whitespace().next().unwrap().parse().unwrap();
                assert!(
                    got_events[..6] == strip(raw_want[..6].to_vec())[..],
                    "{name} run {i} first events"
                );
                assert_eq!(got_events.len(), 6 + rest, "{name} run {i} event count");
            } else {
                assert!(got_events == strip(raw_want), "{name} run {i} events");
            }
            let want_logs: Vec<String> = want["logs"]
                .as_array()
                .unwrap()
                .iter()
                .map(|e| e.as_str().unwrap().to_owned())
                .collect();
            assert_eq!(*logs.lock().unwrap(), want_logs, "{name} run {i} logs");
            let state_now = std::fs::read_to_string(&state)
                .ok()
                .and_then(|t| serde_json::from_str::<Value>(&t).ok())
                .map(|mut v| {
                    if let Some(s) = v.get("skip").cloned() {
                        v["skip"] = compact(&s);
                    }
                    v
                });
            assert_eq!(
                state_now.unwrap_or(Value::Null),
                want["state"],
                "{name} run {i} state"
            );
            assert_eq!(
                w.skipped().len(),
                want["skipCount"].as_u64().unwrap() as usize,
                "{name} run {i} skip count"
            );
            assert_eq!(
                fake.sh.taken.load(Ordering::SeqCst),
                fake.sh.dropped.load(Ordering::SeqCst),
                "{name}: every hold is gone"
            );
        }
        replayed += 1;
    }
    assert_eq!(
        replayed,
        fixture["rows"].as_array().unwrap().len() - INTENTIONAL.len()
    );
}

// --- fail closed ----------------------------------------------------------------------------------

#[tokio::test]
async fn a_lost_exclusion_ends_the_batch_and_is_never_retaken() {
    let mut f = Fake::new(&["a", "b", "c"]);
    f.lose_after = Some(1);
    let f = Arc::new(f);
    let (w, logs) = worker(&f, None);
    let r = w.run().await;
    assert_eq!(r.added, vec!["a"]);
    assert!(r.lost && r.deferred);
    let ev = f.sh.events();
    assert!(!ev.iter().any(|e| e == "reload"), "{ev:?}");
    assert!(!ev.contains(&"register:b".to_string()), "{ev:?}");
    assert_eq!(
        f.sh.taken.load(Ordering::SeqCst),
        1,
        "no second hold in the run"
    );
    assert!(logs
        .lock()
        .unwrap()
        .iter()
        .any(|l| l.contains("exclusion ended")));
    assert_eq!(
        f.sh.taken.load(Ordering::SeqCst),
        f.sh.dropped.load(Ordering::SeqCst)
    );
}

#[tokio::test]
async fn an_unconfirmable_exclusion_starts_nothing() {
    let mut f = Fake::new(&["a", "b"]);
    f.unconfirmed_after = Some(1);
    let f = Arc::new(f);
    let (w, _) = worker(&f, None);
    let r = w.run().await;
    // Still `excluded()` for the reload decision, but no further step may start.
    assert_eq!(r.added, vec!["a"]);
    assert!(r.lost);
    assert!(!f.sh.events().contains(&"register:b".to_string()));
}

#[tokio::test]
async fn an_exclusion_lost_before_the_reload_skips_the_reload() {
    let mut f = Fake::new(&["a"]);
    f.lose_after = Some(1);
    let f = Arc::new(f);
    let (w, logs) = worker(&f, None);
    let r = w.run().await;
    assert_eq!(r.added, vec!["a"]);
    assert!(r.lost);
    assert!(!f.sh.events().iter().any(|e| e == "reload"));
    assert!(logs.lock().unwrap().last().unwrap().contains("next reload"));
}

#[tokio::test]
async fn the_reload_is_fenced_by_the_revision_the_write_answered_with() {
    let f = Fake::new(&["a"]);
    *f.on_disk.lock().unwrap() = "someone-else".into();
    let f = Arc::new(f);
    let (w, logs) = worker(&f, None);
    let r = w.run().await;
    assert_eq!(r.added, vec!["a"]);
    assert!(!f.sh.events().iter().any(|e| e == "reload"));
    assert!(logs
        .lock()
        .unwrap()
        .last()
        .unwrap()
        .contains("changed after the last registration"));
}

#[tokio::test]
async fn the_scan_is_repeated_under_the_exclusion() {
    let f = Fake::new(&["a", "b"]);
    f.vanish_on_second_list.lock().unwrap().insert("a".into());
    let f = Arc::new(f);
    let (w, _) = worker(&f, None);
    let r = w.run().await;
    assert_eq!(r.added, vec!["b"], "a went away between the scans");
    assert!(!f.sh.events().contains(&"register:a".to_string()));
}

#[tokio::test]
async fn an_unsettled_file_is_neither_registered_nor_remembered_and_is_retried() {
    let f = Fake::new(&["half", "done"]);
    f.unsettled.lock().unwrap().insert("half".into());
    let f = Arc::new(f);
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("s.json");
    let (w, logs) = worker(&f, Some(state.clone()));
    let r = w.run().await;
    assert_eq!(r.added, vec!["done"]);
    assert_eq!(r.unsettled.len(), 1);
    assert!(w.skipped().is_empty());
    assert!(!state.exists());
    assert!(logs
        .lock()
        .unwrap()
        .iter()
        .any(|l| l.contains("half is not ready")));
    f.unsettled.lock().unwrap().clear();
    let r = w.run().await;
    assert_eq!(r.added, vec!["half"]);
}

#[tokio::test]
async fn nothing_is_taken_when_everything_listed_is_already_refused() {
    let f = Arc::new(Fake::new(&["x"]));
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("s.json");
    std::fs::write(&state, r#"{"skip":["x"]}"#).unwrap();
    let (w, _) = worker(&f, Some(state));
    let r = w.run().await;
    assert!(r.added.is_empty());
    assert_eq!(f.sh.taken.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_refused_exclusion_defers_without_touching_the_skip_list() {
    let f = Fake::new(&["a"]);
    f.refuse.store(true, Ordering::SeqCst);
    let f = Arc::new(f);
    let (w, _) = worker(&f, None);
    let r = w.run().await;
    assert!(r.deferred && r.added.is_empty());
    assert!(w.skipped().is_empty());
}

// --- cancellation ---------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn aborting_at_every_await_gives_the_hold_back_and_frees_the_slot() {
    let mut reached_end = false;
    for budget in 0..60 {
        let mut f = Fake::new(&["a", "b"]);
        f.budget = Some(AtomicI64::new(budget));
        let f = Arc::new(f);
        let (w, _) = worker(&f, None);
        let w = Arc::new(w);
        let task = {
            let w = Arc::clone(&w);
            tokio::spawn(async move { w.run().await })
        };
        tokio::time::sleep(Duration::from_millis(15)).await;
        if task.is_finished() {
            reached_end = true;
            task.await.unwrap();
        } else {
            task.abort();
            let _ = task.await;
        }
        if let Some(b) = &f.budget {
            b.store(1000, Ordering::SeqCst);
        }
        // tick's blocked await is separately exercised below; the run must own the hold
        // until the effect settles, rather than treating caller cancellation as settlement.
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(
            f.sh.taken.load(Ordering::SeqCst),
            f.sh.dropped.load(Ordering::SeqCst),
            "budget {budget}: the hold is given back"
        );
        // The slot is free again, whatever happened.
        let f2 = Arc::new(Fake::new(&["c"]));
        let (w2, _) = worker(&f2, None);
        assert_eq!(w2.run().await.added, vec!["c"]);
        assert!(
            !w.running.load(Ordering::SeqCst),
            "budget {budget}: run slot cleared"
        );
    }
    assert!(reached_end, "the budgets reach past the last await");
}

#[tokio::test]
async fn a_second_run_while_one_is_in_flight_does_nothing() {
    let mut f = Fake::new(&["a"]);
    f.budget = Some(AtomicI64::new(1_000_000));
    let f = Arc::new(f);
    let (w, _) = worker(&f, None);
    let slot = RunSlot::take(&w.running).unwrap();
    let r = w.run().await;
    assert_eq!(r.to_json(), json!({"added": [], "skipped": []}));
    drop(slot);
    assert_eq!(w.run().await.added, vec!["a"]);
}

// --- state file -----------------------------------------------------------------------------------

#[tokio::test]
async fn the_state_file_is_node_compatible_private_and_survives_a_restart() {
    use std::os::unix::fs::PermissionsExt;
    let mut f = Fake::new(&["draft"]);
    f.outcomes.insert("draft".into(), vec![Out::Status(400)]);
    let f = Arc::new(f);
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("model-folder-sync.json");
    let (w, _) = worker(&f, Some(state.clone()));
    w.run().await;
    assert_eq!(
        std::fs::read_to_string(&state).unwrap(),
        r#"{"skip":["draft"]}"#
    );
    assert_eq!(
        std::fs::metadata(&state).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let (w2, _) = worker(&f, Some(state.clone()));
    assert_eq!(w2.skipped(), vec!["draft"]);
    // A file Node wrote is read as is; garbage is a first run.
    std::fs::write(&state, r#"{"skip":["x","y"]}"#).unwrap();
    assert_eq!(worker(&f, Some(state.clone())).0.skipped(), vec!["x", "y"]);
    std::fs::write(&state, "not json").unwrap();
    assert!(worker(&f, Some(state)).0.skipped().is_empty());
}

#[tokio::test(start_paused = true)]
async fn the_timer_scans_first_then_on_the_interval_and_stops() {
    let f = Arc::new(Fake::new(&[]));
    let (w, _) = worker(&f, None);
    let (tx, rx) = tokio::sync::watch::channel(false);
    let handle = spawn(Arc::new(w), Schedule::default(), rx);
    tokio::time::sleep(FIRST_DELAY - Duration::from_secs(1)).await;
    assert_eq!(f.list_calls.load(Ordering::SeqCst), 0);
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(f.list_calls.load(Ordering::SeqCst), 1);
    tokio::time::sleep(INTERVAL).await;
    assert_eq!(f.list_calls.load(Ordering::SeqCst), 2);
    tx.send(true).unwrap();
    handle.await.unwrap();
}

#[test]
fn the_marker_is_the_one_features_carries() {
    assert!(crate::FEATURES.contains(&MARKER));
}

#[test]
fn encode_component_is_encode_uri_component() {
    assert_eq!(
        live::encode_component("a b/é(1)!~*'"),
        "a%20b%2F%C3%A9(1)!~*'"
    );
}

// --- the settled-file check -----------------------------------------------------------------------

mod settled_files {
    use super::super::files::{probe, stable, stem_of};
    use super::super::Settled;
    use std::path::Path;
    use std::time::{Duration, SystemTime};

    fn gguf(path: &Path, age: Duration) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, [b"GGUF".as_slice(), &[0u8; 60]].concat()).unwrap();
        let f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        f.set_modified(SystemTime::now() - age).unwrap();
    }
    const OLD: Duration = Duration::from_secs(3600);
    fn look(root: &Path, stem: &str) -> Result<super::super::files::Seen, String> {
        probe(root, stem, SystemTime::now(), Duration::from_secs(120))
    }
    fn root() -> (tempfile::TempDir, std::path::PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let r = std::fs::canonicalize(d.path()).unwrap();
        (d, r)
    }

    #[test]
    fn stems_follow_the_managers_naming() {
        assert_eq!(stem_of("m.gguf").as_deref(), Some("m"));
        assert_eq!(stem_of("M.GGUF").as_deref(), Some("M"));
        assert_eq!(stem_of("m-00001-of-00003.gguf").as_deref(), Some("m"));
        assert_eq!(stem_of("m-1-of-3.gguf").as_deref(), Some("m-1-of-3"));
        assert_eq!(stem_of("m.bin"), None);
        assert_eq!(stem_of("m.gguf.download"), None);
    }

    #[test]
    fn a_complete_old_file_is_settled_when_two_looks_agree() {
        let (_d, r) = root();
        gguf(&r.join("sub/m.gguf"), OLD);
        let (a, b) = (look(&r, "m"), look(&r, "m"));
        assert!(a.is_ok(), "{a:?}");
        assert_eq!(stable(&a, &b), Settled::Yes);
    }

    #[test]
    fn a_file_that_changed_between_looks_is_not_settled() {
        let (_d, r) = root();
        gguf(&r.join("m.gguf"), OLD);
        let a = look(&r, "m");
        std::fs::OpenOptions::new()
            .append(true)
            .open(r.join("m.gguf"))
            .unwrap();
        std::fs::write(r.join("m.gguf"), [b"GGUF".as_slice(), &[0u8; 90]].concat()).unwrap();
        let f = std::fs::OpenOptions::new()
            .write(true)
            .open(r.join("m.gguf"))
            .unwrap();
        f.set_modified(SystemTime::now() - OLD).unwrap();
        let b = look(&r, "m");
        assert!(matches!(stable(&a, &b), Settled::Not(w) if w.contains("still changing")));
    }

    #[test]
    fn partial_downloads_and_copies_are_refused() {
        let (_d, r) = root();
        gguf(&r.join("young.gguf"), Duration::from_secs(5));
        assert!(look(&r, "young").unwrap_err().contains("recently"));
        for temp in [
            "m.gguf.download",
            "m.gguf.part",
            "m.gguf.crdownload",
            ".m.gguf.Ab12Cd",
        ] {
            let (_d2, r2) = root();
            gguf(&r2.join("m.gguf"), OLD);
            std::fs::write(r2.join(temp), b"x").unwrap();
            assert!(
                look(&r2, "m").unwrap_err().contains("still running"),
                "{temp}"
            );
        }
        std::fs::write(r.join("short.gguf"), b"GGUF").unwrap();
        assert!(look(&r, "short").unwrap_err().contains("too small"));
        let header_less = r.join("none.gguf");
        std::fs::write(&header_less, vec![0u8; 64]).unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&header_less)
            .unwrap()
            .set_modified(SystemTime::now() - OLD)
            .unwrap();
        assert!(look(&r, "none").unwrap_err().contains("no GGUF header"));
        // A modification time in the future is not an old file.
        gguf(&r.join("future.gguf"), Duration::from_secs(1));
        std::fs::OpenOptions::new()
            .write(true)
            .open(r.join("future.gguf"))
            .unwrap()
            .set_modified(SystemTime::now() + OLD)
            .unwrap();
        assert!(look(&r, "future").is_err());
    }

    #[test]
    fn every_shard_of_a_stem_must_be_complete() {
        let (_d, r) = root();
        gguf(&r.join("m-00001-of-00002.gguf"), OLD);
        gguf(&r.join("m-00002-of-00002.gguf"), Duration::from_secs(3));
        assert!(look(&r, "m").is_err());
        gguf(&r.join("m-00002-of-00002.gguf"), OLD);
        assert_eq!(look(&r, "m").unwrap().files_len(), 2);
    }

    #[test]
    fn a_link_is_never_followed_and_nothing_outside_the_root_is_read() {
        let (_d, r) = root();
        let outside = tempfile::tempdir().unwrap();
        gguf(&outside.path().join("m.gguf"), OLD);
        // A link to a file, a link to a folder holding one, a link from inside to outside.
        std::os::unix::fs::symlink(outside.path().join("m.gguf"), r.join("m.gguf")).unwrap();
        std::os::unix::fs::symlink(outside.path(), r.join("folder")).unwrap();
        assert!(look(&r, "m").unwrap_err().contains("no complete file"));
        // A real file next to the links is still found.
        gguf(&r.join("real/m.gguf"), OLD);
        assert!(look(&r, "m").is_ok());
        // A link to a file inside the root is a link too.
        let (_d2, r2) = root();
        gguf(&r2.join("a/target.gguf"), OLD);
        std::os::unix::fs::symlink(r2.join("a/target.gguf"), r2.join("alias.gguf")).unwrap();
        assert!(look(&r2, "alias").is_err());
        // A directory named like a model is not a file.
        std::fs::create_dir_all(r2.join("dir.gguf")).unwrap();
        assert!(look(&r2, "dir").is_err());
    }

    #[test]
    fn a_missing_file_is_not_a_complete_one() {
        let (_d, r) = root();
        assert!(look(&r, "ghost").is_err());
        assert!(look(&r.join("nope"), "ghost").is_err());
    }
}

#[tokio::test]
async fn a_final_registration_without_revision_must_not_reuse_an_earlier_revision() {
    let mut f = Fake::new(&["a", "b"]);
    f.outcomes.insert("b".into(), vec![Out::MissingRevision]);
    let f = Arc::new(f);
    let (w, _) = worker(&f, None);
    w.run().await;
    assert!(!f.sh.events().contains(&"reload".to_owned()));
}
