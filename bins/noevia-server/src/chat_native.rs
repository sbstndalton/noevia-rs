//! The native chat executor (track R8): `POST /api/chat` for the accounts the executor switch names,
//! the turn run by `crates/chat-turn` after core prepared it.
//!
//! [`serve`] relays the turn to core's `/internal/chat/turn` as chat_turn.rs does, with one more header
//! (`x-noevia-chat-handoff`, the per-start secret; any copy the client sent is removed). Core runs its
//! preparation as always and, for a turn the native executor can run, ends it with one `prepared` frame
//! (rust-chat-native.cjs); any other turn core runs to the end on the same stream. Every frame before
//! `prepared` reaches the client as core wrote it; from there [`chat_turn::native::run`] continues with
//! [`FrontHost`], and [`chat_turn::after::Saving`] keeps what the turn leaves.
//!
//! Switch: `NOEVIA_CHAT_EXECUTOR` (config.rs): unset or `node` is today's relay; `native:<user id>,...`
//! names accounts; `native` is everyone. The switch engages only on a front that holds the approval
//! registry (chat port S9): without it [`approvals`] is `None` and every turn is relayed as before.
//! A Diary space and a Cowork start are never handed off (chat_turn.rs only calls here for a turn).
//!
//! What the turn needs of this process, and where it comes from:
//! - the client's stream: SSE frames into the response body ([`Wire`]), a 5 s keep-alive, the 4 s partial
//!   saver (chat-partial.cjs), the turn record fed with every frame sent;
//! - the engine: the prepared `upstreamUrl`, with the model's admission lease (engine-admission
//!   `Ask::Engine`) on every request;
//! - core, through its loopback bridges (rust-chat-native.cjs): tool runs and Drive write targets, the
//!   model manager's health and load, the template tools gate, the engine-busy signal, the chat title
//!   store, usage and prefill samples;
//! - this process: the account's state files and partial (its workspace directory), the tool policy,
//!   the audit log, the transcript append and the tombstones, and the approval registry (S9).

use std::collections::VecDeque;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use axum::body::Body;
use axum::http::{header, HeaderValue, Response};
use bytes::Bytes;
use chat_turn::after::{Keep, Saving};
use chat_turn::handoff::{Frames, Piece};
use chat_turn::host::{Answer, EngineRequest, GateRefusal, Host, Read, Target, ToolRun};
use chat_turn::js;
use chat_turn::native::{self, Helpers, Process};
use chat_turn::prepared::Prepared;
use chat_turn::record::TurnRecord;
use http_body::Frame;
use js_json::{wtf16, JValue};
use tokio::sync::mpsc;

use crate::App;

/// The header core reads to hand the turn off (rust-chat-native.cjs HANDOFF_HEADER).
pub const HANDOFF_HEADER: &str = "x-noevia-chat-handoff";
const KEEP_ALIVE: &[u8] = b": keep-alive\n\n";
const BRIDGE_TIMEOUT: Duration = Duration::from_secs(30);
/// Tool runs can take long (a browser task, a large Drive read); core's own call has no deadline either.
const TOOL_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// Which accounts the native executor runs.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Switch {
    /// Every turn goes to core, as before (the default).
    #[default]
    Node,
    /// These accounts' turns go native.
    Accounts(Vec<String>),
    /// Every account's.
    All,
}

impl Switch {
    /// `NOEVIA_CHAT_EXECUTOR`: unset, empty or `node`; `native`; `native:<id>,<id>`.
    pub fn parse(value: Option<&str>) -> Result<Switch, String> {
        let v = value.map(str::trim).unwrap_or("");
        match v {
            "" | "node" => Ok(Switch::Node),
            "native" => Ok(Switch::All),
            _ => match v.strip_prefix("native:") {
                Some(list) => {
                    let ids: Vec<String> = list
                        .split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                        .collect();
                    if ids.is_empty()
                        || ids.iter().any(|id| {
                            id.len() > 64
                                || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                        })
                    {
                        Err(
                            "NOEVIA_CHAT_EXECUTOR: native:<account id>,... needs account ids"
                                .into(),
                        )
                    } else {
                        Ok(Switch::Accounts(ids))
                    }
                }
                None => Err(
                    "NOEVIA_CHAT_EXECUTOR must be node, native or native:<account id>,...".into(),
                ),
            },
        }
    }

    pub fn names(&self, user: &str) -> bool {
        match self {
            Switch::Node => false,
            Switch::All => true,
            Switch::Accounts(ids) => ids.iter().any(|id| id == user),
        }
    }
}

/// The approval registry's calls a native turn makes (chat port S9's `approval_registry`).
pub trait Approvals: Send + Sync {
    /// The chat's permission binding for `scope` (chat.cjs permissionContext), or `Err` when it cannot
    /// be verified (store failing, chat moved, deleted or held elsewhere, workspace revoked).
    fn binding(&self, user: &str, chat: &str, scope: &str, holder: &JValue) -> Result<JValue, ()>;
    /// "Allow for this chat" holds.
    fn chat_wide(&self, user: &str, chat: Option<&str>, scope: &str, binding: &JValue) -> bool;
    /// Opens the card and waits for its answer: "approve", "deny", "timeout" or "aborted".
    fn ask(
        &self,
        user: &str,
        request: &JValue,
    ) -> Pin<Box<dyn std::future::Future<Output = String> + Send>>;
    /// Ends the chat's grant.
    fn revoke(&self, user: &str, chat: Option<&str>);
}

/// The registry [`install_approvals`] stored (tests only).
static INSTALLED: std::sync::OnceLock<Arc<dyn Approvals>> = std::sync::OnceLock::new();

/// Test-only seam until chat port S9 lands: stores a process-wide approval registry, which
/// [`approvals`] then returns, so an end-to-end test can run the native path. Nothing in the binary
/// calls this; a second call is ignored (the first registry stays).
#[doc(hidden)]
pub fn install_approvals(a: Arc<dyn Approvals>) {
    let _ = INSTALLED.set(a);
}

/// The approval registry, when this front holds one (S9). `None`: the native executor never engages.
pub(crate) fn approvals(_app: &Arc<App>) -> Option<Arc<dyn Approvals>> {
    INSTALLED.get().cloned()
}

/// Whether this turn goes native: the switch names the account and this front can run it.
pub(crate) fn wanted(app: &Arc<App>, user: &str) -> bool {
    app.config.chat_executor.names(user) && approvals(app).is_some()
}

/// The client's body: frames pushed as the turn writes them, ended when the sender goes.
struct Wire(mpsc::UnboundedReceiver<Bytes>);

impl http_body::Body for Wire {
    type Data = Bytes;
    type Error = std::convert::Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        self.0.poll_recv(cx).map(|b| b.map(|b| Ok(Frame::data(b))))
    }
}

fn is_event_stream(res: &Response<Body>) -> bool {
    res.headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("text/event-stream"))
}

/// Relays the turn with the hand-off asked for, and runs it natively from the `prepared` frame.
/// `extra` is what chat_turn.rs sends core (token, user, the gate's lease); `relay` sends it.
pub(crate) async fn serve<F, Fut>(app: &Arc<App>, user: String, relay: F) -> Option<Response<Body>>
where
    F: FnOnce(HeaderValue) -> Fut,
    Fut: std::future::Future<Output = Option<Response<Body>>>,
{
    let token = HeaderValue::from_str(app.config.research.front_token()?).ok()?;
    let approvals = approvals(app)?;
    let res = relay(token).await?;
    if !is_event_stream(&res) {
        return Some(res);
    }
    let (parts, core) = res.into_parts();
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(drive(Arc::clone(app), user, core, tx, approvals));
    Some(Response::from_parts(parts, Body::new(Wire(rx))))
}

/// Strips a client's copy of the hand-off header (only the front may ask for one).
pub(crate) fn strip_client_header(headers: &mut axum::http::HeaderMap) {
    headers.remove(HANDOFF_HEADER);
}

/// Adds the hand-off header to what chat_turn.rs sends core.
pub(crate) fn handoff_header(token: HeaderValue) -> (&'static str, HeaderValue) {
    (HANDOFF_HEADER, token)
}

async fn drive(
    app: Arc<App>,
    user: String,
    core: Body,
    tx: mpsc::UnboundedSender<Bytes>,
    approvals: Arc<dyn Approvals>,
) {
    use http_body_util::BodyExt;
    let mut frames = Frames::new();
    let mut core = core;
    let mut forwarded: Vec<JValue> = Vec::new();
    let prepared = loop {
        let Some(frame) = core.frame().await else {
            break None;
        };
        let Ok(frame) = frame else { break None };
        let Ok(data) = frame.into_data() else {
            continue;
        };
        let mut handed = None;
        for piece in frames.feed(&data) {
            match piece {
                Piece::Forward(bytes) => {
                    for text in String::from_utf8_lossy(&bytes).split("\n\n") {
                        if let Some(json) = text.strip_prefix("data: ") {
                            if let Ok(v) = wtf16::parse(json) {
                                forwarded.push(v);
                            }
                        }
                    }
                    if tx.send(Bytes::from(bytes)).is_err() {
                        // The client left: closing core's stream ends its turn, which keeps it (#679).
                        return;
                    }
                }
                Piece::Prepared(p) => handed = Some(Some(p)),
                Piece::Unreadable => handed = Some(None),
            }
        }
        if let Some(h) = handed {
            break h;
        }
    };
    if !frames.handed_off() {
        // Core ran the turn itself; what it left without a blank line goes out as it is.
        let rest = frames.rest();
        if !rest.is_empty() {
            let _ = tx.send(Bytes::from(rest));
        }
        return;
    }
    drop(core);
    let failed = |tx: &mpsc::UnboundedSender<Bytes>| {
        let e = js::obj([
            ("type", js::s("error")),
            ("text", js::s(native::FAILED_TEXT)),
        ]);
        let _ = tx.send(Bytes::from(format!("data: {}\n\n", js::stringify(&e))));
    };
    let Some(frame) = prepared else {
        eprintln!("chat native: core's prepared frame does not parse");
        return failed(&tx);
    };
    let p = match Prepared::read(&frame) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("chat native: {}", e.0);
            return failed(&tx);
        }
    };
    let Some(data_dir) = app.config.data_dir.clone() else {
        return failed(&tx);
    };
    let Ok(user_dir) = server_projects::store::user_dir(&data_dir, &user) else {
        return failed(&tx);
    };
    let host = Arc::new(FrontHost {
        app: Arc::clone(&app),
        user,
        user_dir,
        data_dir,
        tx,
        saving: Mutex::new(Saving::new(TurnRecord::new(crate::identity::now_ms))),
        lease: tokio::sync::Mutex::new(None),
        approvals,
        client: reqwest::Client::new(),
    });
    for v in &forwarded {
        host.observe(v, false);
    }
    let chat = p.record.then(|| p.chat_id.clone()).flatten();
    if let Some(c) = &chat {
        host.end_partial(&chat_turn::after::safe_chat_id(c));
    }
    let ticks = tokio::spawn(ticks(Arc::clone(&host), chat.clone()));
    let process = process();
    let helpers = helpers();
    let ended = native::run(&*host, process, &helpers, &p).await;
    ticks.abort();
    let left = matches!(ended, native::Ended::Left) || host.left();
    let keep = Arc::clone(&host);
    let message = p.message.clone();
    let _ = tokio::task::spawn_blocking(move || {
        if let Ok(saving) = keep.saving.lock() {
            saving.finish(
                &*keep,
                chat.as_deref(),
                &message,
                left,
                crate::identity::now_ms(),
            );
        }
    })
    .await;
}

/// The keep-alive (5 s) and the partial saver (4 s) while the native part runs.
async fn ticks(host: Arc<FrontHost>, chat: Option<String>) {
    let mut beat = tokio::time::interval(Duration::from_secs(5));
    let mut save = tokio::time::interval(Duration::from_secs(4));
    beat.tick().await;
    save.tick().await;
    loop {
        tokio::select! {
            _ = beat.tick() => {
                if host.tx.send(Bytes::from_static(KEEP_ALIVE)).is_err() { return; }
            }
            _ = save.tick() => {
                let Some(c) = &chat else { continue };
                let entry = host.saving.lock().ok().and_then(|s| s.record.partial_entry());
                if let Some(entry) = entry {
                    let mut file = chat_turn::record::partial_file(&entry, crate::identity::now_ms());
                    if let Some(start) = host.app.config.start_id.clone() {
                        js::set(&mut file, "running", js::obj([("start", js::s(start))]));
                    }
                    host.write_partial(&chat_turn::after::safe_chat_id(c), &file);
                }
            }
        }
    }
}

fn process() -> &'static Process {
    static PROCESS: std::sync::OnceLock<Process> = std::sync::OnceLock::new();
    PROCESS.get_or_init(Process::default)
}

fn helpers() -> Helpers<'static> {
    Helpers {
        clean_search_args: &|n, a| chat_turn::helpers::clean_search_args(n, a).into_owned(),
        announces_tool_action: &chat_turn::helpers::announces_tool_action,
        announced_note: chat_turn::helpers::ANNOUNCED_NOTE,
        reduce_tool_result: &|t, cap| chat_turn::reduce::reduce_tool_result(t, cap).text,
        source_links: &chat_turn::helpers::source_links,
        fallback_text: &|s| chat_turn::helpers::fallback_text(s),
        final_answer_note: chat_turn::helpers::FINAL_ANSWER_NOTE,
        // core's TOOL_RESULT_CAP (toolboxes.cjs), the model's copy of a tool result.
        tool_result_cap: 8000,
    }
}

/// One native turn's side of this process.
pub(crate) struct FrontHost {
    app: Arc<App>,
    user: String,
    user_dir: PathBuf,
    data_dir: PathBuf,
    tx: mpsc::UnboundedSender<Bytes>,
    saving: Mutex<Saving>,
    lease: tokio::sync::Mutex<Option<engine_admission::Lease>>,
    approvals: Arc<dyn Approvals>,
    client: reqwest::Client,
}

/// An engine answer being read.
pub(crate) struct EngineBody {
    response: Option<reqwest::Response>,
    pending: VecDeque<Bytes>,
}

impl FrontHost {
    fn observe(&self, event: &JValue, left: bool) {
        if let Ok(mut s) = self.saving.lock() {
            s.observe(event, left);
        }
    }

    fn state_file(&self, id: &str) -> PathBuf {
        self.user_dir.join(format!(
            "context-{}.json",
            context_fit::fingerprint(&js::s(id))
        ))
    }

    fn write_partial(&self, safe: &str, file: &JValue) {
        let path = server_projects::partial::file_for(&self.user_dir, safe);
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Err(e) = crate::gdrive::write_atomic(&path, &js::stringify(file)) {
            eprintln!("chat native: could not save the partial reply: {e}");
        }
    }

    /// A JSON call on core's loopback bridges, its body written as JS writes it.
    async fn core(&self, path: &str, body: &JValue, timeout: Duration) -> Option<(u16, JValue)> {
        let (status, bytes) = self
            .app
            .config
            .research
            .node_call_text(path, js::stringify(body), timeout)
            .await?;
        let v = if bytes.is_empty() {
            JValue::Null
        } else {
            wtf16::parse(&String::from_utf8_lossy(&bytes)).ok()?
        };
        Some((status, v))
    }

    fn core_later(&self, path: &'static str, body: JValue) {
        let app = Arc::clone(&self.app);
        tokio::spawn(async move {
            let _ = app
                .config
                .research
                .node_call_text(path, js::stringify(&body), BRIDGE_TIMEOUT)
                .await;
        });
    }
}

impl Host for FrontHost {
    type Body = EngineBody;

    fn emit(&self, event: &JValue) {
        let left = self.left();
        self.observe(event, left);
        if !left {
            let _ = self
                .tx
                .send(Bytes::from(format!("data: {}\n\n", js::stringify(event))));
        }
    }

    fn left(&self) -> bool {
        self.tx.is_closed()
    }

    fn now_ms(&self) -> f64 {
        // Milliseconds since 1970: exact in an f64 for the next quarter million years.
        #[allow(clippy::cast_precision_loss)]
        let now = crate::identity::now_ms() as f64;
        now
    }

    async fn lease(&self, model: &str) -> Result<Option<Vec<(String, String)>>, GateRefusal> {
        let admission = self.app.config.research.admission();
        let cancel = model_stream::Cancel::new();
        match admission
            .enter(engine_admission::Ask::Engine { model }, &cancel)
            .await
        {
            Ok(lease) => {
                let headers = lease.headers().to_vec();
                *self.lease.lock().await = Some(lease);
                Ok(Some(headers))
            }
            Err(r) => Err(GateRefusal {
                text: r.message,
                code: r.code,
                retryable: Some(r.status == 503 || r.status == 429),
            }),
        }
    }

    fn release_lease(&self) {
        let lease = self.lease.try_lock().ok().and_then(|mut l| l.take());
        if let Some(l) = lease {
            tokio::spawn(l.release());
        }
    }

    async fn health(&self) -> Option<JValue> {
        let (status, v) = self
            .core(
                "/internal/chat/native/health",
                &js::obj(Vec::<(String, JValue)>::new()),
                BRIDGE_TIMEOUT,
            )
            .await?;
        (status == 200 && v.get("ok") == &JValue::Bool(true)).then(|| v.get("body").clone())
    }

    async fn load(&self, model: &str) -> bool {
        let r = self
            .core(
                "/internal/chat/native/load",
                &js::obj([("model", js::s(model))]),
                Duration::from_secs(600),
            )
            .await;
        r.is_some_and(|(s, v)| s == 200 && v.get("ok") == &JValue::Bool(true))
    }

    async fn template_allows_tools(&self, model: &str) -> bool {
        // Unknown is yes, as the gate itself answers.
        let r = self
            .core(
                "/internal/chat/native/tools-allowed",
                &js::obj([("model", js::s(model))]),
                BRIDGE_TIMEOUT,
            )
            .await;
        r.is_none_or(|(s, v)| s != 200 || v.get("allowed") != &JValue::Bool(false))
    }

    async fn request(&self, request: &EngineRequest) -> Result<Answer<EngineBody>, String> {
        let mut r = self.client.post(&request.url).body(request.body.clone());
        for (k, v) in &request.headers {
            r = r.header(k, v);
        }
        if let Some(ms) = request.timeout_ms {
            r = r.timeout(Duration::from_millis(ms));
        }
        let send = r.send();
        let answer = tokio::select! {
            a = send => a.map_err(|e| e.without_url().to_string())?,
            () = closed(&self.tx) => return Err("This operation was aborted".into()),
        };
        let headers = answer
            .headers()
            .iter()
            .filter_map(|(k, v)| {
                v.to_str()
                    .ok()
                    .map(|v| (k.as_str().to_string(), v.to_string()))
            })
            .collect();
        Ok(Answer {
            status: answer.status().as_u16(),
            headers,
            body: EngineBody {
                response: Some(answer),
                pending: VecDeque::new(),
            },
        })
    }

    async fn read(&self, body: &mut EngineBody, idle_ms: Option<u64>) -> Read {
        if let Some(b) = body.pending.pop_front() {
            return Read::Chunk(b.to_vec());
        }
        let Some(response) = body.response.as_mut() else {
            return Read::End;
        };
        let next = response.chunk();
        let limit = async {
            match idle_ms {
                Some(ms) => tokio::time::sleep(Duration::from_millis(ms)).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            r = next => match r {
                Ok(Some(b)) => Read::Chunk(b.to_vec()),
                Ok(None) => Read::End,
                Err(e) => Read::Failed(e.without_url().to_string()),
            },
            () = limit => {
                // The round's own abort (chat.cjs roundAbort): the engine connection is let go.
                body.response = None;
                Read::Idle
            }
            () = closed(&self.tx) => Read::Aborted,
        }
    }

    async fn text(&self, body: EngineBody, cap: usize) -> (String, bool) {
        let mut out: Vec<u8> = body
            .pending
            .iter()
            .flat_map(|b| b.iter().copied())
            .collect();
        let Some(mut response) = body.response else {
            return (String::from_utf8_lossy(&out).into_owned(), false);
        };
        while out.len() <= cap {
            match response.chunk().await {
                Ok(Some(b)) => out.extend_from_slice(&b),
                _ => break,
            }
        }
        let capped = out.len() > cap;
        out.truncate(cap);
        (String::from_utf8_lossy(&out).into_owned(), capped)
    }

    fn read_state(&self, id: &str) -> JValue {
        std::fs::read_to_string(self.state_file(id))
            .ok()
            .and_then(|t| wtf16::parse(&t).ok())
            .unwrap_or_else(|| JValue::Obj(Vec::new()))
    }

    fn save_state(&self, id: &str, state: &JValue) -> Result<(), String> {
        if self.app.config.data_dir.is_none() || !self.user_dir.is_dir() {
            return Err("the account's workspace is gone".into());
        }
        crate::gdrive::write_atomic(&self.state_file(id), &js::stringify(state))
            .map_err(|e| e.to_string())
    }

    fn mark_tools_unsupported(&self, model: &str) {
        self.core_later(
            "/internal/chat/native/tools-unsupported",
            js::obj([("model", js::s(model))]),
        );
    }

    async fn engine_busy(&self) -> bool {
        let r = self
            .core(
                "/internal/chat/native/busy",
                &js::obj(Vec::<(String, JValue)>::new()),
                BRIDGE_TIMEOUT,
            )
            .await;
        r.is_some_and(|(s, v)| s == 200 && v.get("busy") == &JValue::Bool(true))
    }

    async fn title_auto_named(&self, list: Option<&str>, chat: &str, first_message: &str) -> bool {
        let body = js::obj([
            ("user", js::s(self.user.clone())),
            ("listId", list.map_or(JValue::Null, js::s)),
            ("chatId", js::s(chat)),
            ("firstMessage", js::s(first_message)),
        ]);
        let r = self
            .core(
                "/internal/chat/native/title-auto-named",
                &body,
                BRIDGE_TIMEOUT,
            )
            .await;
        r.is_some_and(|(s, v)| s == 200 && v.get("autoNamed") == &JValue::Bool(true))
    }

    fn title_save(&self, list: Option<&str>, chat: &str, title: &str, first_message: &str) {
        self.core_later(
            "/internal/chat/native/title-save",
            js::obj([
                ("user", js::s(self.user.clone())),
                ("listId", list.map_or(JValue::Null, js::s)),
                ("chatId", js::s(chat)),
                ("title", js::s(title)),
                ("firstMessage", js::s(first_message)),
            ]),
        );
    }

    fn record_usage(&self, model: &str, reported: &JValue) {
        self.core_later(
            "/internal/chat/usage",
            js::obj([
                ("user", js::s(self.user.clone())),
                ("model", js::s(model)),
                ("reported", reported.clone()),
            ]),
        );
    }

    fn prefill_sample(&self, model: &str, prompt: f64, first_token_ms: f64) {
        self.core_later(
            "/internal/chat/prefill",
            js::obj([
                ("model", js::s(model)),
                ("promptTokens", js::n(prompt)),
                ("firstTokenMs", js::n(first_token_ms)),
            ]),
        );
    }

    fn mtp_evidence(&self, _model: &str, _drafted: f64, _accepted: f64) {
        // Draft acceptance evidence stays with the model engine's own observations (named difference).
    }

    fn user_id(&self) -> String {
        self.user.clone()
    }

    fn uuid(&self) -> String {
        uuid_v4()
    }

    fn tool_policy(&self, name: &str, write: bool) -> String {
        crate::connector_routes::mode_of(&self.app, &self.user, name, write).to_string()
    }

    async fn write_target(&self, name: &str, args: &str, chat_key: Option<&str>) -> Target {
        let body = js::obj([
            ("user", js::s(self.user.clone())),
            ("name", js::s(name)),
            ("args", js::s(args)),
            ("chatKey", chat_key.map_or(JValue::Null, js::s)),
        ]);
        match self
            .core("/internal/chat/tools/target", &body, BRIDGE_TIMEOUT)
            .await
        {
            Some((200, v)) => {
                if let Some(e) = v.get("error").as_str() {
                    Target::Refused(e.to_string())
                } else if let Some(t) = v.get("target").as_str() {
                    Target::Found {
                        target: t.to_string(),
                        kind: v.get("kind").as_str().map(str::to_string),
                    }
                } else {
                    Target::None
                }
            }
            // chat.cjs: a lookup that fails refuses the write before its card.
            _ => Target::Refused("The file this would change could not be looked up".into()),
        }
    }

    async fn permission_binding(
        &self,
        chat_id: Option<&str>,
        scope: &str,
        holder: &JValue,
    ) -> Result<JValue, ()> {
        let Some(chat) = chat_id else {
            return Ok(JValue::Null);
        };
        let approvals = Arc::clone(&self.approvals);
        let (user, chat, scope, holder) = (
            self.user.clone(),
            chat.to_string(),
            scope.to_string(),
            holder.clone(),
        );
        tokio::task::spawn_blocking(move || approvals.binding(&user, &chat, &scope, &holder))
            .await
            .map_err(|_| ())?
    }

    fn chat_wide_approved(&self, chat_id: Option<&str>, scope: &str, binding: &JValue) -> bool {
        self.approvals
            .chat_wide(&self.user, chat_id, scope, binding)
    }

    async fn await_approval(&self, request: &JValue) -> String {
        let ask = self.approvals.ask(&self.user, request);
        tokio::select! {
            d = ask => d,
            () = closed(&self.tx) => "aborted".into(),
        }
    }

    fn revoke_grant(&self, chat_id: Option<&str>) {
        self.approvals.revoke(&self.user, chat_id);
    }

    fn audit(&self, event: &str, details: &JValue) {
        let app = Arc::clone(&self.app);
        let (user, event, detail) = (self.user.clone(), event.to_string(), js::stringify(details));
        tokio::task::spawn_blocking(move || {
            let at = crate::identity::now_ms();
            let written = app.writes.writer().and_then(|w| {
                w.write(|tx| {
                    tx.execute(
                        "INSERT INTO audit_events(actor_user_id,target_user_id,action,detail,created_at) VALUES(?1,?2,?3,?4,?5)",
                        server_store::rusqlite::params![user, user, event, detail, at],
                    )?;
                    Ok(())
                })
            });
            if let Err(e) = written {
                eprintln!("chat native: audit not written: {e}");
            }
        });
    }

    async fn execute_tool(&self, run: &ToolRun) -> Result<(String, bool), String> {
        let body = js::obj([
            ("user", js::s(self.user.clone())),
            (
                "projectId",
                run.project_id.clone().map_or(JValue::Null, js::s),
            ),
            ("name", js::s(run.name.clone())),
            ("args", js::s(run.args.clone())),
            (
                "allowed",
                JValue::Arr(run.allowed.iter().cloned().map(js::s).collect()),
            ),
            (
                "routes",
                JValue::Arr(
                    run.routes
                        .iter()
                        .map(|(a, b)| JValue::Arr(vec![js::s(a.clone()), js::s(b.clone())]))
                        .collect(),
                ),
            ),
            ("chatKey", run.chat_key.clone().map_or(JValue::Null, js::s)),
            ("exchangeKey", js::s(run.exchange_key.clone())),
        ]);
        match self
            .core("/internal/chat/tools/execute", &body, TOOL_TIMEOUT)
            .await
        {
            Some((200, v)) => Ok((
                v.get("text").as_str().unwrap_or("").to_string(),
                v.get("failed") == &JValue::Bool(true),
            )),
            Some((_, v)) => Err(v
                .get("error")
                .as_str()
                .unwrap_or("the tool bridge failed")
                .to_string()),
            None => Err("the tool bridge did not answer".into()),
        }
    }
}

impl Keep for FrontHost {
    fn append_if_last(&self, chat: &str, message: &str, entry: &JValue) -> Result<String, String> {
        use server_projects::transcript::{append_if_last_as, Appended};
        match append_if_last_as(
            &self.data_dir,
            &self.user,
            chat,
            message,
            entry,
            server_projects::journal::Origin::Turn,
        ) {
            Ok(Appended::Saved { .. }) => Ok("saved".into()),
            Ok(Appended::NotLast) => Ok("not-last".into()),
            Ok(Appended::NoUserMessage) => Ok("no-user-message".into()),
            Ok(Appended::Deleted) => Ok("deleted".into()),
            Err(e) => Err(format!("{e:?}")),
        }
    }

    fn keep_partial(&self, chat: &str, file: &JValue) {
        self.write_partial(chat, file);
    }

    fn end_partial(&self, chat: &str) {
        let path = server_projects::partial::file_for(&self.user_dir, chat);
        if std::fs::remove_file(&path).is_err() && path.exists() {
            eprintln!("chat native: could not remove a finished reply's partial");
        }
    }

    fn tombstoned(&self, chat: &str) -> bool {
        server_projects::transcript::tombstoned(&self.user_dir, chat).unwrap_or(false)
    }

    fn delete_unlisted(&self, chat: &str) {
        let _ = server_projects::chat_delete::delete(
            &self.data_dir,
            &self.user,
            chat,
            server_projects::chat_delete::Kind::Unlisted,
            server_projects::journal::Origin::Turn,
            server_store::RustProjects::owned(),
        );
    }
}

/// Resolves once the client has gone (the response body dropped).
async fn closed(tx: &mpsc::UnboundedSender<Bytes>) {
    tx.closed().await;
}

fn uuid_v4() -> String {
    use rand_core::RngCore;
    let mut b = [0u8; 16];
    rand_core::OsRng.fill_bytes(&mut b);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn the_switch_reads_node_native_and_accounts() {
        assert_eq!(Switch::parse(None), Ok(Switch::Node));
        assert_eq!(Switch::parse(Some(" node ")), Ok(Switch::Node));
        assert_eq!(Switch::parse(Some("native")), Ok(Switch::All));
        let some = Switch::parse(Some("native:qa-user-1, qa-user-2")).unwrap();
        assert!(some.names("qa-user-2") && !some.names("qa-user-3"));
        assert!(Switch::parse(Some("native:")).is_err());
        assert!(Switch::parse(Some("rust")).is_err());
        assert!(Switch::parse(Some("native:a b")).is_err());
        assert!(!Switch::Node.names("x"));
    }

    #[test]
    fn uuids_are_version_four() {
        let u = uuid_v4();
        assert_eq!(u.len(), 36);
        assert_eq!(&u[14..15], "4");
        assert!(matches!(&u[19..20], "8" | "9" | "a" | "b"));
    }
}
