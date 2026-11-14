//! The native chat executor (track R8): `POST /api/chat` for the accounts the executor switch names,
//! the turn run by `crates/chat-turn` after core prepared it.
//!
//! [`serve`] relays the turn to core's `/internal/chat/turn` as chat_turn.rs does, with one more header
//! (`x-noevia-chat-handoff`, the per-start secret; any copy the client sent is removed). Core answers a
//! relayed turn as NDJSON (chat port S7, core rust-chat-wire.cjs), runs its preparation as always and,
//! for a turn the native executor can run, ends it with one `prepared` line (rust-chat-native.cjs); any
//! other turn core runs to the end on the same stream. The answer [`serve`] gives chat_turn.rs is that
//! NDJSON body with core's own headers: core's lines as core wrote them until `prepared`, then the lines
//! [`chat_turn::native::run`] writes with [`FrontHost`] (`JSON.stringify(event) + "\n"`, as core's wire
//! writes each event). So chat_wire.rs writes the client's event stream (the headers, the heartbeat, the
//! frames, the checks, the failure frame) and chat_admission.rs holds the gate's lease (S8) for both
//! executors alike; when either drops the body (the client left, or the lease was lost), the native turn
//! sees the client gone. [`chat_turn::after::Saving`] keeps what the turn leaves.
//!
//! Shadow (P7a): `NOEVIA_CHAT_PREPARE` (chat_prepare_shadow.rs) also prepares a handed-off turn natively and
//! logs whether the messages are core's; the turn still runs from core's frame.
//!
//! Switch: `NOEVIA_CHAT_EXECUTOR` (config.rs): unset or `node` is today's relay; `native:<user id>,...`
//! names accounts; `native` is everyone. The turn's cards use this front's approval registry (chat port S9,
//! [`approvals`]); its chat-wide grants are core's to check, redeem and revoke (the tool bridges, D17).
//! The Diary space itself (`spaceId: 'diary'`, the companion's own exchange) and a Cowork start are never handed off
//! (chat_turn.rs only calls here for a turn); Diary extras and Diary project turns are (track R16).
//!
//! What the turn needs of this process, and where it comes from:
//! - the client's stream: NDJSON lines into the response body ([`Wire`]), which chat_wire.rs frames (its
//!   5 s keep-alive included); the 4 s partial saver (chat-partial.cjs); the turn record fed with every
//!   event sent;
//! - the engine: the prepared `upstreamUrl`, with the model's admission lease (engine-admission
//!   `Ask::Engine`) on every request;
//! - core, through its loopback bridges (rust-chat-native.cjs): tool runs and Drive write targets, the
//!   model manager's health and load, the template tools gate, the engine-busy signal, the chat title
//!   store, usage and prefill samples;
//! - this process: the account's state files and partial (its workspace directory), the tool policy,
//!   the audit log, the transcript append and the tombstones, and the approval registry (S9).

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::task::{Context, Poll};
use std::time::Duration;

use axum::body::Body;
use axum::http::{header, HeaderValue, Response};
use bytes::Bytes;
use chat_turn::after::{Keep, Saving};
use chat_turn::handoff::{Frames, Piece};
use chat_turn::host::{
    Answer, EditTarget, EngineRequest, GateRefusal, Host, Ran, Read, Scrub, Skill, Target, ToolRun,
};
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
/// The longest line before the hand-off the turn record reads (an event line past it still passes on).
const OBSERVE_CAP: usize = 1024 * 1024;
/// What the turn record reads of core's lines before the hand-off at most: the preparation sends a handful
/// of events (meta, sources, telemetry, status, warning, tools_scope, route_pending). Past this (a turn core
/// runs itself), nothing more is read; every byte still passes on.
const OBSERVE_TOTAL: usize = 4 * 1024 * 1024;
const OBSERVE_EVENTS: usize = 256;
/// How many of core's bytes may wait in the body before reading core waits for the client (backpressure, as
/// the relay of a Node turn has it through its socket).
const FORWARD_QUEUE: usize = 1024 * 1024;
const BRIDGE_TIMEOUT: Duration = Duration::from_secs(30);
/// Core's step supervisor answers within its own deadline (step-supervision.cjs: at most 2 s); later is `continue`.
const SUPERVISE_TIMEOUT: Duration = Duration::from_secs(10);
/// Core's framing reasoner answers within its own deadline (framing-reasoner.cjs: the decision timeout, 6 s by
/// default) after its budget check; later is a skip (the raw result goes on, as on any skip).
const CONDENSE_TIMEOUT: Duration = Duration::from_secs(60);
/// Core's grant check for a native turn's write (rust-chat-native.cjs GRANT_PATH; D17).
const GRANT_PATH: &str = "/internal/chat/tools/grant";
/// Core's revoke of the chat's grant (rust-chat-native.cjs REVOKE_PATH; Node's revokeChatGrant).
const REVOKE_PATH: &str = "/internal/chat/tools/revoke";
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

/// What a native turn asks of the chat approval registry (chat port S9's `approval_registry`) for its cards,
/// and the chat's permission binding. The grant check, its redeem and its revoke are core's (D17: Node's
/// `chatWideApproved`, `redeemChatGrant` and `revokeChatGrant` behind the tool bridges), so that Node's
/// revocation epochs and dispatch fence stay the one authority for a write under "Allow for this chat".
pub trait Approvals: Send + Sync {
    /// The chat's permission binding for `scope` (chat.cjs permissionContext), or `None` when it cannot
    /// be verified (store failing, chat moved, deleted or held elsewhere). Blocking (the store's file lock).
    fn binding(&self, user: &str, chat: &str, scope: &str, holder: &JValue) -> Option<JValue>;
    /// Opens the card's question (S9 `openApproval`) before the card is shown: false when it could not be.
    fn open(&self, user: &str, request: &JValue) -> bool;
    /// Waits for the opened question's answer: "approve", "deny", "timeout" or "unavailable". Dropping the
    /// future (the client left) withdraws the question, as Node's dropped long-poll does (#679).
    fn wait(
        &self,
        user: &str,
        request: &JValue,
    ) -> Pin<Box<dyn std::future::Future<Output = String> + Send>>;
    /// Withdraws an opened question the turn will not wait for any more (S9 `cancel`: the turn stopped).
    fn cancel(&self, user: &str, id: &str);
}

/// The front's own registry (`App::approvals`) and grant contexts (server-projects chat_move).
struct Registry {
    app: Arc<App>,
}

impl Approvals for Registry {
    fn binding(&self, user: &str, chat: &str, scope: &str, holder: &JValue) -> Option<JValue> {
        use server_projects::chat_move::Holder;
        let start = self.app.config.start_id.as_deref()?;
        let data = self.app.config.data_dir.as_deref()?;
        let canonical = crate::approval_registry::safe_id(chat);
        if canonical.is_empty() || user.is_empty() || scope.is_empty() {
            return None;
        }
        let context = server_projects::chat_move::grant_context(data, user, chat, start).ok()?;
        // chat-grant-context.cjs validate, then permissionContext: a tombstoned chat, or one another holder
        // than the turn's now holds, binds nothing.
        let held_elsewhere = match &context.holder {
            Holder::Absent => false,
            Holder::Free => holder.get("kind").as_str() != Some("free"),
            Holder::Project(id) => {
                holder.get("kind").as_str() != Some("project")
                    || holder.get("id").as_str() != Some(id.as_str())
            }
        };
        if context.tombstoned
            || held_elsewhere
            || context.canonical != canonical
            || context.start != start
        {
            return None;
        }
        let held = match &context.holder {
            Holder::Free => js::obj([("kind", js::s("free"))]),
            Holder::Absent => js::obj([("kind", js::s("absent"))]),
            Holder::Project(id) => js::obj([("kind", js::s("project")), ("id", js::s(id.clone()))]),
        };
        // chat-grant-context.cjs binding(): this key order.
        Some(js::obj([
            ("user", js::s(user)),
            ("canonicalChatId", js::s(canonical.clone())),
            ("scope", js::s(scope)),
            (
                "context",
                js::obj([
                    ("canonicalChatId", js::s(canonical)),
                    ("holder", held),
                    ("generation", js::s(context.generation)),
                    ("start", js::s(context.start)),
                    ("tombstoned", JValue::Bool(false)),
                ]),
            ),
        ]))
    }

    fn open(&self, user: &str, request: &JValue) -> bool {
        question(user, request).is_some_and(|q| self.app.approvals.open(q).is_ok())
    }

    fn wait(
        &self,
        user: &str,
        request: &JValue,
    ) -> Pin<Box<dyn std::future::Future<Output = String> + Send>> {
        use crate::approval_registry::Outcome;
        let app = Arc::clone(&self.app);
        let user = user.to_string();
        let id = request.get("id").as_str().unwrap_or("").to_string();
        Box::pin(async move {
            match app.approvals.wait(&user, &id).await {
                Ok(Outcome::Approve) => "approve".into(),
                Ok(Outcome::Deny) => "deny".into(),
                Ok(Outcome::Timeout) => "timeout".into(),
                // A routing answer to a write question, or no such question: not an approval.
                Ok(_) | Err(_) => "unavailable".into(),
            }
        })
    }

    fn cancel(&self, user: &str, id: &str) {
        self.app.approvals.cancel(user, id);
    }
}

/// The registry question for the gate's card request (`{id, userId, chatId, scope, binding, once}`).
fn question(user: &str, request: &JValue) -> Option<crate::approval_registry::Question> {
    use crate::approval_registry::{Kind, Question};
    let text = |k: &str| request.get(k).as_str().map(str::to_string);
    let binding = match request.get("binding") {
        JValue::Obj(_) => serde_json::from_str(&js::stringify(request.get("binding"))).ok(),
        _ => None,
    };
    Some(Question {
        id: text("id")?,
        user: user.to_string(),
        kind: Kind::Write,
        chat: text("chatId"),
        scope: text("scope").filter(|s| !s.is_empty()),
        binding,
        // #2446: approve_all on a once card approves that call only.
        once: request.get("once") == &JValue::Bool(true),
    })
}

/// Tests only: a process-wide registry stand-in that [`approvals`] returns instead of the front's own.
static INSTALLED: std::sync::RwLock<Option<Arc<dyn Approvals>>> = std::sync::RwLock::new(None);

/// Test-only seam: stores (or with `None` removes) a process-wide [`Approvals`], which [`approvals`] then
/// returns instead of the front's registry, so an end-to-end test can answer recorded cards (a timeout
/// included) at once. Nothing in the binary calls this.
#[doc(hidden)]
pub fn install_approvals(a: Option<Arc<dyn Approvals>>) {
    if let Ok(mut installed) = INSTALLED.write() {
        *installed = a;
    }
}

/// The approvals a native turn uses: the front's registry (chat port S9 is on main), or a test's stand-in.
pub(crate) fn approvals(app: &Arc<App>) -> Arc<dyn Approvals> {
    let installed = INSTALLED.read().ok().and_then(|a| a.clone());
    installed.unwrap_or_else(|| {
        Arc::new(Registry {
            app: Arc::clone(app),
        })
    })
}

/// Whether this turn goes native: the switch names the account and this front can run it.
pub(crate) fn wanted(app: &Arc<App>, user: &str) -> bool {
    app.config.chat_executor.names(user)
}

/// The bytes queued in a [`Wire`] and not yet taken by chat_wire.rs.
#[derive(Default)]
struct Queued {
    bytes: AtomicUsize,
    drained: tokio::sync::Notify,
}

/// The NDJSON body chat_wire.rs reads: core's bytes, then the native turn's lines, ended when the sender goes.
struct Wire(mpsc::UnboundedReceiver<Bytes>, Arc<Queued>);

impl http_body::Body for Wire {
    type Data = Bytes;
    type Error = std::convert::Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        let polled = self.0.poll_recv(cx);
        if let Poll::Ready(Some(b)) = &polled {
            self.1.bytes.fetch_sub(b.len(), Ordering::SeqCst);
            self.1.drained.notify_waiters();
        }
        polled.map(|b| b.map(|b| Ok(Frame::data(b))))
    }
}

/// Queues `b` for the client: false when the client has gone.
fn push(tx: &mpsc::UnboundedSender<Bytes>, queued: &Queued, b: Bytes) -> bool {
    let n = b.len();
    queued.bytes.fetch_add(n, Ordering::SeqCst);
    if tx.send(b).is_err() {
        queued.bytes.fetch_sub(n, Ordering::SeqCst);
        return false;
    }
    true
}

/// Waits while more than [`FORWARD_QUEUE`] bytes wait for the client; false when the client has gone.
async fn room(tx: &mpsc::UnboundedSender<Bytes>, queued: &Queued) -> bool {
    loop {
        let drained = queued.drained.notified();
        if tx.is_closed() {
            return false;
        }
        if queued.bytes.load(Ordering::SeqCst) <= FORWARD_QUEUE {
            return true;
        }
        tokio::select! {
            () = drained => {}
            () = tx.closed() => return false,
        }
    }
}

/// Relays the turn with the hand-off asked for, and runs it natively from the `prepared` frame.
/// `extra` is what chat_turn.rs sends core (token, user, the gate's lease); `relay` sends it.
/// `shadow`: the request body, when the turn is also prepared natively in shadow (chat_prepare_shadow.rs).
pub(crate) async fn serve<F, Fut>(
    app: &Arc<App>,
    user: String,
    shadow: Option<Bytes>,
    relay: F,
) -> Option<Response<Body>>
where
    F: FnOnce(HeaderValue) -> Fut,
    Fut: std::future::Future<Output = Option<Response<Body>>>,
{
    let mut token = HeaderValue::from_str(app.config.research.front_token()?).ok()?;
    token.set_sensitive(true);
    let approvals = approvals(app);
    let res = relay(token).await?;
    // A JSON refusal, or a core that is not this front's pair (chat_turn.rs answers those).
    if !crate::chat_wire::is_ndjson(res.headers()) {
        return Some(res);
    }
    let (mut parts, core) = res.into_parts();
    // The body is no longer core's own: its length (if core named one) is not this body's.
    parts.headers.remove(header::CONTENT_LENGTH);
    let (tx, rx) = mpsc::unbounded_channel();
    let queued = Arc::new(Queued::default());
    tokio::spawn(drive(
        Arc::clone(app),
        user,
        core,
        tx,
        Arc::clone(&queued),
        approvals,
        shadow,
    ));
    Some(Response::from_parts(parts, Body::new(Wire(rx, queued))))
}

/// Strips a client's copy of the hand-off header (only the front may ask for one).
pub(crate) fn strip_client_header(headers: &mut axum::http::HeaderMap) {
    headers.remove(HANDOFF_HEADER);
    headers.remove(chat_turn::handoff::HANDOFF_CAPS_HEADER);
}

/// Adds the hand-off header to what chat_turn.rs sends core.
/// The headers that ask core for the hand-off: the secret, and what this executor runs (core keeps a turn
/// needing anything it does not name, such as instruction skills, in Node).
pub(crate) fn handoff_headers(token: HeaderValue) -> [(&'static str, HeaderValue); 2] {
    [
        (HANDOFF_HEADER, token),
        (
            chat_turn::handoff::HANDOFF_CAPS_HEADER,
            HeaderValue::from_static(chat_turn::handoff::HANDOFF_CAPS),
        ),
    ]
}

async fn drive(
    app: Arc<App>,
    user: String,
    core: Body,
    tx: mpsc::UnboundedSender<Bytes>,
    queued: Arc<Queued>,
    approvals: Arc<dyn Approvals>,
    shadow: Option<Bytes>,
) {
    use http_body_util::BodyExt;
    let mut frames = Frames::new();
    let mut core = core;
    let mut lines = Lines::default();
    let prepared = loop {
        // The client gone (or the gate's lease lost: chat_admission.rs drops the body) while core is
        // quiet: dropping core's stream here ends its turn, as the relay of a Node turn does (#679).
        let next = tokio::select! {
            f = core.frame() => f,
            () = tx.closed() => {
                journal_after_left(&app, &user, &mut frames, &mut core).await;
                return;
            }
        };
        let Some(frame) = next else {
            break None;
        };
        let Ok(frame) = frame else { break None };
        let Ok(data) = frame.into_data() else {
            continue;
        };
        let mut handed = None;
        let mut pieces = frames.feed(&data).into_iter();
        while let Some(piece) = pieces.next() {
            match piece {
                Piece::Forward(bytes) => {
                    lines.feed(&bytes);
                    if !push(&tx, &queued, Bytes::from(bytes)) || !room(&tx, &queued).await {
                        // The client left: closing core's stream ends its turn, which keeps it (#679). A frame
                        // already in this chunk handed a Diary journal over: it is finished here (track R16).
                        for rest in pieces.by_ref() {
                            if let Piece::Prepared(p) = rest {
                                Journal::finish_with(&app, &user, journal_handle(&p), Vec::new());
                            }
                        }
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
        // Core ran the turn itself (or its stream broke): what it held back goes on, chat_wire.rs judges it.
        let rest = frames.rest();
        if !rest.is_empty() {
            let _ = push(&tx, &queued, Bytes::from(rest));
        }
        return;
    }
    let forwarded = lines.events;
    drop(core);
    let failed = |tx: &mpsc::UnboundedSender<Bytes>| {
        let e = js::obj([
            ("type", js::s("error")),
            ("text", js::s(native::FAILED_TEXT)),
        ]);
        let _ = tx.send(Bytes::from(format!("{}\n", js::stringify(&e))));
    };
    let Some(frame) = prepared else {
        eprintln!("chat native: core's prepared frame does not parse");
        return failed(&tx);
    };
    let p = match Prepared::read(&frame) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("chat native: {}", e.0);
            Journal::fail(&app, &user, journal_handle(&frame));
            return failed(&tx);
        }
    };
    let Some(data_dir) = app.config.data_dir.clone() else {
        Journal::fail(&app, &user, p.diary_journal.clone());
        return failed(&tx);
    };
    let Ok(user_dir) = server_projects::store::user_dir(&data_dir, &user) else {
        Journal::fail(&app, &user, p.diary_journal.clone());
        return failed(&tx);
    };
    // Track R8 P7a: the same turn prepared natively beside core's frame, compared and logged only.
    if let Some(body) = shadow {
        crate::chat_prepare_shadow::spawn(Arc::clone(&app), user_dir.clone(), body, frame);
    }
    let host = Arc::new(FrontHost {
        app: Arc::clone(&app),
        user,
        user_dir,
        data_dir,
        tx,
        queued,
        saving: Mutex::new(Saving::new(TurnRecord::new(crate::identity::now_ms))),
        partial: PartialGate::default(),
        journal: OnceLock::new(),
        lease: Mutex::new(None),
        lease_token: Mutex::new(None),
        approvals,
        client: engine_client(),
    });
    for v in &forwarded {
        host.observe(v, false);
    }
    let chat = p.record.then(|| p.chat_id.clone()).flatten();
    if let Some(c) = &chat {
        // What core's saver left at the cut; the 4 s saver below goes on (noevia#2750: this once ended it).
        clear_partial(&host.user_dir, &chat_turn::after::safe_chat_id(c));
    }
    // Track R16: a Diary extras preparation's recovery journal, fed every event from here on.
    if let Some(handle) = &p.diary_journal {
        let journal = Journal::start(Arc::clone(&host.app), host.user.clone(), handle.clone());
        let _ = host.journal.set(journal);
    }
    let ticks = tokio::spawn(ticks(Arc::clone(&host), chat.clone()));
    // Node's `close` handler gives the lease back the moment the client leaves, before the turn's
    // own end (the audit, the tool-use count and the save come after it).
    let watcher = {
        let host = Arc::clone(&host);
        tokio::spawn(async move {
            closed(&host.tx).await;
            host.release_lease();
        })
    };
    let process = process();
    let helpers = helpers();
    let ended = native::run(&*host, process, &helpers, &p).await;
    ticks.abort();
    // A partial write in progress finishes before the turn's end is saved (review F1).
    let _ = ticks.await;
    watcher.abort();
    // handleChatTurn's finally finishes the Diary journal first (track R16).
    if let Some(journal) = host.journal.get() {
        journal.finish().await;
    }
    let left = matches!(ended, native::Ended::Left) || host.left();
    let keep = Arc::clone(&host);
    let message = p.message.clone();
    // handleChatTurn's finally reads `body.chatId` (the title's chatId: the request's own id when it is
    // text), not chat.cjs's chatId, which a new project chat gets made up for it.
    let turn_chat = Some(p.title.chat_id.clone()).filter(|c| !c.is_empty());
    let record = p.record;
    let _ = tokio::task::spawn_blocking(move || {
        if let Ok(saving) = keep.saving.lock() {
            saving.finish(
                &*keep,
                turn_chat.as_deref(),
                record,
                &message,
                left,
                crate::identity::now_ms(),
            );
        }
    })
    .await;
}

/// The partial saver (4 s) while the native part runs (chat_wire.rs writes the keep-alive).
async fn ticks(host: Arc<FrontHost>, chat: Option<String>) {
    let Some(c) = chat else { return };
    let mut save = tokio::time::interval(Duration::from_secs(4));
    save.tick().await;
    loop {
        save.tick().await;
        if host.left() {
            return;
        }
        let entry = host
            .saving
            .lock()
            .ok()
            .and_then(|s| s.record.partial_entry());
        if let Some(entry) = entry {
            let mut file = chat_turn::record::partial_file(&entry, crate::identity::now_ms());
            if let Some(start) = host.app.config.start_id.clone() {
                js::set(&mut file, "running", js::obj([("start", js::s(start))]));
            }
            let (h, safe) = (Arc::clone(&host), chat_turn::after::safe_chat_id(&c));
            let _ = tokio::task::spawn_blocking(move || h.tick_partial(&safe, &file)).await;
        }
    }
}

/// Core's lines before the hand-off, read for the turn record (object lines only; a line longer than
/// [`OBSERVE_CAP`] is not read, it still passes on).
#[derive(Default)]
struct Lines {
    held: Vec<u8>,
    skipping: bool,
    read: usize,
    full: bool,
    events: Vec<JValue>,
}

impl Lines {
    fn feed(&mut self, bytes: &[u8]) {
        if self.full {
            return;
        }
        self.read += bytes.len();
        if self.read > OBSERVE_TOTAL || self.events.len() >= OBSERVE_EVENTS {
            // Core is running the turn itself: its lines are not the hand-off's.
            self.full = true;
            self.held = Vec::new();
            return;
        }
        let mut rest = bytes;
        while !rest.is_empty() {
            let end = rest.iter().position(|b| *b == b'\n');
            let take = end.map_or(rest.len(), |at| at + 1);
            let (part, after) = rest.split_at(take);
            rest = after;
            if !self.skipping {
                self.held.extend_from_slice(part);
                if self.held.len() > OBSERVE_CAP {
                    self.held = Vec::new();
                    self.skipping = true;
                }
            }
            if end.is_some() {
                if !self.skipping {
                    let line = std::mem::take(&mut self.held);
                    let text = line.strip_suffix(b"\n").unwrap_or(&line);
                    if let Some(v) = std::str::from_utf8(text)
                        .ok()
                        .and_then(|t| wtf16::parse(t).ok())
                        .filter(|v| matches!(v, JValue::Obj(_)))
                    {
                        self.events.push(v);
                    }
                }
                self.skipping = false;
            }
        }
    }
}

/// One client for every native turn's engine calls; like Node's fetch it takes no proxy from the environment.
fn engine_client() -> reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .no_proxy()
                .build()
                .unwrap_or_else(|_| reqwest::Client::new())
        })
        .clone()
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

/// The 4 s partial saver's gate (review F1): its ticks write until the turn's end has kept or cleared the
/// partial, never after. Only the end closes it; the start of the turn only clears what core left (noevia#2750).
#[derive(Default)]
struct PartialGate(Mutex<bool>);

impl PartialGate {
    /// Runs `write` unless the turn's end came first (under the gate's lock, so the end cannot come between).
    fn tick(&self, write: impl FnOnce()) {
        let Ok(ended) = self.0.lock() else {
            return;
        };
        if !*ended {
            write();
        }
    }

    /// The turn's end: `then` runs under the lock, and no tick writes after it.
    fn end(&self, then: impl FnOnce()) {
        let mut ended = self.0.lock().unwrap_or_else(|e| e.into_inner());
        *ended = true;
        then();
    }
}

/// core's diary-journal op (rust-chat-native.cjs, track R16).
const JOURNAL_PATH: &str = "/internal/chat/native/diary-journal";
/// The most events one journal call carries (core takes up to 10000; a tool result is at most 8000 units).
const JOURNAL_BATCH: usize = 256;
/// How long the turn's end waits for the journal's last call.
const JOURNAL_FINISH: Duration = Duration::from_secs(3);
/// How long a client gone before the hand-off leaves core's stream open for a frame already on the way.
const JOURNAL_LEFT: Duration = Duration::from_millis(100);

enum JournalItem {
    Event(JValue),
    Finish,
}

/// Track R16: a Diary extras preparation's recovery journal (core diary-jobs.cjs), held by core under the frame's
/// handle (rust-chat-journal.cjs). Every event the turn sends goes to it in order, as chat.cjs `send` feeds it, through
/// core's diary-journal op: one call at a time, each with what came while the last one ran. The turn's end finishes
/// it. A call that fails stops the feeding, and the finish is still sent (without the events since): a record whose
/// events were lost reads `uncertain` (diary-jobs.cjs finish).
struct Journal {
    tx: mpsc::UnboundedSender<JournalItem>,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Journal {
    fn start(app: Arc<App>, user: String, handle: String) -> Journal {
        let (tx, mut rx) = mpsc::unbounded_channel::<JournalItem>();
        let task = tokio::spawn(async move {
            let mut live = true;
            while let Some(first) = rx.recv().await {
                let mut events = Vec::new();
                let mut finish = false;
                let mut next = Some(first);
                while let Some(item) = next.take() {
                    match item {
                        JournalItem::Event(e) => events.push(e),
                        JournalItem::Finish => finish = true,
                    }
                    if !finish && events.len() < JOURNAL_BATCH {
                        next = rx.try_recv().ok();
                    }
                }
                if !live && finish {
                    events.clear();
                }
                if live || finish {
                    let body = js::obj([
                        ("user", js::s(user.clone())),
                        ("handle", js::s(handle.clone())),
                        ("events", JValue::Arr(events)),
                        ("finish", JValue::Bool(finish)),
                    ]);
                    let answer = app
                        .config
                        .research
                        .node_call_text(JOURNAL_PATH, js::stringify(&body), BRIDGE_TIMEOUT)
                        .await;
                    if !matches!(answer, Some((204, _))) {
                        eprintln!(
                            "chat native: the Diary journal stopped (status {:?})",
                            answer.map(|(s, _)| s)
                        );
                        live = false;
                    }
                }
                if finish {
                    break;
                }
            }
        });
        Journal {
            tx,
            task: Mutex::new(Some(task)),
        }
    }

    fn event(&self, event: &JValue) {
        let _ = self.tx.send(JournalItem::Event(event.clone()));
    }

    /// A turn that ends before it runs (the frame unreadable, no account folder): the client got the failure text,
    /// so the journal gets it too and is finished, as chat.cjs would journal its own error.
    fn fail(app: &Arc<App>, user: &str, handle: Option<String>) {
        let error = js::obj([
            ("type", js::s("error")),
            ("text", js::s(native::FAILED_TEXT)),
        ]);
        Journal::finish_with(app, user, handle, vec![error]);
    }

    /// One call: `events`, then the journal's finish (a turn that never ran here). Its answer is not waited for.
    fn finish_with(app: &Arc<App>, user: &str, handle: Option<String>, events: Vec<JValue>) {
        let Some(handle) = handle else { return };
        let body = js::obj([
            ("user", js::s(user)),
            ("handle", js::s(handle)),
            ("events", JValue::Arr(events)),
            ("finish", JValue::Bool(true)),
        ]);
        let app = Arc::clone(app);
        tokio::spawn(async move {
            let _ = app
                .config
                .research
                .node_call_text(JOURNAL_PATH, js::stringify(&body), BRIDGE_TIMEOUT)
                .await;
        });
    }

    /// The turn's end: what is left, then the journal's finish (waited for, at most [`JOURNAL_FINISH`]).
    async fn finish(&self) {
        let _ = self.tx.send(JournalItem::Finish);
        let task = self.task.lock().ok().and_then(|mut t| t.take());
        if let Some(task) = task {
            let _ = tokio::time::timeout(JOURNAL_FINISH, task).await;
        }
    }
}

/// The client left while core was still preparing (#679: dropping core's stream ends its turn). Core may have
/// handed the turn off just then, a Diary journal with it (its frame already on the way): what core sent within a
/// moment is read for that frame, and its journal is finished, as Node's turn would end it (track R16).
async fn journal_after_left(app: &Arc<App>, user: &str, frames: &mut Frames, core: &mut Body) {
    use http_body_util::BodyExt;
    let deadline = tokio::time::sleep(JOURNAL_LEFT);
    tokio::pin!(deadline);
    loop {
        let next = tokio::select! {
            f = core.frame() => f,
            () = &mut deadline => return,
        };
        let Some(Ok(frame)) = next else { return };
        let Ok(data) = frame.into_data() else {
            continue;
        };
        for piece in frames.feed(&data) {
            match piece {
                Piece::Prepared(p) => {
                    return Journal::finish_with(app, user, journal_handle(&p), Vec::new());
                }
                Piece::Unreadable => return,
                Piece::Forward(_) => {}
            }
        }
    }
}

/// The journal handle of a frame [`Prepared::read`] refused, when it names one.
fn journal_handle(frame: &JValue) -> Option<String> {
    frame
        .get("turn")
        .get("diaryJournal")
        .as_str()
        .filter(|h| h.len() == 32 && h.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
        .map(str::to_string)
}

/// Removes a chat's partial file (chat-partial.cjs's finish without a reply to keep).
fn clear_partial(user_dir: &Path, chat: &str) {
    let path = server_projects::partial::file_for(user_dir, chat);
    if std::fs::remove_file(&path).is_err() && path.exists() {
        eprintln!("chat native: could not remove a finished reply's partial");
    }
}

/// One native turn's side of this process.
pub(crate) struct FrontHost {
    app: Arc<App>,
    user: String,
    user_dir: PathBuf,
    data_dir: PathBuf,
    tx: mpsc::UnboundedSender<Bytes>,
    queued: Arc<Queued>,
    saving: Mutex<Saving>,
    /// Closed by the turn's end: a partial tick that comes later writes nothing (review F1).
    partial: PartialGate,
    /// A Diary extras preparation's recovery journal (track R16), set once at the turn's start.
    journal: OnceLock<Journal>,
    lease: Mutex<Option<engine_admission::Lease>>,
    /// The turn's `x-noevia-lease` value, as [`Host::lease`] returned it (the load bridge carries it).
    lease_token: Mutex<Option<String>>,
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

    /// The 4 s partial save: nothing once the turn's end has kept or cleared the partial.
    fn tick_partial(&self, safe: &str, file: &JValue) {
        self.partial.tick(|| self.write_partial(safe, file));
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

    /// A JSON call on core's loopback bridges, its body written as JS writes it. The client gone, the call
    /// is dropped (core's bridge sees its caller go: a load stops, as chat.cjs's does with the turn's signal).
    async fn core(&self, path: &str, body: &JValue, timeout: Duration) -> Option<(u16, JValue)> {
        tokio::select! {
            r = self.core_full(path, body, timeout) => r,
            () = closed(&self.tx) => None,
        }
    }

    /// [`Self::core`] that runs to its answer whatever the client does.
    async fn core_full(
        &self,
        path: &str,
        body: &JValue,
        timeout: Duration,
    ) -> Option<(u16, JValue)> {
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
        // chat.cjs `send` journals every event, the client gone or not.
        if let Some(journal) = self.journal.get() {
            journal.event(event);
        }
        self.observe(event, left);
        if !left {
            let _ = push(
                &self.tx,
                &self.queued,
                Bytes::from(format!("{}\n", js::stringify(event))),
            );
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
        let entered = tokio::select! {
            r = admission.enter(engine_admission::Ask::Engine { model }, &cancel) => r,
            () = closed(&self.tx) => {
                // The client left while the turn waited for the model: it takes nothing (Node's turn
                // signal ends its wait the same way).
                cancel.cancel();
                return Err(GateRefusal {
                    text: "This operation was aborted".into(),
                    code: None,
                    retryable: Some(false),
                });
            }
        };
        match entered {
            Ok(lease) => {
                let headers = lease.headers().to_vec();
                let token = headers
                    .iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case("x-noevia-lease"))
                    .map(|(_, v)| v.clone());
                if let Ok(mut t) = self.lease_token.lock() {
                    *t = token;
                }
                if let Ok(mut slot) = self.lease.lock() {
                    *slot = Some(lease);
                }
                // The client may have left between the grant and here: the watcher found no lease then.
                if self.left() {
                    self.release_lease();
                }
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
        let lease = self.lease.lock().ok().and_then(|mut l| l.take());
        if let Some(l) = lease {
            tokio::spawn(l.release());
        }
    }

    // Core answers health and load 409 `{enabled:false}` when the model manager is off; the turn
    // never asks then (the prepared frame's `managerEnabled`), and either is read as not ok.
    async fn health(&self) -> Option<JValue> {
        let (status, v) = self
            .core(
                "/internal/chat/native/health",
                &js::obj([("user", js::s(self.user.clone()))]),
                BRIDGE_TIMEOUT,
            )
            .await?;
        (status == 200 && v.get("ok") == &JValue::Bool(true)).then(|| v.get("body").clone())
    }

    async fn load(&self, model: &str) -> bool {
        let lease = self.lease_token.lock().ok().and_then(|t| t.clone());
        let r = self
            .core(
                "/internal/chat/native/load",
                &js::obj([
                    ("user", js::s(self.user.clone())),
                    ("model", js::s(model)),
                    ("lease", lease.map_or(JValue::Null, js::s)),
                ]),
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
                &js::obj([("user", js::s(self.user.clone())), ("model", js::s(model))]),
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
            let next = tokio::select! {
                c = response.chunk() => c,
                () = closed(&self.tx) => break,
            };
            match next {
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
            js::obj([("user", js::s(self.user.clone())), ("model", js::s(model))]),
        );
    }

    fn gate_record(&self, event: &str, tool: &str) {
        // Track R15: the tool gate's log stays core's (tool-gate.cjs record).
        self.core_later(
            "/internal/chat/native/gate-record",
            js::obj([
                ("user", js::s(self.user.clone())),
                ("event", js::s(event)),
                ("tool", js::s(tool)),
            ]),
        );
    }

    async fn engine_busy(&self) -> bool {
        // chat.cjs asks inferenceBusy(1): its turn is one count in core's gate. A native turn holds the
        // route's admission lease (S8) and, while it lasts, the model's lease: each is one count there.
        let own = 1 + usize::from(self.lease.lock().is_ok_and(|l| l.is_some()));
        let r = self
            .core(
                "/internal/chat/native/busy",
                &js::obj([("user", js::s(self.user.clone())), ("own", js::count(own))]),
                BRIDGE_TIMEOUT,
            )
            .await;
        r.is_some_and(|(s, v)| s == 200 && v.get("busy") == &JValue::Bool(true))
    }

    async fn condense(
        &self,
        kind: &str,
        message: &str,
        tool: &str,
        result_text: &str,
        model: &str,
    ) -> Option<(String, JValue)> {
        // Track R15 (#740): core's framing reasoner through the condense op; no answer, or one not understood, is
        // a skip (the raw framed result goes on).
        let body = js::obj([
            ("user", js::s(self.user.clone())),
            ("kind", js::s(kind)),
            ("message", js::s(message)),
            ("tool", js::s(tool)),
            ("resultText", js::s(result_text)),
            ("model", js::s(model)),
        ]);
        match self
            .core("/internal/chat/native/condense", &body, CONDENSE_TIMEOUT)
            .await
        {
            Some((200, v)) => match (v.get("rendered").as_str(), v.get("trace")) {
                (Some(rendered), trace @ JValue::Obj(_)) => {
                    Some((rendered.to_string(), trace.clone()))
                }
                _ => None,
            },
            Some((status, _)) => {
                eprintln!("chat native: the framing reasoner answered {status}; the prefetched result goes on as read");
                None
            }
            None => None,
        }
    }

    async fn reasoner_trace(&self, trace: &JValue, answer_chars: f64, answer_ms: f64) {
        // Track R15 (#740): the opt-in trace stays core's (the person's preference and their workspace's file),
        // written before `done` as chat.cjs writes it; a trace never changes the reply.
        let body = js::obj([
            ("user", js::s(self.user.clone())),
            ("trace", trace.clone()),
            ("answerChars", js::n(answer_chars)),
            ("answerMs", js::n(answer_ms)),
        ]);
        match self
            .core(
                "/internal/chat/native/reasoner-trace",
                &body,
                BRIDGE_TIMEOUT,
            )
            .await
        {
            Some((204, _)) | None => {}
            Some((status, _)) => {
                eprintln!(
                    "chat native: core answered {status} to the reasoning trace; it is not kept"
                );
            }
        }
    }

    async fn supervise(&self, round: i32, outputs: &[JValue], goal: &str, wrote: bool) -> String {
        // Track R15: core's step supervisor (chat.cjs superviseNextStep) through the supervise op; no answer, or one
        // not understood, is `continue` (the supervisor's own fallback).
        let body = js::obj([
            ("user", js::s(self.user.clone())),
            ("round", js::n(f64::from(round))),
            ("goal", js::s(goal)),
            ("outputs", JValue::Arr(outputs.to_vec())),
            ("wrote", JValue::Bool(wrote)),
        ]);
        let r = self
            .core("/internal/chat/native/supervise", &body, SUPERVISE_TIMEOUT)
            .await;
        match r {
            Some((200, v)) => match v.get("action").as_str() {
                Some(a @ ("verify" | "escalate")) => a.to_string(),
                _ => "continue".to_string(),
            },
            Some((status, _)) => {
                eprintln!("chat native: the step supervisor answered {status}; the turn continues unsupervised for this round");
                "continue".to_string()
            }
            None => "continue".to_string(),
        }
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
                ("user", js::s(self.user.clone())),
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

    async fn edit_target(&self, project_id: Option<&str>, name: &str, args: &str) -> EditTarget {
        // Track R14: the front's own lookup when NOEVIA_RUST_EDIT_TARGET=1 (None: core's bridge).
        if crate::edit_target_native::enabled() {
            if let Some(found) =
                crate::edit_target_native::lookup(&self.app, &self.user, project_id, name, args)
                    .await
            {
                return found;
            }
        }
        let body = js::obj([
            ("user", js::s(self.user.clone())),
            ("projectId", project_id.map_or(JValue::Null, js::s)),
            ("name", js::s(name)),
            ("args", js::s(args)),
        ]);
        match self
            .core("/internal/chat/tools/edit-target", &body, BRIDGE_TIMEOUT)
            .await
        {
            Some((200, v)) => {
                if let Some(e) = v.get("error").as_str() {
                    return EditTarget::Refused(e.to_string());
                }
                match (v.get("path").as_str(), v.get("digest").as_str()) {
                    (Some(path), Some(digest))
                        if digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit()) =>
                    {
                        EditTarget::Found {
                            path: path.to_string(),
                            account: v.get("account").as_str().map(str::to_string),
                            digest: digest.to_string(),
                        }
                    }
                    _ => EditTarget::Failed,
                }
            }
            _ => EditTarget::Failed,
        }
    }

    async fn skills_revoked(
        &self,
        project_id: &str,
        loaded: &[Skill],
    ) -> Option<Vec<(String, String)>> {
        let body = js::obj([
            ("user", js::s(self.user.clone())),
            ("projectId", js::s(project_id)),
            ("loaded", skills_json(loaded)),
        ]);
        match self
            .core("/internal/chat/skills/revoked", &body, BRIDGE_TIMEOUT)
            .await
        {
            Some((200, v)) => match v.get("gone") {
                JValue::Arr(items) => items
                    .iter()
                    .map(|g| {
                        Some((
                            g.get("file").as_str()?.to_string(),
                            g.get("name").as_str()?.to_string(),
                        ))
                    })
                    .collect(),
                _ => None,
            },
            _ => None,
        }
    }

    async fn skills_read(
        &self,
        project_id: &str,
        chat_id: Option<&str>,
        candidates: &[Skill],
        args: &str,
        result: &str,
    ) -> Option<Vec<Skill>> {
        let body = js::obj([
            ("user", js::s(self.user.clone())),
            ("projectId", js::s(project_id)),
            ("chatId", chat_id.map_or(JValue::Null, js::s)),
            ("candidates", skills_json(candidates)),
            ("args", js::s(args)),
            ("result", js::s(result)),
        ]);
        match self
            .core("/internal/chat/skills/read", &body, BRIDGE_TIMEOUT)
            .await
        {
            Some((200, v)) => skills_of(v.get("loaded")),
            _ => None,
        }
    }

    async fn skills_scrub(
        &self,
        project_id: &str,
        chat_id: Option<&str>,
        messages: &[JValue],
    ) -> Option<Scrub> {
        let body = js::obj([
            ("user", js::s(self.user.clone())),
            ("projectId", js::s(project_id)),
            ("chatId", chat_id.map_or(JValue::Null, js::s)),
            ("messages", JValue::Arr(messages.to_vec())),
        ]);
        let (status, v) = self
            .core("/internal/chat/skills/scrub", &body, BRIDGE_TIMEOUT)
            .await?;
        if status != 200 {
            return None;
        }
        let JValue::Arr(changed) = v.get("changed") else {
            return None;
        };
        let changed = changed
            .iter()
            .map(|c| match c {
                JValue::Arr(pair) => match (
                    pair.first().and_then(JValue::as_num),
                    pair.get(1).and_then(JValue::as_str),
                ) {
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    (Some(i), Some(text))
                        if i >= 0.0 && i.fract() == 0.0 && (i as usize) < messages.len() =>
                    {
                        Some((i as usize, text.to_string()))
                    }
                    _ => None,
                },
                _ => None,
            })
            .collect::<Option<Vec<_>>>()?;
        let JValue::Arr(removed) = v.get("removed") else {
            return None;
        };
        let removed = removed
            .iter()
            .map(|r| r.as_str().map(str::to_string))
            .collect::<Option<Vec<_>>>()?;
        Some(Scrub { changed, removed })
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
            .ok()
            .flatten()
            .ok_or(())
    }

    async fn chat_wide_approved(
        &self,
        chat_id: Option<&str>,
        scope: &str,
        binding: &JValue,
        exchange_key: &str,
    ) -> Option<String> {
        let chat = chat_id?;
        if matches!(binding, JValue::Null) {
            return None;
        }
        let body = js::obj([
            ("user", js::s(self.user.clone())),
            ("chat", js::s(chat)),
            ("scope", js::s(scope)),
            ("binding", binding.clone()),
            ("exchangeKey", js::s(exchange_key)),
        ]);
        // A core without the grant bridge (404) or any other answer: no grant, so the card is asked (D17).
        // Run to its answer when the client leaves meanwhile, as Node's grant check does: the gate then
        // refuses as cancelled (with a grant) or as stopped on the card (without one).
        let (status, v) = self.core_full(GRANT_PATH, &body, BRIDGE_TIMEOUT).await?;
        let handle = v.get("handle").as_str()?;
        (status == 200
            && v.get("granted") == &JValue::Bool(true)
            && handle.len() == 32
            && handle
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
        .then(|| handle.to_string())
    }

    async fn open_approval(&self, request: &JValue) -> bool {
        self.approvals.open(&self.user, request)
    }

    async fn await_approval(&self, request: &JValue) -> String {
        let id = request.get("id").as_str().unwrap_or("").to_string();
        // rust-approval-registry.cjs wait: a turn already stopped, or stopped while waiting, withdraws the
        // question (#679: a later answer finds nothing, and no grant comes of it).
        let wait = self.approvals.wait(&self.user, request);
        if self.left() {
            drop(wait);
            self.approvals.cancel(&self.user, &id);
            return "aborted".into();
        }
        let answer = tokio::select! {
            d = wait => d,
            () = closed(&self.tx) => "aborted".into(),
        };
        if answer == "aborted" {
            self.approvals.cancel(&self.user, &id);
        }
        answer
    }

    async fn revoke_grant(&self, chat_id: Option<&str>) {
        let Some(chat) = chat_id else {
            return;
        };
        let body = js::obj([("user", js::s(self.user.clone())), ("chat", js::s(chat))]);
        // Node's revokeChatGrant moves its epoch first and retries an unconfirmed revoke; when core cannot
        // be asked, the front's own revoke still ends the grant here (and asks Node to acknowledge it).
        if !matches!(
            self.core_full(REVOKE_PATH, &body, BRIDGE_TIMEOUT).await,
            Some((200, _))
        ) {
            let _ = self.app.revoke_approvals(&self.user, chat).await;
        }
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

    async fn execute_tool(&self, run: &ToolRun) -> Ran {
        // Track R13: read_project_file answered here when NOEVIA_RUST_READ_FILE is on and the call is one
        // the port answers as core would (project_file_read.rs); core still counts it.
        if crate::project_file_read::enabled() && run.name == crate::project_file_read::NAME {
            let (data, user, call) = (self.data_dir.clone(), self.user.clone(), run.clone());
            let native = tokio::task::spawn_blocking(move || {
                crate::project_file_read::answer(&data, &user, &call, |u| {
                    crate::project_file_read::account_active(&data, u)
                })
            });
            if let Ok(Some((text, failed))) = native.await {
                // Core counts it (recordToolUse) before the answer goes on, as its bridge counts a call it runs.
                let counted = self
                    .core_full(
                        "/internal/chat/native/tool-used",
                        &js::obj([
                            ("user", js::s(self.user.clone())),
                            ("name", js::s(run.name.clone())),
                        ]),
                        BRIDGE_TIMEOUT,
                    )
                    .await;
                if !matches!(counted, Some((204, _))) {
                    eprintln!(
                        "chat native: a read_project_file the front answered was not counted"
                    );
                }
                return Ran::Done {
                    text,
                    failed,
                    not_run: false,
                };
            }
        }
        let mut body = vec![
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
            (
                "editTarget",
                run.edit_target.clone().map_or(JValue::Null, js::s),
            ),
        ];
        if let Some(g) = &run.grant {
            body.push((
                "grant",
                js::obj([
                    ("handle", js::s(g.handle.clone())),
                    ("chat", js::s(g.chat.clone())),
                    ("scope", js::s(g.scope.clone())),
                    ("binding", g.binding.clone()),
                ]),
            ));
        }
        let body = js::obj(body);
        // Node's tool gets the turn's abort signal when the client leaves and the turn still takes its
        // answer: the front asks core to abort the run, and waits for it.
        let call = self.core_full("/internal/chat/tools/execute", &body, TOOL_TIMEOUT);
        tokio::pin!(call);
        let answer = tokio::select! {
            r = &mut call => r,
            () = closed(&self.tx) => {
                self.core_later(
                    "/internal/chat/tools/abort",
                    js::obj([
                        ("user", js::s(self.user.clone())),
                        ("exchangeKey", js::s(run.exchange_key.clone())),
                    ]),
                );
                call.await
            }
        };
        match answer {
            // A grant core did not redeem: the call did not run.
            Some((200, v))
                if run.grant.is_some() && v.get("redeem").as_str() == Some("refused") =>
            {
                Ran::Refused
            }
            Some((200, v)) if v.get("redeem").as_str() == Some("aborted") => Ran::Aborted,
            Some((200, v)) if v.get("redeem").as_str().is_some() => Ran::Unavailable,
            // A grant core refused to read (400) was never run: not checked.
            Some((400, _)) if run.grant.is_some() => Ran::Unavailable,
            Some((200, v)) => Ran::Done {
                text: v.get("text").as_str().unwrap_or("").to_string(),
                failed: v.get("failed") == &JValue::Bool(true),
                not_run: v.get("notRun") == &JValue::Bool(true),
            },
            Some((_, v)) => Ran::Threw(
                v.get("error")
                    .as_str()
                    .unwrap_or("the tool bridge failed")
                    .to_string(),
            ),
            None => Ran::Threw("the tool bridge did not answer".into()),
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
        self.partial.end(|| self.write_partial(chat, file));
    }

    fn end_partial(&self, chat: &str) {
        self.partial.end(|| clear_partial(&self.user_dir, chat));
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

/// Skills as the bridges carry them: `[{file, hash, name}]`.
fn skills_json(list: &[Skill]) -> JValue {
    JValue::Arr(
        list.iter()
            .map(|k| {
                js::obj([
                    ("file", js::s(k.file.clone())),
                    ("hash", js::s(k.hash.clone())),
                    ("name", js::s(k.name.clone())),
                ])
            })
            .collect(),
    )
}

/// `[{file, hash, name}]` read back; `None` for anything else.
fn skills_of(v: &JValue) -> Option<Vec<Skill>> {
    let JValue::Arr(items) = v else { return None };
    items
        .iter()
        .map(|k| {
            Some(Skill {
                file: k.get("file").as_str()?.to_string(),
                hash: k.get("hash").as_str()?.to_string(),
                name: k.get("name").as_str()?.to_string(),
            })
        })
        .collect()
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
    fn a_card_request_becomes_the_registry_question_once_included() {
        let request = |once: bool| {
            js::obj([
                ("id", js::s("ap-1")),
                ("userId", js::s("qa-user")),
                ("chatId", js::s("qa-chat-1")),
                ("scope", js::s("project:qa")),
                ("binding", js::obj([("user", js::s("qa-user"))])),
                ("once", JValue::Bool(once)),
            ])
        };
        let q = question("qa-user", &request(true)).unwrap();
        assert!(q.once);
        assert_eq!(
            (q.id.as_str(), q.chat.as_deref(), q.scope.as_deref()),
            ("ap-1", Some("qa-chat-1"), Some("project:qa"))
        );
        assert_eq!(q.binding, Some(serde_json::json!({"user": "qa-user"})));
        assert!(!question("qa-user", &request(false)).unwrap().once);
        let no_chat = js::obj([
            ("id", js::s("ap-2")),
            ("chatId", JValue::Null),
            ("scope", js::s("")),
            ("binding", JValue::Null),
            ("once", JValue::Bool(false)),
        ]);
        let q = question("qa-user", &no_chat).unwrap();
        assert_eq!((q.chat, q.scope, q.binding), (None, None, None));
        assert!(question("qa-user", &js::obj([("once", JValue::Bool(true))])).is_none());
    }

    #[test]
    fn the_start_of_a_turn_clears_the_partial_and_the_saver_writes_until_the_end() {
        // noevia#2750: the start-of-turn clear once closed the gate, so no 4 s tick ever wrote.
        let dir = tempfile::tempdir().unwrap();
        let path = server_projects::partial::file_for(dir.path(), "qa-chat-1");
        let write = || {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, "{}").unwrap();
        };
        write();
        let gate = PartialGate::default();
        clear_partial(dir.path(), "qa-chat-1");
        assert!(!path.exists(), "core's partial is cleared at the start");
        gate.tick(write);
        assert!(path.exists(), "a tick after the start writes the partial");
        gate.end(|| clear_partial(dir.path(), "qa-chat-1"));
        assert!(!path.exists());
        gate.tick(write);
        assert!(!path.exists(), "no tick writes after the turn's end");
        clear_partial(dir.path(), "qa-chat-1");
    }

    #[test]
    fn uuids_are_version_four() {
        let u = uuid_v4();
        assert_eq!(u.len(), 36);
        assert_eq!(&u[14..15], "4");
        assert!(matches!(&u[19..20], "8" | "9" | "a" | "b"));
    }
}
