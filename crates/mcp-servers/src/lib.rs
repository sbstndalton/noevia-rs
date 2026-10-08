//! noevia-core's `server/mcp-servers.cjs` in Rust, exported from `dav-parse.wasm`
//! (MCP_SERVERS_IMPL):
//!
//! - [`parse_servers`]: `parseMcpServers(env)`. `MCP_SERVERS` is a comma-separated list of
//!   `id|url|auth` entries (auth `none`, `nextcloud`, `internal` or `bearer:ENV_NAME`);
//!   `MCP_SERVER_URL` alone means one Nextcloud server. The URL must be http(s) with no embedded
//!   credentials; `internal` needs a loopback IP literal and there is at most one; ids are cut to
//!   40 of `[A-Za-z0-9_-]` and the first valid entry for an id wins. Every entry the JS drops
//!   comes with the JS's warning, word for word.
//! - [`parse_toolboxes`] / [`toolbox_offered`]: `parseEnabledToolboxes` and `createToolboxOffered`.
//!
//! The environment's values never cross: for `bearer:ENV_NAME` the JS only warns when that
//! variable is unset, so the port returns a [`Warning::Bearer`] the host renders after looking the
//! name up itself (the server is configured either way, exactly as in the JS).
//!
//! # Stricter than the JS
//!
//! The JS judges a URL with `new URL` (V8's WHATWG parser); this uses the `url` crate's. Where the
//! two could disagree the port refuses the whole list ([`Refusal::Ambiguous`]; the host then
//! configures no MCP server) instead of guessing:
//!
//! - a URL it has to judge holds anything but printable ASCII (IDNA, whitespace and control
//!   handling differ across parsers), or its authority holds `%` or an `xn--` label;
//! - an entry was dropped for a URL reason (not a URL, not http(s), credentials, not loopback)
//!   and a later entry with the same id would be accepted, or the dropped one was `internal` and
//!   a later `internal` would be accepted: had the parsers disagreed about the first, the JS
//!   would have dropped the second. A config the JS rejects is therefore never accepted here.

#![forbid(unsafe_code)]

use prompt_framing::js::{trim, units};
use prompt_framing::json::{self, Value};
use std::collections::HashSet;
use url::Url;

/// The largest request [`call`] accepts (the op byte and the JSON).
pub const MAX_INPUT_BYTES: usize = 1024 * 1024 + 1;
/// JSON nesting of a request.
const MAX_DEPTH: usize = 4;
/// Ids keep this many characters.
pub const MAX_ID: usize = 40;

/// How a server authenticates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Auth {
    None,
    Nextcloud,
    Internal,
    Bearer,
}

impl Auth {
    pub fn name(self) -> &'static str {
        match self {
            Auth::None => "none",
            Auth::Nextcloud => "nextcloud",
            Auth::Internal => "internal",
            Auth::Bearer => "bearer",
        }
    }
}

/// One configured server, as the JS object `{ id, url, auth[, tokenEnv] }`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Server {
    pub id: Vec<u16>,
    pub url: Vec<u16>,
    pub auth: Auth,
    /// `bearer:ENV_NAME`: the variable's name (`[A-Z0-9_]+`), never its value.
    pub token_env: Option<Vec<u16>>,
}

/// One `console.warn` of the JS, in order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Warning {
    /// The text itself.
    Text(Vec<u16>),
    /// `[mcp] server "<id>": <env> is not set, so its tools will not authenticate`, printed only
    /// when that environment variable is unset or empty (the host checks).
    Bearer { id: Vec<u16>, env: Vec<u16> },
}

/// What [`parse_servers`] returns.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Parsed {
    pub servers: Vec<Server>,
    pub warnings: Vec<Warning>,
}

/// Why the port will not answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    Input,
    TooLarge,
    /// The answer could depend on where two WHATWG URL parsers disagree (see the crate docs).
    Ambiguous,
}

impl Refusal {
    pub fn json(self) -> &'static str {
        match self {
            Refusal::Input => r#"{"error":"input"}"#,
            Refusal::TooLarge => r#"{"error":"too_large"}"#,
            Refusal::Ambiguous => r#"{"error":"ambiguous"}"#,
        }
    }
}

fn cat(parts: &[&[u16]]) -> Vec<u16> {
    parts.iter().flat_map(|p| p.iter().copied()).collect()
}

/// `isLoopbackLiteral(hostname)`: `127.x.y.z` with every part 0-255, `::1` or
/// `0:0:0:0:0:0:0:1`, brackets around it ignored.
pub fn is_loopback_literal(hostname: &[u16]) -> bool {
    // .replace(/^\[|\]$/g, ''): one leading `[` and one trailing `]`, independently.
    let mut h = hostname;
    if let Some(rest) = h.strip_prefix(&[0x5b]) {
        h = rest;
    }
    if let Some(rest) = h.strip_suffix(&[0x5d]) {
        h = rest;
    }
    let parts: Vec<&[u16]> = h.split(|&c| c == 0x2e).collect();
    let digits =
        |p: &[u16]| !p.is_empty() && p.len() <= 3 && p.iter().all(|c| (0x30..=0x39).contains(c));
    if parts.len() == 4
        && parts.first() == Some(&&units("127")[..])
        && parts.iter().all(|p| digits(p))
    {
        return parts
            .iter()
            .all(|p| p.iter().fold(0u32, |n, &c| n * 10 + u32::from(c - 0x30)) <= 255);
    }
    h == units("::1").as_slice() || h == units("0:0:0:0:0:0:0:1").as_slice()
}

/// The URL judgement of `shapeOk` and the hostname `internal` reads.
enum Shape {
    /// `new URL` throws.
    Invalid,
    /// Not http(s), or credentials in it.
    Refused,
    /// Acceptable; the URL's `hostname`.
    Ok(Vec<u16>),
}

fn shape(raw: &[u16]) -> Result<Shape, Refusal> {
    if !raw.iter().all(|c| (0x21..=0x7e).contains(c)) {
        return Err(Refusal::Ambiguous);
    }
    let text: String = raw.iter().map(|&c| char::from(c as u8)).collect();
    // The authority as a special-scheme URL reads it: after `scheme:` and any slashes or
    // backslashes, up to the path, query or fragment.
    if let Some((_, rest)) = text.split_once(':') {
        let rest = rest.trim_start_matches(['/', '\\']);
        let authority = rest.split(['/', '\\', '?', '#']).next().unwrap_or("");
        if authority.contains('%') || authority.to_ascii_lowercase().contains("xn--") {
            return Err(Refusal::Ambiguous);
        }
    }
    let Ok(u) = Url::parse(&text) else {
        return Ok(Shape::Invalid);
    };
    let creds = !u.username().is_empty() || u.password().is_some_and(|p| !p.is_empty());
    if !matches!(u.scheme(), "http" | "https") || creds {
        return Ok(Shape::Refused);
    }
    Ok(Shape::Ok(units(u.host_str().unwrap_or(""))))
}

/// `shapeOk(raw, label)`: the hostname when it passes, else the JS's warning.
fn shape_ok(
    raw: &[u16],
    label: &[u16],
    warnings: &mut Vec<Warning>,
) -> Result<Option<Vec<u16>>, Refusal> {
    match shape(raw)? {
        Shape::Ok(host) => Ok(Some(host)),
        Shape::Refused => {
            warnings.push(Warning::Text(cat(&[
                &units("[mcp] ignoring "),
                label,
                &units(": must be http(s) with no embedded credentials"),
            ])));
            Ok(None)
        }
        Shape::Invalid => {
            warnings.push(Warning::Text(cat(&[
                &units("[mcp] ignoring "),
                label,
                &units(": not a valid URL"),
            ])));
            Ok(None)
        }
    }
}

fn split(s: &[u16], sep: u16) -> impl Iterator<Item = &[u16]> {
    s.split(move |&c| c == sep)
}

/// `parseMcpServers(env)` given `env.MCP_SERVERS` and `env.MCP_SERVER_URL` (`None` for unset).
pub fn parse_servers(list: Option<&[u16]>, single: Option<&[u16]>) -> Result<Parsed, Refusal> {
    let mut out = Parsed::default();
    let list = trim(list.unwrap_or(&[]));
    if !list.is_empty() {
        let mut seen: HashSet<Vec<u16>> = HashSet::new();
        let mut have_internal = false;
        // Ids dropped for a URL reason, and whether an `internal` one was (see the crate docs).
        let mut url_dropped: HashSet<Vec<u16>> = HashSet::new();
        let mut url_dropped_internal = false;
        for entry in split(list, 0x2c).map(trim).filter(|e| !e.is_empty()) {
            let mut fields = split(entry, 0x7c).map(trim);
            let raw_id = fields.next().unwrap_or(&[]);
            let raw_url = fields.next().unwrap_or(&[]);
            let raw_auth = fields.next().unwrap_or(&[]);
            let id: Vec<u16> = raw_id
                .iter()
                .copied()
                .filter(|&c| {
                    c < 0x80
                        && (char::from(c as u8).is_ascii_alphanumeric() || c == 0x5f || c == 0x2d)
                })
                .take(MAX_ID)
                .collect();
            let w = &mut out.warnings;
            if id.is_empty() || raw_url.is_empty() {
                w.push(Warning::Text(cat(&[
                    &units("[mcp] ignoring malformed MCP_SERVERS entry \""),
                    entry,
                    &units("\""),
                ])));
                continue;
            }
            if seen.contains(&id) {
                w.push(Warning::Text(cat(&[
                    &units("[mcp] ignoring duplicate MCP server id \""),
                    &id,
                    &units("\""),
                ])));
                continue;
            }
            let internal = raw_auth == units("internal").as_slice();
            let label = cat(&[&units("MCP server \""), &id, &units("\"")]);
            let Some(host) = shape_ok(raw_url, &label, w)? else {
                url_dropped.insert(id);
                url_dropped_internal |= internal;
                continue;
            };
            let mut auth = Auth::None;
            let mut token_env = None;
            if internal {
                if !is_loopback_literal(&host) {
                    let shown: &[u16] = if host.is_empty() {
                        &units("that host")
                    } else {
                        &host
                    };
                    w.push(Warning::Text(cat(&[
                        &units("[mcp] ignoring internal server \""),
                        &id,
                        &units("\": "),
                        shown,
                        &units(" is not a loopback IP literal. Use 127.0.0.1, not a name."),
                    ])));
                    url_dropped.insert(id);
                    url_dropped_internal = true;
                    continue;
                }
                if have_internal {
                    w.push(Warning::Text(cat(&[
                        &units("[mcp] ignoring internal server \""),
                        &id,
                        &units(
                            "\": there is only one in-process server and it is already configured",
                        ),
                    ])));
                    continue;
                }
                if url_dropped_internal {
                    return Err(Refusal::Ambiguous);
                }
                auth = Auth::Internal;
            } else if raw_auth == units("nextcloud").as_slice() {
                auth = Auth::Nextcloud;
            } else if let Some(rest) = raw_auth.strip_prefix(units("bearer:").as_slice()) {
                let env = trim(rest);
                let named = !env.is_empty()
                    && env.iter().all(|&c| {
                        (0x41..=0x5a).contains(&c) || (0x30..=0x39).contains(&c) || c == 0x5f
                    });
                if named {
                    auth = Auth::Bearer;
                    token_env = Some(env.to_vec());
                    w.push(Warning::Bearer {
                        id: id.clone(),
                        env: env.to_vec(),
                    });
                } else {
                    w.push(Warning::Text(cat(&[
                        &units("[mcp] server \""),
                        &id,
                        &units("\": bearer needs an environment variable name, got \""),
                        env,
                        &units("\" — treating as none"),
                    ])));
                }
            } else if !raw_auth.is_empty() && raw_auth != units("none").as_slice() {
                w.push(Warning::Text(cat(&[
                    &units("[mcp] server \""),
                    &id,
                    &units("\": unknown auth \""),
                    raw_auth,
                    &units("\", treating as none"),
                ])));
            }
            if url_dropped.contains(&id) {
                return Err(Refusal::Ambiguous);
            }
            seen.insert(id.clone());
            have_internal |= auth == Auth::Internal;
            out.servers.push(Server {
                id,
                url: raw_url.to_vec(),
                auth,
                token_env,
            });
        }
        return Ok(out);
    }
    let single = trim(single.unwrap_or(&[]));
    if single.is_empty() {
        return Ok(out);
    }
    if shape_ok(single, &units("MCP_SERVER_URL"), &mut out.warnings)?.is_some() {
        out.servers.push(Server {
            id: units("nextcloud"),
            url: single.to_vec(),
            auth: Auth::Nextcloud,
            token_env: None,
        });
    }
    Ok(out)
}

/// `parseEnabledToolboxes(env)` given `env.ENABLED_TOOLBOXES`: `None` offers everything, else the
/// ids (the JS's `Set`, so first occurrences in order).
pub fn parse_toolboxes(raw: Option<&[u16]>) -> Option<Vec<Vec<u16>>> {
    let raw = trim(raw.unwrap_or(&[]));
    if raw.is_empty() {
        return None;
    }
    let mut ids: Vec<Vec<u16>> = Vec::new();
    let mut seen: HashSet<&[u16]> = HashSet::new();
    for id in split(raw, 0x2c).map(trim).filter(|x| !x.is_empty()) {
        if seen.insert(id) {
            ids.push(id.to_vec());
        }
    }
    (!ids.is_empty()).then_some(ids)
}

/// `createToolboxOffered(enabled)(id)`: core always, `dir-*` always, else as enabled.
pub fn toolbox_offered(enabled: Option<&[Vec<u16>]>, id: &[u16]) -> bool {
    if id == units("core").as_slice() || id.starts_with(&units("dir-")) {
        return true;
    }
    enabled.is_none_or(|ids| ids.iter().any(|x| x.as_slice() == id))
}

fn push(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(s.as_bytes());
}

/// The reply of [`parse_servers`]: `{"servers":[{"id":…,"url":…,"auth":…[,"tokenEnv":…]}],
/// "warnings":["…"|{"bearer":ENV,"id":…}]}`.
pub fn parsed_json(out: &mut Vec<u8>, p: &Parsed) {
    push(out, "{\"servers\":[");
    for (i, s) in p.servers.iter().enumerate() {
        if i > 0 {
            push(out, ",");
        }
        push(out, "{\"id\":");
        json::push_str(out, &s.id);
        push(out, ",\"url\":");
        json::push_str(out, &s.url);
        push(out, ",\"auth\":\"");
        push(out, s.auth.name());
        push(out, "\"");
        if let Some(env) = &s.token_env {
            push(out, ",\"tokenEnv\":");
            json::push_str(out, env);
        }
        push(out, "}");
    }
    push(out, "],\"warnings\":[");
    for (i, w) in p.warnings.iter().enumerate() {
        if i > 0 {
            push(out, ",");
        }
        match w {
            Warning::Text(t) => json::push_str(out, t),
            Warning::Bearer { id, env } => {
                push(out, "{\"bearer\":");
                json::push_str(out, env);
                push(out, ",\"id\":");
                json::push_str(out, id);
                push(out, "}");
            }
        }
    }
    push(out, "]}");
}

/// A string or null (unset).
fn opt_str(v: &Value) -> Result<Option<&[u16]>, Refusal> {
    match v {
        Value::Null => Ok(None),
        Value::Str(s) => Ok(Some(s)),
        _ => Err(Refusal::Input),
    }
}

fn run(op: u8, tree: &Value) -> Result<Vec<u8>, Refusal> {
    let mut out = Vec::new();
    match (op, tree) {
        (1, Value::Arr(xs)) => {
            let [list, single] = xs.as_slice() else {
                return Err(Refusal::Input);
            };
            let p = parse_servers(opt_str(list)?, opt_str(single)?)?;
            parsed_json(&mut out, &p);
        }
        (2, v) => {
            push(&mut out, "{\"enabled\":");
            match parse_toolboxes(opt_str(v)?) {
                None => push(&mut out, "null"),
                Some(ids) => {
                    push(&mut out, "[");
                    for (i, id) in ids.iter().enumerate() {
                        if i > 0 {
                            push(&mut out, ",");
                        }
                        json::push_str(&mut out, id);
                    }
                    push(&mut out, "]");
                }
            }
            push(&mut out, "}");
        }
        (3, Value::Arr(xs)) => {
            let [enabled, Value::Str(id)] = xs.as_slice() else {
                return Err(Refusal::Input);
            };
            let enabled: Option<Vec<Vec<u16>>> = match enabled {
                Value::Null => None,
                Value::Arr(ids) => Some(
                    ids.iter()
                        .map(|x| x.as_str().map(<[u16]>::to_vec).ok_or(Refusal::Input))
                        .collect::<Result<_, _>>()?,
                ),
                _ => return Err(Refusal::Input),
            };
            push(
                &mut out,
                if toolbox_offered(enabled.as_deref(), id) {
                    "{\"offered\":true}"
                } else {
                    "{\"offered\":false}"
                },
            );
        }
        _ => return Err(Refusal::Input),
    }
    Ok(out)
}

/// The wasm call: input `u8(op)` and UTF-8 JSON. Op 1 `[MCP_SERVERS|null, MCP_SERVER_URL|null]`,
/// reply as [`parsed_json`]; op 2 `ENABLED_TOOLBOXES|null`, reply `{"enabled":null|[…]}`; op 3
/// `[null|[id,…], id]`, reply `{"offered":bool}`. Status 1 refuses with
/// `{"error":"input"|"too_large"|"ambiguous"}`.
pub fn call(input: &[u8]) -> (u32, String) {
    if input.len() > MAX_INPUT_BYTES {
        return (1, Refusal::TooLarge.json().to_owned());
    }
    let Some((&op, body)) = input.split_first() else {
        return (1, Refusal::Input.json().to_owned());
    };
    let Some(tree) = json::parse_utf8(body, MAX_DEPTH) else {
        return (1, Refusal::Input.json().to_owned());
    };
    match run(op, &tree).map(String::from_utf8) {
        Ok(Ok(s)) => (0, s),
        Ok(Err(_)) => (1, Refusal::Input.json().to_owned()),
        Err(r) => (1, r.json().to_owned()),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn parse(list: &str) -> Result<Parsed, Refusal> {
        parse_servers(Some(&units(list)), None)
    }

    #[test]
    fn loopback_literals() {
        for h in [
            "127.0.0.1",
            "127.255.255.255",
            "[::1]",
            "::1",
            "0:0:0:0:0:0:0:1",
            "[127.0.0.1]",
        ] {
            assert!(is_loopback_literal(&units(h)), "{h}");
        }
        for h in [
            "localhost",
            "127.0.0.256",
            "127.0.0",
            "128.0.0.1",
            "",
            "[::2]",
            "127.0.0.1.",
            "127.0.0.0001",
        ] {
            assert!(!is_loopback_literal(&units(h)), "{h}");
        }
    }

    #[test]
    fn internal_rules() {
        let p = parse("a|http://127.0.0.1:9/mcp|internal,b|http://127.0.0.2/|internal").unwrap();
        assert_eq!(p.servers.len(), 1);
        assert_eq!(p.servers[0].auth, Auth::Internal);
        let p = parse("a|http://localhost/|internal").unwrap();
        assert!(p.servers.is_empty());
        // WHATWG reads 127.1 as 127.0.0.1.
        assert_eq!(parse("a|http://127.1/|internal").unwrap().servers.len(), 1);
    }

    #[test]
    fn ambiguous_refusals() {
        assert_eq!(parse("a|http://exämple.com/|none"), Err(Refusal::Ambiguous));
        assert_eq!(parse("a|http://xn--e1a.com/|none"), Err(Refusal::Ambiguous));
        assert_eq!(parse("a|http://%41.com/|none"), Err(Refusal::Ambiguous));
        // A dropped entry, then the same id: the JS answer depends on the first verdict.
        assert_eq!(
            parse("a|ftp://x/|none,a|http://y/|none"),
            Err(Refusal::Ambiguous)
        );
        assert_eq!(
            parse("a|http://localhost/|internal,b|http://127.0.0.1/|internal"),
            Err(Refusal::Ambiguous)
        );
        // Percent-encoding in a path is fine.
        assert_eq!(parse("a|http://h/%2F|none").unwrap().servers.len(), 1);
        // A skipped entry (never judged) is fine.
        assert_eq!(parse("a||none,a|http://y/|none").unwrap().servers.len(), 1);
    }

    #[test]
    fn a_megabyte_of_entries_is_hashed() {
        let list: Vec<String> = (0..80_000)
            .map(|i| format!("s{i}|http://h/|none"))
            .collect();
        let p = parse(&list.join(",")).unwrap();
        assert_eq!(p.servers.len(), 80_000);
        let ids: Vec<String> = (0..200_000).map(|i| format!("b{}", i % 1000)).collect();
        assert_eq!(
            parse_toolboxes(Some(&units(&ids.join(",")))).unwrap().len(),
            1000
        );
    }

    #[test]
    fn call_shapes() {
        assert_eq!(call(b""), (1, Refusal::Input.json().to_owned()));
        assert_eq!(
            call(b"\x01[null,null]"),
            (0, r#"{"servers":[],"warnings":[]}"#.to_owned())
        );
        assert_eq!(
            call(b"\x01[null,\" http://h/ \"]"),
            (0, r#"{"servers":[{"id":"nextcloud","url":"http://h/","auth":"nextcloud"}],"warnings":[]}"#.to_owned())
        );
        assert_eq!(
            call(b"\x02\" a, b ,a,,\""),
            (0, r#"{"enabled":["a","b"]}"#.to_owned())
        );
        assert_eq!(call(b"\x02\" , \""), (0, r#"{"enabled":null}"#.to_owned()));
        assert_eq!(
            call(b"\x03[[\"a\"],\"dir-x\"]"),
            (0, r#"{"offered":true}"#.to_owned())
        );
        assert_eq!(
            call(b"\x03[[\"a\"],\"b\"]"),
            (0, r#"{"offered":false}"#.to_owned())
        );
        assert_eq!(
            call(b"\x03[[1],\"b\"]"),
            (1, Refusal::Input.json().to_owned())
        );
        assert_eq!(call(b"\x09null"), (1, Refusal::Input.json().to_owned()));
        let big = vec![b' '; MAX_INPUT_BYTES + 1];
        assert_eq!(call(&big), (1, Refusal::TooLarge.json().to_owned()));
    }
}
