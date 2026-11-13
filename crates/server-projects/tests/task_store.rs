//! crates/server-projects task_store: every operation core's own jobs.cjs ran in
//! tests/fixtures/task-store/node.v1.json (tools/gen-task-store-fixtures.cjs, synthetic data, the
//! script's clock), replayed through the Rust store in a fresh directory. Each answer and, after
//! every step, every file in jobs/ must match byte for byte. Where the Rust store differs on purpose
//! (task_store.rs, "Fixed on purpose"), the fixture's Node answer shows the defect and the Rust
//! answer is checked against its own expectation here.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use js_json::{wtf16, JValue};
use server_projects::task_store::{NewJob, Options, Refusal, Store};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/task-store/node.v1.json"
);

fn fixture() -> JValue {
    wtf16::parse(&fs::read_to_string(FIXTURE).unwrap()).unwrap()
}

fn text(v: &JValue) -> String {
    wtf16::stringify(v).unwrap()
}

fn options(spec: &JValue) -> Options {
    let mut o = Options::default();
    if let JValue::Arr(kinds) = spec.get("kinds") {
        o.kinds = Some(
            kinds
                .iter()
                .map(|k| k.as_str().unwrap().to_string())
                .collect(),
        );
    }
    o.durable = spec.get("durable").as_bool().unwrap_or(false);
    if let Some(ms) = spec.get("retainMs").as_num() {
        o.retain_ms = ms;
    }
    if let Some(n) = spec.get("maxJobs").as_num() {
        o.max_jobs = n as usize;
    }
    o
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

/// `{id, kind, status, updatedAt}` of a job, as the generator's `brief`.
fn brief(job: Option<server_projects::task_journals::Job>) -> JValue {
    match job {
        None => JValue::Null,
        Some(j) => JValue::obj([
            ("id", j.id),
            ("kind", j.kind),
            ("status", j.status),
            ("updatedAt", j.updated_at),
        ]),
    }
}

fn answer(r: Result<JValue, Refusal>) -> JValue {
    match r {
        Ok(v) => JValue::obj([("ok", v)]),
        Err(e) => JValue::obj([(
            "error",
            JValue::obj([
                (
                    "status",
                    e.status.map_or(JValue::Null, |s| JValue::Num(f64::from(s))),
                ),
                ("message", JValue::Str(e.message)),
            ]),
        )]),
    }
}

struct Run {
    dir: tempfile::TempDir,
    clock: Arc<AtomicU64>,
    next_id: Arc<Mutex<Option<String>>>,
    specs: JValue,
    stores: BTreeMap<String, Store>,
    expected: BTreeMap<String, Vec<u8>>,
}

impl Run {
    fn new(specs: &JValue) -> Run {
        Run {
            dir: tempfile::tempdir().unwrap(),
            clock: Arc::new(AtomicU64::new(0f64.to_bits())),
            next_id: Arc::new(Mutex::new(None)),
            specs: specs.clone(),
            stores: BTreeMap::new(),
            expected: BTreeMap::new(),
        }
    }

    fn store(&mut self, name: &str) -> &Store {
        if !self.stores.contains_key(name) {
            let clock = self.clock.clone();
            let ids = self.next_id.clone();
            let store = Store::with(
                self.dir.path(),
                options(self.specs.get(name)),
                Box::new(move || f64::from_bits(clock.load(Ordering::SeqCst))),
                Box::new(move || ids.lock().unwrap().take().expect("no uuid set")),
            );
            self.stores.insert(name.to_string(), store);
        }
        &self.stores[name]
    }

    fn jobs(&self) -> std::path::PathBuf {
        self.dir.path().join("jobs")
    }

    /// One step; its answer as the generator records it (`None` for steps without one).
    fn step(&mut self, s: &JValue) -> Option<JValue> {
        let op = s.get("op").as_str().unwrap();
        let store_name = s.get("store").as_str().unwrap_or("").to_string();
        let id = s.get("id").as_str().unwrap_or("").to_string();
        match op {
            "clock" => {
                let t = s.get("t").as_num().unwrap();
                self.clock.store(t.to_bits(), Ordering::SeqCst);
                None
            }
            "restart" => {
                self.stores.clear();
                None
            }
            "seed" => {
                fs::create_dir_all(self.jobs()).unwrap();
                let bytes = match s.get("hex").as_str() {
                    Some(h) => unhex(h),
                    None => s.get("text").as_str().unwrap().as_bytes().to_vec(),
                };
                fs::write(self.jobs().join(s.get("name").as_str().unwrap()), bytes).unwrap();
                None
            }
            "create" => {
                *self.next_id.lock().unwrap() = s.get("uuid").as_str().map(str::to_string);
                let caps: Vec<String> = match s.get("capabilities") {
                    JValue::Arr(a) => a.iter().map(|c| c.as_str().unwrap().to_string()).collect(),
                    _ => Vec::new(),
                };
                let kind = s.get("kind").as_str().unwrap().to_string();
                let project = s.get("projectId").as_str().map(str::to_string);
                let parent = s.get("parentId").as_str().map(str::to_string);
                let r = self.store(&store_name).create(NewJob {
                    kind: &kind,
                    project_id: project.as_deref(),
                    parent_id: parent.as_deref(),
                    capabilities: &caps,
                });
                *self.next_id.lock().unwrap() = None;
                Some(answer(r.map(JValue::Str)))
            }
            "append" => {
                let data = match s.get("data").as_str() {
                    Some(t) => wtf16::parse(t).unwrap(),
                    None => JValue::Undefined,
                };
                let kind = s.get("type").as_str().unwrap().to_string();
                Some(answer(self.store(&store_name).append(&id, &kind, data)))
            }
            "get" => Some(answer(self.store(&store_name).get(&id).map(brief))),
            "cancel" => Some(answer(
                self.store(&store_name).cancel(&id, &|_| false).map(brief),
            )),
            "recover" => Some(answer(
                self.store(&store_name)
                    .recover(&|_| false)
                    .map(|n| JValue::Num(n as f64)),
            )),
            "prune" => Some(answer(
                self.store(&store_name).prune().map(|()| JValue::Null),
            )),
            other => panic!("unknown op {other}"),
        }
    }

    /// Applies the generator's record of what changed to the expected directory.
    fn expect_files(&mut self, files: &JValue) {
        if let JValue::Obj(written) = files.get("written") {
            for (name, change) in written {
                let appended = !matches!(change.get("appended"), JValue::Undefined);
                let key = if appended { "appended" } else { "whole" };
                let bytes = match change.get("hex").as_str() {
                    Some(h) => unhex(h),
                    None => change.get(key).as_str().unwrap().as_bytes().to_vec(),
                };
                if appended {
                    self.expected.entry(name.clone()).or_default().extend(bytes);
                } else {
                    self.expected.insert(name.clone(), bytes);
                }
            }
        }
        if let JValue::Arr(removed) = files.get("removed") {
            for name in removed {
                self.expected.remove(name.as_str().unwrap());
            }
        }
    }

    fn actual_files(&self) -> BTreeMap<String, Vec<u8>> {
        let mut out = BTreeMap::new();
        if let Ok(entries) = fs::read_dir(self.jobs()) {
            for e in entries.flatten() {
                out.insert(
                    e.file_name().into_string().unwrap(),
                    fs::read(e.path()).unwrap(),
                );
            }
        }
        out
    }
}

fn steps(f: &JValue, name: &str) -> Vec<JValue> {
    match f.get("scenarios").get(name) {
        JValue::Arr(a) => a.clone(),
        _ => panic!("no scenario {name}"),
    }
}

fn show(files: &BTreeMap<String, Vec<u8>>) -> BTreeMap<String, String> {
    files
        .iter()
        .map(|(k, v)| (k.clone(), String::from_utf8_lossy(v).into_owned()))
        .collect()
}

/// Replays a scenario, comparing every answer and the directory after every step; stops before
/// step `stop` when given.
fn replay(f: &JValue, name: &str, stop: Option<usize>) -> Run {
    let mut run = Run::new(f.get("stores"));
    for (i, s) in steps(f, name).iter().enumerate() {
        if Some(i) == stop {
            break;
        }
        let got = run.step(s);
        if let Some(got) = got {
            assert_eq!(
                text(&got),
                text(s.get("result")),
                "{name} step {i}: {}",
                text(s)
            );
        }
        run.expect_files(s.get("files"));
        assert_eq!(
            show(&run.actual_files()),
            show(&run.expected),
            "{name} step {i}: files"
        );
    }
    run
}

#[test]
fn fixture_records_its_sources() {
    // The Node sources the fixture was recorded from (core a72ae94). A fixture regenerated from
    // other sources fails here until these are updated on purpose.
    let f = fixture();
    for (name, sha) in [
        (
            "jobs.cjs",
            "3a04aea9e594428d62cb5dc7f29eb937d322c6b24d83c9b2d9b08211366e5457",
        ),
        (
            "code-plan.cjs",
            "966f6d39054b2948a6796cdbf1f8310c1a15ac461c56e63523ecaa3fe6b860f2",
        ),
        (
            "code-review-verdict.cjs",
            "fde7e49afb0a60ee035036d83fc984426017d9519c283d55023b2e8773c328e2",
        ),
        (
            "task-lifecycle.cjs",
            "f99c9f9387f647fd579fbcd3acc6c6a19531995313cd4cc6894b333633ec5da1",
        ),
    ] {
        assert_eq!(f.get("sources").get(name).as_str(), Some(sha), "{name}");
    }
}

#[test]
fn every_node_scenario_replays_byte_for_byte() {
    let f = fixture();
    let JValue::Obj(all) = f.get("scenarios") else {
        panic!("no scenarios")
    };
    let mut seen = 0;
    for (name, _) in all {
        match name.as_str() {
            // Differences, below.
            "torn-differences" | "prune-differences" => continue,
            // Its last step is a second job.created, which only Node takes.
            "refusals" => {
                let last = steps(&f, name).len() - 1;
                replay(&f, name, Some(last));
            }
            _ => {
                replay(&f, name, None);
            }
        }
        seen += 1;
    }
    assert!(seen >= 10, "{seen} scenarios");
}

#[test]
fn difference_a_second_job_created_is_refused() {
    let f = fixture();
    let all = steps(&f, "refusals");
    let last = all.last().unwrap();
    // Node took it and so replaced the live job's kind and capabilities.
    assert_eq!(
        last.get("result").get("ok").get("type").as_str(),
        Some("job.created")
    );
    let mut run = replay(&f, "refusals", Some(all.len() - 1));
    let before = run.actual_files();
    let got = run.step(last).unwrap();
    assert_eq!(
        text(&got),
        r#"{"error":{"status":null,"message":"A job is created with create()"}}"#
    );
    assert_eq!(run.actual_files(), before);
}

/// The torn-tail differences: Node's answers after a restart show the defect, the Rust store keeps
/// every row it counted and never writes after a damaged line.
#[test]
fn differences_on_a_torn_tail() {
    let f = fixture();
    let all = steps(&f, "torn-differences");
    let node: Vec<String> = all
        .iter()
        .filter(|s| !matches!(s.get("result"), JValue::Undefined))
        .map(|s| text(s.get("result")))
        .collect();
    // After the restart Node lost `job.started` (queued) and read the other two as damaged.
    assert!(node[3].contains(r#""status":"queued""#), "{}", node[3]);
    assert!(node[6].contains("Unreadable job journal"), "{}", node[6]);
    assert!(node[9].contains("Unreadable job journal"), "{}", node[9]);

    let mut run = Run::new(f.get("stores"));
    let mut got = Vec::new();
    for s in &all {
        if let Some(a) = run.step(s) {
            got.push(text(&a));
        }
    }
    let a = "00000000-0000-4000-8000-000000000001";
    let b = "00000000-0000-4000-8000-000000000002";
    let c = "00000000-0000-4000-8000-000000000003";
    // The answers before each restart are Node's.
    for i in [0, 1, 2, 4, 5, 7] {
        assert_eq!(got[i], node[i], "answer {i}");
    }
    assert_eq!(
        got[3],
        format!(r#"{{"ok":{{"id":"{a}","kind":"code","status":"running","updatedAt":12000}}}}"#)
    );
    assert_eq!(
        got[6],
        format!(r#"{{"ok":{{"id":"{b}","kind":"code","status":"running","updatedAt":12000}}}}"#)
    );
    // The line that does not parse but ends in a newline is damage: nothing is written after it,
    // and the journal still reads (the bad last line dropped, as Node's reader drops it).
    assert_eq!(
        got[8],
        r#"{"error":{"status":409,"message":"Unreadable job journal; review required"}}"#
    );
    assert_eq!(
        got[9],
        format!(r#"{{"ok":{{"id":"{c}","kind":"code","status":"queued","updatedAt":10}}}}"#)
    );
    // The row that lost its newline is kept, then the new one.
    let journal = fs::read_to_string(run.jobs().join(format!("{a}.jsonl"))).unwrap();
    let lines: Vec<&str> = journal.split_inclusive('\n').collect();
    assert_eq!(lines.len(), 3);
    assert!(lines[1].contains(r#""seq":2,"type":"job.started""#));
    assert!(lines[2].starts_with(&format!(r#"{{"job":"{a}","seq":3,"type":"progress""#)));
    // The line that did not parse is still the last one.
    let journal = fs::read_to_string(run.jobs().join(format!("{c}.jsonl"))).unwrap();
    assert!(journal.ends_with("not json\n"));
    assert_eq!(journal.lines().count(), 2);
}

#[test]
fn difference_prune_removes_a_journal_by_its_own_name() {
    let f = fixture();
    let all = steps(&f, "prune-differences");
    // Node: the derived id of a journal without job.created is null, so prune, and every create
    // after it, throws 400.
    assert_eq!(
        text(all[3].get("result")),
        r#"{"error":{"status":400,"message":"Invalid job id"}}"#
    );
    assert_eq!(
        text(all[4].get("result")),
        r#"{"error":{"status":400,"message":"Invalid job id"}}"#
    );
    let mut run = Run::new(f.get("stores"));
    let mut got = Vec::new();
    for s in &all {
        if let Some(a) = run.step(s) {
            got.push(text(&a));
        }
    }
    assert_eq!(got[1], r#"{"ok":null}"#);
    assert_eq!(got[2], r#"{"ok":"00000000-0000-4000-8000-000000000003"}"#);
    let left: Vec<String> = run.actual_files().into_keys().collect();
    assert_eq!(
        left,
        vec![
            "00000000-0000-4000-8000-000000000001.jsonl".to_string(),
            "00000000-0000-4000-8000-000000000003.jsonl".to_string()
        ]
    );
}

#[test]
fn data_is_written_in_js_property_order() {
    let dir = tempfile::tempdir().unwrap();
    let s = store(dir.path(), Options::default());
    let id = s
        .create(NewJob {
            kind: "code",
            project_id: None,
            parent_id: None,
            capabilities: &[],
        })
        .unwrap();
    // As a JS object holds it: index keys first, ascending; a repeated key's last value in its
    // first place; at every level.
    let data = JValue::obj([
        ("b", JValue::Num(1.0)),
        ("2", JValue::Str("two".into())),
        ("b", JValue::Num(3.0)),
        (
            "nested",
            JValue::Arr(vec![JValue::obj([
                ("z", JValue::Null),
                ("10", JValue::Null),
                ("1", JValue::Null),
            ])]),
        ),
    ]);
    let event = s.append(&id, "progress", data).unwrap();
    assert_eq!(
        text(event.get("data")),
        r#"{"2":"two","b":3,"nested":[{"1":null,"10":null,"z":null}]}"#
    );
    // A chained journal keeps verifying after such a row.
    let dir = tempfile::tempdir().unwrap();
    let durable = Options {
        durable: true,
        ..Options::default()
    };
    let s = store(dir.path(), durable);
    let id = s
        .create(NewJob {
            kind: "code",
            project_id: None,
            parent_id: None,
            capabilities: &[],
        })
        .unwrap();
    let data = JValue::obj([("b", JValue::Num(1.0)), ("2", JValue::Num(2.0))]);
    s.append(&id, "progress", data).unwrap();
    assert_eq!(s.get(&id).unwrap().unwrap().status.as_str(), Some("queued"));
    s.append(&id, "job.started", JValue::Undefined).unwrap();
}

#[test]
fn prune_with_times_that_are_not_numbers_does_not_panic() {
    let dir = tempfile::tempdir().unwrap();
    let jobs = dir.path().join("jobs");
    fs::create_dir_all(&jobs).unwrap();
    for n in 0..40u32 {
        let id = format!("00000000-0000-4000-8000-{n:012}");
        let at = match n % 3 {
            0 => "null".to_string(),
            1 => r#""x""#.to_string(),
            _ => (100 + n).to_string(),
        };
        let at = if n % 5 == 0 {
            String::new()
        } else {
            format!(r#","at":{at}"#)
        };
        let created = format!(
            r#"{{"job":"{id}","seq":1,"type":"job.created","at":1,"data":{{"kind":"code"}}}}"#
        );
        let done = format!(r#"{{"job":"{id}","seq":2,"type":"job.completed"{at},"data":{{}}}}"#);
        fs::write(
            jobs.join(format!("{id}.jsonl")),
            format!("{created}\n{done}\n"),
        )
        .unwrap();
    }
    let options = Options {
        max_jobs: 5,
        retain_ms: 1e12,
        ..Options::default()
    };
    let s = store(dir.path(), options);
    s.prune().unwrap();
    assert_eq!(s.ids().len(), 5);
}

#[test]
fn a_cancel_that_meets_a_finished_run_keeps_its_end() {
    let dir = tempfile::tempdir().unwrap();
    let s = store(dir.path(), Options::default());
    let id = s
        .create(NewJob {
            kind: "code",
            project_id: None,
            parent_id: None,
            capabilities: &[],
        })
        .unwrap();
    s.append(&id, "job.started", JValue::Undefined).unwrap();
    // The run ends while the host is asked (no lock is held then), and the host has no run left.
    let job = s
        .cancel(&id, &|x| {
            s.append(
                x,
                "job.completed",
                JValue::obj([("result", JValue::Num(1.0))]),
            )
            .unwrap();
            false
        })
        .unwrap()
        .unwrap();
    assert_eq!(job.status.as_str(), Some("completed"));
}

#[test]
fn difference_a_full_or_hard_linked_journal_takes_no_row() {
    let dir = tempfile::tempdir().unwrap();
    let s = store(dir.path(), Options::default());
    let id = s
        .create(NewJob {
            kind: "code",
            project_id: None,
            parent_id: None,
            capabilities: &[],
        })
        .unwrap();
    let path = dir.path().join("jobs").join(format!("{id}.jsonl"));
    // Just under the reader's limit: one more row would put it over.
    let pad = "x".repeat(64 * 1024 * 1024 - fs::metadata(&path).unwrap().len() as usize - 200);
    let row =
        format!(r#"{{"job":"{id}","seq":2,"type":"progress","at":2,"data":{{"p":"{pad}"}}}}"#);
    let mut f = fs::OpenOptions::new().append(true).open(&path).unwrap();
    std::io::Write::write_all(&mut f, format!("{row}\n").as_bytes()).unwrap();
    drop(f);
    let before = fs::metadata(&path).unwrap().len();
    let e = s
        .append(
            &id,
            "progress",
            JValue::obj([("stage", JValue::Str("y".repeat(300)))]),
        )
        .unwrap_err();
    assert_eq!(e.message, "The job journal is full");
    assert_eq!(fs::metadata(&path).unwrap().len(), before);

    #[cfg(unix)]
    {
        let id = s
            .create(NewJob {
                kind: "code",
                project_id: None,
                parent_id: None,
                capabilities: &[],
            })
            .unwrap();
        let path = dir.path().join("jobs").join(format!("{id}.jsonl"));
        fs::hard_link(&path, dir.path().join("other.jsonl")).unwrap();
        let e = s.append(&id, "job.started", JValue::Undefined).unwrap_err();
        assert!(e.is_unreadable());
    }
}

fn store(dir: &Path, options: Options) -> Store {
    let n = Arc::new(AtomicU64::new(0));
    Store::with(
        dir,
        options,
        Box::new(|| 5000.0),
        Box::new(move || {
            format!(
                "00000000-0000-4000-8000-{:012}",
                n.fetch_add(1, Ordering::SeqCst) + 1
            )
        }),
    )
}

#[test]
fn difference_event_data_is_an_object() {
    let dir = tempfile::tempdir().unwrap();
    let s = store(dir.path(), Options::default());
    let id = s
        .create(NewJob {
            kind: "code",
            project_id: None,
            parent_id: None,
            capabilities: &[],
        })
        .unwrap();
    for data in [
        JValue::Null,
        JValue::Num(5.0),
        JValue::Str("x".into()),
        JValue::Arr(vec![]),
    ] {
        let e = s.append(&id, "progress", data).unwrap_err();
        assert_eq!(e.message, "Job event data is an object");
        assert_eq!(e.http_status(), 500);
    }
    assert_eq!(
        fs::read_to_string(dir.path().join("jobs").join(format!("{id}.jsonl")))
            .unwrap()
            .lines()
            .count(),
        1
    );
}

#[cfg(unix)]
#[test]
fn difference_a_linked_journal_is_neither_read_nor_written() {
    let dir = tempfile::tempdir().unwrap();
    let outside = dir.path().join("outside.jsonl");
    let id = "00000000-0000-4000-8000-000000000009";
    let row =
        format!(r#"{{"job":"{id}","seq":1,"type":"job.created","at":1,"data":{{"kind":"code"}}}}"#);
    fs::write(&outside, format!("{row}\n")).unwrap();
    fs::create_dir_all(dir.path().join("jobs")).unwrap();
    std::os::unix::fs::symlink(
        &outside,
        dir.path().join("jobs").join(format!("{id}.jsonl")),
    )
    .unwrap();
    let s = store(dir.path(), Options::default());
    assert!(s.get(id).unwrap_err().is_unreadable());
    assert!(s
        .append(id, "job.started", JValue::Undefined)
        .unwrap_err()
        .is_unreadable());
    assert_eq!(fs::read_to_string(&outside).unwrap(), format!("{row}\n"));
}

#[test]
fn difference_a_last_line_this_port_cannot_hold_refuses_the_write() {
    let dir = tempfile::tempdir().unwrap();
    let id = "00000000-0000-4000-8000-000000000009";
    let created =
        format!(r#"{{"job":"{id}","seq":1,"type":"job.created","at":1,"data":{{"kind":"code"}}}}"#);
    // JSON.parse reads 1e400 as Infinity; the Rust reader cannot, so it must not cut the line off.
    let odd = format!(r#"{{"job":"{id}","seq":2,"type":"progress","at":2,"data":{{"n":1e400}}}}"#);
    fs::create_dir_all(dir.path().join("jobs")).unwrap();
    let path = dir.path().join("jobs").join(format!("{id}.jsonl"));
    fs::write(&path, format!("{created}\n{odd}\n")).unwrap();
    let s = store(dir.path(), Options::default());
    assert!(s
        .append(id, "job.started", JValue::Undefined)
        .unwrap_err()
        .is_unreadable());
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        format!("{created}\n{odd}\n")
    );
}

#[cfg(unix)]
#[test]
fn new_directory_and_journal_modes_are_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let s = store(dir.path(), Options::default());
    let id = s
        .create(NewJob {
            kind: "code",
            project_id: Some("qa-project"),
            parent_id: None,
            capabilities: &[],
        })
        .unwrap();
    let jobs = dir.path().join("jobs");
    assert_eq!(fs::metadata(&jobs).unwrap().permissions().mode() & 0o077, 0);
    let file = jobs.join(format!("{id}.jsonl"));
    assert_eq!(fs::metadata(&file).unwrap().permissions().mode() & 0o177, 0);
}

#[test]
fn a_live_run_is_aborted_not_written() {
    let dir = tempfile::tempdir().unwrap();
    let s = store(dir.path(), Options::default());
    let id = s
        .create(NewJob {
            kind: "code",
            project_id: None,
            parent_id: None,
            capabilities: &[],
        })
        .unwrap();
    s.append(&id, "job.started", JValue::Undefined).unwrap();
    let asked = Mutex::new(Vec::new());
    let job = s
        .cancel(&id, &|x| {
            asked.lock().unwrap().push(x.to_string());
            true
        })
        .unwrap()
        .unwrap();
    assert_eq!(job.status.as_str(), Some("running"));
    assert_eq!(asked.into_inner().unwrap(), vec![id.clone()]);
    // recover skips what the host still runs.
    assert_eq!(s.recover(&|x| x == id).unwrap(), 0);
    assert_eq!(s.recover(&|_| false).unwrap(), 1);
    assert_eq!(
        s.get(&id).unwrap().unwrap().status.as_str(),
        Some("interrupted")
    );
}

#[test]
fn concurrent_appends_keep_one_sequence() {
    let dir = tempfile::tempdir().unwrap();
    let s = Arc::new(store(dir.path(), Options::default()));
    let id = s
        .create(NewJob {
            kind: "code",
            project_id: None,
            parent_id: None,
            capabilities: &[],
        })
        .unwrap();
    let handles: Vec<_> = (0..8)
        .map(|t| {
            let s = s.clone();
            let id = id.clone();
            std::thread::spawn(move || {
                for i in 0..20 {
                    let data = JValue::obj([("stage", JValue::Str(format!("t{t}-{i}")))]);
                    s.append(&id, "progress", data).unwrap();
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    let journal = fs::read_to_string(dir.path().join("jobs").join(format!("{id}.jsonl"))).unwrap();
    for (i, line) in journal.lines().enumerate() {
        let row = wtf16::parse(line).unwrap();
        assert_eq!(row.get("seq").as_num(), Some((i + 1) as f64));
    }
    assert_eq!(journal.lines().count(), 161);
}
