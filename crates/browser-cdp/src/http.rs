//! Where the browser engine is: `NOEVIA_BROWSER_ENGINE` as `http://host:port` (the browser's
//! WebSocket found with `GET /json/version`) or `ws://host:port/devtools/browser/<id>`, and the tiny
//! HTTP/1.1 GET that reads `/json/version`.

use crate::ws;
use std::io::{self, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream, ToSocketAddrs};
use std::time::Duration;

const MAX_BODY: usize = 1 << 20;

/// A parsed engine address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Endpoint {
    /// `true`: `http://`, the WebSocket path comes from `/json/version`.
    pub discover: bool,
    pub host: String,
    pub port: u16,
    /// The WebSocket path (and query) for `ws://`.
    pub path: String,
}

pub(crate) fn parse_endpoint(raw: &str) -> Result<Endpoint, String> {
    let url = url::Url::parse(raw.trim())
        .map_err(|e| format!("The browser engine address is not a URL ({e})."))?;
    let discover = match url.scheme() {
        "http" => true,
        "ws" => false,
        "https" | "wss" => {
            return Err("The browser engine address must be ws:// or http://: the engine is on the deployment's private network, so TLS is not supported.".into())
        }
        other => return Err(format!("The browser engine address must be ws:// or http://, not {other}://.")),
    };
    let host = url
        .host_str()
        .filter(|h| !h.is_empty())
        .ok_or("The browser engine address has no host.")?;
    let port = url
        .port_or_known_default()
        .ok_or("The browser engine address has no port.")?;
    let mut path = url.path().to_string();
    if let Some(q) = url.query() {
        path = format!("{path}?{q}");
    }
    Ok(Endpoint {
        discover,
        host: host.to_string(),
        port,
        path,
    })
}

impl Endpoint {
    /// The socket address, and the Host header to send: the endpoint's own `host:port` when the host
    /// is an IP literal or `localhost`, else the resolved IP's (Chromium's DevTools server refuses
    /// any other Host value).
    pub(crate) fn resolve(&self) -> Result<(SocketAddr, String), String> {
        let bare = self.host.trim_start_matches('[').trim_end_matches(']');
        let addr = (bare, self.port)
            .to_socket_addrs()
            .map_err(|e| {
                format!(
                    "The browser engine host {} does not resolve ({e}).",
                    self.host
                )
            })?
            .next()
            .ok_or_else(|| format!("The browser engine host {} does not resolve.", self.host))?;
        let literal = bare.parse::<IpAddr>().is_ok() || bare.eq_ignore_ascii_case("localhost");
        let host = if literal {
            format!("{}:{}", self.host, self.port)
        } else {
            addr.to_string()
        };
        Ok((addr, host))
    }
}

/// The `webSocketDebuggerUrl` of a `/json/version` body.
pub(crate) fn version_ws_url(body: &[u8]) -> Result<String, String> {
    let text = std::str::from_utf8(body)
        .map_err(|_| "The browser engine's /json/version is not UTF-8.")?;
    let v = js_json::parse(text).map_err(|_| "The browser engine's /json/version is not JSON.")?;
    match v.get("webSocketDebuggerUrl").as_str() {
        Some(u) if !u.is_empty() => Ok(u.to_string()),
        _ => Err("The browser engine's /json/version has no webSocketDebuggerUrl.".into()),
    }
}

/// The status and body of a complete response, `None` while more is needed. `eof`: the server
/// closed, so a body without a length ends here.
pub(crate) fn parse_response(raw: &[u8], eof: bool) -> Result<Option<(u16, Vec<u8>)>, String> {
    let Some(end) = raw.windows(4).position(|w| w == b"\r\n\r\n") else {
        return if eof {
            Err("The browser engine closed before answering.".into())
        } else {
            Ok(None)
        };
    };
    let head = std::str::from_utf8(raw.get(..end + 4).unwrap_or_default())
        .map_err(|_| "Malformed HTTP head.")?;
    let body = raw.get(end + 4..).unwrap_or_default();
    let status = ws::status(head).ok_or("Malformed HTTP status line.")?;
    if ws::header(head, "transfer-encoding")
        .is_some_and(|t| t.to_ascii_lowercase().contains("chunked"))
    {
        return match dechunk(body)? {
            Some(b) => Ok(Some((status, b))),
            None if eof => Err("The browser engine closed inside a chunked body.".into()),
            None => Ok(None),
        };
    }
    if let Some(len) = ws::header(head, "content-length") {
        let len: usize = len.parse().map_err(|_| "Malformed Content-Length.")?;
        return match body.get(..len) {
            Some(b) => Ok(Some((status, b.to_vec()))),
            None if eof => Err("The browser engine closed inside the body.".into()),
            None => Ok(None),
        };
    }
    Ok(eof.then(|| (status, body.to_vec())))
}

/// A whole chunked body, `None` while incomplete.
fn dechunk(mut rest: &[u8]) -> Result<Option<Vec<u8>>, String> {
    let mut out = Vec::new();
    loop {
        let Some(line_end) = rest.windows(2).position(|w| w == b"\r\n") else {
            return Ok(None);
        };
        let line = std::str::from_utf8(rest.get(..line_end).unwrap_or_default())
            .map_err(|_| "Malformed chunk size.")?;
        let size = usize::from_str_radix(line.split(';').next().unwrap_or("").trim(), 16)
            .map_err(|_| "Malformed chunk size.")?;
        rest = rest.get(line_end + 2..).unwrap_or_default();
        if size == 0 {
            return Ok(Some(out));
        }
        let (Some(chunk), Some(after)) = (rest.get(..size), rest.get(size..size + 2)) else {
            return Ok(None);
        };
        if after != b"\r\n" {
            return Err("Malformed chunk.".into());
        }
        out.extend_from_slice(chunk);
        rest = rest.get(size + 2..).unwrap_or_default();
    }
}

/// `GET path` with this Host header; `Connection: close`.
pub(crate) fn get(
    addr: SocketAddr,
    host: &str,
    path: &str,
    timeout: Duration,
) -> io::Result<(u16, Vec<u8>)> {
    let mut stream = TcpStream::connect_timeout(&addr, timeout)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    stream.write_all(format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nAccept: application/json\r\nConnection: close\r\n\r\n").as_bytes())?;
    let mut raw = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = stream.read(&mut buf)?;
        raw.extend_from_slice(buf.get(..n).unwrap_or_default());
        if raw.len() > MAX_BODY {
            return Err(ws::bad("The browser engine's answer is too large"));
        }
        if let Some(done) = parse_response(&raw, n == 0).map_err(ws::bad)? {
            return Ok(done);
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;

    #[test]
    fn endpoints() {
        let e = parse_endpoint("http://browser:9222").unwrap();
        assert_eq!(
            e,
            Endpoint {
                discover: true,
                host: "browser".into(),
                port: 9222,
                path: "/".into()
            }
        );
        let e = parse_endpoint("ws://10.0.0.5:9222/devtools/browser/abc-1?x=1").unwrap();
        assert_eq!(
            (e.discover, e.port, e.path.as_str()),
            (false, 9222, "/devtools/browser/abc-1?x=1")
        );
        assert_eq!(parse_endpoint("http://browser").unwrap().port, 80);
        for refused in [
            "wss://browser:9222/devtools/browser/x",
            "https://browser:9222",
        ] {
            assert!(
                parse_endpoint(refused)
                    .unwrap_err()
                    .contains("ws:// or http://"),
                "{refused}"
            );
        }
        assert!(parse_endpoint("ftp://browser").is_err());
        assert!(parse_endpoint("not a url").is_err());
        let (addr, host) = parse_endpoint("http://127.0.0.1:9222")
            .unwrap()
            .resolve()
            .unwrap();
        assert_eq!(
            (addr.to_string().as_str(), host.as_str()),
            ("127.0.0.1:9222", "127.0.0.1:9222")
        );
        let (_, host) = parse_endpoint("ws://[::1]:9222/x")
            .unwrap()
            .resolve()
            .unwrap();
        assert_eq!(host, "[::1]:9222");
    }

    #[test]
    fn json_version() {
        let body = br#"{"Browser":"QA/1.0","Protocol-Version":"1.3","webSocketDebuggerUrl":"ws://127.0.0.1:9222/devtools/browser/0f1e"}"#;
        assert_eq!(
            version_ws_url(body).unwrap(),
            "ws://127.0.0.1:9222/devtools/browser/0f1e"
        );
        assert!(version_ws_url(br#"{"Browser":"QA"}"#).is_err());
        assert!(version_ws_url(b"<html>").is_err());
    }

    #[test]
    fn responses() {
        let full = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello";
        assert_eq!(
            parse_response(full, false).unwrap(),
            Some((200, b"hello".to_vec()))
        );
        assert_eq!(
            parse_response(&full[..full.len() - 2], false).unwrap(),
            None
        );
        assert!(parse_response(&full[..full.len() - 2], true).is_err());
        let chunked = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\nWiki\r\n6;x=y\r\npedia \r\n0\r\n\r\n";
        assert_eq!(
            parse_response(chunked, false).unwrap(),
            Some((200, b"Wikipedia ".to_vec()))
        );
        assert_eq!(parse_response(&chunked[..60], false).unwrap(), None);
        assert!(parse_response(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\n",
            false
        )
        .is_err());
        assert_eq!(
            parse_response(b"HTTP/1.0 404 Not Found\r\n\r\nnope", false).unwrap(),
            None
        );
        assert_eq!(
            parse_response(b"HTTP/1.0 404 Not Found\r\n\r\nnope", true).unwrap(),
            Some((404, b"nope".to_vec()))
        );
        assert_eq!(parse_response(b"HTTP/1.1 200", false).unwrap(), None);
    }
}
