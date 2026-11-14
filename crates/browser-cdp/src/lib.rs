//! Browser mode's production engine: crate browser-task's [`engine`] traits over the Chrome DevTools
//! Protocol, driving an external headless Chromium (a container exposing
//! `--remote-debugging-port` on the deployment's private network).
//!
//! `NOEVIA_BROWSER_ENGINE` ([`ENGINE_ENV`]) is `http://host:port` (the browser WebSocket is found
//! with `GET /json/version`) or `ws://host:port/devtools/browser/<id>`; `https`/`wss` are refused.
//! One [`CdpLaunch::launch`] is one connection (flatten session mode: one `sessionId` per page),
//! with one reader thread that answers callers by `id` and handles events: the route gate, proxy
//! authentication and dialogs are answered on that thread with send-only messages, so it never
//! waits on a reply it would have to read itself. Every call has a deadline (the action timeout, else
//! 30 s); a dropped connection fails every waiter and marks every page closed.
//!
//! - A context is `Target.createBrowserContext { disposeOnDetach: true, proxyServer? }`: an
//!   isolated profile, dropped by the browser when this connection goes.
//! - Every page and out-of-process frame of a routed context has `Fetch.enable` at the request
//!   stage; each paused request is asked of the gate, then continued or failed `BlockedByClient`.
//! - New pages (the context's own and popups) arrive paused through the browser-level
//!   `Target.setAutoAttach { waitForDebuggerOnStart: true }`, are set up (Page, Runtime, Fetch, their
//!   own auto-attach for frames and workers), then run; pages of other contexts (the browser is
//!   shared) are run and detached at once.
//! - [`Browser::close`] only disconnects: it never sends `Browser.close`.
//!
//! Named differences from Playwright (core's executor runs Playwright in-process):
//! - No downloads: `Browser.setDownloadBehavior { behavior: 'deny' }` per context until a download
//!   directory shared with the browser container exists, so `on_download` never fires.
//! - WebSockets are not refused by the engine: Fetch does not intercept them. The gate given to
//!   `route_websocket` is kept for a future engine; the egress proxy, which every connection of a
//!   context goes through, is the boundary.
//! - Requests made by workers and service workers are not asked of the gate (nor are service
//!   workers blocked); again the egress proxy is the boundary.
//! - Selectors are CSS only (`document.querySelector`); Playwright's own engines (`text=`, `>>`,
//!   `:has-text()`) are not supported.
//! - Actions do not wait for actionability: `click`/`hover` scroll the element into view once and
//!   fail with "Element is not visible" on an empty box; `fill`/`select_option` fail at once.
//! - The mouse wheel is turned at (400, 300), the centre of the default 800x600 viewport.
//! - `set_input_files` hands the path to the browser, so the file must exist at the same path in
//!   the browser's container.
//! - The WebSocket address `/json/version` gives is used for its path only: the connection goes to
//!   the configured host and port. The Host header is the configured `host:port` when the host is an
//!   IP literal or `localhost`, else the resolved IP and port (Chromium's DevTools server refuses
//!   any other Host value).
//! - Dialogs are dismissed (`beforeunload` accepted), as Playwright does without a listener.
//!
//! [`engine`]: browser_task::engine

mod cdp;
mod http;
mod js;
mod keys;
mod ws;

use browser_task::engine::{
    Browser, CloseHook, Context, DownloadHook, EngineError, Handle, Launch, Page, PageHook, Proxy,
    Request, Result, Route, RouteGate, SocketGate,
};
use browser_task::Fail;
use cdp::{lock, timeout_error, Conn, CLOSED};
use js_json::JValue;
use std::collections::{HashMap, HashSet};
use std::net::TcpStream;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError, Weak};
use std::time::{Duration, Instant};

/// The environment variable naming the browser engine.
pub const ENGINE_ENV: &str = "NOEVIA_BROWSER_ENGINE";
/// A call's deadline when the trait gives none.
const CALL_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const POLL: Duration = Duration::from_millis(100);
/// One poll's call never gets less than this, even at the deadline.
const MIN_CALL: Duration = Duration::from_millis(500);

fn s(text: &str) -> JValue {
    JValue::Str(text.to_string())
}

fn num(n: impl Into<f64>) -> JValue {
    JValue::Num(n.into())
}

fn none() -> JValue {
    JValue::Obj(Vec::new())
}

fn closed() -> EngineError {
    EngineError::new(CLOSED)
}

fn auto_attach() -> JValue {
    JValue::obj([
        ("autoAttach", JValue::Bool(true)),
        ("waitForDebuggerOnStart", JValue::Bool(true)),
        ("flatten", JValue::Bool(true)),
    ])
}

/// An evaluation's exception as Playwright reports it: the first line of the thrown error, without
/// a plain `Error: ` prefix.
fn exception(reply: &JValue) -> Option<String> {
    let details = reply.get("exceptionDetails");
    if !details.is_object() {
        return None;
    }
    let e = details.get("exception");
    let text = e
        .get("description")
        .as_str()
        .or_else(|| e.get("value").as_str())
        .or_else(|| details.get("text").as_str());
    let first = text
        .unwrap_or("Evaluation failed")
        .lines()
        .next()
        .unwrap_or("");
    Some(first.strip_prefix("Error: ").unwrap_or(first).to_string())
}

/// [`Launch`] over `NOEVIA_BROWSER_ENGINE`.
pub struct CdpLaunch {
    endpoint: String,
}

impl CdpLaunch {
    pub fn new(endpoint: impl Into<String>) -> CdpLaunch {
        CdpLaunch {
            endpoint: endpoint.into(),
        }
    }

    /// From [`ENGINE_ENV`], when it is set and not empty.
    pub fn from_env() -> Option<CdpLaunch> {
        std::env::var(ENGINE_ENV)
            .ok()
            .filter(|v| !v.trim().is_empty())
            .map(CdpLaunch::new)
    }

    /// One connection to the engine.
    pub fn connect(&self) -> std::result::Result<Arc<CdpBrowser>, String> {
        let endpoint = http::parse_endpoint(&self.endpoint)?;
        let (addr, host) = endpoint.resolve()?;
        let unreachable =
            |e: std::io::Error| format!("The browser engine at {host} is unreachable ({e}).");
        let path = if endpoint.discover {
            let (status, body) =
                http::get(addr, &host, "/json/version", CONNECT_TIMEOUT).map_err(unreachable)?;
            if status != 200 {
                return Err(format!(
                    "The browser engine at {host} answered /json/version with HTTP {status}."
                ));
            }
            let found = http::version_ws_url(&body)?;
            let url = url::Url::parse(&found).map_err(|_| {
                "The browser engine's webSocketDebuggerUrl is not a URL.".to_string()
            })?;
            match url.query() {
                Some(q) => format!("{}?{q}", url.path()),
                None => url.path().to_string(),
            }
        } else {
            endpoint.path.clone()
        };
        let mut stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT).map_err(unreachable)?;
        let _ = stream.set_nodelay(true);
        stream
            .set_read_timeout(Some(CONNECT_TIMEOUT))
            .map_err(unreachable)?;
        stream
            .set_write_timeout(Some(CALL_TIMEOUT))
            .map_err(unreachable)?;
        ws::handshake(&mut stream, &host, &path).map_err(unreachable)?;
        stream.set_read_timeout(None).map_err(unreachable)?;
        let reader = stream.try_clone().map_err(unreachable)?;
        let conn = Conn::new(stream);
        let browser = Arc::new_cyclic(|me| CdpBrowser {
            me: me.clone(),
            conn: conn.clone(),
            sessions: Mutex::default(),
            contexts: Mutex::default(),
            targets: Mutex::default(),
        });
        let (events, gone) = (Arc::downgrade(&browser), Arc::downgrade(&browser));
        conn.start(
            reader,
            Box::new(move |session, method, params| {
                if let Some(b) = events.upgrade() {
                    b.event(session, method, params);
                }
            }),
            Box::new(move || {
                if let Some(b) = gone.upgrade() {
                    b.disconnected();
                }
            }),
        );
        conn.call(None, "Target.setAutoAttach", auto_attach(), CALL_TIMEOUT)
            .map_err(|e| e.message)?;
        Ok(browser)
    }
}

impl Launch for CdpLaunch {
    fn launch(&self) -> std::result::Result<Arc<dyn Browser>, Fail> {
        match self.connect() {
            Ok(b) => Ok(b),
            Err(e) => Err(Fail::plain(None, &e)),
        }
    }
}

/// Which context (and page, for a page) a session belongs to.
#[derive(Clone)]
struct Entry {
    ctx: Weak<CdpContext>,
    page: Option<Weak<CdpPage>>,
}

/// One connection to the shared browser.
pub struct CdpBrowser {
    me: Weak<CdpBrowser>,
    conn: Arc<Conn>,
    sessions: Mutex<HashMap<String, Entry>>,
    contexts: Mutex<HashMap<String, Weak<CdpContext>>>,
    /// Page targets attached here (a target is set up once, however it was reported).
    targets: Mutex<HashSet<String>>,
}

impl Drop for CdpBrowser {
    fn drop(&mut self) {
        self.conn.close();
    }
}

impl CdpBrowser {
    fn event(&self, session: Option<&str>, method: &str, params: &JValue) {
        match method {
            "Target.attachedToTarget" => self.attached(session, params),
            "Target.detachedFromTarget" => {
                if let Some(sid) = params.get("sessionId").as_str() {
                    self.detached(sid);
                }
            }
            "Target.targetDestroyed" | "Target.targetCrashed" => {
                if let Some(target) = params.get("targetId").as_str() {
                    self.destroyed(target);
                }
            }
            "Inspector.detached" => {
                if let Some(sid) = session {
                    self.detached(sid);
                }
            }
            _ => {
                if let Some(sid) = session {
                    self.session_event(sid, method, params);
                }
            }
        }
    }

    /// A target attached to this connection (paused until it is run).
    fn attached(&self, parent: Option<&str>, params: &JValue) {
        let Some(sid) = params.get("sessionId").as_str() else {
            return;
        };
        let info = params.get("targetInfo");
        let target = info.get("targetId").as_str().unwrap_or("").to_string();
        let kind = info.get("type").as_str().unwrap_or("");
        let ctx = match kind {
            "page" => {
                let id = info.get("browserContextId").as_str().unwrap_or("");
                let ctx = lock(&self.contexts).get(id).and_then(Weak::upgrade);
                ctx.filter(|c| {
                    !c.closed.load(Ordering::SeqCst) && lock(&self.targets).insert(target.clone())
                })
            }
            "iframe" => {
                let entry = parent.and_then(|p| lock(&self.sessions).get(p).cloned());
                entry.and_then(|e| e.ctx.upgrade())
            }
            _ => None,
        };
        let Some(ctx) = ctx else {
            // Not ours (another context of the shared browser), or a worker: let it run, let go.
            self.conn
                .send(Some(sid), "Runtime.runIfWaitingForDebugger", none());
            self.conn.send(
                parent,
                "Target.detachFromTarget",
                JValue::obj([("sessionId", s(sid))]),
            );
            return;
        };
        let sid = sid.to_string();
        if kind == "page" {
            std::thread::spawn(move || ctx.adopt(sid, target));
        } else {
            lock(&self.sessions).insert(
                sid.clone(),
                Entry {
                    ctx: Arc::downgrade(&ctx),
                    page: None,
                },
            );
            lock(&ctx.sessions).push(sid.clone());
            std::thread::spawn(move || ctx.setup_frame(&sid));
        }
    }

    fn detached(&self, sid: &str) {
        let entry = lock(&self.sessions).remove(sid);
        let Some(entry) = entry else { return };
        if let Some(ctx) = entry.ctx.upgrade() {
            lock(&ctx.sessions).retain(|s| s != sid);
        }
        if let Some(page) = entry.page.and_then(|p| p.upgrade()) {
            lock(&self.targets).remove(&page.target);
            page.mark_closed();
        }
    }

    fn destroyed(&self, target: &str) {
        let sid = lock(&self.sessions)
            .iter()
            .find(|(_, e)| {
                e.page
                    .as_ref()
                    .and_then(Weak::upgrade)
                    .is_some_and(|p| p.target == target)
            })
            .map(|(sid, _)| sid.clone());
        if let Some(sid) = sid {
            self.detached(&sid);
        }
    }

    fn disconnected(&self) {
        let entries: Vec<Entry> = lock(&self.sessions).drain().map(|(_, e)| e).collect();
        for page in entries
            .into_iter()
            .filter_map(|e| e.page.and_then(|p| p.upgrade()))
        {
            page.mark_closed();
        }
    }

    fn session_event(&self, sid: &str, method: &str, params: &JValue) {
        let entry = lock(&self.sessions).get(sid).cloned();
        let Some(entry) = entry else { return };
        match method {
            "Fetch.requestPaused" => self.paused(sid, &entry, params),
            "Fetch.authRequired" => self.auth(sid, &entry, params),
            "Page.javascriptDialogOpening" => {
                let accept = params.get("type").as_str() == Some("beforeunload");
                self.conn.send(
                    Some(sid),
                    "Page.handleJavaScriptDialog",
                    JValue::obj([("accept", JValue::Bool(accept))]),
                );
            }
            _ => {
                if let Some(page) = entry.page.as_ref().and_then(Weak::upgrade) {
                    page.event(method, params);
                }
            }
        }
    }

    /// `Fetch.requestPaused`: the gate decides, on this (the reader) thread.
    fn paused(&self, sid: &str, entry: &Entry, params: &JValue) {
        let Some(id) = params.get("requestId").as_str() else {
            return;
        };
        let request = params.get("request");
        let url = format!(
            "{}{}",
            request.get("url").as_str().unwrap_or(""),
            request.get("urlFragment").as_str().unwrap_or("")
        );
        let page = entry.page.as_ref().and_then(Weak::upgrade);
        let frame = params.get("frameId").as_str();
        let main_frame_navigation = params.get("resourceType").as_str() == Some("Document")
            && page.is_some_and(|p| frame.is_some_and(|f| f == lock(&p.state).main_frame));
        let gate = entry.ctx.upgrade().map(|c| lock(&c.gate).clone());
        let route = match gate {
            Some(Some(gate)) => gate(&Request {
                url,
                main_frame_navigation,
            }),
            Some(None) => Route::Continue,
            None => Route::Abort,
        };
        match route {
            Route::Continue => self.conn.send(
                Some(sid),
                "Fetch.continueRequest",
                JValue::obj([("requestId", s(id))]),
            ),
            Route::Abort => self.conn.send(
                Some(sid),
                "Fetch.failRequest",
                JValue::obj([("requestId", s(id)), ("errorReason", s("BlockedByClient"))]),
            ),
        }
    }

    /// `Fetch.authRequired`: the context's proxy credentials for its proxy, nothing for anyone else.
    fn auth(&self, sid: &str, entry: &Entry, params: &JValue) {
        let Some(id) = params.get("requestId").as_str() else {
            return;
        };
        let proxy = entry.ctx.upgrade().and_then(|c| c.proxy.clone());
        let response = match proxy {
            Some(p) if params.get("authChallenge").get("source").as_str() == Some("Proxy") => {
                JValue::obj([
                    ("response", s("ProvideCredentials")),
                    ("username", s(&p.username)),
                    ("password", s(&p.password)),
                ])
            }
            _ => JValue::obj([("response", s("CancelAuth"))]),
        };
        self.conn.send(
            Some(sid),
            "Fetch.continueWithAuth",
            JValue::obj([("requestId", s(id)), ("authChallengeResponse", response)]),
        );
    }
}

impl Browser for CdpBrowser {
    fn new_context(&self, proxy: Option<&Proxy>) -> Result<Arc<dyn Context>> {
        let browser = self.me.upgrade().ok_or_else(closed)?;
        let mut params = vec![("disposeOnDetach", JValue::Bool(true))];
        if let Some(p) = proxy {
            params.push(("proxyServer", s(&p.server)));
        }
        let made = self.conn.call(
            None,
            "Target.createBrowserContext",
            JValue::obj(params),
            CALL_TIMEOUT,
        )?;
        let id = made
            .get("browserContextId")
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| EngineError::new("No browser context was created."))?;
        let deny = JValue::obj([("behavior", s("deny")), ("browserContextId", s(&id))]);
        if let Err(e) = self
            .conn
            .call(None, "Browser.setDownloadBehavior", deny, CALL_TIMEOUT)
        {
            self.conn.send(
                None,
                "Target.disposeBrowserContext",
                JValue::obj([("browserContextId", s(&id))]),
            );
            return Err(e);
        }
        let ctx = Arc::new_cyclic(|me| CdpContext {
            me: me.clone(),
            browser,
            id: id.clone(),
            proxy: proxy.cloned(),
            gate: Mutex::new(None),
            socket_gate: Mutex::new(None),
            hooks: Mutex::default(),
            pages: Mutex::default(),
            sessions: Mutex::default(),
            created: Mutex::default(),
            created_changed: Condvar::new(),
            closed: AtomicBool::new(false),
        });
        lock(&self.contexts).insert(id, Arc::downgrade(&ctx));
        Ok(ctx)
    }

    /// Disconnects only: the browser is shared, and the contexts go with this connection.
    fn close(&self) -> Result<()> {
        self.conn.close();
        Ok(())
    }
}

/// An isolated browser context.
pub struct CdpContext {
    me: Weak<CdpContext>,
    browser: Arc<CdpBrowser>,
    id: String,
    proxy: Option<Proxy>,
    gate: Mutex<Option<RouteGate>>,
    /// Kept for a future engine: CDP's Fetch does not see WebSockets (a named difference).
    #[allow(dead_code)]
    socket_gate: Mutex<Option<SocketGate>>,
    hooks: Mutex<Vec<PageHook>>,
    pages: Mutex<Vec<Arc<CdpPage>>>,
    /// Every session of the context (pages and frames), for `Fetch.enable`.
    sessions: Mutex<Vec<String>>,
    /// Targets [`Context::new_page`] created, and the session each attached as.
    created: Mutex<HashMap<String, Option<String>>>,
    created_changed: Condvar,
    closed: AtomicBool,
}

impl CdpContext {
    fn conn(&self) -> &Conn {
        &self.browser.conn
    }

    fn fetch_params(&self) -> JValue {
        let pattern = JValue::obj([("urlPattern", s("*")), ("requestStage", s("Request"))]);
        JValue::obj([
            ("patterns", JValue::Arr(vec![pattern])),
            ("handleAuthRequests", JValue::Bool(self.proxy.is_some())),
        ])
    }

    fn hooks(&self) -> Vec<PageHook> {
        lock(&self.hooks).clone()
    }

    /// A page target of this context attached: [`Context::new_page`]'s own, or a popup.
    fn adopt(self: Arc<Self>, sid: String, target: String) {
        {
            let mut created = lock(&self.created);
            if let Some(slot) = created.get_mut(&target) {
                *slot = Some(sid);
                self.created_changed.notify_all();
                return;
            }
        }
        if let Ok(page) = self.open_page(sid, target) {
            for hook in self.hooks() {
                hook(page.clone());
            }
        }
    }

    /// Page, Runtime, Fetch (when routed) and auto-attach on a new page, then it runs.
    fn open_page(&self, sid: String, target: String) -> Result<Arc<CdpPage>> {
        let page = CdpPage::new(self.browser.conn.clone(), sid.clone(), target);
        lock(&self.browser.sessions).insert(
            sid.clone(),
            Entry {
                ctx: self.me.clone(),
                page: Some(Arc::downgrade(&page)),
            },
        );
        lock(&self.sessions).push(sid.clone());
        let setup = (|| -> Result<()> {
            page.call("Page.enable", none(), CALL_TIMEOUT)?;
            page.call("Runtime.enable", none(), CALL_TIMEOUT)?;
            let routed = lock(&self.gate).is_some();
            if routed {
                page.call("Fetch.enable", self.fetch_params(), CALL_TIMEOUT)?;
            }
            page.call("Target.setAutoAttach", auto_attach(), CALL_TIMEOUT)?;
            let tree = page.call("Page.getFrameTree", none(), CALL_TIMEOUT)?;
            page.set_frame(tree.get("frameTree").get("frame"));
            page.call("Runtime.runIfWaitingForDebugger", none(), CALL_TIMEOUT)?;
            Ok(())
        })();
        if let Err(e) = setup {
            self.conn()
                .send(Some(&sid), "Runtime.runIfWaitingForDebugger", none());
            return Err(e);
        }
        lock(&self.pages).push(page.clone());
        Ok(page)
    }

    /// An out-of-process frame: routed like its page, then run.
    fn setup_frame(&self, sid: &str) {
        let routed = lock(&self.gate).is_some();
        if routed {
            let _ = self
                .conn()
                .call(Some(sid), "Fetch.enable", self.fetch_params(), CALL_TIMEOUT);
        }
        let _ = self.conn().call(
            Some(sid),
            "Target.setAutoAttach",
            auto_attach(),
            CALL_TIMEOUT,
        );
        self.conn()
            .send(Some(sid), "Runtime.runIfWaitingForDebugger", none());
    }
}

impl Context for CdpContext {
    fn route(&self, gate: RouteGate) -> Result<()> {
        *lock(&self.gate) = Some(gate);
        let sessions = lock(&self.sessions).clone();
        for sid in sessions {
            self.conn().call(
                Some(&sid),
                "Fetch.enable",
                self.fetch_params(),
                CALL_TIMEOUT,
            )?;
        }
        Ok(())
    }

    fn route_websocket(&self, gate: SocketGate) -> Result<()> {
        *lock(&self.socket_gate) = Some(gate);
        Ok(())
    }

    fn on_page(&self, hook: PageHook) {
        lock(&self.hooks).push(hook);
    }

    fn new_page(&self) -> Result<Arc<dyn Page>> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(closed());
        }
        let mut created = lock(&self.created);
        let params = JValue::obj([("url", s("about:blank")), ("browserContextId", s(&self.id))]);
        let made = self
            .conn()
            .call(None, "Target.createTarget", params, CALL_TIMEOUT)?;
        let target = made
            .get("targetId")
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| EngineError::new("No page was created."))?;
        created.insert(target.clone(), None);
        // Its attachment arrives on the reader thread, maybe before the reply did.
        let deadline = Instant::now() + CALL_TIMEOUT;
        let sid = loop {
            if let Some(Some(sid)) = created.get(&target) {
                break Some(sid.clone());
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() || self.conn().is_dead() {
                break None;
            }
            created = self
                .created_changed
                .wait_timeout(created, left.min(POLL))
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        };
        created.remove(&target);
        drop(created);
        let sid = sid.ok_or_else(|| {
            if self.conn().is_dead() {
                closed()
            } else {
                timeout_error(CALL_TIMEOUT)
            }
        })?;
        let page = self.open_page(sid, target)?;
        // The executor relies on its hook having run before it gets the page.
        for hook in self.hooks() {
            hook(page.clone());
        }
        Ok(page)
    }

    fn pages(&self) -> Vec<Arc<dyn Page>> {
        lock(&self.pages)
            .iter()
            .filter(|p| !p.is_closed())
            .map(|p| p.clone() as Arc<dyn Page>)
            .collect()
    }

    fn close(&self) -> Result<()> {
        if self.closed.load(Ordering::SeqCst) {
            return Ok(());
        }
        let params = JValue::obj([("browserContextId", s(&self.id))]);
        self.conn()
            .call(None, "Target.disposeBrowserContext", params, CALL_TIMEOUT)?;
        self.closed.store(true, Ordering::SeqCst);
        lock(&self.browser.contexts).remove(&self.id);
        let sessions = lock(&self.sessions).clone();
        for sid in sessions {
            self.browser.detached(&sid);
        }
        Ok(())
    }
}

#[derive(Default)]
struct PageState {
    url: String,
    main_frame: String,
    /// The main frame's committed document, and whether its `load` fired.
    loader: String,
    loaded: bool,
    commits: u64,
    same_document: u64,
    /// A main-frame navigation started and has not committed or stopped yet.
    starting: bool,
    closed: bool,
}

static NEXT_PAGE: AtomicU64 = AtomicU64::new(1);

/// One page (a `page` target and its session).
pub struct CdpPage {
    me: Weak<CdpPage>,
    id: u64,
    conn: Arc<Conn>,
    session: String,
    target: String,
    state: Mutex<PageState>,
    changed: Condvar,
    close_hooks: Mutex<Vec<CloseHook>>,
    /// Never called: downloads are denied (a named difference).
    #[allow(dead_code)]
    download_hooks: Mutex<Vec<DownloadHook>>,
}

impl CdpPage {
    fn new(conn: Arc<Conn>, session: String, target: String) -> Arc<CdpPage> {
        let state = PageState {
            url: "about:blank".into(),
            main_frame: target.clone(),
            ..PageState::default()
        };
        Arc::new_cyclic(|me| CdpPage {
            me: me.clone(),
            id: NEXT_PAGE.fetch_add(1, Ordering::SeqCst),
            conn,
            session,
            target,
            state: Mutex::new(state),
            changed: Condvar::new(),
            close_hooks: Mutex::default(),
            download_hooks: Mutex::default(),
        })
    }

    fn call(&self, method: &str, params: JValue, timeout: Duration) -> Result<JValue> {
        if self.is_closed() {
            return Err(closed());
        }
        self.conn.call(Some(&self.session), method, params, timeout)
    }

    fn evaluate(&self, expression: &str, by_value: bool, timeout: Duration) -> Result<JValue> {
        let params = JValue::obj([
            ("expression", s(expression)),
            ("returnByValue", JValue::Bool(by_value)),
            ("awaitPromise", JValue::Bool(true)),
        ]);
        self.call("Runtime.evaluate", params, timeout)
    }

    fn set_frame(&self, frame: &JValue) {
        let mut st = lock(&self.state);
        if let Some(id) = frame.get("id").as_str() {
            st.main_frame = id.to_string();
        }
        if let Some(url) = frame.get("url").as_str().filter(|u| !u.is_empty()) {
            st.url = format!("{url}{}", frame.get("urlFragment").as_str().unwrap_or(""));
        }
        if let Some(loader) = frame.get("loaderId").as_str() {
            st.loader = loader.to_string();
        }
    }

    fn mark_closed(&self) {
        {
            let mut st = lock(&self.state);
            if st.closed {
                return;
            }
            st.closed = true;
        }
        self.changed.notify_all();
        let hooks = std::mem::take(&mut *lock(&self.close_hooks));
        for hook in hooks {
            hook();
        }
    }

    fn event(&self, method: &str, params: &JValue) {
        {
            let mut st = lock(&self.state);
            let main = st.main_frame.clone();
            let is_main = |id: &JValue| id.as_str() == Some(main.as_str());
            match method {
                "Page.frameNavigated" => {
                    let frame = params.get("frame");
                    if !frame.get("parentId").is_nullish() {
                        return;
                    }
                    st.main_frame = frame.get("id").as_str().unwrap_or("").to_string();
                    st.url = format!(
                        "{}{}",
                        frame.get("url").as_str().unwrap_or(""),
                        frame.get("urlFragment").as_str().unwrap_or("")
                    );
                    st.loader = frame.get("loaderId").as_str().unwrap_or("").to_string();
                    st.loaded = params.get("type").as_str() == Some("BackForwardCacheRestore");
                    st.commits += 1;
                    st.starting = false;
                }
                "Page.navigatedWithinDocument" if is_main(params.get("frameId")) => {
                    st.url = params.get("url").as_str().unwrap_or("").to_string();
                    st.same_document += 1;
                }
                "Page.loadEventFired" => st.loaded = true,
                "Page.frameStartedLoading" if is_main(params.get("frameId")) => st.starting = true,
                "Page.frameStoppedLoading" if is_main(params.get("frameId")) => st.starting = false,
                _ => return,
            }
        }
        self.changed.notify_all();
    }

    /// Waits until `done` holds, the page closes, or the deadline.
    fn wait_until(
        &self,
        deadline: Instant,
        timeout: Duration,
        done: impl Fn(&PageState) -> bool,
    ) -> Result<()> {
        let mut st = lock(&self.state);
        loop {
            if st.closed {
                return Err(closed());
            }
            if done(&st) {
                return Ok(());
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(timeout_error(timeout));
            }
            st = self
                .changed
                .wait_timeout(st, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    /// Calls `step` every 100 ms until it answers or the deadline passes (`None`).
    fn poll<T>(
        &self,
        timeout: Duration,
        mut step: impl FnMut(Duration) -> Option<Result<T>>,
    ) -> Option<Result<T>> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(done) = step(
                deadline
                    .saturating_duration_since(Instant::now())
                    .max(MIN_CALL),
            ) {
                return Some(done);
            }
            if self.is_closed() {
                return Some(Err(closed()));
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return None;
            }
            std::thread::sleep(POLL.min(left));
        }
    }

    fn handle(&self, reply: &JValue) -> Option<Arc<dyn Handle>> {
        let object = reply.get("result").get("objectId").as_str()?;
        let page = self.me.upgrade()?;
        Some(Arc::new(CdpHandle {
            page,
            object: object.to_string(),
        }))
    }

    fn text(&self, selector: &str, timeout: Duration) -> Result<String> {
        let expression = js::inner_text(selector);
        let found = self.poll(timeout, |left| {
            match self.evaluate(&expression, true, left) {
                Err(_) => None,
                Ok(r) => match exception(&r) {
                    Some(m) => Some(Err(EngineError::new(m))),
                    None => r
                        .get("result")
                        .get("value")
                        .get("text")
                        .as_str()
                        .map(|t| Ok(t.to_string())),
                },
            }
        });
        found.unwrap_or_else(|| Err(timeout_error(timeout)))
    }

    fn press_key(&self, name: &str, timeout: Duration) -> Result<()> {
        let key = keys::lookup(name)
            .ok_or_else(|| EngineError::new(format!("Unknown key: \"{name}\"")))?;
        let mut down = vec![
            (
                "type",
                s(if key.text.is_some() {
                    "keyDown"
                } else {
                    "rawKeyDown"
                }),
            ),
            ("modifiers", num(0)),
            ("windowsVirtualKeyCode", num(key.key_code)),
            ("code", s(key.code)),
            ("key", s(&key.key)),
            ("autoRepeat", JValue::Bool(false)),
            ("location", num(0)),
            ("isKeypad", JValue::Bool(false)),
        ];
        if let Some(text) = &key.text {
            down.push(("text", s(text)));
            down.push(("unmodifiedText", s(text)));
        }
        self.call("Input.dispatchKeyEvent", JValue::obj(down), timeout)?;
        let up = JValue::obj([
            ("type", s("keyUp")),
            ("modifiers", num(0)),
            ("windowsVirtualKeyCode", num(key.key_code)),
            ("code", s(key.code)),
            ("key", s(&key.key)),
            ("location", num(0)),
        ]);
        self.call("Input.dispatchKeyEvent", up, timeout).map(|_| ())
    }

    fn mouse(
        &self,
        kind: &str,
        (x, y): (f64, f64),
        button: &str,
        buttons: u32,
        timeout: Duration,
    ) -> Result<()> {
        let mut params = vec![
            ("type", s(kind)),
            ("x", num(x)),
            ("y", num(y)),
            ("button", s(button)),
            ("buttons", num(buttons)),
            ("modifiers", num(0)),
        ];
        if kind != "mouseMoved" {
            params.push(("clickCount", num(1)));
        }
        self.call("Input.dispatchMouseEvent", JValue::obj(params), timeout)
            .map(|_| ())
    }
}

impl Page for CdpPage {
    fn id(&self) -> u64 {
        self.id
    }

    /// Never fires: downloads are denied (see the crate's named differences).
    fn on_download(&self, hook: DownloadHook) {
        lock(&self.download_hooks).push(hook);
    }

    fn on_close(&self, hook: CloseHook) {
        lock(&self.close_hooks).push(hook);
    }

    fn is_closed(&self) -> bool {
        lock(&self.state).closed
    }

    fn url(&self) -> String {
        lock(&self.state).url.clone()
    }

    fn locate(&self, selector: &str, timeout: Duration) -> Result<Option<Arc<dyn Handle>>> {
        let expression = js::query(selector);
        let found = self.poll(timeout, |left| {
            match self.evaluate(&expression, false, left) {
                // A navigation in flight: the document is not there yet.
                Err(_) => None,
                Ok(r) => match exception(&r) {
                    Some(m) => Some(Err(EngineError::new(m))),
                    None => self.handle(&r).map(Ok),
                },
            }
        });
        found.transpose()
    }

    fn active_element(&self) -> Result<Option<Arc<dyn Handle>>> {
        let r = self.evaluate("document.activeElement", false, CALL_TIMEOUT)?;
        match exception(&r) {
            Some(m) => Err(EngineError::new(m)),
            None => Ok(self.handle(&r)),
        }
    }

    fn locator_text(&self, selector: &str, timeout: Duration) -> Result<String> {
        self.text(selector, timeout)
    }

    fn body_text(&self, timeout: Duration) -> Result<String> {
        self.text("body", timeout)
    }

    fn goto(&self, url: &str, timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        let r = self.call("Page.navigate", JValue::obj([("url", s(url))]), timeout)?;
        if let Some(error) = r.get("errorText").as_str().filter(|e| !e.is_empty()) {
            return Err(EngineError::new(error));
        }
        // No loader: a same-document navigation, already done.
        let Some(loader) = r.get("loaderId").as_str().filter(|l| !l.is_empty()) else {
            return Ok(());
        };
        self.wait_until(deadline, timeout, |st| st.loader == loader && st.loaded)
    }

    fn go_back(&self, timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        let history = self.call("Page.getNavigationHistory", none(), timeout)?;
        let index = history.get("currentIndex").as_num().unwrap_or(0.0);
        let entries = match history.get("entries") {
            JValue::Arr(entries) => entries.as_slice(),
            _ => &[],
        };
        if index < 1.0 {
            return Ok(());
        }
        let Some(entry) = entries
            .get(index as usize - 1)
            .and_then(|e| e.get("id").as_num())
        else {
            return Ok(());
        };
        let (commits, same) = {
            let st = lock(&self.state);
            (st.commits, st.same_document)
        };
        self.call(
            "Page.navigateToHistoryEntry",
            JValue::obj([("entryId", num(entry))]),
            timeout,
        )?;
        self.wait_until(deadline, timeout, |st| {
            (st.commits > commits && st.loaded) || st.same_document > same
        })
    }

    fn keyboard_press(&self, key: &str) -> Result<()> {
        self.press_key(key, CALL_TIMEOUT)
    }

    fn wheel(&self, dy: f64) -> Result<()> {
        let params = JValue::obj([
            ("type", s("mouseWheel")),
            ("x", num(400)),
            ("y", num(300)),
            ("deltaX", num(0)),
            ("deltaY", num(dy)),
        ]);
        self.call("Input.dispatchMouseEvent", params, CALL_TIMEOUT)
            .map(|_| ())
    }

    fn wait_for_timeout(&self, ms: f64) -> Result<()> {
        let ms = if ms.is_finite() && ms > 0.0 {
            ms.min(86_400_000.0)
        } else {
            0.0
        };
        std::thread::sleep(Duration::from_secs_f64(ms / 1000.0));
        Ok(())
    }

    fn screenshot(&self) -> Result<Vec<u8>> {
        use base64::Engine as _;
        let r = self.call(
            "Page.captureScreenshot",
            JValue::obj([("format", s("png"))]),
            CALL_TIMEOUT,
        )?;
        let data = r
            .get("data")
            .as_str()
            .ok_or_else(|| EngineError::new("No screenshot was taken."))?;
        base64::engine::general_purpose::STANDARD
            .decode(data)
            .map_err(|_| EngineError::new("The screenshot is not base64."))
    }

    fn wait_for_load_state(&self, timeout: Duration) -> Result<()> {
        let found = self.poll(timeout, |left| {
            if lock(&self.state).starting {
                return None;
            }
            match self.evaluate("document.readyState", true, left) {
                Ok(r)
                    if r.get("result")
                        .get("value")
                        .as_str()
                        .is_some_and(|v| v != "loading") =>
                {
                    Some(Ok(()))
                }
                _ => None,
            }
        });
        found.unwrap_or_else(|| Err(timeout_error(timeout)))
    }
}

/// One element (a remote object of the page's main world).
struct CdpHandle {
    page: Arc<CdpPage>,
    object: String,
}

impl CdpHandle {
    /// `function(element, ...args)` in the page, its value returned by value.
    fn run(&self, function: &str, args: Vec<JValue>, timeout: Duration) -> Result<JValue> {
        let mut arguments = vec![JValue::obj([("objectId", s(&self.object))])];
        arguments.extend(args.into_iter().map(|v| JValue::obj([("value", v)])));
        let params = JValue::obj([
            ("functionDeclaration", s(function)),
            ("objectId", s(&self.object)),
            ("arguments", JValue::Arr(arguments)),
            ("returnByValue", JValue::Bool(true)),
            ("awaitPromise", JValue::Bool(true)),
        ]);
        let r = self.page.call("Runtime.callFunctionOn", params, timeout)?;
        match exception(&r) {
            Some(m) => Err(EngineError::new(m)),
            None => Ok(r.get("result").get("value").clone()),
        }
    }

    /// Scrolled into view: the centre of its box, refused when the box is empty.
    fn centre(&self, timeout: Duration) -> Result<(f64, f64)> {
        let r = self.run(js::SCROLL_AND_MEASURE, Vec::new(), timeout)?;
        let n = |k: &str| r.get(k).as_num().unwrap_or(0.0);
        if !(n("width") > 0.0 && n("height") > 0.0) {
            return Err(EngineError::new("Element is not visible"));
        }
        Ok((n("x"), n("y")))
    }

    fn flag(&self, function: &str) -> Result<bool> {
        Ok(self.run(function, Vec::new(), CALL_TIMEOUT)?.as_bool() == Some(true))
    }
}

impl Handle for CdpHandle {
    fn describe(&self) -> Result<JValue> {
        self.run(js::DESCRIBE_IN_PAGE, Vec::new(), CALL_TIMEOUT)
    }

    fn is_connected(&self) -> Result<bool> {
        self.flag(js::IS_CONNECTED)
    }

    fn is_focused(&self) -> Result<bool> {
        self.flag(js::IS_FOCUSED)
    }

    fn click(&self, timeout: Duration) -> Result<()> {
        let at = self.centre(timeout)?;
        self.page.mouse("mouseMoved", at, "none", 0, timeout)?;
        self.page.mouse("mousePressed", at, "left", 1, timeout)?;
        self.page.mouse("mouseReleased", at, "left", 0, timeout)
    }

    fn hover(&self, timeout: Duration) -> Result<()> {
        let at = self.centre(timeout)?;
        self.page.mouse("mouseMoved", at, "none", 0, timeout)
    }

    fn fill(&self, value: &str, timeout: Duration) -> Result<()> {
        let how = self.run(js::FILL, vec![s(value)], timeout)?;
        if how.as_str() == Some("type") && !value.is_empty() {
            self.page.call(
                "Input.insertText",
                JValue::obj([("text", s(value))]),
                timeout,
            )?;
        }
        Ok(())
    }

    fn select_option(&self, value: &str, timeout: Duration) -> Result<()> {
        self.run(js::SELECT_OPTION, vec![s(value)], timeout)
            .map(|_| ())
    }

    fn press(&self, key: &str, timeout: Duration) -> Result<()> {
        if keys::lookup(key).is_none() {
            return Err(EngineError::new(format!("Unknown key: \"{key}\"")));
        }
        self.run(js::FOCUS, Vec::new(), timeout)?;
        self.page.press_key(key, timeout)
    }

    fn set_input_files(&self, file: &Path, timeout: Duration) -> Result<()> {
        let path = file
            .to_str()
            .ok_or_else(|| EngineError::new("The file path is not UTF-8."))?;
        let params = JValue::obj([
            ("files", JValue::Arr(vec![s(path)])),
            ("objectId", s(&self.object)),
        ]);
        self.page
            .call("DOM.setFileInputFiles", params, timeout)
            .map(|_| ())
    }

    fn submit(&self) -> Result<()> {
        self.run(js::SUBMIT, Vec::new(), CALL_TIMEOUT).map(|_| ())
    }

    fn dispose(&self) -> Result<()> {
        self.page
            .call(
                "Runtime.releaseObject",
                JValue::obj([("objectId", s(&self.object))]),
                CALL_TIMEOUT,
            )
            .map(|_| ())
    }
}
