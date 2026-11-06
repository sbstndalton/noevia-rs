//! The chat mirror's work, native (sbstndalton/noevia#741, #742, #2153; crate `chat-mirror-exec`):
//! one Diary mirror pass for one account and one chat's brain build. They replace Node's executors
//! (core `chat-vault-mirror.cjs` sync and `chat-brain.cjs` createBrainRunner/createBrainBuilder),
//! with core's output over the inputs core accepted (crates/chat-mirror-exec/tests/oracle.rs; the
//! deviations are listed in that crate's lib.rs).
//!
//! What a pass reads is the account's own: its lists (`projects.json` without the internal
//! projects, `free-chats.json`), its tombstones (`deleted-chats.json`), its transcripts (the
//! store), its brains and its index (`users/<id>/`). Its Diary calls go through the Diary client
//! as that account (`server_diary::Diary::call_file_within`, core `diaryFiles`, 15 s). The gates
//! are core's (`server_account::vault_mirror::exec_gates`), read when the pass or the build starts
//! and, for a build, again after the model call; an unreadable gate fails closed.
//!
//! The reasoner role goes through [`EngineSeam`], the trait the engine's admission sits behind
//! (as row A's `EngineEntry`). Its implementation today is [`NodeEngine`]: the answer model, the
//! `admitReasoner` decision (keep-alongside list and load budget) and the call on the default
//! provider are still Node's, reached on loopback at `POST /internal/chat-mirror/engine`
//! (`{op: loaded|admit|complete}`). SWITCH LATER: when the front owns the engine admission (row H,
//! move/inference-admission-h) and the provider registry, implement `EngineSeam` over those and
//! delete core's seam. The prompt, the schema, the parameters and the deadline are this crate's
//! data; core only forwards the call.
use crate::research_routes::Research;
use chat_mirror_exec::brain::{self, Reasoner};
use chat_mirror_exec::{
    BoxFut, BrainSource, Cas, Completion, Diary, FileError, Leaf, Lists, MirrorSource, Revision,
    UserRoot,
};
use chat_mirror_sched::{BrainExecutor, MirrorExecutor, MirrorOutcome};
use js_json::{wtf16, JValue};
use server_account::vault_mirror::{exec_gates, ExecGates};
use server_projects::chats::{FREE_CHATS_FILE, TOMBSTONES_FILE};
use server_projects::store;
use server_store::Store;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

pub const ENGINE_PATH: &str = "/internal/chat-mirror/engine";
/// core diaryFileCall's deadline.
const DIARY_CALL: Duration = Duration::from_secs(15);
const ENGINE_FACTS: Duration = Duration::from_secs(10);
/// The reasoner's own deadline is 30 s (brain::DEFAULT_DEADLINE); the bridge allows a little more.
const ENGINE_CALL: Duration = Duration::from_secs(40);

/// What the executors need from the process; each is resolved per use, so a store or a Diary
/// that is not ready yet fails that one pass (retried), never the schedule.
pub trait Host: Send + Sync + 'static {
    fn data_dir(&self) -> &Path;
    /// Blocking: the account's gates. `None`: not readable (fail closed).
    fn gates(&self, user: &str) -> Option<ExecGates>;
    /// Blocking: the Diary file API as the account. `None`: no Diary configured or no store.
    fn files(&self) -> Option<Arc<dyn Diary>>;
    fn engine(&self) -> Arc<dyn EngineSeam>;
    fn log(&self, line: &str) {
        eprintln!("noevia-server: {line}");
    }
}

/// The engine's admission and the call (see the module notes for what implements it).
pub trait EngineSeam: Send + Sync + 'static {
    fn loaded(&self) -> BoxFut<Result<Option<String>, ()>>;
    fn admit(
        &self,
        model: String,
        answer_model: Option<String>,
    ) -> BoxFut<Result<Option<String>, ()>>;
    fn complete(&self, call: Completion) -> BoxFut<Result<String, String>>;
    /// A `brain.fallback` / `brain.built` entry for the decision log (core recordDecision('chat-brain')).
    /// Fire and forget.
    fn record(&self, _entry: JValue) {}
}

/// The production host: the App's own layers.
pub struct AppHost {
    pub data_dir: PathBuf,
    pub identity: Arc<crate::identity::IdentityLayer>,
    pub feature_context: Option<Arc<crate::feature_context::Layer>>,
    pub diary: Option<Arc<server_diary::Diary>>,
    pub engine: Arc<dyn EngineSeam>,
}

impl Host for AppHost {
    fn data_dir(&self) -> &Path {
        &self.data_dir
    }
    fn gates(&self, user: &str) -> Option<ExecGates> {
        let store = self.identity.store_blocking().ok()?;
        let controller = self.feature_context.as_ref()?.controller();
        store
            .read(|r| exec_gates(&controller, r, &self.data_dir, user))
            .ok()
    }
    fn files(&self) -> Option<Arc<dyn Diary>> {
        let diary = self.diary.clone()?;
        let store = self.identity.store_blocking().ok()?;
        Some(Arc::new(DiaryAs {
            diary,
            store,
            data_dir: self.data_dir.clone(),
        }))
    }
    fn engine(&self) -> Arc<dyn EngineSeam> {
        Arc::clone(&self.engine)
    }
}

/// The executors behind the schedule.
pub struct NativeWork<H: Host> {
    host: Arc<H>,
}

impl<H: Host> NativeWork<H> {
    pub fn new(host: Arc<H>) -> Arc<Self> {
        Arc::new(NativeWork { host })
    }
}

async fn gates_async<H: Host>(host: &Arc<H>, user: &str) -> Option<ExecGates> {
    let (h, u) = (Arc::clone(host), user.to_owned());
    tokio::task::spawn_blocking(move || h.gates(&u))
        .await
        .ok()
        .flatten()
}

impl<H: Host> MirrorExecutor for NativeWork<H> {
    fn sync(&self, user: &str, dirty: Vec<String>) -> BoxFut<MirrorOutcome> {
        let host = Arc::clone(&self.host);
        let user = user.to_owned();
        Box::pin(async move {
            // Checked when it runs, not when it was asked for: switching off stops writes at once.
            let Some(g) = gates_async(&host, &user).await else {
                host.log("chat-vault-mirror: sync failed: the feature gates are not readable");
                return MirrorOutcome::Failed;
            };
            if !(g.available && g.opted_in) {
                return MirrorOutcome::Off;
            }
            let h = Arc::clone(&host);
            let Some(files) = tokio::task::spawn_blocking(move || h.files())
                .await
                .ok()
                .flatten()
            else {
                host.log("chat-vault-mirror: sync failed: the Diary is not configured");
                return MirrorOutcome::Failed;
            };
            // Held open before any Diary call: a linked or missing account folder fails here.
            let root = match open_root(host.data_dir(), &user) {
                Ok(r) => r,
                Err(e) => {
                    host.log(&format!("chat-vault-mirror: sync failed {e}"));
                    return MirrorOutcome::Failed;
                }
            };
            let source = Workspace {
                host: Arc::clone(&host),
                root,
            };
            let dirty: HashSet<String> = dirty.into_iter().collect();
            match chat_mirror_exec::sync::sync(&user, &dirty, &source, &*files).await {
                Ok(true) => MirrorOutcome::Done,
                Ok(false) => MirrorOutcome::Failed,
                Err(e) => {
                    host.log(&format!("chat-vault-mirror: sync failed {e}"));
                    MirrorOutcome::Failed
                }
            }
        })
    }
}

impl<H: Host> BrainExecutor for NativeWork<H> {
    fn run(&self, user: &str, chat: &str) -> BoxFut<String> {
        let host = Arc::clone(&self.host);
        let (user, chat) = (user.to_owned(), chat.to_owned());
        Box::pin(async move {
            let Some(g) = gates_async(&host, &user).await else {
                host.log("chat-brain: build failed: the feature gates are not readable");
                return "error".to_owned();
            };
            let source = Brains {
                root: open_root(host.data_dir(), &user),
                user: user.clone(),
                host: Arc::clone(&host),
                first: std::sync::Mutex::new(Some(g.clone())),
            };
            let reasoner = EngineReasoner {
                gates: g,
                engine: host.engine(),
            };
            chat_mirror_exec::run_brain(&user, &chat, &source, &reasoner).await
        })
    }
}

// ── The account's workspace ─────────────────────────────────────────────────────────────────
//
// Everything local goes through the account folder held open ([`UserRoot`], review 2026-11-05
// P1): `users/<id>` is opened without following a link and every file below it is opened
// relative to that descriptor, never followed, and the folder is re-checked before each step. The
// readers keep the store's parsing (`store::parse_projects`, `store::parse_json`); they differ
// only in how the bytes are opened, and in that a link or a non-regular file there is an error.

/// Larger than any list or transcript the store writes.
const MAX_FILE: u64 = 512 * 1024 * 1024;

/// The account folder for `user` (a store id: 36 hex digits and dashes), held open.
fn open_root(data_dir: &Path, user: &str) -> Result<UserRoot, String> {
    store::user_dir(data_dir, user).map_err(|e| format!("{e:?}"))?;
    UserRoot::open(data_dir, user)
}

fn bytes(root: &UserRoot, file: &str) -> Result<Option<Vec<u8>>, String> {
    match root.read(None, file, MAX_FILE)? {
        Leaf::Missing => Ok(None),
        Leaf::File { over: true, .. } => Err(format!("refused: {file} is too large")),
        Leaf::File { bytes, .. } => Ok(Some(bytes)),
    }
}

fn lists(root: &UserRoot) -> Result<Lists, String> {
    let free_chats = match bytes(root, FREE_CHATS_FILE)? {
        Some(b) => match store::parse_json(&b).map_err(|e| format!("{e:?}"))? {
            Some(JValue::Arr(items)) => items,
            _ => Vec::new(),
        },
        None => Vec::new(),
    };
    let projects = match bytes(root, store::PROJECTS_FILE)? {
        Some(b) => store::parse_projects(&b).map_err(|e| format!("{e:?}"))?,
        None => Vec::new(),
    };
    let projects = projects
        .into_iter()
        .filter(|p| !server_projects::source_reply::internal_project(p))
        .collect();
    Ok(Lists {
        free_chats,
        projects,
    })
}

/// chat-lists.cjs `readTombstones(dir)`: the strings of the list; nothing when it does not parse.
/// A link or an unreadable file there is an error (never followed).
fn tombstone_list(root: &UserRoot) -> Result<Vec<String>, String> {
    Ok(match bytes(root, TOMBSTONES_FILE)? {
        Some(b) => match store::parse_json(&b) {
            Ok(Some(JValue::Arr(ids))) => ids
                .iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect(),
            _ => Vec::new(),
        },
        None => Vec::new(),
    })
}

fn tombstones(root: &UserRoot) -> Result<HashSet<String>, String> {
    Ok(tombstone_list(root)?.into_iter().collect())
}

/// `transcript::read(...).history` through the held folder: a deleted chat has none; otherwise
/// `.history || []` of `history-<safe id>.json`.
fn history(root: &UserRoot, chat: &str) -> Result<Vec<JValue>, String> {
    let safe = server_projects::history::safe_chat_id(chat);
    if safe.is_empty() {
        return Err("InvalidChat".into());
    }
    if tombstone_list(root)?.contains(&safe) {
        return Ok(Vec::new());
    }
    let parsed = match bytes(root, &format!("history-{safe}.json"))? {
        Some(b) => store::parse_json(&b).map_err(|e| format!("{e:?}"))?,
        None => None,
    };
    let stored = match &parsed {
        Some(v @ JValue::Obj(_)) => v.get("history").clone(),
        _ => JValue::Undefined,
    };
    let history = if stored.truthy() {
        stored
    } else {
        JValue::Arr(Vec::new())
    };
    server_projects::history::revision_of(&history).ok_or("Corrupt")?;
    Ok(match history {
        JValue::Arr(items) => items,
        _ => Vec::new(),
    })
}

struct Workspace<H: Host> {
    host: Arc<H>,
    root: UserRoot,
}

impl<H: Host> MirrorSource for Workspace<H> {
    fn lists(&self, _: &str) -> Result<Lists, String> {
        lists(&self.root)
    }
    fn deleted(&self, _: &str) -> Result<HashSet<String>, String> {
        tombstones(&self.root)
    }
    fn history(&self, _: &str, chat: &str) -> Result<Vec<JValue>, String> {
        history(&self.root, chat)
    }
    fn brain(&self, _: &str, chat: &str) -> Option<JValue> {
        brain::read(&self.root, chat).map(|r| r.brain)
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
    fn log(&self, line: &str) {
        self.host.log(line);
    }
}

struct Brains<H: Host> {
    host: Arc<H>,
    user: String,
    /// The account folder, opened when the build started; `Err` fails each step that needs it.
    root: Result<UserRoot, String>,
    /// The gates read when the build started (core read them on the first `enabled` call); later
    /// calls read them again.
    first: std::sync::Mutex<Option<ExecGates>>,
}

impl<H: Host> Brains<H> {
    fn root(&self) -> Result<&UserRoot, String> {
        self.root.as_ref().map_err(Clone::clone)
    }
}

impl<H: Host> BrainSource for Brains<H> {
    fn enabled(&self, user: &str) -> bool {
        let first = match self.first.lock() {
            Ok(mut f) => f.take(),
            Err(p) => p.into_inner().take(),
        };
        // A re-check after the model call reads the files and the store again (blocking, brief).
        let g = first.or_else(|| self.host.gates(user));
        g.is_some_and(|g| g.reasoner && g.available && g.opted_in)
    }
    fn chat(&self, _: &str, chat: &str) -> Result<Option<JValue>, String> {
        listed(self.root()?, chat)
    }
    fn snapshot(&self, _: &str, chat: &str) -> Result<(Vec<JValue>, String), String> {
        let history = history(self.root()?, chat)?;
        let revision = revision(&history)?;
        Ok((history, revision))
    }
    fn deleted(&self, _: &str) -> Result<HashSet<String>, String> {
        tombstones(self.root()?)
    }
    fn read(&self, _: &str, chat: &str) -> Option<brain::Record> {
        brain::read(self.root().ok()?, chat)
    }
    fn write_if(
        &self,
        _: &str,
        chat: &str,
        expected: &Revision,
        b: &JValue,
        source_updated_at: f64,
        built_at: f64,
    ) -> Result<Cas, String> {
        let root = self.root()?;
        // The authority's locks, in chat_move's order: the transcript lock (every transcript save
        // and chat delete), then projects.json and free-chats.json (every list change). Held across
        // the re-read and the write, across processes (file locks), so nothing changes in between.
        let dir = self.host.data_dir().join("users").join(&self.user);
        let lock = |name: &str| {
            server_projects::lock::acquire(&dir.join(name)).map_err(|e| format!("lock: {e:?}"))
        };
        let _transcripts = lock(server_projects::transcript::LOCK_NAME)?;
        let _projects = lock(store::PROJECTS_FILE)?;
        let _free = lock(FREE_CHATS_FILE)?;
        root.verify()?;
        let Some(meta) = listed(root, chat)? else {
            if tombstones(root)?.contains(chat) {
                brain::remove(root, chat);
            }
            return Ok(Cas::Gone);
        };
        let same_time =
            wtf16::stringify(meta.get("updatedAt")) == wtf16::stringify(&expected.updated_at);
        if !same_time || revision(&history(root, chat)?)? != expected.transcript {
            return Ok(Cas::Stale);
        }
        brain::write(root, chat, b, source_updated_at, built_at)?;
        Ok(Cas::Written)
    }
    fn remove(&self, _: &str, chat: &str) {
        if let Ok(root) = self.root() {
            brain::remove(root, chat);
        }
    }
    #[allow(clippy::cast_precision_loss)]
    fn now_ms(&self) -> f64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0.0, |d| d.as_millis() as f64)
    }
    fn log(&self, line: &str) {
        self.host.log(line);
    }
}

fn revision(history: &[JValue]) -> Result<String, String> {
    server_projects::history::revision_of(&JValue::Arr(history.to_vec()))
        .ok_or_else(|| "Corrupt".to_owned())
}

/// The chat's entry in the account's own lists (flatMap of the projects' chats, then free chats).
fn listed(root: &UserRoot, chat: &str) -> Result<Option<JValue>, String> {
    {
        let l = lists(root)?;
        let mut all = l.free_chats.into_iter();
        if let Some(c) = all.find(|c| c.truthy() && c.get("id").as_str() == Some(chat)) {
            return Ok(Some(c));
        }
        for p in l.projects {
            match p.get("chats") {
                JValue::Arr(items) => {
                    if let Some(c) = items
                        .iter()
                        .find(|c| c.truthy() && c.get("id").as_str() == Some(chat))
                    {
                        return Ok(Some(c.clone()));
                    }
                }
                // flatMap keeps a non-array value as one element.
                c if c.truthy() && c.get("id").as_str() == Some(chat) => {
                    return Ok(Some(c.clone()))
                }
                _ => {}
            }
        }
        Ok(None)
    }
}

struct EngineReasoner {
    gates: ExecGates,
    engine: Arc<dyn EngineSeam>,
}

impl Reasoner for EngineReasoner {
    fn enabled(&self) -> bool {
        self.gates.reasoner && self.gates.framing
    }
    fn model(&self) -> String {
        self.gates.reasoner_model.clone()
    }
    fn loaded_model(&self) -> BoxFut<Result<Option<String>, ()>> {
        self.engine.loaded()
    }
    fn admit(
        &self,
        model: String,
        answer_model: Option<String>,
    ) -> BoxFut<Result<Option<String>, ()>> {
        self.engine.admit(model, answer_model)
    }
    fn complete(&self, call: Completion) -> BoxFut<Result<String, String>> {
        self.engine.complete(call)
    }
    fn record(&self, entry: JValue) {
        self.engine.record(entry);
    }
}

// ── The Diary as the account ────────────────────────────────────────────────────────────────

struct DiaryAs {
    diary: Arc<server_diary::Diary>,
    store: Arc<Store>,
    data_dir: PathBuf,
}

impl DiaryAs {
    fn call(
        &self,
        user: &str,
        method: reqwest::Method,
        endpoint: &'static str,
        body: JValue,
    ) -> BoxFut<Result<JValue, FileError>> {
        let (diary, store, data_dir, user) = (
            Arc::clone(&self.diary),
            Arc::clone(&self.store),
            self.data_dir.clone(),
            user.to_owned(),
        );
        Box::pin(async move {
            let text = wtf16::stringify(&body);
            diary
                .call_file_within(&store, &data_dir, &user, method, endpoint, text, DIARY_CALL)
                .await
                .map_err(|f| FileError {
                    status: Some(f.status),
                    message: f.message,
                })
        })
    }
}

impl Diary for DiaryAs {
    fn read(&self, user: &str, path: &str) -> BoxFut<Result<JValue, FileError>> {
        self.call(
            user,
            reqwest::Method::POST,
            "/file",
            JValue::obj([("path", JValue::from(path))]),
        )
    }
    fn write(&self, user: &str, body: JValue) -> BoxFut<Result<JValue, FileError>> {
        self.call(user, reqwest::Method::PUT, "/file", body)
    }
    fn mkdir(&self, user: &str, path: &str) -> BoxFut<Result<JValue, FileError>> {
        self.call(
            user,
            reqwest::Method::POST,
            "/directory",
            JValue::obj([("path", JValue::from(path))]),
        )
    }
    fn ops(&self, user: &str, body: JValue) -> BoxFut<Result<JValue, FileError>> {
        self.call(user, reqwest::Method::POST, "/workspace-ops", body)
    }
}

// ── The engine seam, Node's for now ─────────────────────────────────────────────────────────

/// [`EngineSeam`] over core's loopback seam (see the module notes).
pub struct NodeEngine {
    pub bridge: Arc<dyn NodeBridge>,
    pub log: Arc<dyn Fn(&str) + Send + Sync>,
}

/// Core's loopback (the per-start secret): `POST ENGINE_PATH` with a JSON body.
pub trait NodeBridge: Send + Sync + 'static {
    fn post(&self, body: String, deadline: Duration) -> BoxFut<Option<(u16, Vec<u8>)>>;
}

/// The production bridge: the research layer's loopback client.
pub(crate) struct ResearchBridge(pub Arc<Research>);

impl NodeBridge for ResearchBridge {
    fn post(&self, body: String, deadline: Duration) -> BoxFut<Option<(u16, Vec<u8>)>> {
        let research = Arc::clone(&self.0);
        Box::pin(async move { research.node_call_text(ENGINE_PATH, body, deadline).await })
    }
}

impl NodeEngine {
    pub(crate) fn over(research: Arc<Research>) -> Self {
        NodeEngine {
            bridge: Arc::new(ResearchBridge(research)),
            log: Arc::new(|line: &str| eprintln!("noevia-server: {line}")),
        }
    }
}

impl NodeEngine {
    fn post(&self, body: JValue, deadline: Duration) -> BoxFut<Option<(u16, Vec<u8>)>> {
        let Some(text) = wtf16::stringify(&body) else {
            return Box::pin(async { None });
        };
        self.bridge.post(text, deadline)
    }
    fn ask(&self, body: JValue, deadline: Duration) -> BoxFut<Option<JValue>> {
        let post = self.post(body, deadline);
        Box::pin(async move {
            let (status, bytes) = post.await?;
            if status != 200 {
                return None;
            }
            wtf16::parse(&String::from_utf8_lossy(&bytes)).ok()
        })
    }
}

fn opt_str(v: &JValue) -> Result<Option<String>, ()> {
    match v {
        JValue::Null => Ok(None),
        JValue::Str(s) => Ok(Some(s.clone())),
        _ => Err(()),
    }
}

impl EngineSeam for NodeEngine {
    fn loaded(&self) -> BoxFut<Result<Option<String>, ()>> {
        let ask = self.ask(JValue::obj([("op", JValue::from("loaded"))]), ENGINE_FACTS);
        Box::pin(async move { ask.await.ok_or(()).and_then(|v| opt_str(v.get("model"))) })
    }
    fn admit(
        &self,
        model: String,
        answer_model: Option<String>,
    ) -> BoxFut<Result<Option<String>, ()>> {
        let body = JValue::obj([
            ("op", JValue::from("admit")),
            ("model", JValue::Str(model)),
            (
                "answerModel",
                answer_model.map_or(JValue::Null, JValue::Str),
            ),
        ]);
        let ask = self.ask(body, ENGINE_FACTS);
        Box::pin(async move { ask.await.ok_or(()).and_then(|v| opt_str(v.get("refusal"))) })
    }
    fn record(&self, entry: JValue) {
        // Core answers a recorded entry with 204 (no body); only that is delivered.
        let post = self.post(
            JValue::obj([("op", JValue::from("record")), ("entry", entry)]),
            ENGINE_FACTS,
        );
        let log = Arc::clone(&self.log);
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn(async move {
                match post.await {
                    Some((204, _)) => {}
                    Some((status, _)) => log(&format!(
                        "chat-brain: a decision log entry was not recorded ({status})"
                    )),
                    None => log("chat-brain: a decision log entry was not recorded (no answer)"),
                }
            });
        }
    }
    fn complete(&self, call: Completion) -> BoxFut<Result<String, String>> {
        let body = JValue::obj([
            ("op", JValue::from("complete")),
            ("model", JValue::Str(call.model)),
            ("schema", call.schema),
            ("messages", call.messages),
        ]);
        let ask = self.ask(body, ENGINE_CALL);
        Box::pin(async move {
            match ask.await {
                Some(v) => v
                    .get("content")
                    .as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| "no content".to_owned()),
                None => Err("the engine did not answer".to_owned()),
            }
        })
    }
}
