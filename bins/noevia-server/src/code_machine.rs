//! Council p4: the front's side of the Code engine relay (core code-machine.cjs #1853 over
//! chat-machines.cjs and machine-health.cjs), behind `NOEVIA_RUST_CODE_RELAY=1`, which is off by
//! default. Unset (or any other value), a native Code start whose Code role is on an inference
//! machine is refused with 409 exactly as before (code_routes.rs `ON_A_MACHINE`).
//!
//! What this module is:
//! - [`code_target`]: core's `codeTargetFrom(chatMachines, autoRoles)`: the Code role's machine,
//!   else (when the account's roles name no local Code model) the Smart role's, read from the
//!   `inference:machines` row Node owns (engine-admission `chat_target`, read-only);
//! - [`FrontMachines`]: what the relay crate needs of the machines ([`code_engine_relay::Machines`]),
//!   as chat-machines.cjs answers it: `ready` (the #1638 health probe of `/health` at the machine's
//!   origin, then #1851's served-model check of `/v1/models`, both cached ~30 s as Node's shared
//!   probe cache), `contextTokens` (#1854's `/props` `n_ctx`, the 8,192 floor when it does not say),
//!   `fetch` through the on-this-network client (no redirect, no proxy, every name resolved under
//!   the rule) and `connectError` (#1840's texts);
//! - [`start`]: the relay itself, its listener on `CODE_ENGINE_RELAY_PORT` (code-engine-relay
//!   `bind`), started once at boot when the switch is on.
//!
//! code_routes.rs `resolve` asks [`open`] for a lease when core says `machine: true`; the lease
//! (a [`code_engine_relay::Opened`]) is held by the start's inference gate, which lives as long as
//! the Executor run, and closes when the run ends (its `Drop` closes it, cutting any request in
//! flight).
//!
//! Named differences from Node (each a refusal or a cache detail, never a different success):
//! - the relay's per-request re-check reads the roles of the account whose task last opened a lease
//!   (Node's `autoRoles()` there reads whatever account its request context has); it matters only
//!   when the Code role is not assigned and the Smart role is, and Code tasks are administrators';
//! - the probe cache keeps at most 32 entries and drops the oldest, as Node's; a probe in flight is
//!   not shared between two callers (each makes its own request);
//! - a registry row that cannot be read moves nothing (no target), as engine-admission reads it; core's
//!   native authority there refuses with `machinesUnavailable`.
use crate::App;
use code_engine_relay::{Forward, Machines, Ready, Target, Upstream};
use engine_admission::machines;
use js_json::{wtf16, JValue};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The switch.
pub const SWITCH: &str = "NOEVIA_RUST_CODE_RELAY";
const MACHINES_KEY: &str = "inference:machines";
/// machine-health.cjs: `ttlMs`, `timeoutMs`, `maxEntries`.
const HEALTH_TTL: Duration = Duration::from_secs(30);
const HEALTH_TIMEOUT: Duration = Duration::from_millis(2500);
const CACHE_ENTRIES: usize = 32;
/// chat-machines.cjs: `MODELS_TIMEOUT_MS`, `PROPS_TIMEOUT_MS`, the body caps, `MAX_MODELS`.
const MODELS_TIMEOUT: Duration = Duration::from_secs(5);
const PROPS_TIMEOUT: Duration = Duration::from_millis(2500);
const MODELS_BODY_CAP: usize = 256 * 1024;
const PROPS_BODY_CAP: usize = 1024 * 1024;
const MAX_MODELS: usize = 200;
const CONTEXT_MIN: f64 = 2048.0;
const CONTEXT_MAX: f64 = 2_000_000.0;
/// `CONTEXT_FALLBACK`: a server that does not say its context gets the local engine's floor.
pub const CONTEXT_FALLBACK: f64 = 8192.0;

/// Whether the switch is on (exactly `1`), and only while the front runs Code tasks
/// (`NOEVIA_CODE_EXECUTOR=rust`): otherwise core runs them and its own relay must keep
/// `CODE_ENGINE_RELAY_PORT` (noevia#2720).
pub fn enabled(get: &impl Fn(&str) -> Option<String>) -> bool {
    get(SWITCH).as_deref() == Some("1")
        && get(crate::code_routes::SWITCH).as_deref() == Some("rust")
}

/// A chat role on another machine, with the role it was found for (the texts name the role).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeTarget {
    pub machine: machines::Target,
}

impl CodeTarget {
    /// The relay's view of it.
    pub fn relay(&self) -> Target {
        Target {
            machine_id: self.machine.machine_id.clone(),
            machine_name: self.machine.machine_name.clone(),
            base_url: self.machine.base_url.clone(),
            model: self.machine.model.clone(),
        }
    }
}

/// code-machine.cjs `codeTargetFrom`: the Code role's machine; else, when the account's roles name
/// no local Code model, the Smart role's. `row` is the registry row's text, `roles` the account's
/// `auto-roles.json` (`null` when it does not read).
pub fn code_target(row: Option<&str>, roles: &JValue) -> Option<CodeTarget> {
    if let Some(t) = machines::chat_target(row, "code") {
        return Some(CodeTarget { machine: t });
    }
    if roles.get("code").truthy() {
        return None;
    }
    machines::chat_target(row, "smart").map(|t| CodeTarget { machine: t })
}

/// The registry row as Node stores it (text only), `None` when there is none or it cannot be read.
pub fn registry_row(app: &App) -> Option<String> {
    let writer = app.writes.writer().ok()?;
    writer
        .read(|r| {
            r.row(
                "SELECT CASE WHEN typeof(value)='text' THEN value ELSE NULL END FROM settings WHERE key=?1",
                [MACHINES_KEY],
                |row| row.get::<_, Option<String>>(0),
            )
        })
        .ok()?
        .flatten()
}

/// The account's roles file (`null` when it does not read).
pub fn account_roles(app: &App, user: &str) -> JValue {
    let Some(data_dir) = app.config.data_dir.clone() else {
        return JValue::Null;
    };
    server_projects::store::user_dir(&data_dir, user)
        .map(|dir| crate::auto_roles_routes::read_roles(&dir))
        .unwrap_or(JValue::Null)
}

/// machine-health.cjs `healthUrl(base)`: `/health` at the base URL's origin, `None` for anything
/// that is not an http(s) URL.
pub fn health_url(base: &str, path: &str) -> Option<String> {
    let u = reqwest::Url::parse(base.trim()).ok()?;
    if !matches!(u.scheme(), "http" | "https") {
        return None;
    }
    let host = u.host_str()?;
    let host = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    Some(match u.port() {
        Some(p) => format!("{}://{host}:{p}{path}", u.scheme()),
        None => format!("{}://{host}{path}", u.scheme()),
    })
}

/// One probe's result, as machine-health.cjs `probeUrl` gives it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Health {
    Up,
    /// It answered, with this status.
    Down(u16),
    /// It did not answer: `not-on-this-network`, `refused`, `timeout`, `not-found` or `network`.
    Unreachable(&'static str),
    /// No usable URL.
    Unknown,
}

/// `probeUrl`'s status rule (no headers are ever sent for a chat machine): 2xx is up, and so is a
/// 401/403 (it is there and wants a login).
pub fn health_of_status(status: u16) -> Health {
    if (200..300).contains(&status) || status == 401 || status == 403 {
        Health::Up
    } else {
        Health::Down(status)
    }
}

/// `probeUrl`'s reason for a request that failed.
pub fn health_of_error(e: &reqwest::Error) -> Health {
    if model_stream::net::refused_in_chain(e) {
        return Health::Unreachable("not-on-this-network");
    }
    if e.is_timeout() {
        return Health::Unreachable("timeout");
    }
    let text = chain_text(e);
    Health::Unreachable(if text.contains("Connection refused") {
        "refused"
    } else if text.contains("failed to lookup")
        || text.contains("dns error")
        || text.contains("Name or service not known")
    {
        "not-found"
    } else {
        "network"
    })
}

fn chain_text(e: &(dyn std::error::Error + 'static)) -> String {
    let mut out = e.to_string();
    let mut cur = e.source();
    while let Some(s) = cur {
        out.push_str(": ");
        out.push_str(&s.to_string());
        cur = s.source();
    }
    out
}

/// chat-machines.cjs `unreachableText(t, detail)`.
pub fn unreachable_text(t: &machines::Target, detail: &str) -> String {
    let role = label(&t.role);
    let detail = if detail.is_empty() {
        String::new()
    } else {
        format!(": {detail}")
    };
    format!(
        "{role} model\u{2019}s machine ({}) is unreachable{detail}. Check that it is on and serving {} on port {}, or move the {role} role back to this machine in Settings \u{2192} Inference machines.",
        t.machine_name, t.model, t.port
    )
}

/// chat-machines.cjs `wrongModelText(t, served)`.
pub fn wrong_model_text(t: &machines::Target, served: &[String]) -> String {
    let role = label(&t.role);
    let shown = served
        .iter()
        .take(3)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    let shown = if shown.is_empty() {
        "no model".to_string()
    } else {
        shown
    };
    let more = if served.len() > 3 { " and more" } else { "" };
    format!(
        "The {role} machine ({}) serves {shown}{more}, not {}. Start {} on port {}, or choose the model it serves for the {role} role in Settings \u{2192} Inference machines.",
        t.machine_name, t.model, t.model, t.port
    )
}

fn label(role: &str) -> &str {
    match role {
        "fast" => "Fast",
        "smart" => "Smart",
        "code" => "Code",
        "creative" => "Creative",
        "vision" => "Vision",
        other => other,
    }
}

/// chat-machines.cjs `ready(t)` after the probe: the health result, then (only when up) the served
/// models (`None`: they could not be read, which blames nothing).
pub fn ready_of(t: &machines::Target, health: &Health, models: Option<&[String]>) -> Ready {
    let detail = match health {
        Health::Up => {
            return match models {
                Some(served) if !served.contains(&t.model) => Ready {
                    ok: false,
                    code: Some("machineWrongModel".into()),
                    error: Some(wrong_model_text(t, served)),
                },
                _ => Ready::ok(),
            }
        }
        Health::Unreachable("not-on-this-network") => "its address is not on this network".into(),
        // `it answered ${reason.replace(/^http-/, 'HTTP ')}`.trim()
        Health::Down(status) => format!("it answered HTTP {status}"),
        Health::Unreachable("timeout") => "no answer in time".into(),
        Health::Unreachable(_) | Health::Unknown => String::new(),
    };
    Ready {
        ok: false,
        code: Some("machineUnreachable".into()),
        error: Some(unreachable_text(t, &detail)),
    }
}

/// chat-machines.cjs `modelIds(body)`.
pub fn model_ids(body: &JValue) -> Vec<String> {
    let rows = match (body.get("data"), body.get("models")) {
        (JValue::Arr(rows), _) | (_, JValue::Arr(rows)) => rows.as_slice(),
        _ => &[],
    };
    let mut out: Vec<String> = Vec::new();
    for r in rows {
        let id = match r {
            JValue::Str(s) => Some(s.clone()),
            _ => {
                // `r?.id ?? r?.model ?? r?.name`: the first that is neither null nor undefined.
                let pick = [r.get("id"), r.get("model"), r.get("name")]
                    .into_iter()
                    .find(|v| !matches!(v, JValue::Null | JValue::Undefined));
                pick.and_then(JValue::as_str).map(str::to_string)
            }
        };
        if let Some(id) = id {
            let ok = !id.trim().is_empty()
                && wtf16::len(&id) <= 200
                && !id.chars().any(|c| (c as u32) < 0x20 || c as u32 == 0x7f)
                && !out.contains(&id);
            if ok {
                out.push(id);
            }
        }
        if out.len() >= MAX_MODELS {
            break;
        }
    }
    out
}

/// #1854 `propsFactsOf(body).contextTokens`: `n_ctx` when it is a whole number in range.
pub fn props_context(body: &JValue) -> Option<f64> {
    let n = js_number(body.get("default_generation_settings").get("n_ctx"))?;
    (n.fract() == 0.0 && (CONTEXT_MIN..=CONTEXT_MAX).contains(&n)).then_some(n)
}

/// JS `Number(v)` for the values a JSON body can hold (`None`: NaN).
fn js_number(v: &JValue) -> Option<f64> {
    match v {
        JValue::Num(n) => Some(*n),
        JValue::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        JValue::Null => Some(0.0),
        JValue::Str(s) => {
            let t = s.trim();
            if t.is_empty() {
                Some(0.0)
            } else {
                t.parse::<f64>().ok().filter(|n| n.is_finite())
            }
        }
        _ => None,
    }
}

/// What the relay needs of the machines, as chat-machines.cjs answers it.
pub struct FrontMachines {
    client: reqwest::Client,
    /// The role each target was found for (the texts name it): base URL + model → role, port.
    found: Mutex<HashMap<String, machines::Target>>,
    health: Mutex<Cache<Health>>,
    facts: Mutex<Cache<Facts>>,
}

#[derive(Debug, Clone, Default, PartialEq)]
struct Facts {
    models: Option<Vec<String>>,
    context: Option<f64>,
}

struct Cache<T> {
    entries: Vec<(String, Instant, T)>,
}

impl<T: Clone> Cache<T> {
    fn new() -> Cache<T> {
        Cache {
            entries: Vec::new(),
        }
    }
    fn get(&self, key: &str, now: Instant) -> Option<T> {
        self.entries
            .iter()
            .find(|(k, at, _)| k == key && now.duration_since(*at) < HEALTH_TTL)
            .map(|(_, _, v)| v.clone())
    }
    fn put(&mut self, key: String, now: Instant, value: T) {
        self.entries.retain(|(k, _, _)| *k != key);
        self.entries.push((key, now, value));
        while self.entries.len() > CACHE_ENTRIES {
            self.entries.remove(0);
        }
    }
}

/// The machine's answer to one forwarded request.
pub struct MachineAnswer(reqwest::Response);

impl Upstream for MachineAnswer {
    fn status(&self) -> u16 {
        self.0.status().as_u16()
    }
    fn content_type(&self) -> Option<String> {
        self.0
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    }
    async fn next(&mut self) -> Option<Result<Vec<u8>, ()>> {
        match self.0.chunk().await {
            Ok(Some(b)) => Some(Ok(b.to_vec())),
            Ok(None) => None,
            Err(_) => Some(Err(())),
        }
    }
}

/// A forwarded request that never reached the machine.
#[derive(Debug)]
pub struct Unsent {
    refused_address: bool,
    redirect: bool,
    timeout: bool,
}

async fn read_capped(r: reqwest::Response, cap: usize) -> Option<String> {
    let mut r = r;
    let mut out = Vec::new();
    while let Some(chunk) = r.chunk().await.ok()? {
        out.extend_from_slice(&chunk);
        if out.len() > cap {
            return None;
        }
    }
    Some(String::from_utf8_lossy(&out).into_owned())
}

impl FrontMachines {
    pub fn new() -> Result<FrontMachines, String> {
        Ok(FrontMachines::with_client(
            model_stream::net::this_network_client()?,
        ))
    }

    /// With a given client (tests reach a loopback machine with a plain one).
    pub fn with_client(client: reqwest::Client) -> FrontMachines {
        FrontMachines {
            client,
            found: Mutex::new(HashMap::new()),
            health: Mutex::new(Cache::new()),
            facts: Mutex::new(Cache::new()),
        }
    }

    /// Remember the role a relay target was found for (the texts name the role and the port).
    pub fn remember(&self, t: &CodeTarget) {
        let key = format!("{}#{}", t.machine.base_url, t.machine.model);
        if let Ok(mut f) = self.found.lock() {
            f.insert(key, t.machine.clone());
        }
    }

    fn full(&self, t: &Target) -> machines::Target {
        let key = format!("{}#{}", t.base_url, t.model);
        self.found
            .lock()
            .ok()
            .and_then(|f| f.get(&key).cloned())
            .unwrap_or_else(|| machines::Target {
                role: "code".into(),
                machine_id: t.machine_id.clone(),
                machine_name: t.machine_name.clone(),
                port: port_of(&t.base_url),
                model: t.model.clone(),
                base_url: t.base_url.clone(),
            })
    }

    async fn probe(&self, base: &str) -> Health {
        let Some(url) = health_url(base, "/health") else {
            return Health::Unknown;
        };
        let now = Instant::now();
        if let Some(hit) = self.health.lock().ok().and_then(|c| c.get(&url, now)) {
            return hit;
        }
        let result = if !model_stream::net::url_allowed(&url) {
            Health::Unreachable("not-on-this-network")
        } else {
            match self.client.get(&url).timeout(HEALTH_TIMEOUT).send().await {
                Ok(r) => health_of_status(r.status().as_u16()),
                Err(e) => health_of_error(&e),
            }
        };
        if let Ok(mut c) = self.health.lock() {
            c.put(url, Instant::now(), result.clone());
        }
        result
    }

    async fn get_json(&self, url: &str, timeout: Duration, cap: usize) -> Option<JValue> {
        if !model_stream::net::url_allowed(url) {
            return None;
        }
        let r = self.client.get(url).timeout(timeout).send().await.ok()?;
        if !r.status().is_success() {
            return None;
        }
        wtf16::parse(&read_capped(r, cap).await?).ok()
    }

    /// `facts(t)`: the served models and the context, cached beside the health result; a read that
    /// fails keeps the last known value.
    async fn facts(&self, t: &Target) -> Facts {
        let key = format!("{}#facts#{}", t.base_url, t.model);
        let now = Instant::now();
        let prev = {
            let Ok(c) = self.facts.lock() else {
                return Facts::default();
            };
            if let Some(hit) = c.get(&key, now) {
                return hit;
            }
            c.entries
                .iter()
                .find(|(k, _, _)| *k == key)
                .map(|(_, _, v)| v.clone())
                .unwrap_or_default()
        };
        let models_url = format!("{}/v1/models", t.base_url);
        let props_url = format!(
            "{}/props?model={}",
            t.base_url,
            encode_uri_component(&t.model)
        );
        let (models, props) = tokio::join!(
            self.get_json(&models_url, MODELS_TIMEOUT, MODELS_BODY_CAP),
            self.get_json(&props_url, PROPS_TIMEOUT, PROPS_BODY_CAP),
        );
        let out = Facts {
            models: models.map(|b| model_ids(&b)).or(prev.models),
            context: props.as_ref().and_then(props_context).or(prev.context),
        };
        if let Ok(mut c) = self.facts.lock() {
            c.put(key, Instant::now(), out.clone());
        }
        out
    }
}

fn port_of(base: &str) -> u32 {
    reqwest::Url::parse(base)
        .ok()
        .and_then(|u| u.port_or_known_default())
        .map_or(0, u32::from)
}

/// JS `encodeURIComponent`.
pub fn encode_uri_component(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

impl Machines for FrontMachines {
    type Upstream = MachineAnswer;
    type Error = Unsent;

    async fn ready(&self, target: &Target) -> Ready {
        let t = self.full(target);
        let health = self.probe(&t.base_url).await;
        if health != Health::Up {
            return ready_of(&t, &health, None);
        }
        let facts = self.facts(target).await;
        ready_of(&t, &health, facts.models.as_deref())
    }

    async fn context_tokens(&self, target: &Target) -> Option<f64> {
        Some(self.facts(target).await.context.unwrap_or(CONTEXT_FALLBACK))
    }

    async fn fetch(&self, request: Forward) -> Result<MachineAnswer, Unsent> {
        if !model_stream::net::url_allowed(&request.url) {
            return Err(Unsent {
                refused_address: true,
                redirect: false,
                timeout: false,
            });
        }
        let sent = self
            .client
            .post(&request.url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(reqwest::header::ACCEPT, request.accept)
            .body(request.body)
            .send()
            .await
            .map_err(|e| Unsent {
                refused_address: model_stream::net::refused_in_chain(&e),
                redirect: e.is_redirect(),
                timeout: e.is_timeout(),
            })?;
        if sent.status().is_redirection() {
            // `redirect: 'error'`: a redirect is a failed fetch, never followed.
            return Err(Unsent {
                refused_address: false,
                redirect: true,
                timeout: false,
            });
        }
        Ok(MachineAnswer(sent))
    }

    fn connect_error(&self, target: &Target, error: Option<&Unsent>) -> String {
        let t = self.full(target);
        match error {
            None => unreachable_text(&t, "no answer in time"),
            Some(e) if e.timeout => unreachable_text(&t, "no answer in time"),
            Some(e) => machines::connect_error(&t, e.refused_address, e.redirect),
        }
    }
}

/// The front's relay.
pub type FrontRelay = code_engine_relay::Relay<FrontMachines>;

/// The relay's state in the front: the relay, and the account whose task last opened a lease (the
/// per-request re-check reads its roles).
pub struct RelayState {
    pub relay: Arc<FrontRelay>,
    pub machines: Arc<FrontMachines>,
    user: Arc<Mutex<Option<String>>>,
}

fn log_relay(entry: &JValue) {
    println!(
        "[code-relay] {}",
        wtf16::stringify(entry).unwrap_or_default()
    );
}

/// Build the relay over the front's registry. `url` is the relay as the sandbox reaches it (`None`:
/// no relay; a Machine Code start then refuses with `machineRelayOff`).
pub fn relay_over(
    app: std::sync::Weak<App>,
    machines: Arc<FrontMachines>,
    url: Option<String>,
) -> RelayState {
    let user: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let (who, m) = (Arc::clone(&user), Arc::clone(&machines));
    let target: code_engine_relay::TargetFn = Arc::new(move || {
        let app = app.upgrade()?;
        let roles = who
            .lock()
            .ok()
            .and_then(|u| u.clone())
            .map_or(JValue::Null, |u| account_roles(&app, &u));
        let t = code_target(registry_row(&app).as_deref(), &roles)?;
        m.remember(&t);
        Some(t.relay())
    });
    let mut o = code_engine_relay::Options::new(Arc::clone(&machines), target, url);
    o.log = Arc::new(|e| log_relay(&e));
    RelayState {
        relay: Arc::new(code_engine_relay::Relay::new(o)),
        machines,
        user,
    }
}

impl RelayState {
    /// Open the relay for one Executor run of `user`'s task.
    pub async fn open(
        &self,
        user: &str,
        task_id: &str,
        model: Option<&str>,
    ) -> Result<code_engine_relay::Opened, code_engine_relay::Refusal> {
        if let Ok(mut u) = self.user.lock() {
            *u = Some(user.to_string());
        }
        self.relay.open(task_id, model, None).await
    }
}

/// This machine's interface addresses (`os.networkInterfaces()`): the IPv4 addresses the kernel
/// keeps as local (`/proc/net/fib_trie`) and the IPv6 ones (`/proc/net/if_inet6`).
pub fn interface_addresses() -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(trie) = std::fs::read_to_string("/proc/net/fib_trie") {
        out.extend(local_v4(&trie));
    }
    if let Ok(v6) = std::fs::read_to_string("/proc/net/if_inet6") {
        out.extend(local_v6(&v6));
    }
    out
}

/// The `/32 host LOCAL` entries of a fib_trie listing.
pub fn local_v4(trie: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut last: Option<&str> = None;
    for line in trie.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("|-- ") {
            last = Some(rest.trim());
        } else if t.contains("/32 host LOCAL") {
            if let Some(a) = last {
                if a.parse::<std::net::Ipv4Addr>().is_ok() && !out.iter().any(|x| x == a) {
                    out.push(a.to_string());
                }
            }
        }
    }
    out
}

/// The addresses of an if_inet6 listing (32 hex digits per line).
pub fn local_v6(listing: &str) -> Vec<String> {
    listing
        .lines()
        .filter_map(|l| {
            let hex = l.split_whitespace().next()?;
            if hex.len() != 32 {
                return None;
            }
            let mut bytes = [0u8; 16];
            for (i, b) in bytes.iter_mut().enumerate() {
                *b = u8::from_str_radix(hex.get(i * 2..i * 2 + 2)?, 16).ok()?;
            }
            Some(std::net::Ipv6Addr::from(bytes).to_string())
        })
        .collect()
}

async fn lookup(host: &str) -> Result<String, String> {
    let found = tokio::net::lookup_host((host, 0))
        .await
        .map_err(|e| e.to_string())?;
    found
        .map(|a| a.ip().to_string())
        .next()
        .ok_or_else(|| format!("getaddrinfo ENOTFOUND {host}"))
}

/// Start the relay at boot when the switch is on: read `CODE_ENGINE_RELAY_PORT/HOST/BIND` as Node
/// reads them, pick the bind address (a name that does not resolve: loopback; an address that is not
/// this machine's: the relay off), bind the listener (a bind that fails turns the relay off) and
/// serve until the process ends. The relay is never the reason the front does not start: a host
/// that is not a host name (Node's start error) leaves the relay off, logged. `None`: switch off.
pub async fn start(
    app: &Arc<App>,
    get: &(impl Fn(&str) -> Option<String> + Sync),
) -> Option<Arc<RelayState>> {
    if !enabled(get) {
        return None;
    }
    let machines = match FrontMachines::new() {
        Ok(m) => Arc::new(m),
        Err(e) => {
            log_relay(&JValue::obj([
                ("event", JValue::from("relay.bind_failed")),
                ("error", JValue::from(e)),
            ]));
            return None;
        }
    };
    let env = match code_engine_relay::bind::from_env(get) {
        Ok(env) => env,
        Err(reason) => {
            log_relay(&JValue::obj([
                ("event", JValue::from("relay.bind_failed")),
                ("error", JValue::from(reason)),
            ]));
            return Some(Arc::new(relay_over(Arc::downgrade(app), machines, None)));
        }
    };
    let state = Arc::new(relay_over(Arc::downgrade(app), machines, env.url.clone()));
    let Some(port) = env.port else {
        return Some(state);
    };
    let looked = match &env.bind {
        code_engine_relay::bind::Bind::Lookup(h) => Some(lookup(h).await),
        code_engine_relay::bind::Bind::Explicit(_) => None,
    };
    let (address, fallback) = env
        .bind
        .address(|h| looked.unwrap_or(Err(format!("getaddrinfo ENOTFOUND {h}"))));
    if let Some(entry) = fallback {
        log_relay(&entry);
    }
    if let Some(reason) = code_engine_relay::bind::bind_problem(&address, &interface_addresses()) {
        state.relay.disable();
        log_relay(&code_engine_relay::bind::rejected(port, &address, reason));
        return Some(state);
    }
    let bare = address.split('%').next().unwrap_or(&address).to_string();
    let listener = match tokio::net::TcpListener::bind((bare.as_str(), port)).await {
        Ok(l) => l,
        Err(e) => {
            state.relay.disable();
            log_relay(&JValue::obj([
                ("event", JValue::from("relay.bind_failed")),
                ("port", JValue::Num(f64::from(port))),
                ("error", JValue::from(e.to_string())),
            ]));
            return Some(state);
        }
    };
    log_relay(&JValue::obj([
        ("event", JValue::from("relay.listening")),
        ("bind", JValue::from(address.clone())),
        ("port", JValue::Num(f64::from(port))),
    ]));
    let relay = Arc::clone(&state.relay);
    tokio::spawn(code_engine_relay::listener::serve(
        listener,
        relay,
        code_engine_relay::bind::SERVER_LIMITS,
        std::future::pending::<()>(),
    ));
    Some(state)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    const ROW: &str = r#"{"version":1,"machines":[{"id":"qa-m1","name":"QA Box","address":"http://10.9.0.7"}],"assignments":{"chatSmart":{"machine":"qa-m1","port":8081,"model":"qa-smart"},"chatCode":{"machine":"qa-m1","port":8080,"model":"qa-code"}}}"#;
    const SMART_ONLY: &str = r#"{"version":1,"machines":[{"id":"qa-m1","name":"QA Box","address":"http://10.9.0.7"}],"assignments":{"chatSmart":{"machine":"qa-m1","port":8081,"model":"qa-smart"}}}"#;

    fn roles(text: &str) -> JValue {
        wtf16::parse(text).unwrap()
    }

    #[test]
    fn the_switch_is_exactly_1() {
        for (v, on) in [
            (Some("1"), true),
            (Some("true"), false),
            (Some(" 1"), false),
            (Some(""), false),
            (None, false),
        ] {
            let get = |k: &str| match k {
                SWITCH => v.map(str::to_string),
                crate::code_routes::SWITCH => Some("rust".to_string()),
                _ => None,
            };
            assert_eq!(enabled(&get), on, "{v:?}");
            // Core runs Code tasks: the front never takes the relay port.
            let core_runs = |k: &str| (k == SWITCH).then(|| v.map(str::to_string)).flatten();
            assert!(!enabled(&core_runs), "{v:?}");
        }
    }

    #[test]
    fn code_target_is_code_then_smart_unless_a_local_code_model_is_named() {
        let t = code_target(Some(ROW), &JValue::Null).unwrap();
        assert_eq!(
            (t.machine.role.as_str(), t.machine.model.as_str()),
            ("code", "qa-code")
        );
        assert_eq!(t.machine.base_url, "http://10.9.0.7:8080");
        // The Code role on a machine wins over a local Code model (code-machine.cjs: `code` first).
        let t = code_target(Some(ROW), &roles(r#"{"code":"qa-local"}"#)).unwrap();
        assert_eq!(t.machine.model, "qa-code");
        // Only Smart on a machine: it is the fallback...
        let t = code_target(Some(SMART_ONLY), &roles(r#"{"smart":"x"}"#)).unwrap();
        assert_eq!(
            (t.machine.role.as_str(), t.machine.model.as_str()),
            ("smart", "qa-smart")
        );
        assert!(code_target(Some(SMART_ONLY), &JValue::Null).is_some());
        // ...unless the account names a local Code model (any truthy value).
        assert!(code_target(Some(SMART_ONLY), &roles(r#"{"code":"qa-local"}"#)).is_none());
        assert!(code_target(Some(SMART_ONLY), &roles(r#"{"code":""}"#)).is_some());
        // Nothing assigned, no row, a damaged row: this machine.
        assert!(code_target(None, &JValue::Null).is_none());
        assert!(code_target(Some("not json"), &JValue::Null).is_none());
        let relay = code_target(Some(ROW), &JValue::Null).unwrap().relay();
        assert_eq!(
            code_engine_relay::target_key(Some(&relay)).unwrap(),
            "qa-m1|http://10.9.0.7:8080|qa-code"
        );
        assert_eq!(relay.machine_name, "QA Box");
    }

    fn target(role: &str) -> machines::Target {
        machines::Target {
            role: role.into(),
            machine_id: "qa-m1".into(),
            machine_name: "Synthetic Pi".into(),
            port: 8080,
            model: "synthetic-remote-fast".into(),
            base_url: "http://10.9.0.7:8080".into(),
        }
    }

    #[test]
    fn ready_answers_with_node_s_codes_and_texts() {
        let t = target("fast");
        let up = ready_of(&t, &Health::Up, None);
        assert_eq!(up, Ready::ok());
        let served = vec!["synthetic-remote-fast".to_string()];
        assert_eq!(ready_of(&t, &Health::Up, Some(&served)), Ready::ok());
        // chat-machines-1840.test.cjs line 345: the wrong model.
        let wrong = ready_of(&t, &Health::Up, Some(&["synthetic-other-gguf".to_string()]));
        assert_eq!(wrong.code.as_deref(), Some("machineWrongModel"));
        assert_eq!(
            wrong.error.as_deref().unwrap(),
            "The Fast machine (Synthetic Pi) serves synthetic-other-gguf, not synthetic-remote-fast. Start synthetic-remote-fast on port 8080, or choose the model it serves for the Fast role in Settings \u{2192} Inference machines."
        );
        let four: Vec<String> = ["a", "b", "c", "d"].iter().map(|s| s.to_string()).collect();
        assert!(ready_of(&t, &Health::Up, Some(&four))
            .error
            .unwrap()
            .starts_with("The Fast machine (Synthetic Pi) serves a, b, c and more, not "));
        assert!(ready_of(&t, &Health::Up, Some(&[]))
            .error
            .unwrap()
            .contains("serves no model, not "));
        let base = "Fast model\u{2019}s machine (Synthetic Pi) is unreachable";
        let tail = ". Check that it is on and serving synthetic-remote-fast on port 8080, or move the Fast role back to this machine in Settings \u{2192} Inference machines.";
        for (h, detail) in [
            (
                Health::Unreachable("not-on-this-network"),
                ": its address is not on this network",
            ),
            (Health::Down(503), ": it answered HTTP 503"),
            (Health::Unreachable("timeout"), ": no answer in time"),
            (Health::Unreachable("refused"), ""),
            (Health::Unreachable("network"), ""),
            (Health::Unknown, ""),
        ] {
            let r = ready_of(&t, &h, Some(&served));
            assert!(!r.ok);
            assert_eq!(r.code.as_deref(), Some("machineUnreachable"), "{h:?}");
            assert_eq!(r.error.unwrap(), format!("{base}{detail}{tail}"), "{h:?}");
        }
        // The Code role's label.
        assert!(ready_of(&target("code"), &Health::Unknown, None)
            .error
            .unwrap()
            .starts_with("Code model\u{2019}s machine"));
    }

    #[test]
    fn the_probe_status_rule_is_machine_health_s() {
        for s in [200, 204, 299, 401, 403] {
            assert_eq!(health_of_status(s), Health::Up, "{s}");
        }
        for s in [301, 404, 500, 503] {
            assert_eq!(health_of_status(s), Health::Down(s), "{s}");
        }
    }

    #[test]
    fn health_url_is_health_at_the_origin() {
        assert_eq!(
            health_url("http://10.9.0.7:8080", "/health").as_deref(),
            Some("http://10.9.0.7:8080/health")
        );
        assert_eq!(
            health_url("  https://box.lan/v1/x?y ", "/health").as_deref(),
            Some("https://box.lan/health")
        );
        assert_eq!(
            health_url("http://[fd7a::1]:81", "/health").as_deref(),
            Some("http://[fd7a::1]:81/health")
        );
        // The default port is not written (Node's `u.host` leaves it out).
        assert_eq!(
            health_url("http://box.lan:80", "/health").as_deref(),
            Some("http://box.lan/health")
        );
        for bad in ["", "ftp://x", "box.lan:8080", "not a url"] {
            assert_eq!(health_url(bad, "/health"), None, "{bad}");
        }
    }

    #[test]
    fn model_ids_reads_data_then_models() {
        let ids = |t: &str| model_ids(&wtf16::parse(t).unwrap());
        assert_eq!(
            ids(
                r#"{"data":[{"id":"a"},{"id":"a"},{"model":"b"},{"name":"c"},"d",{"id":" "},{"id":5},{"id":"x\u0001"},null]}"#
            ),
            ["a", "b", "c", "d"]
        );
        assert_eq!(ids(r#"{"models":[{"name":"m"}]}"#), ["m"]);
        // `data` wins when it is an array, even empty.
        assert!(ids(r#"{"data":[],"models":[{"name":"m"}]}"#).is_empty());
        assert_eq!(ids(r#"{"data":{},"models":[{"name":"m"}]}"#), ["m"]);
        // `id ?? model ?? name`: a null id falls through, a non-string id does not.
        assert_eq!(ids(r#"{"data":[{"id":null,"model":"b"}]}"#), ["b"]);
        assert!(ids(r#"{"data":[{"id":7,"model":"b"}]}"#).is_empty());
        let long = "x".repeat(201);
        assert!(ids(&format!(r#"{{"data":["{long}"]}}"#)).is_empty());
        let many: Vec<String> = (0..250).map(|i| format!("\"m{i}\"")).collect();
        assert_eq!(
            ids(&format!(r#"{{"data":[{}]}}"#, many.join(","))).len(),
            200
        );
        assert!(ids("null").is_empty());
    }

    #[test]
    fn props_context_is_a_whole_n_ctx_in_range() {
        let ctx = |t: &str| props_context(&wtf16::parse(t).unwrap());
        assert_eq!(
            ctx(r#"{"default_generation_settings":{"n_ctx":32768}}"#),
            Some(32768.0)
        );
        assert_eq!(
            ctx(r#"{"default_generation_settings":{"n_ctx":"4096"}}"#),
            Some(4096.0)
        );
        for bad in [
            r#"{"default_generation_settings":{"n_ctx":2047}}"#,
            r#"{"default_generation_settings":{"n_ctx":2000001}}"#,
            r#"{"default_generation_settings":{"n_ctx":4096.5}}"#,
            r#"{"default_generation_settings":{"n_ctx":"x"}}"#,
            r#"{"default_generation_settings":{}}"#,
            r#"{}"#,
        ] {
            assert_eq!(ctx(bad), None, "{bad}");
        }
        assert_eq!(
            ctx(r#"{"default_generation_settings":{"n_ctx":2048}}"#),
            Some(2048.0)
        );
    }

    #[test]
    fn encode_uri_component_is_js_s() {
        assert_eq!(encode_uri_component("qa-model_1.gguf"), "qa-model_1.gguf");
        assert_eq!(encode_uri_component("a b/c?d&é"), "a%20b%2Fc%3Fd%26%C3%A9");
        assert_eq!(encode_uri_component("!~*'()"), "!~*'()");
    }

    #[test]
    fn interface_listings_are_read() {
        let trie = "Main:\n  +-- 0.0.0.0/0 3 0 5\n     |-- 0.0.0.0\n        /0 universe UNICAST\n     +-- 127.0.0.0/8 2 0 2\n        |-- 127.0.0.1\n           /32 host LOCAL\n     |-- 172.18.0.5\n        /32 host LOCAL\n     |-- 172.18.255.255\n        /32 link BROADCAST\nLocal:\n     |-- 172.18.0.5\n        /32 host LOCAL\n";
        assert_eq!(local_v4(trie), ["127.0.0.1", "172.18.0.5"]);
        let v6 = "00000000000000000000000000000001 01 80 10 80       lo\nfe800000000000000042acfffe120005 1f 40 20 80     eth0\nbad\n";
        assert_eq!(local_v6(v6), ["::1", "fe80::42:acff:fe12:5"]);
    }

    #[test]
    fn the_cache_keeps_32_entries_and_expires() {
        let mut c: Cache<u8> = Cache::new();
        let t0 = Instant::now();
        for i in 0..40u8 {
            c.put(format!("k{i}"), t0, i);
        }
        assert_eq!(c.entries.len(), CACHE_ENTRIES);
        assert_eq!(c.get("k0", t0), None);
        assert_eq!(c.get("k39", t0), Some(39));
        assert_eq!(c.get("k39", t0 + HEALTH_TTL), None);
        c.put("k39".into(), t0, 1);
        assert_eq!(c.get("k39", t0), Some(1));
        assert_eq!(c.entries.len(), CACHE_ENTRIES);
    }
}
