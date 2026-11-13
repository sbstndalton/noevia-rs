//! `server/code-meta.cjs`: what a coding run can say about itself (spec-agent-execution §3), read
//! defensively from the free-form shapes ACP harnesses send, saying what was missing rather than
//! defaulting it to zero, and the identity a coding result is scoped to (§1).
//!
//! Every function takes and returns JS values as `js_json` holds them (WTF-16 strings, JS key
//! order). Numbers are read as the crate docs' difference D1 describes.

use crate::js::{self, object_like};
use crate::Thrown;
use js_json::{wtf16, JValue};
use sha2::{Digest, Sha256};

const INPUT: [&str; 6] = [
    "inputTokens",
    "input_tokens",
    "promptTokens",
    "prompt_tokens",
    "input",
    "prompt",
];
const OUTPUT: [&str; 6] = [
    "outputTokens",
    "output_tokens",
    "completionTokens",
    "completion_tokens",
    "output",
    "completion",
];
const TOTAL: [&str; 3] = ["totalTokens", "total_tokens", "total"];
const USED: [&str; 5] = [
    "used",
    "usedTokens",
    "used_tokens",
    "contextUsed",
    "context_used",
];
const SIZE: [&str; 5] = ["size", "contextSize", "context_size", "limit", "window"];
const EXIT_KEYS: [&str; 5] = ["exitCode", "exit_code", "exitStatus", "status_code", "code"];

const NAME_MAX: usize = 80;
const VERSION_MAX: usize = 40;
const EXIT_CODES_MAX: usize = 50;

/// A value read as a number: a number as it is, a string by `Number()`; anything else (and a
/// blank string) is absent, NaN (difference D1; Node runs every value through `Number()`).
fn number(v: &JValue) -> f64 {
    match v {
        JValue::Num(n) => *n,
        // As Node: `true` is 1 (an exit code of `true` reads as a failure), `false` 0.
        JValue::Bool(b) => f64::from(u8::from(*b)),
        JValue::Str(s) if !js::trim(s).is_empty() => js::string_to_number(s),
        _ => f64::NAN,
    }
}

/// `num`: a finite count of at least zero, or absent.
fn num(v: &JValue) -> Option<f64> {
    let n = number(v);
    (n.is_finite() && n >= 0.0).then_some(n)
}

fn first_num(source: &JValue, keys: &[&str]) -> Option<f64> {
    keys.iter().find_map(|key| num(source.get(key)))
}

fn opt(n: Option<f64>) -> JValue {
    n.map_or(JValue::Null, JValue::Num)
}

/// `readUsage(meta)`: `{input, output, total}` from the first place a harness might hang token
/// counts (`usage`, `tokenUsage`, `tokens`, `noevia.usage`, the object itself) that holds an input
/// or an output count, or `null`.
pub fn read_usage(meta: &JValue) -> JValue {
    if !object_like(meta) {
        return JValue::Null;
    }
    let candidates = [
        meta.get("usage"),
        meta.get("tokenUsage"),
        meta.get("tokens"),
        meta.get("noevia").get("usage"),
        meta,
    ];
    let Some(holder) = candidates.into_iter().find(|c| {
        object_like(c) && (first_num(c, &INPUT).is_some() || first_num(c, &OUTPUT).is_some())
    }) else {
        return JValue::Null;
    };
    let input = first_num(holder, &INPUT);
    let output = first_num(holder, &OUTPUT);
    let total = first_num(holder, &TOTAL).or(match (input, output) {
        (Some(i), Some(o)) => Some(i + o),
        _ => None,
    });
    JValue::obj([
        ("input", opt(input)),
        ("output", opt(output)),
        ("total", opt(total)),
    ])
}

/// `Math.round(x)`: the nearest integer, a half rounded up.
fn round(x: f64) -> f64 {
    let f = x.floor();
    if x - f >= 0.5 {
        f + 1.0
    } else {
        f
    }
}

/// `readContext(source)`: context-window occupancy `{used, size, percent}` (never token usage),
/// from the source itself, its `usage`, `_meta` or `context`, or `null`.
pub fn read_context(source: &JValue) -> JValue {
    if !object_like(source) {
        return JValue::Null;
    }
    for holder in [
        source,
        source.get("usage"),
        source.get("_meta"),
        source.get("context"),
    ] {
        if !object_like(holder) {
            continue;
        }
        if let (Some(used), Some(size)) = (first_num(holder, &USED), first_num(holder, &SIZE)) {
            if size > 0.0 {
                return JValue::obj([
                    ("used", JValue::Num(used)),
                    ("size", JValue::Num(size)),
                    ("percent", JValue::Num(round((used / size) * 100.0))),
                ]);
            }
        }
    }
    JValue::Null
}

/// `readExitCode(source)`: a command's exit status wherever the harness put it (zero is a real
/// answer), or `null`.
pub fn read_exit_code(source: &JValue) -> JValue {
    if !object_like(source) {
        return JValue::Null;
    }
    let meta = source.get("_meta");
    for holder in [
        source,
        meta,
        source.get("meta"),
        source.get("rawOutput"),
        meta.get("noevia"),
    ] {
        if !object_like(holder) {
            continue;
        }
        for key in EXIT_KEYS {
            let n = number(holder.get(key));
            if js::is_integer(n) {
                return JValue::Num(n);
            }
        }
    }
    JValue::Null
}

/// `readAgent(result)`: `{name, version, protocolVersion}` from what the agent said about itself at
/// `initialize` (`agentInfo`, `serverInfo` or `_meta.agentInfo`), names cut to 80 and 40 units.
pub fn read_agent(result: &JValue) -> JValue {
    let info = [
        result.get("agentInfo"),
        result.get("serverInfo"),
        result.get("_meta").get("agentInfo"),
    ]
    .into_iter()
    .find(|v| v.truthy())
    .unwrap_or(&JValue::Null);
    let cut = |key: &str, max: usize| match info.get(key) {
        JValue::Str(s) => JValue::Str(js::slice(s, max)),
        _ => JValue::Null,
    };
    JValue::obj([
        ("name", cut("name", NAME_MAX)),
        ("version", cut("version", VERSION_MAX)),
        ("protocolVersion", opt(num(result.get("protocolVersion")))),
    ])
}

/// `[...capabilities].sort()`: spread (an array's elements, a string's code points) and the
/// default sort (by `String()` of each element in UTF-16 order, stable, `undefined` last).
fn sorted_capabilities(capabilities: &JValue) -> Result<Vec<JValue>, Thrown> {
    let items: Vec<JValue> = match capabilities {
        JValue::Arr(items) => items.clone(),
        JValue::Str(s) => s.chars().map(|c| JValue::Str(c.to_string())).collect(),
        _ => return Err(Thrown::type_error("capabilities is not iterable")),
    };
    let (undefined, mut defined): (Vec<JValue>, Vec<JValue>) = items
        .into_iter()
        .partition(|v| matches!(v, JValue::Undefined));
    // V8 converts an element only when it compares it, and with two or more every one is compared.
    if defined.len() >= 2 {
        let mut keyed = Vec::with_capacity(defined.len());
        for v in defined {
            keyed.push((js::units(&js::to_string(&v)?), v));
        }
        keyed.sort_by(|a, b| a.0.cmp(&b.0));
        defined = keyed.into_iter().map(|(_, v)| v).collect();
    }
    defined.extend(undefined);
    Ok(defined)
}

/// evidence.cjs `sorted`: object keys in default-sort (UTF-16) order at every level, then JS
/// property order (`Object.fromEntries` puts array-index keys first).
fn sorted(v: &JValue) -> JValue {
    match v {
        JValue::Arr(items) => JValue::Arr(items.iter().map(sorted).collect()),
        JValue::Obj(items) => {
            let mut keyed: Vec<(Vec<u16>, String, JValue)> = items
                .iter()
                .map(|(k, v)| (js::units(k), k.clone(), sorted(v)))
                .collect();
            keyed.sort_by(|a, b| a.0.cmp(&b.0));
            let mut out: Vec<(String, JValue)> =
                keyed.into_iter().map(|(_, k, v)| (k, v)).collect();
            wtf16::order_keys(&mut out);
            JValue::Obj(out)
        }
        other => other.clone(),
    }
}

/// evidence.cjs `identityHash`: sha256 (hex) of `JSON.stringify(sorted(identity))`.
pub fn identity_hash(identity: &JValue) -> String {
    let text = wtf16::stringify(&sorted(identity)).unwrap_or_default();
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// `codingIdentity({harness, harnessVersion, model, provider, protocolVersion, capabilities,
/// promptPreparation, sandbox} = {})`: `{identity, identityHash}`. Throws (as Node does) for a
/// `null` argument, capabilities that cannot be spread, and capabilities whose `String()`
/// throws while sorting.
pub fn coding_identity(args: &JValue) -> Result<JValue, Thrown> {
    let empty = JValue::Obj(Vec::new());
    let args = match args {
        JValue::Undefined => &empty,
        JValue::Null => {
            return Err(Thrown::type_error(
                "Cannot destructure property 'harness' of '(intermediate value)(intermediate value)(intermediate value)' as it is null.",
            ))
        }
        other => other,
    };
    let or = |key: &str, default: JValue| match args.get(key) {
        JValue::Undefined => default,
        v => v.clone(),
    };
    let truthy_or = |key: &str, default: &str| {
        let v = args.get(key);
        if v.truthy() {
            v.clone()
        } else {
            JValue::Str(default.into())
        }
    };
    let capabilities = sorted_capabilities(&or("capabilities", JValue::Arr(Vec::new())))?;
    let identity = JValue::obj([
        ("backend", JValue::Str("acp".into())),
        ("harness", truthy_or("harness", "unknown")),
        ("harnessVersion", or("harnessVersion", JValue::Null)),
        ("model", or("model", JValue::Null)),
        ("provider", or("provider", JValue::Null)),
        ("protocolVersion", or("protocolVersion", JValue::Null)),
        ("capabilities", JValue::Arr(capabilities)),
        (
            "promptPreparation",
            or("promptPreparation", JValue::Str("direct".into())),
        ),
        ("sandbox", truthy_or("sandbox", "spawn")),
    ]);
    let hash = identity_hash(&identity);
    Ok(JValue::obj([
        ("identity", identity),
        ("identityHash", JValue::Str(hash)),
    ]))
}

fn finite(v: &JValue) -> Option<f64> {
    v.as_num().filter(|n| n.is_finite())
}

/// `summarize(input = {})`: one summary of what a run reported, with `limitations` naming what
/// the harness did not say. Never throws.
pub fn summarize(input: &JValue) -> JValue {
    let empty = JValue::Obj(Vec::new());
    let source = if object_like(input) { input } else { &empty };
    let agent = match source.get("agent") {
        v if object_like(v) => v,
        _ => &empty,
    };
    let usage = Some(source.get("usage")).filter(|v| object_like(v));
    let exits: Vec<&JValue> = match source.get("exits") {
        JValue::Arr(items) => items.iter().filter(|e| object_like(e)).collect(),
        _ => Vec::new(),
    };
    let message_chunks = finite(source.get("messageChunks")).unwrap_or(0.0);
    let context = match source.get("context") {
        c if object_like(c)
            && finite(c.get("used")).is_some()
            && finite(c.get("size")).is_some() =>
        {
            c.clone()
        }
        _ => JValue::Null,
    };
    let reported = usage.filter(|u| !u.get("total").is_nullish());

    let mut limitations = Vec::new();
    if !agent.get("version").truthy() {
        limitations
            .push("The harness did not report its version, so this cannot be scoped to one.");
    }
    if reported.is_none() {
        limitations.push("The harness did not report token usage.");
    }
    if exits.is_empty() {
        limitations.push("No command exit codes were reported.");
    }
    let string = |v: &JValue| match v {
        JValue::Str(s) => JValue::Str(s.clone()),
        _ => JValue::Null,
    };
    let failed = exits
        .iter()
        .filter(|e| {
            e.get("exitCode")
                .as_num()
                .is_some_and(|n| js::is_integer(n) && n != 0.0)
        })
        .count();
    JValue::obj([
        ("harness", string(agent.get("name"))),
        ("harnessVersion", string(agent.get("version"))),
        ("protocolVersion", opt(finite(agent.get("protocolVersion")))),
        ("usage", reported.cloned().unwrap_or(JValue::Null)),
        ("context", context),
        ("commands", JValue::Num(exits.len() as f64)),
        ("failedCommands", JValue::Num(failed as f64)),
        (
            "exitCodes",
            JValue::Arr(
                exits
                    .iter()
                    .take(EXIT_CODES_MAX)
                    .map(|e| (*e).clone())
                    .collect(),
            ),
        ),
        ("messageChunks", JValue::Num(message_chunks)),
        (
            "limitations",
            JValue::Arr(
                limitations
                    .into_iter()
                    .map(|s| JValue::Str(s.into()))
                    .collect(),
            ),
        ),
    ])
}
