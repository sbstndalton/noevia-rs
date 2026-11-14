//! The browser engine, shaped like the Playwright calls core's browser-executor.cjs makes, so the
//! executor ports its order and its checks one for one. An engine only carries actions out: it
//! never decides what may run (the executor asks crate browser-policy first, and every request the
//! page makes goes through the executor's [`RouteGate`]).
//!
//! Every call blocks until the engine answers. A failure is an [`EngineError`] whose message is what
//! the executor reports (its first line, at most 200 UTF-16 units).

use js_json::JValue;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

/// A failed engine call (Playwright's thrown `Error`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineError {
    pub message: String,
}

impl EngineError {
    pub fn new(message: impl Into<String>) -> EngineError {
        EngineError {
            message: message.into(),
        }
    }
}

pub type Result<T> = std::result::Result<T, EngineError>;

/// The task's egress grant as the context's proxy (`{ server, username, password }`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proxy {
    pub server: String,
    pub username: String,
    pub password: String,
}

/// One request the page makes (Playwright's `route.request()`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub url: String,
    /// `req.isNavigationRequest() && req.frame() === req.frame().page().mainFrame()`.
    pub main_frame_navigation: bool,
}

/// What the gate answers for a request: `route.continue()` or `route.abort('blockedbyclient')`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    Continue,
    Abort,
}

/// What the gate answers for a WebSocket: connect it, or close it with this code and reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Socket {
    Connect,
    Close { code: u16, reason: String },
}

pub type RouteGate = Arc<dyn Fn(&Request) -> Route + Send + Sync>;
pub type SocketGate = Arc<dyn Fn(&str) -> Socket + Send + Sync>;
pub type PageHook = Arc<dyn Fn(Arc<dyn Page>) + Send + Sync>;
pub type DownloadHook = Arc<dyn Fn(Arc<dyn Download>) + Send + Sync>;
pub type CloseHook = Arc<dyn Fn() + Send + Sync>;

/// `launch()`: one fresh browser per executor.
pub trait Launch: Send + Sync {
    fn launch(&self) -> std::result::Result<Arc<dyn Browser>, crate::Fail>;
}

pub trait Browser: Send + Sync {
    /// `browser.newContext({ acceptDownloads: true, serviceWorkers: 'block', proxy? })`: an isolated
    /// profile (no cookies, storage or credentials from anything else).
    fn new_context(&self, proxy: Option<&Proxy>) -> Result<Arc<dyn Context>>;
    /// `browser.close()`.
    fn close(&self) -> Result<()>;
}

pub trait Context: Send + Sync {
    /// `context.route('**/*', …)`: every request the context makes is asked.
    fn route(&self, gate: RouteGate) -> Result<()>;
    /// `context.routeWebSocket(/.*/, …)`.
    fn route_websocket(&self, gate: SocketGate) -> Result<()>;
    /// `context.on('page', …)`: called for every page the context opens (popups too), before
    /// [`Context::new_page`] returns its page.
    fn on_page(&self, hook: PageHook);
    fn new_page(&self) -> Result<Arc<dyn Page>>;
    /// `context.pages()`.
    fn pages(&self) -> Vec<Arc<dyn Page>>;
    /// `context.close()`.
    fn close(&self) -> Result<()>;
}

pub trait Page: Send + Sync {
    /// Identity, for `session.page !== page`.
    fn id(&self) -> u64;
    fn on_download(&self, hook: DownloadHook);
    fn on_close(&self, hook: CloseHook);
    fn is_closed(&self) -> bool;
    fn url(&self) -> String;
    /// `page.locator(selector).first().elementHandle({ timeout })`: `None` when there is none.
    fn locate(&self, selector: &str, timeout: Duration) -> Result<Option<Arc<dyn Handle>>>;
    /// `page.evaluateHandle(() => document.activeElement)` as an element.
    fn active_element(&self) -> Result<Option<Arc<dyn Handle>>>;
    /// `page.locator(selector).first().innerText({ timeout })`.
    fn locator_text(&self, selector: &str, timeout: Duration) -> Result<String>;
    /// `page.innerText('body', { timeout })`.
    fn body_text(&self, timeout: Duration) -> Result<String>;
    fn goto(&self, url: &str, timeout: Duration) -> Result<()>;
    fn go_back(&self, timeout: Duration) -> Result<()>;
    /// `page.keyboard.press(key)`.
    fn keyboard_press(&self, key: &str) -> Result<()>;
    /// `page.mouse.wheel(0, dy)`.
    fn wheel(&self, dy: f64) -> Result<()>;
    fn wait_for_timeout(&self, ms: f64) -> Result<()>;
    /// `page.screenshot({ type: 'png' })`.
    fn screenshot(&self) -> Result<Vec<u8>>;
    /// `page.waitForLoadState('domcontentloaded', { timeout })`.
    fn wait_for_load_state(&self, timeout: Duration) -> Result<()>;
}

/// One element the executor holds (an `ElementHandle`).
pub trait Handle: Send + Sync {
    /// `handle.evaluate(describeInPage)`: the facts of the element that will really act
    /// (core browser-executor.cjs `describeInPage`, which a real engine runs in the page as is).
    fn describe(&self) -> Result<JValue>;
    /// `handle.evaluate((el) => el.isConnected)`.
    fn is_connected(&self) -> Result<bool>;
    /// `handle.evaluate((el) => el === document.activeElement)`.
    fn is_focused(&self) -> Result<bool>;
    fn click(&self, timeout: Duration) -> Result<()>;
    fn hover(&self, timeout: Duration) -> Result<()>;
    fn fill(&self, value: &str, timeout: Duration) -> Result<()>;
    fn select_option(&self, value: &str, timeout: Duration) -> Result<()>;
    fn press(&self, key: &str, timeout: Duration) -> Result<()>;
    fn set_input_files(&self, file: &Path, timeout: Duration) -> Result<()>;
    /// `handle.evaluate((el) => { const f = el.form || el.closest('form') || el; f.requestSubmit ? f.requestSubmit() : f.submit(); })`.
    fn submit(&self) -> Result<()>;
    fn dispose(&self) -> Result<()>;
}

/// A download the page started (Playwright's `Download`).
pub trait Download: Send + Sync {
    fn suggested_filename(&self) -> String;
    fn url(&self) -> String;
    fn save_as(&self, target: &Path) -> Result<()>;
}
