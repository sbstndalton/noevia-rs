//! noevia-core's in-app MCP server (`server/mcp-internal.cjs`) in Rust: the capability tokens, the
//! single-use replay guard for `tools/call`, the static catalogue and the JSON-RPC handler, plus the
//! listener's request rule (POST to one path, a 256 KiB body, the bearer token). [`Handler::handle`]
//! answers a request or hands back an [`Invoke`] for the caller to run, and [`Invoke::finish`] turns the
//! tool's text or error into the reply core sends; [`listener`] is the loopback socket around it
//! (`startInternalServer`). Unused until a front wires it (behind its own switch); the Node module stays
//! the production path.
//!
//! The oracle is core's `server/mcp-internal.test.cjs` (ported in `tests/node_parity.rs`); the token
//! bytes are the same as `mintToken`'s and the front's `mcp_call::mint_call_token`.
//!
//! Named differences from the Node module: none in behaviour. `verifyToken`'s base64url reading follows
//! Node's lenient `Buffer.from(_, 'base64url')` (characters outside the alphabet are skipped, `=` ends
//! the data, both alphabets are read), so a signature Node accepts is accepted here.
use js_json::JValue;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Mutex;

pub mod listener;
pub mod project_tools;

/// `PROTOCOL_VERSION`.
pub const PROTOCOL_VERSION: &str = "2025-06-18";
/// `SERVER_INFO.name` / `.version`.
pub const SERVER_NAME: &str = "noevia-internal";
pub const SERVER_VERSION: &str = "1";
/// `TOKEN_VERSION`.
pub const TOKEN_VERSION: f64 = 1.0;
/// `TOKEN_TTL_MS`.
pub const TOKEN_TTL_MS: f64 = 30_000.0;
/// `MAX_BODY_BYTES` of the listener.
pub const MAX_BODY_BYTES: usize = 256 * 1024;
/// The listener's default path.
pub const RPC_PATH: &str = "/mcp";
/// A token longer than this (UTF-16 units) is malformed.
const MAX_TOKEN_UNITS: usize = 4096;
/// The replay guard sweeps expired entries once it holds more than this many.
const GUARD_SWEEP_AT: usize = 4096;

// ── base64url and HMAC ──

/// `Buffer.from(bytes).toString('base64url')`: no padding.
pub fn b64url(bytes: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk.first().copied().unwrap_or(0),
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        let take = chunk.len() + 1;
        for i in 0..take {
            let idx = ((n >> (18 - 6 * i)) & 63) as usize;
            if let Some(c) = A.get(idx) {
                out.push(char::from(*c));
            }
        }
    }
    out
}

/// `Buffer.from(text, 'base64url')` as Node reads it: both alphabets, anything else skipped, `=` ends
/// the data, a dangling 6 bits dropped. Never fails (Node's never throws either).
pub fn b64url_decode_lenient(text: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in text.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' => break,
            _ => continue,
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((acc >> bits) & 0xff) as u8);
        }
    }
    out
}

/// HMAC-SHA256 (RFC 2104).
pub fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    let mut block = [0u8; 64];
    let digest;
    let key = if key.len() > 64 {
        digest = Sha256::digest(key);
        digest.as_slice()
    } else {
        key
    };
    for (slot, byte) in block.iter_mut().zip(key) {
        *slot = *byte;
    }
    let mut inner = Sha256::new();
    inner.update(block.map(|b| b ^ 0x36));
    inner.update(data);
    let inner = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(block.map(|b| b ^ 0x5c));
    outer.update(inner);
    outer.finalize().into()
}

/// `crypto.timingSafeEqual` after the length check: no early exit on the bytes.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
}

// ── tokens ──

/// What `mintToken(key, { ... })` is given. `jti` and `now` are the caller's (core: `randomUUID()`,
/// `Date.now()`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Mint<'a> {
    pub uid: Option<&'a str>,
    pub pid: Option<&'a str>,
    pub cid: Option<&'a str>,
    pub write: bool,
    /// `t`: the approved edit target (#648), kept only when not empty.
    pub target: Option<&'a str>,
    pub discovery: bool,
    /// `ttlMs`; `None` is [`TOKEN_TTL_MS`].
    pub ttl_ms: Option<f64>,
}

/// `mintToken`: `base64url(JSON payload) "." base64url(HMAC-SHA256(key, that))`, the payload's keys in
/// core's order (`v uid pid cid jti exp w d [t]`). An empty `uid`/`pid`/`cid` is `null`, as `x || null`.
pub fn mint(key: &[u8], m: &Mint<'_>, jti: &str, now_ms: f64) -> String {
    let id = |v: Option<&str>| {
        v.filter(|s| !s.is_empty())
            .map_or(JValue::Null, |s| JValue::Str(s.to_owned()))
    };
    let mut pairs = vec![
        ("v", JValue::Num(TOKEN_VERSION)),
        ("uid", id(m.uid)),
        ("pid", id(m.pid)),
        ("cid", id(m.cid)),
        ("jti", JValue::Str(jti.to_owned())),
        (
            "exp",
            JValue::Num(now_ms + m.ttl_ms.unwrap_or(TOKEN_TTL_MS)),
        ),
        ("w", JValue::Num(if m.write { 1.0 } else { 0.0 })),
        ("d", JValue::Num(if m.discovery { 1.0 } else { 0.0 })),
    ];
    if let Some(t) = m.target.filter(|t| !t.is_empty()) {
        pairs.push(("t", JValue::Str(t.to_owned())));
    }
    let payload = js_json::stringify(&JValue::obj(pairs)).unwrap_or_default();
    let body = b64url(payload.as_bytes());
    let signature = hmac_sha256(key, body.as_bytes());
    format!("{body}.{}", b64url(&signature))
}

/// Why `verifyToken` refused (its `reason`, shown as `unauthorized: <reason>`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    Malformed,
    BadSignature,
    Version,
    Expired,
}

impl Refused {
    pub fn reason(self) -> &'static str {
        match self {
            Refused::Malformed => "malformed",
            Refused::BadSignature => "bad signature",
            Refused::Version => "version",
            Refused::Expired => "expired",
        }
    }
}

/// A verified token's claims: the parsed payload (any other keys kept, as core keeps them).
#[derive(Debug, Clone, PartialEq)]
pub struct Claims(pub JValue);

impl Claims {
    pub fn get(&self, key: &str) -> &JValue {
        self.0.get(key)
    }
    /// `claims.jti` (a non-empty string once verified).
    pub fn jti(&self) -> &str {
        self.get("jti").as_str().unwrap_or("")
    }
    /// `claims.exp` (a number once verified).
    pub fn exp(&self) -> f64 {
        self.get("exp").as_num().unwrap_or(f64::NAN)
    }
}

/// `verifyToken(key, token, now)`: signature, version and expiry; the `jti` is not consumed here.
pub fn verify(key: &[u8], token: &str, now_ms: f64) -> Result<Claims, Refused> {
    if js_json::js_len(token) > MAX_TOKEN_UNITS {
        return Err(Refused::Malformed);
    }
    let Some(dot) = token.find('.') else {
        return Err(Refused::Malformed);
    };
    let (body, rest) = token.split_at(dot);
    let signature = rest.get(1..).unwrap_or("");
    if dot == 0 || signature.contains('.') {
        return Err(Refused::Malformed);
    }
    let given = b64url_decode_lenient(signature);
    if !same(&given, &hmac_sha256(key, body.as_bytes())) {
        return Err(Refused::BadSignature);
    }
    let decoded = b64url_decode_lenient(body);
    let claims =
        js_json::parse(&String::from_utf8_lossy(&decoded)).map_err(|_| Refused::Malformed)?;
    // `!claims || claims.v !== TOKEN_VERSION`
    if !claims.truthy() || claims.get("v").as_num() != Some(TOKEN_VERSION) {
        return Err(Refused::Version);
    }
    match claims.get("exp").as_num() {
        Some(exp) if exp > now_ms => {}
        _ => return Err(Refused::Expired),
    }
    if claims.get("jti").as_str().is_none_or(str::is_empty) {
        return Err(Refused::Malformed);
    }
    Ok(Claims(claims))
}

// ── replay guard ──

/// `createReplayGuard()`: each `jti` may run `tools/call` once (while the guard remembers it).
#[derive(Debug, Default)]
pub struct ReplayGuard {
    used: Mutex<HashMap<String, f64>>,
}

impl ReplayGuard {
    pub fn new() -> ReplayGuard {
        ReplayGuard::default()
    }

    /// `consume(claims)`: `false` when this `jti` was already used. Past 4096 entries the expired ones
    /// (`exp <= now`) are forgotten first.
    pub fn consume(&self, claims: &Claims, now_ms: f64) -> bool {
        let mut used = self.used.lock().unwrap_or_else(|e| e.into_inner());
        if used.len() > GUARD_SWEEP_AT {
            // `exp <= t` deletes; a NaN exp never does.
            used.retain(|_, exp| *exp > now_ms || exp.is_nan());
        }
        let jti = claims.jti();
        if used.contains_key(jti) {
            return false;
        }
        used.insert(jti.to_owned(), claims.exp());
        true
    }

    /// `size`.
    pub fn size(&self) -> usize {
        self.used.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
}

// ── catalogue ──

/// One entry of core's `definitions` (`{ name: { description, schema, write, handler } }`), the handler
/// left to the caller.
#[derive(Debug, Clone, PartialEq)]
pub struct Definition {
    pub name: String,
    /// `undefined` (left out of the catalogue entry) when `None`.
    pub description: Option<String>,
    /// `d.schema || { type: 'object', properties: {}, required: [] }`.
    pub schema: Option<JValue>,
    pub write: bool,
}

/// `catalogueOf(definitions)`: `{ name, description, inputSchema, annotations: { readOnlyHint } }`.
pub fn catalogue_of(definitions: &[Definition]) -> Vec<JValue> {
    definitions
        .iter()
        .map(|d| {
            let schema = d
                .schema
                .clone()
                .filter(JValue::truthy)
                .unwrap_or_else(default_schema);
            let mut pairs = vec![("name", JValue::Str(d.name.clone()))];
            if let Some(text) = &d.description {
                pairs.push(("description", JValue::Str(text.clone())));
            }
            pairs.push(("inputSchema", schema));
            pairs.push((
                "annotations",
                JValue::obj([("readOnlyHint", JValue::Bool(!d.write))]),
            ));
            JValue::obj(pairs)
        })
        .collect()
}

fn default_schema() -> JValue {
    JValue::obj([
        ("type", JValue::from("object")),
        ("properties", JValue::Obj(vec![])),
        ("required", JValue::Arr(vec![])),
    ])
}

/// `textResult(text, isError)`.
pub fn text_result(text: &str, is_error: bool) -> JValue {
    let mut pairs = vec![(
        "content",
        JValue::Arr(vec![JValue::obj([
            ("type", JValue::from("text")),
            ("text", JValue::Str(text.to_owned())),
        ])]),
    )];
    if is_error {
        pairs.push(("isError", JValue::Bool(true)));
    }
    JValue::obj(pairs)
}

fn rpc_error(id: &JValue, code: f64, message: &str) -> JValue {
    JValue::obj([
        ("jsonrpc", JValue::from("2.0")),
        ("id", nullish_to_null(id)),
        (
            "error",
            JValue::obj([
                ("code", JValue::Num(code)),
                ("message", JValue::from(message)),
            ]),
        ),
    ])
}

fn rpc_ok(id: &JValue, result: JValue) -> JValue {
    JValue::obj([
        ("jsonrpc", JValue::from("2.0")),
        ("id", nullish_to_null(id)),
        ("result", result),
    ])
}

/// `id ?? null`.
fn nullish_to_null(id: &JValue) -> JValue {
    if id.is_nullish() {
        JValue::Null
    } else {
        id.clone()
    }
}

// ── handler ──

/// One answer: an HTTP status and the JSON-RPC body (`None`: an empty body, the 202).
#[derive(Debug, Clone, PartialEq)]
pub struct Reply {
    pub status: u16,
    pub body: Option<JValue>,
}

impl Reply {
    /// The listener's payload: `''` for no body, else `JSON.stringify(body)`.
    pub fn payload(&self) -> String {
        self.body
            .as_ref()
            .and_then(js_json::stringify)
            .unwrap_or_default()
    }
}

/// The `ctx` a tool handler is given: identity from the verified token only.
#[derive(Debug, Clone, PartialEq)]
pub struct Ctx {
    /// `claims.uid` (truthy once a call gets here).
    pub user_id: JValue,
    pub project_id: JValue,
    pub chat_id: JValue,
    /// `editTarget`, only when the token's `t` is a non-empty string.
    pub edit_target: Option<String>,
}

/// A verified, approved, first-use `tools/call` for the caller to run (core: `runAs(uid, handler)`).
#[derive(Debug, Clone, PartialEq)]
pub struct Invoke {
    pub id: JValue,
    pub name: String,
    /// `params.arguments` when a non-array object, else `{}`.
    pub args: JValue,
    pub ctx: Ctx,
}

impl Invoke {
    /// The reply for the tool's answer: its text (`None` is core's `null`/`undefined`, an empty text), or
    /// the message it failed with (`runAs` or the handler threw), cut to 300 units after `ERROR: `.
    pub fn finish(&self, answered: Result<Option<String>, String>) -> Reply {
        let result = match answered {
            Ok(text) => text_result(text.as_deref().unwrap_or(""), false),
            // `(err && err.message) || err`: an Error with an empty message reads as `String(err)`.
            Err(message) => text_result(
                &format!(
                    "ERROR: {}",
                    js_json::js_slice(
                        if message.is_empty() {
                            "Error"
                        } else {
                            &message
                        },
                        300
                    )
                ),
                true,
            ),
        };
        Reply {
            status: 200,
            body: Some(rpc_ok(&self.id, result)),
        }
    }
}

/// What [`Handler::handle`] decided.
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    Reply(Reply),
    Invoke(Invoke),
}

fn reply(status: u16, body: JValue) -> Step {
    Step::Reply(Reply {
        status,
        body: Some(body),
    })
}

/// `createHandler({ key, definitions, guard })`.
#[derive(Debug)]
pub struct Handler {
    key: Vec<u8>,
    definitions: Vec<Definition>,
    catalogue: Vec<JValue>,
    guard: ReplayGuard,
}

impl Handler {
    pub fn new(key: &[u8], definitions: Vec<Definition>) -> Handler {
        let catalogue = catalogue_of(&definitions);
        Handler {
            key: key.to_vec(),
            definitions,
            catalogue,
            guard: ReplayGuard::new(),
        }
    }

    /// The replay guard (for its size).
    pub fn guard(&self) -> &ReplayGuard {
        &self.guard
    }

    /// `handle(message, token)` at `now_ms`, up to the tool call itself.
    pub fn handle(&self, message: &JValue, token: &str, now_ms: f64) -> Step {
        // `message && message.id`: a falsy message is its own id.
        let (id, method) = if message.truthy() {
            (message.get("id").clone(), message.get("method").clone())
        } else {
            (message.clone(), message.clone())
        };
        let claims = match verify(&self.key, token, now_ms) {
            Ok(c) => c,
            Err(why) => {
                return reply(
                    401,
                    rpc_error(&id, -32001.0, &format!("unauthorized: {}", why.reason())),
                )
            }
        };
        match method.as_str() {
            Some("initialize") => {
                return reply(
                    200,
                    rpc_ok(
                        &id,
                        JValue::obj([
                            ("protocolVersion", JValue::from(PROTOCOL_VERSION)),
                            (
                                "capabilities",
                                JValue::obj([("tools", JValue::Obj(vec![]))]),
                            ),
                            (
                                "serverInfo",
                                JValue::obj([
                                    ("name", JValue::from(SERVER_NAME)),
                                    ("version", JValue::from(SERVER_VERSION)),
                                ]),
                            ),
                        ]),
                    ),
                )
            }
            Some("notifications/initialized") => {
                return Step::Reply(Reply {
                    status: 202,
                    body: None,
                })
            }
            Some("tools/list") => {
                return reply(
                    200,
                    rpc_ok(
                        &id,
                        JValue::obj([("tools", JValue::Arr(self.catalogue.clone()))]),
                    ),
                )
            }
            Some("tools/call") => {}
            _ => {
                // `method not found: ${method}`: String(method), which a JSON value can only fail for an
                // object with an own toString (a TypeError core's listener answers 400 for).
                let Ok(shown) = js_json::to_js_string(&method) else {
                    return invalid_request();
                };
                return reply(
                    200,
                    rpc_error(&id, -32601.0, &format!("method not found: {shown}")),
                );
            }
        }
        if claims.get("d").truthy() {
            return reply(
                403,
                rpc_error(&id, -32002.0, "this token may list tools but not call them"),
            );
        }
        if !claims.get("uid").truthy() {
            return reply(
                403,
                rpc_error(&id, -32002.0, "no user is in scope for this call"),
            );
        }
        let params = message.get("params");
        let empty = JValue::Obj(vec![]);
        let params = if params.truthy() { params } else { &empty };
        let name = params.get("name").as_str().unwrap_or("").to_owned();
        let Some(definition) = self.definitions.iter().find(|d| d.name == name) else {
            return reply(
                200,
                rpc_ok(
                    &id,
                    text_result(&format!("ERROR: unknown tool \"{name}\""), true),
                ),
            );
        };
        if definition.write && !claims.get("w").truthy() {
            return reply(
                403,
                rpc_error(
                    &id,
                    -32003.0,
                    &format!("\"{name}\" changes data and was not approved"),
                ),
            );
        }
        if !self.guard.consume(&claims, now_ms) {
            return reply(
                401,
                rpc_error(&id, -32004.0, "this authorization has already been used"),
            );
        }
        let args = match params.get("arguments") {
            a @ JValue::Obj(_) => a.clone(),
            _ => JValue::Obj(vec![]),
        };
        let edit_target = claims
            .get("t")
            .as_str()
            .filter(|t| !t.is_empty())
            .map(str::to_owned);
        Step::Invoke(Invoke {
            id,
            name,
            args,
            ctx: Ctx {
                user_id: claims.get("uid").clone(),
                project_id: claims.get("pid").clone(),
                chat_id: claims.get("cid").clone(),
                edit_target,
            },
        })
    }

    /// The listener (`startInternalServer`'s request handler) up to the tool call: `POST <path>` only
    /// (the query ignored), the body read as UTF-8 (`None`: it passed [`MAX_BODY_BYTES`] or failed, core's
    /// 400 `invalid request`), JSON-parsed (else 400 `parse error`), the token after `Bearer `.
    pub fn listen(
        &self,
        method: &str,
        url: &str,
        path: &str,
        body: Option<&[u8]>,
        authorization: Option<&str>,
        now_ms: f64,
    ) -> Step {
        let target = url.split('?').next().unwrap_or("");
        if method != "POST" || target != path {
            return reply(404, JValue::obj([("error", JValue::from("not found"))]));
        }
        let Some(raw) = body.filter(|b| b.len() <= MAX_BODY_BYTES) else {
            return invalid_request();
        };
        let Ok(message) = js_json::parse(&String::from_utf8_lossy(raw)) else {
            return reply(400, rpc_error(&JValue::Null, -32700.0, "parse error"));
        };
        let token = authorization
            .and_then(|a| a.strip_prefix("Bearer "))
            .unwrap_or("");
        self.handle(&message, token, now_ms)
    }
}

fn invalid_request() -> Step {
    reply(400, rpc_error(&JValue::Null, -32600.0, "invalid request"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn base64url_round_trips_and_reads_leniently() {
        for len in 0..40usize {
            let bytes: Vec<u8> = (0..len).map(|i| (i * 37 + 11) as u8).collect();
            let text = b64url(&bytes);
            assert!(!text.contains('='));
            assert_eq!(b64url_decode_lenient(&text), bytes);
            // The standard alphabet and padding read the same (Node reads both).
            let std = text.replace('-', "+").replace('_', "/");
            assert_eq!(b64url_decode_lenient(&format!("{std}==")), bytes);
            // Characters outside the alphabet are skipped.
            assert_eq!(b64url_decode_lenient(&format!(" {text}\n")), bytes);
        }
        assert_eq!(b64url(b"\xff\xfe"), "__4");
        assert_eq!(b64url_decode_lenient("a"), Vec::<u8>::new());
    }

    #[test]
    fn hmac_matches_rfc4231_case_2() {
        let mac = hmac_sha256(b"Jefe", b"what do ya want for nothing?");
        let hex: String = mac.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex,
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn a_falsy_message_is_its_own_id() {
        let h = Handler::new(b"k", vec![]);
        let Step::Reply(r) = h.handle(&JValue::Num(0.0), "x", 0.0) else {
            panic!()
        };
        assert_eq!(r.status, 401);
        assert_eq!(
            r.payload(),
            r#"{"jsonrpc":"2.0","id":0,"error":{"code":-32001,"message":"unauthorized: malformed"}}"#
        );
    }
}
