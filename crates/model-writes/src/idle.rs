//! `PUT /api/models/idle-unload` (`routes/models.cjs`, `idle-unload-setting.cjs` `save`): how long
//! an idle chat model stays loaded. The model manager stores the value and rewrites the chat
//! preset's `[*] sleep-idle-seconds`; the write and the router's reload run under one maintenance
//! hold, and the answer says whether the router is known to run on the saved value.
//!
//! The flow is [`save`] over a [`Host`] the caller implements (the model manager's HTTP answers,
//! the hold, the reload, the router's presets and the audit record), in core's order.

use crate::body;
use js_json::JValue;

pub const MIN_SECONDS: f64 = 300.0;
pub const MAX_SECONDS: f64 = 86_400.0;
pub const DEFAULT_SECONDS: f64 = 1800.0;
/// The audit action.
pub const AUDIT: &str = "models.idle_unload";
/// The manager route, after `MODEL_LOADER_URL` without its trailing slashes.
pub const MANAGER_PATH: &str = "/api/v1/idle-unload";
/// `fetchJson`'s deadline for the manager's two calls.
pub const MANAGER_TIMEOUT_MS: u64 = 30_000;

const INVALID: &str = "Choose Never or a time from 5 minutes to 24 hours.";

/// A refusal: core's `fail(status, error, errorId)` and the route's answer of it.
#[derive(Debug, Clone, PartialEq)]
pub struct Fail {
    pub status: u16,
    pub message: String,
    pub error_id: Option<&'static str>,
    /// The 400 for an invalid request names the allowed range.
    pub range: bool,
}

impl Fail {
    pub fn new(status: u16, message: &str, error_id: Option<&'static str>) -> Fail {
        Fail {
            status,
            message: message.to_string(),
            error_id,
            range: false,
        }
    }

    /// The route's `fail(e)`: `{ error, errorId?, minSeconds?, maxSeconds? }`.
    pub fn body(&self) -> JValue {
        let mut items = vec![("error".to_string(), JValue::Str(self.message.clone()))];
        if let Some(id) = self.error_id {
            items.push(("errorId".to_string(), JValue::from(id)));
        }
        if self.range {
            items.push(("minSeconds".to_string(), JValue::Num(MIN_SECONDS)));
            items.push(("maxSeconds".to_string(), JValue::Num(MAX_SECONDS)));
        }
        JValue::Obj(items)
    }
}

/// The route's answer when the deployment has no model manager (`MODEL_LOADER_URL` unset).
pub fn unavailable() -> JValue {
    JValue::obj([
        (
            "error",
            JValue::from("The idle unload setting is unavailable"),
        ),
        ("errorId", JValue::from("idleUnloadUnavailable")),
    ])
}

fn is_integer(n: f64) -> bool {
    n.is_finite() && n.trunc() == n
}

/// `validSeconds(seconds)` of `body.seconds` (own property only): `null` clears the choice, 0 is
/// Never.
pub fn requested(body: &JValue) -> Result<JValue, Fail> {
    let seconds = match body {
        JValue::Obj(items) => items
            .iter()
            .find(|(k, _)| k == "seconds")
            .map_or(JValue::Undefined, |(_, v)| v.clone()),
        _ => JValue::Undefined,
    };
    let ok = match &seconds {
        JValue::Null => true,
        JValue::Num(n) => is_integer(*n) && (*n == 0.0 || (MIN_SECONDS..=MAX_SECONDS).contains(n)),
        _ => false,
    };
    if ok {
        Ok(seconds)
    } else {
        Err(Fail {
            range: true,
            ..Fail::new(400, INVALID, Some("idleUnloadInvalid"))
        })
    }
}

/// `view(body)`: the manager's answer as the client reads it, or `None` when it is not one.
pub fn view(body: &JValue) -> Option<JValue> {
    let n = |v: &JValue| match v {
        JValue::Num(x) if is_integer(*x) && *x >= 0.0 => Some(*x),
        _ => None,
    };
    let num = |v: Option<f64>| v.map_or(JValue::Null, JValue::Num);
    let one_of = |v: &JValue, allowed: &[&str]| match v.as_str() {
        Some(s) if allowed.contains(&s) => Some(v.clone()),
        _ => None,
    };
    let source = one_of(body.get("source"), &["setting", "env", "default"])?;
    let seconds = n(body.get("seconds"))?;
    Some(JValue::obj([
        ("seconds", JValue::Num(seconds)),
        ("source", source),
        ("fallbackSeconds", num(n(body.get("fallbackSeconds")))),
        (
            "fallbackSource",
            one_of(body.get("fallbackSource"), &["env", "default"]).unwrap_or(JValue::Null),
        ),
        (
            "defaultSeconds",
            JValue::Num(n(body.get("defaultSeconds")).unwrap_or(DEFAULT_SECONDS)),
        ),
        (
            "minSeconds",
            JValue::Num(n(body.get("minSeconds")).unwrap_or(MIN_SECONDS)),
        ),
        (
            "maxSeconds",
            JValue::Num(n(body.get("maxSeconds")).unwrap_or(MAX_SECONDS)),
        ),
    ]))
}

/// An HTTP answer of the model manager (`fetchJson`'s `{ ok, status, body }`).
#[derive(Debug, Clone, PartialEq)]
pub struct Reply {
    pub ok: bool,
    pub status: u16,
    pub body: JValue,
}

/// What a PUT wrote: the manager's view and its `preset` (`written`, `unchanged`, `failed`, or
/// `null`).
#[derive(Debug, Clone, PartialEq)]
pub struct Written {
    pub view: JValue,
    pub preset: JValue,
}

/// `call(method)`'s reading of the manager's answer (`None`: it did not answer).
pub fn manager_answer(reply: Option<&Reply>) -> Result<Written, Fail> {
    let engine = |m: &str| Fail::new(502, m, Some("idleUnloadEngine"));
    let r = match reply {
        Some(r) => r,
        None => {
            return Err(engine(
                "The model manager did not answer. Try again in a moment.",
            ))
        }
    };
    match r.status {
        404 | 405 => {
            return Err(Fail::new(
                404,
                "This model manager cannot change the idle unload time yet.",
                Some("idleUnloadUnavailable"),
            ))
        }
        400 => return Err(Fail::new(400, INVALID, Some("idleUnloadInvalid"))),
        503 => {
            return Err(Fail::new(
                503,
                "The model manager has no place to keep this setting.",
                Some("idleUnloadUnavailable"),
            ))
        }
        _ => {}
    }
    if !r.ok {
        return Err(engine(
            "The model manager did not answer. Try again in a moment.",
        ));
    }
    let view =
        view(&r.body).ok_or_else(|| engine("The model manager gave an unexpected answer."))?;
    let preset = match r.body.get("preset").as_str() {
        Some(p @ ("written" | "unchanged" | "failed")) => JValue::from(p),
        _ => JValue::Null,
    };
    Ok(Written { view, preset })
}

/// `globalIdleSeconds(text)`: the `[*]` section's `sleep-idle-seconds` as the model manager
/// writes it; 0 (Never) without one; `None` for a value that is not a whole number.
pub fn global_idle_seconds(text: &str) -> Option<f64> {
    let mut in_global = false;
    for raw in text.split('\n') {
        let raw = raw.strip_suffix('\r').unwrap_or(raw);
        let line = chat_turn::js::trim(raw);
        if line.starts_with('[') && line.ends_with(']') {
            in_global = line == "[*]";
            continue;
        }
        if !in_global {
            continue;
        }
        let Some(at) = raw.find('=') else { continue };
        let key = chat_turn::js::trim(raw.get(..at).unwrap_or(""));
        let key = key
            .strip_prefix("--")
            .or_else(|| key.strip_prefix('-'))
            .unwrap_or(key);
        if key != "sleep-idle-seconds" {
            continue;
        }
        let value = chat_turn::js::trim(raw.get(at + 1..).unwrap_or(""));
        return (!value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()))
            .then(|| value.parse().unwrap_or(f64::NAN));
    }
    Some(0.0)
}

/// Why a maintenance hold was not taken (nothing is held then).
#[derive(Debug, Clone, PartialEq)]
pub enum HoldRefusal {
    /// Another hold is in place (core's `holdRefused`, a 409).
    Busy,
    /// The hold could not be taken or covered: core's error, answered as it is.
    Failed { status: u16, message: String },
}

/// The audit record could not be written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuditFailed;

/// The hold's lease ended under the reload (core's `coverageLost`, which `writePresets` throws
/// on; every other reload failure is an answer, `{ ok: false }` at worst).
#[derive(Debug, Clone, PartialEq)]
pub struct CoverageLost {
    pub status: u16,
    pub message: String,
}

/// What [`save`] needs of the deployment.
pub trait Host {
    /// `MODEL_LOADER_URL` without its trailing slashes is empty.
    fn manager_missing(&self) -> bool;
    /// GET or PUT on [`MANAGER_PATH`] with `body` (JSON text), the token header when one is set;
    /// `None` when it does not answer.
    fn manager(&mut self, method: &str, body: Option<&str>) -> Option<Reply>;
    /// Whether the held writer exists (core's `writePresets`, the llama.cpp manager's).
    fn has_writer(&self) -> bool;
    /// Takes the maintenance hold and checks it covers a preset write; on `Err` nothing is held
    /// (a hold taken and found uncovered is released by the host, as `writePresets`'s `finally`).
    fn hold(&mut self) -> Result<(), HoldRefusal>;
    /// The router's reload under the hold (`reloadHeld({ unload: false })`): its answer, `{ ok:
    /// false }` for any failure but the lease ending, which is `Err`.
    fn reload_held(&mut self) -> Result<Reply, CoverageLost>;
    /// Releases the hold.
    fn release(&mut self);
    /// `reloadPresets({ unload: false })` without a hold, where there is no held writer: `None`
    /// when the deployment has no reload or it threw.
    fn reload(&mut self) -> Option<Option<Reply>>;
    /// `presetsInRouter()`: the presets the router runs on, `None` when unknown.
    fn router_text(&mut self) -> Option<String>;
    /// The audit record; `Err` when it could not be written (core's audit throws: the route's 500).
    fn audit(&mut self, action: &str, detail: JValue) -> Result<(), AuditFailed>;
}

fn call(host: &mut impl Host, method: &str, body: Option<&str>) -> Result<Written, Fail> {
    if host.manager_missing() {
        return Err(Fail::new(
            404,
            "The idle unload setting needs the model manager.",
            Some("idleUnloadUnavailable"),
        ));
    }
    manager_answer(host.manager(method, body).as_ref())
}

fn hold_failed(refusal: HoldRefusal) -> Fail {
    match refusal {
        HoldRefusal::Busy => Fail::new(
            409,
            "Models are busy, so the idle unload time was not changed. Try again shortly.",
            Some("idleUnloadBusy"),
        ),
        HoldRefusal::Failed { status, message } => Fail::new(status, &message, None),
    }
}

/// `save(body, actor)` (the actor is the host's to record).
pub fn save(body: &JValue, host: &mut impl Host) -> Result<JValue, Fail> {
    let seconds = requested(body)?;
    // For the audit only; any failure reads as no previous value.
    let previous = call(host, "GET", None)
        .map(|w| w.view.get("seconds").clone())
        .unwrap_or(JValue::Null);
    let put = js_json::wtf16::stringify(&JValue::obj([("seconds", seconds)])).unwrap_or_default();
    let audit =
        |host: &mut dyn FnMut(JValue) -> Result<(), AuditFailed>, w: &Written, previous: JValue| {
            host(JValue::obj([
                ("seconds", w.view.get("seconds").clone()),
                ("source", w.view.get("source").clone()),
                ("previous", previous),
            ]))
            .map_err(|AuditFailed| Fail::new(500, "Could not change the idle unload time", None))
        };
    let (written, reload) = if host.has_writer() {
        host.hold().map_err(hold_failed)?;
        let written = match call(host, "PUT", Some(&put)) {
            Ok(w) => w,
            Err(e) => {
                host.release();
                return Err(e);
            }
        };
        let reload = if written.preset.as_str() == Some("failed") {
            None
        } else {
            match host.reload_held() {
                Ok(r) => Some(r),
                Err(lost) => {
                    host.release();
                    return Err(Fail::new(lost.status, &lost.message, None));
                }
            }
        };
        host.release();
        audit(&mut |d| host.audit(AUDIT, d), &written, previous)?;
        (written, reload)
    } else {
        // Without the held writer the audit comes first, then the reload.
        let written = call(host, "PUT", Some(&put))?;
        audit(&mut |d| host.audit(AUDIT, d), &written, previous)?;
        let reload = if written.preset.as_str() == Some("failed") {
            None
        } else {
            host.reload().flatten()
        };
        (written, reload)
    };
    let apply = if written.preset.as_str() == Some("failed") {
        JValue::obj([("state", JValue::from("failed"))])
    } else if reload.as_ref().is_some_and(|r| r.ok) {
        JValue::obj([("state", JValue::from("applied"))])
    } else {
        let mut items = vec![("state".to_string(), JValue::from("pending"))];
        if let Some(JValue::Arr(loaded)) = reload.as_ref().map(|r| r.body.get("loaded")) {
            items.push((
                "loaded".to_string(),
                JValue::Arr(
                    loaded
                        .iter()
                        .filter(|x| x.as_str().is_some())
                        .cloned()
                        .collect(),
                ),
            ));
        }
        JValue::Obj(items)
    };
    let applied = apply.get("state").as_str() == Some("applied")
        && host
            .router_text()
            .and_then(|t| global_idle_seconds(&t))
            .is_some_and(|s| JValue::Num(s) == *written.view.get("seconds"));
    let JValue::Obj(mut items) = written.view else {
        return Err(Fail::new(500, "", None));
    };
    items.push(("apply".to_string(), apply));
    items.push(("applied".to_string(), JValue::Bool(applied)));
    Ok(JValue::Obj(items))
}

/// What one PUT answers.
#[derive(Debug, Clone, PartialEq)]
pub struct Put {
    pub status: u16,
    pub body: JValue,
    /// `Cache-Control: no-store` (every answer past the administrator check and the dependency).
    pub no_store: bool,
}

/// One `PUT /api/models/idle-unload` for a deployment whose model manager is configured. `None`:
/// a body the port cannot hold ([`body::Read::Unsupported`]), which goes to core unchanged.
pub fn put(admin: bool, body: Option<&[u8]>, host: &mut impl Host) -> Option<Put> {
    if !admin {
        return Some(Put {
            status: 403,
            body: body::administrator_required(),
            no_store: false,
        });
    }
    let value = match body::read_json(body) {
        body::Read::Value(v) => v,
        body::Read::Invalid => {
            return Some(Put {
                status: 400,
                body: body::invalid_json(),
                no_store: true,
            })
        }
        body::Read::Unsupported => return None,
    };
    Some(match save(&value, host) {
        Ok(answer) => Put {
            status: 200,
            body: answer,
            no_store: true,
        },
        Err(fail) => Put {
            status: fail.status,
            body: fail.body(),
            no_store: true,
        },
    })
}
