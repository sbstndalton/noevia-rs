//! The native mirror pass and brain build as the server wires them (src/chat_mirror_work.rs): the
//! account's own lists, tombstones and transcripts from the real store, the index and the brains in
//! `users/<id>/`, the Diary behind its trait and the engine behind its seam. The byte-for-byte
//! comparison with core is crates/chat-mirror-exec/tests/oracle.rs. Synthetic, QA-named data only.
#![cfg(target_os = "linux")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use chat_mirror_exec::{BoxFut, Completion, Diary, FileError};
use chat_mirror_sched::{BrainExecutor, MirrorExecutor, MirrorOutcome};
use js_json::{wtf16, JValue};
use noevia_server::chat_mirror_work::{EngineSeam, Host, NativeWork};
use noevia_server::project_store::{serve_with_context, Store, TOKEN_HEADER};
use serde_json::json;
use server_account::vault_mirror::ExecGates;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const USER: &str = "11111111-2222-4333-8444-555555555555";
const TOKEN: &str = "synthetic-store-token-0123456789abcdef";
const START: &str = "0123456789abcdef0123456789abcdef";
const BRAIN: &str = r#"{"brain_schema":1,"summary":"QA summary","decisions":[],"facts":["f"],"open_questions":[],"entities":[]}"#;

#[derive(Default)]
struct FakeDiary {
    files: Mutex<Vec<(String, String)>>,
    calls: Mutex<Vec<String>>,
}

impl Diary for FakeDiary {
    fn read(&self, _: &str, path: &str) -> BoxFut<Result<JValue, FileError>> {
        self.calls.lock().unwrap().push(format!("read {path}"));
        let f = self
            .files
            .lock()
            .unwrap()
            .iter()
            .find(|(p, _)| p == path)
            .cloned();
        Box::pin(async move {
            match f {
                Some((_, c)) => Ok(JValue::obj([
                    ("content", JValue::Str(c)),
                    ("version", JValue::from("v")),
                ])),
                None => Err(FileError {
                    status: Some(404),
                    message: "missing".into(),
                }),
            }
        })
    }
    fn write(&self, _: &str, body: JValue) -> BoxFut<Result<JValue, FileError>> {
        let p = body.get("path").as_str().unwrap().to_owned();
        self.calls.lock().unwrap().push(format!("write {p}"));
        let mut files = self.files.lock().unwrap();
        files.retain(|(q, _)| *q != p);
        files.push((p, body.get("content").as_str().unwrap().to_owned()));
        Box::pin(async { Ok(JValue::obj([("version", JValue::from("v"))])) })
    }
    fn mkdir(&self, _: &str, path: &str) -> BoxFut<Result<JValue, FileError>> {
        self.calls.lock().unwrap().push(format!("mkdir {path}"));
        Box::pin(async { Ok(JValue::Obj(vec![])) })
    }
    fn ops(&self, _: &str, body: JValue) -> BoxFut<Result<JValue, FileError>> {
        let (op, p) = (
            body.get("op").as_str().unwrap().to_owned(),
            body.get("path").as_str().unwrap().to_owned(),
        );
        self.calls.lock().unwrap().push(format!("{op} {p}"));
        let mut files = self.files.lock().unwrap();
        let found = files.iter().any(|(q, _)| *q == p);
        let r = match op.as_str() {
            "stat" if found => Ok(JValue::obj([
                ("isDir", JValue::Bool(false)),
                ("version", JValue::from("v")),
            ])),
            "delete" if found => {
                files.retain(|(q, _)| *q != p);
                Ok(JValue::Obj(vec![]))
            }
            _ => Err(FileError {
                status: Some(404),
                message: "missing".into(),
            }),
        };
        Box::pin(async move { r })
    }
}

struct Engine {
    admit: Mutex<Result<Option<&'static str>, ()>>,
    calls: Mutex<Vec<String>>,
    /// Gates to switch to while the model runs.
    then: Mutex<Option<ExecGates>>,
    host_gates: Arc<Mutex<Option<ExecGates>>>,
    /// Suspends the model call: signals the first, waits for the second.
    hold: Mutex<Option<(Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)>>,
}

impl EngineSeam for Engine {
    fn loaded(&self) -> BoxFut<Result<Option<String>, ()>> {
        Box::pin(async { Ok(Some("qa-answer".to_owned())) })
    }
    fn admit(&self, model: String, answer: Option<String>) -> BoxFut<Result<Option<String>, ()>> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("admit {model} {answer:?}"));
        let r = *self.admit.lock().unwrap();
        let r = r.map(|o| o.map(str::to_owned));
        Box::pin(async move { r })
    }
    fn complete(&self, call: Completion) -> BoxFut<Result<String, String>> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("complete {}", call.model));
        if let Some(g) = self.then.lock().unwrap().take() {
            *self.host_gates.lock().unwrap() = Some(g);
        }
        let hold = self.hold.lock().unwrap().take();
        Box::pin(async move {
            if let Some((entered, release)) = hold {
                entered.notify_one();
                release.notified().await;
            }
            Ok(BRAIN.to_owned())
        })
    }
}

struct TestHost {
    dir: PathBuf,
    gates: Arc<Mutex<Option<ExecGates>>>,
    diary: Option<Arc<FakeDiary>>,
    engine: Arc<Engine>,
}

impl Host for TestHost {
    fn data_dir(&self) -> &Path {
        &self.dir
    }
    fn gates(&self, _: &str) -> Option<ExecGates> {
        self.gates.lock().unwrap().clone()
    }
    fn files(&self) -> Option<Arc<dyn Diary>> {
        self.diary.clone().map(|d| d as Arc<dyn Diary>)
    }
    fn engine(&self) -> Arc<dyn EngineSeam> {
        Arc::clone(&self.engine) as Arc<dyn EngineSeam>
    }
}

fn on() -> ExecGates {
    ExecGates {
        available: true,
        opted_in: true,
        reasoner: true,
        framing: true,
        reasoner_model: "qa-reasoner".into(),
    }
}

async fn store(root: &Path) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let store = Store {
        data_dir: root.to_path_buf(),
        token: TOKEN.into(),
        switch: server_store::RustProjects::owned(),
        start_id: START.into(),
    };
    tokio::spawn(serve_with_context(
        listener,
        store,
        None,
        None,
        std::future::pending(),
    ));
    addr
}

async fn save(addr: SocketAddr, chat: &str, history: serde_json::Value) {
    let body =
        json!({"user": USER, "chat": chat, "history": history, "origin": "history"}).to_string();
    let mut s = TcpStream::connect(addr).await.unwrap();
    let req = format!(
        "POST /v1/history/write HTTP/1.1\r\nhost: store\r\nconnection: close\r\ncontent-type: application/json\r\ncontent-length: {}\r\n{TOKEN_HEADER}: {TOKEN}\r\n\r\n{body}",
        body.len()
    );
    s.write_all(req.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    s.read_to_end(&mut out).await.unwrap();
    assert!(
        String::from_utf8_lossy(&out).starts_with("HTTP/1.1 200"),
        "{}",
        String::from_utf8_lossy(&out)
    );
}

fn setup() -> (tempfile::TempDir, PathBuf) {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("users").join(USER);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("free-chats.json"),
        r#"[{"id":"qa-free","title":"QA free chat","createdAt":1730000000000,"updatedAt":1730000000001}]"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("projects.json"),
        r#"{"projects":[{"id":"p1","name":"QA project","chats":[{"id":"qa-proj","title":"QA in project","createdAt":1730000000002,"updatedAt":1730000000003}]},{"id":"cowork-diary-extras","name":"Extras","chats":[{"id":"qa-internal","title":"internal","createdAt":1}]}]}"#,
    )
    .unwrap();
    (root, dir)
}

fn own(root: &Path) -> chat_mirror_exec::UserRoot {
    chat_mirror_exec::UserRoot::open(root, USER).unwrap()
}

fn host(root: &Path, gates: Option<ExecGates>, diary: Option<Arc<FakeDiary>>) -> Arc<TestHost> {
    let g = Arc::new(Mutex::new(gates));
    Arc::new(TestHost {
        dir: root.to_path_buf(),
        gates: Arc::clone(&g),
        diary,
        engine: Arc::new(Engine {
            admit: Mutex::new(Ok(None)),
            calls: Mutex::default(),
            then: Mutex::new(None),
            host_gates: g,
            hold: Mutex::new(None),
        }),
    })
}

#[tokio::test]
async fn a_pass_writes_the_accounts_own_notes_and_index_and_trashes_only_tombstoned_chats() {
    let (root, dir) = setup();
    let addr = store(root.path()).await;
    save(
        addr,
        "qa-free",
        json!([{"role": "user", "content": "QA hello"}, {"role": "assistant", "content": "QA hi"}]),
    )
    .await;
    let diary = Arc::new(FakeDiary::default());
    let work = NativeWork::new(host(root.path(), Some(on()), Some(Arc::clone(&diary))));

    assert_eq!(work.sync(USER, vec![]).await, MirrorOutcome::Done);
    let files = diary.files.lock().unwrap().clone();
    let paths: Vec<&str> = files.iter().map(|(p, _)| p.as_str()).collect();
    assert_eq!(
        paths,
        [
            "Chats/Inbox/QA free chat.md",
            "Chats/QA project/QA in project.md"
        ],
        "internal projects are never mirrored"
    );
    let free = &files[0].1;
    assert!(
        free.starts_with("---\nnoevia_id: \"qa-free\"\ncreated: 2024-10-27T03:33:20.000Z\n"),
        "{free}"
    );
    assert!(
        free.contains("## You\n\nQA hello\n\n## Assistant\n\nQA hi\n"),
        "{free}"
    );
    let index = std::fs::read_to_string(dir.join("chat-vault-mirror-index.json")).unwrap();
    assert!(
        index.starts_with(
            r#"{"notes":{"qa-free":{"path":"Chats/Inbox/QA free chat.md","version":"v","meta":""#
        ),
        "{index}"
    );

    // Gone without a tombstone: kept. Tombstoned: trashed and dropped from the index.
    std::fs::write(dir.join("free-chats.json"), "[]").unwrap();
    assert_eq!(work.sync(USER, vec![]).await, MirrorOutcome::Done);
    assert_eq!(diary.files.lock().unwrap().len(), 2);
    std::fs::write(dir.join("deleted-chats.json"), r#"["qa-free"]"#).unwrap();
    assert_eq!(work.sync(USER, vec![]).await, MirrorOutcome::Done);
    let paths: Vec<String> = diary
        .files
        .lock()
        .unwrap()
        .iter()
        .map(|(p, _)| p.clone())
        .collect();
    assert_eq!(paths, ["Chats/QA project/QA in project.md"]);
    let index = std::fs::read_to_string(dir.join("chat-vault-mirror-index.json")).unwrap();
    assert!(!index.contains("qa-free"), "{index}");
}

#[tokio::test]
async fn closed_gates_write_nothing_and_unreadable_gates_or_no_diary_fail_closed() {
    let (root, dir) = setup();
    let diary = Arc::new(FakeDiary::default());
    let mut off = on();
    off.opted_in = false;
    assert_eq!(
        NativeWork::new(host(root.path(), Some(off), Some(Arc::clone(&diary))))
            .sync(USER, vec![])
            .await,
        MirrorOutcome::Off
    );
    let mut unavailable = on();
    unavailable.available = false;
    assert_eq!(
        NativeWork::new(host(
            root.path(),
            Some(unavailable),
            Some(Arc::clone(&diary))
        ))
        .sync(USER, vec![])
        .await,
        MirrorOutcome::Off
    );
    assert_eq!(
        NativeWork::new(host(root.path(), None, Some(Arc::clone(&diary))))
            .sync(USER, vec![])
            .await,
        MirrorOutcome::Failed
    );
    assert_eq!(
        NativeWork::new(host(root.path(), Some(on()), None))
            .sync(USER, vec![])
            .await,
        MirrorOutcome::Failed
    );
    assert!(diary.calls.lock().unwrap().is_empty());
    assert!(!dir.join("chat-vault-mirror-index.json").exists());
    // Not an account id: a failed pass, nothing read or written outside users/.
    assert_eq!(
        NativeWork::new(host(root.path(), Some(on()), Some(Arc::clone(&diary))))
            .sync("../../etc", vec![])
            .await,
        MirrorOutcome::Failed
    );
}

#[tokio::test]
async fn a_brain_is_built_through_the_engine_seam_and_stored_in_the_accounts_workspace() {
    let (root, _dir) = setup();
    let addr = store(root.path()).await;
    save(
        addr,
        "qa-free",
        json!([{"role": "user", "content": "QA plan"}]),
    )
    .await;
    let h = host(root.path(), Some(on()), None);
    let work = NativeWork::new(Arc::clone(&h));
    assert_eq!(work.run(USER, "qa-free").await, "built");
    assert_eq!(
        *h.engine.calls.lock().unwrap(),
        [
            "admit qa-reasoner Some(\"qa-answer\")",
            "complete qa-reasoner"
        ]
    );
    let stored = chat_mirror_exec::brain::read(&own(root.path()), "qa-free").unwrap();
    assert_eq!(wtf16::stringify(&stored.brain).unwrap(), BRAIN);
    assert_eq!(stored.source_updated_at, 1_730_000_000_001.0);
    assert_eq!(work.run(USER, "qa-free").await, "current");

    // No transcript yet: `empty`, before any admission. The admission does not answer: the build
    // ends `error` and never calls the model. It refuses: `budget`.
    let h2 = host(root.path(), Some(on()), None);
    assert_eq!(
        NativeWork::new(Arc::clone(&h2)).run(USER, "qa-proj").await,
        "empty"
    );
    save(addr, "qa-proj", json!([{"role": "user", "content": "QA"}])).await;
    *h2.engine.admit.lock().unwrap() = Err(());
    assert_eq!(
        NativeWork::new(Arc::clone(&h2)).run(USER, "qa-proj").await,
        "error"
    );
    *h2.engine.admit.lock().unwrap() = Ok(Some("budget"));
    assert_eq!(
        NativeWork::new(Arc::clone(&h2)).run(USER, "qa-proj").await,
        "budget"
    );
    assert!(!h2
        .engine
        .calls
        .lock()
        .unwrap()
        .iter()
        .any(|c| c.starts_with("complete")));

    // Switched off while the model ran: nothing is written.
    let h3 = host(root.path(), Some(on()), None);
    let mut off = on();
    off.opted_in = false;
    *h3.engine.then.lock().unwrap() = Some(off);
    assert_eq!(
        NativeWork::new(Arc::clone(&h3)).run(USER, "qa-proj").await,
        "off"
    );
    assert!(chat_mirror_exec::brain::read(&own(root.path()), "qa-proj").is_none());
    // Gates unreadable: error. The reasoner switched off: off.
    assert_eq!(
        NativeWork::new(host(root.path(), None, None))
            .run(USER, "qa-proj")
            .await,
        "error"
    );
    let mut no_reasoner = on();
    no_reasoner.reasoner = false;
    assert_eq!(
        NativeWork::new(host(root.path(), Some(no_reasoner), None))
            .run(USER, "qa-proj")
            .await,
        "off"
    );
}

/// Review 2026-11-05 P1: the account folder is a link to another account's folder. The pass and
/// the build fail closed before any Diary call or local write; the other account is untouched.
#[tokio::test]
async fn an_account_folder_linked_to_another_account_is_refused() {
    const OTHER: &str = "99999999-2222-4333-8444-555555555555";
    let (root, dir) = setup();
    let other = root.path().join("users").join(OTHER);
    std::fs::rename(&dir, &other).unwrap();
    let theirs = chat_mirror_exec::UserRoot::open(root.path(), OTHER).unwrap();
    chat_mirror_exec::brain::write(&theirs, "qa-free", &wtf16::parse(BRAIN).unwrap(), 1.0, 2.0)
        .unwrap();
    std::os::unix::fs::symlink(&other, &dir).unwrap();
    let snapshot = || {
        let mut v: Vec<_> = walkdir(&other);
        v.sort();
        v
    };
    let before = snapshot();
    let diary = Arc::new(FakeDiary::default());
    let work = NativeWork::new(host(root.path(), Some(on()), Some(Arc::clone(&diary))));
    assert_eq!(work.sync(USER, vec![]).await, MirrorOutcome::Failed);
    assert_eq!(work.run(USER, "qa-free").await, "error");
    assert!(diary.calls.lock().unwrap().is_empty(), "Diary called");
    assert_eq!(snapshot(), before, "the other account changed");
}

fn walkdir(d: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(d).unwrap() {
        let p = e.unwrap().path();
        if std::fs::symlink_metadata(&p).unwrap().is_dir() {
            out.extend(walkdir(&p));
        } else {
            out.push((p.clone(), std::fs::read(&p).unwrap_or_default()));
        }
    }
    out
}

/// Suspends the next model call of `h`; returns (entered, release).
fn suspend(h: &TestHost) -> (Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>) {
    let pair = (
        Arc::new(tokio::sync::Notify::new()),
        Arc::new(tokio::sync::Notify::new()),
    );
    *h.engine.hold.lock().unwrap() = Some((Arc::clone(&pair.0), Arc::clone(&pair.1)));
    pair
}

/// Root ruling 2026-11-05 (stale brain publication): the transcript is saved through the real store
/// while the build is suspended in the model call. The write is a compare-and-swap under the
/// store's locks: no brain from the old snapshot, the build answers "stale" (rescheduled), and the
/// next build stores a brain of the new transcript.
#[tokio::test]
async fn a_transcript_saved_while_the_build_is_suspended_gets_no_stale_brain() {
    let (root, _dir) = setup();
    let addr = store(root.path()).await;
    save(
        addr,
        "qa-free",
        json!([{"role": "user", "content": "QA one"}]),
    )
    .await;
    let h = host(root.path(), Some(on()), None);
    let (entered, release) = suspend(&h);
    let work = NativeWork::new(Arc::clone(&h));
    let w = Arc::clone(&work);
    let build = tokio::spawn(async move { w.run(USER, "qa-free").await });
    entered.notified().await;
    save(
        addr,
        "qa-free",
        json!([{"role": "user", "content": "QA one"}, {"role": "assistant", "content": "QA two"}]),
    )
    .await;
    release.notify_one();
    assert_eq!(build.await.unwrap(), "stale");
    assert!(
        chat_mirror_exec::brain::read(&own(root.path()), "qa-free").is_none(),
        "a brain of the old transcript was written"
    );
    assert_eq!(work.run(USER, "qa-free").await, "built");
}

/// Deleted (tombstoned, gone from the list) while the build is suspended: nothing written, and a
/// brain stored earlier is removed.
#[tokio::test]
async fn a_chat_deleted_while_the_build_is_suspended_gets_no_brain() {
    let (root, dir) = setup();
    let addr = store(root.path()).await;
    save(
        addr,
        "qa-free",
        json!([{"role": "user", "content": "QA one"}]),
    )
    .await;
    let h = host(root.path(), Some(on()), None);
    let work = NativeWork::new(Arc::clone(&h));
    assert_eq!(work.run(USER, "qa-free").await, "built");
    // A newer edit, so the next build calls the model.
    std::fs::write(
        dir.join("free-chats.json"),
        r#"[{"id":"qa-free","title":"QA free chat","createdAt":1730000000000,"updatedAt":1730000000009}]"#,
    )
    .unwrap();
    let (entered, release) = suspend(&h);
    let w = Arc::clone(&work);
    let build = tokio::spawn(async move { w.run(USER, "qa-free").await });
    entered.notified().await;
    std::fs::write(dir.join("free-chats.json"), "[]").unwrap();
    std::fs::write(dir.join("deleted-chats.json"), r#"["qa-free"]"#).unwrap();
    release.notify_one();
    assert_eq!(build.await.unwrap(), "gone");
    assert!(
        chat_mirror_exec::brain::read(&own(root.path()), "qa-free").is_none(),
        "the tombstoned chat kept its brain"
    );
}

struct Bridge {
    answer: Option<u16>,
    bodies: Mutex<Vec<String>>,
}

impl noevia_server::chat_mirror_work::NodeBridge for Bridge {
    fn post(&self, body: String, _: std::time::Duration) -> BoxFut<Option<(u16, Vec<u8>)>> {
        self.bodies.lock().unwrap().push(body);
        let a = self.answer;
        Box::pin(async move { a.map(|s| (s, Vec::new())) })
    }
}

/// NodeEngine::record: core's 204 is a recorded entry; any other answer, or none, is logged.
#[tokio::test]
async fn a_recorded_decision_entry_is_core_204_and_anything_else_is_logged() {
    use noevia_server::chat_mirror_work::NodeEngine;
    for (answer, logged) in [
        (Some(204), false),
        (Some(200), true),
        (Some(500), true),
        (None, true),
    ] {
        let bridge = Arc::new(Bridge {
            answer,
            bodies: Mutex::default(),
        });
        let lines: Arc<Mutex<Vec<String>>> = Arc::default();
        let l = Arc::clone(&lines);
        let engine = NodeEngine {
            bridge: Arc::clone(&bridge) as Arc<dyn noevia_server::chat_mirror_work::NodeBridge>,
            log: Arc::new(move |line: &str| l.lock().unwrap().push(line.to_owned())),
        };
        engine.record(json_entry());
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            bridge.bodies.lock().unwrap().as_slice(),
            [r#"{"op":"record","entry":{"event":"brain.built"}}"#],
            "{answer:?}"
        );
        assert_eq!(!lines.lock().unwrap().is_empty(), logged, "{answer:?}");
    }
}

fn json_entry() -> JValue {
    wtf16::parse(r#"{"event":"brain.built"}"#).unwrap()
}
