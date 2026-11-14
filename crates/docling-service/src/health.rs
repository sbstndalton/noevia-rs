//! The image's health probe without Python: what the docling image's `HEALTHCHECK` asks of
//! `python -c "import urllib.request; urllib.request.urlopen('http://127.0.0.1:8031/health', timeout=5)"`,
//! as plain std networking (no new dependency, no TLS).
//!
//! Healthy means what `urlopen` returning means: the final answer has a status from 200 to 299, after
//! skipping `100 Continue` answers and following at most 10 redirects (301, 302, 303, 307, 308 with a
//! `Location` or `URI` header; the same URL at most 4 times). Everything else is unhealthy: any other
//! status, a refused or silent connection (the timeout counts per socket operation, as Python's
//! does), a status line that is not `HTTP/1.x` with three digits, a header line over 65,536 bytes or
//! more than 100 headers (http.client's limits), a redirect to a URL without a host.
//!
//! Stricter than the reference, never looser: a redirect to `https:` or `ftp:` is unhealthy here
//! (Python would try it), and proxy variables are ignored (Python would honour `http_proxy`, which
//! a loopback probe never wants). The body is not read, as `urlopen` does not read it.
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{TcpStream, ToSocketAddrs},
    time::Duration,
};

pub const DEFAULT_PORT: u16 = 8031;
pub const TIMEOUT: Duration = Duration::from_secs(5);
const MAX_REDIRECTIONS: usize = 10;
const MAX_REPEATS: usize = 4;
const MAX_LINE: usize = 65_536;
const MAX_HEADERS: usize = 100;

/// Why the probe failed (one line for the operator; the URL is ours, never a secret).
#[derive(Debug, PartialEq, Eq)]
pub enum Unhealthy {
    Url(String),
    Connect(String),
    Protocol(&'static str),
    Status(u16),
    Redirect(&'static str),
}

impl std::fmt::Display for Unhealthy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Unhealthy::Url(url) => write!(f, "unsupported URL {url}"),
            Unhealthy::Connect(error) => write!(f, "no answer: {error}"),
            Unhealthy::Protocol(what) => write!(f, "bad answer: {what}"),
            Unhealthy::Status(code) => write!(f, "HTTP {code}"),
            Unhealthy::Redirect(what) => write!(f, "redirect refused: {what}"),
        }
    }
}

/// The URL the image probes: `DOCLING_PORT` as the server reads it (unset = 8031), so a container
/// started on another port is probed where it listens (the reference always probed 8031).
pub fn default_url(port: Option<&str>) -> Result<String, Unhealthy> {
    let port = match port {
        None => DEFAULT_PORT,
        Some(value) => value
            .parse::<u16>()
            .map_err(|_| Unhealthy::Url(format!("DOCLING_PORT={value}")))?,
    };
    Ok(format!("http://127.0.0.1:{port}/health"))
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Target {
    host: String,
    port: u16,
    path: String,
}

impl Target {
    fn url(&self) -> String {
        format!("http://{}:{}{}", self.host, self.port, self.path)
    }

    fn authority(&self) -> String {
        if self.port == 80 {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

fn parse(url: &str) -> Result<Target, Unhealthy> {
    let bad = || Unhealthy::Url(url.to_string());
    let rest = url
        .get(..7)
        .filter(|scheme| scheme.eq_ignore_ascii_case("http://"))
        .and_then(|_| url.get(7..))
        .ok_or_else(bad)?;
    let (authority, path) = match rest.find(['/', '?', '#']) {
        Some(at) => rest.split_at(at),
        None => (rest, ""),
    };
    // The fragment is never sent.
    let path = path.split('#').next().unwrap_or("");
    let path = if path.is_empty() {
        "/".to_string()
    } else if path.starts_with('?') {
        format!("/{path}")
    } else {
        path.to_string()
    };
    if authority.contains('@') || authority.is_empty() {
        return Err(bad());
    }
    let (host, port) = if let Some(inner) = authority.strip_prefix('[') {
        let (host, after) = inner.split_once(']').ok_or_else(bad)?;
        let port = match after {
            "" => 80,
            _ => after.strip_prefix(':').and_then(port_of).ok_or_else(bad)?,
        };
        (format!("[{host}]"), port)
    } else {
        match authority.rsplit_once(':') {
            Some((host, port)) => (host.to_string(), port_of(port).ok_or_else(bad)?),
            None => (authority.to_string(), 80),
        }
    };
    if host.is_empty() || path.chars().any(|c| c.is_ascii_control() || c == ' ') {
        return Err(bad());
    }
    Ok(Target { host, port, path })
}

/// http.client: an empty port means the default; digits only.
fn port_of(text: &str) -> Option<u16> {
    if text.is_empty() {
        return Some(80);
    }
    if !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// Where a `Location` points from `base` (urljoin for the forms a server sends).
fn join(base: &Target, location: &str) -> Result<Option<Target>, Unhealthy> {
    let location = location.trim();
    let lower = location.to_ascii_lowercase();
    if lower.starts_with("http://") {
        return parse(location).map(Some);
    }
    if lower.starts_with("https://") || lower.starts_with("ftp://") {
        return Ok(None);
    }
    if let Some(scheme_end) = location.find(':') {
        let scheme = lower.get(..scheme_end).unwrap_or("");
        if !scheme.is_empty()
            && !location.starts_with('/')
            && scheme
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"+-.".contains(&b))
        {
            // Any other scheme: urllib refuses it too.
            return Err(Unhealthy::Redirect("scheme"));
        }
    }
    if let Some(rest) = location.strip_prefix("//") {
        return parse(&format!("http://{rest}")).map(Some);
    }
    let path = if location.starts_with('/') {
        location.to_string()
    } else if location.is_empty() {
        base.path.clone()
    } else if location.starts_with('?') {
        let path = base.path.split('?').next().unwrap_or("/");
        format!("{path}{location}")
    } else {
        let path = base.path.split('?').next().unwrap_or("/");
        let directory = path.rsplit_once('/').map_or("", |(dir, _)| dir);
        format!("{directory}/{location}")
    };
    Ok(Some(Target {
        path: normalise(&path),
        ..base.clone()
    }))
}

/// Remove `.` and `..` segments (RFC 3986 section 5.2.4), keeping the query.
fn normalise(path: &str) -> String {
    let (path, query) = match path.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (path, None),
    };
    let mut out: Vec<&str> = Vec::new();
    let segments: Vec<&str> = path.split('/').skip(1).collect();
    let last = segments.len().saturating_sub(1);
    for (index, segment) in segments.iter().enumerate() {
        match *segment {
            "." => {
                if index == last {
                    out.push("");
                }
            }
            ".." => {
                out.pop();
                if index == last {
                    out.push("");
                }
            }
            other => out.push(other),
        }
    }
    let mut joined = format!("/{}", out.join("/"));
    if let Some(query) = query {
        joined.push('?');
        joined.push_str(query);
    }
    joined
}

/// One line, at most MAX_LINE bytes plus its terminator (http.client's _MAXLINE).
fn read_line(reader: &mut impl BufRead) -> Result<Vec<u8>, Unhealthy> {
    let mut line = Vec::new();
    let read = reader
        .by_ref()
        .take(MAX_LINE as u64 + 1)
        .read_until(b'\n', &mut line)
        .map_err(|e| Unhealthy::Connect(e.to_string()))?;
    if read > MAX_LINE {
        return Err(Unhealthy::Protocol("line too long"));
    }
    Ok(line)
}

/// The status of one exchange and its redirect target header, if any.
fn exchange(target: &Target, timeout: Duration) -> Result<(u16, Option<String>), Unhealthy> {
    let addresses = (
        target.host.trim_start_matches('[').trim_end_matches(']'),
        target.port,
    )
        .to_socket_addrs()
        .map_err(|e| Unhealthy::Connect(e.to_string()))?;
    let mut last = Unhealthy::Connect("no address".to_string());
    let mut stream = None;
    for address in addresses {
        match TcpStream::connect_timeout(&address, timeout) {
            Ok(connected) => {
                stream = Some(connected);
                break;
            }
            Err(error) => last = Unhealthy::Connect(error.to_string()),
        }
    }
    let mut stream = stream.ok_or(last)?;
    let io = |e: std::io::Error| Unhealthy::Connect(e.to_string());
    stream.set_read_timeout(Some(timeout)).map_err(io)?;
    stream.set_write_timeout(Some(timeout)).map_err(io)?;
    let request = format!(
        "GET {} HTTP/1.1\r\nAccept-Encoding: identity\r\nHost: {}\r\nUser-Agent: noevia-healthcheck\r\nConnection: close\r\n\r\n",
        target.path,
        target.authority()
    );
    stream.write_all(request.as_bytes()).map_err(io)?;
    let mut reader = BufReader::new(stream);
    loop {
        let line = read_line(&mut reader)?;
        if line.is_empty() {
            return Err(Unhealthy::Protocol("closed before a status line"));
        }
        let status = status_of(&line)?;
        let headers = read_headers(&mut reader)?;
        if status == 100 {
            continue;
        }
        let location = headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("location"))
            .or_else(|| {
                headers
                    .iter()
                    .find(|(name, _)| name.eq_ignore_ascii_case("uri"))
            })
            .map(|(_, value)| value.clone());
        return Ok((status, location));
    }
}

/// `HTTP/1.x NNN reason`: http.client's `_read_status` and `begin` version rule.
fn status_of(line: &[u8]) -> Result<u16, Unhealthy> {
    let text = String::from_utf8_lossy(line);
    let text = text.trim_end_matches(['\r', '\n']);
    // str.split(None, 2): runs of whitespace separate the fields.
    let mut parts = text.split_whitespace();
    let version = parts.next().unwrap_or("");
    let code = parts.next().unwrap_or("");
    if !version.starts_with("HTTP/") {
        return Err(Unhealthy::Protocol("not an HTTP status line"));
    }
    if !version.starts_with("HTTP/1.") {
        return Err(Unhealthy::Protocol("unknown protocol version"));
    }
    if code.len() != 3 || !code.bytes().all(|b| b.is_ascii_digit()) {
        return Err(Unhealthy::Protocol("bad status code"));
    }
    let code: u16 = code
        .parse()
        .map_err(|_| Unhealthy::Protocol("bad status code"))?;
    if !(100..=999).contains(&code) {
        return Err(Unhealthy::Protocol("bad status code"));
    }
    Ok(code)
}

fn read_headers(reader: &mut impl BufRead) -> Result<Vec<(String, String)>, Unhealthy> {
    let mut headers: Vec<(String, String)> = Vec::new();
    loop {
        let line = read_line(reader)?;
        if line.is_empty() || line == b"\r\n" || line == b"\n" {
            return Ok(headers);
        }
        if headers.len() >= MAX_HEADERS {
            return Err(Unhealthy::Protocol("too many headers"));
        }
        let text = String::from_utf8_lossy(&line);
        let text = text.trim_end_matches(['\r', '\n']);
        if text.starts_with([' ', '\t']) {
            // A folded continuation belongs to the previous header.
            if let Some((_, value)) = headers.last_mut() {
                value.push(' ');
                value.push_str(text.trim());
            }
            continue;
        }
        if let Some((name, value)) = text.split_once(':') {
            headers.push((name.trim().to_string(), value.trim().to_string()));
        } else {
            headers.push((text.to_string(), String::new()));
        }
    }
}

/// Probe `url` the way `urlopen(url, timeout)` would; Ok when the reference would return.
pub fn probe(url: &str, timeout: Duration) -> Result<u16, Unhealthy> {
    let mut target = parse(url)?;
    let mut visited: Vec<String> = Vec::new();
    loop {
        let (status, location) = exchange(&target, timeout)?;
        if (200..300).contains(&status) {
            return Ok(status);
        }
        if !matches!(status, 301 | 302 | 303 | 307 | 308) {
            return Err(Unhealthy::Status(status));
        }
        let Some(location) = location else {
            return Err(Unhealthy::Status(status));
        };
        let Some(next) = join(&target, &location)? else {
            return Err(Unhealthy::Redirect("not plain http"));
        };
        let next_url = next.url();
        if visited.iter().filter(|seen| **seen == next_url).count() >= MAX_REPEATS
            || visited.len() >= MAX_REDIRECTIONS
        {
            return Err(Unhealthy::Redirect("loop"));
        }
        visited.push(next_url);
        target = next;
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn urls() {
        let t = parse("http://127.0.0.1:8031/health").unwrap();
        assert_eq!(
            (t.host.as_str(), t.port, t.path.as_str()),
            ("127.0.0.1", 8031, "/health")
        );
        assert_eq!(parse("HTTP://h").unwrap().path, "/");
        assert_eq!(parse("http://h?x=1#frag").unwrap().path, "/?x=1");
        assert_eq!(parse("http://h:/p").unwrap().port, 80);
        assert_eq!(parse("http://[::1]:9/p").unwrap().host, "[::1]");
        for bad in [
            "https://h/",
            "ftp://h/",
            "http://",
            "http://u@h/",
            "http://h:x/",
            "http://h:99999/",
            "http://h/a b",
            "h/health",
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn default_url_follows_the_server_port() {
        assert_eq!(default_url(None).unwrap(), "http://127.0.0.1:8031/health");
        assert_eq!(
            default_url(Some("9000")).unwrap(),
            "http://127.0.0.1:9000/health"
        );
        assert!(default_url(Some("nine")).is_err());
        assert!(default_url(Some("70000")).is_err());
    }

    #[test]
    fn redirects_resolve_like_urljoin() {
        let base = parse("http://h:1/a/b/c?q").unwrap();
        let at = |l: &str| join(&base, l).unwrap().map(|t| t.url());
        assert_eq!(at("/x").as_deref(), Some("http://h:1/x"));
        assert_eq!(at("d").as_deref(), Some("http://h:1/a/b/d"));
        assert_eq!(at("../d").as_deref(), Some("http://h:1/a/d"));
        assert_eq!(at("./").as_deref(), Some("http://h:1/a/b/"));
        assert_eq!(at("?z").as_deref(), Some("http://h:1/a/b/c?z"));
        assert_eq!(at("//o:2/p").as_deref(), Some("http://o:2/p"));
        assert_eq!(at("http://o/p").as_deref(), Some("http://o:80/p"));
        assert_eq!(at("https://o/p"), None);
        assert_eq!(at("FTP://o/p"), None);
        assert!(join(&base, "file:///etc/passwd").is_err());
        assert!(join(&base, "javascript:x").is_err());
    }

    #[test]
    fn status_lines() {
        assert_eq!(status_of(b"HTTP/1.1 200 OK\r\n"), Ok(200));
        assert_eq!(status_of(b"HTTP/1.0 503\r\n"), Ok(503));
        assert_eq!(status_of(b"HTTP/1.1 204\n"), Ok(204));
        assert_eq!(status_of(b"HTTP/1.1 \t 299  Fine\r\n"), Ok(299));
        for bad in [
            &b"HTTP/2.0 200 OK\r\n"[..],
            b"ICY 200 OK\r\n",
            b"HTTP/1.1 20 OK\r\n",
            b"HTTP/1.1 2000 OK\r\n",
            b"HTTP/1.1 OK\r\n",
            b"\r\n",
            b"HTTP/1.1 099 x\r\n",
            b"garbage\r\n",
        ] {
            assert!(status_of(bad).is_err(), "{}", String::from_utf8_lossy(bad));
        }
    }
}
