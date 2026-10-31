//! crates/server-projects task_journals: the answers core's own jobs.cjs + view() gave for the same bytes
//! (tests/fixtures/task-journals, written once by running them on synthetic events), then reading beside a live
//! writer with jobs.cjs's exact write mechanics.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use js_json::JValue;
use serde_json::Value;
use server_projects::task_journals::{self as tj, Found, Loaded, Surface};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const USER: &str = "00000000-0000-4000-8000-00000000aaaa";
const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/task-journals");

fn id(n: u32) -> String {
    format!("00000000-0000-4000-8000-{n:012}")
}

fn event(job: &str, seq: u32, kind: &str, data: &str) -> String {
    format!(
        r#"{{"job":"{job}","seq":{seq},"type":"{kind}","at":{},"data":{data}}}"#,
        1000 + seq
    )
}

fn created(job: &str, kind: &str, project: &str) -> String {
    event(
        job,
        1,
        "job.created",
        &format!(r#"{{"kind":"{kind}","projectId":"{project}","capabilities":[]}}"#),
    )
}

fn jobs_dir(data: &Path) -> PathBuf {
    let dir = data.join("users").join(USER).join("jobs");
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn view_value(job: &tj::Job) -> Value {
    let text = js_json::wtf16::stringify(&tj::code_view(job, &JValue::Null)).unwrap();
    serde_json::from_str(&text).unwrap()
}

#[test]
fn every_view_matches_what_node_answered_for_the_same_journal() {
    let expected: Value =
        serde_json::from_str(&fs::read_to_string(format!("{FIXTURES}/expected.json")).unwrap())
            .unwrap();
    let cases = expected.as_object().unwrap();
    assert!(cases.len() >= 8);
    for (name, case) in cases {
        let path = PathBuf::from(format!("{FIXTURES}/{name}.jsonl"));
        let job_id = case["id"].as_str().unwrap();
        let Loaded::Job(job) = tj::load(&path, job_id) else {
            panic!("{name}: no job")
        };
        assert_eq!(view_value(&job), case["view"], "{name}");
    }
}

#[test]
fn a_half_written_last_line_is_left_out_and_a_mid_file_fault_is_unreadable() {
    let data = tempfile::tempdir().unwrap();
    let dir = jobs_dir(data.path());
    let job = id(1);
    let path = dir.join(format!("{job}.jsonl"));
    let whole = format!(
        "{}\n{}\n",
        created(&job, "code", "p1"),
        event(&job, 2, "job.started", "{}")
    );
    fs::write(&path, format!("{whole}{{\"job\":\"{job}\",\"seq\":3,\"ty")).unwrap();
    let Loaded::Job(read) = tj::load(&path, &job) else {
        panic!("a torn tail is not damage")
    };
    assert_eq!(read.status.as_str(), Some("running"));

    // One torn line and nothing else is no job yet.
    fs::write(&path, r#"{"job":"x"#).unwrap();
    assert!(matches!(tj::load(&path, &job), Loaded::Absent));
    fs::write(&path, "").unwrap();
    assert!(matches!(tj::load(&path, &job), Loaded::Absent));

    fs::write(
        &path,
        format!(
            "{}\nnot json\n{}\n",
            created(&job, "code", "p1"),
            event(&job, 3, "job.started", "{}")
        ),
    )
    .unwrap();
    let Loaded::Unreadable { kind, project } = tj::load(&path, &job) else {
        panic!("a bad line in the middle is damage")
    };
    assert_eq!(
        (kind.as_deref(), project.as_deref()),
        (Some("code"), Some("p1"))
    );
}

/// A chained row as jobs.cjs writes one: sha256 of the row's JSON without `hash`, then `hash` last.
fn chained(job: &str, seq: u32, kind: &str, data: &str, previous: &str) -> (String, String) {
    use sha2::{Digest, Sha256};
    let payload = format!(
        r#"{{"job":"{job}","seq":{seq},"type":"{kind}","at":{},"data":{data},"previous":{previous}}}"#,
        1000 + seq
    );
    let hash: String = Sha256::digest(payload.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    (
        format!("{},\"hash\":\"{hash}\"}}", payload.trim_end_matches('}')),
        hash,
    )
}

#[test]
fn a_hash_chained_journal_is_verified_and_fails_closed_on_a_tail_at_rest() {
    let data = tempfile::tempdir().unwrap();
    let dir = jobs_dir(data.path());
    let job = id(2);
    let path = dir.join(format!("{job}.jsonl"));
    let (a, ha) = chained(
        &job,
        1,
        "job.created",
        r#"{"kind":"code","projectId":"p1"}"#,
        "null",
    );
    let (b, _) = chained(&job, 2, "job.started", "{}", &format!("\"{ha}\""));
    fs::write(&path, format!("{a}\n{b}\n")).unwrap();
    assert!(matches!(tj::load(&path, &job), Loaded::Job(_)));
    fs::write(
        &path,
        format!("{a}\n{}\n", b.replace("job.started", "job.completed")),
    )
    .unwrap();
    assert!(
        matches!(tj::load(&path, &job), Loaded::Unreadable { .. }),
        "a tampered row"
    );
    fs::write(&path, format!("{a}\n{{\"job\":\"tor")).unwrap();
    assert!(
        matches!(tj::load(&path, &job), Loaded::Unreadable { .. }),
        "a torn tail at rest"
    );
}

#[test]
fn a_journal_too_big_or_not_a_file_is_unreadable_and_never_followed() {
    let data = tempfile::tempdir().unwrap();
    let dir = jobs_dir(data.path());
    let job = id(3);
    let path = dir.join(format!("{job}.jsonl"));
    let f = fs::File::create(&path).unwrap();
    f.set_len(tj::MAX_JOURNAL_BYTES + 1).unwrap();
    assert!(matches!(tj::load(&path, &job), Loaded::Unreadable { .. }));
    fs::remove_file(&path).unwrap();
    #[cfg(unix)]
    {
        let secret = data.path().join("elsewhere.jsonl");
        fs::write(&secret, created(&job, "code", "p1")).unwrap();
        std::os::unix::fs::symlink(&secret, &path).unwrap();
        assert!(matches!(tj::load(&path, &job), Loaded::Unreadable { .. }));
    }
}

/// jobs.cjs `append`: cut back a line that lost its newline, then one write of the whole line. `chunks`
/// splits the write so a reader can see it half done, which a single write can also show.
fn append_like_node(path: &Path, line: &str, chunks: usize) {
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .read(true)
        .open(path)
        .unwrap();
    let raw = fs::read(path).unwrap();
    if raw.last().is_some_and(|b| *b != b'\n') {
        let keep = raw.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
        f.set_len(keep as u64).unwrap();
    }
    let bytes = format!("{line}\n").into_bytes();
    let size = bytes.len().div_ceil(chunks.max(1));
    for part in bytes.chunks(size) {
        f.write_all(part).unwrap();
        f.flush().unwrap();
        std::thread::yield_now();
    }
}

#[test]
fn a_reader_beside_a_live_writer_only_ever_sees_a_whole_prefix_of_the_journal() {
    let data = tempfile::tempdir().unwrap();
    let dir = jobs_dir(data.path());
    let job = id(4);
    let path = dir.join(format!("{job}.jsonl"));
    fs::write(
        &path,
        format!(
            "{}\n{}\n",
            created(&job, "code", "p1"),
            event(&job, 2, "job.started", "{}")
        ),
    )
    .unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let writer = {
        let (path, job, stop) = (path.clone(), job.clone(), Arc::clone(&stop));
        std::thread::spawn(move || {
            let mut seq = 3;
            let mut crash = false;
            while !stop.load(Ordering::SeqCst) && seq < 4000 {
                let filler = "é".repeat(40);
                let line = event(
                    &job,
                    seq,
                    "progress",
                    &format!(r#"{{"stage":"{seq}-{filler}"}}"#),
                );
                if crash {
                    // A crash mid-write: a fragment with no newline, which the next append cuts back.
                    let mut f = OpenOptions::new().append(true).open(&path).unwrap();
                    f.write_all(&line.as_bytes()[..line.len() / 2]).unwrap();
                }
                crash = seq % 17 == 0;
                append_like_node(&path, &line, 3);
                seq += 1;
            }
            seq
        })
    };
    let deadline = Instant::now() + Duration::from_millis(1500);
    let (mut reads, mut last) = (0, 0_u64);
    while Instant::now() < deadline {
        let Loaded::Job(read) = tj::load(&path, &job) else {
            panic!("a live journal read as damaged after {reads} reads")
        };
        assert_eq!(read.status.as_str(), Some("running"));
        let stage = read
            .stage
            .as_str()
            .map_or(0, |s| s.split('-').next().unwrap().parse().unwrap());
        assert!(
            stage >= last,
            "the journal went backwards: {stage} after {last}"
        );
        last = stage;
        reads += 1;
    }
    stop.store(true, Ordering::SeqCst);
    let wrote = writer.join().unwrap();
    assert!(reads > 5 && wrote > 20, "reads {reads}, writes {wrote}");
}

#[test]
fn the_list_is_this_surface_and_project_newest_first_and_a_stray_file_breaks_nothing() {
    let data = tempfile::tempdir().unwrap();
    let dir = jobs_dir(data.path());
    let write = |n: u32, at: u32, kind: &str, project: &str, tail: &str| {
        let job = id(n);
        let first = created(&job, kind, project).replace("\"at\":1001", &format!("\"at\":{at}"));
        fs::write(dir.join(format!("{job}.jsonl")), format!("{first}\n{tail}")).unwrap();
    };
    write(10, 5000, "code", "p1", "");
    write(11, 9000, "code", "p1", "");
    write(12, 7000, "code", "p2", "");
    write(13, 8000, "browser", "p1", "");
    write(14, 9500, "deep_research", "p1", "");
    // Damaged, with a readable first line: only its own surface and project show it.
    write(15, 1, "code", "p2", "garbage\n{}\n");
    write(16, 1, "deep_research", "p1", "garbage\n{}\n");
    // Damaged from the first line: nothing says whose it is.
    fs::write(dir.join(format!("{}.jsonl", id(17))), "garbage\n{}\n").unwrap();
    fs::write(dir.join("not-a-job.jsonl"), "x").unwrap();
    fs::write(dir.join("other.txt"), "x").unwrap();
    let ids = |surface, project, active| -> Vec<(String, String)> {
        tj::list(data.path(), USER, surface, project, active)
            .unwrap()
            .iter()
            .map(|j| {
                (
                    j.id.as_str().unwrap().to_owned(),
                    j.status.as_str().unwrap().to_owned(),
                )
            })
            .collect()
    };
    let q = |n: u32| (id(n), "queued".to_owned());
    let bad = |n: u32| (id(n), "unreadable".to_owned());
    assert_eq!(
        ids(Surface::Code, Some("p1"), false),
        vec![q(11), q(10), bad(17)]
    );
    assert_eq!(
        ids(Surface::Code, Some("p2"), false),
        vec![q(12), bad(15), bad(17)]
    );
    assert_eq!(
        ids(Surface::Browser, Some("p1"), false),
        vec![q(13), bad(17)]
    );
    assert_eq!(ids(Surface::Code, None, true), vec![q(11), q(12), q(10)]);
    // No journal directory at all is an empty list.
    assert!(tj::list(
        data.path(),
        "00000000-0000-4000-8000-00000000bbbb",
        Surface::Code,
        None,
        false
    )
    .unwrap()
    .is_empty());
    assert!(tj::list(data.path(), "../../etc", Surface::Code, None, false).is_err());
}

#[test]
fn get_is_scoped_to_the_surface_and_project_and_says_when_a_task_cannot_be_read() {
    let data = tempfile::tempdir().unwrap();
    let dir = jobs_dir(data.path());
    let job = id(20);
    fs::write(
        dir.join(format!("{job}.jsonl")),
        format!("{}\n", created(&job, "code", "p1")),
    )
    .unwrap();
    assert!(matches!(
        tj::get(data.path(), USER, Surface::Code, "p1", &job).unwrap(),
        Found::Task(_)
    ));
    assert!(matches!(
        tj::get(data.path(), USER, Surface::Code, "p2", &job).unwrap(),
        Found::Missing
    ));
    assert!(matches!(
        tj::get(data.path(), USER, Surface::Browser, "p1", &job).unwrap(),
        Found::Missing
    ));
    assert!(matches!(
        tj::get(data.path(), USER, Surface::Code, "p1", &id(21)).unwrap(),
        Found::Missing
    ));
    assert!(matches!(
        tj::get(data.path(), USER, Surface::Code, "p1", "../secret").unwrap(),
        Found::Missing
    ));
    fs::write(
        dir.join(format!("{job}.jsonl")),
        format!("{}\nbad\n{{}}\n", created(&job, "code", "p1")),
    )
    .unwrap();
    assert!(matches!(
        tj::get(data.path(), USER, Surface::Code, "p1", &job).unwrap(),
        Found::Unreadable
    ));
}

#[test]
fn browser_and_active_rows_carry_node_fields_and_the_waiting_card() {
    let job = id(30);
    let rows: Vec<JValue> = [
        event(
            &job,
            1,
            "job.created",
            r#"{"kind":"browser","projectId":"p1","capabilities":["open_browser"]}"#,
        ),
        event(&job, 2, "job.started", "{}"),
        event(&job, 3, "checkpoint.created", r#"{"domains":["x.test"]}"#),
        event(&job, 4, "tool.uncertain", r#"{"tool":"network"}"#),
    ]
    .iter()
    .map(|l| js_json::wtf16::parse(l).unwrap())
    .collect();
    let derived = tj::derive(&rows);
    let card = js_json::wtf16::parse(r#"{"id":"a1","action":"submit_form"}"#).unwrap();
    let view: Value = serde_json::from_str(
        &js_json::wtf16::stringify(&tj::browser_view(&derived, &card)).unwrap(),
    )
    .unwrap();
    assert_eq!(view["domains"][0], "x.test");
    assert_eq!(view["approval"]["id"], "a1");
    assert_eq!(view["uncertain"][0]["tool"], "network");
    assert!(view.get("plan").is_none());
    let row: Value = serde_json::from_str(
        &js_json::wtf16::stringify(&tj::active_row(
            &derived,
            &JValue::Str("Alpha".into()),
            &card,
        ))
        .unwrap(),
    )
    .unwrap();
    assert_eq!(row["projectName"], "Alpha");
    assert_eq!(row["approvalAction"], "submit_form");
    assert!(
        tj::internal_project("cowork-diary-extras")
            && tj::internal_project("cowork-chat-context-x")
            && !tj::internal_project("p1")
    );
}
