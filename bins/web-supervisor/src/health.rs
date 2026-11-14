//! `noevia-web-supervisor --healthcheck`: the web image's HEALTHCHECK without Node (inert: the
//! image still runs its `node -e` probe; nothing calls this until a release moves the entrypoint).
//!
//! The probe it replaces (noevia build/web.Dockerfile, HEALTHCHECK, quoted in
//! tests/health.rs as `NODE_PROBE`):
//!
//! ```text
//! fetch('http://127.0.0.1:'+(process.env.UI_PORT||8021)+'/api/setup/status',
//!       {signal:AbortSignal.timeout(5000)})
//!   .then(r=>process.exit(r.ok?0:1)).catch(()=>process.exit(1))
//! ```
//!
//! Behaviour kept from it:
//! - the port is `UI_PORT` when set and not empty, else 8021, pasted into the URL as text, so the
//!   URL parser's rules apply: tab, LF and CR vanish; an empty port means 80; only ASCII digits
//!   (leading zeros allowed) up to 65535 are a port; anything else makes the URL invalid (exit 1);
//! - one deadline of five seconds over the whole exchange, redirects included;
//! - healthy (exit 0) only for a final status 200..=299; the body is never read;
//! - redirects (301, 302, 303, 307, 308 with a Location) are followed, at most 20, against the
//!   current URL (absolute, scheme-relative, absolute-path, query-only and relative references,
//!   dot segments removed); a 3xx without a Location is final (not ok);
//! - interim 1xx answers (except 101) are skipped; a 101, a broken or missing status line, a
//!   head over 16 KiB (Node's default header limit), a reset or an early close are failures (exit 1).
//!
//! Known differences, all ending in exit 1 where Node might have reached a healthy answer:
//! - a redirect to `https:` (no TLS here; the front never sends one, it answers itself);
//! - a `UI_PORT` with a path, query or fragment after the digits (`8021/x`): Node probes another
//!   path, here the port is invalid. Such a value is also no port the front could bind.
//! - a non-UTF-8 `UI_PORT`.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

/// `process.env.UI_PORT || 8021`.
pub const DEFAULT_PORT: &str = "8021";
/// The route the probe asks (answered by the front itself, no login needed).
pub const PATH: &str = "/api/setup/status";
/// `AbortSignal.timeout(5000)`.
pub const DEADLINE: Duration = Duration::from_secs(5);
/// Fetch's redirect limit: the 21st redirect is a network error.
pub const MAX_REDIRECTS: usize = 20;
/// Largest response head accepted: Node's default HTTP header limit (16 KiB).
pub const MAX_HEAD_BYTES: usize = 16 * 1024;

/// One HTTP request target: where to connect and what to ask.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// Host as written in the URL (lowercase; IPv6 without brackets).
    pub host: String,
    pub port: u16,
    /// Path and query, percent-encoded, always starting with `/`.
    pub path: String,
}

impl Target {
    /// The Host header: the URL's host, the port left out when it is 80 (as fetch sends it).
    pub fn host_header(&self) -> String {
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        if self.port == 80 {
            host
        } else {
            format!("{host}:{}", self.port)
        }
    }
}

/// The status line and the one header the probe needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Head {
    pub status: u16,
    pub location: Option<String>,
}

/// Why a probe failed (all of them exit 1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    /// The URL (port) is invalid; nothing was connected.
    InvalidUrl,
    /// Connect, write or read failed, or the peer closed before a full head.
    Network,
    /// The deadline passed.
    Timeout,
    /// No HTTP/1.x status line, a 101, or a head over the cap.
    BadResponse,
    /// More than 20 redirects, or a Location that is not a usable `http:` URL.
    BadRedirect,
}

/// `'http://127.0.0.1:' + (UI_PORT || 8021) + '/api/setup/status'`, or `None` when that URL is invalid.
pub fn start_target(ui_port: Option<&str>) -> Option<Target> {
    let raw = match ui_port {
        Some(p) if !p.is_empty() => p,
        _ => DEFAULT_PORT,
    };
    let cleaned: String = raw.chars().filter(|c| !matches!(c, '\t' | '\n' | '\r')).collect();
    Some(Target {
        host: "127.0.0.1".to_string(),
        port: parse_port(&cleaned)?,
        path: PATH.to_string(),
    })
}

/// The URL parser's port state for an `http:` URL: empty is the default port 80, otherwise ASCII
/// digits only (leading zeros allowed), at most 65535.
pub fn parse_port(text: &str) -> Option<u16> {
    if text.is_empty() {
        return Some(80);
    }
    if !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let digits = text.trim_start_matches('0');
    if digits.is_empty() {
        return Some(0);
    }
    if digits.len() > 5 {
        return None;
    }
    digits.parse::<u32>().ok().and_then(|n| u16::try_from(n).ok())
}

/// Parses a response head (status line and headers, without the blank line).
pub fn parse_head(head: &[u8]) -> Result<Head, Failure> {
    let text = String::from_utf8_lossy(head);
    let mut lines = text.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l));
    let status_line = lines.next().ok_or(Failure::BadResponse)?;
    let mut parts = status_line.splitn(3, ' ');
    let version = parts.next().unwrap_or_default();
    let code = parts.next().unwrap_or_default();
    if !version.starts_with("HTTP/1.") || version.len() != 8 {
        return Err(Failure::BadResponse);
    }
    if code.len() != 3 || !code.bytes().all(|b| b.is_ascii_digit()) {
        return Err(Failure::BadResponse);
    }
    let status: u16 = code.parse().map_err(|_| Failure::BadResponse)?;
    if !(100..=999).contains(&status) {
        return Err(Failure::BadResponse);
    }
    let mut location = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.eq_ignore_ascii_case("location") && location.is_none() {
            location = Some(value.trim_matches(|c| c == ' ' || c == '\t').to_string());
        }
    }
    Ok(Head { status, location })
}

/// Finds the end of the first head in `buf`: (head length, bytes up to and including the blank line).
fn head_end(buf: &[u8]) -> Option<(usize, usize)> {
    let mut i = 0;
    while i < buf.len() {
        if buf.get(i) == Some(&b'\n') {
            if buf.get(i + 1) == Some(&b'\n') {
                return Some((i, i + 2));
            }
            if buf.get(i + 1) == Some(&b'\r') && buf.get(i + 2) == Some(&b'\n') {
                return Some((i, i + 3));
            }
        }
        i += 1;
    }
    None
}

fn remaining(deadline: Instant) -> Result<Duration, Failure> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        Err(Failure::Timeout)
    } else {
        Ok(left)
    }
}

fn io_failure(e: &std::io::Error) -> Failure {
    match e.kind() {
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => Failure::Timeout,
        _ => Failure::Network,
    }
}

fn connect(target: &Target, deadline: Instant) -> Result<TcpStream, Failure> {
    let addrs: Vec<SocketAddr> = (target.host.as_str(), target.port)
        .to_socket_addrs()
        .map_err(|_| Failure::Network)?
        .collect();
    let mut last = Failure::Network;
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, remaining(deadline)?) {
            Ok(stream) => return Ok(stream),
            Err(e) => last = io_failure(&e),
        }
    }
    Err(last)
}

/// One request and its final (non-interim) head, within the deadline.
pub fn exchange(target: &Target, deadline: Instant) -> Result<Head, Failure> {
    let mut stream = connect(target, deadline)?;
    stream
        .set_write_timeout(Some(remaining(deadline)?))
        .map_err(|e| io_failure(&e))?;
    let request = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nAccept: */*\r\nConnection: close\r\n\r\n",
        target.path,
        target.host_header()
    );
    stream.write_all(request.as_bytes()).map_err(|e| io_failure(&e))?;
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        while let Some((len, end)) = head_end(&buf) {
            if len > MAX_HEAD_BYTES {
                return Err(Failure::BadResponse);
            }
            let head = parse_head(buf.get(..len).unwrap_or_default())?;
            if head.status == 101 {
                return Err(Failure::BadResponse);
            }
            if head.status >= 200 {
                return Ok(head);
            }
            buf.drain(..end);
        }
        if buf.len() > MAX_HEAD_BYTES {
            return Err(Failure::BadResponse);
        }
        stream
            .set_read_timeout(Some(remaining(deadline)?))
            .map_err(|e| io_failure(&e))?;
        match stream.read(&mut chunk) {
            Ok(0) => return Err(Failure::Network),
            Ok(n) => buf.extend_from_slice(chunk.get(..n).unwrap_or_default()),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(io_failure(&e)),
        }
    }
}

fn is_redirect(status: u16) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

/// Follows the probe from `start` to its final status, at most 20 redirects, one deadline.
pub fn probe(start: Target, budget: Duration) -> Result<u16, Failure> {
    let deadline = Instant::now() + budget;
    let mut target = start;
    let mut followed = 0usize;
    loop {
        let head = exchange(&target, deadline)?;
        match head.location {
            Some(location) if is_redirect(head.status) => {
                if followed == MAX_REDIRECTS {
                    return Err(Failure::BadRedirect);
                }
                followed += 1;
                target = resolve(&target, &location).ok_or(Failure::BadRedirect)?;
            }
            _ => return Ok(head.status),
        }
    }
}

/// The exit status of the probe: 0 for a final 2xx, 1 for anything else.
pub fn exit_code(result: &Result<u16, Failure>) -> i32 {
    match result {
        Ok(status) if (200..=299).contains(status) => 0,
        _ => 1,
    }
}

/// `--healthcheck`: reads `UI_PORT` from the process environment and probes with the five-second deadline.
pub fn run() -> i32 {
    let port = std::env::var_os("UI_PORT");
    let port = match port.as_ref().map(|p| p.to_str()) {
        None => None,
        Some(Some(text)) => Some(text),
        Some(None) => return 1,
    };
    match start_target(port) {
        Some(target) => exit_code(&probe(target, DEADLINE)),
        None => 1,
    }
}

// --- URL resolution for Location (WHATWG rules, http only) ---

/// Percent-encodes what the URL parser encodes in a path (`path`) or a query (`!path`).
fn encode(text: &str, path: bool) -> String {
    let mut out = String::with_capacity(text.len());
    for b in text.bytes() {
        let escape = b <= 0x20
            || b >= 0x7f
            || matches!(b, b'"' | b'<' | b'>')
            || (path && matches!(b, b'`' | b'{' | b'}'))
            || (!path && b == b'\'');
        if escape {
            out.push_str(&format!("%{b:02X}"));
        } else {
            out.push(char::from(b));
        }
    }
    out
}

/// Removes `.` and `..` segments (also `%2e` spellings) from an absolute path.
fn remove_dots(path: &str) -> String {
    let segments: Vec<&str> = path.split('/').skip(1).collect();
    let mut out: Vec<&str> = Vec::new();
    let count = segments.len();
    for (i, seg) in segments.iter().enumerate() {
        let lower = seg.to_ascii_lowercase();
        let single = lower == "." || lower == "%2e";
        let double = matches!(lower.as_str(), ".." | ".%2e" | "%2e." | "%2e%2e");
        let last = i + 1 == count;
        if double {
            out.pop();
            if last {
                out.push("");
            }
        } else if single {
            if last {
                out.push("");
            }
        } else {
            out.push(seg);
        }
    }
    format!("/{}", out.join("/"))
}

/// Splits `path?query` and encodes each part.
fn path_and_query(raw: &str) -> String {
    let (path, query) = match raw.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (raw, None),
    };
    let path = if path.is_empty() { "/".to_string() } else { remove_dots(&path.replace('\\', "/")) };
    let mut out = encode(&path, true);
    if let Some(q) = query {
        out.push('?');
        out.push_str(&encode(q, false));
    }
    out
}

/// Parses `host[:port]` (userinfo dropped, IPv6 in brackets) of an `http:` authority.
fn authority(text: &str) -> Option<(String, u16)> {
    let hostport = text.rsplit_once('@').map_or(text, |(_, h)| h);
    let (host, port) = if let Some(rest) = hostport.strip_prefix('[') {
        let (host, after) = rest.split_once(']')?;
        let port = match after {
            "" => "",
            p => p.strip_prefix(':')?,
        };
        (host.to_string(), port)
    } else {
        match hostport.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), p),
            None => (hostport.to_string(), ""),
        }
    };
    if host.is_empty() || host.bytes().any(|b| b <= 0x20 || matches!(b, b'/' | b'?' | b'#' | b'\\')) {
        return None;
    }
    Some((host.to_ascii_lowercase(), parse_port(port)?))
}

/// The scheme of an absolute reference (`letters[+-.]*:` before any `/`, `?` or `#`), lowercase.
fn scheme(text: &str) -> Option<String> {
    let (scheme, _) = text.split_once(':')?;
    let mut bytes = scheme.bytes();
    let first = bytes.next()?;
    if !first.is_ascii_alphabetic() {
        return None;
    }
    if !bytes.all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.')) {
        return None;
    }
    Some(scheme.to_ascii_lowercase())
}

/// Resolves a Location against the current target, as fetch does; `None` = network error
/// (unparsable, not `http:`).
pub fn resolve(base: &Target, location: &str) -> Option<Target> {
    let cleaned: String = location.chars().filter(|c| !matches!(c, '\t' | '\n' | '\r')).collect();
    let trimmed = cleaned.trim_matches(|c: char| c <= ' ');
    let reference = trimmed.split_once('#').map_or(trimmed, |(r, _)| r);
    // An `http:` prefix on an `http:` base only matters before two slashes (an authority);
    // otherwise the rest is relative to the base, as in the URL parser's special-relative state.
    let reference = match scheme(reference) {
        Some(s) if s == "http" => reference.get(s.len() + 1..).unwrap_or_default(),
        Some(_) => return None,
        None => reference,
    };
    let slashy = |c: Option<char>| matches!(c, Some('/' | '\\'));
    let mut chars = reference.chars();
    let first = chars.next();
    if slashy(first) && slashy(chars.next()) {
        // A special scheme ignores every slash and backslash before the authority.
        let rest = reference.trim_start_matches(['/', '\\']);
        if rest.is_empty() {
            return None;
        }
        return absolute(rest);
    }
    let base_path = base.path.split_once('?').map_or(base.path.as_str(), |(p, _)| p);
    let raw = if reference.is_empty() {
        return Some(base.clone());
    } else if slashy(first) {
        reference.to_string()
    } else if reference.starts_with('?') {
        format!("{base_path}{reference}")
    } else {
        let dir = base_path.rsplit_once('/').map_or("", |(d, _)| d);
        format!("{dir}/{reference}")
    };
    Some(Target {
        host: base.host.clone(),
        port: base.port,
        path: path_and_query(&raw),
    })
}

/// `authority[/path][?query]` after `http://`.
fn absolute(rest: &str) -> Option<Target> {
    let end = rest.find(['/', '?', '\\']).unwrap_or(rest.len());
    let (auth, tail) = rest.split_at(end);
    let (host, port) = authority(auth)?;
    let tail = if tail.starts_with('?') { format!("/{tail}") } else { tail.to_string() };
    Some(Target {
        host,
        port,
        path: if tail.is_empty() { "/".to_string() } else { path_and_query(&tail) },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(host: &str, port: u16, path: &str) -> Target {
        Target { host: host.to_string(), port, path: path.to_string() }
    }

    #[test]
    fn start_url_follows_the_or_and_the_port_state() {
        let target = |p: Option<&str>| start_target(p).map(|t| t.port);
        assert_eq!(target(None), Some(8021));
        assert_eq!(target(Some("")), Some(8021)); // '' || 8021
        assert_eq!(target(Some("9000")), Some(9000));
        assert_eq!(target(Some("09000")), Some(9000));
        assert_eq!(target(Some("0")), Some(0));
        assert_eq!(target(Some("65535")), Some(65535));
        assert_eq!(target(Some("65536")), None);
        assert_eq!(target(Some("0000000000080")), Some(80));
        assert_eq!(target(Some("99999999999")), None);
        assert_eq!(target(Some("\t")), Some(80)); // tab removed: empty port = 80
        assert_eq!(target(Some("90\n00")), Some(9000));
        assert_eq!(target(Some(" 9000")), None);
        assert_eq!(target(Some("9000 ")), None);
        assert_eq!(target(Some("abc")), None);
        assert_eq!(target(Some("-1")), None);
        assert_eq!(target(Some("+80")), None);
        assert_eq!(target(Some("８０")), None); // full-width digits are not ASCII digits
        assert_eq!(start_target(None).map(|t| t.path), Some(PATH.to_string()));
    }

    #[test]
    fn host_header_drops_the_default_port() {
        assert_eq!(t("127.0.0.1", 8021, "/").host_header(), "127.0.0.1:8021");
        assert_eq!(t("127.0.0.1", 80, "/").host_header(), "127.0.0.1");
        assert_eq!(t("::1", 9, "/").host_header(), "[::1]:9");
    }

    #[test]
    fn heads_parse_status_and_location() {
        assert_eq!(
            parse_head(b"HTTP/1.1 200 OK\r\nContent-Type: x"),
            Ok(Head { status: 200, location: None })
        );
        assert_eq!(
            parse_head(b"HTTP/1.0 302 Found\r\nlOcAtIoN:  /next \r\nLocation: /second"),
            Ok(Head { status: 302, location: Some("/next".to_string()) })
        );
        assert_eq!(parse_head(b"HTTP/1.1 204"), Ok(Head { status: 204, location: None }));
        assert_eq!(parse_head(b"HTTP/1.1 200 OK\nX: y"), Ok(Head { status: 200, location: None }));
        assert_eq!(parse_head(b"HTTP/2 200 OK"), Err(Failure::BadResponse));
        assert_eq!(parse_head(b"HTTP/1.1 20 OK"), Err(Failure::BadResponse));
        assert_eq!(parse_head(b"HTTP/1.1 2000 OK"), Err(Failure::BadResponse));
        assert_eq!(parse_head(b"HTTP/1.1 099 x"), Err(Failure::BadResponse));
        assert_eq!(parse_head(b"hello"), Err(Failure::BadResponse));
        assert_eq!(parse_head(b""), Err(Failure::BadResponse));
    }

    #[test]
    fn head_end_finds_crlf_and_lf_blank_lines() {
        assert_eq!(head_end(b"HTTP/1.1 200 OK\r\n\r\nbody"), Some((16, 19)));
        assert_eq!(head_end(b"HTTP/1.1 200 OK\n\nbody"), Some((15, 17)));
        assert_eq!(head_end(b"HTTP/1.1 200 OK\r\nX: y\r\n"), None);
    }

    #[test]
    fn exit_code_is_zero_only_for_2xx() {
        for (r, code) in [
            (Ok(200), 0),
            (Ok(204), 0),
            (Ok(299), 0),
            (Ok(199), 1),
            (Ok(300), 1),
            (Ok(304), 1),
            (Ok(404), 1),
            (Ok(500), 1),
            (Err(Failure::Timeout), 1),
            (Err(Failure::Network), 1),
        ] {
            assert_eq!(exit_code(&r), code, "{r:?}");
        }
    }

    #[test]
    fn locations_resolve_like_the_url_parser() {
        let base = t("127.0.0.1", 8021, "/api/setup/status");
        let r = |loc: &str| resolve(&base, loc);
        assert_eq!(r("/ok"), Some(t("127.0.0.1", 8021, "/ok")));
        assert_eq!(r("next"), Some(t("127.0.0.1", 8021, "/api/setup/next")));
        assert_eq!(r("../x"), Some(t("127.0.0.1", 8021, "/api/x")));
        assert_eq!(r("../../../../x"), Some(t("127.0.0.1", 8021, "/x")));
        assert_eq!(r("./"), Some(t("127.0.0.1", 8021, "/api/setup/")));
        assert_eq!(r(".."), Some(t("127.0.0.1", 8021, "/api/")));
        assert_eq!(r("?a=1"), Some(t("127.0.0.1", 8021, "/api/setup/status?a=1")));
        assert_eq!(r(""), Some(base.clone()));
        assert_eq!(r("#frag"), Some(base.clone()));
        assert_eq!(r("/a b"), Some(t("127.0.0.1", 8021, "/a%20b")));
        assert_eq!(r("/a{b}?c d"), Some(t("127.0.0.1", 8021, "/a%7Bb%7D?c%20d")));
        assert_eq!(r("/é"), Some(t("127.0.0.1", 8021, "/%C3%A9")));
        assert_eq!(r("\\x\\y"), Some(t("127.0.0.1", 8021, "/x/y")));
        assert_eq!(r("/a/%2e%2E/b"), Some(t("127.0.0.1", 8021, "/b")));
        assert_eq!(r("  /trim\t "), Some(t("127.0.0.1", 8021, "/trim")));
        assert_eq!(r("http://127.0.0.1:9/p?q#f"), Some(t("127.0.0.1", 9, "/p?q")));
        assert_eq!(r("HTTP://LocalHost:9"), Some(t("localhost", 9, "/")));
        assert_eq!(r("http://127.0.0.1"), Some(t("127.0.0.1", 80, "/")));
        assert_eq!(r("http://127.0.0.1:"), Some(t("127.0.0.1", 80, "/")));
        assert_eq!(r("http:\\\\127.0.0.1:9\\p"), Some(t("127.0.0.1", 9, "/p")));
        // `http:` without two slashes is relative to an `http:` base.
        assert_eq!(r("http:127.0.0.1:9/p"), Some(t("127.0.0.1", 8021, "/api/setup/127.0.0.1:9/p")));
        assert_eq!(r("http:/p"), Some(t("127.0.0.1", 8021, "/p")));
        assert_eq!(r("http:"), Some(base.clone()));
        assert_eq!(r("http:?q"), Some(t("127.0.0.1", 8021, "/api/setup/status?q")));
        assert_eq!(r("///127.0.0.1:7/s"), Some(t("127.0.0.1", 7, "/s")));
        assert_eq!(r("/\\127.0.0.1:7/s"), Some(t("127.0.0.1", 7, "/s")));
        assert_eq!(r("1x:y"), Some(t("127.0.0.1", 8021, "/api/setup/1x:y")));
        assert_eq!(r("a:b"), None);
        assert_eq!(r("http://user:pw@127.0.0.1:9/"), Some(t("127.0.0.1", 9, "/")));
        assert_eq!(r("http://[::1]:9/v6"), Some(t("::1", 9, "/v6")));
        assert_eq!(r("http://127.0.0.1:9?q"), Some(t("127.0.0.1", 9, "/?q")));
        assert_eq!(r("//127.0.0.1:7/s"), Some(t("127.0.0.1", 7, "/s")));
        assert_eq!(r("https://127.0.0.1/"), None);
        assert_eq!(r("ftp://x/"), None);
        assert_eq!(r("http://"), None);
        assert_eq!(r("http://127.0.0.1:x/"), None);
        assert_eq!(r("http://127.0.0.1:70000/"), None);
        assert_eq!(r("http://[::1/"), None);
    }

    #[test]
    fn dot_segments_and_encoding() {
        assert_eq!(remove_dots("/a/b/../c/./d"), "/a/c/d");
        assert_eq!(remove_dots("/a/b/.."), "/a/");
        assert_eq!(remove_dots("/a/b/."), "/a/b/");
        assert_eq!(remove_dots("/.."), "/");
        assert_eq!(encode("a\"<>`{}", true), "a%22%3C%3E%60%7B%7D");
        assert_eq!(encode("a\"<>`{}'", false), "a%22%3C%3E`{}%27");
        assert_eq!(encode("it's", true), "it's");
    }
}
