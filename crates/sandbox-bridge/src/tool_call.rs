//! `toolCallFor(payload)` from noevia-core `code-sandbox/pi-acp-bridge.cjs`: the ACP tool call
//! noevia classifies, built from what the sandboxed agent reported.
//!
//! Input is the payload as JSON text (the host passes `JSON.stringify(payload)`); output is JSON
//! text the host parses. Every JS quirk is kept, because noevia's approval classifier sees the
//! result on the wire (`JSON.stringify`):
//! - `name = String(payload.toolName || '')` and `String(payload.toolCallId || 'pi-' + name)`
//!   use full JS coercion (numbers, arrays joined, `[object Object]`; an object with an own
//!   `toString` throws a TypeError);
//! - `KIND[name]` is a lookup on an object literal, so `Object.prototype` names leak through: a
//!   method name (`toString`, `constructor`, …) yields a function, which `JSON.stringify` drops
//!   (no `kind` on the wire), and `__proto__` yields `Object.prototype`, which serialises as `{}`;
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

/// `Object.prototype`'s own property names whose value is a function (Node 22 / V8).
const PROTO_METHODS: [&str; 11] = [
    "constructor",
    "__defineGetter__",
    "__defineSetter__",
    "hasOwnProperty",
    "__lookupGetter__",
    "__lookupSetter__",
    "isPrototypeOf",
    "propertyIsEnumerable",
    "toString",
    "valueOf",
    "toLocaleString",
];

enum Kind {
    Text(&'static str),
    /// A function: absent from `JSON.stringify`.
    Omitted,
    /// `Object.prototype`: `{}`.
    EmptyObject,
}

fn kind_for(name: &str) -> Kind {
    match name {
        "bash" => Kind::Text("execute"),
        "write" | "edit" => Kind::Text("edit"),
        "read" => Kind::Text("read"),
        "grep" | "find" | "ls" => Kind::Text("search"),
        "__proto__" => Kind::EmptyObject,
        n if PROTO_METHODS.contains(&n) => Kind::Omitted,
        _ => Kind::Text("other"),
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
    let kind = if outside {
        Kind::Text("other")
    } else if String::from_utf16(&name).is_ok() {
        kind_for(&name_text)
    } else {
        Kind::Text("other")
    };

    let mut out = String::from("{\"toolCallId\":");
    write_str16(&id, &mut out);
    out.push_str(",\"title\":");
    write_str16(&title, &mut out);
    match kind {
        Kind::Text(k) => {
            out.push_str(",\"kind\":\"");
            out.push_str(k);
            out.push('"');
        }
        Kind::EmptyObject => out.push_str(",\"kind\":{}"),
        Kind::Omitted => {}
    }
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
