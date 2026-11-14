//! noevia-core `server/kiwix.cjs` (`createKiwixTools`, D9) in Rust: the read-only "Offline Wikipedia"
//! built-in toolbox, `wikipedia_search` and `wikipedia_read`, over an internal kiwix-serve. Behaviour, not
//! lines: `tests/fixtures.rs` replays `tests/fixtures/kiwix-node.v1.json`, written by core's own module
//! (`tools/gen-fixtures.cjs`), and compares every answer and every requested URL.
//!
//! - [`ID`], [`NAMES`] and [`box_json`]: the toolbox entry (`box`), `JSON.stringify` of Node's object.
//! - [`base`]: the constructor's `KIWIX_URL` check and the origin the tools fetch under
//!   (`origin + pathname` without trailing slashes).
//! - [`execute`]: the exported `execute` wrapper, a thrown error included (`ERROR: ` and its message cut
//!   to 200 UTF-16 units); `None` for a name that is not one of the two tools.
//!
//! # The host's HTTP contract
//!
//! The crate does no I/O. [`execute`] calls `get` with the full URL Node's `get` fetches
//! (`origin + pathAndQuery`, exactly as built; nothing is normalised, so a `..` segment reaches the
//! client as it is). The host must do what core's `get` does around `fetchImpl`:
//!
//! - fetch with `redirect: 'error'` (a redirect is a failure) and a [`TIMEOUT_MS`] timeout;
//! - on a status outside 200-299 fail with `offline Wikipedia answered <status>` (see [`status_error`]);
//! - otherwise read at most [`BODY_CAP_BYTES`] bytes of the body as UTF-8 (`readCappedText`): invalid
//!   sequences become U+FFFD, and a body over the cap is cut there (one U+FFFD at the very end of a cut
//!   body is dropped) without any error: kiwix.cjs ignores `capped`;
//! - a fetch that fails answers its error's message (`fetch failed`, the abort reason's message, ...).
//!   An empty message stands for `String(error)` of an error without one, which this port writes as
//!   `Error`.
//!
//! # Strings
//!
//! JavaScript strings are UTF-16 and the module cuts them by code units (`slice(0, 200)`,
//! `slice(0, 240)`, `slice(0, cap)`, `slice(offset, offset + cap)`), so an answer can hold a lone
//! surrogate. Answers use [`js_json::wtf16`]'s mapping (a lone surrogate unit is the character
//! `U+10F800 + (unit - 0xD800)`), and so do the strings in `args` (parse them with
//! [`js_json::wtf16::parse`] to keep lone surrogates; a plain [`js_json::parse`] refuses them). The
//! bodies and error messages `get` returns are ordinary text.
//!
//! The article text goes through [`research_core::sources::strip_boilerplate`] (core
//! `strip-boilerplate.cjs`) and is framed with [`prompt_framing::framing::frame_untrusted`] (what
//! `prompt-framing.cjs` `frameUntrusted` runs in dav-parse.wasm). The module's own refusal of a text
//! over 8 Mi units cannot happen here: a body is at most [`BODY_CAP_BYTES`] bytes.
use js_json::{number_to_string, to_js_string, wtf16, JValue};
use prompt_framing::framing::frame_untrusted;
use prompt_framing::js::{decode_uri_component, is_js_space, trim, units};
use research_core::sources::strip_boilerplate;

/// The toolbox id (`ID`).
pub const ID: &str = "offline-wikipedia";
/// The two tool names, in schema order (`names`).
pub const NAMES: [&str; 2] = ["wikipedia_search", "wikipedia_read"];
/// The constructor's default `cap` (UTF-16 units of an answer's text).
pub const DEFAULT_CAP: usize = 8000;
/// The constructor's default `timeoutMs`, for the host's fetch.
pub const TIMEOUT_MS: u64 = 10_000;
/// The body cap `get` reads with (`readCappedText(r, 2_000_000)`), for the host.
pub const BODY_CAP_BYTES: usize = 2_000_000;

const BOX_JSON: &str = concat!(
    r#"{"id":"offline-wikipedia","label":"Offline Wikipedia","description":"Search and read the offline Wikipedia copy on this server. Read-only.","source":"builtin","#,
    r#""tools":[{"type":"function","function":{"name":"wikipedia_search","description":"Search the offline Wikipedia. Returns titles with a path for wikipedia_read.","#,
    r#""parameters":{"type":"object","properties":{"query":{"type":"string"}},"required":["query"]}}},"#,
    r#"{"type":"function","function":{"name":"wikipedia_read","description":"Read one offline Wikipedia article by the path wikipedia_search returned. Use offset to continue.","#,
    r#""parameters":{"type":"object","properties":{"path":{"type":"string"},"offset":{"type":"integer","minimum":0}},"required":["path"]}}}],"#,
    r#""reads":["wikipedia_search","wikipedia_read"]}"#
);

/// `JSON.stringify(box)`: the toolbox entry with both tool schemas, as Node holds it.
pub fn box_json() -> &'static str {
    BOX_JSON
}

const NOT_PLAIN: &str = "KIWIX_URL must be a plain http(s) origin";

/// A checked `KIWIX_URL`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Base {
    origin: String,
}

impl Base {
    /// `base.origin + base.pathname.replace(/\/+$/, '')`: what every request URL starts with.
    pub fn origin(&self) -> &str {
        &self.origin
    }
}

/// The constructor's check of `baseUrl`: `new URL(raw)` (`Invalid URL` when it does not parse), then
/// `KIWIX_URL must be a plain http(s) origin` unless the scheme is http or https with no user name,
/// password or (non-empty) query. A fragment is dropped; the path is kept without trailing slashes.
pub fn base(raw: &str) -> Result<Base, String> {
    let url = url::Url::parse(raw).map_err(|_| "Invalid URL".to_owned())?;
    let plain = matches!(url.scheme(), "http" | "https")
        && url.username().is_empty()
        && url.password().is_none_or(str::is_empty)
        && url.query().is_none_or(str::is_empty);
    if !plain {
        return Err(NOT_PLAIN.to_owned());
    }
    let origin = url.origin().ascii_serialization();
    Ok(Base {
        origin: format!("{origin}{}", url.path().trim_end_matches('/')),
    })
}

/// The message core's `get` throws for a response that is not OK.
pub fn status_error(status: u16) -> String {
    format!("offline Wikipedia answered {status}")
}

/// The exported `execute(name, args)` of `createKiwixTools({ baseUrl, cap })`, `get` standing for
/// the fetch (see the crate documentation). `None` is Node's `undefined`: not one of the two tools.
pub fn execute(
    base: &Base,
    name: &str,
    args: &JValue,
    cap: usize,
    get: &mut dyn FnMut(&str) -> Result<String, String>,
) -> Option<String> {
    let answer = match name {
        "wikipedia_search" => search(base, args, cap, get),
        "wikipedia_read" => read(base, args, cap, get),
        _ => return None,
    };
    Some(match answer {
        Ok(text) => js_string(&text),
        Err(message) => {
            // `ERROR: ${String(e.message || e).slice(0, 200)}`
            let message = if message.is_empty() {
                units("Error")
            } else {
                message
            };
            let mut out = units("ERROR: ");
            out.extend(message.iter().take(200));
            js_string(&out)
        }
    })
}

/// A thrown error's message, as UTF-16 units.
type Thrown = Vec<u16>;

fn search(
    base: &Base,
    args: &JValue,
    cap: usize,
    get: &mut dyn FnMut(&str) -> Result<String, String>,
) -> Result<Vec<u16>, Thrown> {
    // String(args.query || '').trim().slice(0, 200)
    let raw = string_or_empty(property(args, "query")?)?;
    let query = prefix(trim(&raw), 200);
    if query.is_empty() {
        return Ok(units("ERROR: a query is required"));
    }
    let pattern = encode_uri_component(query)?;
    let xml = get(&format!(
        "{}/search?pattern={pattern}&format=xml&pageLength=6",
        base.origin
    ))
    .map_err(|e| units(&e))?;
    let mut lines: Vec<Vec<u16>> = Vec::new();
    for body in items(&xml) {
        let title = tag(body, "title");
        let link = tag(body, "link");
        let path = without_host(&link);
        if title.is_empty() || !path.starts_with("/content/") {
            continue;
        }
        // stripBoilerplate(description).replace(/\s+/g, ' ').slice(0, 240)
        let snippet = collapse_spaces(&units(&strip_boilerplate(&tag(body, "description"))));
        let mut line = units(&format!(
            "{}. {title}\n   path: {path}\n   ",
            lines.len() + 1
        ));
        line.extend_from_slice(prefix(&snippet, 240));
        lines.push(line);
    }
    if lines.is_empty() {
        let mut out = units("Offline Wikipedia has nothing for \"");
        out.extend_from_slice(query);
        out.extend(units("\"."));
        return Ok(out);
    }
    let joined = lines.join(&0x0a);
    Ok(prefix(&joined, cap).to_vec())
}

fn read(
    base: &Base,
    args: &JValue,
    cap: usize,
    get: &mut dyn FnMut(&str) -> Result<String, String>,
) -> Result<Vec<u16>, Thrown> {
    let raw = string_or_empty(property(args, "path")?)?;
    let raw = trim(&raw);
    if !article_path(raw) {
        return Ok(units("ERROR: use a path returned by wikipedia_search"));
    }
    // Math.max(0, Number.parseInt(args.offset ?? 0, 10) || 0)
    let offset = property(args, "offset")?;
    let offset = if offset.is_nullish() {
        0.0
    } else {
        parse_int(&string_of(offset)?)
    };
    let mut path = String::new();
    for (i, segment) in raw.split(|&c| c == 0x2f).enumerate() {
        if i > 0 {
            path.push('/');
        }
        if i < 2 {
            path.push_str(&String::from_utf16_lossy(segment));
        } else {
            let decoded = decode_uri_component(segment).unwrap_or_else(|| segment.to_vec());
            path.push_str(&encode_uri_component(&decoded)?);
        }
    }
    let body = get(&format!("{}{path}", base.origin)).map_err(|e| units(&e))?;
    let text = units(&strip_boilerplate(&body));
    let len = text.len() as f64;
    let end = offset + cap as f64;
    let from = if offset >= len {
        text.len()
    } else {
        offset as usize
    };
    let to = if end >= len { text.len() } else { end as usize };
    let mut framed = text.get(from..to.max(from)).unwrap_or(&[]).to_vec();
    if end < len {
        framed.extend(units(&format!(
            "\n\u{2026}[continues; call again with offset {}]",
            number_to_string(end)
        )));
    }
    Ok(frame_untrusted(
        &units("Offline Wikipedia article"),
        &[],
        &framed,
    ))
}

/// `/^\/content\/[^?#\s]+$/.test(raw)`, no `..` or `.` segment, no `//`, at most 400 units.
fn article_path(raw: &[u16]) -> bool {
    let Some(rest) = raw.strip_prefix(units("/content/").as_slice()) else {
        return false;
    };
    !rest.is_empty()
        && rest
            .iter()
            .all(|&c| c != 0x3f && c != 0x23 && !is_js_space(c))
        && !raw
            .split(|&c| c == 0x2f)
            .any(|s| s == [0x2e, 0x2e] || s == [0x2e])
        && !raw.windows(2).any(|w| w == [0x2f, 0x2f])
        && raw.len() <= 400
}

/// `args[key]`, throwing as V8 does for `null` and `undefined`.
fn property<'a>(args: &'a JValue, key: &str) -> Result<&'a JValue, Thrown> {
    match args {
        JValue::Null => Err(units(&format!(
            "Cannot read properties of null (reading '{key}')"
        ))),
        JValue::Undefined => Err(units(&format!(
            "Cannot read properties of undefined (reading '{key}')"
        ))),
        other => Ok(other.get(key)),
    }
}

/// `String(v)`. V8's TypeError for an object with an own `toString` (a JSON value is never callable);
/// arrays nested over a thousand deep are refused with the same message here.
fn string_of(v: &JValue) -> Result<Vec<u16>, Thrown> {
    to_js_string(v)
        .map(|s| js_units(&s))
        .map_err(|_| units("Cannot convert object to primitive value"))
}

/// `String(v || '')`.
fn string_or_empty(v: &JValue) -> Result<Vec<u16>, Thrown> {
    if v.truthy() {
        string_of(v)
    } else {
        Ok(Vec::new())
    }
}

/// `s.slice(0, n)`.
fn prefix(s: &[u16], n: usize) -> &[u16] {
    s.get(..n.min(s.len())).unwrap_or(s)
}

/// `s.replace(/\s+/g, ' ')`.
fn collapse_spaces(s: &[u16]) -> Vec<u16> {
    let mut out = Vec::with_capacity(s.len());
    let mut in_space = false;
    for &c in s {
        if is_js_space(c) {
            if !in_space {
                out.push(0x20);
            }
            in_space = true;
        } else {
            out.push(c);
            in_space = false;
        }
    }
    out
}

/// `Number.parseInt(s, 10)` then `|| 0` and `Math.max(0, ...)`.
fn parse_int(s: &[u16]) -> f64 {
    let start = s.iter().position(|&c| !is_js_space(c)).unwrap_or(s.len());
    let mut rest = s.get(start..).unwrap_or(&[]);
    let mut negative = false;
    if let Some((&sign, tail)) = rest.split_first() {
        if sign == 0x2b || sign == 0x2d {
            negative = sign == 0x2d;
            rest = tail;
        }
    }
    let digits: String = rest
        .iter()
        .take_while(|&&c| (0x30..=0x39).contains(&c))
        .map(|&c| char::from(c as u8))
        .collect();
    let value = digits.parse::<f64>().unwrap_or(0.0);
    if negative || value.is_nan() {
        0.0
    } else {
        value
    }
}

/// `encodeURIComponent(s)`; a lone surrogate throws `URI malformed`.
fn encode_uri_component(s: &[u16]) -> Result<String, Thrown> {
    const KEEP: &[u8] = b"-_.!~*'()";
    let mut out = String::with_capacity(s.len());
    for c in char::decode_utf16(s.iter().copied()) {
        let c = c.map_err(|_| units("URI malformed"))?;
        if c.is_ascii_alphanumeric() || (c.is_ascii() && KEEP.contains(&(c as u8))) {
            out.push(c);
        } else {
            let mut buf = [0u8; 4];
            for b in c.encode_utf8(&mut buf).bytes() {
                out.push_str(&format!("%{b:02X}"));
            }
        }
    }
    Ok(out)
}

/// The bodies of `xml.matchAll(/<item>([\s\S]*?)<\/item>/g)`.
fn items(xml: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut pos = 0;
    while let Some(open) = xml.get(pos..).and_then(|r| r.find("<item>")) {
        let from = pos + open + "<item>".len();
        let Some(close) = xml.get(from..).and_then(|r| r.find("</item>")) else {
            break;
        };
        out.push(xml.get(from..from + close).unwrap_or(""));
        pos = from + close + "</item>".len();
    }
    out
}

/// `tag(xml, name)`: the first `<name[^>]*>([\s\S]*?)</name>`, decoded and trimmed; '' if none.
fn tag(xml: &str, name: &str) -> String {
    let open = format!("<{name}");
    let close = format!("</{name}>");
    let found = (|| {
        let at = xml.find(&open)? + open.len();
        let body = at + xml.get(at..)?.find('>')? + 1;
        let end = body + xml.get(body..)?.find(&close)?;
        xml.get(body..end)
    })();
    found.map_or_else(String::new, |s| js_trim(&decode(s)).to_owned())
}

/// `decode`: CDATA sections unwrapped, then `&lt;`, `&gt;`, `&quot;`, `&#39;`/`&apos;` and `&amp;`.
fn decode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut pos = 0;
    while let Some(open) = s.get(pos..).and_then(|r| r.find("<![CDATA[")) {
        let from = pos + open + "<![CDATA[".len();
        let Some(close) = s.get(from..).and_then(|r| r.find("]]>")) else {
            break;
        };
        out.push_str(s.get(pos..pos + open).unwrap_or(""));
        out.push_str(s.get(from..from + close).unwrap_or(""));
        pos = from + close + "]]>".len();
    }
    out.push_str(s.get(pos..).unwrap_or(""));
    out.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// `link.replace(/^https?:\/\/[^/]+/, '')`.
fn without_host(link: &str) -> &str {
    let rest = link
        .strip_prefix("https://")
        .or_else(|| link.strip_prefix("http://"));
    match rest {
        Some(r) if !r.is_empty() && !r.starts_with('/') => {
            r.find('/').and_then(|i| r.get(i..)).unwrap_or("")
        }
        _ => link,
    }
}

/// `String.prototype.trim` on text without lone surrogates.
fn js_trim(s: &str) -> &str {
    s.trim_matches(|c: char| u16::try_from(u32::from(c)).is_ok_and(is_js_space))
}

/// A string in [`wtf16`]'s mapping as UTF-16 units.
fn js_units(s: &str) -> Vec<u16> {
    let mut out = Vec::with_capacity(s.len());
    for c in s.chars() {
        match wtf16::lone_unit(c) {
            Some(u) => out.push(u),
            None => {
                let mut buf = [0u16; 2];
                out.extend_from_slice(c.encode_utf16(&mut buf));
            }
        }
    }
    out
}

/// UTF-16 units as a string in [`wtf16`]'s mapping.
fn js_string(s: &[u16]) -> String {
    char::decode_utf16(s.iter().copied())
        .map(|r| {
            r.unwrap_or_else(|e| {
                wtf16::lone_char(e.unpaired_surrogate()).unwrap_or(char::REPLACEMENT_CHARACTER)
            })
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn the_box_is_json() {
        assert!(js_json::parse(box_json()).is_ok());
    }

    #[test]
    fn helpers() {
        assert_eq!(
            encode_uri_component(&units("a b&é😀")).unwrap(),
            "a%20b%26%C3%A9%F0%9F%98%80"
        );
        assert!(encode_uri_component(&[0x61, 0xd800]).is_err());
        assert_eq!(parse_int(&units(" \u{a0}-0012x")), 0.0);
        assert_eq!(parse_int(&units("\u{2028}+12.5")), 12.0);
        assert_eq!(without_host("http://h"), "");
        assert_eq!(without_host("http:///content/x"), "http:///content/x");
        assert_eq!(
            tag(
                "<a>x</a><title x=1>&amp;lt; <![CDATA[ y ]]></title>",
                "title"
            ),
            "&lt;  y"
        );
    }
}
