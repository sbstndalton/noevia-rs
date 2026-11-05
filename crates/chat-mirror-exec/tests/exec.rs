//! The runner's outcomes (core createBrainRunner) and the local-write guarantees: atomic index
//! and brain writes, no link following, a failed write leaves the old file whole.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use chat_mirror_exec::brain::{self, Reasoner};
use chat_mirror_exec::{BoxFut, BrainSource, Completion};
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
    std::fs::create_dir_all(&d).unwrap();
    d
}

const BRAIN: &str = r#"{"brain_schema":1,"summary":"s","decisions":[],"facts":[],"open_questions":[],"entities":[]}"#;

struct Src {
    dir: PathBuf,
    enabled: Mutex<Vec<bool>>, // popped per call; last one sticks
    chats: Mutex<Vec<Option<JValue>>>,
    tombstones: Mutex<HashSet<String>>,
    tomb_after_build: bool,
    builds: AtomicUsize,
}

impl Src {
    fn new(dir: PathBuf) -> Self {
        Src {
            dir,
            enabled: Mutex::new(vec![true]),
            chats: Mutex::new(vec![Some(
                wtf16::parse(r#"{"id":"c","title":"T","updatedAt":5}"#).unwrap(),
            )]),
            tombstones: Mutex::new(HashSet::new()),
            tomb_after_build: false,
            builds: AtomicUsize::new(0),
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
    fn history(&self, _: &str, _: &str) -> Result<Vec<JValue>, String> {
        if self.tomb_after_build {
            self.tombstones.lock().unwrap().insert("c".into());
        }
        Ok(vec![
            wtf16::parse(r#"{"role":"user","content":"hi"}"#).unwrap()
        ])
    }
    fn deleted(&self, _: &str) -> Result<HashSet<String>, String> {
        Ok(self.tombstones.lock().unwrap().clone())
    }
    fn read(&self, _: &str, chat: &str) -> Option<brain::Record> {
        brain::read(&self.dir, chat)
    }
    fn write(&self, _: &str, chat: &str, b: &JValue, s: f64, t: f64) -> Result<(), String> {
        brain::write(&self.dir, chat, b, s, t).map(|_| ())
    }
    fn remove(&self, _: &str, chat: &str) {
        brain::remove(&self.dir, chat);
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
        Box::pin(async { Ok(BRAIN.to_owned()) })
    }
}

#[tokio::test(flavor = "current_thread")]
async fn runner_outcomes() {
    let dir = tmpdir("runner");
    let src = Src::new(dir.clone());
    assert_eq!(
        chat_mirror_exec::run_brain("u", "c", &src, &R(&src)).await,
        "built"
    );
    let rec = brain::read(&dir, "c").unwrap();
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
    assert!(brain::read(&dir2, "c").is_none());

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
    assert!(brain::read(&dir, "c").is_none());

    // Gone from the lists: an absent chat keeps its brain, a tombstoned one loses it.
    brain::write(&dir, "c", &wtf16::parse(BRAIN).unwrap(), 1.0, 1.0).unwrap();
    let src4 = Src::new(dir.clone());
    *src4.chats.lock().unwrap() = vec![None];
    assert_eq!(
        chat_mirror_exec::run_brain("u", "c", &src4, &R(&src4)).await,
        "gone"
    );
    assert!(brain::read(&dir, "c").is_some());
    src4.tombstones.lock().unwrap().insert("c".into());
    assert_eq!(
        chat_mirror_exec::run_brain("u", "c", &src4, &R(&src4)).await,
        "gone"
    );
    assert!(brain::read(&dir, "c").is_none());
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
fn index_write_is_atomic_and_never_follows_links() {
    use std::os::unix::fs::symlink;
    let dir = tmpdir("index");
    let outside = tmpdir("outside");
    let target = outside.join("victim.json");
    std::fs::write(&target, "untouched").unwrap();
    // A link where the index goes: the rename replaces the link, the target stays as it was.
    symlink(&target, dir.join(chat_mirror_exec::INDEX_FILE)).unwrap();
    // A linked index reads as none ({}), it is not followed.
    assert_eq!(chat_mirror_exec::read_index(&dir), JValue::Obj(vec![]));
    let state =
        wtf16::parse(r#"{"notes":{"a":{"path":"Chats/Inbox/A.md","version":"v1","meta":"m"}}}"#)
            .unwrap();
    chat_mirror_exec::write_index(&dir, &state).unwrap();
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "untouched");
    let idx = dir.join(chat_mirror_exec::INDEX_FILE);
    assert!(!std::fs::symlink_metadata(&idx)
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(
        std::fs::read_to_string(&idx).unwrap(),
        wtf16::stringify(&state).unwrap()
    );

    // A failed write (the temp file cannot be made: a folder sits at its name) leaves the old index whole.
    let tmp = dir.join(format!(
        "{}.{}.tmp",
        chat_mirror_exec::INDEX_FILE,
        std::process::id()
    ));
    std::fs::create_dir(&tmp).unwrap();
    std::fs::write(tmp.join("x"), "x").unwrap();
    assert!(chat_mirror_exec::write_index(&dir, &JValue::Obj(vec![])).is_err());
    assert_eq!(
        std::fs::read_to_string(&idx).unwrap(),
        wtf16::stringify(&state).unwrap()
    );

    // A linked brain folder is refused for writes and reads as none.
    let bdir = tmpdir("brainlink");
    symlink(&outside, bdir.join(brain::BRAIN_DIR)).unwrap();
    assert!(brain::write(&bdir, "c", &wtf16::parse(BRAIN).unwrap(), 1.0, 1.0).is_err());
    assert!(brain::read(&bdir, "c").is_none());
    assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 1);
    for d in [dir, outside, bdir] {
        let _ = std::fs::remove_dir_all(d);
    }
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
