//! The host side of noevia-core's `server/code-actions.cjs`: what that module does around this
//! crate (which it reaches through `dav-parse.wasm`), over JavaScript values ([`JValue`] in
//! js-json's [`wtf16`] form: a lone surrogate is one mapped character). Nothing here decides an
//! action itself: [`classify`], [`decide`] and [`pick_option`] build the JSON request the core
//! loader (`dav-parse-wasm.cjs` `codeActionsClassify` / `codeActionsDecide` / `codeActionsPick`)
//! sends, hand it to this crate's own [`crate::call`] (the function the wasm export wraps), read the
//! reply as the loader does (UTF-8, fatal; `JSON.parse`; the status; the reply's shape) and apply
//! the crate's answer alone. Only the host's policy is new:
//!
//! - `classifyInput`: the projection of a call ([`classify_input`]); `commandOf` ([`command_of`]).
//! - `askPort`: a refusal or fault becomes a reason (the crate's `{"error"}` code, `reply` for a
//!   reply that is not UTF-8, not JSON or not the documented shape, `too_large` for a request over
//!   [`crate::MAX_INPUT_BYTES`], `input` for a decide input of the wrong shape or a pick whose
//!   wanted kind is not a string, `unexpected` where Node's projection throws a TypeError), cut to
//!   40 UTF-16 units, and is logged to stderr command-text free on its first occurrence and the
//!   10th, 100th, … (at most 200 distinct event/reason pairs), as `portWarn` does.
//! - `unverifiedClassification`: a call the crate could not classify is the riskiest kind
//!   (`external_account`, with `execute_command` when it is or has a command, and the kind's own
//!   class), approval `always`, unreadable, not simple, never standable, `unverified: true`, why in
//!   words ([`WHY`]), only the paths the call states itself, and `outsideWorkspace: true` when the
//!   call says so.
//! - `decide`: an unverified classification is decided by the host floor and then a person (not
//!   standable), without the crate; a classified call the crate cannot answer for gets the floor
//!   (`hostFloor`, the crate's two deny rules: an edit or delete with paths and `inWorkspace ===
//!   false`; a restricted task missing a class other than none or read), then, for an `ambiguous`
//!   network host, the crate is asked again without the command and only its deny is kept, then
//!   [`UNVERIFIED_ASK`], not standable. A floor that cannot be computed asks, not standable.
//! - `pickOption`: any refusal or fault is `{ outcome: 'cancelled' }`.
//!
//! Every answer is the object Node returns, keys in Node's order, strings unit for unit; tests/host.rs
//! replays tests/fixtures/host-node.v1.json, recorded from the Node module over the pinned wasm by
//! tools/gen-host-fixtures.cjs. JavaScript semantics kept: `typeof`, `Array.isArray`, truthiness,
//! `String(x)` (js-json `to_js_string`), `.trim()` (JS whitespace), `Array.prototype.includes`
//! (SameValueZero; two objects are never the same, as no two parts of JSON data are), `new Set`
//! first-occurrence order, own-property table lookups, `.slice(0, 40)` in UTF-16 units.
//!
//! Each `*_with` function takes the crate call as a [`Port`] (Node's `wasmLoader` test option):
//! anything with the [`crate::call`] signature.
//!
//! # Named differences from Node (tested; not copied)
//!
//! - **D1, `decide` never throws.** Node's `decide` throws a TypeError for a `null` input (its
//!   `hostFloor(null)` destructures `null` outside its `try`) and for an unverified classification
//!   whose truthy `unverifiedWhy` `String()` cannot convert (an object with an own `toString` key,
//!   or an array holding one; `unverifiedAsk` runs outside any `try`). The port answers
//!   `{ decision: 'ask', reason: UNVERIFIED_ASK, standable: false }`: a person is asked, and no
//!   standing approval covers it, which is what every other doubt in this module gives. The harness
//!   always passes an object it built, so this is reachable only by a direct caller.
//! - **D2, [`WHY`] is read by own entry.** Node's `whyOf` reads `WHY[fault]` through the prototype
//!   chain, so a fault reason named like an `Object.prototype` member (`constructor`, `toString`,
//!   `hasOwnProperty`, `__proto__`, …) puts a function (dropped by `JSON.stringify`, its source text
//!   on an approval card) or `Object.prototype` in `unverifiedWhy`. The port gives such a reason the
//!   default words, "the checker did not answer". No reason the crate or the loader raises has
//!   such a name; the rows are recorded with injected faults.
//!
//! Not differences, but limits of the Rust side: a panic in the crate (the wasm's `trap`) is not
//! caught (the crate documents that it does not panic); a [`JValue`] is data, so a getter, a
//! callable `toString` or a cycle cannot arise; js-json's `to_js_string` gives up on arrays nested
//! over 1,000 deep (taken as `String()` throwing).

use std::sync::{Mutex, PoisonError};

use js_json::{to_js_string, wtf16, JValue};
use prompt_framing::js::is_js_space;

/// noevia's action classes (`ACTIONS`), as the frozen Node object lists them.
pub const ACTIONS: [(&str, &str); 10] = [
    ("READ", READ),
    ("EDIT", EDIT),
    ("EXECUTE", EXECUTE),
    ("INSTALL", "install_dependency"),
    ("NETWORK", "network"),
    ("DELETE", DELETE),
    ("GIT_PUSH", "git_push"),
    ("BROWSER", "open_browser"),
    ("EXTERNAL", EXTERNAL),
    ("NONE", NONE),
];
const READ: &str = "read_repository";
const EDIT: &str = "edit_file";
const EXECUTE: &str = "execute_command";
const DELETE: &str = "delete";
const EXTERNAL: &str = "external_account";
const NONE: &str = "none";

/// The keys an ACP agent may put its command text under in `rawInput`, in the order they are read
/// (`COMMAND_KEYS`).
pub const COMMAND_KEYS: [&str; 5] = ["command", "cmd", "script", "shell", "commandLine"];

/// How the ACP `kind` maps to a class (`KIND_CLASS`; the crate keeps the same table). Only used to
/// name the classes an unverified call is treated as; a kind not listed is `execute_command`.
pub const KIND_CLASS: [(&str, &str); 9] = [
    ("read", READ),
    ("search", READ),
    ("edit", EDIT),
    ("move", EDIT),
    ("delete", DELETE),
    ("fetch", "network"),
    ("execute", EXECUTE),
    ("think", NONE),
    ("other", EXECUTE),
];

/// Why the crate could not check a call, in words for the approval card (`WHY`).
pub const WHY: [(&str, &str); 3] = [
    ("too_large", "too large or complex to check"),
    ("input", "its arguments have a form that cannot be checked"),
    ("ambiguous", "it names a host that cannot be read exactly"),
];
/// `whyOf` of any other reason.
pub const UNKNOWN_WHY: &str = "the checker did not answer";

/// The reason of the ask when the crate could not answer for a classified call (`UNVERIFIED_ASK`).
pub const UNVERIFIED_ASK: &str =
    "This action could not be verified, so a person has to approve it.";

/// The crate call: [`crate::call`]'s signature, input `u8(op)` and the JSON request.
pub type Port<'a> = dyn FnMut(&[u8]) -> (u32, Vec<u8>) + 'a;

/// `ACTIONS` as the Node object.
pub fn actions_object() -> JValue {
    JValue::obj(ACTIONS.iter().map(|(k, v)| (*k, JValue::from(*v))))
}

/// `COMMAND_KEYS` as the Node array.
pub fn command_keys_array() -> JValue {
    JValue::Arr(COMMAND_KEYS.iter().map(|k| JValue::from(*k)).collect())
}

// ── JavaScript helpers ───────────────────────────────────────────────────────

/// `v && typeof v === 'object'`.
fn is_object(v: &JValue) -> bool {
    matches!(v, JValue::Obj(_) | JValue::Arr(_))
}

fn is_str(v: &JValue, s: &str) -> bool {
    matches!(v, JValue::Str(x) if x == s)
}

/// `Array.prototype.includes`' SameValueZero over JSON data (two objects are never the same one).
fn same_value_zero(a: &JValue, b: &JValue) -> bool {
    match (a, b) {
        (JValue::Undefined, JValue::Undefined) | (JValue::Null, JValue::Null) => true,
        (JValue::Bool(x), JValue::Bool(y)) => x == y,
        (JValue::Num(x), JValue::Num(y)) => x == y || (x.is_nan() && y.is_nan()),
        (JValue::Str(x), JValue::Str(y)) => x == y,
        _ => false,
    }
}

/// `String.prototype.trim` (every JS whitespace character is in the BMP; a mapped lone surrogate
/// is not whitespace).
fn js_trim(s: &str) -> &str {
    s.trim_matches(|c: char| u16::try_from(u32::from(c)).is_ok_and(is_js_space))
}

/// `a.every((x) => typeof x === 'string')` for an array.
fn all_strings(v: &JValue) -> bool {
    matches!(v, JValue::Arr(items) if items.iter().all(|x| matches!(x, JValue::Str(_))))
}

/// `Object.keys(o)` is exactly `keys` (any order) for a non-array object (the loader's `exactKeys`).
fn exact_keys(o: &JValue, keys: &[&str]) -> bool {
    matches!(o, JValue::Obj(items) if items.len() == keys.len()
        && keys.iter().all(|k| items.iter().any(|(x, _)| x == k)))
}

// ── askPort ──────────────────────────────────────────────────────────────────

static SEEN: Mutex<Vec<(String, u64)>> = Mutex::new(Vec::new());

/// `portWarn`'s line for the `n`th occurrence, if that one is logged.
fn warn_line(event: &str, reason: &str, n: u64) -> Option<String> {
    let text = n.to_string();
    let decade = n >= 10
        && text
            .strip_prefix('1')
            .is_some_and(|rest| rest.bytes().all(|b| b == b'0'));
    if n != 1 && !decade {
        return None;
    }
    let times = if n > 1 {
        format!(" ({n} times so far)")
    } else {
        String::new()
    };
    Some(format!(
        "[code-actions] {event} ({reason}); the call was handled as unverified{times}"
    ))
}

/// `portWarn(event, reason)`: counted per pair, at most 200 pairs.
fn port_warn(event: &str, reason: &str) {
    let key = format!("{event}:{reason}");
    let n = {
        let mut seen = SEEN.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(entry) = seen.iter_mut().find(|(k, _)| *k == key) {
            entry.1 += 1;
            entry.1
        } else {
            if seen.len() >= 200 {
                return;
            }
            seen.push((key, 1));
            1
        }
    };
    if let Some(line) = warn_line(event, reason, n) {
        eprintln!("{line}");
    }
}

/// `String(err?.reason || 'unexpected').slice(0, 40)`.
fn fault_reason(reason: &str) -> String {
    if reason.is_empty() {
        "unexpected".to_owned()
    } else {
        wtf16::slice(reason, 40)
    }
}

/// The loader's `jsonOpCall` and reply check, then `askPort`'s reason: the reply, or the
/// (not yet cut) reason.
fn ask_crate(
    port: &mut Port<'_>,
    op: u8,
    args: &JValue,
    shaped: fn(&JValue) -> bool,
) -> Result<JValue, String> {
    let Some(body) = wtf16::stringify(args) else {
        return Err("input".to_owned());
    };
    if body.len() + 1 > crate::MAX_INPUT_BYTES {
        return Err("too_large".to_owned());
    }
    let mut input = Vec::with_capacity(body.len() + 1);
    input.push(op);
    input.extend_from_slice(body.as_bytes());
    let (status, out) = port(&input);
    // TextDecoder('utf-8', { fatal: true }): a leading BOM is dropped.
    let Ok(text) = std::str::from_utf8(&out) else {
        return Err("reply".to_owned());
    };
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let Ok(reply) = wtf16::parse(text) else {
        return Err("reply".to_owned());
    };
    if status != 0 {
        return Err(match reply.get("error") {
            JValue::Str(code) => code.clone(),
            _ => "unknown".to_owned(),
        });
    }
    if !shaped(&reply) {
        return Err("reply".to_owned());
    }
    Ok(reply)
}

/// `askPort(what, …)`: the reply, or the reason (logged).
fn ask_port(what: &str, asked: Result<JValue, String>) -> Result<JValue, String> {
    asked.map_err(|reason| {
        let fault = fault_reason(&reason);
        port_warn(&format!("code_actions.{what}.wasm_fault"), &fault);
        fault
    })
}

const CLASSES: [&str; 10] = [
    READ,
    EDIT,
    EXECUTE,
    "install_dependency",
    "network",
    DELETE,
    "git_push",
    "open_browser",
    EXTERNAL,
    NONE,
];

fn is_class(v: &JValue) -> bool {
    matches!(v, JValue::Str(s) if CLASSES.contains(&s.as_str()))
}

/// `codeActionsClassify`'s reply check.
fn classified_shape(c: &JValue) -> bool {
    exact_keys(
        c,
        &[
            "action",
            "approval",
            "command",
            "paths",
            "readable",
            "actions",
            "simple",
            "standable",
        ],
    ) && is_class(c.get("action"))
        && matches!(c.get("approval"), JValue::Str(a) if ["never", "always", "capability"].contains(&a.as_str()))
        && matches!(c.get("command"), JValue::Str(_))
        && all_strings(c.get("paths"))
        && matches!(c.get("readable"), JValue::Bool(_))
        && matches!(c.get("actions"), JValue::Arr(a) if !a.is_empty() && a.iter().all(is_class))
        && matches!(c.get("simple"), JValue::Bool(_))
        && matches!(c.get("standable"), JValue::Bool(_))
}

/// `codeActionsDecide`'s reply check.
fn decision_shape(d: &JValue) -> bool {
    exact_keys(d, &["decision", "reason"])
        && matches!(d.get("decision"), JValue::Str(s) if ["allow", "ask", "deny"].contains(&s.as_str()))
        && matches!(d.get("reason"), JValue::Str(_))
}

/// `codeActionsPick`'s reply check.
fn pick_shape(o: &JValue) -> bool {
    (exact_keys(o, &["outcome"]) && is_str(o.get("outcome"), "cancelled"))
        || (exact_keys(o, &["outcome", "optionId"])
            && is_str(o.get("outcome"), "selected")
            && matches!(o.get("optionId"), JValue::Str(_)))
}

// ── classify ─────────────────────────────────────────────────────────────────

/// `commandOf(rawInput)`: the command text an ACP agent put in `rawInput`; `None` where Node throws
/// (an array element `String()` cannot convert).
pub fn command_of(raw_input: &JValue) -> Option<String> {
    if !is_object(raw_input) {
        return Some(String::new());
    }
    let joined = |items: &[JValue]| -> Option<String> {
        let parts = items
            .iter()
            .map(|x| to_js_string(x).ok())
            .collect::<Option<Vec<_>>>()?;
        Some(js_trim(&parts.join(" ")).to_owned())
    };
    for key in COMMAND_KEYS {
        match raw_input.get(key) {
            JValue::Str(s) if !js_trim(s).is_empty() => return Some(js_trim(s).to_owned()),
            JValue::Arr(items) if !items.is_empty() => return joined(items),
            _ => {}
        }
    }
    match raw_input.get("args") {
        JValue::Arr(items) if !items.is_empty() => joined(items),
        _ => Some(String::new()),
    }
}

/// `projectElement`: an element `String()` reads the same way everywhere, else `{}` (refused).
fn project_element(e: &JValue) -> JValue {
    match e {
        JValue::Str(_) | JValue::Bool(_) | JValue::Null => e.clone(),
        JValue::Num(n) if n.is_finite() => e.clone(),
        _ => JValue::Obj(Vec::new()),
    }
}

/// `projectValue`.
fn project_value(v: &JValue) -> JValue {
    match v {
        JValue::Str(_) => v.clone(),
        JValue::Arr(items) => JValue::Arr(items.iter().map(project_element).collect()),
        _ => JValue::Null,
    }
}

/// `l && typeof l.path === 'string' ? l.path : null`.
fn location_path(l: &JValue) -> Option<&str> {
    if l.truthy() {
        l.get("path").as_str()
    } else {
        None
    }
}

/// `classifyInput(call)`: what classify sends the crate; `None` where Node throws (a `null` or
/// `undefined` call).
pub fn classify_input(call: &JValue) -> Option<JValue> {
    if call.is_nullish() {
        return None;
    }
    let raw = call.get("rawInput");
    let raw_input = if is_object(raw) {
        let mut out = Vec::new();
        for key in COMMAND_KEYS.iter().copied().chain(["args"]) {
            let value = raw.get(key);
            if !matches!(value, JValue::Undefined) {
                out.push((key.to_owned(), project_value(value)));
            }
        }
        out.push((
            "noeviaOutsideWorkspace".to_owned(),
            JValue::Bool(matches!(
                raw.get("noeviaOutsideWorkspace"),
                JValue::Bool(true)
            )),
        ));
        JValue::Obj(out)
    } else {
        JValue::Null
    };
    let kind = match call.get("kind") {
        JValue::Str(s) => JValue::Str(s.clone()),
        _ => JValue::Null,
    };
    let locations = match call.get("locations") {
        JValue::Arr(items) => items
            .iter()
            .map(|l| location_path(l).map_or(JValue::Null, JValue::from))
            .collect(),
        _ => Vec::new(),
    };
    Some(JValue::obj([
        ("kind", kind),
        ("rawInput", raw_input),
        ("locations", JValue::Arr(locations)),
    ]))
}

/// `whyOf(fault)`, own entries only (D2).
fn why_of(fault: &str) -> &'static str {
    WHY.iter()
        .find(|(k, _)| *k == fault)
        .map_or(UNKNOWN_WHY, |(_, why)| why)
}

/// `unverifiedClassification(call, fault)`.
fn unverified_classification(call: &JValue, fault: &str) -> JValue {
    let raw = call.get("rawInput");
    let command = command_of(raw).unwrap_or_default();
    let mut paths: Vec<&str> = Vec::new();
    if let JValue::Arr(items) = call.get("locations") {
        for p in items.iter().filter_map(location_path) {
            if !p.is_empty() && !paths.contains(&p) {
                paths.push(p);
            }
        }
    }
    let outside = is_object(raw) && matches!(raw.get("noeviaOutsideWorkspace"), JValue::Bool(true));
    let kind = call.get("kind").as_str().unwrap_or("");
    let kind_class = KIND_CLASS
        .iter()
        .find(|(k, _)| *k == kind)
        .map_or(EXECUTE, |(_, c)| c);
    let mut actions = vec![EXTERNAL];
    if kind == "execute" || !command.is_empty() {
        actions.push(EXECUTE);
    }
    if !actions.contains(&kind_class) {
        actions.push(kind_class);
    }
    let mut out = vec![
        ("action", JValue::from(EXTERNAL)),
        ("approval", JValue::from("always")),
        ("command", JValue::Str(command)),
        (
            "paths",
            JValue::Arr(paths.into_iter().map(JValue::from).collect()),
        ),
        ("readable", JValue::Bool(false)),
        (
            "actions",
            JValue::Arr(actions.into_iter().map(JValue::from).collect()),
        ),
        ("simple", JValue::Bool(false)),
        ("standable", JValue::Bool(false)),
        ("unverified", JValue::Bool(true)),
        ("unverifiedWhy", JValue::from(why_of(fault))),
    ];
    if outside {
        out.push(("outsideWorkspace", JValue::Bool(true)));
    }
    JValue::obj(out)
}

/// code-actions.cjs `classify(call)` through this crate.
pub fn classify(call: &JValue) -> JValue {
    classify_with(call, &mut crate::call)
}

/// [`classify`] through `port`.
pub fn classify_with(call: &JValue, port: &mut Port<'_>) -> JValue {
    let empty = JValue::Obj(Vec::new());
    let call = if matches!(call, JValue::Undefined) {
        &empty
    } else {
        call
    };
    let asked = match classify_input(call) {
        Some(projection) => ask_crate(port, 1, &JValue::Arr(vec![projection]), classified_shape),
        None => Err("unexpected".to_owned()),
    };
    match ask_port("classify", asked) {
        Ok(reply) => reply,
        Err(fault) => unverified_classification(call, &fault),
    }
}

// ── decide ───────────────────────────────────────────────────────────────────

/// `decideInput(input)`: the crate's `[classified, capabilities, domains, inWorkspace]`, or the
/// reason Node's `askPort` records when it throws: `"unexpected"` (a TypeError: a `null` or
/// `undefined` input) or `"input"` (a value of the wrong shape).
pub fn decide_input(input: &JValue) -> Result<JValue, &'static str> {
    if input.is_nullish() {
        return Err("unexpected");
    }
    let c = input.get("classified");
    let or_empty = |v: &JValue| match v {
        JValue::Undefined => JValue::Arr(Vec::new()),
        _ => v.clone(),
    };
    let capabilities = or_empty(input.get("capabilities"));
    let domains = or_empty(input.get("domains"));
    let in_workspace = match input.get("inWorkspace") {
        JValue::Undefined => JValue::Null,
        v => v.clone(),
    };
    let actions = match c.get("actions") {
        a @ JValue::Arr(_) => a.clone(),
        _ => JValue::Arr(Vec::new()),
    };
    let ok = is_object(c)
        && matches!(c.get("action"), JValue::Str(_))
        && matches!(c.get("approval"), JValue::Str(_))
        && matches!(c.get("command"), JValue::Str(_))
        && matches!(c.get("readable"), JValue::Bool(_))
        && matches!(c.get("simple"), JValue::Bool(_))
        && all_strings(c.get("paths"))
        && all_strings(&actions)
        && all_strings(&capabilities)
        && all_strings(&domains)
        && matches!(in_workspace, JValue::Bool(_) | JValue::Null);
    if !ok {
        return Err("input");
    }
    let classified = JValue::obj([
        ("action", c.get("action").clone()),
        ("approval", c.get("approval").clone()),
        ("command", c.get("command").clone()),
        ("readable", c.get("readable").clone()),
        ("paths", c.get("paths").clone()),
        ("actions", actions),
        ("simple", c.get("simple").clone()),
    ]);
    Ok(JValue::Arr(vec![
        classified,
        capabilities,
        domains,
        in_workspace,
    ]))
}

fn answer(decision: &str, reason: String) -> JValue {
    JValue::obj([
        ("decision", JValue::from(decision)),
        ("reason", JValue::Str(reason)),
    ])
}

/// `askNotStandable(reason)`.
fn ask_not_standable(reason: String) -> JValue {
    JValue::obj([
        ("decision", JValue::from("ask")),
        ("reason", JValue::Str(reason)),
        ("standable", JValue::Bool(false)),
    ])
}

/// What `hostFloor` gives.
enum Floor {
    /// A deny.
    Deny(JValue),
    /// Neither rule applies (`null`).
    Clear,
    /// It cannot be computed (`undefined`).
    Unknown,
}

/// `hostFloor({ classified, capabilities, inWorkspace })` of a non-null `input`.
fn host_floor(input: &JValue) -> Floor {
    let c = input.get("classified");
    if c.is_nullish() {
        return Floor::Unknown; // `c.actions` throws inside the `try`
    }
    let capabilities = input.get("capabilities");
    let single = [c.get("action").clone()];
    let all: &[JValue] = match c.get("actions") {
        JValue::Arr(items) if !items.is_empty() => items,
        _ => &single,
    };
    let writes = all.iter().any(|a| is_str(a, EDIT) || is_str(a, DELETE));
    if writes
        && matches!(c.get("paths"), JValue::Arr(p) if !p.is_empty())
        && matches!(input.get("inWorkspace"), JValue::Bool(false))
    {
        return Floor::Deny(answer(
            "deny",
            "The path is outside this task\u{2019}s workspace.".to_owned(),
        ));
    }
    let JValue::Arr(granted) = capabilities else {
        return Floor::Unknown;
    };
    let missing = all.iter().find(|a| {
        !is_str(a, NONE)
            && !is_str(a, READ)
            && !granted.is_empty()
            && !granted.iter().any(|g| same_value_zero(g, a))
    });
    let Some(missing) = missing.filter(|m| m.truthy()) else {
        return Floor::Clear;
    };
    let Ok(name) = to_js_string(missing) else {
        return Floor::Unknown; // `String(missing)` throws inside the `try`
    };
    let note = if matches!(c.get("unverified"), JValue::Bool(true)) {
        " (This command could not be checked, so it is treated as the most consequential kind of action.)"
    } else {
        ""
    };
    Floor::Deny(answer(
        "deny",
        format!(
            "This task was not granted {}.{note}",
            name.replace('_', " ")
        ),
    ))
}

/// `unverifiedAsk(c)`; `None` where Node throws (D1).
fn unverified_ask(c: &JValue) -> Option<String> {
    let place = if matches!(c.get("outsideWorkspace"), JValue::Bool(true)) {
        " It reaches outside the task\u{2019}s workspace."
    } else {
        " The paths its command would touch are unknown, so the task\u{2019}s workspace limits could not be checked either."
    };
    let why = c.get("unverifiedWhy");
    let why = if why.truthy() {
        to_js_string(why).ok()?
    } else {
        UNKNOWN_WHY.to_owned()
    };
    Some(format!(
        "This action could not be checked ({why}), so a person has to approve it.{place}"
    ))
}

/// code-actions.cjs `decide({ classified, capabilities, domains, inWorkspace })` through this
/// crate; `None` for `inWorkspace` is `null` (the same as leaving it out). A `capabilities` or
/// `domains` of [`JValue::Undefined`] is left out (Node's default `[]`).
pub fn decide(
    classified: &JValue,
    capabilities: &JValue,
    domains: &JValue,
    in_workspace: Option<bool>,
) -> JValue {
    let input = JValue::obj([
        ("classified", classified.clone()),
        ("capabilities", capabilities.clone()),
        ("domains", domains.clone()),
        (
            "inWorkspace",
            in_workspace.map_or(JValue::Null, JValue::Bool),
        ),
    ]);
    decide_request(&input)
}

/// code-actions.cjs `decide(input)` over Node's whole input object (any value).
pub fn decide_request(input: &JValue) -> JValue {
    decide_request_with(input, &mut crate::call)
}

/// [`decide_request`] through `port`.
pub fn decide_request_with(input: &JValue, port: &mut Port<'_>) -> JValue {
    let empty = JValue::Obj(Vec::new());
    let input = if matches!(input, JValue::Undefined) {
        &empty
    } else {
        input
    };
    let not_verified = || ask_not_standable(UNVERIFIED_ASK.to_owned());
    let c = input.get("classified");
    if matches!(c.get("unverified"), JValue::Bool(true)) {
        // decideUnverified: the floor, then a person.
        if let Floor::Deny(deny) = host_floor(input) {
            return deny;
        }
        return unverified_ask(c).map_or_else(not_verified, ask_not_standable);
    }
    let asked = decide_input(input)
        .map_err(str::to_owned)
        .and_then(|args| ask_crate(port, 2, &args, decision_shape));
    let fault = match ask_port("decide", asked) {
        Ok(reply) => return reply,
        Err(fault) => fault,
    };
    if input.is_nullish() {
        return not_verified(); // D1: Node's hostFloor(null) throws
    }
    let floor = host_floor(input);
    if let Floor::Deny(deny) = floor {
        return deny;
    }
    if fault == "ambiguous"
        && matches!(floor, Floor::Clear)
        && is_str(c.get("approval"), "capability")
    {
        // Asked again without the command: only a deny is kept.
        if let Ok(JValue::Arr(mut args)) = decide_input(input) {
            if let Some(JValue::Obj(classified)) = args.first_mut() {
                wtf16::set(classified, "command", JValue::from(""));
            }
            let again = ask_port(
                "decide.again",
                ask_crate(port, 2, &JValue::Arr(args), decision_shape),
            );
            if let Ok(reply) = again {
                if is_str(reply.get("decision"), "deny") {
                    return reply;
                }
            }
        }
    }
    not_verified()
}

// ── pickOption ───────────────────────────────────────────────────────────────

/// `pickInput(options)`: what pickOption sends the crate.
pub fn pick_input(options: &JValue) -> JValue {
    let JValue::Arr(items) = options else {
        return JValue::Arr(Vec::new());
    };
    JValue::Arr(
        items
            .iter()
            .map(|o| match o.get("optionId") {
                JValue::Str(id) if o.truthy() => JValue::obj([
                    ("optionId", JValue::Str(id.clone())),
                    (
                        "kind",
                        match o.get("kind") {
                            JValue::Str(k) => JValue::Str(k.clone()),
                            _ => JValue::Null,
                        },
                    ),
                ]),
                _ => JValue::Null,
            })
            .collect(),
    )
}

/// code-actions.cjs `pickOption(options, wanted)` through this crate, for a string `wanted`.
pub fn pick_option(options: &JValue, kind: &str) -> JValue {
    pick_option_value(options, &JValue::from(kind))
}

/// code-actions.cjs `pickOption(options, wanted)` for any `wanted` (a non-string cancels).
pub fn pick_option_value(options: &JValue, wanted: &JValue) -> JValue {
    pick_option_with(options, wanted, &mut crate::call)
}

/// [`pick_option_value`] through `port`.
pub fn pick_option_with(options: &JValue, wanted: &JValue, port: &mut Port<'_>) -> JValue {
    let asked = if matches!(wanted, JValue::Str(_)) {
        let args = JValue::Arr(vec![pick_input(options), wanted.clone()]);
        ask_crate(port, 3, &args, pick_shape)
    } else {
        Err("input".to_owned())
    };
    ask_port("pick", asked)
        .unwrap_or_else(|_| JValue::obj([("outcome", JValue::from("cancelled"))]))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn text(v: &JValue) -> String {
        wtf16::stringify(v).unwrap()
    }

    #[test]
    fn a_reply_the_loader_would_refuse_is_a_fault() {
        let call = wtf16::parse(
            r#"{"kind":"edit","rawInput":{"command":"rm x"},"locations":[{"path":"a"}]}"#,
        )
        .unwrap();
        let want = text(&unverified_classification(&call, "reply"));
        let replies: [(u32, &[u8]); 7] = [
            (0, b"not json"),
            (0, b"\xff{}"),
            (0, br#"{"action":"edit_file"}"#),
            (0, br#"{"action":"nope","approval":"always","command":"","paths":[],"readable":true,"actions":["edit_file"],"simple":true,"standable":true}"#),
            (0, br#"{"action":"edit_file","approval":"always","command":"","paths":[],"readable":true,"actions":[],"simple":true,"standable":true}"#),
            (1, b"[]"),
            (2, br#"{"error":5}"#),
        ];
        for (status, bytes) in replies {
            let mut port = |_: &[u8]| (status, bytes.to_vec());
            assert_eq!(text(&classify_with(&call, &mut port)), want, "{bytes:?}");
        }
        // A leading BOM is dropped by the decoder, as TextDecoder does.
        let mut port = |input: &[u8]| {
            let (s, mut out) = crate::call(input);
            out.splice(0..0, "\u{feff}".bytes());
            (s, out)
        };
        assert_eq!(classify_with(&call, &mut port), classify(&call));
        let mut port = |_: &[u8]| (0, br#"{"outcome":"selected"}"#.to_vec());
        assert_eq!(
            text(&pick_option_with(
                &JValue::Arr(vec![]),
                &JValue::from("allow_once"),
                &mut port
            )),
            r#"{"outcome":"cancelled"}"#
        );
        let mut port = |_: &[u8]| {
            (
                0,
                br#"{"decision":"allow","reason":"x","extra":1}"#.to_vec(),
            )
        };
        let base = wtf16::parse(r#"{"classified":{"action":"read_repository","approval":"never","command":"","paths":[],"readable":true,"actions":["read_repository"],"simple":true}}"#).unwrap();
        assert_eq!(
            text(&decide_request_with(&base, &mut port)),
            text(&ask_not_standable(UNVERIFIED_ASK.to_owned()))
        );
    }

    #[test]
    fn faults_are_logged_on_the_first_and_every_power_of_ten() {
        let logged: Vec<u64> = (1..=2000)
            .filter(|&n| warn_line("e", "r", n).is_some())
            .collect();
        assert_eq!(logged, [1, 10, 100, 1000]);
        assert_eq!(
            warn_line("code_actions.classify.wasm_fault", "too_large", 1).unwrap(),
            "[code-actions] code_actions.classify.wasm_fault (too_large); the call was handled as unverified"
        );
        assert_eq!(
            warn_line("e", "r", 10).unwrap(),
            "[code-actions] e (r); the call was handled as unverified (10 times so far)"
        );
    }
}
