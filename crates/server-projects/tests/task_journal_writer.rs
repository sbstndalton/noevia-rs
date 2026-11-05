#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use js_json::{wtf16, JValue};
use server_projects::task_journals::{
    self as tj,
    writer::{Error, Store},
    Loaded, Surface,
};
use std::{fs, io::Write};
const USER: &str = "00000000-0000-4000-8000-00000000aaaa";
const ID: &str = "00000000-0000-4000-8000-000000000001";
fn setup(surface: Surface) -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("users").join(USER)).unwrap();
    let store = Store::open(dir.path(), USER, surface).unwrap();
    (dir, store)
}
fn empty() -> JValue {
    JValue::Obj(vec![])
}
fn path(dir: &std::path::Path) -> std::path::PathBuf {
    dir.join("users")
        .join(USER)
        .join("jobs")
        .join(format!("{ID}.jsonl"))
}
#[test]
fn create_append_and_terminal_guard_roundtrip_existing_reader() {
    let (dir, s) = setup(Surface::Code);
    s.create(ID, "p", vec![], 1).unwrap();
    s.append(ID, "job.started", empty(), 2).unwrap();
    assert_eq!(
        s.append(ID, "job.completed", empty(), 3)
            .unwrap()
            .status
            .as_str(),
        Some("completed")
    );
    assert!(matches!(
        s.append(ID, "progress", empty(), 4),
        Err(Error::Finished)
    ));
    let Loaded::Job(j) = tj::load(&path(dir.path()), ID) else {
        panic!()
    };
    assert_eq!(j.status.as_str(), Some("completed"));
    assert!(s.create(ID, "p", vec![], 5).is_err());
}
#[test]
fn recovery_of_node_written_approval_is_interrupted_once() {
    let (dir, s) = setup(Surface::Code);
    let fixture = include_str!("fixtures/task-journals/interrupted.jsonl");
    // The first three rows predate Node's recorded restart event.
    let bytes = fixture.lines().take(3).collect::<Vec<_>>().join("\n") + "\n";
    let id = "00000000-0000-4000-8000-000000000003";
    let p = path(dir.path()).with_file_name(format!("{id}.jsonl"));
    fs::write(&p, bytes).unwrap();
    assert_eq!(s.recover(2000).unwrap(), 1);
    assert_eq!(s.recover(2001).unwrap(), 0);
    let Loaded::Job(j) = tj::load(&p, id) else {
        panic!()
    };
    assert_eq!(j.status.as_str(), Some("interrupted"));
    assert_eq!(
        j.error.as_str(),
        Some("The server restarted while waiting for approval; start again to be asked again.")
    );
}
#[test]
fn recovery_and_append_are_surface_and_account_scoped() {
    let (dir, s) = setup(Surface::Browser);
    s.create(ID, "p", vec![], 1).unwrap();
    let code = Store::open(dir.path(), USER, Surface::Code).unwrap();
    assert_eq!(code.recover(2).unwrap(), 0);
    assert!(matches!(
        code.append(ID, "job.started", empty(), 2),
        Err(Error::WrongSurface)
    ));
    let other = "00000000-0000-4000-8000-00000000bbbb";
    fs::create_dir_all(dir.path().join("users").join(other)).unwrap();
    let other = Store::open(dir.path(), other, Surface::Browser).unwrap();
    assert_eq!(other.recover(3).unwrap(), 0);
    assert_eq!(s.recover(4).unwrap(), 1);
}
#[test]
fn torn_plain_tail_is_removed_before_append() {
    let (dir, s) = setup(Surface::Code);
    s.create(ID, "p", vec![], 1).unwrap();
    fs::OpenOptions::new()
        .append(true)
        .open(path(dir.path()))
        .unwrap()
        .write_all(b"{\"torn\":")
        .unwrap();
    s.append(ID, "job.started", empty(), 2).unwrap();
    let Loaded::Job(j) = tj::load(&path(dir.path()), ID) else {
        panic!()
    };
    assert_eq!(j.status.as_str(), Some("running"));
}
#[test]
fn corrupt_journal_is_never_overwritten_or_recovered() {
    let (dir, s) = setup(Surface::Code);
    s.create(ID, "p", vec![], 1).unwrap();
    fs::OpenOptions::new()
        .append(true)
        .open(path(dir.path()))
        .unwrap()
        .write_all(b"invalid\n{}\n")
        .unwrap();
    let before = fs::read(path(dir.path())).unwrap();
    assert!(matches!(
        s.append(ID, "job.started", empty(), 2),
        Err(Error::Unreadable)
    ));
    assert_eq!(s.recover(3).unwrap(), 0);
    assert_eq!(fs::read(path(dir.path())).unwrap(), before);
}
#[test]
fn authority_events_cannot_be_injected_through_executor_append() {
    let (_dir, s) = setup(Surface::Code);
    s.create(ID, "p", vec![], 1).unwrap();
    for kind in [
        "task.stage",
        "task.revision",
        "job.created",
        "review.completed",
        "unknown",
    ] {
        assert!(matches!(
            s.append(ID, kind, empty(), 2),
            Err(Error::InvalidEvent)
        ));
    }
    assert!(matches!(
        s.append("../escape", "job.started", empty(), 3),
        Err(Error::InvalidId)
    ));
}
#[test]
fn cancelled_tasks_can_retain_cleanup_reports_only() {
    let (_dir, s) = setup(Surface::Browser);
    s.create(ID, "p", vec![], 1).unwrap();
    s.append(ID, "job.cancelled", empty(), 2).unwrap();
    assert_eq!(
        s.append(ID, "artifact.created", empty(), 3)
            .unwrap()
            .status
            .as_str(),
        Some("cancelled")
    );
    assert!(matches!(
        s.append(ID, "job.completed", empty(), 4),
        Err(Error::Finished)
    ));
}
#[test]
fn absent_account_is_not_resurrected() {
    let dir = tempfile::tempdir().unwrap();
    assert!(Store::open(dir.path(), USER, Surface::Code).is_err());
    assert!(!dir.path().join("users").exists());
}
#[test]
fn node_hash_chain_is_extended_without_changing_prior_bytes() {
    use sha2::{Digest, Sha256};
    let (dir, s) = setup(Surface::Code);
    let payload = format!(
        r#"{{"job":"{ID}","seq":1,"type":"job.created","at":1,"data":{{"kind":"code","projectId":"p"}},"previous":null}}"#
    );
    let hash = Sha256::digest(payload.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    let original = format!(
        "{},\"hash\":\"{hash}\"}}\n",
        payload.strip_suffix('}').unwrap()
    );
    fs::write(path(dir.path()), &original).unwrap();
    s.append(ID, "job.started", empty(), 2).unwrap();
    let bytes = fs::read_to_string(path(dir.path())).unwrap();
    assert!(bytes.starts_with(&original));
    let last = wtf16::parse(bytes.lines().last().unwrap()).unwrap();
    assert_eq!(last.get("previous").as_str(), Some(hash.as_str()));
    assert!(matches!(tj::load(&path(dir.path()), ID), Loaded::Job(_)));
    fs::OpenOptions::new()
        .append(true)
        .open(path(dir.path()))
        .unwrap()
        .write_all(b"{")
        .unwrap();
    let damaged = fs::read(path(dir.path())).unwrap();
    assert!(matches!(
        s.append(ID, "progress", empty(), 3),
        Err(Error::Unreadable)
    ));
    assert_eq!(fs::read(path(dir.path())).unwrap(), damaged);
}
#[cfg(unix)]
#[test]
fn symlink_journal_and_lock_never_write_the_target() {
    use std::os::unix::fs::symlink;
    let (dir, s) = setup(Surface::Code);
    let outside = dir.path().join("outside");
    fs::write(&outside, b"untouched").unwrap();
    symlink(&outside, path(dir.path())).unwrap();
    assert!(s.append(ID, "job.started", empty(), 2).is_err());
    assert_eq!(fs::read(&outside).unwrap(), b"untouched");
    let lock = path(dir.path()).with_file_name("native-task-writer.lock");
    fs::remove_file(&lock).unwrap();
    symlink(&outside, lock).unwrap();
    assert!(s.recover(3).is_err());
    assert_eq!(fs::read(&outside).unwrap(), b"untouched");
}
#[test]
fn concurrent_native_writers_have_a_single_sequence_and_no_lost_events() {
    let (dir, s) = setup(Surface::Browser);
    s.create(ID, "p", vec![], 1).unwrap();
    s.append(ID, "job.started", empty(), 2).unwrap();
    let root = std::sync::Arc::new(dir.path().to_path_buf());
    let threads: Vec<_> = (0..4)
        .map(|_| {
            let root = root.clone();
            std::thread::spawn(move || {
                let s = Store::open(&root, USER, Surface::Browser).unwrap();
                for n in 0..10 {
                    loop {
                        match s.append(ID, "progress", empty(), 3 + n) {
                            Ok(_) => break,
                            Err(Error::Busy) => std::thread::yield_now(),
                            error => panic!("{error:?}"),
                        }
                    }
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    let bytes = fs::read_to_string(path(dir.path())).unwrap();
    let rows: Vec<_> = bytes.lines().map(|s| wtf16::parse(s).unwrap()).collect();
    assert_eq!(rows.len(), 42);
    for (n, row) in rows.iter().enumerate() {
        assert_eq!(row.get("seq").as_num(), Some((n + 1) as f64));
    }
}
#[test]
fn deleted_account_after_open_is_not_recreated() {
    let (dir, s) = setup(Surface::Code);
    fs::remove_dir_all(dir.path().join("users").join(USER)).unwrap();
    assert!(s.create(ID, "p", vec![], 1).is_err());
    assert!(!dir.path().join("users").join(USER).exists());
}
#[test]
fn valid_terminal_json_without_a_newline_still_refuses_an_append() {
    let (dir, s) = setup(Surface::Code);
    s.create(ID, "p", vec![], 1).unwrap();
    s.append(ID, "job.completed", empty(), 2).unwrap();
    let mut bytes = fs::read(path(dir.path())).unwrap();
    assert_eq!(bytes.pop(), Some(b'\n'));
    fs::write(path(dir.path()), &bytes).unwrap();
    assert!(matches!(
        s.append(ID, "job.started", empty(), 3),
        Err(Error::Finished)
    ));
    assert_eq!(fs::read(path(dir.path())).unwrap(), bytes);
    assert_eq!(s.recover(4).unwrap(), 0);
    assert_eq!(fs::read(path(dir.path())).unwrap(), bytes);
}
