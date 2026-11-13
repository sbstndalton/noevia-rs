//! The pure parts of noevia-core's `server/mcp.cjs` (the streamable-http MCP client), in Rust.
//!
//! What is here:
//!
//! - the protocol constants and timeouts (`PROTOCOL_VERSION`, `CLIENT_INFO`, the `rpc` and
//!   `listTools`/`callTool`/`disconnect` defaults, `MAX_RESPONSE_BYTES`);
//! - [`positive_env`]: `positiveEnv`, the reader of the list budgets;
//! - [`built_in_headers`]: `withBuiltInHeaders`, which drops the reserved headers a caller
//!   supplies (so a caller cannot forge the protocol or session headers);
//! - the request bodies: [`initialize_request`], [`initialized_notification`],
//!   [`list_tools_request`], [`call_tool_request`];
//! - [`ListBudget`] and [`next_cursor`]: the page and byte budget of `listTools`;
//! - [`result_to_text`]: `resultToText`, which flattens a tool result for the model.
//!
//! What is NOT here, and stays on the caller's side until the transport is ported: the HTTP
//! round trip, the session id, the reply framing (`mcp-frame` already does that), per-user
//! credentials, the public-only connect for directory servers, error messages that carry server
//! text, and the decision that a session is never cached (per-user, see `mcp.cjs` lines 233-245).
//!
//! [`ToolResult::structured_json`] is the caller's `JSON.stringify` of `structuredContent`. It is
//! taken as text on purpose: `serde_json`'s map sorts keys, and JSON.stringify keeps insertion
//! order, so this crate must not re-serialise a value it parsed.
//!
//! Lengths and trimming follow JavaScript: `String.prototype.trim` over the ECMAScript
//! whitespace set, not `str::trim`.

#![forbid(unsafe_code)]

use serde_json::{json, Value};

/// `PROTOCOL_VERSION` in mcp.cjs.
pub const PROTOCOL_VERSION: &str = "2025-06-18";
/// `CLIENT_INFO` in mcp.cjs.
pub const CLIENT_NAME: &str = "noevia";
pub const CLIENT_VERSION: &str = "1";
/// `MAX_RESPONSE_BYTES`: the most of one reply body that is read.
pub const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
/// The default `timeoutMs` of `rpc` (30 s) and of `connect` (30 s).
pub const RPC_TIMEOUT_MS: u64 = 30_000;
/// The default `timeoutMs` of `listTools` and `callTool` (60 s).
pub const LIST_TIMEOUT_MS: u64 = 60_000;
pub const CALL_TIMEOUT_MS: u64 = 60_000;
/// The default `timeoutMs` of `disconnect` (5 s), and the shorter wait once the caller is gone.
pub const DISCONNECT_TIMEOUT_MS: u64 = 5_000;
pub const ABORTED_DISCONNECT_MS: u64 = 1_000;
/// `MCP_LIST_TOOLS_MAX_PAGES` and `MCP_LIST_TOOLS_MAX_BYTES` defaults.
pub const LIST_MAX_PAGES: usize = 20;
pub const LIST_MAX_BYTES: usize = 16 * 1024 * 1024;

/// `RESERVED_HEADERS`, compared lower-cased.
const RESERVED_HEADERS: [&str; 4] = [
    "content-type",
    "accept",
    "mcp-protocol-version",
    "mcp-session-id",
];

/// `positiveEnv`: the value as a JS `Number` (trimmed; empty is 0), floored when finite and
/// above zero, else `fallback`. `None` is an unset variable (`Number(undefined)` is NaN).
/// Not identical to JS for hex, octal or binary spellings (`0x10`), which Rust does not parse
/// here and so take the fallback; the budgets are set in decimal in practice.
pub fn positive_env(value: Option<&str>, fallback: usize) -> usize {
    let n = match value.map(str::trim) {
        Some("") => 0.0,
        Some(s) => s.parse::<f64>().unwrap_or(f64::NAN),
        None => f64::NAN,
    };
    if n.is_finite() && n > 0.0 {
        // Saturating float-to-int cast: an enormous setting caps at usize::MAX.
        n.floor() as usize
    } else {
        fallback
    }
}

/// `withBuiltInHeaders(extra, builtIn)`: the caller's headers without the reserved names
/// (matched lower-cased), then the built-in ones. `extra` keeps its order.
pub fn built_in_headers(
    extra: &[(String, String)],
    built_in: &[(&str, &str)],
) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = extra
        .iter()
        .filter(|(k, _)| !RESERVED_HEADERS.contains(&k.to_lowercase().as_str()))
        .cloned()
        .collect();
    // `{...out, ...builtIn}`: a built-in name that is already present takes the new value in
    // its place; a new one is appended.
    for (k, v) in built_in {
        match out.iter_mut().find(|(ek, _)| ek == k) {
            Some(slot) => slot.1 = (*v).to_owned(),
            None => out.push(((*k).to_owned(), (*v).to_owned())),
        }
    }
    out
}

/// The `initialize` request (`connect`, first round trip).
pub fn initialize_request(id: u64) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "initialize",
        "params": {
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": { "name": CLIENT_NAME, "version": CLIENT_VERSION },
        },
    })
}

/// The `notifications/initialized` notification (no id; the server answers 202, empty body).
pub fn initialized_notification() -> Value {
    json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })
}

/// One `tools/list` request. The first page has no cursor and empty params.
pub fn list_tools_request(id: u64, cursor: Option<&str>) -> Value {
    let params = match cursor.filter(|c| !c.is_empty()) {
        Some(c) => json!({ "cursor": c }),
        None => json!({}),
    };
    json!({ "jsonrpc": "2.0", "id": id, "method": "tools/list", "params": params })
}

/// One `tools/call` request. `args || {}`: an absent or falsy argument becomes `{}`.
pub fn call_tool_request(id: u64, name: &str, args: Option<Value>) -> Value {
    let arguments = match args {
        Some(v) if js_truthy(&v) => v,
        _ => json!({}),
    };
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": { "name": name, "arguments": arguments },
    })
}

/// JavaScript truthiness of a JSON value: `null`, `false`, `0` and `""` are falsy.
pub fn js_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Value::String(s) => !s.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// The `nextCursor` of a `tools/list` result, when it is a non-empty string. Any other value
/// ends the listing here, where JS would send it back as a cursor; no server in use sends one.
pub fn next_cursor(result: &Value) -> Option<String> {
    result
        .get("nextCursor")
        .and_then(Value::as_str)
        .filter(|c| !c.is_empty())
        .map(str::to_owned)
}

/// Whether a `tools/list` page may be asked for, and how many bytes of tool JSON are kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListBudget {
    max_pages: usize,
    max_bytes: usize,
    pages: usize,
    bytes: usize,
}

/// What to do with one listed tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admit {
    /// The tool is kept.
    Keep,
    /// The tool does not fit the byte budget: it is not kept, and the listing ends with the
    /// tools kept so far (the JS `return all`).
    Stop,
}

impl ListBudget {
    pub fn new(max_pages: usize, max_bytes: usize) -> Self {
        Self {
            max_pages,
            max_bytes,
            pages: 0,
            bytes: 0,
        }
    }

    /// `page >= maxPages` is checked before each request: `true` while another page may be asked.
    pub fn page_allowed(&self) -> bool {
        self.pages < self.max_pages
    }

    /// Call once per page asked for.
    pub fn page_asked(&mut self) {
        self.pages = self.pages.saturating_add(1);
    }

    /// `bytes + size > maxBytes` stops the listing; otherwise the size is counted and the tool
    /// kept. `json_bytes` is the UTF-8 length of the tool's JSON.
    pub fn admit_tool(&mut self, json_bytes: usize) -> Admit {
        let next = self.bytes.saturating_add(json_bytes);
        if next > self.max_bytes {
            return Admit::Stop;
        }
        self.bytes = next;
        Admit::Keep
    }
}

/// One part of a tool result's `content` array, as the caller sorted it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Content {
    /// `{type: 'text', text: <string>}`.
    Text(String),
    /// Any other part with a type: rendered as `[<type> content omitted]`.
    Typed(String),
    /// A part with no usable type (absent, empty, `null`): contributes nothing.
    Untyped,
}

/// The parts of a `tools/call` result that `resultToText` reads.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ToolResult {
    pub content: Vec<Content>,
    /// The JS `JSON.stringify(result.structuredContent)`, present only when the JS value is truthy.
    pub structured_json: Option<String>,
    pub is_error: bool,
}

/// `resultToText(result)`. A failed call is text, not an error: `ERROR from tool: …`.
/// The caller returns `''` for a falsy or non-object result, before building a [`ToolResult`].
pub fn result_to_text(result: &ToolResult) -> String {
    let parts: Vec<String> = result
        .content
        .iter()
        .filter_map(|c| match c {
            Content::Text(s) => Some(s.clone()),
            Content::Typed(t) => Some(format!("[{t} content omitted]")),
            Content::Untyped => None,
        })
        .collect();
    let mut text = js_trim(&parts.join("\n")).to_owned();
    if text.is_empty() {
        if let Some(structured) = &result.structured_json {
            text = structured.clone();
        }
    }
    if result.is_error {
        let detail = if text.is_empty() {
            "(no detail)"
        } else {
            &text
        };
        return format!("ERROR from tool: {detail}");
    }
    text
}

/// `String.prototype.trim` with the ECMAScript whitespace set.
pub fn js_trim(s: &str) -> &str {
    s.trim_matches(is_js_space)
}

fn is_js_space(c: char) -> bool {
    matches!(
        c as u32,
        0x09..=0x0d
            | 0x20
            | 0xa0
            | 0x1680
            | 0x2000..=0x200a
            | 0x2028
            | 0x2029
            | 0x202f
            | 0x205f
            | 0x3000
            | 0xfeff
    )
}
