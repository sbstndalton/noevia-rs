//! The Rust decisions the Node runner reaches through dav-parse.wasm and its small JS wrappers
//! (core dav-parse-wasm.cjs `autotunePlan`, `tuneContention`, `templateCaps`, `servingVerdict`,
//! `longProfilePairs`/`longProfileSection`; load-advisor.cjs `judge`; long-profile.cjs `pairsOf`/
//! `sectionFor`), as a production `Deps` answers them. The trace replay checks each against the
//! decision Node's default implementation took (`decide` events recorded with `injected: false`).

use crate::deps::Fail;
use crate::js;
use js_json::{wtf16, JValue};

/// A decision crate's reply, or the module's refusal (Node's DavParseError).
fn decided(code: u32, reply: &str, what: &str) -> Result<JValue, Fail> {
    if code != 0 {
        return Err(Fail::new(format!("{what} is unavailable")));
    }
    wtf16::parse(reply).map_err(|_| Fail::new(format!("{what} reply has an unexpected shape")))
}

/// The wrappers send `text.isWellFormed() ? text : text.toWellFormed()` (lone surrogates U+FFFD).
fn well_formed(v: &JValue) -> String {
    let text = wtf16::stringify(v).unwrap_or_default();
    text.chars()
        .map(|c| {
            if wtf16::lone_unit(c).is_some() {
                '\u{fffd}'
            } else {
                c
            }
        })
        .collect()
}

/// `autotunePlan(request)`.
pub fn plan(request: &JValue) -> Result<JValue, Fail> {
    let (code, reply) = autotune_plan::plan_json(&well_formed(request));
    decided(code, &reply, "autotune plan")
}

/// `tuneContention(request)`: the decision's seven fields.
pub fn contention(request: &JValue) -> Result<JValue, Fail> {
    let (code, reply) = tune_contention::decide_json(&well_formed(request));
    let r = decided(code, &reply, "tune contention")?;
    Ok(JValue::obj(
        [
            "action",
            "reason",
            "foreign",
            "unload",
            "fingerprint",
            "since",
            "waitedMs",
        ]
        .iter()
        .map(|k| (*k, r.get(k).clone())),
    ))
}

const CAPS_FIELDS: [&str; 9] = [
    "known",
    "tools",
    "toolCalls",
    "toolRole",
    "systemRole",
    "strictAlternation",
    "raises",
    "thinking",
    "sendTools",
];
/// dav-parse-wasm.cjs `MAX_TEMPLATE_BYTES`.
const MAX_TEMPLATE_BYTES: usize = 256 * 1024;

/// `templateCaps(template)`: the capability booleans (`Err` where Node's wrapper throws: a template
/// over 256 KiB or a reply of another shape).
pub fn template_caps(template: &str) -> Result<JValue, Fail> {
    if template.len() > MAX_TEMPLATE_BYTES {
        return Err(Fail::new("chat template is too large"));
    }
    let reply = chat_template_caps::reply_json(&chat_template_caps::analyze(template));
    let r = decided(0, &reply, "template caps")?;
    if CAPS_FIELDS.iter().any(|k| r.get(k).as_bool().is_none()) {
        return Err(Fail::new("template caps reply has an unexpected shape"));
    }
    Ok(JValue::obj(
        CAPS_FIELDS.iter().map(|k| (*k, r.get(k).clone())),
    ))
}

/// `servingVerdict(status, body)`: `{ passed, kind, reason }`.
pub fn serving_verdict(status: f64, body: &str) -> Result<JValue, Fail> {
    if !(status.fract() == 0.0 && (0.0..=f64::from(u32::MAX)).contains(&status)) {
        return Err(Fail::new("provider error input has the wrong type"));
    }
    let v = chat_template_caps::serving_verdict(status as u32, body);
    let r = decided(0, &chat_template_caps::verdict_json(&v), "serving verdict")?;
    Ok(JValue::obj([
        ("passed", r.get("passed").clone()),
        ("kind", r.get("kind").clone()),
        ("reason", r.get("reason").clone()),
    ]))
}

/// load-advisor.cjs `excerpt(text)`: C0/C1 controls (but tab and newline) as spaces, at most 2048
/// UTF-16 units, made well formed.
fn excerpt(text: &JValue) -> String {
    let Some(text) = text.as_str() else {
        return String::new();
    };
    let flat: String = text
        .chars()
        .map(|c| {
            if matches!(c, '\u{0}'..='\u{8}' | '\u{b}'..='\u{1f}' | '\u{7f}'..='\u{9f}') {
                ' '
            } else {
                c
            }
        })
        .collect();
    wtf16::slice(&flat, 2048)
        .chars()
        .map(|c| {
            if wtf16::lone_unit(c).is_some() {
                '\u{fffd}'
            } else {
                c
            }
        })
        .collect()
}

/// load-advisor.cjs `judge({ cause, evidence })` without a decision service (a tie keeps the rule's
/// verdict, `advisor: 'unavailable'`): `{ outcome, reason, classification }`, or `null` when the
/// verdict is unavailable.
pub fn judge(request: &JValue) -> JValue {
    let cause = request
        .get("cause")
        .as_str()
        .filter(|c| matches!(*c, "oom" | "load" | "timeout" | "time" | "recall"))
        .unwrap_or("load");
    let ev = request.get("evidence");
    let int = |v: &JValue, lo: f64, hi: f64| match v.as_num() {
        Some(n) if n.fract() == 0.0 && n >= lo && n <= hi && n.abs() <= 9_007_199_254_740_991.0 => {
            JValue::Num(n)
        }
        _ => JValue::Null,
    };
    let evidence = if ev.is_object() {
        JValue::obj([
            ("status", int(ev.get("status"), 0.0, 999.0)),
            ("exitCode", int(ev.get("exitCode"), -1024.0, 1024.0)),
            ("text", JValue::from(excerpt(ev.get("text")))),
            (
                "crash",
                JValue::Bool(ev.get("crash") == &JValue::Bool(true)),
            ),
        ])
    } else {
        JValue::Null
    };
    let ask = JValue::obj([("cause", JValue::from(cause)), ("evidence", evidence)]);
    let (code, reply) = load_verdict::verdict_json(&well_formed(&ask));
    let Ok(v) = decided(code, &reply, "load verdict") else {
        return JValue::Null;
    };
    let advisor = if v.get("ask").truthy() {
        "unavailable"
    } else {
        "not_asked"
    };
    JValue::obj([
        ("outcome", v.get("outcome").clone()),
        ("reason", v.get("reason").clone()),
        (
            "classification",
            JValue::obj([
                ("source", v.get("source").clone()),
                ("rule", v.get("rule").clone()),
                ("ruleId", v.get("ruleId").clone()),
                ("advice", v.get("advice").clone()),
                ("adviceUsed", v.get("adviceUsed").clone()),
                ("advisor", JValue::from(advisor)),
            ]),
        ),
    ])
}

/// long-profile.cjs `pairsOf(rows, presets)`: `[{ base, long }]`, `[]` when the module cannot
/// answer. `file_of(row)` is `fileOf(row, presets)`.
pub fn long_pairs(rows: &JValue, file_of: impl Fn(&JValue) -> Option<String>) -> JValue {
    let mut seen = std::collections::HashSet::new();
    let mut input = Vec::new();
    for row in js::items(rows) {
        let Some(id) = row.get("id").as_str().filter(|i| !i.is_empty()) else {
            continue;
        };
        if seen.contains(id) || id.len() > 512 || input.len() >= 512 {
            continue;
        }
        seen.insert(id.to_owned());
        let model = file_of(row).filter(|m| m.len() <= 4096);
        input.push(JValue::obj([
            ("id", JValue::from(id)),
            ("model", model.map_or(JValue::Null, JValue::from)),
        ]));
    }
    let ask = JValue::obj([("op", JValue::from("pairs")), ("rows", JValue::Arr(input))]);
    let (code, reply) = long_profile::run_json(&well_formed(&ask));
    match decided(code, &reply, "long profile") {
        Ok(r) => match r.get("pairs") {
            JValue::Arr(pairs) => JValue::Arr(
                pairs
                    .iter()
                    .map(|p| {
                        JValue::obj([
                            ("base", p.get("base").clone()),
                            ("long", p.get("long").clone()),
                        ])
                    })
                    .collect(),
            ),
            _ => JValue::Arr(Vec::new()),
        },
        Err(_) => JValue::Arr(Vec::new()),
    }
}

/// long-profile.cjs `sectionFor(text, row, presets)`: `{ ok, id, text }` or `{ ok: false, reason }`
/// (`unavailable` when the module cannot answer). `model` and `mmproj` are `fileOf(row, presets)`
/// and the row's `--mmproj` (else its models.ini line).
pub fn long_section(
    text: &str,
    base: &JValue,
    model: Option<String>,
    mmproj: Option<String>,
) -> JValue {
    let ask = JValue::obj([
        ("op", JValue::from("section")),
        ("text", JValue::from(text)),
        ("base", base.clone()),
        ("model", model.map_or(JValue::Null, JValue::from)),
        ("mmproj", mmproj.map_or(JValue::Null, JValue::from)),
    ]);
    let (code, reply) = long_profile::run_json(&well_formed(&ask));
    match decided(code, &reply, "long profile") {
        Ok(r) if r.get("ok") == &JValue::Bool(true) => JValue::obj([
            ("ok", JValue::Bool(true)),
            ("id", r.get("id").clone()),
            ("text", r.get("text").clone()),
        ]),
        Ok(r) => JValue::obj([
            ("ok", JValue::Bool(false)),
            ("reason", r.get("reason").clone()),
        ]),
        Err(_) => JValue::obj([
            ("ok", JValue::Bool(false)),
            ("reason", JValue::from("unavailable")),
        ]),
    }
}

/// The argument after one of `names` in a router row's argv (long-profile.cjs `argOf`).
pub fn arg_of(args: &JValue, names: &[&str]) -> Option<String> {
    let items = js::items(args);
    let i = items
        .iter()
        .position(|a| a.as_str().is_some_and(|s| names.contains(&s)))?;
    items
        .get(i + 1)?
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}
