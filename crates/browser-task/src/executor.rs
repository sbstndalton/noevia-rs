//! core browser-executor.cjs (`createBrowserExecutor`): open / act / close over an [`Engine`], with
//! crate browser-policy deciding before every action.
//!
//! - The facts the policy judges come from the page (the engine's `describe`, core's
//!   `describeInPage`): a model names an element, it never describes it.
//! - Every request the page makes is asked of the task's allowed list here (so an action can
//!   report "blocked" and the audit say what was refused); the egress proxy is the boundary a
//!   server redirect cannot pass. After every action the page's own address is checked too, and a
//!   page that ended up somewhere it may not be is reported blocked and stepped back.
//! - Secrets are typed only on an origin they are bound to; evidence is masked, and screenshots
//!   stop once a session has handled a secret.
//! - Uploads come only from files the task was given; downloads land in the task's own directory.
//! - An approval binds the page origin and the exact element that supplied the card.
//!
//! [`Engine`]: crate::engine

use crate::engine::{
    self, Browser, Context, Download, Handle, Launch, Page, Proxy, Request, Route, Socket,
};
use crate::policy::{self, Secret, Substituted};
use crate::signal::{Signal, Slot};
use crate::Fail;
use base64::Engine as _;
use js_json::{to_js_string, wtf16, JValue};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::Duration;

/// `ACTIONS`.
pub const ACTIONS: [&str; 12] = [
    "navigate",
    "click",
    "type",
    "select",
    "press",
    "upload",
    "submit",
    "extract",
    "screenshot",
    "scroll",
    "hover",
    "wait",
];
/// `ACTION_TIMEOUT_MS`.
pub const ACTION_TIMEOUT: Duration = Duration::from_millis(15_000);
const MAX_EXTRACT: usize = 20_000;
const SECRET_SCREENSHOT_REASON: &str =
    "Screenshot omitted because this browser session has handled a secret.";
const FAILED_SCREENSHOT_REASON: &str = "Screenshot omitted because capture failed.";
const DECLINED: &str = "Declined.";
const PAGE_CHANGED: &str = "The page changed while approval was pending; review the action again.";
const TARGET_CHANGED: &str =
    "The approved target changed while approval was pending; review the action again.";
const NOT_ON_LIST: &str = "Not on this task\u{2019}s allowed list.";
/// `sameElement`'s keys.
const SAME_KEYS: [&str; 10] = [
    "tag",
    "type",
    "role",
    "name",
    "text",
    "value",
    "inForm",
    "formMethod",
    "formAction",
    "frameOrigin",
];

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn s(text: &str) -> JValue {
    JValue::Str(text.to_string())
}

/// What a field holds once it is there (the page the session follows, the launched browser).
type Current<T> = Mutex<Option<Arc<T>>>;

/// A flight a field holds once it is started.
pub type Held<T> = Mutex<Option<Arc<Flight<T>>>>;

/// A result computed once on its own thread and waited for by anyone (a promise kept in a field).
pub struct Flight<T: Clone + Send + 'static> {
    slot: Slot<T>,
}

impl<T: Clone + Send + 'static> Flight<T> {
    /// `work` on its own thread. A panic in it (an engine's, say) settles the flight with `fallback`,
    /// so nobody waits on it forever.
    pub fn spawn_or(fallback: T, work: impl FnOnce() -> T + Send + 'static) -> Arc<Flight<T>> {
        let flight = Arc::new(Flight { slot: Slot::new() });
        let f = flight.clone();
        std::thread::spawn(move || {
            let value =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(work)).unwrap_or(fallback);
            f.slot.settle(value);
        });
        flight
    }

    pub fn wait(&self) -> T {
        self.slot.wait()
    }

    /// `None` when it has not finished within `limit`.
    pub fn wait_for(&self, limit: Duration) -> Option<T> {
        self.slot.wait_for(limit)
    }

    pub fn peek(&self) -> Option<T> {
        self.slot.peek()
    }
}

/// `String(v)`, `''` where JS would throw (an object with its own `toString`).
pub(crate) fn js_string(v: &JValue) -> String {
    to_js_string(v).unwrap_or_default()
}

/// What V8 throws for `String(v)` of an object whose own `toString` is not a function.
pub(crate) const NOT_PRIMITIVE: &str = "Cannot convert object to primitive value";

/// `String(v)` where core lets the TypeError escape: a 500 ("Browser request failed").
pub(crate) fn coerce(v: &JValue) -> Result<String, Fail> {
    to_js_string(v).map_err(|_| Fail::plain(None, NOT_PRIMITIVE))
}

/// Node's `path.basename` (POSIX): trailing slashes dropped, then what follows the last slash.
fn js_basename(p: &str) -> &str {
    let trimmed = p.trim_end_matches('/');
    if trimmed.is_empty() {
        return if p.is_empty() { "" } else { "/" };
    }
    trimmed.rsplit('/').next().unwrap_or(trimmed)
}

/// `Number(v)` for JSON values.
pub(crate) fn js_number(v: &JValue) -> f64 {
    match v {
        JValue::Undefined => f64::NAN,
        JValue::Null => 0.0,
        JValue::Bool(b) => f64::from(u8::from(*b)),
        JValue::Num(n) => *n,
        JValue::Str(t) => string_number(t),
        JValue::Arr(_) | JValue::Obj(_) => string_number(&js_string(v)),
    }
}

fn js_space(c: char) -> bool {
    matches!(c, '\u{9}'..='\u{d}' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}')
}

/// `Number(string)` (StringToNumber).
fn string_number(text: &str) -> f64 {
    let t = text.trim_matches(js_space);
    if t.is_empty() {
        return 0.0;
    }
    let radix = |prefix: &str, base: u32| -> Option<f64> {
        let digits = t.get(2..)?;
        (t.len() > 2
            && t.get(..2)?.eq_ignore_ascii_case(prefix)
            && digits.chars().all(|c| c.is_digit(base)))
        .then(|| {
            digits.chars().fold(0.0, |acc, c| {
                acc * f64::from(base) + f64::from(c.to_digit(base).unwrap_or(0))
            })
        })
    };
    if let Some(n) = radix("0x", 16)
        .or_else(|| radix("0o", 8))
        .or_else(|| radix("0b", 2))
    {
        return n;
    }
    match t {
        "Infinity" | "+Infinity" => return f64::INFINITY,
        "-Infinity" => return f64::NEG_INFINITY,
        _ => {}
    }
    let body = t.strip_prefix(['+', '-']).unwrap_or(t);
    let ok = !body.is_empty()
        && body
            .chars()
            .all(|c| c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E' | '+' | '-'))
        && body
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_digit() || c == '.');
    if !ok {
        return f64::NAN;
    }
    t.parse::<f64>().unwrap_or(f64::NAN)
}

/// `new URL(url)`'s origin for http(s), else `''`.
pub(crate) fn origin_of_url(url: &str) -> String {
    match url::Url::parse(url) {
        Ok(u) if matches!(u.scheme(), "http" | "https") => u.origin().ascii_serialization(),
        _ => String::new(),
    }
}

fn origin_of(page: &Arc<dyn Page>) -> String {
    origin_of_url(&page.url())
}

/// `/^https?:/i`.
fn http_like(url: &str) -> bool {
    let lower = url
        .get(..6)
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    lower.starts_with("http:") || lower.starts_with("https:")
}

/// `askApproval(card)`: `approve`, or anything else (refused). A failure is a refusal.
pub type Ask = Arc<dyn Fn(&JValue) -> Result<String, Fail> + Send + Sync>;

/// The executor's dependencies.
pub struct Deps {
    pub launch: Arc<dyn Launch>,
    /// `askApproval(card)`: `approve`, or anything else (refused). A failure is a refusal.
    pub ask: Ask,
    pub secrets: Vec<Secret>,
    pub log: Arc<dyn Fn(JValue) + Send + Sync>,
    pub timeout: Duration,
    /// Development only: a session without an egress proxy.
    pub direct: bool,
    /// The session ids (`crypto.randomUUID`).
    pub new_id: Arc<dyn Fn() -> String + Send + Sync>,
}

/// `open()`'s task.
pub struct Open {
    pub job_id: String,
    pub allowed_domains: Vec<String>,
    pub downloads_dir: PathBuf,
    pub upload_files: Vec<PathBuf>,
    pub signal: Option<Arc<Signal>>,
    pub proxy: Option<Proxy>,
}

struct Saved {
    name: String,
    from: String,
}

/// The signal a session's close hangs on, and its hook.
type Hooked = (Arc<Signal>, Option<u64>);

struct Session {
    id: String,
    job_id: String,
    context: Arc<dyn Context>,
    page: Current<dyn Page>,
    allowed: Vec<String>,
    downloads_dir: PathBuf,
    uploads: HashSet<PathBuf>,
    blocked: Mutex<Vec<JValue>>,
    downloads: Mutex<Vec<Saved>>,
    closed: AtomicBool,
    secret_typed: AtomicBool,
    nav_blocked: Mutex<Option<String>>,
    download_seq: AtomicU64,
    abort: Mutex<Option<Hooked>>,
    close_flight: Held<bool>,
}

impl Session {
    fn page(&self) -> Option<Arc<dyn Page>> {
        lock(&self.page).clone()
    }
}

/// One executor: one browser (launched on first open), its sessions.
pub struct Executor {
    deps: Deps,
    me: Weak<Executor>,
    sessions: Mutex<Vec<Arc<Session>>>,
    browser: Current<dyn Browser>,
    shutdown_flight: Held<bool>,
}

fn status_error(status: u16, message: &str) -> Fail {
    Fail::plain(Some(status), message)
}

fn cancelled() -> Fail {
    status_error(499, "Cancelled")
}

impl Executor {
    pub fn new(deps: Deps) -> Arc<Executor> {
        Arc::new_cyclic(|me| Executor {
            deps,
            me: me.clone(),
            sessions: Mutex::new(Vec::new()),
            browser: Mutex::new(None),
            shutdown_flight: Mutex::new(None),
        })
    }

    fn mask(&self, text: &str) -> String {
        policy::mask(text, &self.deps.secrets)
    }

    fn log(&self, entry: Vec<(&str, JValue)>) {
        (self.deps.log)(JValue::obj(entry));
    }

    fn session(&self, id: &str) -> Option<Arc<Session>> {
        lock(&self.sessions).iter().find(|s| s.id == id).cloned()
    }

    /// `open(task)`: the session id.
    pub fn open(&self, task: Open) -> Result<String, Fail> {
        if lock(&self.shutdown_flight).is_some() {
            return Err(status_error(409, "Browser executor is shut down."));
        }
        if task.allowed_domains.is_empty() {
            return Err(status_error(
                400,
                "A browser task needs at least one allowed domain.",
            ));
        }
        if task.downloads_dir.as_os_str().is_empty() {
            return Err(status_error(
                400,
                "A browser task needs its own downloads directory.",
            ));
        }
        if task.proxy.is_none() && !self.deps.direct {
            return Err(status_error(
                409,
                "A browser task needs an egress proxy grant for its domains.",
            ));
        }
        make_private_dir(&task.downloads_dir).map_err(|e| Fail::plain(None, &e.to_string()))?;
        let mut uploads = HashSet::new();
        for f in &task.upload_files {
            uploads
                .insert(std::fs::canonicalize(f).map_err(|e| Fail::plain(None, &e.to_string()))?);
        }
        let aborted = || task.signal.as_ref().is_some_and(|sig| sig.is_aborted());
        if aborted() {
            return Err(cancelled());
        }
        let browser = {
            let existing = lock(&self.browser).clone();
            match existing {
                Some(b) => b,
                None => {
                    let b = self.deps.launch.launch()?;
                    *lock(&self.browser) = Some(b.clone());
                    b
                }
            }
        };
        if aborted() {
            return Err(cancelled());
        }
        let context = browser
            .new_context(task.proxy.as_ref())
            .map_err(|e| Fail::plain(None, &e.message))?;
        let id = (self.deps.new_id)();
        let session = Arc::new(Session {
            id: id.clone(),
            job_id: task.job_id.clone(),
            context: context.clone(),
            page: Mutex::new(None),
            allowed: task.allowed_domains.clone(),
            downloads_dir: task.downloads_dir.clone(),
            uploads,
            blocked: Mutex::new(Vec::new()),
            downloads: Mutex::new(Vec::new()),
            closed: AtomicBool::new(false),
            secret_typed: AtomicBool::new(false),
            nav_blocked: Mutex::new(None),
            download_seq: AtomicU64::new(0),
            abort: Mutex::new(None),
            close_flight: Mutex::new(None),
        });
        // Own the context before any setup can fail.
        lock(&self.sessions).push(session.clone());
        if let Some(signal) = &task.signal {
            let me = self.me.clone();
            let sid = id.clone();
            let hook = signal.on_abort(move || {
                if let Some(me) = me.upgrade() {
                    let _ = me.close_flight(&sid);
                }
            });
            *lock(&session.abort) = Some((signal.clone(), hook));
        }
        let setup = (|| -> Result<(), Fail> {
            let engine_fail = |e: engine::EngineError| Fail::plain(None, &e.message);
            if aborted() {
                return Err(cancelled());
            }
            let weak = Arc::downgrade(&session);
            let me = self.me.clone();
            context
                .route(Arc::new(move |req: &Request| {
                    let (Some(session), Some(me)) = (weak.upgrade(), me.upgrade()) else {
                        return Route::Abort;
                    };
                    let lower = req
                        .url
                        .get(..5)
                        .map(str::to_ascii_lowercase)
                        .unwrap_or_default();
                    if lower.starts_with("data:") || lower.starts_with("blob:") {
                        return Route::Continue;
                    }
                    match policy::navigation(&req.url, &session.allowed) {
                        Ok(_) => Route::Continue,
                        Err(reason) => {
                            me.refuse(&session, Some(req), &req.url, &reason);
                            Route::Abort
                        }
                    }
                }))
                .map_err(engine_fail)?;
            if aborted() {
                return Err(cancelled());
            }
            let weak = Arc::downgrade(&session);
            let me = self.me.clone();
            context
                .route_websocket(Arc::new(move |url: &str| {
                    let (Some(session), Some(me)) = (weak.upgrade(), me.upgrade()) else {
                        return Socket::Close {
                            code: 1008,
                            reason: NOT_ON_LIST.into(),
                        };
                    };
                    let as_http = if url.get(..2).is_some_and(|p| p.eq_ignore_ascii_case("ws")) {
                        format!("http{}", url.get(2..).unwrap_or(""))
                    } else {
                        url.to_string()
                    };
                    match policy::navigation(&as_http, &session.allowed) {
                        Ok(_) => Socket::Connect,
                        Err(reason) => {
                            me.refuse(&session, None, url, &reason);
                            Socket::Close {
                                code: 1008,
                                reason: NOT_ON_LIST.into(),
                            }
                        }
                    }
                }))
                .map_err(engine_fail)?;
            if aborted() {
                return Err(cancelled());
            }
            let weak = Arc::downgrade(&session);
            let me = self.me.clone();
            let ctx = Arc::downgrade(&context);
            context.on_page(Arc::new(move |page: Arc<dyn Page>| {
                let Some(session) = weak.upgrade() else {
                    return;
                };
                // Popups share the context's routing; the session follows the newest page, and falls
                // back to the most recent one still open when it closes.
                *lock(&session.page) = Some(page.clone());
                let (w, m) = (Arc::downgrade(&session), me.clone());
                page.on_download(Arc::new(move |download: Arc<dyn Download>| {
                    if let (Some(session), Some(me)) = (w.upgrade(), m.upgrade()) {
                        me.save(&session, download.as_ref());
                    }
                }));
                let (w, c, pid) = (Arc::downgrade(&session), ctx.clone(), page.id());
                page.on_close(Arc::new(move || {
                    let (Some(session), Some(context)) = (w.upgrade(), c.upgrade()) else {
                        return;
                    };
                    if session.page().map(|p| p.id()) != Some(pid) {
                        return;
                    }
                    let open: Vec<Arc<dyn Page>> = context
                        .pages()
                        .into_iter()
                        .filter(|p| !p.is_closed())
                        .collect();
                    *lock(&session.page) = open.last().cloned();
                }));
            }));
            let page = context.new_page().map_err(engine_fail)?;
            *lock(&session.page) = Some(page);
            if aborted() {
                return Err(cancelled());
            }
            Ok(())
        })();
        if let Err(e) = setup {
            // Exact-context cleanup starts without hiding the setup error behind a hung close.
            let _ = self.close_flight(&id);
            return Err(e);
        }
        self.log(vec![
            ("event", s("browser.opened")),
            ("jobId", s(&task.job_id)),
            ("session", s(&id)),
            (
                "allowedDomains",
                JValue::Arr(task.allowed_domains.iter().map(|d| s(d)).collect()),
            ),
        ]);
        Ok(id)
    }

    /// `refuse(req, url, reason)`.
    fn refuse(&self, session: &Session, req: Option<&Request>, url: &str, reason: &str) {
        let shown = wtf16::slice(&self.mask(url), 300);
        {
            let mut blocked = lock(&session.blocked);
            if blocked.len() < 100 {
                blocked.push(JValue::obj([("url", s(&shown)), ("reason", s(reason))]));
            }
        }
        self.log(vec![
            ("event", s("browser.blocked")),
            ("jobId", s(&session.job_id)),
            ("url", s(&shown)),
            ("reason", s(reason)),
        ]);
        // A page leaving for a host it may not visit: the action that caused it reports blocked.
        if req.is_some_and(|r| r.main_frame_navigation) {
            *lock(&session.nav_blocked) = Some(reason.to_string());
        }
    }

    /// `save(session, download)`: the site's suggested name, base name only, numbered, in the task's
    /// own directory. A failed save is dropped (core's `.catch(() => {})`).
    fn save(&self, session: &Session, download: &dyn Download) {
        let suggested = download.suggested_filename();
        let suggested = if suggested.is_empty() {
            "download".to_string()
        } else {
            suggested
        };
        let trimmed = js_basename(&suggested).trim_start_matches('.');
        let name = if trimmed.is_empty() {
            "download".to_string()
        } else {
            trimmed.to_string()
        };
        let seq = session.download_seq.fetch_add(1, Ordering::SeqCst) + 1;
        let target = session.downloads_dir.join(format!("{seq}-{name}"));
        if download.save_as(&target).is_ok() {
            let from = wtf16::slice(&self.mask(&download.url()), 300);
            lock(&session.downloads).push(Saved { name, from });
        }
    }

    fn evidence(&self, session: &Session, page: &Arc<dyn Page>) -> Vec<(String, JValue)> {
        if session.secret_typed.load(Ordering::SeqCst) {
            return vec![
                ("screenshotOmitted".into(), JValue::Bool(true)),
                (
                    "screenshotOmissionReason".into(),
                    s(SECRET_SCREENSHOT_REASON),
                ),
            ];
        }
        match page.screenshot() {
            Ok(png) => vec![(
                "screenshot".into(),
                s(&base64::engine::general_purpose::STANDARD.encode(png)),
            )],
            Err(_) => vec![
                ("screenshotOmitted".into(), JValue::Bool(true)),
                (
                    "screenshotOmissionReason".into(),
                    s(FAILED_SCREENSHOT_REASON),
                ),
            ],
        }
    }

    /// `act(sessionId, action)`: `{ status: 'done'|'blocked'|'needs_approval'|'uncertain', origin,
    /// reason?, evidence?, downloads? }`. An unknown session is an error (404).
    pub fn act(&self, session_id: &str, action: &JValue) -> Result<JValue, Fail> {
        let session = self
            .session(session_id)
            .ok_or_else(|| status_error(404, "No such browser session."))?;
        let mut target: Option<Arc<dyn Handle>> = None;
        let out = self.act_with(&session, action, &mut target);
        // One action owns at most one handle, and every outcome releases it.
        if let Some(handle) = target {
            let _ = handle.dispose();
        }
        out
    }

    fn result(
        &self,
        session: &Session,
        kind: &str,
        status: &str,
        origin: &str,
        extra: Vec<(String, JValue)>,
    ) -> JValue {
        let mut out: Vec<(String, JValue)> =
            vec![("status".into(), s(status)), ("origin".into(), s(origin))];
        for (k, v) in extra {
            js_json::wtf16::set(&mut out, &k, v);
        }
        let mut origin_out = origin.to_string();
        let mut reason_out = String::new();
        for (k, v) in out.iter_mut() {
            if k == "reason" {
                if let JValue::Str(r) = v {
                    *r = self.mask(r);
                    reason_out = r.clone();
                }
            }
            if k == "origin" {
                origin_out = js_string(v);
            }
        }
        self.log(vec![
            ("event", s("browser.act")),
            ("jobId", s(&session.job_id)),
            ("session", s(&session.id)),
            ("type", s(kind)),
            ("status", s(status)),
            ("origin", s(&origin_out)),
            ("reason", s(&reason_out)),
        ]);
        JValue::Obj(out)
    }

    fn act_with(
        &self,
        session: &Arc<Session>,
        action: &JValue,
        target: &mut Option<Arc<dyn Handle>>,
    ) -> Result<JValue, Fail> {
        let kind = if action.get("type").truthy() {
            js_string(action.get("type"))
        } else {
            String::new()
        };
        // Answered as they are, without a log line (core returns these before its `result`).
        let unlogged = |reason: &str| {
            JValue::obj([
                ("status", s("blocked")),
                ("origin", s("")),
                ("reason", s(reason)),
            ])
        };
        if session.closed.load(Ordering::SeqCst) {
            return Ok(unlogged("The browser session is closed."));
        }
        let Some(page) = session.page() else {
            return Ok(unlogged("No page is open in this session."));
        };
        let origin = origin_of(&page);
        let blocked = |reason: &str| vec![("reason".to_string(), s(reason))];
        let done =
            |me: &Executor, status: &str, extra: Vec<(String, JValue)>| -> Result<JValue, Fail> {
                Ok(me.result(session, &kind, status, &origin, extra))
            };
        // The page the session follows now (a popup may have taken over), else the action's.
        let current = || session.page().unwrap_or_else(|| page.clone());
        if !ACTIONS.contains(&kind.as_str()) {
            let shown = wtf16::slice(&self.mask(&kind), 40);
            return done(
                self,
                "blocked",
                blocked(&format!("Unknown action \u{201c}{shown}\u{201d}.")),
            );
        }
        // Navigation is a GET here; data is sent by submitting the page's own form, which asks.
        let method = action.get("method");
        if kind == "navigate" && method.truthy() && coerce(method)?.to_uppercase() != "GET" {
            return done(
                self,
                "blocked",
                blocked(
                    "Only plain navigation is supported; submit the page\u{2019}s form instead.",
                ),
            );
        }
        let timeout = self.deps.timeout;
        // The element as it really is, never as the model described it.
        let selector = action.get("selector");
        let has_selector = selector.truthy();
        let sel = if has_selector && kind != "navigate" {
            Some(coerce(selector)?)
        } else {
            None
        };
        let mut element: Option<JValue> = None;
        if let Some(sel) = sel.as_deref() {
            let found = match page.locate(sel, timeout) {
                Ok(Some(handle)) => {
                    *target = Some(handle.clone());
                    handle.describe().ok()
                }
                _ => None,
            };
            match found {
                Some(facts) => element = Some(facts),
                None => return done(self, "blocked", blocked("That element is not on the page.")),
            }
        }
        let mut facts_element = element.clone().unwrap_or(JValue::Undefined);
        if kind == "press" && element.is_none() {
            // The key goes to whatever has focus; judge that element.
            *target = page.active_element().ok().flatten();
            element = target.as_ref().and_then(|h| h.describe().ok());
            // Unknown focus, or focus inside a frame: assume the worst, so any Enter asks.
            if element.as_ref().is_some_and(|e| {
                matches!(
                    e.get("tag").as_str(),
                    Some("iframe" | "frame" | "object" | "embed")
                )
            }) || element.as_ref().is_some_and(|e| !e.truthy())
            {
                element = None;
            }
            facts_element = element
                .clone()
                .unwrap_or_else(|| JValue::obj([("inForm", JValue::Bool(true))]));
        }
        let verdict = policy::classify(&kind, action, &facts_element, &origin, &session.allowed);
        if verdict.status == "blocked" {
            return done(self, "blocked", blocked(&verdict.reason));
        }
        // Values the action would type, with secrets resolved for this origin, and only for it.
        let mut typed: Option<String> = None;
        if kind == "type" || kind == "select" {
            let frame = element.as_ref().map(|e| js_string(e.get("frameOrigin")));
            let into = match frame {
                Some(f) if http_like(&f) => f,
                _ => origin.clone(),
            };
            let text = action.get("text");
            let raw = if !text.is_nullish() {
                text
            } else {
                action.get("value")
            };
            let raw = if raw.is_nullish() {
                String::new()
            } else {
                coerce(raw)?
            };
            let into = if into.is_empty() {
                "about:blank".to_string()
            } else {
                into
            };
            match policy::substitute(&raw, &self.deps.secrets, &into) {
                Substituted::Refused(reason) => return done(self, "blocked", blocked(&reason)),
                Substituted::Ok { value, used } => {
                    typed = Some(value);
                    if !used.is_empty() {
                        session.secret_typed.store(true, Ordering::SeqCst);
                    }
                }
            }
        }
        let mut file: Option<PathBuf> = None;
        if kind == "upload" {
            let raw = action.get("file");
            // core's realpath is inside its try: a value String() refuses is no file.
            let raw = if raw.truthy() {
                to_js_string(raw).ok()
            } else {
                Some(String::new())
            };
            file = raw.and_then(|r| std::fs::canonicalize(&r).ok());
            if !file.as_ref().is_some_and(|f| session.uploads.contains(f)) {
                return done(
                    self,
                    "blocked",
                    blocked("Only files given to this task can be uploaded."),
                );
            }
        }

        if verdict.status == "needs_approval" {
            let masked_element = element.as_ref().map_or(JValue::Null, |e| {
                let mut items = match e {
                    JValue::Obj(items) => items.clone(),
                    _ => Vec::new(),
                };
                for key in ["text", "name", "value", "formAction"] {
                    let v = e.get(key);
                    let raw = if v.is_nullish() {
                        String::new()
                    } else {
                        js_string(v)
                    };
                    js_json::wtf16::set(&mut items, key, s(&self.mask(&raw)));
                }
                JValue::Obj(items)
            });
            let url = action.get("url");
            let mut card: Vec<(String, JValue)> = vec![
                ("jobId".into(), s(&session.job_id)),
                ("origin".into(), s(&origin)),
                ("action".into(), s(&kind)),
                ("reason".into(), s(&verdict.reason)),
                ("element".into(), masked_element),
                (
                    "url".into(),
                    if url.truthy() {
                        s(&self.mask(&js_string(url)))
                    } else {
                        JValue::Null
                    },
                ),
                (
                    "typed".into(),
                    typed.as_ref().map_or(JValue::Null, |t| s(&self.mask(t))),
                ),
                (
                    "file".into(),
                    file.as_ref()
                        .and_then(|f| f.file_name())
                        .map_or(JValue::Null, |n| s(&n.to_string_lossy())),
                ),
            ];
            card.extend(self.evidence(session, &current()));
            let answer = (self.deps.ask)(&JValue::Obj(card)).unwrap_or_else(|_| "deny".into());
            self.log(vec![
                ("event", s("browser.decided")),
                ("jobId", s(&session.job_id)),
                ("type", s(&kind)),
                ("origin", s(&origin)),
                ("decision", s(&answer)),
            ]);
            if answer != "approve" {
                return done(self, "blocked", blocked(DECLINED));
            }
            // Approval binds the page origin and the exact node that supplied the card: keep and
            // re-check the original handle, then dispatch through it.
            let current = session.page();
            if current.as_ref().map(|p| p.id()) != Some(page.id())
                || page.is_closed()
                || origin_of(&page) != origin
            {
                let now = current.unwrap_or_else(|| page.clone());
                return done(
                    self,
                    "blocked",
                    vec![
                        ("reason".into(), s(PAGE_CHANGED)),
                        ("origin".into(), s(&origin_of(&now))),
                    ],
                );
            }
            if let Some(handle) = target.as_ref() {
                let connected = handle.is_connected().unwrap_or(false);
                let now = if connected {
                    handle.describe().ok()
                } else {
                    None
                };
                let same = now
                    .as_ref()
                    .is_some_and(|n| n.truthy() && same_element(element.as_ref(), n));
                if !connected || !same {
                    return done(self, "blocked", blocked(TARGET_CHANGED));
                }
                if kind == "press" && !has_selector && !handle.is_focused().unwrap_or(false) {
                    return done(self, "blocked", blocked(TARGET_CHANGED));
                }
            }
        }

        *lock(&session.nav_blocked) = None;
        let mut dispatched = false;
        let null_read = |m: &str| {
            engine::EngineError::new(format!("Cannot read properties of null (reading '{m}')"))
        };
        let on = |m: &str| target.clone().ok_or_else(|| null_read(m));
        // `String(action.url)` and `String(action.key)` run inside core's try: a refusal is the action's
        // failure.
        let url = if kind == "navigate" {
            to_js_string(action.get("url")).map_err(|_| NOT_PRIMITIVE)
        } else {
            Ok(String::new())
        };
        let key = if kind == "press" {
            to_js_string(action.get("key")).map_err(|_| NOT_PRIMITIVE)
        } else {
            Ok(String::new())
        };
        let step: engine::Result<Option<JValue>> = (|| {
            match kind.as_str() {
                "navigate" => {
                    dispatched = true;
                    page.goto(&url.clone().map_err(engine::EngineError::new)?, timeout)?;
                }
                "click" => {
                    dispatched = true;
                    on("click")?.click(timeout)?;
                }
                "hover" => on("hover")?.hover(timeout)?,
                "type" => {
                    dispatched = true;
                    on("fill")?.fill(typed.as_deref().unwrap_or(""), timeout)?;
                    typed = None;
                }
                "select" => {
                    dispatched = true;
                    on("selectOption")?.select_option(typed.as_deref().unwrap_or(""), timeout)?;
                    typed = None;
                }
                "press" => {
                    dispatched = true;
                    let key = key.clone().map_err(engine::EngineError::new)?;
                    match target.as_ref() {
                        Some(h) => h.press(&key, timeout)?,
                        None => page.keyboard_press(&key)?,
                    }
                }
                "upload" => {
                    dispatched = true;
                    if let Some(f) = file.as_ref() {
                        on("setInputFiles")?.set_input_files(f, timeout)?;
                    }
                }
                "submit" => {
                    dispatched = true;
                    on("evaluate")?.submit()?;
                }
                "scroll" => {
                    let dy = js_number(action.get("dy"));
                    page.wheel(if dy == 0.0 || dy.is_nan() { 600.0 } else { dy })?;
                }
                "wait" => {
                    let ms = js_number(action.get("ms"));
                    let ms = if ms == 0.0 || ms.is_nan() { 500.0 } else { ms };
                    page.wait_for_timeout(ms.min(5000.0))?;
                }
                "extract" => {
                    let text = if let Some(sel) = sel.as_deref() {
                        page.locator_text(sel, timeout)?
                    } else {
                        page.body_text(timeout)?
                    };
                    let shown = self.mask(&wtf16::slice(&text, MAX_EXTRACT));
                    return Ok(Some(JValue::obj([("text", s(&shown))])));
                }
                "screenshot" => return Ok(Some(JValue::Obj(self.evidence(session, &current())))),
                _ => {}
            }
            Ok(None)
        })();
        let origin_now = || session.page().map(|p| origin_of(&p)).unwrap_or_default();
        // core reads `session.page.url()` next: a session whose pages all closed throws there.
        let step = match step {
            Ok(None) if session.page().is_none() => {
                let _ = page.wait_for_load_state(timeout);
                Err(null_read("url"))
            }
            other => other,
        };
        match step {
            Ok(Some(evidence)) => done(self, "done", vec![("evidence".into(), evidence)]),
            Ok(None) => {
                let _ = page.wait_for_load_state(timeout);
                // Where the page actually is, whatever the routing saw.
                let landed = current().url();
                let refused = if http_like(&landed) {
                    policy::navigation(&landed, &session.allowed).err()
                } else {
                    None
                };
                {
                    let mut nav = lock(&session.nav_blocked);
                    if let (Some(reason), true) = (&refused, nav.is_none()) {
                        *nav = Some(reason.clone());
                        let mut list = lock(&session.blocked);
                        if list.len() < 100 {
                            list.push(JValue::obj([
                                ("url", s(&wtf16::slice(&self.mask(&landed), 300))),
                                ("reason", s(reason)),
                            ]));
                        }
                    }
                }
                let nav = lock(&session.nav_blocked).clone();
                if let Some(reason) = nav {
                    // Step back off the browser's error page, or the refused address.
                    if refused.is_some() || origin_now().is_empty() {
                        let _ = current().go_back(timeout);
                    }
                    return done(
                        self,
                        "blocked",
                        vec![
                            ("reason".into(), s(&reason)),
                            ("origin".into(), s(&origin_now())),
                        ],
                    );
                }
                let downloads: Vec<JValue> = lock(&session.downloads)
                    .iter()
                    .map(|d| JValue::obj([("name", s(&d.name)), ("from", s(&d.from))]))
                    .collect();
                done(
                    self,
                    "done",
                    vec![
                        ("origin".into(), s(&origin_now())),
                        ("downloads".into(), JValue::Arr(downloads)),
                    ],
                )
            }
            Err(error) => {
                let nav = lock(&session.nav_blocked).clone();
                if let Some(reason) = nav {
                    if origin_now().is_empty() {
                        // core's `session.page.goBack()` on a session with no page throws out of the action.
                        let Some(p) = session.page() else {
                            return Err(Fail::plain(
                                None,
                                "Cannot read properties of null (reading 'goBack')",
                            ));
                        };
                        let _ = p.go_back(timeout);
                    }
                    return done(
                        self,
                        "blocked",
                        vec![
                            ("reason".into(), s(&reason)),
                            ("origin".into(), s(&origin_now())),
                        ],
                    );
                }
                // Once an action with effects was dispatched, a failure does not prove it did not happen.
                // `String(error?.message || error)`: an empty message is the error's own name.
                let first = error.message.split('\n').next().unwrap_or("");
                let first = if error.message.is_empty() {
                    "Error"
                } else {
                    first
                };
                let reason = wtf16::slice(first, 200);
                done(
                    self,
                    if dispatched { "uncertain" } else { "blocked" },
                    vec![
                        ("reason".into(), s(&reason)),
                        ("origin".into(), s(&origin_now())),
                    ],
                )
            }
        }
    }

    fn close_flight(&self, session_id: &str) -> Option<Arc<Flight<bool>>> {
        let session = self.session(session_id)?;
        let mut flight = lock(&session.close_flight);
        if let Some(f) = flight.as_ref() {
            return Some(f.clone());
        }
        // Admission is refused at once; completion is a separate proof.
        session.closed.store(true, Ordering::SeqCst);
        if let Some((signal, hook)) = lock(&session.abort).take() {
            signal.off(hook);
        }
        let (context, log, job, sid) = (
            session.context.clone(),
            self.deps.log.clone(),
            session.job_id.clone(),
            session.id.clone(),
        );
        let f = Flight::spawn_or(false, move || match context.close() {
            Ok(()) => {
                log(JValue::obj([
                    ("event", s("browser.closed")),
                    ("jobId", s(&job)),
                    ("session", s(&sid)),
                ]));
                true
            }
            Err(_) => false,
        });
        *flight = Some(f.clone());
        Some(f)
    }

    /// `close(sessionId)`: whether the context's close was confirmed.
    pub fn close(&self, session_id: &str) -> bool {
        self.close_flight(session_id).is_some_and(|f| f.wait())
    }

    /// `closeTask(jobId)`: every session of the task closed, confirmed.
    pub fn close_task(&self, job_id: &str) -> bool {
        let ids: Vec<String> = lock(&self.sessions)
            .iter()
            .filter(|s| s.job_id == job_id)
            .map(|s| s.id.clone())
            .collect();
        let flights: Vec<Option<Arc<Flight<bool>>>> =
            ids.iter().map(|id| self.close_flight(id)).collect();
        flights.into_iter().all(|f| f.is_some_and(|f| f.wait()))
    }

    /// `shutdown()`: every session closed, then the browser; confirmed only when the browser closed.
    pub fn shutdown(&self) -> bool {
        let flight = {
            let mut slot = lock(&self.shutdown_flight);
            if let Some(f) = slot.as_ref() {
                f.clone()
            } else {
                let me = self.me.clone();
                let f = Flight::spawn_or(false, move || {
                    let Some(me) = me.upgrade() else { return false };
                    let ids: Vec<String> =
                        lock(&me.sessions).iter().map(|s| s.id.clone()).collect();
                    let flights: Vec<_> = ids.iter().filter_map(|id| me.close_flight(id)).collect();
                    for f in flights {
                        f.wait();
                    }
                    let browser = lock(&me.browser).clone();
                    if let Some(b) = browser {
                        if b.close().is_err() {
                            return false;
                        }
                    }
                    *lock(&me.browser) = None;
                    true
                });
                *slot = Some(f.clone());
                f
            }
        };
        flight.wait()
    }
}

/// `sameElement(before, after)`: `(before?.[key] ?? '') === (after?.[key] ?? '')` for each key.
fn same_element(before: Option<&JValue>, after: &JValue) -> bool {
    let read = |v: Option<&JValue>, key: &str| -> JValue {
        let x = v.map_or(&JValue::Undefined, |v| v.get(key));
        if x.is_nullish() {
            JValue::Str(String::new())
        } else {
            x.clone()
        }
    };
    SAME_KEYS
        .iter()
        .all(|k| strict_equal(&read(before, k), &read(Some(after), k)))
}

/// `===` over JSON values (objects and arrays by identity: two read separately never are).
fn strict_equal(a: &JValue, b: &JValue) -> bool {
    match (a, b) {
        (JValue::Num(x), JValue::Num(y)) => x == y,
        (JValue::Str(x), JValue::Str(y)) => x == y,
        (JValue::Bool(x), JValue::Bool(y)) => x == y,
        (JValue::Null, JValue::Null) | (JValue::Undefined, JValue::Undefined) => true,
        _ => false,
    }
}

/// `fs.mkdirSync(dir, { recursive: true, mode: 0o700 })`.
fn make_private_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(dir)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers() {
        for (v, want) in [
            (JValue::Str(" 300 ".into()), 300.0),
            (JValue::Str("".into()), 0.0),
            (JValue::Str("0x10".into()), 16.0),
            (JValue::Str("1e3".into()), 1000.0),
            (JValue::Bool(true), 1.0),
            (JValue::Null, 0.0),
            (JValue::Arr(vec![]), 0.0),
            (JValue::Arr(vec![JValue::Num(7.0)]), 7.0),
        ] {
            assert_eq!(js_number(&v), want, "{v:?}");
        }
        for v in [
            JValue::Undefined,
            JValue::Str("abc".into()),
            JValue::Str("inf".into()),
            JValue::Str("1 2".into()),
        ] {
            assert!(js_number(&v).is_nan(), "{v:?}");
        }
    }

    #[test]
    fn basenames() {
        for (p, want) in [
            ("../report.csv", "report.csv"),
            ("report.csv/.", "."),
            ("a/b/", "b"),
            ("..", ".."),
            ("/", "/"),
            ("", ""),
            ("x\\y", "x\\y"),
        ] {
            assert_eq!(js_basename(p), want, "{p}");
        }
    }

    #[test]
    fn origins() {
        assert_eq!(
            origin_of_url("https://Shop.Example.TEST:443/a?b"),
            "https://shop.example.test"
        );
        assert_eq!(origin_of_url("about:blank"), "");
        assert_eq!(origin_of_url("chrome-error://chromewebdata/"), "");
    }

    #[test]
    fn same_elements() {
        let a = JValue::obj([
            ("tag", JValue::Str("button".into())),
            ("inForm", JValue::Bool(true)),
        ]);
        let b = JValue::obj([
            ("tag", JValue::Str("button".into())),
            ("inForm", JValue::Bool(true)),
            ("text", JValue::Null),
        ]);
        assert!(same_element(Some(&a), &b));
        let c = JValue::obj([
            ("tag", JValue::Str("button".into())),
            ("inForm", JValue::Bool(false)),
        ]);
        assert!(!same_element(Some(&a), &c));
        assert!(!same_element(None, &a));
    }
}
