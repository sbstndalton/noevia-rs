//! noevia-core's `/api/model-manager/*` proxy (`routes/models.cjs`, the model-manager block with
//! the write prelude and the wrapper around `handle`), as decisions over a [`Host`] the server
//! implements: the HTTP calls, the shared folder-scan cache, the inference gate's holds, the
//! roles file, the embedding and reranker protection and the role/state clean-up stay with the
//! server; this crate decides, in core's order, what to ask and what to answer.
//!
//! [`handle`] runs one request. The server, before calling it:
//! - reads the body only for a method that carries one ([`carries_body`]), at most [`BODY_LIMIT`]
//!   bytes (`None` when more arrived);
//! - passes `path` and `search` as WHATWG `URL` gives `url.pathname` and `url.search`;
//! - keeps answering exact `GET /api/model-manager/models` itself (core refuses it before this
//!   block, #2304).
//!
//! The answer is [`Outcome::Answer`] (status, JSON body to write with `wtf16::stringify`, and
//! whether `Cache-Control: no-store` was set) or [`Outcome::Pass`] for a path outside the block.
//! ([`Outcome::Core`] is no longer produced: a body the port cannot hold is refused, below.)
//!
//! An administrator's write ([`protected_action`]) runs, in core, inside a reranker-settings
//! snapshot (`rerank-settings.cjs withSettings`: `run(fn)` first reads the settings from the
//! native owner and runs the handler on that snapshot; it is not a lock). The crate asks for that
//! snapshot first ([`Host::reranker_settings`]); when it cannot be read, core's 503 `Internal
//! error` is the answer before anything else (no reads fence, no scan, no forward). The host
//! answers the sidecar and reranker questions of that request from the same snapshot.
//!
//! Where core is unsafe, the port departs from it and fails closed; each is a named difference,
//! recorded (Node's own answer is in the fixture beside the port's) and replayed:
//! - **Control characters**: a decoded path segment holding a character below U+0020 or U+007F is
//!   400 `Invalid path`. Core passes `models/delete%0A` on unguarded (no route matches it) while
//!   the model manager strips the final newline and acts on it as the delete; the same holds for the
//!   sweep, the benchmark start, a section rename or autoconfig (the client's `budget_gib` kept) and
//!   the preset writes (no hold).
//! - **The judged payload is the forwarded one**: on `benchmark/start`, `benchmark/sweep`,
//!   `models/delete` and a section PUT, a non-empty body that does not parse to a JSON object (core
//!   reads it as `{}` and forwards the raw bytes, which the model manager may read differently:
//!   `NaN`, a dictionary of `models`), a body the port cannot hold, or an `aliases`/`models` that is
//!   present but not an array is 400; otherwise the body forwarded is the parsed value written
//!   again (`wtf16::stringify`; an empty body is `{}`), so the model manager sees exactly what was
//!   judged. (This also answers core's `null`-body TypeError with that 400.)
//! - **Methods**: a method Node's HTTP parser does not accept ([`NODE_METHODS`]; hyper accepts any
//!   token, `delete` included) is 400 `Invalid method`, before anything.
//! - **A section named `rename`**: `PUT sections/rename` is a section write (core's
//!   `endsWith('/rename')` skipped its path checks and `presetRefusal`); a rename is decided by
//!   the segment count.
//!
//! Named, not changed: a non-empty scan cache that lacks a key lets that key through unchecked
//! (core scans fresh only when the cache is empty); the sweep guard's start-up `adopt()` is not
//! part of this contract: the front relays `POST benchmark/sweep` to core, which keeps its guard, but
//! only after the same [`strict_payload`] check (with `aliases`) and with the parsed body written
//! again, so core judges and the model manager receives the checked value.
//!
//! Core's behaviour is kept where it is wrong but harmless; each is in the recording:
//! - `PATH_OPTION_ALIASES` is a plain object, so a section PUT whose `values` has a non-string
//!   under an inherited name (`constructor`, `toString`, `__proto__`, …) is refused as
//!   `Not saved: function Object() { [native code] } must be a text path` (or `[object Object]`);
//! - a scanned entry whose `sections` is a string matches a section name by substring in the
//!   section guards, and makes a folder-scan delete of it a 500; a `null` scanned entry makes both a
//!   500;
//! - a benchmark alias that is falsy or not a string (`0`, `null`, `[]`) is named in the
//!   "needs chat models" refusal as `String()` gives it (`null` as nothing).

pub mod evidence;
pub mod js;
pub mod sections;

use js::{prop, strict_eq, string, string_or_empty, to_number, trim};
use js_json::{wtf16, JValue};

/// The methods Node's HTTP parser accepts (`http.METHODS`).
pub const NODE_METHODS: [&str; 34] = [
    "ACL",
    "BIND",
    "CHECKOUT",
    "CONNECT",
    "COPY",
    "DELETE",
    "GET",
    "HEAD",
    "LINK",
    "LOCK",
    "M-SEARCH",
    "MERGE",
    "MKACTIVITY",
    "MKCALENDAR",
    "MKCOL",
    "MOVE",
    "NOTIFY",
    "OPTIONS",
    "PATCH",
    "POST",
    "PROPFIND",
    "PROPPATCH",
    "PURGE",
    "PUT",
    "QUERY",
    "REBIND",
    "REPORT",
    "SEARCH",
    "SOURCE",
    "SUBSCRIBE",
    "TRACE",
    "UNBIND",
    "UNLINK",
    "UNLOCK",
];

/// The block's prefix.
pub const PREFIX: &str = "/api/model-manager/";
/// `readBody(req, 1024 * 1024)`.
pub const BODY_LIMIT: usize = 1024 * 1024;
/// The forwarded request's deadline (`fetchJson`'s third argument).
pub const FORWARD_TIMEOUT_MS: u64 = 10 * 60 * 1000;
/// A delete's fresh folder scan's deadline.
pub const SCAN_TIMEOUT_MS: u64 = 60 * 1000;
/// At most this many models in one sweep, and the benchmark's budget check looks at no more.
pub const MAX_ALIASES: usize = 64;

pub const SYSTEM_MODEL_DELETE_REASON: &str = "System routing model — not deleted";
pub const SIDECAR_MODEL_DELETE_REASON: &str =
    "A running sidecar (embedding or reranking) depends on this model — not deleted";
pub const PRESETS_BUSY: &str = "Models are busy, so nothing was changed. Try again shortly.";
/// benchmark-sweep-guard.cjs: the reason the sweep's maintenance hold gives.
pub const SWEEP_PAUSE_REASON: &str = "Chat is paused while noevia runs a throughput sweep. It will be available again when the sweep finishes or is cancelled.";
pub const SWEEP_BUSY_ERROR: &str = "Requests are in progress, or a calibration, auto-tune or settings change is running. Wait for it to finish, then start the sweep.";
pub const SWEEP_UNAVAILABLE_ERROR: &str = "The maintenance lease authority is not answering, so the sweep was not started. Try again shortly.";

/// An error thrown out of the route: index.cjs answers it with http.cjs `errorResponse`.
#[derive(Debug, Clone, PartialEq)]
pub struct Thrown {
    /// `err.status` (`undefined` for a TypeError of core's own lines).
    pub status: JValue,
    pub message: String,
}

impl Thrown {
    /// A TypeError (a property of `null`, a call of something that is not a function): a 500.
    pub fn type_error() -> Thrown {
        Thrown {
            status: JValue::Undefined,
            message: String::new(),
        }
    }

    pub fn new(status: u16, message: &str) -> Thrown {
        Thrown {
            status: JValue::from(u32::from(status)),
            message: message.to_string(),
        }
    }

    /// http.cjs `errorResponse(err)`: a 4xx keeps its message, anything else is a fixed sentence.
    pub fn answer(&self) -> (u16, JValue) {
        let status = to_number(&self.status).unwrap_or(f64::NAN);
        let integer = status.is_finite() && status.trunc() == status;
        let error = |s: &str| JValue::obj([("error", JValue::from(s))]);
        if integer && (400.0..500.0).contains(&status) {
            let message = if self.message.is_empty() {
                "Request failed"
            } else {
                &self.message
            };
            return (status as u16, error(message));
        }
        let code = if integer && (500.0..600.0).contains(&status) {
            status as u16
        } else {
            500
        };
        (code, error("Internal error"))
    }
}

/// The deployment: `MODEL_LOADER_URL`, `MODEL_LOADER_TOKEN` and which optional parts the model
/// manager has (each is `typeof modelManager.<name> === 'function'` in core).
#[derive(Clone, Default, PartialEq)]
pub struct Config {
    pub loader_url: Option<String>,
    pub loader_token: Option<String>,
    pub caps: Caps,
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("loader_url", &self.loader_url)
            .field("loader_token", &self.loader_token.as_ref().map(|_| "<set>"))
            .field("caps", &self.caps)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Caps {
    /// `loadRefusal(alias)`: the budget check of a benchmark or sweep model.
    pub load_refusal: bool,
    /// `presetRefusal(name, options)`: the budget check of a section write.
    pub preset_refusal: bool,
    /// `sizingBudgetGib()`: the budget autoconfig sizes against.
    pub sizing_budget: bool,
    /// `writePresets(write, reloadIf)`: a models.ini write and its reload under one hold.
    pub preset_writer: bool,
    /// `reloadPresets({ unload: false })`, used without a held writer.
    pub reload_presets: bool,
    /// `recordEvidence(model, record)`.
    pub record_evidence: bool,
    /// `unload(id)`.
    pub unload: bool,
    /// `holdMaintenance`: there is a sweep guard.
    pub sweep_guard: bool,
}

/// One request as the block sees it.
#[derive(Debug, Clone, Copy)]
pub struct Request<'a> {
    pub method: &'a str,
    /// `url.pathname`.
    pub path: &'a str,
    /// `url.search` (`""` or `?…`).
    pub search: &'a str,
    /// `authn.user.role`.
    pub role: &'a str,
    /// The body, for a method that carries one; `None` when more than [`BODY_LIMIT`] bytes came.
    pub body: Option<&'a [u8]>,
}

/// A request to the model manager (`fetchJson(url, { method, headers, body }, timeoutMs)`).
#[derive(Clone, PartialEq)]
pub struct Fetch {
    pub url: String,
    pub method: String,
    /// In order; `X-Model-Loader-Token` only with a non-empty token.
    pub headers: Vec<(String, String)>,
    /// The request body as read (UTF-8, invalid bytes as U+FFFD); `None` for GET, HEAD and DELETE.
    pub body: Option<String>,
    pub timeout_ms: u64,
}

impl std::fmt::Debug for Fetch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let headers: Vec<(&str, &str)> = self
            .headers
            .iter()
            .map(|(k, v)| {
                (
                    k.as_str(),
                    if k == "X-Model-Loader-Token" {
                        "<set>"
                    } else {
                        v.as_str()
                    },
                )
            })
            .collect();
        f.debug_struct("Fetch")
            .field("url", &self.url)
            .field("method", &self.method)
            .field("headers", &headers)
            .field("body", &self.body.as_ref().map(String::len))
            .field("timeout_ms", &self.timeout_ms)
            .finish()
    }
}

/// `fetchJson`'s answer: `{ ok, status, body }` (`body` the parsed JSON, else the text;
/// `undefined` for an answer over the size cap).
#[derive(Debug, Clone, PartialEq)]
pub struct Reply {
    pub ok: bool,
    pub status: u16,
    pub body: JValue,
}

/// Why the sweep's maintenance hold was not given: the rejection's `status` and `code`.
#[derive(Debug, Clone, PartialEq)]
pub struct HoldError {
    pub status: JValue,
    pub code: JValue,
}

/// Why `writePresets` did not write.
#[derive(Debug, Clone, PartialEq)]
pub enum PresetHoldError {
    /// `holdRefused` (the gate is busy): the 409 `modelsBusy`.
    Refused,
    /// Anything else the hold threw (the lease not covering the write, for one): the route throws it.
    Threw(Thrown),
}

/// What the server asks or does, in core's order. Every method is called only when core would.
pub trait Host {
    /// The reranker-settings snapshot an administrator's write runs on (core `withSettings`),
    /// read before anything else; a failure is core's throw (503). The sidecar and reranker
    /// questions of the request are answered from it.
    fn reranker_settings(&mut self) -> Result<(), Thrown>;
    /// `service.invalidateModelReads()`: before a write (any method but GET and HEAD) and again
    /// once it settled.
    fn invalidate_model_reads(&mut self);
    /// `embeddingProtectionUnknown(env)`: the embedding service manages its container but its
    /// protected names are not current.
    fn embedding_protection_unknown(&mut self) -> bool;
    /// `isSystemModel(id)` for a non-empty trimmed `key` (the roles file beside models.ini, then the
    /// deprecated Laya name rule).
    fn is_system_model(&mut self, key: &str) -> bool;
    /// `isSidecarModel(name, env)` for a non-empty trimmed `name`; throws (503) when the reranker
    /// settings cannot be read.
    fn is_sidecar_model(&mut self, name: &str) -> Result<bool, Thrown>;
    /// `rerankModelNames(env).has(name)` for a non-empty trimmed `name`; throws as above.
    fn is_reranker_model(&mut self, name: &str) -> Result<bool, Thrown>;
    /// `await service.servedCatalogue()` (`[{ name, labels }]`, or anything else when unreadable).
    fn served_catalogue(&mut self) -> Result<JValue, Thrown>;
    /// `await modelManager.loadRefusal(alias)`, `null` for none and for a rejection.
    fn load_refusal(&mut self, alias: &str) -> JValue;
    /// The sweep guard's `acquire()`: `holdMaintenance(reason, { recovery: false })`.
    fn sweep_hold(&mut self, reason: &str) -> Result<(), HoldError>;
    /// The hold's release (the model manager refused the sweep).
    fn sweep_release(&mut self);
    /// `sweepGuard.watch(release, models)`: keep the hold until the model manager reports the sweep
    /// over (a lost answer included: the sweep may have started).
    fn sweep_watch(&mut self, models: usize);
    /// `modelScanCache.get('models')?.body` (`undefined` without an entry).
    fn scan_cache_get(&mut self) -> JValue;
    /// `modelScanCache.set('models', { at: Date.now(), body })`.
    fn scan_cache_set(&mut self, body: &JValue);
    /// `modelScanCache.clear()`.
    fn scan_cache_clear(&mut self);
    /// `await modelManager.presetRefusal(name, options)`.
    fn preset_refusal(&mut self, name: &str, options: &JValue) -> Result<JValue, Thrown>;
    /// `modelManager.sizingBudgetGib()`.
    fn sizing_budget_gib(&mut self) -> JValue;
    /// `fetchJson(...)`; `None` when it threw (no answer, a timeout, a redirect).
    fn fetch(&mut self, request: &Fetch) -> Option<Reply>;
    /// `writePresets`: take the preset hold. On `Err` nothing is held: a hold taken and then found
    /// unable to cover the write is released by the host before it answers (as model_maint's
    /// `preset_hold` does); [`Host::preset_release`] follows only an `Ok`.
    fn preset_hold(&mut self) -> Result<(), PresetHoldError>;
    /// `writePresets`: the router's reload under the hold (only when the write asks for it). Its
    /// result is not read; a throw (the lease lost) is the route's.
    fn preset_reload(&mut self) -> Result<(), Thrown>;
    /// `writePresets`: release the hold (after the reload, whatever it did).
    fn preset_release(&mut self);
    /// `reloadPresets({ unload: false })` without a held writer; any answer or failure is ignored.
    fn reload_presets(&mut self);
    /// `Date.now()`.
    fn now_ms(&mut self) -> f64;
    /// `modelManager.recordEvidence(model, record)`, not awaited, a failure ignored.
    fn record_evidence(&mut self, model: &JValue, record: &JValue);
    /// `await modelManager.unload(id)`: whether it answered `ok` (a rejection is `false`).
    fn unload(&mut self, id: &JValue) -> bool;
    /// `await service.clearRoleReferences(id)`: the roles that named it.
    fn clear_role_references(&mut self, id: &JValue) -> Result<JValue, Thrown>;
    /// `await service.forgetModelState(id)`.
    fn forget_model_state(&mut self, id: &JValue) -> Result<(), Thrown>;
}

/// The route's answer.
#[derive(Debug, Clone, PartialEq)]
pub struct Answer {
    pub status: u16,
    pub body: JValue,
    /// `Cache-Control: no-store` was set before it.
    pub no_store: bool,
    /// The route threw; this is `errorResponse`'s answer.
    pub thrown: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Pass,
    Answer(Answer),
    Core,
}

/// Whether `readBody` runs for `method`.
pub fn carries_body(method: &str) -> bool {
    !matches!(method, "GET" | "HEAD" | "DELETE")
}

/// The wrapper's `protectedAction` for this block: an administrator's write (not GET, HEAD or
/// OPTIONS) runs under the reranker-settings lock.
pub fn protected_action(method: &str, path: &str, role: &str) -> bool {
    path.starts_with(PREFIX) && !matches!(method, "GET" | "HEAD" | "OPTIONS") && role == "admin"
}

/// `/^[\w./%:+@-]*$/`.
fn raw_path_ok(raw: &str) -> bool {
    raw.bytes().all(|b| {
        b.is_ascii_alphanumeric()
            || matches!(b, b'_' | b'.' | b'/' | b'%' | b':' | b'+' | b'@' | b'-')
    })
}

/// The decoded manager path, or `None` for 400 `Invalid path`.
pub fn manager_path(raw_rest: &str) -> Option<String> {
    if !raw_path_ok(raw_rest) || raw_rest.contains("..") {
        return None;
    }
    let mut segments = Vec::new();
    for s in raw_rest.split('/') {
        segments.push(js::decode_uri_component(s)?);
    }
    // Named difference: a control character in a decoded segment (core lets `models/delete%0A`
    // past every guard; the model manager strips the final newline).
    if segments.iter().any(|s| {
        s == "."
            || s == ".."
            || s.contains('/')
            || s.contains('\\')
            || s.chars().any(|c| c < '\u{20}' || c == '\u{7f}')
    }) {
        return None;
    }
    Some(segments.join("/"))
}

/// `/^sections\/([^/]+)(?:\/rename)?$/`: the section name.
pub fn section_name(rest: &str) -> Option<&str> {
    let r = rest.strip_prefix("sections/")?;
    let name = if r.contains('/') {
        r.strip_suffix("/rename")?
    } else {
        r
    };
    (!name.is_empty() && !name.contains('/')).then_some(name)
}

/// `/^sections\/[^/]+\/<tail>$/` for one of `tails` (`""`: `/^sections\/[^/]+$/`).
fn section_with(rest: &str, tails: &[&str]) -> bool {
    let Some(r) = rest.strip_prefix("sections/") else {
        return false;
    };
    tails.iter().any(|tail| {
        let name = if tail.is_empty() {
            Some(r)
        } else {
            r.strip_suffix(tail).and_then(|n| n.strip_suffix('/'))
        };
        name.is_some_and(|n| !n.is_empty() && !n.contains('/'))
    })
}

/// `sections/<name>/rename`, by its segments (a section named `rename` is not a rename).
pub fn section_rename(rest: &str) -> bool {
    let parts: Vec<&str> = rest.split('/').collect();
    matches!(parts.as_slice(), ["sections", name, "rename"] if !name.is_empty())
}

/// Whether the forwarded request writes models.ini (#2276), so it runs under the preset hold.
pub fn preset_write(method: &str, path: &str) -> bool {
    (method == "PUT" && (path == "models-ini" || path == "idle-unload"))
        || (matches!(method, "PUT" | "DELETE") && section_with(path, &[""]))
        || (method == "POST" && section_with(path, &["safe-defaults", "rename"]))
}

/// `/^benchmark\/runs\/\d+$/`.
fn benchmark_run(rest: &str) -> bool {
    rest.strip_prefix("benchmark/runs/")
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// The forwarded request's reload condition: `!!r?.ok && r.body?.preset !== 'failed'`.
pub fn reload_wanted(reply: Option<&Reply>) -> bool {
    reply.is_some_and(|r| r.ok && !strict_eq(r.body.get("preset"), &JValue::from("failed")))
}

/// The autoconfig query: the client's `budget_gib` replaced by the sizing budget (#697).
pub fn autoconfig_search(search: &str, budget: &JValue) -> Result<String, Thrown> {
    let query = search.strip_prefix('?').unwrap_or(search);
    let mut pairs: Vec<(String, String)> = form_urlencoded::parse(query.as_bytes())
        .filter(|(k, _)| k != "budget_gib")
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    if to_number(budget)? > 0.0 {
        pairs.push(("budget_gib".to_string(), string(budget)?));
    }
    let text = form_urlencoded::Serializer::new(String::new())
        .extend_pairs(pairs)
        .finish();
    Ok(if text.is_empty() {
        String::new()
    } else {
        format!("?{text}")
    })
}

/// The refusal of a body that is not a JSON object.
pub const NOT_AN_OBJECT: &str = "The request body must be a JSON object.";

/// A guarded route's body (named difference, fail closed): `{}` when empty, else it must parse
/// (as the port holds it) to an object whose `field`, when present, is an array.
pub fn strict_payload(body: Option<&str>, field: Option<&str>) -> Result<JValue, String> {
    let text = body.unwrap_or("");
    if text.is_empty() {
        return Ok(JValue::Obj(Vec::new()));
    }
    let value = match wtf16::parse(text) {
        Ok(v @ JValue::Obj(_)) => v,
        _ => return Err(NOT_AN_OBJECT.to_string()),
    };
    if let Some(f) = field {
        if !matches!(value.get(f), JValue::Undefined | JValue::Arr(_)) {
            return Err(format!("{f} must be an array."));
        }
    }
    Ok(value)
}

fn error(message: &str) -> JValue {
    JValue::obj([("error", JValue::from(message))])
}

fn answer(status: u16, body: JValue, no_store: bool) -> Outcome {
    Outcome::Answer(Answer {
        status,
        body,
        no_store,
        thrown: false,
    })
}

/// `isSystemModel(v)` for any value.
fn is_system(host: &mut impl Host, v: &JValue) -> Result<bool, Thrown> {
    if v.is_nullish() {
        return Ok(false);
    }
    let text = string(v)?;
    let key = trim(&text);
    Ok(!key.is_empty() && host.is_system_model(key))
}

/// `isSidecarModel(v, env)` for any value.
fn is_sidecar(host: &mut impl Host, v: &JValue) -> Result<bool, Thrown> {
    let text = string_or_empty(v)?;
    let n = trim(&text);
    if n.is_empty() {
        return Ok(false);
    }
    host.is_sidecar_model(n)
}

/// `sidecarServiceOf(name)`.
fn sidecar_service(host: &mut impl Host, v: &JValue) -> Result<&'static str, Thrown> {
    let text = string_or_empty(v)?;
    let n = trim(&text);
    Ok(if !n.is_empty() && host.is_reranker_model(n)? {
        "reranker"
    } else {
        "embedding"
    })
}

/// `entry.sections || []` as `.some(...)` reads it.
fn sections_of(entry: &JValue) -> Result<Vec<JValue>, Thrown> {
    let s = entry.get("sections");
    if !s.truthy() {
        return Ok(Vec::new());
    }
    match s {
        JValue::Arr(items) => Ok(items.clone()),
        _ => Err(Thrown::type_error()),
    }
}

/// `scanned.find(f => f.key === key)`: the entry's index.
fn find_key(scanned: &[JValue], key: &JValue) -> Result<Option<usize>, Thrown> {
    for (i, f) in scanned.iter().enumerate() {
        if strict_eq(prop(f, "key")?, key) {
            return Ok(Some(i));
        }
    }
    Ok(None)
}

/// `name` among `f.sections || []` (an array's own item, or (a Node quirk) a substring of a string).
fn sections_include(f: &JValue, name: &str) -> Result<bool, Thrown> {
    let s = prop(f, "sections")?;
    if !s.truthy() {
        return Ok(false);
    }
    match s {
        JValue::Arr(items) => Ok(items.iter().any(|x| x.as_str() == Some(name))),
        JValue::Str(text) => Ok(text.contains(name)),
        _ => Err(Thrown::type_error()),
    }
}

/// An engine id of the delete clean-up, in a JS `Set`: a primitive is itself, an object the place
/// it was read from.
#[derive(Debug, Clone, PartialEq)]
enum Ident {
    Value(JValue),
    At(usize, usize),
}

fn add_unique(set: &mut Vec<(Ident, JValue)>, ident: Ident, value: JValue) {
    let seen = set.iter().any(|(i, _)| match (i, &ident) {
        (Ident::Value(a), Ident::Value(b)) => strict_eq(a, b),
        (a, b) => a == b,
    });
    if !seen {
        set.push((ident, value));
    }
}

fn ident(v: &JValue, entry: usize, slot: usize) -> Ident {
    if matches!(v, JValue::Obj(_) | JValue::Arr(_)) {
        Ident::At(entry, slot)
    } else {
        Ident::Value(v.clone())
    }
}

/// `{ ...detail }`.
fn spread(detail: &JValue) -> Vec<(String, JValue)> {
    match detail {
        JValue::Obj(items) => items.clone(),
        JValue::Arr(items) => items
            .iter()
            .enumerate()
            .map(|(i, v)| (i.to_string(), v.clone()))
            .collect(),
        _ => Vec::new(),
    }
}

/// One request through the block. See the crate documentation for what the server does around it.
pub fn handle(req: &Request<'_>, cfg: &Config, host: &mut impl Host) -> Outcome {
    if !req.path.starts_with(PREFIX) {
        return Outcome::Pass;
    }
    if !NODE_METHODS.contains(&req.method) {
        return answer(400, error("Invalid method"), false);
    }
    if protected_action(req.method, req.path, req.role) {
        if let Err(thrown) = host.reranker_settings() {
            let (status, body) = thrown.answer();
            return Outcome::Answer(Answer {
                status,
                body,
                no_store: false,
                thrown: true,
            });
        }
    }
    let write = !matches!(req.method, "GET" | "HEAD");
    if write {
        host.invalidate_model_reads();
    }
    let mut no_store = false;
    let out = match route(req, cfg, host, &mut no_store) {
        Ok(out) => out,
        Err(thrown) => {
            let (status, body) = thrown.answer();
            Outcome::Answer(Answer {
                status,
                body,
                no_store,
                thrown: true,
            })
        }
    };
    if write && out != Outcome::Core {
        host.invalidate_model_reads();
    }
    out
}

fn route(
    req: &Request<'_>,
    cfg: &Config,
    host: &mut impl Host,
    no_store: &mut bool,
) -> Result<Outcome, Thrown> {
    if req.role != "admin" {
        return Ok(answer(
            403,
            error("Administrator required for model management"),
            false,
        ));
    }
    let base = match cfg.loader_url.as_deref() {
        Some(u) if !u.is_empty() => u.trim_end_matches('/'),
        _ => {
            return Ok(answer(
                404,
                error("Model management service is not configured"),
                false,
            ))
        }
    };
    let raw_rest = req.path.get(PREFIX.len()..).unwrap_or("");
    let Some(rest) = manager_path(raw_rest) else {
        return Ok(answer(400, error("Invalid path"), false));
    };
    let rest = rest.as_str();
    let method = req.method;
    let body: Option<String> = if carries_body(method) {
        match req.body {
            Some(b) if b.len() <= BODY_LIMIT => Some(String::from_utf8_lossy(b).into_owned()),
            _ => return Err(Thrown::new(413, "Request exceeds size limit")),
        }
    } else {
        None
    };
    let section = section_name(rest);
    let section_put = method == "PUT" && section.is_some() && !section_rename(rest);
    let parses = (method == "POST"
        && matches!(
            rest,
            "benchmark/start" | "benchmark/sweep" | "models/delete"
        ))
        || section_put;
    let parsed = if parses {
        let field = match rest {
            "benchmark/start" | "benchmark/sweep" => Some("aliases"),
            "models/delete" => Some("models"),
            _ => None,
        };
        match strict_payload(body.as_deref(), field) {
            Ok(v) => v,
            Err(message) => return Ok(answer(400, error(&message), false)),
        }
    } else {
        JValue::Undefined
    };
    // The model manager is sent what was judged.
    let body = if parses {
        Some(wtf16::stringify(&parsed).unwrap_or_else(|| "{}".to_string()))
    } else {
        body
    };
    let headers = {
        let mut h = vec![("Content-Type".to_string(), "application/json".to_string())];
        if let Some(token) = cfg.loader_token.as_deref().filter(|t| !t.is_empty()) {
            h.push(("X-Model-Loader-Token".to_string(), token.to_string()));
        }
        h
    };
    let caps = cfg.caps;

    // The prompt suite is chat generation (#206), and each model must fit the budget (#697).
    if method == "POST" && rest == "benchmark/start" {
        let aliases = match prop(&parsed, "aliases")? {
            JValue::Arr(items) => items.clone(),
            _ => Vec::new(),
        };
        let catalogue = host.served_catalogue()?;
        let bad = non_chat_aliases(host, &aliases, &catalogue)?;
        if !bad.is_empty() {
            let verb = if bad.len() == 1 { "is" } else { "are" };
            let message = format!(
                "The prompt suite needs chat models; {} {verb} an embedding, reranking or routing model.",
                js::join(&bad, ", ")?
            );
            return Ok(answer(400, error(&message), false));
        }
        if caps.load_refusal {
            for alias in aliases.iter().filter_map(JValue::as_str).take(MAX_ALIASES) {
                let refusal = host.load_refusal(alias);
                if refusal.truthy() {
                    return Ok(answer(409, refusal, false));
                }
            }
        }
    }

    // #873: a sweep's models must fit the budget, and it holds the maintenance gate while it runs.
    let mut sweep: Option<usize> = None;
    if method == "POST" && rest == "benchmark/sweep" {
        let aliases: Vec<String> = match prop(&parsed, "aliases")? {
            JValue::Arr(items) => items
                .iter()
                .filter_map(|a| a.as_str().filter(|s| !s.is_empty()).map(str::to_string))
                .collect(),
            _ => Vec::new(),
        };
        if aliases.len() > MAX_ALIASES {
            return Ok(answer(
                400,
                error("Choose at most 64 models for one sweep."),
                false,
            ));
        }
        if caps.load_refusal {
            for alias in &aliases {
                let refusal = host.load_refusal(alias);
                if refusal.truthy() {
                    return Ok(answer(409, refusal, false));
                }
            }
        }
        if caps.sweep_guard && !aliases.is_empty() {
            if let Err(e) = host.sweep_hold(SWEEP_PAUSE_REASON) {
                let unavailable = strict_eq(&e.status, &JValue::Num(503.0))
                    && strict_eq(&e.code, &JValue::from("maintenanceAuthorityUnavailable"));
                return Ok(if unavailable {
                    answer(
                        503,
                        JValue::obj([
                            ("error", JValue::from(SWEEP_UNAVAILABLE_ERROR)),
                            ("code", JValue::from("maintenanceAuthorityUnavailable")),
                            ("retryable", JValue::Bool(true)),
                        ]),
                        false,
                    )
                } else {
                    answer(409, error(SWEEP_BUSY_ERROR), false)
                });
            }
            sweep = Some(aliases.len());
        }
    }

    // The folder-scan delete (#336, #302): never a system or sidecar model, and the scan it judges by
    // is kept for the clean-up after the delete.
    let mut delete: Option<(Vec<JValue>, JValue)> = None;
    if method == "POST" && rest == "models/delete" {
        if host.embedding_protection_unknown() {
            return Ok(answer(
                409,
                JValue::obj([
                    (
                        "error",
                        JValue::from("Embedding model protection is not current."),
                    ),
                    ("errorId", JValue::from("modelInUse")),
                ]),
                false,
            ));
        }
        let keys = match prop(&parsed, "models")? {
            JValue::Arr(items) => items.clone(),
            _ => Vec::new(),
        };
        let mut scanned = host.scan_cache_get().get("models").clone();
        if !keys.is_empty() && !matches!(scanned, JValue::Arr(_)) {
            let fresh = host.fetch(&Fetch {
                url: format!("{base}/api/v1/models"),
                method: "GET".to_string(),
                headers: headers.clone(),
                body: None,
                timeout_ms: SCAN_TIMEOUT_MS,
            });
            let fresh = fresh.filter(|r| r.ok && r.body.truthy() && r.body.type_of() == "object");
            scanned = fresh
                .as_ref()
                .map_or(JValue::Undefined, |r| r.body.get("models").clone());
            let Some(fresh) = fresh.filter(|_| matches!(scanned, JValue::Arr(_))) else {
                return Ok(answer(409, error("Could not confirm these files are not system models. Refresh the model list and try again."), false));
            };
            host.scan_cache_set(&fresh.body);
        }
        if !keys.is_empty() {
            let list: &[JValue] = match &scanned {
                JValue::Arr(items) => items,
                _ => &[],
            };
            for key in &keys {
                let Some(entry) = find_key(list, key)?.and_then(|i| list.get(i)) else {
                    continue;
                };
                let mut blocked = is_system(host, entry.get("modelId"))?;
                if !blocked {
                    for s in sections_of(entry)? {
                        if is_system(host, &s)? {
                            blocked = true;
                            break;
                        }
                    }
                }
                if blocked {
                    return Ok(answer(
                        400,
                        JValue::obj([
                            ("error", JValue::from(SYSTEM_MODEL_DELETE_REASON)),
                            ("errorId", JValue::from("systemModel")),
                            ("service", JValue::from("decision")),
                        ]),
                        false,
                    ));
                }
            }
            let mut sidecar_blocked = false;
            for key in &keys {
                let Some(entry) = find_key(list, key)?.and_then(|i| list.get(i)) else {
                    continue;
                };
                let mut blocked = is_sidecar(host, entry.get("modelId"))?;
                if !blocked {
                    for s in sections_of(entry)? {
                        if is_sidecar(host, &s)? {
                            blocked = true;
                            break;
                        }
                    }
                }
                if blocked {
                    sidecar_blocked = true;
                    break;
                }
            }
            if sidecar_blocked {
                let mut found = Vec::new();
                for key in &keys {
                    if let Some(entry) = find_key(list, key)?.and_then(|i| list.get(i)) {
                        found.push(entry);
                    }
                }
                let mut names = Vec::new();
                for entry in found {
                    names.push(entry.get("modelId").clone());
                    let s = entry.get("sections");
                    if s.truthy() {
                        names.extend(js::iterate(s)?);
                    }
                }
                let mut held = JValue::Undefined;
                for n in names {
                    if is_sidecar(host, &n)? {
                        held = n;
                        break;
                    }
                }
                let service = sidecar_service(host, &held)?;
                return Ok(answer(
                    409,
                    JValue::obj([
                        ("error", JValue::from(SIDECAR_MODEL_DELETE_REASON)),
                        ("errorId", JValue::from("modelInUse")),
                        ("service", JValue::from(service)),
                    ]),
                    false,
                ));
            }
        }
        delete = Some((keys, scanned));
    }

    // sections/<name>[/rename]: the same guards for a models.ini section written directly.
    if let Some(name) = section.filter(|_| matches!(method, "DELETE" | "POST" | "PUT")) {
        let scanned = host.scan_cache_get().get("models").clone();
        let mut cached: Option<JValue> = None;
        if let JValue::Arr(list) = &scanned {
            for f in list {
                if sections_include(f, name)? {
                    cached = Some(f.clone());
                    break;
                }
            }
        }
        let cached_id = cached
            .as_ref()
            .map_or(JValue::Undefined, |c| c.get("modelId").clone());
        let name_value = JValue::from(name);
        if is_system(host, &name_value)? || (cached.is_some() && is_system(host, &cached_id)?) {
            return Ok(answer(
                400,
                JValue::obj([
                    ("error", JValue::from(SYSTEM_MODEL_DELETE_REASON)),
                    ("errorId", JValue::from("systemModel")),
                    ("service", JValue::from("decision")),
                ]),
                false,
            ));
        }
        if method == "DELETE"
            && (host.embedding_protection_unknown()
                || is_sidecar(host, &name_value)?
                || (cached.is_some() && is_sidecar(host, &cached_id)?))
        {
            let held = if is_sidecar(host, &name_value)? {
                name_value.clone()
            } else {
                cached_id.clone()
            };
            let service = sidecar_service(host, &held)?;
            return Ok(answer(
                409,
                JValue::obj([
                    ("error", JValue::from(SIDECAR_MODEL_DELETE_REASON)),
                    ("errorId", JValue::from("modelInUse")),
                    ("service", JValue::from(service)),
                ]),
                false,
            ));
        }
        if section_put {
            if let Some(message) = sections::path_refusal(&parsed) {
                return Ok(answer(
                    400,
                    JValue::obj([
                        ("error", JValue::Str(message)),
                        ("code", JValue::from("invalid_path")),
                    ]),
                    false,
                ));
            }
            if caps.preset_refusal {
                let options = sections::preset_options(&parsed)?;
                let refusal = host.preset_refusal(name, &options)?;
                if refusal.truthy() {
                    let status = if strict_eq(refusal.get("code"), &JValue::from("invalid_size")) {
                        400
                    } else {
                        409
                    };
                    return Ok(answer(status, refusal, false));
                }
            }
        }
    }

    if method != "GET" {
        host.scan_cache_clear();
    }
    let mut search = req.search.to_string();
    if method == "GET" && section_with(rest, &["autoconfig"]) && caps.sizing_budget {
        let budget = host.sizing_budget_gib();
        search = autoconfig_search(req.search, &budget)?;
    }
    let forward = Fetch {
        url: format!("{base}/api/v1/{raw_rest}{search}"),
        method: method.to_string(),
        headers,
        body,
        timeout_ms: FORWARD_TIMEOUT_MS,
    };

    // #2276: a models.ini write and its reload under the preset editor's hold.
    let result = if preset_write(method, rest) {
        if caps.preset_writer {
            match host.preset_hold() {
                Ok(()) => {}
                Err(PresetHoldError::Refused) => {
                    return Ok(answer(
                        409,
                        JValue::obj([
                            ("error", JValue::from(PRESETS_BUSY)),
                            ("errorId", JValue::from("modelsBusy")),
                            ("code", JValue::from("modelsBusy")),
                        ]),
                        false,
                    ))
                }
                Err(PresetHoldError::Threw(t)) => return Err(t),
            }
            let written = host.fetch(&forward);
            let reload = if reload_wanted(written.as_ref()) {
                host.preset_reload()
            } else {
                Ok(())
            };
            host.preset_release();
            reload?;
            written
        } else {
            let written = host.fetch(&forward);
            if reload_wanted(written.as_ref()) && caps.reload_presets {
                host.reload_presets();
            }
            written
        }
    } else {
        host.fetch(&forward)
    };

    // #873: a refused sweep releases the gate at once; an accepted or unanswered one is watched.
    if let Some(models) = sweep {
        if result.as_ref().is_some_and(|r| !r.ok) {
            host.sweep_release();
        } else {
            host.sweep_watch(models);
        }
    }
    *no_store = true;
    let Some(result) = result else {
        return Ok(answer(
            502,
            error("The model management service is not responding."),
            true,
        ));
    };
    let detail = if result.body.truthy() && result.body.type_of() == "object" {
        result.body.clone()
    } else {
        JValue::obj([("error", JValue::Str(string_or_empty(&result.body)?))])
    };

    // A finished benchmark run viewed in the manager becomes throughput evidence.
    if result.ok && method == "GET" && benchmark_run(rest) && caps.record_evidence {
        let now = host.now_ms();
        for (model, record) in evidence::throughput_records(&detail, now)? {
            host.record_evidence(&model, &record);
        }
    }

    // #302: unload, clear the roles of and forget every engine id the deleted files backed.
    if let Some((keys, scanned)) = delete.filter(|(keys, _)| result.ok && !keys.is_empty()) {
        let list: &[JValue] = match &scanned {
            JValue::Arr(items) => items,
            _ => &[],
        };
        let mut ids: Vec<(Ident, JValue)> = Vec::new();
        for key in &keys {
            let Some(at) = find_key(list, key)? else {
                continue;
            };
            let Some(entry) = list.get(at) else {
                continue;
            };
            let model_id = entry.get("modelId");
            if model_id.truthy() {
                add_unique(&mut ids, ident(model_id, at, 0), model_id.clone());
            }
            let s = entry.get("sections");
            if s.truthy() {
                for (i, section) in js::iterate(s)?.into_iter().enumerate() {
                    add_unique(&mut ids, ident(&section, at, i + 1), section);
                }
            }
        }
        let mut unloaded = false;
        let mut roles: Vec<JValue> = Vec::new();
        for (_, id) in &ids {
            if caps.unload && host.unload(id) {
                unloaded = true;
            }
            for role in js::iterate(&host.clear_role_references(id)?)? {
                let objectish = matches!(role, JValue::Obj(_) | JValue::Arr(_));
                if objectish || !roles.iter().any(|r| strict_eq(r, &role)) {
                    roles.push(role);
                }
            }
            host.forget_model_state(id)?;
        }
        host.scan_cache_clear();
        let mut items = spread(&detail);
        wtf16::set(&mut items, "unloaded", JValue::Bool(unloaded));
        wtf16::set(&mut items, "rolesCleared", JValue::Arr(roles));
        return Ok(answer(result.status, JValue::Obj(items), true));
    }

    let body = if result.ok {
        detail
    } else {
        let d = detail.get("detail");
        let e = detail.get("error");
        let message = if d.truthy() {
            d.clone()
        } else if e.truthy() {
            e.clone()
        } else {
            JValue::from("Model management request failed.")
        };
        JValue::obj([("error", message)])
    };
    Ok(answer(result.status, body, true))
}

/// chat-model-kind.cjs `nonChatAliases(aliases, catalogue)`.
fn non_chat_aliases(
    host: &mut impl Host,
    aliases: &[JValue],
    catalogue: &JValue,
) -> Result<Vec<JValue>, Thrown> {
    let rows: &[JValue] = match catalogue {
        JValue::Arr(items) => items,
        _ => &[],
    };
    let mut bad = Vec::new();
    for alias in aliases {
        let entry = rows
            .iter()
            .find(|m| m.truthy() && strict_eq(m.get("name"), alias));
        let labels = entry.map_or(&JValue::Undefined, |m| m.get("labels"));
        if !chat_generation(host, alias, labels)? {
            bad.push(alias.clone());
        }
    }
    Ok(bad)
}

/// `/^(embedding|embeddings|rerank|reranking|reranker)$/i`.
fn non_chat_label(s: &str) -> bool {
    matches!(
        s.to_ascii_lowercase().as_str(),
        "embedding" | "embeddings" | "rerank" | "reranking" | "reranker"
    )
}

/// chat-model-kind.cjs `isChatGenerationModel(name, labels)`.
fn chat_generation(host: &mut impl Host, name: &JValue, labels: &JValue) -> Result<bool, Thrown> {
    let n = string_or_empty(name)?;
    if n.is_empty() || is_system(host, &JValue::Str(n.clone()))? {
        return Ok(false);
    }
    let lower = n.to_ascii_lowercase();
    if lower.contains("embed") || lower.contains("rerank") {
        return Ok(false);
    }
    if let JValue::Arr(items) = labels {
        for l in items {
            if non_chat_label(&string(l)?) {
                return Ok(false);
            }
        }
    }
    Ok(true)
}
