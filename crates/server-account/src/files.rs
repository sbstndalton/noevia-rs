//! routes/account.cjs for the per-user JSON records Rust owns under NOEVIA_RUST_AUTH: custom
//! instructions (account-instructions.cjs), memory (account-memory.cjs) and interface
//! preferences (account-preferences.cjs), in `UI_DATA_DIR/users/<id>/`. Retention stays Node's (it
//! deletes chats). Reads and validation are the JS modules' own, including how they coerce odd
//! JSON (`Object.hasOwn` takes any value as a property name).

use crate::util;
use crate::{read_json, Account, Fault, Outcome, Reply, Request};
use icu_properties::props::GeneralCategory;
use icu_properties::CodePointMapData;
use js_json::JValue;
use server_auth::Identity;
use server_store::json;
use std::path::{Path, PathBuf};

const INSTRUCTIONS: &str = "account-instructions.json";
const MEMORY: &str = "account-memory.json";
const PREFERENCES: &str = "account-preferences.json";

const MAX_CHARS: usize = 4000;
const MAX_LANGUAGE_CHARS: usize = 40;
const STYLES: &[&str] = &["default", "concise", "detailed"];
const ADVANCED: &[(&str, &[&str])] = &[
    ("length", &["short", "long"]),
    ("tone", &["casual", "formal"]),
    ("formatting", &["minimal", "structured"]),
    ("emoji", &["none", "some"]),
];
const MAX_ITEMS: usize = 50;
const MAX_ITEM_CHARS: usize = 300;
const NOTIFICATION_EVENTS: &[&str] = &["replyFinished", "approvalNeeded"];
const SEND_KEYS: &[&str] = &["enter", "mod-enter"];
const LOCALES: &[&str] = &[
    "system", "en-GB", "en-US", "de-DE", "es-ES", "fr-FR", "it-IT", "nb-NO", "nl-NL", "pt-BR",
    "sv-SE",
];

/// workspace.cjs `userDir(userId)`.
fn user_dir(acct: &Account, uid: &str) -> Result<PathBuf, Fault> {
    let ok = uid.len() == 36 && uid.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-');
    if !ok {
        return Err(Fault::Internal);
    }
    Ok(acct.settings.data_dir.join("users").join(uid))
}

fn readable(m: impl Into<String>) -> Fault {
    Fault::Status(400, m.into())
}

/// `JSON.parse(fs.readFileSync(file))`, `None` where it throws.
fn read_file(dir: &Path, name: &str) -> Option<JValue> {
    let text = std::fs::read_to_string(dir.join(name)).ok()?;
    js_json::parse(&text).ok()
}

/// `Object.hasOwn(table, key)` where the table's own keys are `names`.
fn has_own(names: &[&str], key: &JValue) -> bool {
    js_json::to_js_string(key).is_ok_and(|k| names.contains(&k.as_str()))
}

/// `Object.entries(v)` for a plain-data object or array.
fn entries(v: &JValue) -> Vec<(String, JValue)> {
    match v {
        JValue::Obj(items) => items.clone(),
        JValue::Arr(items) => items
            .iter()
            .enumerate()
            .map(|(i, x)| (i.to_string(), x.clone()))
            .collect(),
        _ => Vec::new(),
    }
}

fn is_object_like(v: &JValue) -> bool {
    matches!(v, JValue::Obj(_) | JValue::Arr(_))
}

fn finite(v: &JValue) -> JValue {
    match v {
        JValue::Num(n) if n.is_finite() => JValue::Num(*n),
        _ => JValue::Null,
    }
}

fn advanced_default() -> JValue {
    JValue::obj(ADVANCED.iter().map(|(k, _)| (*k, JValue::from("auto"))))
}

/// account-instructions.cjs `cleanAdvanced`.
fn clean_advanced(v: &JValue) -> JValue {
    if !v.truthy() || !is_object_like(v) {
        return advanced_default();
    }
    JValue::obj(ADVANCED.iter().map(|(k, names)| {
        let value = v.get(k);
        (
            *k,
            if has_own(names, value) {
                value.clone()
            } else {
                JValue::from("auto")
            },
        )
    }))
}

fn letter(c: char) -> bool {
    matches!(
        CodePointMapData::<GeneralCategory>::new().get(c),
        GeneralCategory::UppercaseLetter
            | GeneralCategory::LowercaseLetter
            | GeneralCategory::TitlecaseLetter
            | GeneralCategory::ModifierLetter
            | GeneralCategory::OtherLetter
    )
}

fn mark(c: char) -> bool {
    matches!(
        CodePointMapData::<GeneralCategory>::new().get(c),
        GeneralCategory::NonspacingMark
            | GeneralCategory::SpacingMark
            | GeneralCategory::EnclosingMark
    )
}

/// `s.replace(/\s+/g, ' ')`.
fn collapse_spaces(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_space = false;
    for c in s.chars() {
        if server_auth::js::is_space(c) {
            if !in_space {
                out.push(' ');
            }
            in_space = true;
        } else {
            out.push(c);
            in_space = false;
        }
    }
    out
}

/// account-instructions.cjs `cleanLanguage`.
fn clean_language(v: &JValue) -> String {
    let Some(s) = v.as_str() else {
        return String::new();
    };
    let collapsed = collapse_spaces(s);
    let trimmed = util::js_trim(&collapsed);
    let mut chars = trimmed.chars();
    let first_ok = chars.next().is_some_and(letter);
    let rest_ok = chars.all(|c| letter(c) || mark(c) || matches!(c, ' ' | '(' | ')' | '\'' | '-'));
    if js_json::js_len(trimmed) <= MAX_LANGUAGE_CHARS
        && trimmed.split(' ').count() <= 4
        && first_ok
        && rest_ok
    {
        trimmed.to_string()
    } else {
        String::new()
    }
}

fn instructions_empty() -> JValue {
    JValue::obj([
        ("text", JValue::from("")),
        ("style", JValue::from("default")),
        ("advanced", advanced_default()),
        ("language", JValue::from("")),
        ("updatedAt", JValue::Null),
    ])
}

/// account-instructions.cjs `read(dir)`.
fn instructions_read(dir: &Path) -> JValue {
    match read_file(dir, INSTRUCTIONS) {
        Some(JValue::Null) | None => instructions_empty(),
        Some(d) => {
            let text = d
                .get("text")
                .as_str()
                .map(|t| util::slice16(t, MAX_CHARS))
                .unwrap_or_default();
            let style = if has_own(STYLES, d.get("style")) {
                d.get("style").clone()
            } else {
                JValue::from("default")
            };
            JValue::obj([
                ("text", JValue::from(text)),
                ("style", style),
                ("advanced", clean_advanced(d.get("advanced"))),
                ("language", JValue::from(clean_language(d.get("language")))),
                ("updatedAt", finite(d.get("updatedAt"))),
            ])
        }
    }
}

/// account-instructions.cjs `write(dir, text, now, style, extras)`.
fn instructions_write(
    acct: &Account,
    dir: &Path,
    body: &JValue,
    now: i64,
) -> Result<JValue, Fault> {
    let text = body.opt("text");
    let style = match body.opt("style") {
        JValue::Undefined | JValue::Null => JValue::from("default"),
        s => s.clone(),
    };
    let Some(text) = text.as_str() else {
        return Err(readable("Send the instructions as text."));
    };
    if !has_own(STYLES, &style) {
        return Err(readable(
            "Choose a response style: default, concise or detailed.",
        ));
    }
    let clean = util::js_trim(text).to_string();
    if js_json::js_len(&clean) > MAX_CHARS {
        return Err(readable(format!(
            "Keep custom instructions under {MAX_CHARS} characters."
        )));
    }
    let previous = instructions_read(dir);
    let mut advanced = previous.get("advanced").clone();
    let extra_advanced = body.opt("advanced");
    if !matches!(extra_advanced, JValue::Undefined) {
        if !extra_advanced.truthy() || !is_object_like(extra_advanced) {
            return Err(readable("Send the advanced style as an object."));
        }
        for (key, value) in entries(extra_advanced) {
            let Some((_, names)) = ADVANCED.iter().find(|(k, _)| *k == key) else {
                return Err(readable(format!("Unknown style control: {key}.")));
            };
            if value.as_str() != Some("auto") && !has_own(names, &value) {
                return Err(readable(format!(
                    "Choose auto, {} for {key}.",
                    names.join(" or ")
                )));
            }
        }
        advanced = clean_advanced(extra_advanced);
    }
    let mut language = previous.get("language").as_str().unwrap_or("").to_string();
    let extra_language = body.opt("language");
    if !matches!(extra_language, JValue::Undefined) {
        if extra_language.as_str() != Some("") && clean_language(extra_language).is_empty() {
            return Err(readable(
                "Name the response language in a few letters, for example Norwegian.",
            ));
        }
        language = clean_language(extra_language);
    }
    let all_auto = ADVANCED
        .iter()
        .all(|(k, _)| advanced.get(k).as_str() == Some("auto"));
    if clean.is_empty() && style.as_str() == Some("default") && language.is_empty() && all_auto {
        json::remove_owned(dir, INSTRUCTIONS, acct.switch)
            .map_err(|_| save_failed("Could not save your instructions"))?;
        return Ok(instructions_empty());
    }
    let record = JValue::obj([
        ("text", JValue::from(clean)),
        ("style", style),
        ("advanced", advanced),
        ("language", JValue::from(language)),
        ("updatedAt", JValue::from(now)),
    ]);
    save(
        acct,
        dir,
        INSTRUCTIONS,
        &record,
        "Could not save your instructions",
    )?;
    Ok(record)
}

fn save_failed(m: &str) -> Fault {
    Fault::Status(500, m.into())
}

fn save(
    acct: &Account,
    dir: &Path,
    name: &str,
    record: &JValue,
    failed: &str,
) -> Result<(), Fault> {
    let text = js_json::stringify(record).ok_or(Fault::Internal)?;
    json::write_owned_text(dir, name, &text, acct.switch).map_err(|_| save_failed(failed))
}

fn memory_empty() -> JValue {
    JValue::obj([
        ("memories", JValue::Arr(Vec::new())),
        ("useProjectMemories", JValue::from(true)),
        ("updatedAt", JValue::Null),
    ])
}

/// account-memory.cjs `read(dir)`.
fn memory_read(dir: &Path) -> JValue {
    match read_file(dir, MEMORY) {
        Some(JValue::Null) | None => memory_empty(),
        Some(d) => {
            let memories: Vec<JValue> = match d.get("memories") {
                JValue::Arr(items) => items
                    .iter()
                    .filter_map(|m| m.as_str())
                    .filter(|m| !util::js_trim(m).is_empty())
                    .map(|m| JValue::from(util::slice16(util::js_trim(m), MAX_ITEM_CHARS)))
                    .take(MAX_ITEMS)
                    .collect(),
                _ => Vec::new(),
            };
            JValue::obj([
                ("memories", JValue::Arr(memories)),
                (
                    "useProjectMemories",
                    JValue::from(d.get("useProjectMemories") != &JValue::Bool(false)),
                ),
                ("updatedAt", finite(d.get("updatedAt"))),
            ])
        }
    }
}

/// account-memory.cjs `write(dir, input, now)`.
fn memory_write(acct: &Account, dir: &Path, input: &JValue, now: i64) -> Result<JValue, Fault> {
    let list = match input.opt("memories") {
        JValue::Arr(items) if input.truthy() && items.iter().all(|m| m.as_str().is_some()) => items,
        _ => return Err(readable("Send memories as a list of text lines.")),
    };
    let use_project = input.get("useProjectMemories");
    if !matches!(use_project, JValue::Undefined) && use_project.as_bool().is_none() {
        return Err(readable("useProjectMemories must be true or false."));
    }
    let mut memories: Vec<String> = Vec::new();
    for m in list.iter().filter_map(JValue::as_str) {
        let clean = util::js_trim(&collapse_spaces(m)).to_string();
        if !clean.is_empty() && !memories.contains(&clean) {
            memories.push(clean);
        }
    }
    if memories.len() > MAX_ITEMS {
        return Err(readable(format!("Keep at most {MAX_ITEMS} memories.")));
    }
    if memories.iter().any(|m| js_json::js_len(m) > MAX_ITEM_CHARS) {
        return Err(readable(format!(
            "Keep each memory under {MAX_ITEM_CHARS} characters."
        )));
    }
    let use_project = use_project != &JValue::Bool(false);
    if memories.is_empty() && use_project {
        json::remove_owned(dir, MEMORY, acct.switch)
            .map_err(|_| save_failed("Could not save your memory"))?;
        return Ok(memory_empty());
    }
    let record = JValue::obj([
        (
            "memories",
            JValue::Arr(memories.into_iter().map(JValue::from).collect()),
        ),
        ("useProjectMemories", JValue::from(use_project)),
        ("updatedAt", JValue::from(now)),
    ]);
    save(acct, dir, MEMORY, &record, "Could not save your memory")?;
    Ok(record)
}

struct Prefs {
    notifications: Vec<(String, bool)>,
    send_key: String,
    locale: String,
    updated_at: JValue,
}

impl Prefs {
    fn defaults() -> Self {
        Prefs {
            notifications: NOTIFICATION_EVENTS
                .iter()
                .map(|e| ((*e).to_string(), true))
                .collect(),
            send_key: "enter".into(),
            locale: "system".into(),
            updated_at: JValue::Null,
        }
    }
    fn json(&self) -> JValue {
        JValue::obj([
            (
                "notifications",
                JValue::obj(
                    self.notifications
                        .iter()
                        .map(|(k, v)| (k.clone(), JValue::from(*v))),
                ),
            ),
            ("sendKey", JValue::from(self.send_key.as_str())),
            ("locale", JValue::from(self.locale.as_str())),
            ("updatedAt", self.updated_at.clone()),
        ])
    }
    fn set(&mut self, event: &str, on: bool) {
        if let Some(slot) = self.notifications.iter_mut().find(|(k, _)| k == event) {
            slot.1 = on;
        }
    }
}

/// account-preferences.cjs `read(dir)` (`normalise`).
fn prefs_read(dir: &Path) -> Prefs {
    let mut out = Prefs::defaults();
    let Some(d) = read_file(dir, PREFERENCES).filter(|d| d.truthy() && is_object_like(d)) else {
        return out;
    };
    let n = d.get("notifications");
    if n.truthy() && is_object_like(n) {
        for e in NOTIFICATION_EVENTS {
            if let Some(b) = n.get(e).as_bool() {
                out.set(e, b);
            }
        }
    }
    if let Some(k) = d.get("sendKey").as_str().filter(|k| SEND_KEYS.contains(k)) {
        out.send_key = k.to_string();
    }
    if let Some(l) = d.get("locale").as_str().filter(|l| LOCALES.contains(l)) {
        out.locale = l.to_string();
    }
    out.updated_at = finite(d.get("updatedAt"));
    out
}

/// account-preferences.cjs `write(dir, patch, now)`.
fn prefs_write(acct: &Account, dir: &Path, patch: &JValue, now: i64) -> Result<JValue, Fault> {
    if !patch.is_object() {
        return Err(readable("Send preferences as an object."));
    }
    let mut next = prefs_read(dir);
    let n = patch.get("notifications");
    if !matches!(n, JValue::Undefined) {
        if !n.truthy() || !is_object_like(n) {
            return Err(readable("Send notification choices as an object."));
        }
        for (event, on) in entries(n) {
            if !NOTIFICATION_EVENTS.contains(&event.as_str()) {
                return Err(readable(format!("Unknown notification event: {event}.")));
            }
            let Some(on) = on.as_bool() else {
                return Err(readable(format!("{event} must be true or false.")));
            };
            next.set(&event, on);
        }
    }
    let k = patch.get("sendKey");
    if !matches!(k, JValue::Undefined) {
        let Some(k) = k.as_str().filter(|k| SEND_KEYS.contains(k)) else {
            return Err(readable("Choose enter or mod-enter to send."));
        };
        next.send_key = k.to_string();
    }
    let l = patch.get("locale");
    if !matches!(l, JValue::Undefined) {
        let Some(l) = l.as_str().filter(|l| LOCALES.contains(l)) else {
            return Err(readable("That format is not available."));
        };
        next.locale = l.to_string();
    }
    next.updated_at = JValue::from(now);
    let record = next.json();
    save(
        acct,
        dir,
        PREFERENCES,
        &record,
        "Could not save your preferences",
    )?;
    Ok(record)
}

fn strs(items: &[&str]) -> JValue {
    JValue::Arr(items.iter().map(|s| JValue::from(*s)).collect())
}

fn merge(record: JValue, extra: Vec<(&str, JValue)>) -> JValue {
    match record {
        JValue::Obj(mut items) => {
            for (k, v) in extra {
                items.push((k.to_string(), v));
            }
            JValue::Obj(items)
        }
        other => other,
    }
}

/// The three routes of routes/account.cjs Rust owns.
pub fn account_files(
    acct: &Account,
    req: &Request,
    authn: &Identity,
    now: i64,
) -> Result<Option<Outcome>, Fault> {
    let (p, m) = (req.path.as_str(), req.method.as_str());
    let which = match p {
        "/api/account/instructions" => 0,
        "/api/account/memory" => 1,
        "/api/account/preferences" => 2,
        _ => return Ok(None),
    };
    let dir = user_dir(acct, &authn.user.id)?;
    let out = |record: JValue| -> JValue {
        match which {
            0 => merge(
                record,
                vec![
                    ("maxChars", JValue::Num(MAX_CHARS as f64)),
                    ("styles", strs(STYLES)),
                    (
                        "advancedOptions",
                        JValue::obj(ADVANCED.iter().map(|(k, names)| {
                            let mut all = vec![JValue::from("auto")];
                            all.extend(names.iter().map(|n| JValue::from(*n)));
                            (*k, JValue::Arr(all))
                        })),
                    ),
                ],
            ),
            1 => merge(
                record,
                vec![
                    ("maxItems", JValue::Num(MAX_ITEMS as f64)),
                    ("maxItemChars", JValue::Num(MAX_ITEM_CHARS as f64)),
                ],
            ),
            _ => merge(
                record,
                vec![(
                    "options",
                    JValue::obj([
                        ("notificationEvents", strs(NOTIFICATION_EVENTS)),
                        ("sendKeys", strs(SEND_KEYS)),
                        ("locales", strs(LOCALES)),
                    ]),
                )],
            ),
        }
    };
    if m == "GET" {
        let record = match which {
            0 => instructions_read(&dir),
            1 => memory_read(&dir),
            _ => prefs_read(&dir).json(),
        };
        return Ok(Some(Reply::json(200, &out(record)).into()));
    }
    if m != "PUT" {
        return Ok(Some(Reply::error(405, "method not allowed").into()));
    }
    // routes/account.cjs: any failure to read the body is a 400 "invalid JSON".
    let Ok(body) = read_json(req, crate::BODY_LIMIT) else {
        return Ok(Some(Reply::error(400, "invalid JSON").into()));
    };
    let result = match which {
        0 => instructions_write(acct, &dir, &body, now),
        1 => memory_write(acct, &dir, &body, now),
        _ => prefs_write(acct, &dir, &body, now),
    };
    Ok(Some(match result {
        Ok(record) => Reply::json(200, &out(record)).into(),
        Err(Fault::Status(s, msg)) => Reply::error(s, &msg).into(),
        Err(f) => return Err(f),
    }))
}
