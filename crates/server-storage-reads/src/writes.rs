//! The storage writes (sbstndalton/noevia#2191 round 3): core storage-client.cjs `createFolder`,
//! `writeFile`, `discardWritten`, `deleteFile`, `removeEmptyFolder` and `fileVersion`, answer for
//! answer, over the same [`Transport`] as the reads (every attempt in a fresh owned child; no
//! redirects, proxies or cookies; the endpoint policy is the caller's, as for the reads).
//!
//! Path contract (as core's): every path is connection-absolute, checked by `safeRelativePath`
//! (storage-path), encoded per segment like `davUrl`; nothing here joins `corpusRoot`. S3 is refused
//! for every write exactly where core refuses it (core has no S3 write).
//!
//! Retry (core `withRetry`): one repeat after 250 ms of a transport failure undici reports as
//! `fetch failed` (no connection, DNS, TLS, a refused redirect, a reset before the headers); a
//! timeout and every HTTP answer are final. A conditional PUT is never repeated (#655), and a
//! transport failure of one is `code: unknown` (it may have landed). `discardWritten` and the
//! empty-folder DELETE are never repeated either. Core has no 428 (credential) retry for storage
//! writes; neither does this.
//!
//! Each attempt's deadline is core's `AbortSignal.timeout` for that request: PUT 60 s, DELETE 30 s
//! (15 s for an empty folder), MKCOL and PROPFIND 15 s.
use crate::protocol::{
    self, Connection, Kind, Method, Request, Response, Transport, TransportFailure,
};
use base64::Engine;
use std::time::Duration;
use zeroize::Zeroizing;

/// core's thrown storage error: `status` is the route's answer, `upstream` what storage said.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StorageError {
    pub status: u16,
    pub message: String,
    pub code: Option<&'static str>,
    pub upstream: Option<u16>,
}

impl StorageError {
    pub(crate) fn new(status: u16, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
            code: None,
            upstream: None,
        }
    }
    pub(crate) fn code(mut self, code: &'static str) -> Self {
        self.code = Some(code);
        self
    }
    pub(crate) fn upstream(mut self, status: u16) -> Self {
        self.upstream = Some(status);
        self
    }
    /// core `refusedLogin(err)`: only a 401 from storage (#849, #1428).
    pub fn refused_login(&self) -> bool {
        self.upstream == Some(401)
    }
}

impl std::fmt::Display for StorageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// core `transportError`: a timeout is 504 `timeout`, the rest 502 `unreachable`.
pub(crate) fn transport_error(failure: TransportFailure) -> StorageError {
    match failure {
        TransportFailure::Timeout => {
            StorageError::new(504, "the storage server did not answer in time").code("timeout")
        }
        TransportFailure::BodyTimeout => {
            StorageError::new(504, "the storage server did not answer in time").code("timeout")
        }
        _ => StorageError::new(502, "the storage server could not be reached").code("unreachable"),
    }
}

/// A write's precondition (core writeFile's `ifMatch` / `ifNoneMatch: '*'`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Condition {
    None,
    IfMatch(String),
    /// Create-only (#687): nothing may exist at the path.
    IfNoneMatch,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Written {
    pub path: String,
    /// The ETag of what was written, '' when the server sent none.
    pub etag: String,
    /// Only a 201: a server that ignores If-None-Match answers an overwrite with 200/204.
    pub created: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Folder {
    pub path: String,
    pub existed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Version {
    Missing,
    /// '' when the server reports no ETag.
    Present(String),
}

fn kind_dav(c: &Connection) -> bool {
    matches!(c.kind, Kind::WebDav | Kind::Nextcloud)
}

fn safe(raw: &str) -> Option<String> {
    storage_path::safe_relative_path(raw)
        .ok()
        .filter(|p| !p.is_empty())
}

/// core `davUrl(conn, path, trailingSlash)`.
pub fn dav_url(c: &Connection, path: &str, trailing: bool) -> String {
    let base = c.base_url.trim_end_matches('/');
    let encoded = path
        .split('/')
        .filter(|s| !s.is_empty())
        .map(protocol::encode_component)
        .collect::<Vec<_>>()
        .join("/");
    format!("{base}/{encoded}{}", if trailing { "/" } else { "" })
}

/// core `quoteEtag`.
pub fn quote_etag(etag: &str) -> String {
    if etag.starts_with('"') || etag.starts_with("W/") {
        etag.to_owned()
    } else {
        format!("\"{etag}\"")
    }
}

fn bad_value(v: &str) -> bool {
    v.is_empty() || v.contains('\r') || v.contains('\n')
}

const PROPFIND_QUERY: &str = "<?xml version=\"1.0\"?><d:propfind xmlns:d=\"DAV:\"><d:prop><d:resourcetype/><d:getetag/></d:prop></d:propfind>";

fn request(
    c: &Connection,
    method: Method,
    url: String,
    extra: Vec<(&str, String)>,
    body: Vec<u8>,
    timeout: Duration,
) -> Request {
    let mut headers: Vec<(String, Zeroizing<String>)> = extra
        .into_iter()
        .map(|(k, v)| (k.to_owned(), Zeroizing::new(v)))
        .collect();
    if method == Method::Put || method == Method::Propfind {
        headers.push((
            "Content-Length".into(),
            Zeroizing::new(body.len().to_string()),
        ));
    }
    if !c.username.is_empty() {
        let secret = String::from_utf8_lossy(&c.secret);
        let pair = Zeroizing::new(format!("{}:{secret}", c.username));
        headers.push((
            "Authorization".into(),
            Zeroizing::new(format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode(pair.as_bytes())
            )),
        ));
    }
    Request {
        method,
        url,
        headers,
        attempt_timeout: timeout,
        body_cap: if method == Method::Propfind {
            crate::LIST_BYTES
        } else {
            crate::TEXT_BYTES
        },
        body: Zeroizing::new(body),
    }
}

/// One request with core's `withRetry` (`once` = no repeat). `make` builds a fresh request per attempt.
fn send(
    transport: &mut impl Transport,
    once: bool,
    make: impl Fn() -> Request,
) -> Result<Response, TransportFailure> {
    match transport.attempt(make()) {
        Ok(r) => Ok(r),
        Err(e) if !once && e.node_retries() => {
            transport.delay(Duration::from_millis(250))?;
            transport.attempt(make())
        }
        Err(e) => Err(e),
    }
}

fn ok(status: u16) -> bool {
    (200..300).contains(&status)
}

/// storage-client.cjs `createFolder(conn, path)`: MKCOL, 405 = it exists.
pub fn create_folder(
    c: &Connection,
    raw: &str,
    t: &mut impl Transport,
) -> Result<Folder, StorageError> {
    let Some(path) = safe(raw) else {
        return Err(StorageError::new(400, "invalid folder path"));
    };
    if c.kind == Kind::S3 {
        return Err(StorageError::new(
            400,
            "S3 has no folders — a prefix appears once a file is stored under it",
        ));
    }
    let url = dav_url(c, &path, true);
    let r = send(t, false, || {
        request(
            c,
            Method::Mkcol,
            url.clone(),
            vec![],
            vec![],
            Duration::from_secs(15),
        )
    })
    .map_err(transport_error)?;
    if r.status == 405 {
        return Ok(Folder {
            path,
            existed: true,
        });
    }
    if !ok(r.status) {
        return Err(StorageError::new(
            if r.status == 409 { 400 } else { 502 },
            format!("could not create \"{path}\" ({})", r.status),
        )
        .upstream(r.status));
    }
    Ok(Folder {
        path,
        existed: false,
    })
}

/// storage-client.cjs `writeFile(conn, path, bytes, { ifMatch, ifNoneMatch })`.
pub fn write_file(
    c: &Connection,
    raw: &str,
    bytes: &[u8],
    condition: &Condition,
    t: &mut impl Transport,
) -> Result<Written, StorageError> {
    let Some(path) = safe(raw) else {
        return Err(StorageError::new(400, "invalid path"));
    };
    if c.kind == Kind::S3 {
        return Err(StorageError::new(400, "writing to S3 is not supported"));
    }
    if let Condition::IfMatch(v) = condition {
        if bad_value(v) {
            return Err(StorageError::new(400, "invalid If-Match value"));
        }
    }
    if bytes.len() > crate::WRITE_BYTES {
        // Not a core answer: core's callers cap first (25 MiB uploads); a larger write is refused here.
        return Err(StorageError::new(
            413,
            format!("file exceeds the {} byte limit", crate::WRITE_BYTES),
        ));
    }
    let conditional = *condition != Condition::None;
    let url = dav_url(c, &path, false);
    let r = send(t, conditional, || {
        let mut extra = vec![("Content-Type", "application/octet-stream".to_owned())];
        match condition {
            Condition::IfMatch(v) => extra.push(("If-Match", quote_etag(v))),
            Condition::IfNoneMatch => extra.push(("If-None-Match", "*".to_owned())),
            Condition::None => {}
        }
        request(
            c,
            Method::Put,
            url.clone(),
            extra,
            bytes.to_vec(),
            Duration::from_secs(60),
        )
    });
    let r = match r {
        Ok(r) => r,
        Err(_) if conditional => {
            return Err(StorageError::new(
                502,
                format!("the connection to storage failed while writing \"{path}\"; it may or may not have been saved"),
            )
            .code("unknown"))
        }
        Err(e) => return Err(transport_error(e)),
    };
    if conditional && r.status == 412 {
        return Err(StorageError::new(
            409,
            if matches!(condition, Condition::IfMatch(_)) {
                format!("\"{path}\" changed in storage before it could be written (412)")
            } else {
                format!("\"{path}\" already exists in storage, so it was not overwritten (412)")
            },
        )
        .code("changed"));
    }
    if !ok(r.status) {
        return Err(StorageError::new(
            if r.status == 409 { 400 } else { 502 },
            format!("could not write \"{path}\" ({})", r.status),
        )
        .upstream(r.status));
    }
    Ok(Written {
        path,
        etag: r.body.text.trim().to_owned(),
        created: r.status == 201,
    })
}

/// storage-client.cjs `discardWritten(conn, path, etag)`: never fails; `(deleted, reason)`.
pub fn discard_written(
    c: &Connection,
    raw: &str,
    etag: &str,
    t: &mut impl Transport,
) -> (bool, &'static str) {
    let Some(path) = safe(raw) else {
        return (false, "invalid-path");
    };
    if c.kind == Kind::S3 {
        return (false, "s3");
    }
    if bad_value(etag) {
        return (false, "no-etag");
    }
    let url = dav_url(c, &path, false);
    let req = request(
        c,
        Method::Delete,
        url,
        vec![("If-Match", quote_etag(etag))],
        vec![],
        Duration::from_secs(30),
    );
    match t.attempt(req) {
        Ok(r) if ok(r.status) => (true, ""),
        Ok(r) if r.status == 412 => (false, "changed"),
        Ok(r) if r.status == 404 => (false, "missing"),
        Ok(_) => (false, "status"),
        Err(_) => (false, "error"),
    }
}

/// `<[p:]collection …/>` as core's two checks spell it: `fileVersion` accepts up to 64 attribute
/// characters after whitespace, `removeEmptyFolder` only whitespace (`attributes`).
fn has_collection(block: &str, attributes: bool) -> bool {
    let b = block.as_bytes();
    let mut i = 0;
    while let Some(at) = b.get(i..).and_then(|r| r.iter().position(|&c| c == b'<')) {
        let start = i + at + 1;
        i = start;
        let mut j = start;
        while b.get(j).is_some_and(u8::is_ascii_alphanumeric) {
            j += 1;
        }
        let name_start = if b.get(j) == Some(&b':') && j > start {
            j + 1
        } else {
            start
        };
        if b.get(name_start..name_start + 10) != Some(b"collection") {
            continue;
        }
        let mut k = name_start + 10;
        if attributes {
            if b.get(k).is_some_and(|c| c.is_ascii_whitespace()) {
                let mut n = 0;
                k += 1;
                while n < 64 && b.get(k).is_some_and(|&c| c != b'>') {
                    k += 1;
                    n += 1;
                }
                // `[^>]{0,64}` then `\/?>`: backtracking lets the `/` be the last attribute char.
                if b.get(k) == Some(&b'>') {
                    return true;
                }
                continue;
            }
        } else {
            while b.get(k).is_some_and(|c| c.is_ascii_whitespace()) {
                k += 1;
            }
        }
        if b.get(k) == Some(&b'/') {
            k += 1;
        }
        if b.get(k) == Some(&b'>') {
            return true;
        }
    }
    false
}

fn etag_text(block: &str) -> String {
    dav_parse::element_texts(block, "getetag", 1)
        .first()
        .map(|t| t.trim().replace("&quot;", "\"").replace("&amp;", "&"))
        .unwrap_or_default()
}

fn propfind(
    c: &Connection,
    url: &str,
    depth: &str,
    t: &mut impl Transport,
) -> Result<Response, StorageError> {
    send(t, false, || {
        request(
            c,
            Method::Propfind,
            url.to_owned(),
            vec![
                ("Depth", depth.to_owned()),
                ("Content-Type", "application/xml".to_owned()),
            ],
            PROPFIND_QUERY.as_bytes().to_vec(),
            Duration::from_secs(15),
        )
    })
    .map_err(transport_error)
}

/// storage-client.cjs `fileVersion(conn, path)`.
pub fn file_version(
    c: &Connection,
    raw: &str,
    t: &mut impl Transport,
) -> Result<Version, StorageError> {
    let Some(path) = safe(raw) else {
        return Err(StorageError::new(400, "invalid path"));
    };
    if !kind_dav(c) {
        return Err(
            StorageError::new(400, "this storage cannot report a file version").code("unsupported"),
        );
    }
    let r = propfind(c, &dav_url(c, &path, false), "0", t)?;
    if r.status == 404 {
        return Ok(Version::Missing);
    }
    if r.status != 207 {
        return Err(
            StorageError::new(502, format!("storage returned {}", r.status)).upstream(r.status),
        );
    }
    if r.body.capped {
        return Err(StorageError::new(502, "storage reply too large").code("too_large"));
    }
    let blocks = dav_parse::element_texts(&r.body.text, "response", 1);
    let Some(block) = blocks.first().filter(|b| !b.is_empty()) else {
        return Err(StorageError::new(502, "storage returned no file state"));
    };
    if has_collection(block, true) {
        return Err(
            StorageError::new(409, format!("\"{path}\" is a folder in storage")).code("folder"),
        );
    }
    let etag = etag_text(block);
    Ok(Version::Present(if etag.contains(['\r', '\n']) {
        String::new()
    } else {
        etag
    }))
}

/// storage-client.cjs `deleteFile(conn, path)`: refuses a folder; `Ok(missing)`.
pub fn delete_file(
    c: &Connection,
    raw: &str,
    t: &mut impl Transport,
) -> Result<bool, StorageError> {
    let Some(path) = safe(raw) else {
        return Err(StorageError::new(400, "invalid path"));
    };
    if c.kind == Kind::S3 {
        return Err(StorageError::new(400, "deleting from S3 is not supported"));
    }
    let etag = match file_version(c, &path, t) {
        Ok(Version::Missing) => return Ok(true),
        Ok(Version::Present(e)) => e,
        Err(e) if e.code == Some("folder") => {
            return Err(StorageError::new(
                400,
                format!("refusing to delete \"{path}\": it is a folder"),
            )
            .code("folder"))
        }
        Err(e) => return Err(e),
    };
    let url = dav_url(c, &path, false);
    let r = send(t, false, || {
        let extra = if etag.is_empty() {
            vec![]
        } else {
            vec![("If-Match", quote_etag(&etag))]
        };
        request(
            c,
            Method::Delete,
            url.clone(),
            extra,
            vec![],
            Duration::from_secs(30),
        )
    })
    .map_err(transport_error)?;
    if r.status == 412 {
        return Err(StorageError::new(
            409,
            format!(
                "\"{path}\" changed in storage before it could be deleted; refresh and try again"
            ),
        )
        .code("changed"));
    }
    if r.status == 404 {
        return Ok(true);
    }
    if !ok(r.status) {
        return Err(StorageError::new(
            502,
            format!("could not delete \"{path}\" ({})", r.status),
        ));
    }
    Ok(false)
}

/// storage-client.cjs `removeEmptyFolder(conn, path)`: `Ok((removed, reason))`.
pub fn remove_empty_folder(
    c: &Connection,
    raw: &str,
    t: &mut impl Transport,
) -> Result<(bool, &'static str), StorageError> {
    let Some(path) = safe(raw) else {
        return Err(StorageError::new(400, "invalid folder path"));
    };
    if !kind_dav(c) {
        return Ok((false, "unsupported"));
    }
    let target = dav_url(c, &path, true);
    let r = propfind(c, &target, "1", t)?;
    if r.status == 404 {
        return Ok((false, "missing"));
    }
    if r.status != 207 {
        return Ok((false, "error"));
    }
    if r.body.capped {
        return Ok((false, "too-large"));
    }
    let Ok(base) = url::Url::parse(&target) else {
        return Ok((false, "error"));
    };
    let Some(request_dir) = dav_parse::decode_uri_component(base.path()) else {
        return Ok((false, "error"));
    };
    let request_dir = request_dir.trim_end_matches('/').to_owned();
    let (mut etag, mut collection, mut children) = (String::new(), false, 0usize);
    for block in dav_parse::element_texts(&r.body.text, "response", dav_parse::MAX_RESPONSES + 1) {
        let Some(href) = dav_parse::element_texts(block, "href", 1).first().copied() else {
            continue;
        };
        let href = dav_parse::decode_xml_entities(href);
        let resolved = base
            .join(dav_parse::js_trim(&href))
            .ok()
            .and_then(|u| dav_parse::decode_uri_component(u.path()));
        let Some(href) = resolved else {
            children += 1;
            continue;
        };
        if href.trim_end_matches('/') == request_dir {
            collection = has_collection(block, false);
            etag = etag_text(block);
        } else {
            children += 1;
        }
    }
    if !collection {
        return Ok((false, "not-directory"));
    }
    if children > 0 {
        return Ok((false, "not-empty"));
    }
    if bad_value(&etag) {
        return Ok((false, "no-etag"));
    }
    let req = request(
        c,
        Method::Delete,
        target,
        vec![("If-Match", quote_etag(&etag))],
        vec![],
        Duration::from_secs(15),
    );
    let r = t.attempt(req).map_err(transport_error)?;
    Ok(match r.status {
        404 => (false, "missing"),
        412 => (false, "changed"),
        s if ok(s) => (true, ""),
        _ => (false, "error"),
    })
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
mod tests {
    use super::*;
    use crate::Decoded;
    use std::time::Duration;

    /// One request as sent: method, URL, headers, body, attempt deadline.
    type Seen = (Method, String, Vec<(String, String)>, Vec<u8>, Duration);
    struct Fake {
        replies: Vec<Result<(u16, String), TransportFailure>>,
        seen: Vec<Seen>,
        delays: usize,
    }
    impl Fake {
        fn new(replies: Vec<Result<(u16, &str), TransportFailure>>) -> Self {
            Self {
                replies: replies
                    .into_iter()
                    .rev()
                    .map(|r| r.map(|(s, b)| (s, b.to_owned())))
                    .collect(),
                seen: Vec::new(),
                delays: 0,
            }
        }
        fn header(&self, i: usize, name: &str) -> Option<String> {
            self.seen[i]
                .2
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.clone())
        }
    }
    impl Transport for Fake {
        fn elapsed(&self) -> Duration {
            Duration::ZERO
        }
        fn amz_date(&self) -> String {
            String::new()
        }
        fn attempt(&mut self, r: Request) -> Result<Response, TransportFailure> {
            assert!(r.attempt_timeout <= r.method.max_attempt());
            assert!(r.body.len() <= r.method.max_body());
            self.seen.push((
                r.method,
                r.url.clone(),
                r.headers
                    .iter()
                    .map(|(k, v)| (k.clone(), v.to_string()))
                    .collect(),
                r.body.to_vec(),
                r.attempt_timeout,
            ));
            let (status, text) = self.replies.pop().expect("a reply")?;
            Ok(Response {
                status,
                body: Decoded {
                    text,
                    capped: false,
                },
            })
        }
        fn delay(&mut self, _: Duration) -> Result<(), TransportFailure> {
            self.delays += 1;
            Ok(())
        }
    }
    fn dav() -> Connection {
        Connection {
            kind: Kind::Nextcloud,
            base_url: "https://cloud.example/remote.php/dav/files/u/".into(),
            username: "u".into(),
            secret: Zeroizing::new(b"pw".to_vec()),
            corpus_root: "Diary".into(),
            bucket: String::new(),
            region: String::new(),
        }
    }
    fn s3() -> Connection {
        Connection {
            kind: Kind::S3,
            ..dav()
        }
    }
    #[test]
    fn paths_are_checked_and_encoded_like_dav_url_and_never_join_the_corpus_root() {
        let mut t = Fake::new(vec![Ok((201, "\"e1\""))]);
        let w = write_file(
            &dav(),
            " noevia projects/p 1/Text/a#b?.md ",
            b"x",
            &Condition::None,
            &mut t,
        )
        .unwrap();
        assert_eq!(w.path, "noevia projects/p 1/Text/a#b?.md");
        assert_eq!(
            t.seen[0].1,
            "https://cloud.example/remote.php/dav/files/u/noevia%20projects/p%201/Text/a%23b%3F.md"
        );
        for bad in ["", "/abs", "../x", "a/../b", "a/./b", "a\0b", "\\x"] {
            let mut t = Fake::new(vec![]);
            let e = write_file(&dav(), bad, b"x", &Condition::None, &mut t).unwrap_err();
            assert_eq!(
                (e.status, e.message.as_str()),
                (400, "invalid path"),
                "{bad:?}"
            );
            assert!(t.seen.is_empty());
            let e = create_folder(&dav(), bad, &mut t).unwrap_err();
            assert_eq!(e.message, "invalid folder path");
            assert_eq!(delete_file(&dav(), bad, &mut t).unwrap_err().status, 400);
            assert_eq!(
                discard_written(&dav(), bad, "e", &mut t),
                (false, "invalid-path")
            );
            assert!(t.seen.is_empty());
        }
    }

    #[test]
    fn a_put_sends_nodes_headers_and_reports_etag_and_created() {
        let mut t = Fake::new(vec![Ok((201, "W/\"abc\""))]);
        let w = write_file(&dav(), "a.md", b"hello", &Condition::IfNoneMatch, &mut t).unwrap();
        assert_eq!(
            w,
            Written {
                path: "a.md".into(),
                etag: "W/\"abc\"".into(),
                created: true
            }
        );
        assert_eq!(t.seen[0].0, Method::Put);
        assert_eq!(t.seen[0].3, b"hello");
        assert_eq!(t.seen[0].4, Duration::from_secs(60));
        assert_eq!(t.header(0, "If-None-Match").as_deref(), Some("*"));
        assert_eq!(
            t.header(0, "Content-Type").as_deref(),
            Some("application/octet-stream")
        );
        assert_eq!(t.header(0, "Content-Length").as_deref(), Some("5"));
        assert_eq!(
            t.header(0, "Authorization").as_deref(),
            Some("Basic dTpwdw==")
        );
        let mut t = Fake::new(vec![Ok((204, ""))]);
        let w = write_file(
            &dav(),
            "a.md",
            b"",
            &Condition::IfMatch("e1".into()),
            &mut t,
        )
        .unwrap();
        assert!(!w.created);
        assert_eq!(t.header(0, "If-Match").as_deref(), Some("\"e1\""));
    }

    #[test]
    fn put_failures_answer_as_core_does() {
        let mut t = Fake::new(vec![Ok((412, ""))]);
        let e = write_file(&dav(), "a.md", b"x", &Condition::IfNoneMatch, &mut t).unwrap_err();
        assert_eq!((e.status, e.code), (409, Some("changed")));
        assert_eq!(
            e.message,
            "\"a.md\" already exists in storage, so it was not overwritten (412)"
        );
        let mut t = Fake::new(vec![Ok((412, ""))]);
        let e = write_file(
            &dav(),
            "a.md",
            b"x",
            &Condition::IfMatch("e".into()),
            &mut t,
        )
        .unwrap_err();
        assert_eq!(
            e.message,
            "\"a.md\" changed in storage before it could be written (412)"
        );
        // An unconditional 412 is just a failed write.
        let mut t = Fake::new(vec![Ok((412, ""))]);
        let e = write_file(&dav(), "a.md", b"x", &Condition::None, &mut t).unwrap_err();
        assert_eq!(
            (e.status, e.upstream, e.message.as_str()),
            (502, Some(412), "could not write \"a.md\" (412)")
        );
        let mut t = Fake::new(vec![Ok((409, ""))]);
        assert_eq!(
            write_file(&dav(), "a.md", b"x", &Condition::None, &mut t)
                .unwrap_err()
                .status,
            400
        );
        let mut t = Fake::new(vec![Ok((401, ""))]);
        assert!(write_file(&dav(), "a.md", b"x", &Condition::None, &mut t)
            .unwrap_err()
            .refused_login());
        for bad in ["", "a\r\nb"] {
            let mut t = Fake::new(vec![]);
            let e = write_file(
                &dav(),
                "a.md",
                b"x",
                &Condition::IfMatch(bad.into()),
                &mut t,
            )
            .unwrap_err();
            assert_eq!(e.message, "invalid If-Match value");
        }
        let mut t = Fake::new(vec![]);
        assert_eq!(
            write_file(&s3(), "a.md", b"x", &Condition::None, &mut t)
                .unwrap_err()
                .message,
            "writing to S3 is not supported"
        );
        let big = vec![0u8; crate::WRITE_BYTES + 1];
        let e = write_file(&dav(), "a.md", &big, &Condition::None, &mut t).unwrap_err();
        assert_eq!(e.status, 413);
        assert!(t.seen.is_empty());
    }

    #[test]
    fn only_unconditional_requests_repeat_a_transport_failure_once() {
        let mut t = Fake::new(vec![Err(TransportFailure::PreHeaderReset), Ok((201, "e"))]);
        assert!(write_file(&dav(), "a.md", b"x", &Condition::None, &mut t).is_ok());
        assert_eq!((t.seen.len(), t.delays), (2, 1));
        let mut t = Fake::new(vec![
            Err(TransportFailure::Redirect),
            Err(TransportFailure::Redirect),
        ]);
        let e = write_file(&dav(), "a.md", b"x", &Condition::None, &mut t).unwrap_err();
        assert_eq!((e.status, e.code), (502, Some("unreachable")));
        let mut t = Fake::new(vec![Err(TransportFailure::Timeout)]);
        let e = write_file(&dav(), "a.md", b"x", &Condition::None, &mut t).unwrap_err();
        assert_eq!((e.status, e.code, t.seen.len()), (504, Some("timeout"), 1));
        let mut t = Fake::new(vec![Err(TransportFailure::PreHeaderReset)]);
        let e = write_file(&dav(), "a.md", b"x", &Condition::IfNoneMatch, &mut t).unwrap_err();
        assert_eq!((e.status, e.code, t.seen.len()), (502, Some("unknown"), 1));
        assert!(e.message.contains("may or may not have been saved"));
    }

    #[test]
    fn mkcol_405_is_an_existing_folder_and_s3_has_none() {
        let mut t = Fake::new(vec![
            Ok((405, "")),
            Ok((201, "")),
            Ok((409, "")),
            Ok((500, "")),
        ]);
        assert!(
            create_folder(&dav(), "noevia projects", &mut t)
                .unwrap()
                .existed
        );
        assert!(
            !create_folder(&dav(), "noevia projects/p", &mut t)
                .unwrap()
                .existed
        );
        assert_eq!(
            t.seen[0].1,
            "https://cloud.example/remote.php/dav/files/u/noevia%20projects/"
        );
        assert_eq!(t.seen[0].4, Duration::from_secs(15));
        let e = create_folder(&dav(), "x", &mut t).unwrap_err();
        assert_eq!(
            (e.status, e.message.as_str()),
            (400, "could not create \"x\" (409)")
        );
        assert_eq!(create_folder(&dav(), "x", &mut t).unwrap_err().status, 502);
        assert_eq!(create_folder(&s3(), "x", &mut t).unwrap_err().status, 400);
    }

    const FILE: &str = r#"<d:multistatus xmlns:d="DAV:"><d:response><d:href>/f/a.md</d:href><d:propstat><d:prop><d:resourcetype/><d:getetag>&quot;e9&quot;</d:getetag></d:prop></d:propstat></d:response></d:multistatus>"#;
    const DIR: &str = r#"<d:multistatus xmlns:d="DAV:"><d:response><d:href>/f/a.md</d:href><d:propstat><d:prop><d:resourcetype><d:collection xmlns:d="DAV:"/></d:resourcetype></d:prop></d:propstat></d:response></d:multistatus>"#;

    #[test]
    fn delete_file_asks_first_refuses_a_folder_and_is_conditional() {
        let mut t = Fake::new(vec![Ok((207, FILE)), Ok((204, ""))]);
        assert!(!delete_file(&dav(), "a.md", &mut t).unwrap());
        assert_eq!(t.header(0, "Depth").as_deref(), Some("0"));
        assert_eq!(t.header(1, "If-Match").as_deref(), Some("\"e9\""));
        let mut t = Fake::new(vec![Ok((207, DIR))]);
        let e = delete_file(&dav(), "a.md", &mut t).unwrap_err();
        assert_eq!((e.status, e.code), (400, Some("folder")));
        assert_eq!(t.seen.len(), 1, "a folder never gets a DELETE");
        let mut t = Fake::new(vec![Ok((404, ""))]);
        assert!(delete_file(&dav(), "a.md", &mut t).unwrap());
        let mut t = Fake::new(vec![Ok((207, FILE)), Ok((412, ""))]);
        assert_eq!(
            delete_file(&dav(), "a.md", &mut t).unwrap_err().code,
            Some("changed")
        );
        let mut t = Fake::new(vec![Ok((500, ""))]);
        assert_eq!(
            delete_file(&dav(), "a.md", &mut t).unwrap_err().upstream,
            Some(500)
        );
        let mut t = Fake::new(vec![Ok((207, "<d:multistatus/>"))]);
        assert_eq!(
            delete_file(&dav(), "a.md", &mut t).unwrap_err().message,
            "storage returned no file state"
        );
    }

    #[test]
    fn discard_needs_an_etag_and_never_repeats() {
        let mut t = Fake::new(vec![]);
        assert_eq!(
            discard_written(&dav(), "a.md", "", &mut t),
            (false, "no-etag")
        );
        assert_eq!(discard_written(&s3(), "a.md", "e", &mut t), (false, "s3"));
        let mut t = Fake::new(vec![Err(TransportFailure::PreHeaderReset)]);
        assert_eq!(
            discard_written(&dav(), "a.md", "e", &mut t),
            (false, "error")
        );
        assert_eq!(t.seen.len(), 1);
        let mut t = Fake::new(vec![Ok((412, "")), Ok((204, ""))]);
        assert_eq!(
            discard_written(&dav(), "a.md", "e", &mut t),
            (false, "changed")
        );
        assert_eq!(discard_written(&dav(), "a.md", "e", &mut t), (true, ""));
        assert_eq!(t.header(1, "If-Match").as_deref(), Some("\"e\""));
    }

    fn listing(children: &[&str], etag: &str) -> String {
        let mut s = format!(
            r#"<d:multistatus xmlns:d="DAV:"><d:response><d:href>/remote.php/dav/files/u/noevia%20projects/p/</d:href><d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype><d:getetag>{etag}</d:getetag></d:prop></d:propstat></d:response>"#
        );
        for c in children {
            s.push_str(&format!("<d:response><d:href>{c}</d:href></d:response>"));
        }
        s.push_str("</d:multistatus>");
        s
    }

    #[test]
    fn remove_empty_folder_deletes_only_an_empty_collection_with_an_etag() {
        let body = listing(&[], "\"d1\"");
        let mut t = Fake::new(vec![Ok((207, &body)), Ok((204, ""))]);
        assert_eq!(
            remove_empty_folder(&dav(), "noevia projects/p", &mut t).unwrap(),
            (true, "")
        );
        assert_eq!(t.header(0, "Depth").as_deref(), Some("1"));
        assert_eq!(
            (t.seen[1].0, t.header(1, "If-Match").as_deref()),
            (Method::Delete, Some("\"d1\""))
        );
        let body = listing(
            &["/remote.php/dav/files/u/noevia%20projects/p/x.md"],
            "\"d1\"",
        );
        let mut t = Fake::new(vec![Ok((207, &body))]);
        assert_eq!(
            remove_empty_folder(&dav(), "noevia projects/p", &mut t).unwrap(),
            (false, "not-empty")
        );
        assert_eq!(t.seen.len(), 1);
        let body = listing(&[], "");
        let mut t = Fake::new(vec![Ok((207, &body))]);
        assert_eq!(
            remove_empty_folder(&dav(), "noevia projects/p", &mut t).unwrap(),
            (false, "no-etag")
        );
        let mut t = Fake::new(vec![Ok((207, FILE))]);
        assert_eq!(
            remove_empty_folder(&dav(), "noevia projects/p", &mut t).unwrap(),
            (false, "not-directory")
        );
        let mut t = Fake::new(vec![Ok((404, ""))]);
        assert_eq!(
            remove_empty_folder(&dav(), "noevia projects/p", &mut t).unwrap(),
            (false, "missing")
        );
        let mut t = Fake::new(vec![]);
        assert_eq!(
            remove_empty_folder(&s3(), "noevia projects/p", &mut t).unwrap(),
            (false, "unsupported")
        );
    }

    #[test]
    fn collection_markers_match_cores_two_patterns() {
        assert!(has_collection("<d:collection/>", true));
        assert!(has_collection("<collection>", true));
        assert!(has_collection("<d:collection xmlns:d=\"DAV:\"/>", true));
        assert!(!has_collection("<d:collection xmlns:d=\"DAV:\"/>", false));
        assert!(has_collection("<d:collection  />", false));
        assert!(!has_collection("<d:collections/>", true));
        assert!(!has_collection("<d:collection ", true));
    }
}
