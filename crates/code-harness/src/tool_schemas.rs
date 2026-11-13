//! `server/code-tool-schemas.cjs`: the Executor guard (noevia#704, part of #511). A coding agent's
//! tool calls are checked for their SHAPE at the ACP boundary (wrong types, a missing or empty
//! required field, NUL or control characters in a path, an unknown kind, oversize content);
//! a malformed call is refused at once with a structured violation the agent can act on, and
//! after the limit the task is blocked. Where a call points is never judged here, nothing here
//! returns "allow", and a call that passes goes on unchanged.
//!
//! The schema check is the stream-guard crate through its wire call (what the wasm module runs
//! for stream-guard.cjs's `checkTextWasm` and `buildCorrectionRequestWasm`), and a command is
//! read by the code-actions crate's `call` (what `code-actions.cjs` `classify` runs), so their
//! answers match Node's by construction. Like Node, it fails closed: when the checker cannot
//! answer, the call is refused as unchecked.

use crate::js::{self, get, object_like};
use crate::Thrown;
use js_json::{wtf16, JValue};

/// Refusals in one task before the guard blocks it.
pub const MAX_VIOLATIONS: u32 = 3;
const PATH_MAX: f64 = 4096.0;
const COMMAND_MAX: f64 = 64.0 * 1024.0;
const TITLE_MAX: f64 = 64.0 * 1024.0;
const URL_MAX: f64 = 8192.0;
const MAX_LOCATIONS: f64 = 64.0;
const MESSAGE_MAX: usize = 300;
const KIND_TEXT_MAX: usize = 40;

/// Every ACP ToolKind (ACP schema `ToolKind`). Anything else is a malformed call.
pub const ACP_KINDS: [&str; 10] = [
    "read",
    "edit",
    "delete",
    "move",
    "search",
    "execute",
    "think",
    "fetch",
    "switch_mode",
    "other",
];
/// Where harnesses put the file a call is about.
pub const PATH_KEYS: [&str; 4] = ["path", "file_path", "filePath", "filepath"];
/// code-actions.cjs `COMMAND_KEYS`: where an ACP agent may put its command text, in order.
const COMMAND_KEYS: [&str; 5] = ["command", "cmd", "script", "shell", "commandLine"];

const READ_FILE: &str = "fs/read_text_file";
const WRITE_FILE: &str = "fs/write_text_file";

/// stream-guard's defaults (stream-guard.cjs `IncrementalValidator`), as the wasm loader sends them.
const GUARD_MAX_DEPTH: f64 = 64.0;
const GUARD_MAX_BYTES: usize = 2 * 1024 * 1024;

fn s(text: &str) -> JValue {
    JValue::Str(text.into())
}

fn num(n: f64) -> JValue {
    JValue::Num(n)
}

fn strings(items: &[&str]) -> JValue {
    JValue::Arr(items.iter().map(|t| s(t)).collect())
}

fn path_schema() -> JValue {
    JValue::obj([("type", s("string")), ("maxLength", num(PATH_MAX))])
}

fn path_properties() -> Vec<(String, JValue)> {
    PATH_KEYS
        .iter()
        .map(|k| (k.to_string(), path_schema()))
        .collect()
}

fn command_schema() -> JValue {
    JValue::obj([
        ("type", strings(&["string", "array"])),
        ("maxLength", num(COMMAND_MAX)),
        ("maxItems", num(4096.0)),
        (
            "items",
            JValue::obj([("type", s("string")), ("maxLength", num(COMMAND_MAX))]),
        ),
    ])
}

fn command_properties() -> Vec<(String, JValue)> {
    COMMAND_KEYS
        .iter()
        .map(|k| (k.to_string(), command_schema()))
        .collect()
}

fn object_schema(properties: Option<Vec<(String, JValue)>>) -> JValue {
    let mut items = vec![("type".to_string(), s("object"))];
    if let Some(p) = properties {
        items.push(("properties".to_string(), JValue::Obj(p)));
    }
    JValue::Obj(items)
}

/// `RAW_INPUT_SCHEMAS[kind]`: `rawInput` per ACP kind, in the stream-guard subset; `None` for a
/// name that is not an ACP kind.
pub fn raw_input_schema(kind: &str) -> Option<JValue> {
    Some(match kind {
        "execute" => {
            let mut p = command_properties();
            p.push(("args".into(), command_schema()));
            p.push(("cwd".into(), path_schema()));
            object_schema(Some(p))
        }
        "edit" | "delete" | "move" | "read" | "search" | "other" => {
            object_schema(Some(path_properties()))
        }
        "fetch" => {
            let mut p = vec![(
                "url".to_string(),
                JValue::obj([("type", s("string")), ("maxLength", num(URL_MAX))]),
            )];
            p.extend(command_properties());
            object_schema(Some(p))
        }
        "think" | "switch_mode" => object_schema(None),
        _ => return None,
    })
}

/// `RAW_INPUT_SCHEMAS`, in Node's key order.
pub fn raw_input_schemas() -> JValue {
    JValue::Obj(
        [
            "execute",
            "edit",
            "delete",
            "move",
            "read",
            "search",
            "fetch",
            "other",
            "think",
            "switch_mode",
        ]
        .iter()
        .filter_map(|k| raw_input_schema(k).map(|v| (k.to_string(), v)))
        .collect(),
    )
}

fn raw_input_schema_or_other(kind: &str) -> JValue {
    raw_input_schema(kind)
        .or_else(|| raw_input_schema("other"))
        .unwrap_or(JValue::Null)
}

/// `toolCallSchema(kind)`: the ACP ToolCall a permission request carries. A name that is not an
/// ACP kind gets `other`'s `rawInput` (difference D3 for names on `Object.prototype`).
pub fn tool_call_schema(kind: &str) -> JValue {
    JValue::obj([
        ("type", s("object")),
        (
            "properties",
            JValue::obj([
                (
                    "toolCallId",
                    JValue::obj([("type", s("string")), ("maxLength", num(256.0))]),
                ),
                (
                    "kind",
                    JValue::obj([("type", s("string")), ("enum", strings(&ACP_KINDS))]),
                ),
                (
                    "title",
                    JValue::obj([("type", s("string")), ("maxLength", num(TITLE_MAX))]),
                ),
                (
                    "status",
                    JValue::obj([("type", s("string")), ("maxLength", num(64.0))]),
                ),
                (
                    "locations",
                    JValue::obj([
                        ("type", s("array")),
                        ("maxItems", num(MAX_LOCATIONS)),
                        (
                            "items",
                            JValue::obj([
                                ("type", s("object")),
                                ("required", strings(&["path"])),
                                (
                                    "properties",
                                    JValue::obj([
                                        ("path", path_schema()),
                                        (
                                            "line",
                                            JValue::obj([("type", strings(&["integer", "null"]))]),
                                        ),
                                    ]),
                                ),
                            ]),
                        ),
                    ]),
                ),
                ("rawInput", raw_input_schema_or_other(kind)),
                ("content", JValue::obj([("type", s("array"))])),
            ]),
        ),
    ])
}

/// `FS_SCHEMAS[method]`: ACP `fs/read_text_file` and `fs/write_text_file` params.
pub fn fs_schema(method: &str) -> Option<JValue> {
    let session = || {
        (
            "sessionId".to_string(),
            JValue::obj([("type", s("string"))]),
        )
    };
    let int_or_null = || JValue::obj([("type", strings(&["integer", "null"]))]);
    Some(match method {
        READ_FILE => JValue::Obj(vec![
            ("type".into(), s("object")),
            ("required".into(), strings(&["path"])),
            (
                "properties".into(),
                JValue::Obj(vec![
                    session(),
                    ("path".into(), path_schema()),
                    ("line".into(), int_or_null()),
                    ("limit".into(), int_or_null()),
                ]),
            ),
        ]),
        WRITE_FILE => JValue::Obj(vec![
            ("type".into(), s("object")),
            ("required".into(), strings(&["path", "content"])),
            (
                "properties".into(),
                JValue::Obj(vec![
                    session(),
                    ("path".into(), path_schema()),
                    ("content".into(), JValue::obj([("type", s("string"))])),
                ]),
            ),
        ]),
        _ => return None,
    })
}

/// `FS_SCHEMAS`.
pub fn fs_schemas() -> JValue {
    JValue::Obj(
        [READ_FILE, WRITE_FILE]
            .iter()
            .filter_map(|m| fs_schema(m).map(|v| (m.to_string(), v)))
            .collect(),
    )
}

const KIND_HINT: &str = "Use one of the ACP tool kinds: read, edit, delete, move, search, execute, think, fetch, switch_mode, other.";

/// `HINTS[key]`: how to fix a call of this kind or method.
fn hint(key: &str) -> Option<&'static str> {
    Some(match key {
        "execute" => "Send the shell command as one non-empty string in rawInput.command.",
        "edit" => "Name the file to change as a non-empty string in locations[].path (or rawInput.path).",
        "delete" => "Name the file to delete as a non-empty string in locations[].path (or rawInput.path).",
        "move" => "Name the file to move as a non-empty string in locations[].path (or rawInput.path).",
        "read" | "search" => "Send each path as a plain non-empty string.",
        "fetch" => "Send the address as a string in rawInput.url.",
        "other" => "Send rawInput as a JSON object and each path as a plain non-empty string.",
        "think" | "switch_mode" => "Send rawInput as a JSON object.",
        "kind" => KIND_HINT,
        READ_FILE => "Send { path } as a plain non-empty string; line and limit, when given, are whole numbers from 1.",
        WRITE_FILE => "Send { path, content } with a plain non-empty path and the whole file as a string.",
        _ => return None,
    })
}

const UNCHECKED_MESSAGE: &str = "Arguments could not be checked.";

/// `isObject`: a plain object (not an array, not `null`).
fn is_object(v: &JValue) -> bool {
    matches!(v, JValue::Obj(_))
}

/// A found violation `{message, path, kind, hint}`.
fn found(message: impl Into<String>, path: impl Into<String>, kind: JValue, hint: &str) -> JValue {
    JValue::obj([
        ("message", JValue::Str(message.into())),
        ("path", JValue::Str(path.into())),
        ("kind", kind),
        ("hint", s(hint)),
    ])
}

fn push_f64(out: &mut Vec<u8>, n: f64) {
    out.extend_from_slice(&n.to_le_bytes());
}

fn push_u32(out: &mut Vec<u8>, n: usize) -> Option<()> {
    out.extend_from_slice(&u32::try_from(n).ok()?.to_le_bytes());
    Some(())
}

fn push_units(out: &mut Vec<u8>, units: &[u16]) {
    for u in units {
        out.extend_from_slice(&u.to_le_bytes());
    }
}

/// `checkTextWasm(schema, text)`: `Ok(Some((message, path)))` for a violation, `Ok(None)` for a
/// valid document, `Err(())` when the module cannot answer.
fn check_text(schema: &JValue, text: &str) -> Result<Option<(String, String)>, ()> {
    let schema_text = wtf16::stringify(schema).ok_or(())?;
    if schema_text.len() > stream_guard::MAX_SCHEMA_BYTES {
        return Err(());
    }
    let mut input = vec![3u8];
    push_f64(&mut input, GUARD_MAX_DEPTH);
    push_f64(&mut input, GUARD_MAX_BYTES as f64);
    push_u32(&mut input, schema_text.len()).ok_or(())?;
    input.extend_from_slice(schema_text.as_bytes());
    let units = js::units(text);
    if units.len() > GUARD_MAX_BYTES {
        input.push(1);
    } else {
        input.push(0);
        push_units(&mut input, &units);
    }
    let (status, reply) = stream_guard::call(&input);
    if status != 0 {
        return Err(());
    }
    let n = reply
        .get(..4)
        .and_then(|b| <[u8; 4]>::try_from(b).ok())
        .map(u32::from_le_bytes)
        .and_then(|n| usize::try_from(n).ok())
        .ok_or(())?;
    let json = reply.get(4..4 + n).ok_or(())?;
    if json.iter().any(|&c| !(0x20..=0x7e).contains(&c)) || reply.len() != 4 + n {
        return Err(());
    }
    let r = wtf16::parse(std::str::from_utf8(json).map_err(|_| ())?).map_err(|_| ())?;
    match r.get("violation") {
        JValue::Null => match r.get("done") {
            JValue::Bool(true) => Ok(None),
            _ => Err(()),
        },
        v => match (v.get("message"), v.get("path")) {
            (JValue::Str(m), JValue::Str(p)) => Ok(Some((m.clone(), p.clone()))),
            _ => Err(()),
        },
    }
}

/// `validate(schema, value)`: the Laya validator over `JSON.stringify(value)`.
fn validate(schema: &JValue, value: &JValue) -> Option<(String, String)> {
    let Some(text) = wtf16::stringify(value) else {
        return Some(("Arguments are missing.".into(), "$".into()));
    };
    match check_text(schema, &text) {
        Err(()) => Some((UNCHECKED_MESSAGE.into(), "$".into())),
        Ok(Some((message, path))) => {
            let path = if path.is_empty() { "$".into() } else { path };
            Some((message, path))
        }
        Ok(None) => None,
    }
}

/// `project(value, schema)`: only the keys the schema names (in the schema's order), so content
/// the schema does not read is never re-parsed.
fn project(value: &JValue, schema: &JValue) -> JValue {
    let (JValue::Obj(_), JValue::Obj(properties)) = (value, schema.get("properties")) else {
        return value.clone();
    };
    JValue::Obj(
        properties
            .iter()
            .filter_map(|(key, _)| match value.get(key) {
                JValue::Undefined => None,
                v => Some((key.clone(), v.clone())),
            })
            .collect(),
    )
}

fn project_tool_call(tool_call: &JValue, kind: &str) -> JValue {
    let mut out = Vec::new();
    for key in ["toolCallId", "kind", "title", "status", "locations"] {
        let v = tool_call.get(key);
        if !v.is_nullish() {
            out.push((key.to_string(), v.clone()));
        }
    }
    let raw = tool_call.get("rawInput");
    if !raw.is_nullish() {
        out.push((
            "rawInput".into(),
            project(raw, &raw_input_schema_or_other(kind)),
        ));
    }
    match tool_call.get("content") {
        JValue::Undefined | JValue::Null => {}
        JValue::Arr(_) => out.push(("content".into(), JValue::Arr(Vec::new()))),
        v => out.push(("content".into(), v.clone())),
    }
    JValue::Obj(out)
}

/// `pathsOf`: every path a tool call names, with where it was found.
fn paths_of(tool_call: &JValue, kind: &str) -> Vec<(String, String)> {
    let mut found = Vec::new();
    if let JValue::Arr(locations) = tool_call.get("locations") {
        for (i, location) in locations.iter().enumerate() {
            if let (true, JValue::Str(p)) = (is_object(location), location.get("path")) {
                found.push((p.clone(), format!("$.locations[{i}].path")));
            }
        }
    }
    let raw = tool_call.get("rawInput");
    if is_object(raw) {
        for key in PATH_KEYS {
            if let JValue::Str(p) = raw.get(key) {
                found.push((p.clone(), format!("$.rawInput.{key}")));
            }
        }
        if kind == "execute" {
            if let JValue::Str(p) = raw.get("cwd") {
                found.push((p.clone(), "$.rawInput.cwd".into()));
            }
        }
    }
    found
}

/// `checkPath`: a non-empty string with no NUL or control character (C0 and DEL).
fn check_path(path: &JValue, at: &str) -> Option<(String, String)> {
    match path {
        JValue::Str(p) if !js::trim(p).is_empty() => {
            if p.chars().any(|c| c <= '\u{1f}' || c == '\u{7f}') {
                Some((
                    format!("Path at {at} contains a NUL or control character"),
                    at.into(),
                ))
            } else {
                None
            }
        }
        _ => Some((format!("Empty path at {at}"), at.into())),
    }
}

/// code-actions.cjs `projectValue`: what classify() reads of one command value.
fn project_command_value(v: &JValue) -> JValue {
    match v {
        JValue::Str(_) => v.clone(),
        JValue::Arr(items) => JValue::Arr(
            items
                .iter()
                .map(|e| match e {
                    JValue::Str(_) | JValue::Bool(_) | JValue::Null => e.clone(),
                    JValue::Num(n) if n.is_finite() => e.clone(),
                    _ => JValue::Obj(Vec::new()),
                })
                .collect(),
        ),
        _ => JValue::Null,
    }
}

/// code-actions.cjs `classifyInput(call)`.
fn classify_input(call: &JValue) -> JValue {
    let raw = call.get("rawInput");
    let raw_input = if object_like(raw) {
        let mut out = Vec::new();
        for key in COMMAND_KEYS.iter().chain(["args"].iter()) {
            let v = raw.get(key);
            if !matches!(v, JValue::Undefined) {
                out.push((key.to_string(), project_command_value(v)));
            }
        }
        out.push((
            "noeviaOutsideWorkspace".into(),
            JValue::Bool(matches!(
                raw.get("noeviaOutsideWorkspace"),
                JValue::Bool(true)
            )),
        ));
        JValue::Obj(out)
    } else {
        JValue::Null
    };
    let locations = match call.get("locations") {
        JValue::Arr(items) => JValue::Arr(
            items
                .iter()
                .map(|l| match (l.truthy(), l.get("path")) {
                    (true, JValue::Str(p)) => JValue::Str(p.clone()),
                    _ => JValue::Null,
                })
                .collect(),
        ),
        _ => JValue::Arr(Vec::new()),
    };
    JValue::obj([
        (
            "kind",
            match call.get("kind") {
                JValue::Str(k) => JValue::Str(k.clone()),
                _ => JValue::Null,
            },
        ),
        ("rawInput", raw_input),
        ("locations", locations),
    ])
}

/// code-actions.cjs `commandOf(rawInput)`, the command of a call the crate could not classify
/// (`''` when `String()` of an element throws, as Node's `try` there gives).
fn command_of(raw: &JValue) -> String {
    if !object_like(raw) {
        return String::new();
    }
    let joined = |items: &[JValue]| -> String {
        let mut parts = Vec::with_capacity(items.len());
        for item in items {
            match js::to_string(item) {
                Ok(t) => parts.push(t),
                Err(_) => return String::new(),
            }
        }
        js::trim(&parts.join(" ")).to_string()
    };
    for key in COMMAND_KEYS {
        match raw.get(key) {
            JValue::Str(t) if !js::trim(t).is_empty() => return js::trim(t).to_string(),
            JValue::Arr(items) if !items.is_empty() => return joined(items),
            _ => {}
        }
    }
    match raw.get("args") {
        JValue::Arr(items) if !items.is_empty() => joined(items),
        _ => String::new(),
    }
}

/// `classify(call).command` by the code-actions crate (op 1 of its wasm call); the unverified
/// classification's command when the crate refuses.
fn classified_command(call: &JValue) -> String {
    let body = wtf16::stringify(&JValue::Arr(vec![classify_input(call)])).unwrap_or_default();
    let mut input = Vec::with_capacity(body.len() + 1);
    input.push(1u8);
    input.extend_from_slice(body.as_bytes());
    let (status, reply) = code_actions::call(&input);
    if status == 0 {
        if let Some(JValue::Str(command)) = std::str::from_utf8(&reply)
            .ok()
            .and_then(|t| wtf16::parse(t).ok())
            .map(|r| r.get("command").clone())
        {
            return command;
        }
    }
    command_of(call.get("rawInput"))
}

/// `checkToolCall(toolCall)`: the schema check for one ACP permission request (the merged
/// ToolCall). `null` for a well-formed call, else `{message, path, kind, hint}`. Pure.
pub fn check_tool_call(tool_call: &JValue) -> JValue {
    if !is_object(tool_call) {
        return found(
            "Type mismatch at $: expected object",
            "$",
            JValue::Null,
            KIND_HINT,
        );
    }
    let stated = tool_call.get("kind");
    let known = matches!(stated, JValue::Str(k) if ACP_KINDS.contains(&k.as_str()));
    if !stated.is_nullish() && !known {
        // D2: Node's String() throws for an own toString; read it as though that key were absent.
        let text = js::to_string(stated).unwrap_or_else(|_| js::to_string_plain(stated));
        let quoted =
            wtf16::stringify(&JValue::Str(js::slice(&text, KIND_TEXT_MAX))).unwrap_or_default();
        return found(
            format!("Unknown tool kind {quoted} at $.kind"),
            "$.kind",
            JValue::Null,
            KIND_HINT,
        );
    }
    let kind = stated.as_str().unwrap_or("other");
    let kind_value = s(kind);
    let hint = hint(kind).unwrap_or_default();
    if let Some((message, path)) =
        validate(&tool_call_schema(kind), &project_tool_call(tool_call, kind))
    {
        return found(message, path, kind_value, hint);
    }
    let paths = paths_of(tool_call, kind);
    for (path, at) in &paths {
        if let Some((message, path)) = check_path(&JValue::Str(path.clone()), at) {
            return found(message, path, kind_value, hint);
        }
    }
    let raw = tool_call.get("rawInput");
    if kind == "execute" {
        let command = classified_command(tool_call);
        if command.is_empty() {
            return found(
                "Missing command at $.rawInput.command",
                "$.rawInput.command",
                kind_value,
                hint,
            );
        }
        if command.contains('\0') {
            return found(
                "Command at $.rawInput.command contains a NUL byte",
                "$.rawInput.command",
                kind_value,
                hint,
            );
        }
    }
    if matches!(kind, "edit" | "delete" | "move") && paths.is_empty() {
        return found(
            format!("A {kind} names no file: missing $.locations[].path"),
            "$.locations",
            kind_value,
            hint,
        );
    }
    if kind == "fetch" {
        let url = match (is_object(raw), raw.get("url")) {
            (true, JValue::Str(u)) => js::trim(u).to_string(),
            _ => String::new(),
        };
        if url.is_empty() && classified_command(tool_call).is_empty() {
            return found(
                "Missing URL at $.rawInput.url",
                "$.rawInput.url",
                kind_value,
                hint,
            );
        }
    }
    JValue::Null
}

/// `checkFsCall(method, params, { maxBytes })`: the schema check for `fs/read_text_file` and
/// `fs/write_text_file` params; `null` or `{message, path, kind, hint}`. `max_bytes` is Node's
/// default `Infinity` when the caller gives none.
pub fn check_fs_call(method: &str, params: &JValue, max_bytes: f64) -> JValue {
    let Some(schema) = fs_schema(method) else {
        return found(format!("Unknown file method {method}"), "$", s(method), "");
    };
    let kind = s(method);
    let hint = hint(method).unwrap_or_default();
    let checked = match params {
        JValue::Obj(items) if matches!(params.get("content"), JValue::Str(_)) => {
            let mut items = items.clone();
            wtf16::set(&mut items, "content", s(""));
            JValue::Obj(items)
        }
        _ => project(params, &schema),
    };
    if let Some((message, path)) = validate(&schema, &checked) {
        return found(message, path, kind, hint);
    }
    if let Some((message, path)) = check_path(params.get("path"), "$.path") {
        return found(message, path, kind, hint);
    }
    for key in ["line", "limit"] {
        let v = params.get(key);
        if !v.is_nullish() && js::less_than_one(v) {
            return found(
                format!("{key} at $.{key} must be 1 or more"),
                format!("$.{key}"),
                kind,
                hint,
            );
        }
    }
    if method == WRITE_FILE {
        if let JValue::Str(content) = params.get("content") {
            if (js::byte_length(content) as f64) > max_bytes {
                return found(
                    format!(
                        "Content at $.content is larger than {} bytes",
                        js_json::number_to_string(max_bytes)
                    ),
                    "$.content",
                    kind,
                    hint,
                );
            }
        }
    }
    JValue::Null
}

fn unchecked_correction() -> JValue {
    JValue::obj([
        ("type", s("schema_violation_correction")),
        (
            "violation",
            JValue::obj([("message", s(UNCHECKED_MESSAGE)), ("path", s("$"))]),
        ),
    ])
}

/// `correctionFor(found)`: the Laya correction request a violation carries (the message cut to
/// 300 units), by stream-guard's correction op; the fixed unchecked request when the module
/// cannot build it (or `found` has no message `String()` can read).
pub fn correction_for(found: &JValue) -> JValue {
    let attempt = || -> Option<JValue> {
        let message = match get(found, "message").ok()? {
            JValue::Undefined | JValue::Null => String::new(),
            v => js::to_string(v).ok()?,
        };
        let path = match found.get("path") {
            p if !p.truthy() => None,
            JValue::Str(p) => Some(p.clone()),
            _ => return None,
        };
        let message = js::slice(&message, stream_guard::MAX_CORRECTION_UNITS);
        let m = js::units(&message);
        let p = path.as_deref().map(js::units);
        if p.as_ref()
            .is_some_and(|p| p.len() > stream_guard::MAX_CORRECTION_UNITS)
        {
            return None;
        }
        let mut input = vec![4u8];
        push_u32(&mut input, MESSAGE_MAX)?;
        input.push(u8::from(p.is_some()));
        push_u32(&mut input, m.len())?;
        push_units(&mut input, &m);
        push_units(&mut input, p.as_deref().unwrap_or(&[]));
        let (status, reply) = stream_guard::call(&input);
        if status != 0 {
            return None;
        }
        let r = wtf16::parse(std::str::from_utf8(&reply).ok()?).ok()?;
        let v = r.get("violation");
        let expected_path = path.map_or(JValue::Null, JValue::Str);
        let ok = r.get("type") == &s("schema_violation_correction")
            && v.get("message") == &JValue::Str(js::slice(&message, MESSAGE_MAX))
            && v.get("path") == &expected_path;
        ok.then(|| {
            JValue::obj([
                ("type", s("schema_violation_correction")),
                (
                    "violation",
                    JValue::obj([
                        ("message", v.get("message").clone()),
                        ("path", v.get("path").clone()),
                    ]),
                ),
            ])
        })
    };
    attempt().unwrap_or_else(unchecked_correction)
}

/// `violationText(violation)`: what was wrong, how to fix it, and how many strikes are left.
/// Throws as Node does for a value that is not a violation.
pub fn violation_text(violation: &JValue) -> Result<String, Thrown> {
    let lead = if get(violation, "blocked")?.truthy() {
        format!(
            "noevia refused this malformed tool call and stopped the task (violation {} of {}).",
            js::to_string(get(violation, "count")?)?,
            js::to_string(get(violation, "limit")?)?
        )
    } else {
        format!(
            "noevia refused this malformed tool call (violation {} of {}). Correct the arguments and call the tool again.",
            js::to_string(get(violation, "count")?)?,
            js::to_string(get(violation, "limit")?)?
        )
    };
    let message = js::to_string(get(get(violation, "violation")?, "message")?)?;
    let hint = js::to_string(get(violation, "hint")?)?;
    Ok(format!("{lead} {message}. {hint}"))
}

/// The JSON-RPC Invalid-params error a malformed `fs/*` call is answered with (Node returns it as
/// an `Error` with `code` and `data`; the harness throws it back to the agent).
#[derive(Debug, Clone, PartialEq)]
pub struct RpcError {
    /// The refusal text, a newline, then `JSON.stringify({noeviaViolation})`.
    pub message: String,
    /// Always -32602.
    pub code: i32,
    /// `{noeviaViolation: violation}`.
    pub data: JValue,
}

/// `violationError(violation)`.
pub fn violation_error(violation: &JValue) -> Result<RpcError, Thrown> {
    let data = JValue::Obj(vec![("noeviaViolation".into(), violation.clone())]);
    let text = violation_text(violation)?;
    Ok(RpcError {
        message: format!("{text}\n{}", wtf16::stringify(&data).unwrap_or_default()),
        code: -32602,
        data,
    })
}

/// `{...value}`: own enumerable properties (an array's indices, a string's UTF-16 units).
fn spread(value: &JValue) -> Vec<(String, JValue)> {
    match value {
        JValue::Obj(items) => items.clone(),
        JValue::Arr(items) => items
            .iter()
            .enumerate()
            .map(|(i, v)| (i.to_string(), v.clone()))
            .collect(),
        JValue::Str(text) => js::units(text)
            .into_iter()
            .enumerate()
            .map(|(i, u)| {
                let c = wtf16::lone_char(u)
                    .map(String::from)
                    .unwrap_or_else(|| String::from_utf16_lossy(&[u]));
                (i.to_string(), JValue::Str(c))
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// `rejectOutcome(selected, violation)`: the protocol's own reject outcome (what a person's
/// Decline sends) with the reason in `_meta.noevia`: a string is the reason itself; a violation
/// adds its text and the violation.
pub fn reject_outcome(selected: &JValue, violation: &JValue) -> Result<JValue, Thrown> {
    let noevia = match violation {
        JValue::Str(reason) => JValue::Obj(vec![("reason".into(), JValue::Str(reason.clone()))]),
        v => JValue::Obj(vec![
            ("reason".into(), JValue::Str(violation_text(v)?)),
            ("violation".into(), v.clone()),
        ]),
    };
    let mut out = spread(selected);
    wtf16::set(
        &mut out,
        "_meta",
        JValue::Obj(vec![("noevia".into(), noevia)]),
    );
    Ok(JValue::Obj(out))
}

/// `clip(error?.message || error)`: what the log keeps of a thrown value.
fn error_text(error: &Thrown) -> Result<String, Thrown> {
    let text = match error {
        Thrown::Error { message, .. } if !message.is_empty() => message.clone(),
        Thrown::Error { name, .. } => name.clone(),
        Thrown::Value(v) if v.is_nullish() => String::new(),
        Thrown::Value(v) => match v.get("message") {
            m if m.truthy() => js::to_string(m)?,
            _ => js::to_string(v)?,
        },
    };
    Ok(js::slice(&text, MESSAGE_MAX))
}

/// What the guard reports to its task: `event(type, data)` appends to the job journal,
/// `log(entry)` is the server log, `on_blocked()` is called once, when the limit is reached, and
/// must stop the agent. Each may fail with what it threw; the guard handles that as Node does.
pub trait GuardHooks {
    fn event(&mut self, kind: &str, data: &JValue) -> Result<(), Thrown>;
    fn log(&mut self, entry: &JValue) -> Result<(), Thrown>;
    fn on_blocked(&mut self) -> Result<(), Thrown>;
}

/// Hooks that record nothing (Node's defaults).
#[derive(Debug, Default, Clone, Copy)]
pub struct NoHooks;

impl GuardHooks for NoHooks {
    fn event(&mut self, _: &str, _: &JValue) -> Result<(), Thrown> {
        Ok(())
    }
    fn log(&mut self, _: &JValue) -> Result<(), Thrown> {
        Ok(())
    }
    fn on_blocked(&mut self) -> Result<(), Thrown> {
        Ok(())
    }
}

/// `createExecutorGuard` options (Node passes numbers; the defaults are Node's).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GuardOptions {
    /// Refusals before the task is blocked (`count >= limit`); default [`MAX_VIOLATIONS`].
    pub limit: f64,
    /// Largest write accepted, in UTF-8 bytes; default `Infinity`.
    pub max_bytes: f64,
}

impl Default for GuardOptions {
    fn default() -> Self {
        GuardOptions {
            limit: f64::from(MAX_VIOLATIONS),
            max_bytes: f64::INFINITY,
        }
    }
}

/// The error a blocked task ends with (Node: an `Error` with `code: 'blocked'`, `publicMessage`
/// and `result`; the job fails with `result.blocked`).
#[derive(Debug, Clone, PartialEq)]
pub struct BlockedError {
    pub message: String,
    /// Always `"blocked"`.
    pub code: &'static str,
    pub public_message: String,
    /// `{blocked: true, reason: 'executor_guard', violations: [...]}`.
    pub result: JValue,
}

enum Record<'a> {
    Permission(&'a JValue),
    Read,
    Write,
}

/// One guard per Code task session (`createExecutorGuard`). [`ExecutorGuard::permission`] returns
/// `None` (go on, unchanged) or the structured violation to answer with as a rejection; the fs
/// checks return `None` or the [`RpcError`] to throw back to the agent. After the limit every call
/// is refused with the same blocked violation, without counting further. An `Err` is what Node
/// would throw out of the call (a failing `log`).
pub struct ExecutorGuard<H: GuardHooks> {
    hooks: H,
    options: GuardOptions,
    count: u64,
    blocked: Option<JValue>,
    history: Vec<JValue>,
}

impl<H: GuardHooks> ExecutorGuard<H> {
    pub fn new(hooks: H, options: GuardOptions) -> Self {
        ExecutorGuard {
            hooks,
            options,
            count: 0,
            blocked: None,
            history: Vec::new(),
        }
    }

    pub fn hooks(&self) -> &H {
        &self.hooks
    }

    pub fn hooks_mut(&mut self) -> &mut H {
        &mut self.hooks
    }

    pub fn into_hooks(self) -> H {
        self.hooks
    }

    /// `guard.violations`.
    pub fn violations(&self) -> u64 {
        self.count
    }

    /// `guard.blocked`.
    pub fn is_blocked(&self) -> bool {
        self.blocked.is_some()
    }

    fn record(&mut self, how: &Record<'_>, v: &JValue) -> Result<(), Thrown> {
        match how {
            Record::Permission(action) => {
                let reason = violation_text(v)?;
                let data = JValue::obj([
                    ("decision", s("denied")),
                    ("action", (*action).clone()),
                    ("automatic", JValue::Bool(true)),
                    ("reason", JValue::Str(reason)),
                    ("violation", v.clone()),
                ]);
                self.hooks.event("approval.decided", &data)
            }
            Record::Read | Record::Write => {
                let name = if matches!(how, Record::Read) {
                    "read_file"
                } else {
                    "write_file"
                };
                let data = JValue::obj([
                    ("name", s(name)),
                    ("failed", JValue::Bool(true)),
                    ("violation", v.clone()),
                ]);
                self.hooks.event("tool.completed", &data)
            }
        }
    }

    fn log_failure(&mut self, event: &str, error: &Thrown) -> Result<(), Thrown> {
        let entry = JValue::obj([
            ("event", s(event)),
            ("error", JValue::Str(error_text(error)?)),
        ]);
        self.hooks.log(&entry)
    }

    fn step_events(&mut self) -> Result<(), Thrown> {
        self.hooks.event(
            "step.started",
            &JValue::obj([("id", s("executor.guard")), ("title", s("Tool-call guard"))]),
        )?;
        let count = self.count as f64;
        self.hooks.event(
            "step.completed",
            &JValue::obj([
                ("id", s("executor.guard")),
                ("failed", JValue::Bool(true)),
                ("blocked", JValue::Bool(true)),
                ("violations", num(count)),
                (
                    "error",
                    JValue::Str(format!(
                        "Stopped after {} malformed tool calls.",
                        js_json::number_to_string(count)
                    )),
                ),
            ]),
        )
    }

    fn refuse(&mut self, found: &JValue, tool: &str, how: Record<'_>) -> Result<JValue, Thrown> {
        if let Some(blocked) = &self.blocked {
            let JValue::Obj(mut items) = blocked.clone() else {
                return Ok(blocked.clone());
            };
            let kind = match found.get("kind") {
                JValue::Undefined | JValue::Null => blocked.get("kind").clone(),
                k => k.clone(),
            };
            wtf16::set(&mut items, "tool", s(tool));
            wtf16::set(&mut items, "kind", kind);
            let again = JValue::Obj(items);
            // The task is ending; the refusal stands either way.
            let _ = self.record(&how, &again);
            return Ok(again);
        }
        self.count += 1;
        let count = self.count as f64;
        let limit = self.options.limit;
        let is_blocked = count >= limit;
        let kind = match found.get("kind") {
            k if k.truthy() => k.clone(),
            _ => JValue::Null,
        };
        let hint = match found.get("hint") {
            h if h.truthy() => h.clone(),
            _ => s(""),
        };
        let JValue::Obj(mut items) = correction_for(found) else {
            return Err(Thrown::type_error("correction is not an object"));
        };
        items.extend([
            ("tool".to_string(), s(tool)),
            ("kind".to_string(), kind.clone()),
            ("hint".to_string(), hint),
            ("count".to_string(), num(count)),
            ("limit".to_string(), num(limit)),
            ("blocked".to_string(), JValue::Bool(is_blocked)),
        ]);
        let violation = JValue::Obj(items);
        let at = violation.get("violation").get("path").clone();
        let message = violation.get("violation").get("message").clone();
        self.history.push(JValue::obj([
            ("tool", s(tool)),
            ("kind", kind.clone()),
            ("path", at.clone()),
            ("message", message),
        ]));
        self.hooks.log(&JValue::obj([
            ("event", s("code.guard_violation")),
            ("tool", s(tool)),
            ("kind", kind),
            ("path", at),
            ("count", num(count)),
            ("limit", num(limit)),
        ]))?;
        if let Err(error) = self.record(&how, &violation) {
            self.log_failure("code.guard_record_failed", &error)?;
        }
        if is_blocked {
            self.blocked = Some(violation.clone());
            if let Err(error) = self.step_events() {
                self.log_failure("code.guard_record_failed", &error)?;
            }
            if let Err(error) = self.hooks.on_blocked() {
                self.log_failure("code.guard_halt_failed", &error)?;
            }
        }
        Ok(violation)
    }

    fn placeholder() -> JValue {
        JValue::Obj(vec![("kind".into(), JValue::Null)])
    }

    /// `guard.permission(toolCall, action = null)`: a permission request's merged ToolCall;
    /// `action` is the class code-actions gave it, for the record (`undefined` is `null`).
    pub fn permission(
        &mut self,
        tool_call: &JValue,
        action: &JValue,
    ) -> Result<Option<JValue>, Thrown> {
        let found = if self.blocked.is_some() {
            Self::placeholder()
        } else {
            check_tool_call(tool_call)
        };
        if found == JValue::Null {
            return Ok(None);
        }
        let action = match action {
            JValue::Undefined => JValue::Null,
            a => a.clone(),
        };
        self.refuse(
            &found,
            "session/request_permission",
            Record::Permission(&action),
        )
        .map(Some)
    }

    /// `guard.readTextFile(params)`.
    pub fn read_text_file(&mut self, params: &JValue) -> Result<Option<RpcError>, Thrown> {
        let found = if self.blocked.is_some() {
            Self::placeholder()
        } else {
            check_fs_call(READ_FILE, params, f64::INFINITY)
        };
        if found == JValue::Null {
            return Ok(None);
        }
        let v = self.refuse(&found, READ_FILE, Record::Read)?;
        violation_error(&v).map(Some)
    }

    /// `guard.writeTextFile(params)`, against the guard's `maxBytes`.
    pub fn write_text_file(&mut self, params: &JValue) -> Result<Option<RpcError>, Thrown> {
        let found = if self.blocked.is_some() {
            Self::placeholder()
        } else {
            check_fs_call(WRITE_FILE, params, self.options.max_bytes)
        };
        if found == JValue::Null {
            return Ok(None);
        }
        let v = self.refuse(&found, WRITE_FILE, Record::Write)?;
        violation_error(&v).map(Some)
    }

    /// `guard.blockedError()`: the error the task ends with once blocked, else `None`.
    pub fn blocked_error(&self) -> Option<BlockedError> {
        self.blocked.as_ref()?;
        let message = format!(
            "Blocked: the coding agent sent {} malformed tool calls, so the task was stopped.",
            self.count
        );
        Some(BlockedError {
            public_message: message.clone(),
            message,
            code: "blocked",
            result: JValue::obj([
                ("blocked", JValue::Bool(true)),
                ("reason", s("executor_guard")),
                ("violations", JValue::Arr(self.history.clone())),
            ]),
        })
    }
}

/// A deployment's check for the guard flag (Node: any function; here one that returns a JS value
/// or what it threw).
pub type FlagCheck = Box<dyn Fn() -> Result<JValue, Thrown> + Send + Sync>;

/// `executorGuardFlag` with `useExecutorGuard`: off until a check is installed, and off unless the
/// installed check answers exactly `true` (a throw is off). Node keeps one per process; here the
/// owner holds it.
#[derive(Default)]
pub struct ExecutorGuardFlag {
    installed: Option<FlagCheck>,
}

impl ExecutorGuardFlag {
    pub fn new() -> Self {
        Self::default()
    }

    /// `useExecutorGuard(enabled)`. (Node's TypeError for a non-function cannot arise here.)
    pub fn install(&mut self, check: FlagCheck) {
        self.installed = Some(check);
    }

    /// `executorGuardFlag.enabled()`.
    pub fn enabled(&self) -> bool {
        self.installed
            .as_ref()
            .is_some_and(|check| matches!(check(), Ok(JValue::Bool(true))))
    }
}
