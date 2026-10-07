//! The start message from noevia-core `code-sandbox/supervisor.cjs`: the first line on a
//! connection, which names the worktree and the agent's environment.
//!
//! ```js
//! let start; try { start = JSON.parse(line); } catch { refuse }
//! if (!start || start.noevia !== 'start') refuse
//! insideRoot(root, start.cwd)   // String(start.cwd || '') inside a try: a throw is a refusal
//! cleanEnv(start.env)           // own allowlisted keys with string values shorter than 4096
//! ```
//!
//! Reply: `{"start":false}`, or `{"start":true,"cwd":"…"|null,"env":{…}}` where `cwd` is
//! `String(start.cwd || '')` (null where that throws) and `env` the allowlisted variables. The
//! host still resolves `cwd` with realpath, checks containment and adds its HOME fallback.

use crate::js::{to_string16, truthy};
use crate::json::{parse, write_str16, Value};

/// supervisor.cjs's `ALLOWED_ENV`.
pub const ALLOWED_ENV: [&str; 11] = [
    "HOME",
    "PATH",
    "LANG",
    "TMPDIR",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "http_proxy",
    "https_proxy",
    "NO_PROXY",
    "CURL_HOME",
    "WGETRC",
];

/// supervisor.cjs's `value.length < 4096` (UTF-16 code units).
pub const MAX_ENV_VALUE_UNITS: usize = 4096;

/// The reply described in the module docs.
pub fn start_json(line: &str) -> String {
    let Some(start) = parse(line) else {
        return "{\"start\":false}".to_owned();
    };
    let is_start = start
        .get("noevia")
        .and_then(Value::as_str16)
        .is_some_and(|s| s == "start".encode_utf16().collect::<Vec<_>>());
    if !is_start {
        return "{\"start\":false}".to_owned();
    }
    let mut out = String::from("{\"start\":true,\"cwd\":");
    match start.get("cwd") {
        Some(v) if truthy(v) => match to_string16(v) {
            Ok(s) => write_str16(&s, &mut out),
            Err(_) => out.push_str("null"),
        },
        _ => out.push_str("\"\""),
    }
    out.push_str(",\"env\":{");
    let mut first = true;
    if let Some(Value::Obj(fields)) = start.get("env") {
        for (k, v) in fields {
            let Value::Str(value) = v else { continue };
            let allowed = ALLOWED_ENV
                .iter()
                .any(|a| a.encode_utf16().eq(k.iter().copied()));
            if !allowed || value.len() >= MAX_ENV_VALUE_UNITS {
                continue;
            }
            if !first {
                out.push(',');
            }
            first = false;
            write_str16(k, &mut out);
            out.push(':');
            write_str16(value, &mut out);
        }
    }
    out.push_str("}}");
    out
}
