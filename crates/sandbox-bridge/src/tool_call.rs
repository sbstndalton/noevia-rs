//! `toolCallFor(payload)` from noevia-core `code-sandbox/pi-acp-bridge.cjs`: the ACP tool call
//! noevia classifies, built from what the sandboxed agent reported.
//!
//! Input is the payload as JSON text (the host passes `JSON.stringify(payload)`); output is JSON
//! text the host parses. Every JS quirk is kept, because noevia's approval classifier sees the
//! result on the wire (`JSON.stringify`):
//! - `name = String(payload.toolName || '')` and `String(payload.toolCallId || 'pi-' + name)`
//!   use full JS coercion (numbers, arrays joined, `[object Object]`; an object with an own
//!   `toString` throws a TypeError);
//! - `kind` comes from the tool table's own names only (`Object.hasOwn`, noevia#1000): a tool
//!   named after an `Object.prototype` property (`toString`, `constructor`, `__proto__`, …) is
//!   `other`, like any unknown name;
//! - an array `input` is an object too: it is kept as is, or spread into index keys when the
//!   call is outside the workspace.

use crate::js::{to_string16, truthy, CoerceError};
use crate::json::{parse, write_str16, write_value, Value};

/// Why no tool call could be built.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The payload is not JSON text of an object.
    Input,
    /// JS would throw a TypeError (coercing an object with an own `toString`).
    TypeError,
    /// Coercion nested past [`crate::js::MAX_COERCE_DEPTH`].
    Depth,
}

impl Error {
    pub const fn code(self) -> &'static str {
        match self {
            Error::Input => "input",
            Error::TypeError => "type_error",
            Error::Depth => "depth",
        }
    }
}

impl From<CoerceError> for Error {
    fn from(e: CoerceError) -> Self {
        match e {
            CoerceError::TypeError => Error::TypeError,
            CoerceError::Depth => Error::Depth,
        }
    }
}

/// The tool table (`KIND` in the JS reference). Unknown names, `Object.prototype` ones included,
/// are `other`.
fn kind_for(name: &str) -> &'static str {
    match name {
        "bash" => "execute",
        "write" | "edit" => "edit",
        "read" => "read",
        "grep" | "find" | "ls" => "search",
        _ => "other",
    }
}

fn u16s(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

/// `toolCallFor(JSON.parse(payload))` as JSON text.
pub fn tool_call_json(payload: &str) -> Result<String, Error> {
    let p = parse(payload).ok_or(Error::Input)?;
    if !matches!(p, Value::Obj(_)) {
        return Err(Error::Input);
    }
    let name: Vec<u16> = match p.get("toolName") {
        Some(v) if truthy(v) => to_string16(v)?,
        _ => Vec::new(),
    };
    let empty = Value::Obj(Vec::new());
    let input = match p.get("input") {
        Some(v @ (Value::Obj(_) | Value::Arr(_) | Value::Raw(_))) => v,
        _ => &empty,
    };
    let input_str = |k: &str| input.get(k).and_then(Value::as_str16);
    let path = input_str("path").or_else(|| input_str("file_path"));
    let outside = p.get("outsideWorkspace") == Some(&Value::Bool(true));
    let id: Vec<u16> = match p.get("toolCallId") {
        Some(v) if truthy(v) => to_string16(v)?,
        _ => {
            let mut s = u16s("pi-");
            s.extend(&name);
            s
        }
    };
    let name_text = String::from_utf16_lossy(&name);
    let title: Vec<u16> = match input_str("command") {
        Some(cmd) if name == u16s("bash") => cmd.to_vec(),
        _ if outside => {
            let mut s = name.clone();
            s.extend(u16s(" outside the workspace"));
            s
        }
        _ => name.clone(),
    };
    let kind = if !outside && String::from_utf16(&name).is_ok() {
        kind_for(&name_text)
    } else {
        "other"
    };

    let mut out = String::from("{\"toolCallId\":");
    write_str16(&id, &mut out);
    out.push_str(",\"title\":");
    write_str16(&title, &mut out);
    out.push_str(",\"kind\":\"");
    out.push_str(kind);
    out.push('"');
    out.push_str(",\"rawInput\":");
    if outside {
        // `{ ...input, noeviaOutsideWorkspace: true }`
        let flag = u16s("noeviaOutsideWorkspace");
        let mut fields: Vec<(Vec<u16>, Value<'_>)> = match input {
            Value::Obj(f) => f.clone(),
            Value::Arr(items) => items
                .iter()
                .enumerate()
                .map(|(i, v)| (u16s(&i.to_string()), v.clone()))
                .collect(),
            _ => Vec::new(),
        };
        match fields.iter_mut().find(|(k, _)| *k == flag) {
            Some(slot) => slot.1 = Value::Bool(true),
            None => fields.push((flag, Value::Bool(true))),
        }
        write_value(&Value::Obj(fields), &mut out);
    } else {
        write_value(input, &mut out);
    }
    if let Some(path) = path.filter(|p| !p.is_empty()) {
        out.push_str(",\"locations\":[{\"path\":");
        write_str16(path, &mut out);
        out.push_str("}]");
    }
    out.push('}');
    Ok(out)
}
