//! Experimental native adapter. Not connected to the task server; see ADOPTION.md.
//! One browser process per session gives an independent profile and proxy boundary.
#![forbid(unsafe_code)]
use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use chromiumoxide::{
    browser::{Browser, BrowserConfig},
    Page,
};
use chromiumoxide_types::{Command, Method, MethodId};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    future::Future,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    task::JoinHandle,
    time::{timeout, Instant},
};

pub type AuditLog = Arc<dyn Fn(Value) + Send + Sync>;

mod guard;
mod socket_proxy;

const LIMIT: Duration = Duration::from_secs(15);
const WORKER_SCRIPT: &str = "if(navigator.serviceWorker) navigator.serviceWorker.register=async()=>{console.warn('Service worker registration blocked');};";
const SECRET_REASON: &str = "Screenshot omitted because this browser session has handled a secret.";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Secret {
    pub value: String,
    pub domains: Vec<String>,
}
#[derive(Clone)]
pub struct ProxyCredentials {
    pub username: String,
    pub password: String,
}
impl std::fmt::Debug for ProxyCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ProxyCredentials(<redacted>)")
    }
}
#[derive(Clone, Debug)]
pub struct Launch {
    pub executable: PathBuf,
    pub profile_dir: PathBuf,
    pub downloads_dir: PathBuf,
    pub allowed_domains: Vec<String>,
    pub upload_files: Vec<PathBuf>,
    pub secrets: BTreeMap<String, Secret>,
    /// Mandatory outside synthetic testing. The proxy enforces every redirect hop.
    pub proxy: Option<String>,
    pub proxy_credentials: Option<ProxyCredentials>,
    pub synthetic_direct: bool,
    pub args: Vec<String>,
    pub timeout: Duration,
}
#[async_trait]
pub trait Approval: Send + Sync {
    async fn decide(&self, card: Value) -> Result<bool>;
}
/// Provisional contract for the task executor owner: one serialized session per instance.
#[async_trait]
pub trait BrowserEngine {
    async fn act(&mut self, action: Value, approval: &dyn Approval) -> Result<Value>;
    async fn close(&mut self) -> bool;
}

#[derive(Serialize)]
struct Raw {
    #[serde(skip)]
    method: &'static str,
    #[serde(flatten)]
    params: Value,
}
impl Method for Raw {
    fn identifier(&self) -> MethodId {
        self.method.into()
    }
}
impl Command for Raw {
    type Response = Value;
}
fn raw(method: &'static str, params: Value) -> Raw {
    Raw { method, params }
}
async fn bounded<T>(duration: Duration, f: impl Future<Output = Result<T>>) -> Result<T> {
    timeout(duration, f)
        .await
        .map_err(|_| anyhow!("Action timed out"))?
}
async fn command(page: &Page, method: &'static str, params: Value) -> Result<Value> {
    bounded(LIMIT, async {
        Ok(page.execute(raw(method, params)).await?.result)
    })
    .await
}
async fn evaluate(page: &Page, expression: String) -> Result<Value> {
    let r = command(
        page,
        "Runtime.evaluate",
        json!({"expression":expression,"returnByValue":true,"awaitPromise":true}),
    )
    .await?;
    if r.get("exceptionDetails").is_some() {
        bail!("Page evaluation failed")
    }
    Ok(r.pointer("/result/value").cloned().unwrap_or(Value::Null))
}
async fn on_node(page: &Page, id: &str, function: String, args: Vec<Value>) -> Result<Value> {
    let r = command(page,"Runtime.callFunctionOn",json!({"objectId":id,"functionDeclaration":function,"arguments":args.into_iter().map(|v|json!({"value":v})).collect::<Vec<_>>(),"returnByValue":true,"awaitPromise":true})).await?;
    if r.get("exceptionDetails").is_some() {
        bail!("Target operation failed")
    }
    Ok(r.pointer("/result/value").cloned().unwrap_or(Value::Null))
}
#[derive(Debug)]
struct CompatibilityError(String);
impl std::fmt::Display for CompatibilityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for CompatibilityError {}
fn compatibility_error(text: String) -> anyhow::Error {
    anyhow!(CompatibilityError(text))
}
fn public_error(kind: &str, error: &anyhow::Error, deadline: Duration) -> String {
    if let Some(error) = error.downcast_ref::<CompatibilityError>() {
        return error.0.clone();
    }
    let method = match kind {
        "type" => "fill",
        "select" => "selectOption",
        "upload" => "setInputFiles",
        other => other,
    };
    let owner = if kind == "navigate" {
        "page"
    } else {
        "elementHandle"
    };
    let text = error.to_string();
    if text == "Action timed out" {
        return format!(
            "{owner}.{method}: Timeout {}ms exceeded.",
            deadline.as_millis()
        );
    }
    if let Some(key) = text.strip_prefix("Key not found: ") {
        return format!("elementHandle.press: Unknown key: {}", json!(key));
    }
    format!("{owner}.{method}: Browser operation failed")
}
fn js_string(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(v)) => v.clone(),
        Some(Value::Array(v)) => v
            .iter()
            .map(|v| js_string(Some(v)))
            .collect::<Vec<_>>()
            .join(","),
        Some(Value::Object(_)) => "[object Object]".into(),
        Some(v) => v.to_string(),
    }
}
fn string(v: &Value, k: &str) -> String {
    v.get(k).and_then(Value::as_str).unwrap_or("").to_owned()
}
fn policy(op: u8, args: Value) -> Result<Value> {
    let mut input = vec![op];
    input.extend(serde_json::to_vec(&args)?);
    let (status, bytes) = browser_policy::call(&input);
    if status != 0 {
        bail!("Browser policy unavailable")
    }
    Ok(serde_json::from_slice(&bytes)?)
}
fn navigation(url: &str, domains: &[String]) -> bool {
    policy(2, json!([url, domains]))
        .ok()
        .and_then(|v| v.get("ok").and_then(Value::as_bool))
        == Some(true)
}
fn validate_proxy(server: &str) -> Result<()> {
    let url = url::Url::parse(server).context("Invalid proxy server")?;
    if !matches!(url.scheme(), "http" | "https" | "socks5") || url.host_str().is_none() {
        bail!("Unsupported proxy server")
    }
    if !url.username().is_empty() || url.password().is_some() {
        bail!("Authenticated proxy grants are not yet supported by this adapter")
    }
    Ok(())
}
fn origin(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .filter(|u| matches!(u.scheme(), "http" | "https"))
        .map(|u| u.origin().ascii_serialization())
        .unwrap_or_default()
}
fn mask(text: &str, secrets: &BTreeMap<String, Secret>) -> String {
    let mut out = text.to_owned();
    let mut entries: Vec<_> = secrets
        .iter()
        .filter(|(_, s)| s.value.encode_utf16().count() >= 4)
        .collect();
    entries.sort_by_key(|(_, s)| std::cmp::Reverse(s.value.len()));
    for (name, s) in entries {
        let encoded = s
            .value
            .bytes()
            .map(|b| {
                if b.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&b) {
                    (b as char).to_string()
                } else {
                    format!("%{b:02X}")
                }
            })
            .collect::<String>();
        for form in [&s.value, &encoded, &encoded.replace("%20", "+")] {
            out = out.replace(form, &format!("{{{{secret:{name}}}}}"));
        }
    }
    out
}
struct ActionLease {
    shutdown: tokio::sync::mpsc::UnboundedSender<()>,
    interrupted: Arc<AtomicBool>,
    complete: bool,
    proxy: tokio::task::AbortHandle,
}
impl Drop for ActionLease {
    fn drop(&mut self) {
        if !self.complete {
            self.interrupted.store(true, Ordering::SeqCst);
            self.proxy.abort();
            let _ = self.shutdown.send(());
        }
    }
}
/// Profile paths must be fresh task-local directories, never shared with another session.
pub struct CdpEngine {
    browser: Browser,
    page: Page,
    pump: JoinHandle<()>,
    event_error: Arc<tokio::sync::Mutex<Option<String>>>,
    config: Launch,
    log: AuditLog,
    job_id: String,
    session_id: String,
    secret_used: bool,
    closed: bool,
    close_confirmed: bool,
    interrupted: Arc<AtomicBool>,
    downloads: Arc<tokio::sync::Mutex<Vec<Value>>>,
    download_task: JoinHandle<()>,
    targets: Arc<tokio::sync::Mutex<Vec<String>>>,
    page_watch: JoinHandle<()>,
    guard: guard::Guard,
    _socket_proxy: socket_proxy::SocketProxy,
}
impl Drop for CdpEngine {
    fn drop(&mut self) {
        self.pump.abort();
        self.download_task.abort();
        self.page_watch.abort();
    }
}
impl CdpEngine {
    pub async fn launch(config: Launch) -> Result<Self> {
        Self::launch_with_log(config, String::new(), Arc::new(|_| {})).await
    }
    pub async fn launch_with_log(config: Launch, job_id: String, log: AuditLog) -> Result<Self> {
        let mut entropy = [0u8; 16];
        std::io::Read::read_exact(&mut std::fs::File::open("/dev/urandom")?, &mut entropy)?;
        let session_id = entropy
            .iter()
            .map(|v| format!("{v:02x}"))
            .collect::<String>();
        if config.allowed_domains.is_empty() {
            bail!("A browser task needs at least one allowed domain.")
        }
        if config.proxy.is_none() && !config.synthetic_direct {
            bail!("A browser task needs an egress proxy grant for its domains.")
        }
        if let Some(proxy) = &config.proxy {
            validate_proxy(proxy)?;
        } else if config.proxy_credentials.is_some() {
            bail!("Proxy credentials require a proxy grant");
        }
        // Exclusive profile creation prevents accidental session reuse.
        std::fs::create_dir(&config.profile_dir).context("Profile must be fresh")?;
        std::fs::create_dir_all(&config.downloads_dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&config.profile_dir, std::fs::Permissions::from_mode(0o700))?;
            std::fs::set_permissions(
                &config.downloads_dir,
                std::fs::Permissions::from_mode(0o700),
            )?;
        }
        let mut builder = BrowserConfig::builder()
            .disable_default_args()
            .args(
                serde_json::from_str::<Vec<String>>(include_str!("launch-flags.json"))?
                    .into_iter()
                    .map(|s| s.trim_start_matches("--").to_owned()),
            )
            .chrome_executable(&config.executable)
            .user_data_dir(&config.profile_dir)
            .launch_timeout(LIMIT)
            .request_timeout(LIMIT)
            .args(
                config
                    .args
                    .iter()
                    .filter(|s| s.as_str() != "--no-proxy-server")
                    .map(|s| s.trim_start_matches("--").to_owned()),
            );
        let socket_proxy = socket_proxy::SocketProxy::start(&config).await?;
        // Every protocol uses the same gate, including CONNECT for secure sockets.
        builder = builder
            .arg(format!("proxy-server={}", socket_proxy.address))
            .arg("proxy-bypass-list=<-loopback>");
        builder = builder.respect_https_errors().viewport(None);
        let launched = bounded(LIMIT, async {
            Ok(Browser::launch(builder.build().map_err(|e| anyhow!(e))?).await?)
        })
        .await;
        let (browser, mut handler) = match launched {
            Ok(pair) => pair,
            Err(error) => {
                // The library reaps a failed launch before returning its error.
                // A timed-out launch may have been cancelled: retain that profile.
                if !error.to_string().contains("timed out") {
                    let _ = tokio::fs::remove_dir_all(&config.profile_dir).await;
                }
                return Err(error);
            }
        };
        let event_error = Arc::new(tokio::sync::Mutex::new(None));
        let errors = event_error.clone();
        let pump = tokio::spawn(async move {
            while let Some(event) = handler.next().await {
                if event.is_err() {
                    *errors.lock().await = Some("Browser connection unavailable".into());
                    break;
                }
            }
        });
        let guard = guard::Guard::start(
            browser.websocket_address(),
            config.allowed_domains.clone(),
            event_error.clone(),
            config.secrets.clone(),
            log.clone(),
            job_id.clone(),
            Some((
                origin(&socket_proxy.address),
                socket_proxy.credentials.clone(),
            )),
        )
        .await?;
        let page = bounded(LIMIT, async { Ok(browser.new_page("about:blank").await?) }).await?;
        use chromiumoxide::cdp::browser_protocol::browser::{
            EventDownloadProgress, EventDownloadWillBegin,
        };
        let mut begins = browser.event_listener::<EventDownloadWillBegin>().await?;
        let mut progress = browser.event_listener::<EventDownloadProgress>().await?;
        use chromiumoxide::cdp::browser_protocol::target::{
            EventTargetCreated, EventTargetDestroyed,
        };
        let mut created = browser.event_listener::<EventTargetCreated>().await?;
        let mut destroyed = browser.event_listener::<EventTargetDestroyed>().await?;
        let targets = Arc::new(tokio::sync::Mutex::new(vec![page
            .target_id()
            .as_ref()
            .to_owned()]));
        let changes = targets.clone();
        let page_watch = tokio::spawn(async move {
            loop {
                tokio::select! {
                    event=created.next()=>{let Some(e)=event else {break};
                        if let Ok(v)=serde_json::to_value(&*e) {if v.pointer("/targetInfo/type").and_then(Value::as_str)==Some("page") {
                            if let Some(id)=v.pointer("/targetInfo/targetId").and_then(Value::as_str) {changes.lock().await.push(id.to_owned());}
                        }}
                    },
                    event=destroyed.next()=>{let Some(e)=event else{break};
                        if let Ok(v)=serde_json::to_value(&*e) {if let Some(id)=v.get("targetId").and_then(Value::as_str){changes.lock().await.retain(|t|t!=id);}}
                    }
                }
            }
        });
        let downloads = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let output = downloads.clone();
        let dir = config.downloads_dir.clone();
        let secrets = config.secrets.clone();
        let download_task = tokio::spawn(async move {
            let mut pending = BTreeMap::new();
            let mut seq = 0u64;
            loop {
                tokio::select! {
                    biased;
                    event=begins.next()=>{let Some(event)=event else{break};
                        if let Ok(v)=serde_json::to_value(&*event) {
                            seq+=1;let suggested=string(&v,"suggestedFilename");
                            let name=suggested.rsplit('/').next().unwrap_or("download").trim_start_matches('.');
                            let name=if name.is_empty(){"download"}else{name};
                            let guid=string(&v,"guid");
                            pending.insert(guid,(format!("{seq}-{name}"),name.to_owned(),mask(&string(&v,"url"),&secrets)));
                        }
                    },
                    event=progress.next()=>{let Some(event)=event else{break};
                        if let Ok(v)=serde_json::to_value(&*event) {
                            let guid=string(&v,"guid");let state=string(&v,"state");
                            if state=="completed" {
                                if let Some((file,name,from))=pending.remove(&guid) {
                                    let source=dir.join(&guid);let destination=dir.join(file);
                                    if tokio::fs::hard_link(&source,&destination).await.is_ok() {
                                        let _=tokio::fs::remove_file(source).await;
                                        output.lock().await.push(json!({"name":name,"from":from,"path":destination}));
                                    }
                                }
                            }else if state=="canceled" {pending.remove(&guid);}
                        }
                    }
                }
            }
        });
        let mut engine = Self {
            browser,
            page,
            pump,
            event_error,
            config,
            log,
            job_id,
            session_id,
            secret_used: false,
            closed: false,
            close_confirmed: false,
            interrupted: Arc::new(AtomicBool::new(false)),
            downloads,
            download_task,
            targets,
            page_watch,
            guard,
            _socket_proxy: socket_proxy,
        };
        if let Err(e) = engine.prepare().await {
            engine.close().await;
            return Err(e);
        }
        (engine.log)(
            json!({"event":"browser.opened","jobId":engine.job_id,"session":engine.session_id,"allowedDomains":engine.config.allowed_domains}),
        );
        Ok(engine)
    }
    async fn prepare(&mut self) -> Result<()> {
        command(&self.page, "Network.enable", json!({})).await?;
        command(
            &self.page,
            "Network.setBypassServiceWorker",
            json!({"bypass":true}),
        )
        .await?;
        // Registration is blocked too: bypass alone would allow persistent worker side effects.

        command(
            &self.page,
            "Page.addScriptToEvaluateOnNewDocument",
            json!({"source":WORKER_SCRIPT}),
        )
        .await?;
        self.browser.execute(raw("Browser.setDownloadBehavior",json!({"behavior":"allowAndName","downloadPath":self.config.downloads_dir,"eventsEnabled":true}))).await?;
        Ok(())
    }
    async fn refresh_page(&mut self) -> Result<()> {
        let newest = self
            .targets
            .lock()
            .await
            .last()
            .cloned()
            .ok_or_else(|| anyhow!("No page is open in this session."))?;
        if newest != self.page.target_id().as_ref() {
            self.page = bounded(self.config.timeout, async {
                Ok(self.browser.get_page(newest.into()).await?)
            })
            .await?;
            self.prepare().await?;
        }
        Ok(())
    }
    pub async fn state(&self) -> Value {
        let blocked = self.guard.blocked.lock().await.clone();
        json!({"origin":origin(&self.url().await.unwrap_or_default()),"blocked":blocked,"downloads":self.downloads.lock().await.clone(),"closed":self.closed || self.guard.finished()})
    }
    /// Transport diagnostics include browser-background traffic, not just task page events.
    pub async fn proxy_denials(&self) -> Vec<Value> {
        self._socket_proxy.blocked.lock().await.clone()
    }
    pub async fn policy_trace(&self) -> Vec<Value> {
        self.guard.trace.lock().await.clone()
    }
    pub async fn event_error(&self) -> Option<String> {
        self.event_error.lock().await.clone()
    }
    /// Disposable local fixture support only; never grants a runtime task permission.
    pub async fn grant_fixture_loopback(&self, fixture: &str) -> Result<()> {
        let url = url::Url::parse(fixture)?;
        if !self.config.synthetic_direct
            || !url.host_str().is_some_and(|h| h.ends_with(".test"))
            || !navigation(fixture, &self.config.allowed_domains)
        {
            bail!("Fixture permission unavailable");
        }
        self.browser.execute(raw("Browser.grantPermissions",json!({"origin":url.origin().ascii_serialization(),"permissions":["localNetworkAccess","localNetwork","loopbackNetwork"]}))).await?;
        Ok(())
    }
    pub async fn launch_arguments(&self) -> Result<Value> {
        Ok(self
            .browser
            .execute(raw("Browser.getBrowserCommandLine", json!({})))
            .await?
            .result)
    }
    pub async fn inspect(&mut self, expression: &str) -> Result<Value> {
        self.refresh_page().await?;
        evaluate(&self.page, expression.to_owned()).await
    }
    async fn url(&self) -> Result<String> {
        Ok(evaluate(&self.page, "location.href".into())
            .await?
            .as_str()
            .unwrap_or("")
            .to_owned())
    }
    async fn target(&self, selector: Option<&str>) -> Result<String> {
        let expr = selector
            .map(|s| format!("document.querySelector({})", json!(s)))
            .unwrap_or("document.activeElement".into());
        let deadline = Instant::now() + self.config.timeout;
        loop {
            let r = command(
                &self.page,
                "Runtime.evaluate",
                json!({"expression":expr,"returnByValue":false}),
            )
            .await?;
            if r.get("exceptionDetails").is_some() {
                bail!("That element is not on the page.")
            }
            if let Some(id) = r.pointer("/result/objectId").and_then(Value::as_str) {
                return Ok(id.into());
            }
            if Instant::now() >= deadline {
                bail!("That element is not on the page.")
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
    async fn describe(&self, id: &str) -> Result<Value> {
        on_node(
            &self.page,
            id,
            format!(
                "function() {{return ({})(this)}}",
                include_str!("describe.js")
            ),
            vec![],
        )
        .await
    }
    async fn evidence(&self) -> Value {
        if self.secret_used {
            return json!({"screenshotOmitted":true,"screenshotOmissionReason":SECRET_REASON});
        }
        match command(
            &self.page,
            "Page.captureScreenshot",
            json!({"format":"png"}),
        )
        .await
        {
            Ok(v) => json!({"screenshot":v.get("data")}),
            Err(_) => {
                json!({"screenshotOmitted":true,"screenshotOmissionReason":"Screenshot omitted because capture failed."})
            }
        }
    }
    async fn navigate(&self, url: &str) -> Result<()> {
        // Use the library's raw connection: its high-level navigation watcher waits
        // beyond the requested action deadline on this browser revision.
        let mut endpoint = url::Url::parse(self.browser.websocket_address())?;
        endpoint.set_path(&format!(
            "/devtools/page/{}",
            self.page.target_id().as_ref()
        ));
        let mut connection =
            chromiumoxide::Connection::<chromiumoxide_types::CdpJsonEventMessage>::connect(
                endpoint.as_str(),
            )
            .await?;
        let enable = connection.submit_command("Page.enable".into(), None, json!({}))?;
        loop {
            match connection
                .next()
                .await
                .ok_or_else(|| anyhow!("Browser connection closed"))??
            {
                chromiumoxide_types::Message::Response(response) if response.id == enable => {
                    if response.error.is_some() {
                        bail!("Cannot enable navigation events")
                    }
                    break;
                }
                _ => {}
            }
        }
        let navigation =
            connection.submit_command("Page.navigate".into(), None, json!({"url":url}))?;
        let mut acknowledged = false;
        let mut loaded = false;
        loop {
            match connection
                .next()
                .await
                .ok_or_else(|| anyhow!("Browser connection closed"))??
            {
                chromiumoxide_types::Message::Response(response) if response.id == navigation => {
                    if response.error.is_some() {
                        bail!("Navigation rejected")
                    }
                    let result = response.result.unwrap_or(Value::Null);
                    if result.get("errorText").is_some() {
                        bail!("Navigation failed")
                    }
                    if result.get("loaderId").is_none() {
                        return Ok(());
                    }
                    acknowledged = true;
                }
                chromiumoxide_types::Message::Event(event)
                    if event.method.as_ref() == "Page.loadEventFired" =>
                {
                    loaded = true
                }
                _ => {}
            }
            if acknowledged && loaded {
                return Ok(());
            }
        }
    }
    async fn actionable(&self, id: &str, enabled: bool) -> Result<Value> {
        bounded(self.config.timeout,async {
            let mut previous = Value::Null;
            loop {
                let point=on_node(&self.page,id,"function(enabled){if(!this.isConnected)throw Error('Detached target');this.scrollIntoView({block:'center',inline:'center'});const r=this.getBoundingClientRect();if((enabled&&this.matches(':disabled'))||getComputedStyle(this).visibility==='hidden'||r.width===0||r.height===0)return null;const x=r.x+r.width/2,y=r.y+r.height/2;const hit=document.elementFromPoint(x,y);return hit&&(hit===this||this.contains(hit))?{x,y}:null;}".into(),vec![json!(enabled)]).await?;
                if !point.is_null() && point == previous {return Ok(point)}
                previous = point;
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }).await
    }
    async fn dispatch(
        &mut self,
        a: &Value,
        id: Option<&str>,
        typed: Option<String>,
    ) -> Result<Option<Value>> {
        let kind = js_string(a.get("type"));
        let target = id.unwrap_or("");
        if id.is_none()
            && ["click", "hover", "type", "select", "upload", "submit"].contains(&kind.as_str())
        {
            let method = match kind.as_str() {
                "type" => "fill",
                "select" => "selectOption",
                "upload" => "setInputFiles",
                other => other,
            };
            return Err(compatibility_error(format!(
                "Cannot read properties of null (reading '{method}')"
            )));
        }
        match kind.as_str() {
            "navigate" => {
                self.navigate(&string(a, "url")).await?;
            }
            "extract" => {
                let text = if id.is_some() {
                    on_node(
                        &self.page,
                        target,
                        "function(){return this.innerText}".into(),
                        vec![],
                    )
                    .await?
                } else {
                    evaluate(&self.page, "document.body.innerText".into()).await?
                };
                let text = String::from_utf16_lossy(
                    &text
                        .as_str()
                        .unwrap_or("")
                        .encode_utf16()
                        .take(20000)
                        .collect::<Vec<_>>(),
                );
                return Ok(Some(json!({"text":mask(&text,&self.config.secrets)})));
            }
            "screenshot" => return Ok(Some(self.evidence().await)),
            "wait" => {
                let ms = a
                    .get("ms")
                    .and_then(Value::as_f64)
                    .filter(|n| n.is_finite() && *n != 0.0)
                    .unwrap_or(500.0)
                    .clamp(0.0, 5000.0);
                tokio::time::sleep(Duration::from_secs_f64(ms / 1000.0)).await;
            }
            "scroll" => {
                let dy = a
                    .get("dy")
                    .and_then(Value::as_f64)
                    .filter(|n| n.is_finite() && *n != 0.0)
                    .unwrap_or(600.0);
                command(
                    &self.page,
                    "Input.dispatchMouseEvent",
                    json!({"type":"mouseWheel","x":1,"y":1,"deltaX":0,"deltaY":dy}),
                )
                .await?;
            }
            "submit" => {
                on_node(&self.page,target,"function(){const f=this.form||this.closest('form')||this;f.requestSubmit?f.requestSubmit():f.submit()}".into(),vec![]).await?;
            }
            "select" => {
                let tag = on_node(
                    &self.page,
                    target,
                    "function(){return this.tagName}".into(),
                    vec![],
                )
                .await?;
                if tag != json!("SELECT") {
                    return Err(compatibility_error(
                        "elementHandle.selectOption: Error: Element is not a <select> element"
                            .into(),
                    ));
                }
                bounded(self.config.timeout, async {
                    loop {
                        let done = on_node(&self.page,target,"function(v){if(this.tagName!=='SELECT')throw Error('Not a select');if(this.disabled)return false;const opt=Array.from(this.options).find(o=>o.value===v);if(!opt)return false;this.value=opt.value;this.dispatchEvent(new Event('input',{bubbles:true}));this.dispatchEvent(new Event('change',{bubbles:true}));return true}".into(),vec![json!(typed)]).await?;
                        if done == json!(true) { return Ok(()); }
                        tokio::time::sleep(Duration::from_millis(25)).await;
                    }
                }).await?;
            }
            "type" => {
                let field=on_node(&self.page,target,"function(){return {tag:this.tagName,type:this.type,editable:this.isContentEditable}}".into(),vec![]).await?;
                if string(&field, "tag") == "INPUT"
                    && ![
                        "text",
                        "email",
                        "number",
                        "password",
                        "search",
                        "tel",
                        "url",
                        "color",
                        "date",
                        "time",
                        "datetime-local",
                        "month",
                        "range",
                        "week",
                    ]
                    .contains(&string(&field, "type").as_str())
                {
                    return Err(compatibility_error(format!(
                        "elementHandle.fill: Error: Input of type {} cannot be filled",
                        json!(string(&field, "type"))
                    )));
                }
                if string(&field, "tag") != "INPUT"
                    && string(&field, "tag") != "TEXTAREA"
                    && field.get("editable").and_then(Value::as_bool) != Some(true)
                {
                    return Err(compatibility_error("elementHandle.fill: Error: Element is not an <input>, <textarea> or [contenteditable] element".into()));
                }
                self.actionable(target, true).await?;
                bounded(self.config.timeout, async {
                    loop {
                        let editable = on_node(&self.page,target,"function(){return !this.disabled&&!this.readOnly&&(this.isContentEditable||this.tagName==='TEXTAREA'||this.tagName==='INPUT')}".into(),vec![]).await?;
                        if editable == json!(true) { break; }
                        tokio::time::sleep(Duration::from_millis(25)).await;
                    }
                    let assigned=on_node(&self.page,target,"function(value){if(this.tagName!=='INPUT'||!['date','time','datetime-local','month','week'].includes(this.type))return false;this.focus();this.value=value;if(this.value!==value)throw Error('Malformed value');this.dispatchEvent(new Event('input',{bubbles:true,composed:true}));this.dispatchEvent(new Event('change',{bubbles:true}));return true}".into(),vec![json!(typed)]).await?;
                    if assigned==json!(true){return Ok(())}
                    on_node(&self.page,target,"function(){this.focus();if(this.isContentEditable){const r=document.createRange();r.selectNodeContents(this);const s=getSelection();s.removeAllRanges();s.addRange(r)}else this.select()}".into(),vec![]).await?;
                    if typed.as_deref() == Some("") {
                        for kind in ["keyDown", "keyUp"] {
                            command(&self.page,"Input.dispatchKeyEvent",json!({"type":kind,"key":"Backspace","code":"Backspace","windowsVirtualKeyCode":8})).await?;
                        }
                    } else {
                        command(&self.page,"Input.insertText",json!({"text":typed})).await?;
                    }
                    Ok(())
                }).await?;
            }
            "upload" => {
                let file = std::fs::canonicalize(string(a, "file"))?;
                if !self
                    .config
                    .upload_files
                    .iter()
                    .filter_map(|p| std::fs::canonicalize(p).ok())
                    .any(|p| p == file)
                {
                    bail!("Only files given to this task can be uploaded.")
                }
                command(
                    &self.page,
                    "DOM.setFileInputFiles",
                    json!({"objectId":target,"files":[file]}),
                )
                .await?;
            }
            "click" | "hover" => {
                let p = self.actionable(target, kind == "click").await?;
                let event = json!({"type":"mouseMoved","x":p.get("x"),"y":p.get("y")});
                command(&self.page, "Input.dispatchMouseEvent", event.clone()).await?;
                if kind == "click" {
                    for t in ["mousePressed", "mouseReleased"] {
                        command(&self.page,"Input.dispatchMouseEvent",json!({"type":t,"x":p.get("x"),"y":p.get("y"),"button":"left","clickCount":1})).await?;
                    }
                }
            }
            "press" => {
                if id.is_some() {
                    on_node(
                        &self.page,
                        target,
                        "function(){this.focus()}".into(),
                        vec![],
                    )
                    .await?;
                }
                let key = string(a, "key");
                // Library key table supports modifier combinations and dispatches trusted CDP input.
                let element = self.page.find_element(":focus").await?;
                element.press_key(key).await?;
            }
            _ => bail!("Unknown action"),
        }
        if kind != "wait" {
            bounded(self.config.timeout, async {
                loop {
                    if evaluate(&self.page, "document.readyState !== 'loading'".into())
                        .await?
                        .as_bool()
                        == Some(true)
                    {
                        return Ok(());
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .ok();
        }
        Ok(None)
    }
    async fn run(&mut self, a: &Value, approval: &dyn Approval, id: Option<&str>) -> Result<Value> {
        let kind = js_string(a.get("type"));
        let before = origin(&self.url().await?);
        let result_secrets = self.config.secrets.clone();
        let result = |status: &str, reason: &str| json!({"status":status,"origin":before,"reason":mask(reason,&result_secrets)});
        if ![
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
        ]
        .contains(&kind.as_str())
        {
            return Ok(result(
                "blocked",
                &format!(
                    "Unknown action “{}”.",
                    String::from_utf16_lossy(
                        &mask(&kind, &self.config.secrets)
                            .encode_utf16()
                            .take(40)
                            .collect::<Vec<_>>()
                    )
                ),
            ));
        }
        if kind == "navigate"
            && a.get("method")
                .and_then(Value::as_str)
                .is_some_and(|m| !m.is_empty() && !m.eq_ignore_ascii_case("GET"))
        {
            return Ok(result(
                "blocked",
                "Only plain navigation is supported; submit the page’s form instead.",
            ));
        }
        let element = if let Some(id) = id {
            self.describe(id).await?
        } else {
            json!({})
        };
        let projected = json!({"tag":string(&element,"tag"),"type":string(&element,"type"),"role":string(&element,"role"),"name":string(&element,"name"),"text":string(&element,"text"),"value":string(&element,"value"),"inForm":element.get("inForm").and_then(Value::as_bool).unwrap_or(false)});
        let verdict = policy(
            1,
            json!([
                {"type":kind,"url":if kind=="navigate"{a.get("url").cloned().unwrap_or(Value::Null)}else{Value::Null},"method":if kind=="navigate"{a.get("method").cloned().unwrap_or(json!("GET"))}else{Value::Null},"key":if kind=="press"{a.get("key").cloned().unwrap_or(json!(""))}else{Value::Null}},
                projected,
                {"origin":before,"allowedDomains":if kind=="navigate"{self.config.allowed_domains.clone()}else{vec![]}}
            ]),
        )?;
        if verdict.get("status").and_then(Value::as_str) == Some("blocked") {
            return Ok(result("blocked", &string(&verdict, "reason")));
        }
        let mut typed = None;
        if kind == "type" || kind == "select" {
            let mut text = a
                .get("text")
                .or(a.get("value"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            let specs: Vec<_> = self
                .config
                .secrets
                .iter()
                .map(|(k, s)| json!([k, s.domains]))
                .collect();
            let sub = policy(
                3,
                json!([
                    text,
                    specs,
                    element
                        .get("frameOrigin")
                        .and_then(Value::as_str)
                        .unwrap_or(&before)
                ]),
            )?;
            if sub.get("ok").and_then(Value::as_bool) != Some(true) {
                return Ok(result("blocked", &string(&sub, "reason")));
            }
            for name in sub
                .get("used")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
            {
                let s = self
                    .config
                    .secrets
                    .get(name)
                    .ok_or_else(|| anyhow!("Secret unavailable"))?;
                text = text.replace(&format!("{{{{secret:{name}}}}}"), &s.value);
                self.secret_used = true;
            }
            typed = Some(text);
        }
        if kind == "upload" {
            let file = std::fs::canonicalize(js_string(a.get("file"))).ok();
            if file.as_ref().is_none_or(|f| {
                !self
                    .config
                    .upload_files
                    .iter()
                    .filter_map(|p| std::fs::canonicalize(p).ok())
                    .any(|p| &p == f)
            }) {
                return Ok(result(
                    "blocked",
                    "Only files given to this task can be uploaded.",
                ));
            }
        }
        if verdict.get("status").and_then(Value::as_str) != Some("allow") {
            let mut card = json!({"jobId":self.job_id,"origin":before,"action":kind,"reason":verdict.get("reason"),"element":if id.is_some(){serde_json::from_str::<Value>(&mask(&serde_json::to_string(&element)?,&self.config.secrets)).ok()}else{None},"url":a.get("url").filter(|v|!v.is_null()).map(|v|mask(&js_string(Some(v)),&self.config.secrets)),"typed":typed.as_ref().map(|t|mask(t,&self.config.secrets)),"file":if kind=="upload"{std::path::Path::new(&js_string(a.get("file"))).file_name().map(|v|v.to_string_lossy().into_owned())}else{None}});
            if let (Some(card), Value::Object(evidence)) =
                (card.as_object_mut(), self.evidence().await)
            {
                card.extend(evidence);
            }
            let answer = bounded(Duration::from_secs(300), approval.decide(card))
                .await
                .unwrap_or(false);
            (self.log)(
                json!({"event":"browser.decided","jobId":self.job_id,"type":kind,"origin":before,"decision":if answer{"approve"}else{"deny"}}),
            );
            if !answer {
                return Ok(result("blocked", "Declined."));
            }
            let active_target = self.targets.lock().await.last().cloned();
            if active_target.as_deref() != Some(self.page.target_id().as_ref())
                || origin(&self.url().await?) != before
            {
                return Ok(result(
                    "blocked",
                    "The page changed while approval was pending; review the action again.",
                ));
            }
            if let Some(id) = id {
                let connected = on_node(
                    &self.page,
                    id,
                    "function(){return this.isConnected}".into(),
                    vec![],
                )
                .await?
                .as_bool()
                    == Some(true);
                let focused = kind != "press"
                    || a.get("selector").is_some()
                    || on_node(
                        &self.page,
                        id,
                        "function(){return this===document.activeElement}".into(),
                        vec![],
                    )
                    .await?
                    .as_bool()
                        == Some(true);
                if !connected || !focused || self.describe(id).await? != element {
                    return Ok(result("blocked","The approved target changed while approval was pending; review the action again."));
                }
            }
        }
        let effect = [
            "navigate", "click", "type", "select", "press", "upload", "submit",
        ]
        .contains(&kind.as_str());
        *self.guard.nav_blocked.lock().await = None;
        let dispatched_target = self.page.target_id().as_ref().to_owned();
        let mut output = bounded(
            if kind == "wait" {
                Duration::from_secs(6)
            } else {
                self.config.timeout
            },
            self.dispatch(a, id, typed),
        )
        .await;
        if !self
            .targets
            .lock()
            .await
            .iter()
            .any(|id| id == &dispatched_target)
        {
            output = Err(anyhow!("Target page closed during the action"));
        }
        if let Err(error) = self.refresh_page().await {
            return Ok(result(
                if effect { "uncertain" } else { "blocked" },
                &mask(&error.to_string(), &self.config.secrets),
            ));
        }
        let nav_blocked = self.guard.nav_blocked.lock().await.take();
        if let Some(reason) = nav_blocked {
            let _ = command(&self.page, "Page.getNavigationHistory", json!({})).await;
            return Ok(
                json!({"status":"blocked","origin":origin(&self.url().await.unwrap_or_default()),"reason":reason}),
            );
        }
        let landed = self.url().await.unwrap_or_default();
        if landed.starts_with("http") && !navigation(&landed, &self.config.allowed_domains) {
            if let Ok(history) = command(&self.page, "Page.getNavigationHistory", json!({})).await {
                if let Some(index) = history
                    .get("currentIndex")
                    .and_then(Value::as_u64)
                    .and_then(|i| i.checked_sub(1))
                {
                    if let Some(entry) = history
                        .get("entries")
                        .and_then(Value::as_array)
                        .and_then(|a| a.get(index as usize))
                        .and_then(|e| e.get("id"))
                    {
                        let _ = command(
                            &self.page,
                            "Page.navigateToHistoryEntry",
                            json!({"entryId":entry}),
                        )
                        .await;
                    }
                }
            }
            return Ok(result(
                "blocked",
                "Navigation left this task's allowed list.",
            ));
        }
        match output {
            Ok(Some(evidence)) => Ok(json!({"status":"done","origin":before,"evidence":evidence})),
            Ok(None) => Ok(
                json!({"status":"done","origin":origin(&landed),"downloads":self.downloads.lock().await.iter().map(|v|json!({"name":v.get("name"),"from":v.get("from")})).collect::<Vec<_>>()}),
            ),
            Err(e) => Ok(
                json!({"status":if effect{"uncertain"}else{"blocked"},"origin":origin(&landed),"reason":mask(&public_error(&kind,&e,self.config.timeout),&self.config.secrets)}),
            ),
        }
    }
}
#[async_trait]
impl BrowserEngine for CdpEngine {
    async fn act(&mut self, action: Value, approval: &dyn Approval) -> Result<Value> {
        if self.closed {
            return Ok(
                json!({"status":"blocked","origin":"","reason":"The browser session is closed."}),
            );
        }
        if self.guard.finished() || self.interrupted.load(Ordering::SeqCst) {
            return Ok(
                json!({"status":"blocked","origin":"","reason":"The browser session was interrupted; close it before starting another task."}),
            );
        }
        let mut lease = ActionLease {
            interrupted: self.interrupted.clone(),
            shutdown: self.guard.shutdown.clone(),
            proxy: self._socket_proxy.abort_handle(),
            complete: false,
        };
        let kind = js_string(action.get("type"));
        let log_action = !self.closed;
        let result = self.act_once(action, approval).await;
        if log_action {
            if let Ok(out) = &result {
                (self.log)(
                    json!({"event":"browser.act","jobId":self.job_id,"session":self.session_id,"type":kind,"status":out.get("status"),"origin":out.get("origin"),"reason":out.get("reason").and_then(Value::as_str).unwrap_or("")}),
                );
            }
        }
        lease.complete = true;
        result
    }
    async fn close(&mut self) -> bool {
        if self.close_confirmed {
            return true;
        }
        self.closed = true;
        self._socket_proxy.stop();
        let result = bounded(LIMIT, async {
            let _ = self.guard.shutdown.send(());
            self.browser.wait().await?;
            Ok(())
        })
        .await
        .is_ok();
        self.pump.abort();
        self.download_task.abort();
        self.page_watch.abort();
        if result {
            self.close_confirmed = true;
            (self.log)(
                json!({"event":"browser.closed","jobId":self.job_id,"session":self.session_id}),
            );
            let _ = tokio::fs::remove_dir_all(&self.config.profile_dir).await;
        }
        result
    }
}

impl CdpEngine {
    async fn act_once(&mut self, action: Value, approval: &dyn Approval) -> Result<Value> {
        if self.closed {
            return Ok(
                json!({"status":"blocked","origin":"","reason":"The browser session is closed."}),
            );
        }
        if let Err(error) = self.refresh_page().await {
            return Ok(json!({"status":"blocked","origin":"","reason":error.to_string()}));
        }
        let kind = js_string(action.get("type"));
        if ![
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
        ]
        .contains(&kind.as_str())
        {
            return self.run(&action, approval, None).await;
        }
        let id = if (action.get("selector").is_some() && kind != "navigate") || kind == "press" {
            match self
                .target(action.get("selector").and_then(Value::as_str))
                .await
            {
                Ok(id) => Some(id),
                Err(_) => {
                    return Ok(
                        json!({"status":"blocked","origin":origin(&self.url().await?),"reason":"That element is not on the page."}),
                    )
                }
            }
        } else {
            None
        };
        let result = self.run(&action, approval, id.as_deref()).await;
        if let Some(id) = id {
            let _ = command(&self.page, "Runtime.releaseObject", json!({"objectId":id})).await;
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unsupported_proxy_credentials_fail_before_launch() {
        assert!(validate_proxy("http://proxy.test:3128").is_ok());
        assert!(validate_proxy("http://user:synthetic-password@proxy.test:3128").is_err());
        assert!(validate_proxy("file:///example").is_err());
    }
    #[test]
    fn policy_and_evidence_boundaries() {
        assert!(navigation(
            "https://fixture.test/path",
            &["fixture.test".into()]
        ));
        assert!(!navigation(
            "https://denied.test/path",
            &["fixture.test".into()]
        ));
        assert!(!navigation("http://127.0.0.1/", &["127.0.0.1".into()]));
        let secrets = BTreeMap::from([(
            "key".into(),
            Secret {
                value: "synthetic key".into(),
                domains: vec!["fixture.test".into()],
            },
        )]);
        assert_eq!(
            mask("synthetic key synthetic%20key synthetic+key", &secrets),
            "{{secret:key}} {{secret:key}} {{secret:key}}"
        );
    }
}
