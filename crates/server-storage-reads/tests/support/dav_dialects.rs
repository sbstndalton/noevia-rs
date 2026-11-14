//! A synthetic, stateful WebDAV server that answers in one of two server dialects, so the storage
//! client is proven against both without a real server (track S1). Every answer is written from the
//! servers' documented behaviour; nothing here was recorded from a real server, and every name is
//! synthetic.
//!
//! - [`Dialect::Files`]: the home at `/remote.php/dav/files/<user>/`. Hrefs percent-encode every
//!   byte but `A-Za-z0-9-_.~` in each segment; XML text escapes `"` as `&quot;`; a write answers
//!   `ETag` and `OC-ETag`; errors carry an `<s:exception>` body.
//! - [`Dialect::Spaces`]: the home at `/dav/spaces/<storage id>$<space id>/`, also reachable at the
//!   legacy `/remote.php/dav/files/<user>/` (hrefs follow the address the request used). Hrefs keep
//!   `$&+:=@` and percent-encode the rest as a path-segment encoder does, so `&` arrives as `&amp;`;
//!   XML text escapes `"` as `&#34;` and `'` as `&#39;`, as an XML encoder that writes numeric
//!   references does; a property a resource does not have comes back in a `404` propstat (empty
//!   element); a write answers `ETag`, `OC-ETag` and `OC-FileId`.
//!
//! Both: MKCOL `405` on an existing resource and `409` without a parent; PUT `409` without a parent,
//! `412` for `If-None-Match: *` on an existing file or an `If-Match` that differs; DELETE `412` for
//! an `If-Match` that differs (a collection's ETag included); `404` for a missing resource;
//! collection ETags change when anything below them changes. Basic `qa-user:qa-token` only (else
//! `401`).
#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::{Arc, Mutex},
    time::Duration,
};

pub const USER: &str = "qa-user";
pub const TOKEN: &str = "qa-token";
/// `Basic base64("qa-user:qa-token")`.
const AUTHORIZATION: &str = "Basic cWEtdXNlcjpxYS10b2tlbg==";
pub const FILES_HOME: &str = "/remote.php/dav/files/qa-user/";
/// A synthetic personal space id: `<storage id>$<space id>`.
pub const SPACE_ID: &str =
    "1284d238-aa92-42ce-bdc4-0b0000009157$4c510ada-c86b-4815-8820-42cdf82c3d51";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dialect {
    Files,
    Spaces,
}

impl Dialect {
    /// The WebDAV home paths this dialect serves (raw, as a client may write them).
    fn homes(self) -> Vec<String> {
        match self {
            Self::Files => vec![FILES_HOME.to_owned()],
            Self::Spaces => vec![
                format!("/dav/spaces/{SPACE_ID}/"),
                format!("/dav/spaces/{}/", SPACE_ID.replace('$', "%24")),
                FILES_HOME.to_owned(),
            ],
        }
    }
    /// The home as this dialect writes it in an href, for a request that came in at `home`.
    fn href_home(self, home: &str) -> String {
        match self {
            Self::Files => FILES_HOME.to_owned(),
            Self::Spaces if home == FILES_HOME => FILES_HOME.to_owned(),
            Self::Spaces => format!("/dav/spaces/{SPACE_ID}/"),
        }
    }
    fn encode_segment(self, segment: &str) -> String {
        let keep: &[u8] = match self {
            Self::Files => b"-_.~",
            Self::Spaces => b"-_.~$&+:=@",
        };
        let mut out = String::new();
        for b in segment.bytes() {
            if b.is_ascii_alphanumeric() || keep.contains(&b) {
                out.push(char::from(b));
            } else {
                out.push_str(&format!("%{b:02X}"));
            }
        }
        out
    }
    /// XML character data as the dialect's writer escapes it.
    fn text(self, s: &str) -> String {
        let mut out = String::new();
        for c in s.chars() {
            match (self, c) {
                (_, '&') => out.push_str("&amp;"),
                (_, '<') => out.push_str("&lt;"),
                (_, '>') => out.push_str("&gt;"),
                (Self::Files, '"') => out.push_str("&quot;"),
                (Self::Spaces, '"') => out.push_str("&#34;"),
                (Self::Spaces, '\'') => out.push_str("&#39;"),
                (_, c) => out.push(c),
            }
        }
        out
    }
    fn multistatus_open(self) -> &'static str {
        match self {
            Self::Files => "<?xml version=\"1.0\"?>\n<d:multistatus xmlns:d=\"DAV:\" xmlns:s=\"http://sabredav.org/ns\" xmlns:oc=\"http://owncloud.org/ns\" xmlns:nc=\"http://nextcloud.org/ns\">",
            Self::Spaces => "<d:multistatus xmlns:s=\"http://sabredav.org/ns\" xmlns:d=\"DAV:\" xmlns:oc=\"http://owncloud.org/ns\">",
        }
    }
    fn error_body(self, exception: &str, message: &str) -> String {
        match self {
            Self::Files => format!(
                "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<d:error xmlns:d=\"DAV:\" xmlns:s=\"http://sabredav.org/ns\">\n  <s:exception>Sabre\\DAV\\Exception\\{exception}</s:exception>\n  <s:message>{message}</s:message>\n</d:error>\n"
            ),
            Self::Spaces => format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?><d:error xmlns:d=\"DAV:\" xmlns:s=\"http://sabredav.org/ns\"><s:exception>Sabre\\DAV\\Exception\\{exception}</s:exception><s:message>{message}</s:message></d:error>"
            ),
        }
    }
}

#[derive(Default)]
pub struct Dav {
    /// Decoded path below the home -> (bytes, version number).
    pub files: BTreeMap<String, (Vec<u8>, u64)>,
    pub dirs: BTreeSet<String>,
    pub next: u64,
    /// (method, raw request target) of every request that reached the server.
    pub seen: Vec<(String, String)>,
}

impl Dav {
    fn etag(&self, n: u64) -> String {
        format!("\"{:016x}\"", n.wrapping_mul(0x9e37_79b9_7f4a_7c15))
    }
    /// A collection's ETag: changes whenever anything below it does (both servers propagate).
    fn dir_etag(&self, key: &str) -> String {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        let below = format!("{key}/");
        let mut feed = |s: &str| {
            for b in s.bytes().chain([0]) {
                h ^= u64::from(b);
                h = h.wrapping_mul(0x0100_0000_01b3);
            }
        };
        feed(key);
        for (k, (_, n)) in &self.files {
            if key.is_empty() || k.starts_with(&below) {
                feed(k);
                feed(&n.to_string());
            }
        }
        for d in &self.dirs {
            if key.is_empty() || d.starts_with(&below) {
                feed(d);
            }
        }
        format!("\"{h:016x}\"")
    }
    fn children(&self, key: &str) -> Vec<(String, bool)> {
        let parent_of = |k: &str| k.rsplit_once('/').map_or("", |(p, _)| p).to_owned();
        let mut out: Vec<(String, bool)> = self
            .dirs
            .iter()
            .filter(|d| !d.is_empty() && parent_of(d) == key)
            .map(|d| (d.clone(), true))
            .collect();
        out.extend(
            self.files
                .keys()
                .filter(|f| parent_of(f) == key)
                .map(|f| (f.clone(), false)),
        );
        out
    }
}

fn decode(p: &str) -> String {
    let b = p.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 3 <= b.len() {
            out.push(u8::from_str_radix(&p[i + 1..i + 3], 16).unwrap());
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap()
}

fn respond(stream: &mut TcpStream, status: u16, headers: &str, body: &[u8]) {
    let mut out = format!(
        "HTTP/1.1 {status} Synthetic\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n",
        body.len()
    )
    .into_bytes();
    out.extend_from_slice(body);
    let _ = stream.write_all(&out);
}

fn error(stream: &mut TcpStream, dialect: Dialect, status: u16, exception: &str, message: &str) {
    respond(
        stream,
        status,
        "Content-Type: application/xml; charset=utf-8\r\n",
        dialect.error_body(exception, message).as_bytes(),
    );
}

fn response_block(dav: &Dav, dialect: Dialect, home: &str, key: &str, dir: bool) -> String {
    let mut href = dialect.href_home(home);
    let encoded: Vec<String> = key
        .split('/')
        .filter(|s| !s.is_empty())
        .map(|s| dialect.encode_segment(s))
        .collect();
    href.push_str(&encoded.join("/"));
    if dir && !key.is_empty() {
        href.push('/');
    }
    let etag = if dir {
        dav.dir_etag(key)
    } else {
        dav.etag(dav.files[key].1)
    };
    let length = (!dir).then(|| dav.files[key].0.len());
    let resourcetype = if dir { "<d:collection/>" } else { "" };
    let lastmod = "Sat, 14 Nov 2026 12:00:00 GMT";
    match dialect {
        Dialect::Files => {
            let mut prop = format!(
                "<d:getlastmodified>{lastmod}</d:getlastmodified><d:resourcetype>{resourcetype}</d:resourcetype><d:getetag>{}</d:getetag>",
                dialect.text(&etag)
            );
            match length {
                Some(n) => prop.push_str(&format!(
                    "<d:getcontentlength>{n}</d:getcontentlength><d:getcontenttype>text/markdown</d:getcontenttype>"
                )),
                None => prop.push_str(
                    "<d:quota-used-bytes>0</d:quota-used-bytes><d:quota-available-bytes>-3</d:quota-available-bytes>",
                ),
            }
            format!(
                "<d:response><d:href>{}</d:href><d:propstat><d:prop>{prop}</d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>",
                dialect.text(&href)
            )
        }
        Dialect::Spaces => {
            let id = format!("{SPACE_ID}!{:08x}", key.len());
            let mut prop = format!(
                "<oc:id>{id}</oc:id><oc:fileid>{id}</oc:fileid><oc:spaceid>{SPACE_ID}</oc:spaceid><oc:permissions>RDNVWZP</oc:permissions><d:getetag>{}</d:getetag><d:resourcetype>{resourcetype}</d:resourcetype>",
                dialect.text(&etag)
            );
            let missing = match length {
                Some(n) => {
                    prop.push_str(&format!(
                        "<d:getcontentlength>{n}</d:getcontentlength><oc:size>{n}</oc:size><d:getcontenttype>text/markdown</d:getcontenttype>"
                    ));
                    String::new()
                }
                None => {
                    prop.push_str("<oc:size>0</oc:size>");
                    "<d:propstat><d:prop><d:getcontentlength></d:getcontentlength><d:getcontenttype></d:getcontenttype></d:prop><d:status>HTTP/1.1 404 Not Found</d:status></d:propstat>".to_owned()
                }
            };
            prop.push_str(&format!("<d:getlastmodified>{lastmod}</d:getlastmodified>"));
            format!(
                "<d:response><d:href>{}</d:href><d:propstat><d:prop>{prop}</d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>{missing}</d:response>",
                dialect.text(&href)
            )
        }
    }
}

fn handle(mut stream: TcpStream, dialect: Dialect, dav: &Mutex<Dav>) {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut raw = Vec::new();
    let mut buf = [0u8; 65536];
    let head_end = loop {
        if let Some(at) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
            break at;
        }
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => raw.extend_from_slice(&buf[..n]),
        }
    };
    let head = String::from_utf8_lossy(&raw[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut first = lines.next().unwrap().split(' ');
    let method = first.next().unwrap().to_owned();
    let target = first.next().unwrap().to_owned();
    let headers: BTreeMap<String, String> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_owned()))
        .collect();
    let length: usize = headers
        .get("content-length")
        .map_or(0, |v| v.parse().unwrap());
    let mut body = raw[head_end + 4..].to_vec();
    while body.len() < length {
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => body.extend_from_slice(&buf[..n]),
        }
    }
    let mut d = dav.lock().unwrap();
    d.seen.push((method.clone(), target.clone()));
    if headers.get("authorization").map(String::as_str) != Some(AUTHORIZATION) {
        let challenge = match dialect {
            Dialect::Files => "WWW-Authenticate: Basic realm=\"QA\", charset=\"UTF-8\"\r\n",
            Dialect::Spaces => "WWW-Authenticate: Basic realm=\"\", charset=\"UTF-8\"\r\n",
        };
        let body = dialect.error_body("NotAuthenticated", "No public access to this resource.");
        return respond(&mut stream, 401, challenge, body.as_bytes());
    }
    let homes = dialect.homes();
    let Some((home, rest)) = homes
        .iter()
        .find_map(|h| target.strip_prefix(h.as_str()).map(|r| (h.as_str(), r)))
        .or_else(|| {
            homes
                .iter()
                .find(|h| target == h.trim_end_matches('/'))
                .map(|h| (h.as_str(), ""))
        })
    else {
        return error(&mut stream, dialect, 404, "NotFound", "Resource not found");
    };
    let home = if home.contains("%24") {
        format!("/dav/spaces/{SPACE_ID}/")
    } else {
        home.to_owned()
    };
    let key = decode(rest).trim_end_matches('/').to_owned();
    let parent = key.rsplit_once('/').map_or("", |(p, _)| p).to_owned();
    let parent_ok = parent.is_empty() || d.dirs.contains(&parent);
    let is_dir = key.is_empty() || d.dirs.contains(&key);
    let if_match = headers.get("if-match").cloned();
    match method.as_str() {
        "MKCOL" => {
            if is_dir || d.files.contains_key(&key) {
                error(
                    &mut stream,
                    dialect,
                    405,
                    "MethodNotAllowed",
                    "The resource you tried to create already exists",
                )
            } else if !parent_ok {
                error(
                    &mut stream,
                    dialect,
                    409,
                    "Conflict",
                    "Parent node does not exist",
                )
            } else {
                d.dirs.insert(key);
                respond(&mut stream, 201, "", b"")
            }
        }
        "PUT" => {
            if !parent_ok {
                return error(
                    &mut stream,
                    dialect,
                    409,
                    "Conflict",
                    "Parent node does not exist",
                );
            }
            if is_dir {
                return error(
                    &mut stream,
                    dialect,
                    409,
                    "Conflict",
                    "A collection exists at this path",
                );
            }
            let existing = d.files.get(&key).map(|(_, n)| *n);
            let current = existing.map(|n| d.etag(n));
            if headers.get("if-none-match").map(String::as_str) == Some("*") && existing.is_some() {
                return error(
                    &mut stream,
                    dialect,
                    412,
                    "PreconditionFailed",
                    "An If-None-Match header was specified, but the ETag matched",
                );
            }
            if let Some(m) = &if_match {
                if current.as_ref() != Some(m) {
                    return error(
                        &mut stream,
                        dialect,
                        412,
                        "PreconditionFailed",
                        "An If-Match header was specified, but none of the specified ETags matched",
                    );
                }
            }
            d.next += 1;
            let n = d.next;
            d.files.insert(key.clone(), (body, n));
            let etag = d.etag(n);
            let status = if existing.is_some() { 204 } else { 201 };
            let headers = match dialect {
                Dialect::Files => format!("ETag: {etag}\r\nOC-ETag: {etag}\r\nOC-FileId: {n:08}ocqa\r\n"),
                Dialect::Spaces => format!(
                    "ETag: {etag}\r\nOC-ETag: {etag}\r\nOC-FileId: {SPACE_ID}!{n:08x}\r\nLast-Modified: Sat, 14 Nov 2026 12:00:00 GMT\r\n"
                ),
            };
            respond(&mut stream, status, &headers, b"")
        }
        "GET" => {
            let Some((bytes, n)) = d.files.get(&key) else {
                return error(&mut stream, dialect, 404, "NotFound", "Resource not found");
            };
            let headers = format!(
                "ETag: {}\r\nContent-Type: text/markdown\r\nLast-Modified: Sat, 14 Nov 2026 12:00:00 GMT\r\n",
                d.etag(*n)
            );
            respond(&mut stream, 200, &headers, bytes)
        }
        "DELETE" => {
            if let Some((_, n)) = d.files.get(&key) {
                if if_match.is_some_and(|m| m != d.etag(*n)) {
                    return error(
                        &mut stream,
                        dialect,
                        412,
                        "PreconditionFailed",
                        "An If-Match header was specified, but none of the specified ETags matched",
                    );
                }
                d.files.remove(&key);
                respond(&mut stream, 204, "", b"")
            } else if !key.is_empty() && d.dirs.contains(&key) {
                if if_match.is_some_and(|m| m != d.dir_etag(&key)) {
                    return error(
                        &mut stream,
                        dialect,
                        412,
                        "PreconditionFailed",
                        "An If-Match header was specified, but none of the specified ETags matched",
                    );
                }
                let below = format!("{key}/");
                d.files.retain(|k, _| !k.starts_with(&below));
                d.dirs.retain(|k| k != &key && !k.starts_with(&below));
                respond(&mut stream, 204, "", b"")
            } else {
                error(&mut stream, dialect, 404, "NotFound", "Resource not found")
            }
        }
        "PROPFIND" => {
            let depth = headers.get("depth").cloned().unwrap_or_default();
            let mut xml = String::from(dialect.multistatus_open());
            if d.files.contains_key(&key) {
                xml.push_str(&response_block(&d, dialect, &home, &key, false));
            } else if is_dir {
                xml.push_str(&response_block(&d, dialect, &home, &key, true));
                if depth == "1" {
                    for (child, dir) in d.children(&key) {
                        xml.push_str(&response_block(&d, dialect, &home, &child, dir));
                    }
                }
            } else {
                return error(&mut stream, dialect, 404, "NotFound", "Resource not found");
            }
            xml.push_str("</d:multistatus>");
            respond(
                &mut stream,
                207,
                "Content-Type: application/xml; charset=utf-8\r\n",
                xml.as_bytes(),
            )
        }
        _ => error(
            &mut stream,
            dialect,
            405,
            "MethodNotAllowed",
            "Method not allowed",
        ),
    }
}

/// A server for `dialect` on a free loopback port.
pub fn server(dialect: Dialect) -> (u16, Arc<Mutex<Dav>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let dav = Arc::new(Mutex::new(Dav::default()));
    let shared = Arc::clone(&dav);
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let shared = Arc::clone(&shared);
            std::thread::spawn(move || handle(stream, dialect, &shared));
        }
    });
    (port, dav)
}
