//! `GET|PUT|DELETE /api/admin/decision-settings` and `POST /api/admin/decision-settings/test`
//! (sbstndalton/noevia#2190), ported from core routes/features.cjs and decision-settings.cjs:
//! Settings -> Models -> Decision service, admin only. Permanent, no switch. Rust is the single
//! writer of the `decision:configuration` settings row (core rust-decision-settings.cjs refuses
//! the routes with `RUST_DECISION_SETTINGS_OWNED`, core rust-auth.cjs refuses the key on Node's
//! connection). Node's decision path re-reads the row by its content fingerprint on every use
//! (core decision-settings.cjs, shared-reread.cjs), so a save here is seen by the next decision
//! without a restart.
//!
//! Order follows core: the session gate (401, CSRF 403), `Administrator required` (403), the 405s
//! (`/test` takes POST, the setting GET, PUT and DELETE), the body (1 MiB, 413; `invalid JSON`
//! 400; an empty body is `{}`), then validation. `PUT {"reset":true}` is DELETE. Every save and
//! reset appends its audit row (`decision.configure`, `decision.reset`) in the same transaction.
//!
//! The view is Node's `get()`: the address an Inference machine gives the decision service (the
//! `inference:machines` row, read as core inference-machines.cjs `readState`/`urlFor`) wins over
//! the saved URL, which wins over the deployment's `COWORK_DECISION_URL`.
//!
//! The connection test resolves a name now and refuses it unless every answer is on this network
//! (core service-endpoint.cjs `checkServiceEndpoint`, link-local allowed: the decision service's
//! rule), then GETs `<url>/health` through a client whose resolver applies the same rule at
//! connect time (DNS rebinding), with no redirect followed, no environment proxy, a 3 s deadline
//! and a 4 KiB body cap; `{"ready":true}` is ready, anything else the one 502.

use crate::{identity, reply, App};
use axum::body::Body;
use axum::http::{Method, Request, Response, StatusCode};
use axum::response::IntoResponse;
use http_body_util::BodyExt;
use js_json::JValue;
use server_store::{Reader, RustDecisionSettings, StoreError};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

pub const SETTINGS: &str = "/api/admin/decision-settings";
pub const TEST: &str = "/api/admin/decision-settings/test";
pub const ROUTES: &[&str] = &[SETTINGS, TEST];

const KEY: &str = RustDecisionSettings::KEY;
const MACHINES_KEY: &str = "inference:machines";
const SOURCE_LIMIT: i64 = 1024 * 1024;
const BODY_LIMIT: usize = 1024 * 1024;
const BODY_DEADLINE: Duration = Duration::from_secs(10);
const HEALTH_DEADLINE: Duration = Duration::from_secs(3);
const HEALTH_CAP: usize = 4096;
const LOOKUP_DEADLINE: Duration = Duration::from_secs(5);
/// core decision-models.cjs PROFILES: the decision model ids an administrator can choose.
const PROFILES: &[&str] = &["laya", "kev-0.8b"];

const INVALID_INPUT: &str =
    "Enter a private decision-service URL and a deadline from 100 to 2000 ms.";
const INVALID_URL: &str =
    "Use a private HTTP address such as http://192.168.1.20:8040, with no path, login or query string.";
const INVALID_MODEL: &str = "Choose a decision model from the list, or automatic.";
const NOT_READY: &str =
    "The decision service is not ready or could not be reached. Check the URL and service status.";
const READY: &str = "Decision service is ready. No inference was run.";

/// The deployment's decision configuration (config.rs, from the environment Node reads too).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Deployment {
    /// `COWORK_DECISION_URL` exactly as set ('' when unset), as Node's `env.COWORK_DECISION_URL||''`.
    pub url: String,
}

impl Deployment {
    pub fn from_lookup(get: &impl Fn(&str) -> Option<String>) -> Self {
        Deployment {
            url: get("COWORK_DECISION_URL").unwrap_or_default(),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// core service-endpoint.cjs: where an operator's own services may be.

fn js_space(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{b}' | '\u{c}' | '\r' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200a}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202f}'
                | '\u{205f}'
                | '\u{3000}'
                | '\u{feff}'
    )
}

fn js_trim(s: &str) -> &str {
    s.trim_matches(js_space)
}

const METADATA_V4: [Ipv4Addr; 2] = [
    Ipv4Addr::new(169, 254, 169, 254),
    Ipv4Addr::new(100, 100, 100, 200),
];
const METADATA_V6: Ipv6Addr = Ipv6Addr::new(0xfd00, 0xec2, 0, 0, 0, 0, 0, 0x254);

fn denied_v4(ip: Ipv4Addr, link_local: bool) -> bool {
    let [a, b, c, d] = ip.octets();
    if a == 0 || METADATA_V4.contains(&ip) {
        return true;
    }
    if a == 169 && b == 254 {
        return !link_local || (c == 169 && d == 254);
    }
    a >= 224
        || (a == 192 && b == 0 && (c == 0 || c == 2))
        || (a == 198 && b == 51 && c == 100)
        || (a == 203 && b == 0 && c == 113)
}

fn local_v4(ip: Ipv4Addr) -> bool {
    let [a, b, ..] = ip.octets();
    a == 127
        || a == 10
        || (a == 172 && (16..=31).contains(&b))
        || (a == 192 && b == 168)
        || (a == 100 && (64..=127).contains(&b))
        || (a == 169 && b == 254)
}

/// core `whyNotLocal(ip, {linkLocal}) === null`: on this network, and not denied.
pub fn is_local(ip: IpAddr, link_local: bool) -> bool {
    match ip {
        IpAddr::V4(v4) => !denied_v4(v4, link_local) && local_v4(v4),
        IpAddr::V6(v6) => {
            let h = v6.segments();
            if v6 == METADATA_V6 {
                return false;
            }
            let zero5 = h.iter().take(5).all(|x| *x == 0);
            if zero5 && h[5] == 0xffff {
                let v4 = Ipv4Addr::new(
                    (h[6] >> 8) as u8,
                    (h[6] & 255) as u8,
                    (h[7] >> 8) as u8,
                    (h[7] & 255) as u8,
                );
                return !denied_v4(v4, link_local) && local_v4(v4);
            }
            if h.iter().take(7).all(|x| *x == 0) {
                return h[7] == 1; // ::1 is loopback; :: and the rest are refused
            }
            if h.iter().take(6).all(|x| *x == 0) {
                return false; // v4-compatible
            }
            if (h[0] & 0xffc0) == 0xfe80 {
                return link_local;
            }
            if (h[0] & 0xff00) == 0xff00
                || (h[0] == 0x64 && h[1] == 0xff9b)
                || (h[0] == 0x2001 && (h[1] == 0 || h[1] == 0xdb8))
                || (h[0] == 0x100 && h[1..4].iter().all(|x| *x == 0))
            {
                return false;
            }
            (h[0] & 0xfe00) == 0xfc00
        }
    }
}

/// A parsed service URL: its origin, and the name to resolve when it is not an IP literal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceUrl {
    pub base_url: String,
    pub name: Option<String>,
}

/// core `parseServiceUrl(raw, {paths:['/'], linkLocal})`; `None` is any refusal (callers word it).
pub fn parse_service_url(raw: &str, link_local: bool) -> Option<ServiceUrl> {
    let url = url::Url::parse(js_trim(raw)).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    if !url.username().is_empty() || url.password().is_some() {
        return None;
    }
    if url.query().is_some_and(|q| !q.is_empty())
        || url.fragment().is_some_and(|f| !f.is_empty())
        || raw.contains('?')
        || raw.contains('#')
    {
        return None;
    }
    if url.path() != "/" {
        return None;
    }
    let base_url = url.origin().ascii_serialization();
    match url.host()? {
        url::Host::Ipv4(v4) => is_local(IpAddr::V4(v4), link_local).then_some(ServiceUrl {
            base_url,
            name: None,
        }),
        url::Host::Ipv6(v6) => is_local(IpAddr::V6(v6), link_local).then_some(ServiceUrl {
            base_url,
            name: None,
        }),
        url::Host::Domain(d) => {
            if d.is_empty()
                || d.strip_suffix('.')
                    .unwrap_or(d)
                    .split('.')
                    .any(str::is_empty)
            {
                return None;
            }
            Some(ServiceUrl {
                base_url,
                name: Some(d.to_string()),
            })
        }
    }
}

/// Whether every answer for `name` is on this network, resolved now. `Err(true)`: the name did
/// not resolve (transient); `Err(false)`: an answer is not on this network.
async fn check_name(name: &str, link_local: bool) -> Result<(), bool> {
    let lookup = tokio::net::lookup_host((name.trim_end_matches('.'), 0));
    let answers: Vec<SocketAddr> = match tokio::time::timeout(LOOKUP_DEADLINE, lookup).await {
        Ok(Ok(a)) => a.collect(),
        _ => return Err(true),
    };
    if answers.is_empty() {
        return Err(true);
    }
    if answers.iter().all(|a| is_local(a.ip(), link_local)) {
        Ok(())
    } else {
        Err(false)
    }
}

/// The connect-time half (core `createServiceLookup`): resolves as usual and refuses the
/// connection unless every answer is on this network.
struct LocalOnly {
    link_local: bool,
}

impl reqwest::dns::Resolve for LocalOnly {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let link_local = self.link_local;
        let host = name.as_str().to_string();
        Box::pin(async move {
            let answers: Vec<SocketAddr> =
                tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
            if answers.is_empty() || !answers.iter().all(|a| is_local(a.ip(), link_local)) {
                return Err(Box::new(std::io::Error::other(
                    "refused: the name does not resolve only to addresses on this network",
                ))
                    as Box<dyn std::error::Error + Send + Sync>);
            }
            let out: reqwest::dns::Addrs = Box::new(answers.into_iter());
            Ok(out)
        })
    }
}

// ---------------------------------------------------------------------------------------------
// The stored rows.

/// A settings row as Node's `JSON.parse(observed.value||'null')` sees it: `Err(())` when the row
/// cannot be read (over the 1 MiB fact limit) or does not parse, Node's fallback in both cases.
fn read_json_row(reader: &Reader<'_>, key: &str) -> Result<Result<JValue, ()>, StoreError> {
    let row = reader.row(
        "SELECT typeof(value), length(CAST(value AS BLOB)), CASE WHEN value IS NULL OR length(CAST(value AS BLOB))<=1048576 THEN CAST(value AS TEXT) ELSE NULL END, CASE WHEN typeof(value) IN ('integer','real') THEN value=0 ELSE 0 END FROM settings WHERE key=?1",
        [key],
        |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<i64>>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, bool>(3)?,
            ))
        },
    )?;
    let Some((kind, size, text, zero)) = row else {
        return Ok(Ok(JValue::Null));
    };
    if size.is_some_and(|n| n > SOURCE_LIMIT) {
        return Ok(Err(()));
    }
    // `value||'null'`: NULL, '' and a numeric 0 read as null; an (empty) blob is a Buffer, truthy.
    let text = match (kind.as_str(), text) {
        ("null", _) => return Ok(Ok(JValue::Null)),
        ("integer" | "real", _) if zero => return Ok(Ok(JValue::Null)),
        ("text", Some(t)) if t.is_empty() => return Ok(Ok(JValue::Null)),
        (_, Some(t)) => t,
        (_, None) => return Ok(Err(())),
    };
    Ok(js_json::parse(&text).map_err(|_| ()))
}

/// core decision-settings.cjs `saved`: the parsed row; a row that cannot be read or parsed is
/// Node's `{url:'',timeoutMs:1500}`.
fn saved(reader: &Reader<'_>) -> Result<JValue, StoreError> {
    Ok(read_json_row(reader, KEY)?.unwrap_or_else(|()| {
        JValue::obj([
            ("url", JValue::Str(String::new())),
            ("timeoutMs", JValue::Num(1500.0)),
        ])
    }))
}

fn is_integer(v: &JValue) -> Option<f64> {
    v.as_num().filter(|n| n.is_finite() && n.fract() == 0.0)
}

/// core inference-machines.cjs `readState(...)` then `urlFor('decision')`: the decision service's
/// machine address and port, or `None` (this machine).
fn machine_url(reader: &Reader<'_>) -> Result<Option<String>, StoreError> {
    let Ok(state) = read_json_row(reader, MACHINES_KEY)? else {
        return Ok(None);
    };
    if !state.truthy() {
        return Ok(None);
    }
    let assignments = state.get("assignments");
    let shape_ok = state.get("version").as_num() == Some(1.0)
        && matches!(state.get("machines"), JValue::Arr(_))
        && assignments.type_of() == "object"
        && assignments.truthy();
    if !shape_ok {
        return Ok(None);
    }
    let JValue::Arr(machines) = state.get("machines") else {
        return Ok(None);
    };
    let machines: Vec<(&str, &str)> = machines
        .iter()
        .filter(|m| m.truthy() && m.get("name").as_str().is_some())
        .filter_map(|m| {
            let id = m.get("id").as_str()?;
            let address = m.get("address").as_str()?;
            (id != "local").then_some((id, address))
        })
        .collect();
    let a = assignments.get("decision");
    if !a.truthy() {
        return Ok(None);
    }
    let Some(machine) = a.get("machine").as_str() else {
        return Ok(None);
    };
    let Some(port) = is_integer(a.get("port")) else {
        return Ok(None);
    };
    let Some((_, address)) = machines.iter().find(|(id, _)| *id == machine) else {
        return Ok(None);
    };
    let url = format!("{address}:{}", js_json::number_to_string(port));
    Ok((!url.is_empty()).then_some(url))
}

/// core decision-settings.cjs `get()`.
fn view(saved: &JValue, machine: Option<&str>, deployment: &Deployment) -> JValue {
    let base = if saved.truthy() {
        saved.clone()
    } else {
        JValue::obj([
            ("url", JValue::Str(deployment.url.clone())),
            ("timeoutMs", JValue::Num(1500.0)),
        ])
    };
    let mut out = vec![
        (
            "url".to_string(),
            machine.map_or_else(|| base.get("url").clone(), |m| JValue::Str(m.into())),
        ),
        ("timeoutMs".to_string(), base.get("timeoutMs").clone()),
    ];
    if let Some(model) = base.get("model").as_str().filter(|m| !m.is_empty()) {
        out.push(("model".into(), JValue::Str(model.into())));
    }
    let source = if machine.is_some() {
        "machine"
    } else if saved.truthy() {
        "admin"
    } else {
        "deployment"
    };
    out.push(("source".into(), JValue::Str(source.into())));
    JValue::Obj(out)
}

// ---------------------------------------------------------------------------------------------
// Validation (core decision-settings.cjs `validate`).

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Refusal {
    Input,
    Url,
    Model,
}

impl Refusal {
    fn answer(self) -> Response<Body> {
        let (message, id) = match self {
            Refusal::Input => (INVALID_INPUT, "invalidInput"),
            Refusal::Url => (INVALID_URL, "invalidUrl"),
            Refusal::Model => (INVALID_MODEL, "invalidModel"),
        };
        answer(
            StatusCode::BAD_REQUEST,
            &JValue::obj([
                ("error", JValue::Str(message.into())),
                ("errorId", JValue::Str(id.into())),
            ]),
        )
    }
}

/// The configuration to save: `{url, timeoutMs, model?}` in Node's key order.
#[derive(Debug, Clone, PartialEq)]
struct Next {
    url: String,
    timeout_ms: f64,
    model: String,
}

impl Next {
    fn stored(&self) -> JValue {
        let mut items = vec![
            ("url".to_string(), JValue::Str(self.url.clone())),
            ("timeoutMs".to_string(), JValue::Num(self.timeout_ms)),
        ];
        if !self.model.is_empty() {
            items.push(("model".into(), JValue::Str(self.model.clone())));
        }
        JValue::Obj(items)
    }
}

fn known(model: &str) -> bool {
    PROFILES.contains(&model)
}

fn validate(value: &JValue, saved: &JValue) -> Result<(Next, Option<String>), Refusal> {
    let url = value.get("url").as_str();
    let timeout = is_integer(value.get("timeoutMs"));
    let (Some(url), Some(timeout)) = (url, timeout) else {
        return Err(Refusal::Input);
    };
    if !value.truthy() || !(100.0..=2000.0).contains(&timeout) {
        return Err(Refusal::Input);
    }
    let parsed = parse_service_url(js_trim(url), true).ok_or(Refusal::Url)?;
    let kept_raw = saved
        .get("model")
        .as_str()
        .map(|m| js_trim(m).to_lowercase())
        .unwrap_or_default();
    let kept = if known(&kept_raw) {
        kept_raw
    } else {
        String::new()
    };
    let model = match value.get("model") {
        JValue::Undefined => kept,
        JValue::Null => String::new(),
        JValue::Str(s) => js_trim(s).to_lowercase(),
        _ => return Err(Refusal::Model),
    };
    if !model.is_empty() && !known(&model) {
        return Err(Refusal::Model);
    }
    Ok((
        Next {
            url: parsed.base_url,
            timeout_ms: timeout,
            model,
        },
        parsed.name,
    ))
}

// ---------------------------------------------------------------------------------------------
// HTTP.

fn answer(status: StatusCode, body: &JValue) -> Response<Body> {
    reply::json_text(status, js_json::stringify(body).unwrap_or_default(), true)
}

fn unavailable() -> Response<Body> {
    reply::error(
        StatusCode::SERVICE_UNAVAILABLE,
        "Decision configuration unavailable",
        true,
    )
}

enum BodyError {
    TooLarge,
    Failed,
}

async fn read_body(mut body: Body) -> Result<Vec<u8>, BodyError> {
    let read = async {
        let mut out = Vec::new();
        while let Some(frame) = body.frame().await {
            if let Ok(data) = frame.map_err(|_| BodyError::Failed)?.into_data() {
                if out.len() + data.len() > BODY_LIMIT {
                    return Err(BodyError::TooLarge);
                }
                out.extend_from_slice(&data);
            }
        }
        Ok(out)
    };
    tokio::time::timeout(BODY_DEADLINE, read)
        .await
        .map_err(|_| BodyError::Failed)?
}

/// core http.cjs `readJson`: `{}` for an empty body, else `JSON.parse` of its UTF-8 text.
async fn read_json(body: Body) -> Result<JValue, Box<Response<Body>>> {
    let bytes = match read_body(body).await {
        Ok(b) => b,
        Err(BodyError::TooLarge) => {
            return Err(Box::new(reply::error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "Request exceeds size limit",
                true,
            )))
        }
        Err(BodyError::Failed) => {
            return Err(Box::new(reply::error(
                StatusCode::BAD_REQUEST,
                "Invalid decision settings",
                true,
            )))
        }
    };
    if bytes.is_empty() {
        return Ok(JValue::Obj(Vec::new()));
    }
    js_json::parse(&String::from_utf8_lossy(&bytes))
        .map_err(|_| Box::new(reply::error(StatusCode::BAD_REQUEST, "invalid JSON", true)))
}

enum Write {
    Save(JValue),
    Reset,
}

pub async fn serve(
    app: &Arc<App>,
    conn: crate::serve::Conn,
    req: Request<Body>,
    path: String,
) -> Response<Body> {
    // Node matched the exact pathnames; the table's trailing-slash tolerance is not its route.
    if !ROUTES.contains(&path.as_str()) {
        return app.proxy.forward(conn.peer, req).await;
    }
    let (parts, body) = req.into_parts();
    let creds = identity::creds_of(&parts);
    let layer = Arc::clone(&app.identity);
    let now = identity::now_ms();
    let gate_path = path.clone();
    let id = match tokio::task::spawn_blocking(move || layer.gate_blocking(&creds, &gate_path, now))
        .await
    {
        Ok(Ok(id)) => id,
        Ok(Err(rejection)) => return rejection.into_response(),
        Err(_) => return identity::Rejection::Unavailable.into_response(),
    };
    if id.user.role != server_auth::Role::Admin {
        return reply::error(StatusCode::FORBIDDEN, "Administrator required", true);
    }
    let method = parts.method.clone();
    let test = path == TEST;
    let allowed = if test {
        method == Method::POST
    } else {
        method == Method::GET || method == Method::PUT || method == Method::DELETE
    };
    if !allowed {
        return reply::error(StatusCode::METHOD_NOT_ALLOWED, "method not allowed", true);
    }
    if test {
        let value = match read_json(body).await {
            Ok(v) => v,
            Err(r) => return *r,
        };
        return connection_test(app, value).await;
    }
    let write = if method == Method::DELETE {
        Some(Write::Reset)
    } else if method == Method::PUT {
        match read_json(body).await {
            Ok(v) if v.get("reset").as_bool() == Some(true) => Some(Write::Reset),
            Ok(v) => Some(Write::Save(v)),
            Err(r) => return *r,
        }
    } else {
        None
    };
    let app = Arc::clone(app);
    let actor = id.user.id.clone();
    let deployment = app.config.decision.clone();
    let result =
        tokio::task::spawn_blocking(move || -> Result<Result<JValue, Refusal>, StoreError> {
            match write {
                None => app.identity.store_blocking()?.read(|r| {
                    let machine = machine_url(r)?;
                    Ok(Ok(view(&saved(r)?, machine.as_deref(), &deployment)))
                }),
                Some(Write::Reset) => app.writes.writer()?.write(|tx| {
                    let r = tx.reader();
                    let had = saved(&r)?.truthy();
                    tx.delete_decision_configuration(RustDecisionSettings::owned())?;
                    audit(
                        tx,
                        &actor,
                        "decision.reset",
                        &JValue::obj([("hadSaved", JValue::Bool(had))]),
                        now,
                    )?;
                    let machine = machine_url(&r)?;
                    Ok(Ok(view(&JValue::Null, machine.as_deref(), &deployment)))
                }),
                Some(Write::Save(value)) => app.writes.writer()?.write(|tx| {
                    let r = tx.reader();
                    let (next, _) = match validate(&value, &saved(&r)?) {
                        Ok(n) => n,
                        Err(refusal) => return Ok(Err(refusal)),
                    };
                    let stored = next.stored();
                    tx.set_decision_configuration(
                        RustDecisionSettings::owned(),
                        &js_json::stringify(&stored).unwrap_or_default(),
                    )?;
                    let model = if next.model.is_empty() {
                        "auto".to_string()
                    } else {
                        next.model.clone()
                    };
                    let detail = JValue::obj([
                        ("url", JValue::Str(next.url.clone())),
                        ("timeoutMs", JValue::Num(next.timeout_ms)),
                        ("model", JValue::Str(model)),
                    ]);
                    audit(tx, &actor, "decision.configure", &detail, now)?;
                    let machine = machine_url(&r)?;
                    Ok(Ok(view(&stored, machine.as_deref(), &deployment)))
                }),
            }
        })
        .await;
    match result {
        Ok(Ok(Ok(v))) => answer(StatusCode::OK, &v),
        Ok(Ok(Err(refusal))) => refusal.answer(),
        _ => unavailable(),
    }
}

/// core auth.cjs `audit(action, actor, actor, detail)`.
fn audit(
    tx: &server_store::WriteTx<'_>,
    actor: &str,
    action: &str,
    detail: &JValue,
    now: i64,
) -> Result<(), StoreError> {
    tx.execute(
        "INSERT INTO audit_events(actor_user_id,target_user_id,action,detail,created_at) VALUES(?1,?1,?2,?3,?4)",
        server_store::rusqlite::params![actor, action, js_json::stringify(detail).unwrap_or_default(), now],
    )?;
    Ok(())
}

async fn connection_test(app: &Arc<App>, value: JValue) -> Response<Body> {
    let layer = Arc::clone(&app.identity);
    let saved = match tokio::task::spawn_blocking(move || layer.store_blocking()?.read(saved)).await
    {
        Ok(Ok(s)) => s,
        _ => return unavailable(),
    };
    let (next, name) = match validate(&value, &saved) {
        Ok(n) => n,
        Err(refusal) => return refusal.answer(),
    };
    if let Some(name) = name {
        if check_name(&name, true).await == Err(false) {
            return Refusal::Url.answer();
        }
    }
    if health(&next.url).await {
        answer(
            StatusCode::OK,
            &JValue::obj([
                ("ok", JValue::Bool(true)),
                ("message", JValue::Str(READY.into())),
                ("messageId", JValue::Str("ready".into())),
            ]),
        )
    } else {
        answer(
            StatusCode::BAD_GATEWAY,
            &JValue::obj([
                ("error", JValue::Str(NOT_READY.into())),
                ("errorId", JValue::Str("notReady".into())),
            ]),
        )
    }
}

/// `GET <base>/health` answered `{"ready":true}` within the deadline and the cap.
async fn health(base: &str) -> bool {
    let Ok(client) = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .dns_resolver(Arc::new(LocalOnly { link_local: true }))
        .build()
    else {
        return false;
    };
    let probe = async {
        let mut response = client.get(format!("{base}/health")).send().await.ok()?;
        if !response.status().is_success() {
            return None;
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.ok()? {
            if body.len() + chunk.len() > HEALTH_CAP {
                return None;
            }
            body.extend_from_slice(&chunk);
        }
        let parsed = js_json::parse(&String::from_utf8_lossy(&body)).ok()?;
        Some(parsed.get("ready").as_bool() == Some(true))
    };
    matches!(
        tokio::time::timeout(HEALTH_DEADLINE, probe).await,
        Ok(Some(true))
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn p(s: &str) -> JValue {
        js_json::parse(s).unwrap()
    }

    #[test]
    fn service_urls_follow_core_rule() {
        for ok in [
            "http://192.168.1.20:8040",
            "http://192.168.1.20:8040/",
            " http://laya:8040 ",
            "https://box.tailnet.ts.net",
            "http://10.0.0.1",
            "http://100.64.0.1:1",
            "http://[::1]:8040",
            "http://[fd00::1]",
            "http://169.254.1.2",
            "http://[fe80::1]",
            "http://[::ffff:192.168.1.1]",
            "http://pi5.lan.",
        ] {
            assert!(parse_service_url(ok, true).is_some(), "{ok}");
        }
        for bad in [
            "",
            "ftp://192.168.1.2",
            "http://u:p@192.168.1.2",
            "http://192.168.1.2/x",
            "http://192.168.1.2/?",
            "http://192.168.1.2#",
            "http://8.8.8.8",
            "http://0.0.0.0",
            "http://169.254.169.254",
            "http://100.100.100.200",
            "http://224.0.0.1",
            "http://198.51.100.1",
            "http://[::]",
            "http://[64:ff9b::1]",
            "http://[2001:db8::1]",
            "http://[ff02::1]",
            "http://[::192.168.1.1]",
            "http://[fd00:ec2::254]",
            "http://[2606:4700::1]",
            "http://a..b",
        ] {
            assert!(parse_service_url(bad, true).is_none(), "{bad}");
        }
        assert!(parse_service_url("http://169.254.1.2", false).is_none());
        assert!(parse_service_url("http://[fe80::1]", false).is_none());
        assert_eq!(
            parse_service_url("HTTP://Laya:8040/", true).unwrap(),
            ServiceUrl {
                base_url: "http://laya:8040".into(),
                name: Some("laya".into())
            }
        );
        assert_eq!(
            parse_service_url("http://192.168.1.20:80", true)
                .unwrap()
                .base_url,
            "http://192.168.1.20"
        );
    }

    #[test]
    fn validate_keeps_or_clears_the_model_as_core() {
        let none = JValue::Null;
        let (n, _) = validate(
            &p(r#"{"url":" http://laya:8040/ ","timeoutMs":1500}"#),
            &none,
        )
        .unwrap();
        assert_eq!(
            js_json::stringify(&n.stored()).unwrap(),
            r#"{"url":"http://laya:8040","timeoutMs":1500}"#
        );
        let kev = p(r#"{"url":"http://laya:8040","timeoutMs":1500,"model":"kev-0.8b"}"#);
        let (n, _) = validate(&p(r#"{"url":"http://kev:1","timeoutMs":100.0}"#), &kev).unwrap();
        assert_eq!(
            js_json::stringify(&n.stored()).unwrap(),
            r#"{"url":"http://kev:1","timeoutMs":100,"model":"kev-0.8b"}"#
        );
        let (n, _) = validate(
            &p(r#"{"url":"http://kev:1","timeoutMs":2000,"model":null}"#),
            &kev,
        )
        .unwrap();
        assert_eq!(n.model, "");
        let (n, _) = validate(
            &p(r#"{"url":"http://kev:1","timeoutMs":2000,"model":" LAYA "}"#),
            &none,
        )
        .unwrap();
        assert_eq!(n.model, "laya");
        let stale = p(r#"{"model":"retired"}"#);
        let (n, _) = validate(&p(r#"{"url":"http://kev:1","timeoutMs":2000}"#), &stale).unwrap();
        assert_eq!(n.model, "");
        for (raw, refusal) in [
            ("null", Refusal::Input),
            ("{}", Refusal::Input),
            (r#"{"url":5,"timeoutMs":1500}"#, Refusal::Input),
            (r#"{"url":"http://laya","timeoutMs":99}"#, Refusal::Input),
            (r#"{"url":"http://laya","timeoutMs":2001}"#, Refusal::Input),
            (r#"{"url":"http://laya","timeoutMs":150.5}"#, Refusal::Input),
            (
                r#"{"url":"http://laya","timeoutMs":"1500"}"#,
                Refusal::Input,
            ),
            (r#"{"url":"http://8.8.8.8","timeoutMs":1500}"#, Refusal::Url),
            (
                r#"{"url":"http://8.8.8.8","timeoutMs":1500,"model":5}"#,
                Refusal::Url,
            ),
            (
                r#"{"url":"http://laya","timeoutMs":1500,"model":5}"#,
                Refusal::Model,
            ),
            (
                r#"{"url":"http://laya","timeoutMs":1500,"model":"gpt"}"#,
                Refusal::Model,
            ),
        ] {
            assert_eq!(validate(&p(raw), &none).unwrap_err(), refusal, "{raw}");
        }
    }

    #[test]
    fn view_is_core_get() {
        let dep = Deployment {
            url: "http://laya:8040".into(),
        };
        let v = |saved: &str, machine: Option<&str>| {
            js_json::stringify(&view(&p(saved), machine, &dep)).unwrap()
        };
        assert_eq!(
            v("null", None),
            r#"{"url":"http://laya:8040","timeoutMs":1500,"source":"deployment"}"#
        );
        assert_eq!(
            v("0", None),
            r#"{"url":"http://laya:8040","timeoutMs":1500,"source":"deployment"}"#
        );
        assert_eq!(
            v(
                r#"{"url":"http://kev:1","timeoutMs":900,"model":"kev-0.8b"}"#,
                None
            ),
            r#"{"url":"http://kev:1","timeoutMs":900,"model":"kev-0.8b","source":"admin"}"#
        );
        assert_eq!(
            v(
                r#"{"url":"http://kev:1","timeoutMs":900}"#,
                Some("http://10.0.0.2:8040")
            ),
            r#"{"url":"http://10.0.0.2:8040","timeoutMs":900,"source":"machine"}"#
        );
        assert_eq!(v("{}", None), r#"{"source":"admin"}"#);
        assert_eq!(v(r#""x""#, None), r#"{"source":"admin"}"#);
    }
}
