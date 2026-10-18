//! What httpx does with a response, as far as the search routes see it: reading a body under a
//! bound, `Response.text`, the text of a transport error, and the URL it builds.
use super::py::{self, Unsupported};
use futures_util::StreamExt;

/// Why a body could not be read.
#[derive(Debug)]
pub enum BodyError {
    /// The transfer failed: an `httpx.HTTPError`, with its message.
    Transport(String),
    /// More than the bound: this side does not read it.
    Cap,
}

/// The whole body of `response`, held to `cap` bytes. The buffer grows by doubling but never
/// beyond `cap` (noevia#1985).
pub async fn read_body(response: reqwest::Response, cap: usize) -> Result<Vec<u8>, BodyError> {
    let expected = response.content_length();
    let mut stream = response.bytes_stream();
    let mut body: Vec<u8> = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(error) => {
                return Err(BodyError::Transport(describe_body(
                    &error,
                    body.len(),
                    expected,
                )))
            }
        };
        let len = body.len().checked_add(chunk.len()).ok_or(BodyError::Cap)?;
        if len > cap {
            return Err(BodyError::Cap);
        }
        if body.capacity() < len {
            let grown = body.capacity().saturating_mul(2).max(len).min(cap);
            body.try_reserve_exact(grown - body.len())
                .map_err(|_| BodyError::Cap)?;
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// The first `cap` bytes of a body (a response whose rest is not read), or `None` on a
/// transport error.
pub async fn read_prefix(response: reqwest::Response, cap: usize) -> Result<Vec<u8>, BodyError> {
    let expected = response.content_length();
    let mut stream = response.bytes_stream();
    let mut body: Vec<u8> = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(error) => {
                return Err(BodyError::Transport(describe_body(
                    &error,
                    body.len(),
                    expected,
                )))
            }
        };
        let room = cap.saturating_sub(body.len());
        body.extend_from_slice(chunk.get(..room.min(chunk.len())).unwrap_or_default());
        if body.len() >= cap {
            break;
        }
    }
    Ok(body)
}

/// The message of the `httpx.HTTPError` a failed request raised. Timeouts have none; a refused
/// connection is anyio's "All connection attempts failed"; a lookup failure is the resolver's.
/// Messages that h11 builds from the bytes it saw are not reproduced: those become the message
/// of a connection closed without an answer.
pub fn describe(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        return String::new();
    }
    if error.is_redirect() {
        return "Exceeded maximum allowed redirects.".to_owned();
    }
    let chain = chain_text(error);
    if error.is_connect() {
        if chain.contains("Name or service not known") || chain.contains("failed to lookup") {
            return "[Errno -2] Name or service not known".to_owned();
        }
        if chain.contains("Temporary failure in name resolution") {
            return "[Errno -3] Temporary failure in name resolution".to_owned();
        }
        return "All connection attempts failed".to_owned();
    }
    "Server disconnected without sending a response.".to_owned()
}

fn describe_body(error: &reqwest::Error, received: usize, expected: Option<u64>) -> String {
    if error.is_timeout() {
        return String::new();
    }
    match expected {
        Some(expected) => format!(
            "peer closed connection without sending complete message body (received {received} bytes, expected {expected})"
        ),
        None => "peer closed connection without sending complete message body (incomplete chunked read)"
            .to_owned(),
    }
}

fn chain_text(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(next) = source {
        text.push_str(": ");
        text.push_str(&next.to_string());
        source = next.source();
    }
    text
}

/// The charset of a `Content-Type`, as httpx reads it.
fn charset(content_type: Option<&str>) -> Option<String> {
    let value = content_type?;
    value.split(';').skip(1).find_map(|part| {
        let (name, rest) = part.split_once('=')?;
        if name.trim().eq_ignore_ascii_case("charset") {
            Some(rest.trim().trim_matches('"').to_ascii_lowercase())
        } else {
            None
        }
    })
}

/// `response.text`: UTF-8 (or the declared charset, when this side knows it) decoded with
/// `errors="replace"`. `Err` for a charset this side does not decode.
pub fn text(body: &[u8], content_type: Option<&str>) -> Result<String, Unsupported> {
    match charset(content_type).as_deref() {
        None | Some("utf-8" | "utf8") => Ok(String::from_utf8_lossy(body).into_owned()),
        Some("iso-8859-1" | "latin-1" | "latin1" | "l1") => {
            Ok(body.iter().map(|b| char::from(*b)).collect())
        }
        Some("us-ascii" | "ascii") => Ok(body
            .iter()
            .map(|b| {
                if b.is_ascii() {
                    char::from(*b)
                } else {
                    '\u{fffd}'
                }
            })
            .collect()),
        Some(_) => Err(Unsupported),
    }
}

/// The first `count` characters of `text.strip()`.
pub fn stripped_head(text: &str, count: usize) -> String {
    py::strip(text).chars().take(count).collect()
}

/// The request line httpx sends for `f"{base}/{path}"` after its URL parsing: the path with
/// `.` and `..` segments resolved and quoted, and the query quoted; the fragment is dropped.
/// `None` for text httpx refuses (an ASCII control character): `httpx.InvalidURL`.
pub fn httpx_target(base: &str, tail: &str) -> Option<(String, Option<String>)> {
    let full = format!("{base}/{tail}");
    if full.chars().any(|c| c.is_ascii() && !is_printable(c)) {
        return None;
    }
    let (rest, _fragment) = match full.split_once('#') {
        Some((rest, fragment)) => (rest, Some(fragment)),
        None => (full.as_str(), None),
    };
    let (location, query) = match rest.split_once('?') {
        Some((location, query)) => (location, Some(query)),
        None => (rest, None),
    };
    // `scheme://authority` + path.
    let after_scheme = location.split_once("://")?.1;
    let slash = after_scheme.find('/').unwrap_or(after_scheme.len());
    let path = after_scheme.get(slash..)?;
    let path = normalize_path(path);
    Some((quote(&path, PATH_SAFE), query.map(|q| quote(q, QUERY_SAFE))))
}

fn is_printable(c: char) -> bool {
    (' '..='~').contains(&c)
}

/// httpx's `normalize_path`: drop `.` and `..` segments.
fn normalize_path(path: &str) -> String {
    if !path.contains('.') {
        return path.to_owned();
    }
    let components: Vec<&str> = path.split('/').collect();
    if !components.contains(&".") && !components.contains(&"..") {
        return path.to_owned();
    }
    let mut output: Vec<&str> = Vec::new();
    for component in components {
        match component {
            "." => {}
            ".." => {
                if !output.is_empty() && output != [""] {
                    output.pop();
                }
            }
            other => output.push(other),
        }
    }
    output.join("/")
}

/// Characters httpx leaves in a path / a query (besides the unreserved ones).
const PATH_SAFE: &str =
    "!$%&'()*+,-./0123456789:;=@ABCDEFGHIJKLMNOPQRSTUVWXYZ[\\]^_abcdefghijklmnopqrstuvwxyz|~";
const QUERY_SAFE: &str =
    "!$%&'()*+,-./0123456789:;=?@ABCDEFGHIJKLMNOPQRSTUVWXYZ[\\]^_`abcdefghijklmnopqrstuvwxyz{|}~";

/// httpx's `quote`: percent-encode what is not safe, keeping existing `%XX` sequences.
fn quote(text: &str, safe: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::new();
    let mut i = 0;
    while let Some(&b) = bytes.get(i) {
        let escape = b == b'%'
            && bytes.get(i + 1).is_some_and(u8::is_ascii_hexdigit)
            && bytes.get(i + 2).is_some_and(u8::is_ascii_hexdigit);
        if escape {
            out.push_str(text.get(i..i + 3).unwrap_or(""));
            i += 3;
            continue;
        }
        // Characters, not bytes, are tested; a non-ASCII character is percent-encoded whole.
        let rest = text.get(i..).unwrap_or("");
        let Some(c) = rest.chars().next() else { break };
        if c.is_ascii() && (c.is_ascii_alphanumeric() || safe.contains(c)) {
            out.push(c);
        } else {
            let mut buffer = [0u8; 4];
            for byte in c.encode_utf8(&mut buffer).bytes() {
                out.push_str(&format!("%{byte:02X}"));
            }
        }
        i += c.len_utf8();
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn charsets() {
        assert_eq!(
            charset(Some("application/json; charset=UTF-8")).as_deref(),
            Some("utf-8")
        );
        assert_eq!(charset(Some("text/plain")), None);
        assert_eq!(text(b"a\xffb", None).unwrap(), "a\u{fffd}b");
        assert_eq!(
            text(b"a\xe9", Some("text/plain; charset=latin-1")).unwrap(),
            "a\u{e9}"
        );
        assert!(text(b"a", Some("text/plain; charset=utf-16")).is_err());
        assert_eq!(stripped_head("  \u{3000}abcdef\n", 3), "abc");
    }

    #[test]
    fn target_matches_httpx() {
        let target = |tail: &str| httpx_target("https://hub.example", tail).unwrap();
        assert_eq!(
            target("org/m/resolve/main/f.gguf"),
            ("/org/m/resolve/main/f.gguf".to_owned(), None)
        );
        assert_eq!(
            target("o/m/resolve/main/a b.gguf").0,
            "/o/m/resolve/main/a%20b.gguf"
        );
        assert_eq!(
            target("o/m/resolve/main/../x.gguf").0,
            "/o/m/resolve/x.gguf"
        );
        assert_eq!(
            target("o/m/resolve/main/q?x=1 2#frag"),
            ("/o/m/resolve/main/q".to_owned(), Some("x=1%202".to_owned()))
        );
        assert_eq!(
            target("o/m/resolve/main/\u{fc}.gguf").0,
            "/o/m/resolve/main/%C3%BC.gguf"
        );
        // An existing escape stays; a lone `%` is a safe character of the path.
        assert_eq!(
            target("o/m/resolve/main/a%2Fb%zz").0,
            "/o/m/resolve/main/a%2Fb%zz"
        );
        assert_eq!(
            target("o/m/resolve/main/{x}`y`.gguf").0,
            "/o/m/resolve/main/%7Bx%7D%60y%60.gguf"
        );
        assert_eq!(httpx_target("https://hub.example", "a\nb"), None);
    }
}
