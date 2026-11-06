//! The runner's outcomes (core createBrainRunner) and the local-write guarantees: atomic index
//! and brain writes, no link following, a failed write leaves the old file whole.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use chat_mirror_exec::brain::{self, Reasoner};
use chat_mirror_exec::{BoxFut, BrainSource, Cas, Completion, Revision, UserRoot};
use js_json::{wtf16, JValue};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

fn tmpdir(tag: &str) -> PathBuf {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let d = std::env::temp_dir().join(format!("chat-mirror-exec-{tag}-{}-{n}", std::process::id()));
    std::fs::create_dir_all(d.join("users/u")).unwrap();
    d
}

fn root(data: &std::path::Path) -> UserRoot {
    UserRoot::open(data, "u").unwrap()
}

const BRAIN: &str = r#"{"brain_schema":1,"summary":"s","decisions":[],"facts":[],"open_questions":[],"entities":[]}"#;

struct Src {
    root: UserRoot,
    enabled: Mutex<Vec<bool>>, // popped per call; last one sticks
    chats: Mutex<Vec<Option<JValue>>>,
    tombstones: Mutex<HashSet<String>>,
    tomb_after_build: bool,
    builds: AtomicUsize,
    /// The store's transcript revision now; `change_during_build` replaces it while the model runs.
    revision: Mutex<String>,
    change_during_build: Mutex<Option<String>>,
}

impl Src {
    fn new(data: PathBuf) -> Self {
        Src {
            root: root(&data),
            enabled: Mutex::new(vec![true]),
            chats: Mutex::new(vec![Some(
                wtf16::parse(r#"{"id":"c","title":"T","updatedAt":5}"#).unwrap(),
            )]),
            tombstones: Mutex::new(HashSet::new()),
            tomb_after_build: false,
            builds: AtomicUsize::new(0),
            revision: Mutex::new("r1".into()),
            change_during_build: Mutex::new(None),
        }
    }
}

fn pop<T: Clone>(v: &Mutex<Vec<T>>) -> T {
    let mut v = v.lock().unwrap();
    if v.len() > 1 {
        v.remove(0)
    } else {
        v[0].clone()
    }
}

impl BrainSource for Src {
    fn enabled(&self, _: &str) -> bool {
        pop(&self.enabled)
    }
    fn chat(&self, _: &str, _: &str) -> Result<Option<JValue>, String> {
        Ok(pop(&self.chats))
    }
    fn snapshot(&self, _: &str, _: &str) -> Result<(Vec<JValue>, String), String> {
        if self.tomb_after_build {
            self.tombstones.lock().unwrap().insert("c".into());
        }
        Ok((
            vec![wtf16::parse(r#"{"role":"user","content":"hi"}"#).unwrap()],
            self.revision.lock().unwrap().clone(),
        ))
    }
    fn deleted(&self, _: &str) -> Result<HashSet<String>, String> {
        Ok(self.tombstones.lock().unwrap().clone())
    }
    fn read(&self, _: &str, chat: &str) -> Option<brain::Record> {
        brain::read(&self.root, chat)
    }
    /// The fake authority: the chat as listed now, the revision now.
    fn write_if(
        &self,
        _: &str,
        chat: &str,
        expected: &Revision,
        b: &JValue,
        s: f64,
        t: f64,
    ) -> Result<Cas, String> {
        let tombstoned = self.tombstones.lock().unwrap().contains(chat);
        let Some(meta) = pop(&self.chats).filter(|_| !tombstoned) else {
            if tombstoned {
                brain::remove(&self.root, chat);
            }
            return Ok(Cas::Gone);
        };
        if *meta.get("updatedAt") != expected.updated_at
            || *self.revision.lock().unwrap() != expected.transcript
        {
            return Ok(Cas::Stale);
        }
        brain::write(&self.root, chat, b, s, t)?;
        Ok(Cas::Written)
    }
    fn remove(&self, _: &str, chat: &str) {
        brain::remove(&self.root, chat);
    }
    fn now_ms(&self) -> f64 {
        7.0
    }
}

struct R<'a>(&'a Src);
impl Reasoner for R<'_> {
    fn enabled(&self) -> bool {
        true
    }
    fn model(&self) -> String {
        "m".into()
    }
    fn loaded_model(&self) -> BoxFut<Result<Option<String>, ()>> {
        Box::pin(async { Ok(None) })
    }
    fn admit(&self, _: String, _: Option<String>) -> BoxFut<Result<Option<String>, ()>> {
        Box::pin(async { Ok(None) })
    }
    fn complete(&self, _: Completion) -> BoxFut<Result<String, String>> {
        self.0.builds.fetch_add(1, Ordering::SeqCst);
        // The chat changes while the model runs (the build is suspended here).
        if let Some(r) = self.0.change_during_build.lock().unwrap().take() {
            *self.0.revision.lock().unwrap() = r;
        }
        Box::pin(async { Ok(BRAIN.to_owned()) })
    }
}

/// Root ruling 2026-11-05: a chat that changes while its build is suspended in the model call gets
/// no brain from the old snapshot (core wrote it); the build answers "stale" (rescheduled by
/// chat-mirror-sched). So does a change of the chat's listed updatedAt.
#[tokio::test(flavor = "current_thread")]
async fn a_chat_changed_during_the_build_gets_no_stale_brain() {
    let dir = tmpdir("stale");
    let src = Src::new(dir.clone());
    *src.change_during_build.lock().unwrap() = Some("r2".into());
    assert_eq!(
        chat_mirror_exec::run_brain("u", "c", &src, &R(&src)).await,
        "stale"
    );
    assert!(
        brain::read(&root(&dir), "c").is_none(),
        "stale brain written"
    );
    // The next build reads the new transcript and writes.
    assert_eq!(
        chat_mirror_exec::run_brain("u", "c", &src, &R(&src)).await,
        "built"
    );

    let dir2 = tmpdir("stale-meta");
    let src2 = Src::new(dir2.clone());
    *src2.chats.lock().unwrap() = vec![
        Some(wtf16::parse(r#"{"id":"c","updatedAt":5}"#).unwrap()),
        Some(wtf16::parse(r#"{"id":"c","updatedAt":6}"#).unwrap()),
    ];
    assert_eq!(
        chat_mirror_exec::run_brain("u", "c", &src2, &R(&src2)).await,
        "stale"
    );
    assert!(brain::read(&root(&dir2), "c").is_none());
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&dir2);
}

#[tokio::test(flavor = "current_thread")]
async fn runner_outcomes() {
    let dir = tmpdir("runner");
    let src = Src::new(dir.clone());
    assert_eq!(
        chat_mirror_exec::run_brain("u", "c", &src, &R(&src)).await,
        "built"
    );
    let rec = brain::read(&root(&dir), "c").unwrap();
    assert_eq!((rec.source_updated_at, rec.built_at), (5.0, 7.0));
    // Same updatedAt: current, no model call.
    assert_eq!(
        chat_mirror_exec::run_brain("u", "c", &src, &R(&src)).await,
        "current"
    );
    assert_eq!(src.builds.load(Ordering::SeqCst), 1);

    // Off before, and switched off while the model ran: nothing written.
    let dir2 = tmpdir("runner-off");
    let src2 = Src::new(dir2.clone());
    *src2.enabled.lock().unwrap() = vec![false];
    assert_eq!(
        chat_mirror_exec::run_brain("u", "c", &src2, &R(&src2)).await,
        "off"
    );
    *src2.enabled.lock().unwrap() = vec![true, false];
    assert_eq!(
        chat_mirror_exec::run_brain("u", "c", &src2, &R(&src2)).await,
        "off"
    );
    assert!(brain::read(&root(&dir2), "c").is_none());

    // #811: tombstoned while the model ran: no write-back, the stored brain removed.
    let mut src3 = Src::new(dir.clone());
    src3.chats = Mutex::new(vec![Some(
        wtf16::parse(r#"{"id":"c","updatedAt":9}"#).unwrap(),
    )]);
    src3.tomb_after_build = true;
    assert_eq!(
        chat_mirror_exec::run_brain("u", "c", &src3, &R(&src3)).await,
        "gone"
    );
    assert!(brain::read(&root(&dir), "c").is_none());

    // Gone from the lists: an absent chat keeps its brain, a tombstoned one loses it.
    brain::write(&root(&dir), "c", &wtf16::parse(BRAIN).unwrap(), 1.0, 1.0).unwrap();
    let src4 = Src::new(dir.clone());
    *src4.chats.lock().unwrap() = vec![None];
    assert_eq!(
        chat_mirror_exec::run_brain("u", "c", &src4, &R(&src4)).await,
        "gone"
    );
    assert!(brain::read(&root(&dir), "c").is_some());
    src4.tombstones.lock().unwrap().insert("c".into());
    assert_eq!(
        chat_mirror_exec::run_brain("u", "c", &src4, &R(&src4)).await,
        "gone"
    );
    assert!(brain::read(&root(&dir), "c").is_none());
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&dir2);
}

#[tokio::test(flavor = "current_thread")]
async fn deadline_is_a_fallback() {
    struct Slow;
    impl Reasoner for Slow {
        fn enabled(&self) -> bool {
            true
        }
        fn model(&self) -> String {
            "m".into()
        }
        fn loaded_model(&self) -> BoxFut<Result<Option<String>, ()>> {
            Box::pin(async { Ok(None) })
        }
        fn admit(&self, _: String, _: Option<String>) -> BoxFut<Result<Option<String>, ()>> {
            Box::pin(async { Ok(None) })
        }
        fn complete(&self, _: Completion) -> BoxFut<Result<String, String>> {
            Box::pin(async {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                Ok(String::new())
            })
        }
        fn deadline(&self) -> std::time::Duration {
            std::time::Duration::from_millis(100)
        }
    }
    let h = vec![wtf16::parse(r#"{"role":"user","content":"hi"}"#).unwrap()];
    assert_eq!(
        brain::build(&Slow, &JValue::Null, &h).await,
        Err("deadline".into())
    );
}

#[test]
fn index_write_is_atomic_and_links_fail_closed() {
    use std::os::unix::fs::symlink;
    let data = tmpdir("index");
    let dir = data.join("users/u");
    let r = root(&data);
    let outside = tmpdir("outside");
    let target = outside.join("victim.json");
    std::fs::write(&target, "untouched").unwrap();
    // A link where the index goes is not followed and not read as `{}`: the read fails closed
    // (review 2026-11-05 P1; core, and this port before, read it as an empty index).
    symlink(&target, dir.join(chat_mirror_exec::INDEX_FILE)).unwrap();
    assert!(chat_mirror_exec::read_index(&r).is_err());
    // A write replaces the link itself (rename), never its target.
    let state =
        wtf16::parse(r#"{"notes":{"a":{"path":"Chats/Inbox/A.md","version":"v1","meta":"m"}}}"#)
            .unwrap();
    chat_mirror_exec::write_index(&r, &state).unwrap();
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "untouched");
    let idx = dir.join(chat_mirror_exec::INDEX_FILE);
    assert!(!std::fs::symlink_metadata(&idx)
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(chat_mirror_exec::read_index(&r).unwrap(), state);
    // Corrupt or misshapen state fails closed too; only a missing index is `{}`.
    for bad in [
        "not json",
        "null",
        "[]",
        r#"{"notes":[]}"#,
        r#"{"notes":{"a":null}}"#,
        r#"{"notes":{"a":{"path":"../../elsewhere.md"}}}"#,
        r#"{"notes":{"a":{"path":7}}}"#,
    ] {
        std::fs::write(&idx, bad).unwrap();
        assert!(chat_mirror_exec::read_index(&r).is_err(), "{bad}");
    }
    std::fs::remove_file(&idx).unwrap();
    assert_eq!(
        chat_mirror_exec::read_index(&r).unwrap(),
        JValue::Obj(vec![])
    );

    // A failed write (the folder refuses new files) leaves the old index whole.
    chat_mirror_exec::write_index(&r, &state).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).unwrap();
    let failed = chat_mirror_exec::write_index(&r, &JValue::Obj(vec![])).is_err();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    // (A superuser is not stopped by the mode; then there is nothing to show here.)
    if failed {
        assert_eq!(chat_mirror_exec::read_index(&r).unwrap(), state);
    }

    // A linked brain folder is refused for writes and reads as none.
    let bdata = tmpdir("brainlink");
    symlink(&outside, bdata.join("users/u").join(brain::BRAIN_DIR)).unwrap();
    let br = root(&bdata);
    assert!(brain::write(&br, "c", &wtf16::parse(BRAIN).unwrap(), 1.0, 1.0).is_err());
    assert!(brain::read(&br, "c").is_none());
    assert!(!outside.join(brain::file_name("c")).exists());
    for d in [data, outside, bdata] {
        let _ = std::fs::remove_dir_all(d);
    }
}

/// Review 2026-11-05 P1: the account folder is swapped for a link to another account's folder
/// after it was opened (between the check and the use). Nothing reads, writes or removes there.
#[test]
fn swapped_account_folder_never_reaches_another_account() {
    use std::os::unix::fs::symlink;
    let data = tmpdir("swap");
    let (a, b) = (data.join("users/u"), data.join("users/v"));
    std::fs::create_dir_all(&b).unwrap();
    let brain_json = wtf16::parse(BRAIN).unwrap();
    let rb = UserRoot::open(&data, "v").unwrap();
    brain::write(&rb, "c", &brain_json, 1.0, 2.0).unwrap();
    std::fs::write(
        b.join(chat_mirror_exec::INDEX_FILE),
        r#"{"notes":{"c":{"path":"Chats/Inbox/B.md","version":"v1","meta":"m"}}}"#,
    )
    .unwrap();
    let snapshot = |d: &std::path::Path| {
        let mut v: Vec<(String, Vec<u8>)> = walk(d);
        v.sort();
        v
    };
    let before = snapshot(&b);

    let ra = root(&data);
    std::fs::rename(&a, data.join("users/u.old")).unwrap();
    symlink(&b, &a).unwrap();

    assert!(brain::read(&ra, "c").is_none(), "B's brain read through A");
    assert!(brain::write(&ra, "c", &brain_json, 9.0, 9.0).is_err());
    brain::remove(&ra, "c");
    assert!(chat_mirror_exec::read_index(&ra).is_err());
    assert!(chat_mirror_exec::write_index(&ra, &JValue::Obj(vec![])).is_err());
    assert!(
        chat_mirror_exec::write_intents(&ra, &JValue::obj([("c", JValue::Arr(vec![]))])).is_err()
    );
    assert!(ra.verify().is_err());
    // Opening A afresh refuses the link.
    assert!(UserRoot::open(&data, "u").is_err());
    assert_eq!(snapshot(&b), before, "B's folder changed");
    assert!(
        walk(&data.join("users/u.old")).is_empty(),
        "A's old folder written"
    );

    // A leaf swapped for a link to B's record (after A wrote its own) reads as none, never B's.
    std::fs::remove_file(&a).unwrap();
    std::fs::rename(data.join("users/u.old"), &a).unwrap();
    let ra = root(&data);
    brain::write(&ra, "c", &brain_json, 3.0, 4.0).unwrap();
    let own = brain::file_for(&a, "c");
    std::fs::remove_file(&own).unwrap();
    symlink(brain::file_for(&b, "c"), &own).unwrap();
    assert!(
        brain::read(&ra, "c").is_none(),
        "B's record read through a leaf link"
    );
    let _ = std::fs::remove_dir_all(&data);
}

fn walk(d: &std::path::Path) -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(d).unwrap() {
        let e = e.unwrap();
        let p = e.path();
        let m = std::fs::symlink_metadata(&p).unwrap();
        if m.is_dir() {
            out.extend(walk(&p));
        } else {
            out.push((
                p.display().to_string(),
                std::fs::read(&p).unwrap_or_default(),
            ));
        }
    }
    out
}

#[test]
fn guard_refuses_traversal() {
    use chat_mirror_exec::note::inside_mirror;
    for bad in [
        "Chats/../x.md",
        "Chats/a/../b.md",
        "../Chats/a/b.md",
        "Chats/.a/b.md",
        "Chats/a/.b.md",
        "Chats/a\\b/c.md",
        "Chats/a/b",
        "Chats/a/b/c.md",
        "Other/a/b.md",
        "Chats//b.md",
    ] {
        assert!(!inside_mirror(&JValue::from(bad)), "{bad}");
    }
    assert!(inside_mirror(&JValue::from("Chats/Inbox/A.md")));
    assert!(!inside_mirror(&JValue::Num(1.0)));
}
