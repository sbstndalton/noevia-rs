//! A small blocking HTTP/1.1 client: one request per connection (`Connection: close`), plain
//! `http://` only. Replay targets a server on this machine or a test network; TLS is out of scope.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

/// Response size cap: a corpus is about contracts, not bulk downloads.
pub const MAX_RESPONSE: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Base {
    pub host: String,
    pub port: u16,
}

impl Base {
    /// Parses `http://host[:port]` (a trailing slash or path is refused: the corpus holds paths).
    pub fn parse(url: &str) -> Result<Self, String> {
        let rest = url
            .strip_prefix("http://")
            .ok_or_else(|| format!("base URL must start with http:// ({url})"))?;
        let rest = rest.trim_end_matches('/');
        if rest.is_empty() || rest.contains('/') {
            return Err(format!("base URL must be http://host[:port] ({url})"));
        }
        let (host, port) = match rest.rsplit_once(':') {
            Some((h, p)) if !h.ends_with(']') || h.starts_with('[') => (
                h.to_string(),
                p.parse::<u16>().map_err(|_| format!("bad port in {url}"))?,
            ),
            _ => (rest.to_string(), 80),
        };
        Ok(Self { host, port })
    }

    pub fn origin(&self) -> String {
        if self.port == 80 {
            format!("http://{}", self.host)
        } else {
            format!("http://{}:{}", self.host, self.port)
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Response {
    pub status: u16,
    /// Lower-case names, in order, repeats kept.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
}

pub fn send(
    base: &Base,
    method: &str,
    target: &str,
    headers: &[(String, String)],
    body: &[u8],
    timeout: Duration,
) -> Result<Response, String> {
    if method.is_empty()
        || !method.bytes().all(|c| c.is_ascii_uppercase())
        || target.contains(['\r', '\n', ' '])
    {
        return Err(format!("refusing request line {method} {target:?}"));
    }
    for (k, v) in headers {
        if k.is_empty() || k.contains(['\r', '\n', ':']) || v.contains(['\r', '\n']) {
            return Err(format!("refusing a header with a line break: {k}"));
        }
    }
    let addr = (
        base.host.trim_start_matches('[').trim_end_matches(']'),
        base.port,
    )
        .to_socket_addrs()
        .map_err(|e| format!("resolve {}: {e}", base.host))?
        .next()
        .ok_or_else(|| format!("resolve {}: no address", base.host))?;
    let mut stream =
        TcpStream::connect_timeout(&addr, timeout).map_err(|e| format!("connect {addr}: {e}"))?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|e| e.to_string())?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|e| e.to_string())?;
    let mut head = format!(
        "{method} {target} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n",
        host_header(base)
    );
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    if !body.is_empty() || !matches!(method, "GET" | "HEAD" | "DELETE" | "OPTIONS") {
        head.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    head.push_str("\r\n");
    stream
        .write_all(head.as_bytes())
        .map_err(|e| format!("send: {e}"))?;
    stream.write_all(body).map_err(|e| format!("send: {e}"))?;
    let mut raw = Vec::new();
    let read = stream.take(MAX_RESPONSE as u64 + 1).read_to_end(&mut raw);
    if raw.len() > MAX_RESPONSE {
        return Err(format!("response over {MAX_RESPONSE} bytes"));
    }
    match read {
        Ok(_) => parse_response(&raw, method == "HEAD"),
        // A server that answers before reading the whole request may reset the connection after
        // a complete response; keep the response if it is complete.
        Err(e) => parse_response(&raw, method == "HEAD").map_err(|_| format!("read: {e}")),
    }
}

fn host_header(base: &Base) -> String {
    if base.port == 80 {
        base.host.clone()
    } else {
        format!("{}:{}", base.host, base.port)
    }
}

/// Parses a complete response read to connection close.
pub fn parse_response(raw: &[u8], head_only: bool) -> Result<Response, String> {
    let mut hdrs = [httparse::EMPTY_HEADER; 128];
    let mut res = httparse::Response::new(&mut hdrs);
    let n = match res
        .parse(raw)
        .map_err(|e| format!("bad response head: {e}"))?
    {
        httparse::Status::Complete(n) => n,
        httparse::Status::Partial => {
            return Err("connection closed inside the response head".into())
        }
    };
    let status = res.code.ok_or("response without a status")?;
    let headers: Vec<(String, String)> = res
        .headers
        .iter()
        .map(|h| {
            (
                h.name.to_ascii_lowercase(),
                String::from_utf8_lossy(h.value).into_owned(),
            )
        })
        .collect();
    let rest = raw.get(n..).unwrap_or(&[]);
    let chunked = headers
        .iter()
        .any(|(k, v)| k == "transfer-encoding" && v.to_ascii_lowercase().contains("chunked"));
    let body = if head_only || status == 204 || status == 304 || (100..200).contains(&status) {
        Vec::new()
    } else if chunked {
        dechunk(rest)?
    } else if let Some(len) = headers
        .iter()
        .find(|(k, _)| k == "content-length")
        .and_then(|(_, v)| v.trim().parse::<usize>().ok())
    {
        rest.get(..len)
            .ok_or("connection closed inside the body")?
            .to_vec()
    } else {
        rest.to_vec()
    };
    Ok(Response {
        status,
        headers,
        body,
    })
}

fn dechunk(mut s: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    loop {
        let line_end = s
            .windows(2)
            .position(|w| w == b"\r\n")
            .ok_or("truncated chunk size")?;
        let line =
            std::str::from_utf8(s.get(..line_end).unwrap_or(&[])).map_err(|_| "bad chunk size")?;
        let size = usize::from_str_radix(line.split(';').next().unwrap_or("").trim(), 16)
            .map_err(|_| format!("bad chunk size {line:?}"))?;
        s = s.get(line_end + 2..).unwrap_or(&[]);
        if size == 0 {
            return Ok(out);
        }
        out.extend_from_slice(s.get(..size).ok_or("truncated chunk")?);
        s = s
            .get(size..)
            .and_then(|t| t.strip_prefix(b"\r\n"))
            .ok_or("chunk without CRLF")?;
    }
}

/// Percent-encodes a path segment or query component (RFC 3986 unreserved kept).
pub fn encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn parses_base_urls() {
        assert_eq!(
            Base::parse("http://127.0.0.1:8021").unwrap(),
            Base {
                host: "127.0.0.1".into(),
                port: 8021
            }
        );
        assert_eq!(
            Base::parse("http://localhost/").unwrap().origin(),
            "http://localhost"
        );
        assert!(Base::parse("https://x").is_err());
        assert!(Base::parse("http://x/api").is_err());
    }

    #[test]
    fn decodes_chunked_and_length_bodies() {
        let r = parse_response(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nSet-Cookie: a=1\r\nSet-Cookie: b=2\r\n\r\n3\r\nabc\r\n2;x=y\r\nde\r\n0\r\n\r\n", false).unwrap();
        assert_eq!((r.status, r.body.as_slice()), (200, b"abcde".as_slice()));
        assert_eq!(
            r.headers.iter().filter(|(k, _)| k == "set-cookie").count(),
            2
        );
        let r = parse_response(
            b"HTTP/1.1 201 Created\r\nContent-Length: 2\r\n\r\n{}extra",
            false,
        )
        .unwrap();
        assert_eq!(r.body, b"{}");
        assert!(
            parse_response(b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\n\r\nshort", false).is_err()
        );
        assert!(parse_response(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nZZ\r\n",
            false
        )
        .is_err());
    }

    #[test]
    fn encodes_components() {
        assert_eq!(encode("a b/c~d"), "a%20b%2Fc~d");
    }

    #[test]
    fn round_trips_through_a_local_server() {
        use std::net::TcpListener;
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let t = std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            let mut raw = Vec::new();
            let mut buf = [0u8; 4096];
            while !String::from_utf8_lossy(&raw).ends_with("\r\n\r\n{}") {
                let n = s.read(&mut buf).unwrap();
                assert!(n > 0, "client closed early");
                raw.extend_from_slice(buf.get(..n).unwrap());
            }
            let req = String::from_utf8_lossy(&raw).into_owned();
            s.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n\r\nok")
                .unwrap();
            req
        });
        let base = Base::parse(&format!("http://127.0.0.1:{port}")).unwrap();
        let r = send(
            &base,
            "POST",
            "/api/x?y=1",
            &[("X-CSRF-Token".into(), "t".into())],
            b"{}",
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!((r.status, r.body.as_slice()), (200, b"ok".as_slice()));
        let req = t.join().unwrap();
        assert!(req.starts_with("POST /api/x?y=1 HTTP/1.1\r\n"));
        assert!(req.contains("Content-Length: 2\r\n") && req.ends_with("\r\n\r\n{}"));
        assert!(send(
            &base,
            "GET",
            "/",
            &[("Bad".into(), "a\r\nInjected: 1".into())],
            b"",
            Duration::from_secs(1)
        )
        .is_err());
    }
}
