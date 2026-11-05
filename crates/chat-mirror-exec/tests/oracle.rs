//! Replays the Node oracle (tests/oracle/record.cjs over noevia-core main's own executors,
//! synthetic accounts, a deterministic fake Diary and a fake reasoner) and compares everything
//! byte for byte: every Diary call and body, the vault tree, the Trash, the index file after each
//! pass, the brain parser, the note header, the store files and the reasoner call.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use chat_mirror_exec::brain::{self, Reasoner};
use chat_mirror_exec::{jsx, BoxFut, Completion, Diary, FileError, Lists, MirrorSource};
use js_json::{wtf16, JValue};
use std::collections::{BTreeSet, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

fn fixture() -> JValue {
    let text = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/oracle.v1.json"
    ))
    .unwrap();
    wtf16::parse(&text).unwrap()
}

fn s(v: &JValue) -> String {
    wtf16::stringify(v).unwrap()
}

fn tmpdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "chat-mirror-exec-{tag}-{}-{}",
        std::process::id(),
        rand_tag()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn rand_tag() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}

#[derive(Default)]
struct DiaryState {
    files: Vec<(String, String, String)>, // path, content, version
    dirs: BTreeSet<String>,
    trash: Vec<JValue>,
    calls: Vec<JValue>,
    fail_write: HashSet<String>,
    fail_delete: HashSet<String>,
    n: u64,
}

impl DiaryState {
    fn next(&mut self) -> String {
        self.n += 1;
        format!("v{}", self.n)
    }
    fn find(&self, p: &str) -> Option<usize> {
        self.files.iter().position(|(q, _, _)| q == p)
    }
    fn put(&mut self, p: &str, content: String, version: String) {
        match self.find(p) {
            Some(i) => self.files[i] = (p.to_owned(), content, version),
            None => self.files.push((p.to_owned(), content, version)),
        }
    }
}

#[derive(Clone, Default)]
struct FakeDiary(Arc<Mutex<DiaryState>>);

fn status(code: u16) -> FileError {
    FileError {
        status: Some(code),
        message: format!("status {code}"),
    }
}

fn call(method: &str, user: &str, body: &JValue) -> JValue {
    JValue::Arr(vec![
        JValue::from(method),
        JValue::from(user),
        JValue::Str(s(body)),
    ])
}

impl Diary for FakeDiary {
    fn read(&self, user: &str, path: &str) -> BoxFut<Result<JValue, FileError>> {
        let mut st = self.0.lock().unwrap();
        st.calls.push(call(
            "read",
            user,
            &JValue::obj([("path", JValue::from(path))]),
        ));
        let r = match st.find(path) {
            Some(i) => {
                let (_, c, v) = &st.files[i];
                Ok(JValue::obj([
                    ("content", JValue::Str(c.clone())),
                    ("version", JValue::Str(v.clone())),
                ]))
            }
            None => Err(status(404)),
        };
        Box::pin(async move { r })
    }
    fn write(&self, user: &str, body: JValue) -> BoxFut<Result<JValue, FileError>> {
        let mut st = self.0.lock().unwrap();
        st.calls.push(call("write", user, &body));
        let p = body.get("path").as_str().unwrap().to_owned();
        let r = if st.fail_write.contains(&p) {
            Err(status(500))
        } else {
            let v = st.next();
            st.put(
                &p,
                body.get("content").as_str().unwrap().to_owned(),
                v.clone(),
            );
            Ok(JValue::obj([("version", JValue::Str(v))]))
        };
        Box::pin(async move { r })
    }
    fn mkdir(&self, user: &str, path: &str) -> BoxFut<Result<JValue, FileError>> {
        let mut st = self.0.lock().unwrap();
        st.calls.push(call(
            "mkdir",
            user,
            &JValue::obj([("path", JValue::from(path))]),
        ));
        let r = if st.dirs.insert(path.to_owned()) {
            Ok(JValue::Obj(vec![]))
        } else {
            Err(status(405))
        };
        Box::pin(async move { r })
    }
    fn ops(&self, user: &str, body: JValue) -> BoxFut<Result<JValue, FileError>> {
        let mut st = self.0.lock().unwrap();
        st.calls.push(call("ops", user, &body));
        let p = body.get("path").as_str().unwrap_or("").to_owned();
        let f = st.find(&p);
        let r = match (body.get("op").as_str().unwrap(), f) {
            ("stat", Some(i)) => Ok(JValue::obj([
                ("isDir", JValue::Bool(false)),
                ("version", JValue::Str(st.files[i].2.clone())),
            ])),
            ("stat", None) if st.dirs.contains(&p) => {
                Ok(JValue::obj([("isDir", JValue::Bool(true))]))
            }
            ("move", Some(i)) => {
                let dest = body.get("destination").as_str().unwrap().to_owned();
                if st.find(&dest).is_some() {
                    Err(status(409))
                } else {
                    let (_, c, v) = st.files.remove(i);
                    st.files.push((dest, c, v));
                    Ok(JValue::Obj(vec![]))
                }
            }
            ("delete", _) if st.fail_delete.contains(&p) => Err(status(503)),
            ("delete", Some(i)) => {
                let (_, c, _) = st.files.remove(i);
                st.trash.push(JValue::obj([
                    ("op", JValue::from("delete")),
                    ("path", JValue::Str(p.clone())),
                    ("content", JValue::Str(c)),
                ]));
                Ok(JValue::Obj(vec![]))
            }
            ("preserve", Some(i)) => {
                let c = st.files[i].1.clone();
                st.trash.push(JValue::obj([
                    ("op", JValue::from("preserve")),
                    ("path", JValue::Str(p.clone())),
                    ("content", JValue::Str(c)),
                ]));
                Ok(JValue::Obj(vec![]))
            }
            ("stat" | "move" | "delete" | "preserve", None) => Err(status(404)),
            _ => Err(status(400)),
        };
        Box::pin(async move { r })
    }
}

#[derive(Default)]
struct State {
    lists: Lists,
    history: Vec<(String, JValue)>,
    tombstones: HashSet<String>,
}

struct Source {
    dir: PathBuf,
    st: Mutex<State>,
}

impl MirrorSource for Source {
    fn lists(&self, _: &str) -> Result<Lists, String> {
        Ok(self.st.lock().unwrap().lists.clone())
    }
    fn deleted(&self, _: &str) -> Result<HashSet<String>, String> {
        Ok(self.st.lock().unwrap().tombstones.clone())
    }
    fn history(&self, _: &str, chat: &str) -> Result<Vec<JValue>, String> {
        let st = self.st.lock().unwrap();
        match st.history.iter().find(|(k, _)| k == chat).map(|(_, v)| v) {
            Some(JValue::Str(t)) if t == "throw" => Err("history unreadable".into()),
            Some(JValue::Arr(h)) => Ok(h.clone()),
            _ => Ok(vec![]),
        }
    }
    fn brain(&self, _: &str, chat: &str) -> Option<JValue> {
        brain::read(&self.dir, chat).map(|r| r.brain)
    }
    fn read_index(&self, _: &str) -> JValue {
        chat_mirror_exec::read_index(&self.dir)
    }
    fn write_index(&self, _: &str, state: &JValue) -> Result<(), String> {
        chat_mirror_exec::write_index(&self.dir, state)
    }
}

fn arr(v: &JValue) -> Vec<JValue> {
    match v {
        JValue::Arr(a) => a.clone(),
        _ => vec![],
    }
}

fn js_order(a: &str, b: &str) -> std::cmp::Ordering {
    jsx::units(a).cmp(&jsx::units(b))
}

#[tokio::test(flavor = "current_thread")]
async fn mirror_passes_match_node() {
    let fx = fixture();
    let mut compared = 0;
    for scenario in arr(fx.get("mirror")) {
        let name = scenario.get("name").as_str().unwrap().to_owned();
        let steps = wtf16::parse(scenario.get("steps").as_str().unwrap()).unwrap();
        let expected = arr(scenario.get("passes"));
        let dir = tmpdir("mirror");
        let source = Source {
            dir: dir.clone(),
            st: Mutex::new(State::default()),
        };
        let diary = FakeDiary::default();
        let mut pass_no = 0;
        for step in arr(&steps) {
            {
                let mut st = source.st.lock().unwrap();
                if step.get("lists").truthy() {
                    st.lists = Lists {
                        free_chats: arr(step.get("lists").get("freeChats")),
                        projects: arr(step.get("lists").get("projects")),
                    };
                }
                if let JValue::Obj(h) = step.get("history") {
                    for (k, v) in h {
                        st.history.retain(|(q, _)| q != k);
                        st.history.push((k.clone(), v.clone()));
                    }
                }
                if step.get("tombstones").truthy() {
                    st.tombstones = arr(step.get("tombstones"))
                        .iter()
                        .filter_map(|v| v.as_str().map(str::to_owned))
                        .collect();
                }
            }
            if let JValue::Obj(b) = step.get("brains") {
                for (id, v) in b {
                    brain::write(&dir, id, v, 1.0, 2.0).unwrap();
                }
            }
            {
                let mut d = diary.0.lock().unwrap();
                if let JValue::Obj(e) = step.get("edit") {
                    for (p, c) in e {
                        let v = d.next();
                        d.put(p, c.as_str().unwrap().to_owned(), v);
                    }
                }
                if let JValue::Obj(e) = step.get("editV") {
                    for (p, cv) in e {
                        let cv = arr(cv);
                        d.put(
                            p,
                            cv[0].as_str().unwrap().to_owned(),
                            cv[1].as_str().unwrap().to_owned(),
                        );
                    }
                }
                for p in arr(step.get("mkdir")) {
                    d.dirs.insert(p.as_str().unwrap().to_owned());
                }
                if step.get("failDelete").truthy() {
                    d.fail_delete = arr(step.get("failDelete"))
                        .iter()
                        .map(|v| v.as_str().unwrap().to_owned())
                        .collect();
                }
                for p in arr(step.get("remove")) {
                    let p = p.as_str().unwrap().to_owned();
                    d.files.retain(|(q, _, _)| *q != p);
                }
                if step.get("failWrite").truthy() {
                    d.fail_write = arr(step.get("failWrite"))
                        .iter()
                        .map(|v| v.as_str().unwrap().to_owned())
                        .collect();
                }
            }
            if let Some(i) = step.get("index").as_str() {
                std::fs::write(dir.join(chat_mirror_exec::INDEX_FILE), i).unwrap();
            }
            if !step.get("pass").truthy() {
                continue;
            }
            diary.0.lock().unwrap().calls.clear();
            let dirty: HashSet<String> = arr(step.get("pass").get("dirty"))
                .iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect();
            let result = match chat_mirror_exec::sync::sync("u1", &dirty, &source, &diary).await {
                Ok(true) => "ok",
                Ok(false) => "partial",
                Err(_) => "threw",
            };
            let want = &expected[pass_no];
            let at = format!("{name} pass {pass_no}");
            assert_eq!(result, want.get("result").as_str().unwrap(), "{at}: result");
            let d = diary.0.lock().unwrap();
            assert_eq!(
                s(&JValue::Arr(d.calls.clone())),
                s(want.get("calls")),
                "{at}: Diary calls"
            );
            let mut files = d.files.clone();
            files.sort_by(|a, b| js_order(&a.0, &b.0));
            let tree = JValue::Obj(
                files
                    .into_iter()
                    .map(|(p, c, v)| {
                        (
                            p,
                            JValue::obj([("content", JValue::Str(c)), ("version", JValue::Str(v))]),
                        )
                    })
                    .collect(),
            );
            assert_eq!(s(&tree), s(want.get("files")), "{at}: vault tree");
            assert_eq!(
                s(&JValue::Arr(d.trash.clone())),
                s(want.get("trash")),
                "{at}: trash"
            );
            let index = std::fs::read(dir.join(chat_mirror_exec::INDEX_FILE))
                .ok()
                .map(|b| String::from_utf8(b).unwrap());
            match want.get("index").as_str() {
                Some(w) => assert_eq!(index.as_deref(), Some(w), "{at}: index bytes"),
                None => assert!(index.is_none(), "{at}: no index"),
            }
            pass_no += 1;
            compared += 1;
        }
        assert_eq!(pass_no, expected.len(), "{name}: pass count");
        let _ = std::fs::remove_dir_all(&dir);
    }
    assert!(compared == 32, "compared {compared} passes");
}

#[test]
fn brain_parse_and_render_match_node() {
    let fx = fixture();
    let b = fx.get("brain");
    let outputs = arr(b.get("outputs"));
    let parsed = arr(b.get("parsed"));
    assert_eq!(outputs.len(), parsed.len());
    for (i, (o, want)) in outputs.iter().zip(parsed.iter()).enumerate() {
        let got = brain::parse(o.as_str().unwrap());
        match (got, want.get("ok").truthy()) {
            (Ok(brain), true) => {
                assert_eq!(
                    s(&brain),
                    want.get("brain").as_str().unwrap(),
                    "case {i}: brain"
                );
                assert_eq!(
                    brain::render_markdown(&brain),
                    want.get("md").as_str().unwrap(),
                    "case {i}: markdown"
                );
            }
            (Err((reason, _)), false) => assert_eq!(
                reason,
                want.get("reason").as_str().unwrap(),
                "case {i}: reason"
            ),
            (g, _) => panic!("case {i}: got {g:?}, want {}", s(want)),
        }
    }
    assert_eq!(s(&brain::json_schema()), b.get("schema").as_str().unwrap());
    assert_eq!(brain::SYSTEM, b.get("system").as_str().unwrap());
}

#[test]
fn brain_store_matches_node() {
    let fx = fixture();
    let a = wtf16::parse(r#"{"brain_schema":1,"summary":"  A plan <for> the trip & more.  ","decisions":["Go by train"],"facts":[],"open_questions":["Which day?"],"entities":["Lisbon","Ana"]}"#).unwrap();
    let dir = tmpdir("store");
    for rec in arr(fx.get("brain").get("stored")) {
        let id = rec.get("id").as_str().unwrap();
        brain::write(&dir, id, &a, 1_730_000_000_000.0, 1_730_000_000_001.0).unwrap();
        let file = dir.join(rec.get("file").as_str().unwrap());
        assert_eq!(brain::file_for(&dir, id), file, "{id}: file name");
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            rec.get("bytes").as_str().unwrap(),
            "{id}: bytes"
        );
        let r = brain::read(&dir, id).unwrap();
        let read = JValue::obj([
            ("brain", r.brain),
            ("chatId", JValue::from(id)),
            ("sourceUpdatedAt", JValue::Num(r.source_updated_at)),
            ("builtAt", JValue::Num(r.built_at)),
        ]);
        assert_eq!(s(&read), rec.get("read").as_str().unwrap(), "{id}: read");
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        brain::remove(&dir, id);
        assert!(brain::read(&dir, id).is_none());
    }
    let _ = std::fs::remove_dir_all(&dir);
}

struct FakeReasoner {
    model: String,
    loaded: Option<String>,
    admit: Option<String>,
    reply: Option<String>,
    seen: Mutex<Vec<(String, JValue)>>,
}

impl Reasoner for FakeReasoner {
    fn enabled(&self) -> bool {
        true
    }
    fn model(&self) -> String {
        self.model.clone()
    }
    fn loaded_model(&self) -> BoxFut<Result<Option<String>, ()>> {
        let l = self.loaded.clone();
        Box::pin(async move { Ok(l) })
    }
    fn admit(&self, model: String, answer: Option<String>) -> BoxFut<Result<Option<String>, ()>> {
        let local = answer.is_some();
        self.seen.lock().unwrap().push((
            "admit".into(),
            JValue::Arr(vec![
                JValue::Str(model),
                JValue::obj([
                    ("answerModel", answer.map_or(JValue::Null, JValue::Str)),
                    ("answerIsLocal", JValue::Bool(local)),
                ]),
            ]),
        ));
        let a = self.admit.clone();
        Box::pin(async move { Ok(a) })
    }
    fn complete(&self, c: Completion) -> BoxFut<Result<String, String>> {
        let call = JValue::obj([
            ("model", JValue::Str(c.model)),
            ("schema", c.schema),
            ("messages", c.messages),
        ]);
        self.seen
            .lock()
            .unwrap()
            .push(("call".into(), JValue::Str(s(&call))));
        let r = self.reply.clone().ok_or_else(|| "down".to_owned());
        Box::pin(async move { r })
    }
}

#[tokio::test(flavor = "current_thread")]
async fn brain_build_matches_node() {
    let fx = fixture();
    let chat = JValue::obj([("title", JValue::from("Trip & co"))]);
    for case in arr(fx.get("brain").get("builds")) {
        let name = case.get("name").as_str().unwrap().to_owned();
        let r = FakeReasoner {
            model: case.get("model").as_str().unwrap().to_owned(),
            loaded: case.get("loaded").as_str().map(str::to_owned),
            admit: case.get("admit").as_str().map(str::to_owned),
            reply: case.get("reply").as_str().map(str::to_owned),
            seen: Mutex::new(vec![]),
        };
        let history = arr(&wtf16::parse(case.get("history").as_str().unwrap()).unwrap());
        let got = match brain::build(&r, &chat, &history).await {
            Ok(b) => s(&b),
            Err(reason) => reason,
        };
        assert_eq!(got, case.get("result").as_str().unwrap(), "{name}: result");
        let seen = JValue::Obj(r.seen.into_inner().unwrap());
        assert_eq!(
            s(&seen),
            case.get("seen").as_str().unwrap(),
            "{name}: reasoner calls"
        );
    }
}
