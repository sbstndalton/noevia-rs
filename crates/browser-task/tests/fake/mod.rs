//! The scripted fake browser of tools/gen-service-fixtures.cjs (handoff spec
//! `R5-browser-fixture-spec.md`), behind crate browser-task's engine traits: the same synthetic
//! pages, the same answers and the same call log.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    dead_code
)]

use browser_task::engine::{
    Browser, CloseHook, Context, Download, DownloadHook, EngineError, Handle, Launch, Page,
    PageHook, Proxy, Request, Result, Route, RouteGate, Socket, SocketGate,
};
use browser_task::Fail;
use js_json::JValue;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub type Log = Arc<Mutex<Vec<JValue>>>;

fn s(t: &str) -> JValue {
    JValue::Str(t.to_string())
}

pub fn entry(parts: Vec<JValue>) -> JValue {
    JValue::Arr(parts)
}

/// The world: `{ pages: { url: Page } }`, changed by the `mutate` op.
pub struct World {
    pub pages: JValue,
}

impl World {
    pub fn page(&self, url: &str) -> JValue {
        let p = self.pages.get(url);
        if p.is_object() {
            p.clone()
        } else {
            JValue::obj([("text", s("")), ("elements", JValue::obj::<&str>([]))])
        }
    }

    pub fn element(&self, url: &str, selector: &str) -> Option<JValue> {
        let e = self.page(url).get("elements").get(selector).clone();
        e.is_object().then_some(e)
    }

    /// `{ page, selector, facts?, remove? }`.
    pub fn mutate(&mut self, op: &JValue) {
        let (url, sel) = (
            op.get("page").as_str().unwrap().to_string(),
            op.get("selector").as_str().unwrap().to_string(),
        );
        let JValue::Obj(pages) = &mut self.pages else {
            panic!("pages")
        };
        let page = &mut pages.iter_mut().find(|(k, _)| *k == url).expect("page").1;
        let JValue::Obj(fields) = page else {
            panic!("page")
        };
        let elements = &mut fields
            .iter_mut()
            .find(|(k, _)| k == "elements")
            .expect("elements")
            .1;
        let JValue::Obj(els) = elements else {
            panic!("elements")
        };
        if op.get("remove").as_bool() == Some(true) {
            els.retain(|(k, _)| *k != sel);
            return;
        }
        let el = &mut els.iter_mut().find(|(k, _)| *k == sel).expect("element").1;
        let JValue::Obj(ef) = el else {
            panic!("element")
        };
        if op.get("facts").is_object() {
            js_json::wtf16::set(ef, "facts", op.get("facts").clone());
        }
        if !matches!(op.get("focused"), JValue::Undefined) {
            js_json::wtf16::set(ef, "focused", op.get("focused").clone());
        }
    }
}

pub struct Fake {
    pub log: Log,
    pub world: Arc<Mutex<World>>,
    pub launch_fails: Option<JValue>,
}

impl Fake {
    fn push(&self, parts: Vec<JValue>) {
        self.log.lock().unwrap().push(entry(parts));
    }
}

impl Fake {
    fn start(self: &Arc<Self>) -> std::result::Result<Arc<dyn Browser>, Fail> {
        self.push(vec![s("launch")]);
        if let Some(f) = &self.launch_fails {
            return Err(Fail {
                status: f.get("status").as_num().map(|n| n as u16),
                message: f.get("message").as_str().unwrap_or("").to_string(),
                public: f.get("publicMessage").as_str().map(str::to_string),
                result: None,
            });
        }
        Ok(Arc::new(FakeBrowser { fake: self.clone() }))
    }
}

pub struct Launcher(pub Arc<Fake>);

impl Launch for Launcher {
    fn launch(&self) -> std::result::Result<Arc<dyn Browser>, Fail> {
        self.0.start()
    }
}

struct FakeBrowser {
    fake: Arc<Fake>,
}

impl Browser for FakeBrowser {
    fn new_context(&self, proxy: Option<&Proxy>) -> Result<Arc<dyn Context>> {
        let mut opts = vec![
            ("acceptDownloads", JValue::Bool(true)),
            ("serviceWorkers", s("block")),
        ];
        if let Some(p) = proxy {
            opts.push((
                "proxy",
                JValue::obj([
                    ("server", s(&p.server)),
                    ("username", s(&p.username)),
                    ("password", s(&p.password)),
                ]),
            ));
        }
        self.fake.push(vec![s("newContext"), JValue::obj(opts)]);
        Ok(Arc::new(FakeContext {
            fake: self.fake.clone(),
            inner: Arc::new(Mutex::new(Ctx::default())),
        }))
    }
    fn close(&self) -> Result<()> {
        self.fake.push(vec![s("browserClose")]);
        Ok(())
    }
}

#[derive(Default)]
struct Ctx {
    gate: Option<RouteGate>,
    ws: Option<SocketGate>,
    page_hook: Option<PageHook>,
    pages: Vec<FakePage>,
}

struct FakeContext {
    fake: Arc<Fake>,
    inner: Arc<Mutex<Ctx>>,
}

impl Context for FakeContext {
    fn route(&self, gate: RouteGate) -> Result<()> {
        self.fake.push(vec![s("route")]);
        self.inner.lock().unwrap().gate = Some(gate);
        Ok(())
    }
    fn route_websocket(&self, gate: SocketGate) -> Result<()> {
        self.fake.push(vec![s("routeWebSocket")]);
        self.inner.lock().unwrap().ws = Some(gate);
        Ok(())
    }
    fn on_page(&self, hook: PageHook) {
        self.inner.lock().unwrap().page_hook = Some(hook);
    }
    fn new_page(&self) -> Result<Arc<dyn Page>> {
        self.fake.push(vec![s("newPage")]);
        let page = FakePage(Arc::new(PageInner {
            fake: self.fake.clone(),
            ctx: self.inner.clone(),
            state: Mutex::new(PageState {
                url: "about:blank".into(),
                previous: "about:blank".into(),
                download: None,
            }),
        }));
        let hook = {
            let mut ctx = self.inner.lock().unwrap();
            ctx.pages.push(page.clone());
            ctx.page_hook.clone()
        };
        if let Some(h) = hook {
            h(Arc::new(page.clone()));
        }
        Ok(Arc::new(page))
    }
    fn pages(&self) -> Vec<Arc<dyn Page>> {
        self.inner
            .lock()
            .unwrap()
            .pages
            .iter()
            .map(|p| Arc::new(p.clone()) as Arc<dyn Page>)
            .collect()
    }
    fn close(&self) -> Result<()> {
        self.fake.push(vec![s("contextClose")]);
        Ok(())
    }
}

struct PageState {
    url: String,
    previous: String,
    download: Option<DownloadHook>,
}

pub struct PageInner {
    fake: Arc<Fake>,
    ctx: Arc<Mutex<Ctx>>,
    state: Mutex<PageState>,
}

/// One page; its clones are the same page.
#[derive(Clone)]
pub struct FakePage(Arc<PageInner>);

impl std::ops::Deref for FakePage {
    type Target = Arc<PageInner>;
    fn deref(&self) -> &Arc<PageInner> {
        &self.0
    }
}

impl PageInner {
    fn url_now(&self) -> String {
        self.state.lock().unwrap().url.clone()
    }

    /// `routeRequest(url, nav)`: true when continued.
    fn route_request(&self, url: &str, nav: bool) -> bool {
        let gate = self.ctx.lock().unwrap().gate.clone();
        let Some(gate) = gate else { return true };
        match gate(&Request {
            url: url.to_string(),
            main_frame_navigation: nav,
        }) {
            Route::Continue => {
                self.fake.push(vec![s("continue"), s(url)]);
                true
            }
            Route::Abort => {
                self.fake
                    .push(vec![s("abort"), s(url), s("blockedbyclient")]);
                false
            }
        }
    }

    /// What goto does after its log line; `Err` when the navigation was refused.
    fn navigate(&self, url: &str) -> std::result::Result<(), ()> {
        if !self.route_request(url, true) {
            return Err(());
        }
        let world = self.fake.world.lock().unwrap();
        let page = world.page(url);
        drop(world);
        let landed = page
            .get("redirect")
            .as_str()
            .map_or(url.to_string(), str::to_string);
        {
            let mut st = self.state.lock().unwrap();
            st.previous = std::mem::replace(&mut st.url, landed.clone());
        }
        let landed_page = self.fake.world.lock().unwrap().page(&landed);
        if let JValue::Arr(subs) = landed_page.get("subrequests") {
            for u in subs {
                self.route_request(u.as_str().unwrap(), false);
            }
        }
        if let JValue::Arr(sockets) = landed_page.get("websockets") {
            let gate = self.ctx.lock().unwrap().ws.clone();
            for u in sockets {
                let u = u.as_str().unwrap();
                if let Some(g) = &gate {
                    match g(u) {
                        Socket::Connect => self.fake.push(vec![s("wsConnect"), s(u)]),
                        Socket::Close { code, .. } => {
                            self.fake
                                .push(vec![s("wsClose"), s(u), JValue::Num(f64::from(code))])
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn handle(self: &Arc<Self>, selector: &str, url: &str, body: bool) -> Arc<dyn Handle> {
        Arc::new(FakeHandle {
            page: self.clone(),
            selector: selector.to_string(),
            url: url.to_string(),
            body,
        })
    }
}

impl Page for FakePage {
    fn id(&self) -> u64 {
        Arc::as_ptr(&self.0) as usize as u64
    }
    fn on_download(&self, hook: DownloadHook) {
        self.state.lock().unwrap().download = Some(hook);
    }
    fn on_close(&self, _hook: CloseHook) {}
    fn is_closed(&self) -> bool {
        false
    }
    fn url(&self) -> String {
        self.url_now()
    }
    fn locate(&self, selector: &str, _timeout: Duration) -> Result<Option<Arc<dyn Handle>>> {
        self.fake.push(vec![s("locate"), s(selector)]);
        let url = self.url_now();
        let found = self
            .fake
            .world
            .lock()
            .unwrap()
            .element(&url, selector)
            .is_some();
        Ok(found.then(|| self.handle(selector, &url, false)))
    }
    fn active_element(&self) -> Result<Option<Arc<dyn Handle>>> {
        self.fake.push(vec![s("activeElement")]);
        let url = self.url_now();
        let page = self.fake.world.lock().unwrap().page(&url);
        if let JValue::Obj(els) = page.get("elements") {
            if let Some((sel, _)) = els
                .iter()
                .find(|(_, e)| e.get("focused").as_bool() == Some(true))
            {
                return Ok(Some(self.handle(sel, &url, false)));
            }
        }
        Ok(Some(self.handle("body", &url, true)))
    }
    fn locator_text(&self, selector: &str, _timeout: Duration) -> Result<String> {
        self.fake.push(vec![s("innerText"), s(selector)]);
        let url = self.url_now();
        let el = self
            .fake
            .world
            .lock()
            .unwrap()
            .element(&url, selector)
            .ok_or_else(|| EngineError::new("not found"))?;
        let text = el.get("text");
        Ok(text.as_str().map_or_else(
            || {
                el.get("facts")
                    .get("text")
                    .as_str()
                    .unwrap_or("")
                    .to_string()
            },
            str::to_string,
        ))
    }
    fn body_text(&self, _timeout: Duration) -> Result<String> {
        self.fake.push(vec![s("innerText"), s("body")]);
        let url = self.url_now();
        Ok(self
            .fake
            .world
            .lock()
            .unwrap()
            .page(&url)
            .get("text")
            .as_str()
            .unwrap_or("")
            .to_string())
    }
    fn goto(&self, url: &str, _timeout: Duration) -> Result<()> {
        self.fake.push(vec![s("goto"), s(url)]);
        self.navigate(url)
            .map_err(|()| EngineError::new(format!("net::ERR_BLOCKED_BY_CLIENT at {url}")))
    }
    fn go_back(&self, _timeout: Duration) -> Result<()> {
        self.fake.push(vec![s("goBack")]);
        let mut st = self.state.lock().unwrap();
        st.url = st.previous.clone();
        Ok(())
    }
    fn keyboard_press(&self, key: &str) -> Result<()> {
        self.fake.push(vec![s("keyboardPress"), s(key)]);
        Ok(())
    }
    fn wheel(&self, dy: f64) -> Result<()> {
        self.fake.push(vec![s("wheel"), JValue::Num(dy)]);
        Ok(())
    }
    fn wait_for_timeout(&self, ms: f64) -> Result<()> {
        self.fake.push(vec![s("waitForTimeout"), JValue::Num(ms)]);
        Ok(())
    }
    fn screenshot(&self) -> Result<Vec<u8>> {
        self.fake.push(vec![s("screenshot")]);
        let url = self.url_now();
        if self
            .fake
            .world
            .lock()
            .unwrap()
            .page(&url)
            .get("screenshot")
            .as_str()
            == Some("fail")
        {
            return Err(EngineError::new("screenshot failed"));
        }
        Ok(format!("png:{url}").into_bytes())
    }
    fn wait_for_load_state(&self, _timeout: Duration) -> Result<()> {
        self.fake.push(vec![s("waitForLoadState")]);
        Ok(())
    }
}

struct FakeHandle {
    page: Arc<PageInner>,
    selector: String,
    url: String,
    body: bool,
}

impl FakeHandle {
    fn element(&self) -> Option<JValue> {
        self.page
            .fake
            .world
            .lock()
            .unwrap()
            .element(&self.url, &self.selector)
    }
    fn act(&self, method: &str, extra: Vec<JValue>) -> Result<Option<JValue>> {
        let mut parts = vec![s(method), s(&self.selector)];
        parts.extend(extra);
        self.page.fake.push(parts);
        let el = self.element();
        if let Some(el) = &el {
            if el.get("fail").as_str() == Some(method) {
                return Err(EngineError::new(
                    el.get("failMessage").as_str().unwrap_or(""),
                ));
            }
        }
        Ok(el)
    }
    fn after(&self, el: Option<JValue>, click: bool) {
        let Some(el) = el else { return };
        if click && el.get("download").is_object() {
            let d = el.get("download");
            let hook = self.page.state.lock().unwrap().download.clone();
            if let Some(h) = hook {
                h(Arc::new(FakeDownload {
                    fake: self.page.fake.clone(),
                    name: d.get("name").as_str().unwrap_or("").to_string(),
                    url: d.get("url").as_str().unwrap_or("").to_string(),
                }));
            }
        }
        if let Some(to) = el.get("navigates").as_str() {
            let _ = self.page.navigate(to);
        }
    }
}

impl Handle for FakeHandle {
    fn describe(&self) -> Result<JValue> {
        self.page.fake.push(vec![s("describe"), s(&self.selector)]);
        if self.body {
            return Ok(JValue::obj([
                ("tag", s("body")),
                ("type", s("")),
                ("role", s("")),
                ("name", s("")),
                ("text", s("")),
                ("value", s("")),
                ("inForm", JValue::Bool(false)),
                ("formMethod", s("")),
                ("formAction", s("")),
                ("frameOrigin", s(&browser_task_origin(&self.url))),
            ]));
        }
        self.element()
            .map(|e| e.get("facts").clone())
            .ok_or_else(|| EngineError::new("detached"))
    }
    fn is_connected(&self) -> Result<bool> {
        self.page
            .fake
            .push(vec![s("isConnected"), s(&self.selector)]);
        Ok(self.body || self.element().is_some())
    }
    fn is_focused(&self) -> Result<bool> {
        self.page.fake.push(vec![s("isFocused"), s(&self.selector)]);
        Ok(self
            .element()
            .is_some_and(|e| e.get("focused").as_bool() == Some(true)))
    }
    fn click(&self, _t: Duration) -> Result<()> {
        let el = self.act("click", vec![])?;
        self.after(el, true);
        Ok(())
    }
    fn hover(&self, _t: Duration) -> Result<()> {
        self.act("hover", vec![]).map(|_| ())
    }
    fn fill(&self, value: &str, _t: Duration) -> Result<()> {
        self.act("fill", vec![s(value)]).map(|_| ())
    }
    fn select_option(&self, value: &str, _t: Duration) -> Result<()> {
        self.act("selectOption", vec![s(value)]).map(|_| ())
    }
    fn press(&self, key: &str, _t: Duration) -> Result<()> {
        self.act("press", vec![s(key)]).map(|_| ())
    }
    fn set_input_files(&self, file: &Path, _t: Duration) -> Result<()> {
        let base = file
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        self.act("setInputFiles", vec![s(&base)]).map(|_| ())
    }
    fn submit(&self) -> Result<()> {
        self.page.fake.push(vec![s("submit"), s(&self.selector)]);
        if let Some(el) = self.element() {
            if let Some(to) = el.get("navigates").as_str() {
                let _ = self.page.navigate(to);
            }
        }
        Ok(())
    }
    fn dispose(&self) -> Result<()> {
        self.act("dispose", vec![]).map(|_| ())
    }
}

fn browser_task_origin(url: &str) -> String {
    match url::Url::parse(url) {
        Ok(u) if matches!(u.scheme(), "http" | "https") => u.origin().ascii_serialization(),
        _ => String::new(),
    }
}

struct FakeDownload {
    fake: Arc<Fake>,
    name: String,
    url: String,
}

impl Download for FakeDownload {
    fn suggested_filename(&self) -> String {
        self.name.clone()
    }
    fn url(&self) -> String {
        self.url.clone()
    }
    fn save_as(&self, target: &Path) -> Result<()> {
        let base = target
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        self.fake.push(vec![s("saveAs"), s(&base)]);
        std::fs::write(target, "qa-download").map_err(|e| EngineError::new(e.to_string()))
    }
}
