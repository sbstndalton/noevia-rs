//! Review 2026-11-05 P1: a Diary mutation that took effect but never reached the index (the pass
//! was killed, its future dropped, or the HTTP deadline expired after the Diary committed) is
//! reconciled by the next pass from the intent journal, so the note stays this chat's own: no
//! "someone else's note" refusal, no suffixed duplicate. And an index the pass cannot trust fails
//! the pass before any Diary call.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use chat_mirror_exec::{BoxFut, Diary, FileError, Lists, MirrorSource, UserRoot};
use js_json::{wtf16, JValue};
use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn data_dir(tag: &str) -> PathBuf {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let d = std::env::temp_dir().join(format!("cmx-recovery-{tag}-{}-{n}", std::process::id()));
    std::fs::create_dir_all(d.join("users/u")).unwrap();
    d
}

/// What the fake Diary does after it commits a mutation of `path`.
#[derive(Clone, Copy, PartialEq)]
enum After {
    Answer,
    /// Commits, then answers an error (a deadline that expired after the server committed).
    Fail,
    /// Commits, then never answers (the pass is killed / its future dropped here).
    Hang,
}

#[derive(Default)]
struct Vault {
    files: BTreeMap<String, (String, String)>,
    n: u64,
    calls: Vec<String>,
    after: Option<(String, &'static str, After)>,
}

#[derive(Clone, Default)]
struct Fake(Arc<Mutex<Vault>>);

fn err(status: u16) -> FileError {
    FileError {
        status: Some(status),
        message: format!("status {status}"),
    }
}

impl Fake {
    fn settle(&self, op: &'static str, path: &str) -> BoxFut<Result<JValue, FileError>> {
        let after = self.0.lock().unwrap().after.clone();
        match after {
            Some((p, o, a)) if p == path && o == op && a != After::Answer => {
                self.0.lock().unwrap().after = None;
                if a == After::Fail {
                    Box::pin(async {
                        Err(FileError {
                            status: None,
                            message: "deadline".into(),
                        })
                    })
                } else {
                    Box::pin(std::future::pending())
                }
            }
            _ => Box::pin(async { Ok(JValue::Obj(vec![])) }),
        }
    }
}

impl Diary for Fake {
    fn read(&self, _: &str, path: &str) -> BoxFut<Result<JValue, FileError>> {
        let mut v = self.0.lock().unwrap();
        v.calls.push(format!("read {path}"));
        let r = match v.files.get(path) {
            Some((c, ver)) => Ok(JValue::obj([
                ("content", JValue::Str(c.clone())),
                ("version", JValue::Str(ver.clone())),
            ])),
            None => Err(err(404)),
        };
        Box::pin(async move { r })
    }
    fn write(&self, _: &str, body: JValue) -> BoxFut<Result<JValue, FileError>> {
        let path = body.get("path").as_str().unwrap().to_owned();
        let version = {
            let mut v = self.0.lock().unwrap();
            v.calls.push(format!("write {path}"));
            v.n += 1;
            let ver = format!("v{}", v.n);
            let content = body.get("content").as_str().unwrap().to_owned();
            v.files.insert(path.clone(), (content, ver.clone()));
            ver
        };
        let settle = self.settle("write", &path);
        Box::pin(async move {
            settle.await?;
            Ok(JValue::obj([("version", JValue::Str(version))]))
        })
    }
    fn mkdir(&self, _: &str, _: &str) -> BoxFut<Result<JValue, FileError>> {
        Box::pin(async { Err(err(405)) })
    }
    fn ops(&self, _: &str, body: JValue) -> BoxFut<Result<JValue, FileError>> {
        let op = body.get("op").as_str().unwrap().to_owned();
        let path = body.get("path").as_str().unwrap().to_owned();
        let mut v = self.0.lock().unwrap();
        v.calls.push(format!("{op} {path}"));
        match op.as_str() {
            "stat" => {
                let r = match v.files.get(&path) {
                    Some((_, ver)) => Ok(JValue::obj([
                        ("isDir", JValue::Bool(false)),
                        ("version", JValue::Str(ver.clone())),
                    ])),
                    None => Err(err(404)),
                };
                Box::pin(async move { r })
            }
            "move" => {
                let dest = body.get("destination").as_str().unwrap().to_owned();
                let f = v.files.remove(&path).unwrap();
                v.files.insert(dest, f);
                drop(v);
                self.settle("move", &path)
            }
            "delete" => {
                v.files.remove(&path);
                drop(v);
                self.settle("delete", &path)
            }
            _ => Box::pin(async { Ok(JValue::Obj(vec![])) }),
        }
    }
}

struct Source {
    root: UserRoot,
    lists: Mutex<Lists>,
    tombstones: Mutex<HashSet<String>>,
}

impl MirrorSource for Source {
    fn lists(&self, _: &str) -> Result<Lists, String> {
        Ok(self.lists.lock().unwrap().clone())
    }
    fn deleted(&self, _: &str) -> Result<HashSet<String>, String> {
        Ok(self.tombstones.lock().unwrap().clone())
    }
    fn history(&self, _: &str, _: &str) -> Result<Vec<JValue>, String> {
        Ok(vec![
            wtf16::parse(r#"{"role":"user","content":"hi"}"#).unwrap()
        ])
    }
    fn brain(&self, _: &str, _: &str) -> Option<JValue> {
        None
    }
    fn read_index(&self, _: &str) -> Result<JValue, String> {
        chat_mirror_exec::read_index(&self.root)
    }
    fn write_index(&self, _: &str, state: &JValue) -> Result<(), String> {
        chat_mirror_exec::write_index(&self.root, state)
    }
    fn read_intents(&self, _: &str) -> Result<JValue, String> {
        chat_mirror_exec::read_intents(&self.root)
    }
    fn write_intents(&self, _: &str, intents: &JValue) -> Result<(), String> {
        chat_mirror_exec::write_intents(&self.root, intents)
    }
}

fn chats(items: &[(&str, &str)]) -> Lists {
    Lists {
        free_chats: items
            .iter()
            .enumerate()
            .map(|(i, (id, title))| {
                JValue::obj([
                    ("id", JValue::from(*id)),
                    ("title", JValue::from(*title)),
                    ("createdAt", JValue::Num(1_730_000_000_000.0 + i as f64)),
                ])
            })
            .collect(),
        projects: vec![],
    }
}

fn setup(tag: &str, items: &[(&str, &str)]) -> (PathBuf, Source, Fake) {
    let data = data_dir(tag);
    let source = Source {
        root: UserRoot::open(&data, "u").unwrap(),
        lists: Mutex::new(chats(items)),
        tombstones: Mutex::new(HashSet::new()),
    };
    (data, source, Fake::default())
}

async fn pass(source: &Source, files: &Fake) -> Result<bool, String> {
    files.0.lock().unwrap().calls.clear();
    chat_mirror_exec::sync::sync("u", &HashSet::new(), source, files).await
}

/// The pass, killed while the Diary call is in flight (its future is dropped).
async fn killed(source: &Source, files: &Fake) {
    let r = tokio::time::timeout(Duration::from_millis(200), pass(source, files)).await;
    assert!(r.is_err(), "the pass was expected to hang on the Diary");
}

fn index(data: &std::path::Path) -> Option<JValue> {
    std::fs::read_to_string(data.join("users/u").join(chat_mirror_exec::INDEX_FILE))
        .ok()
        .map(|t| wtf16::parse(&t).unwrap())
}

fn intents_left(data: &std::path::Path) -> bool {
    data.join("users/u")
        .join(chat_mirror_exec::INTENT_FILE)
        .exists()
}

fn paths(files: &Fake) -> Vec<String> {
    files.0.lock().unwrap().files.keys().cloned().collect()
}

#[tokio::test(flavor = "current_thread")]
async fn killed_between_write_and_index_publish_is_reconciled() {
    let (data, source, files) = setup("kill-write", &[("a", "A")]);
    files.0.lock().unwrap().after = Some(("Chats/Inbox/A.md".into(), "write", After::Hang));
    killed(&source, &files).await;
    // The Diary has the note; the index was never published; the journal says what was sent.
    assert_eq!(paths(&files), ["Chats/Inbox/A.md"]);
    assert!(index(&data).is_none());
    assert!(intents_left(&data));

    assert_eq!(pass(&source, &files).await, Ok(true));
    let idx = index(&data).unwrap();
    let a = idx.get("notes").get("a");
    assert_eq!(a.get("path").as_str(), Some("Chats/Inbox/A.md"));
    assert_eq!(a.get("version").as_str(), Some("v1"));
    assert!(
        matches!(idx.get("notes"), JValue::Obj(n) if n.len() == 1),
        "{idx:?}"
    );
    // Adopted, not refused as someone else's, and no suffixed duplicate.
    assert_eq!(paths(&files), ["Chats/Inbox/A.md"]);
    assert!(!files
        .0
        .lock()
        .unwrap()
        .calls
        .iter()
        .any(|c| c.starts_with("write")));
    assert!(!intents_left(&data));
    let _ = std::fs::remove_dir_all(&data);
}

#[tokio::test(flavor = "current_thread")]
async fn deadline_after_the_diary_committed_is_reconciled() {
    let (data, source, files) = setup("timeout-write", &[("a", "A")]);
    files.0.lock().unwrap().after = Some(("Chats/Inbox/A.md".into(), "write", After::Fail));
    // The note failed as far as the pass knows; the index has no entry; the intent stays.
    assert_eq!(pass(&source, &files).await, Ok(false));
    assert!(matches!(index(&data).unwrap().get("notes"), JValue::Obj(n) if n.is_empty()));
    assert!(intents_left(&data));

    assert_eq!(pass(&source, &files).await, Ok(true));
    let idx = index(&data).unwrap();
    assert_eq!(
        idx.get("notes").get("a").get("path").as_str(),
        Some("Chats/Inbox/A.md")
    );
    assert_eq!(paths(&files), ["Chats/Inbox/A.md"]);
    assert!(!intents_left(&data));
    let _ = std::fs::remove_dir_all(&data);
}

#[tokio::test(flavor = "current_thread")]
async fn write_that_never_landed_is_not_adopted() {
    let (data, source, files) = setup("not-landed", &[("a", "A")]);
    // A journal from a pass killed before its write reached the Diary: nothing to adopt.
    chat_mirror_exec::write_intents(
        &source.root,
        &wtf16::parse(r#"{"a":[{"op":"write","path":"Chats/Inbox/A.md","hash":"00","meta":"m"}]}"#)
            .unwrap(),
    )
    .unwrap();
    // Someone else's note sits there: it stays someone else's.
    files
        .0
        .lock()
        .unwrap()
        .files
        .insert("Chats/Inbox/A.md".into(), ("theirs".into(), "x1".into()));
    assert_eq!(pass(&source, &files).await, Ok(false));
    let idx = index(&data).unwrap();
    assert!(idx.get("notes").get("taken:Chats/Inbox/A.md").truthy());
    assert_eq!(
        files.0.lock().unwrap().files["Chats/Inbox/A.md"].0,
        "theirs"
    );
    let _ = std::fs::remove_dir_all(&data);
}

#[tokio::test(flavor = "current_thread")]
async fn killed_move_and_trash_are_reconciled() {
    let (data, source, files) = setup("kill-move", &[("a", "Old"), ("b", "B")]);
    assert_eq!(pass(&source, &files).await, Ok(true));
    *source.lists.lock().unwrap() = chats(&[("a", "New"), ("b", "B")]);
    files.0.lock().unwrap().after = Some(("Chats/Inbox/Old.md".into(), "move", After::Hang));
    killed(&source, &files).await;
    assert_eq!(paths(&files), ["Chats/Inbox/B.md", "Chats/Inbox/New.md"]);
    assert_eq!(pass(&source, &files).await, Ok(true));
    assert_eq!(
        index(&data)
            .unwrap()
            .get("notes")
            .get("a")
            .get("path")
            .as_str(),
        Some("Chats/Inbox/New.md")
    );
    assert_eq!(paths(&files), ["Chats/Inbox/B.md", "Chats/Inbox/New.md"]);

    // Trash: b is deleted; the delete commits and the pass is killed.
    *source.lists.lock().unwrap() = chats(&[("a", "New")]);
    source.tombstones.lock().unwrap().insert("b".into());
    files.0.lock().unwrap().after = Some(("Chats/Inbox/B.md".into(), "delete", After::Hang));
    killed(&source, &files).await;
    assert_eq!(pass(&source, &files).await, Ok(true));
    assert!(!index(&data).unwrap().get("notes").get("b").truthy());
    assert!(!intents_left(&data));
    let _ = std::fs::remove_dir_all(&data);
}

/// Review 2026-11-05 P1: state the pass cannot trust fails it before any Diary call (mkdir
/// included), and leaves the index as it was. An old path outside the mirror is never read or
/// moved.
#[tokio::test(flavor = "current_thread")]
async fn untrusted_index_fails_before_any_diary_call() {
    for (tag, text) in [
        ("corrupt", "not json".to_owned()),
        (
            "array",
            r#"{"notes":[{"path":"Chats/Inbox/A.md"}]}"#.to_owned(),
        ),
        ("null-entry", r#"{"notes":{"a":null}}"#.to_owned()),
        (
            "outside",
            r#"{"notes":{"a":{"path":"Diary/Private.md","version":"v1","meta":"m"}}}"#.to_owned(),
        ),
        (
            "non-text",
            r#"{"notes":{"a":{"path":["x"],"meta":"m"}}}"#.to_owned(),
        ),
    ] {
        let (data, source, files) = setup(tag, &[("a", "A")]);
        files
            .0
            .lock()
            .unwrap()
            .files
            .insert("Diary/Private.md".into(), ("mine".into(), "p1".into()));
        let file = data.join("users/u").join(chat_mirror_exec::INDEX_FILE);
        std::fs::write(&file, &text).unwrap();
        assert!(pass(&source, &files).await.is_err(), "{tag}");
        assert!(
            files.0.lock().unwrap().calls.is_empty(),
            "{tag}: Diary called"
        );
        assert_eq!(std::fs::read_to_string(&file).unwrap(), text, "{tag}");
        assert_eq!(paths(&files), ["Diary/Private.md"], "{tag}");
        let _ = std::fs::remove_dir_all(&data);
    }
    // A linked index: refused, not read as empty, and not replaced.
    let (data, source, files) = setup("link", &[("a", "A")]);
    let elsewhere = data.join("elsewhere.json");
    std::fs::write(&elsewhere, "{}").unwrap();
    let file = data.join("users/u").join(chat_mirror_exec::INDEX_FILE);
    std::os::unix::fs::symlink(&elsewhere, &file).unwrap();
    assert!(pass(&source, &files).await.is_err());
    assert!(files.0.lock().unwrap().calls.is_empty());
    assert!(std::fs::symlink_metadata(&file)
        .unwrap()
        .file_type()
        .is_symlink());
    let _ = std::fs::remove_dir_all(&data);
}
