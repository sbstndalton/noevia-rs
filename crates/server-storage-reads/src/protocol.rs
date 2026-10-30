//! Protocol orchestration without network/store access. All public requests are reads.
use crate::{
    json, preview, Decoded, Failure, Prefix, LIST_BYTES, MAX_ENTRIES, MAX_PAGES, TEXT_BYTES,
};
use base64::Engine;
use std::time::Duration;
use zeroize::Zeroizing;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    WebDav,
    Nextcloud,
    S3,
    Local,
}

/// No Debug: even the endpoint and username are sensitive configuration.
/// This is not an authentication capability: the eventual store adapter must
/// construct it from a WHERE user_id snapshot using validated request identity.
pub struct Connection {
    pub kind: Kind,
    pub base_url: String,
    pub username: String,
    pub secret: Zeroizing<Vec<u8>>,
    pub corpus_root: String,
    pub bucket: String,
    pub region: String,
}

pub const ENDPOINT_REFUSAL: &str = "An http(s) server URL is required. This server is not approved for member connections. Ask an administrator to add its origin to MEMBER_OUTBOUND_ORIGINS.";

pub fn endpoint_approved(base: &str, admin: bool, member_origins: &str) -> bool {
    if base.len() > s3_sign::MAX_FIELD_BYTES {
        return false;
    }
    let Ok(url) = url::Url::parse(base) else {
        return false;
    };
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return false;
    }
    admin
        || member_origins
            .split(',')
            .map(dav_parse::js_trim)
            .any(|origin| origin == url.origin().ascii_serialization())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation {
    List,
    Read,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    Get,
    Propfind,
    /// The writes (noevia#2191 round 3, [`crate::writes`]): a reply carries the response's ETag
    /// (`ETag`, else `OC-ETag`) as its payload, never the response body.
    Put,
    Mkcol,
    Delete,
}

impl Method {
    /// Whether the child answers with the response's ETag instead of its body.
    pub fn etag_reply(self) -> bool {
        matches!(self, Self::Put | Self::Mkcol | Self::Delete)
    }
    /// The longest one attempt may take: core storage-client.cjs's `AbortSignal.timeout` for the
    /// method (PUT 60 s, DELETE 30 s, the rest at most 20 s).
    pub fn max_attempt(self) -> Duration {
        Duration::from_secs(match self {
            Self::Put => 60,
            Self::Delete => 30,
            _ => 20,
        })
    }
    /// The largest request body the method may carry: a PUT up to [`crate::WRITE_BYTES`], a
    /// PROPFIND its fixed XML query, nothing for the others.
    pub fn max_body(self) -> usize {
        match self {
            Self::Put => crate::WRITE_BYTES,
            Self::Propfind => crate::PROPFIND_BODY_BYTES,
            _ => 0,
        }
    }
}

/// Deliberately no Debug: headers and URL may contain credentials and private names.
pub struct Request {
    pub method: Method,
    pub url: String,
    pub headers: Vec<(String, Zeroizing<String>)>,
    pub attempt_timeout: Duration,
    pub body_cap: usize,
    /// The request body (a PUT's bytes, a PROPFIND's query); empty for a read.
    pub body: Zeroizing<Vec<u8>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransportFailure {
    PreHeaderReset,
    PreHeaderAborted,
    PreHeaderBrokenPipe,
    PreHeaderUnexpectedEof,
    Timeout,
    Redirect,
    Tls,
    Dns,
    Header,
    Body,
    /// Parent projection of an existing Body-phase Timeout frame; never sent on IPC.
    BodyTimeout,
    Other,
}
impl TransportFailure {
    pub fn retryable(self) -> bool {
        matches!(
            self,
            Self::PreHeaderReset
                | Self::PreHeaderAborted
                | Self::PreHeaderBrokenPipe
                | Self::PreHeaderUnexpectedEof
        )
    }
    /// Whether core storage-client.cjs `withRetry` repeats it once after 250 ms: every failure
    /// undici reports as `TypeError: fetch failed` (no connection, DNS, TLS, a refused redirect
    /// under `redirect: 'error'`, a malformed or reset response before its headers). A timeout
    /// (`TimeoutError`) and anything after the headers are not retried.
    pub fn node_retries(self) -> bool {
        !matches!(self, Self::Timeout | Self::Body | Self::BodyTimeout)
    }
    /// The answer core gives (storage-client.cjs `transportError`, then the route): a timeout
    /// before the headers is 504 "the storage server did not answer in time", any other
    /// transport failure before them 502 "the storage server could not be reached". After the
    /// headers the body read's own error passes through: a broken stream is `terminated`,
    /// while the active request deadline aborts it with a TimeoutError. Keep that phase
    /// distinct from a pre-header timeout (tests/storage_browse.rs, noevia#2085).
    pub fn public(self) -> Failure {
        match self {
            Self::Timeout => Failure::new(504, "the storage server did not answer in time"),
            Self::Body => Failure::new(502, "terminated"),
            Self::BodyTimeout => Failure::new(502, "The operation was aborted due to timeout"),
            _ => Failure::new(502, "the storage server could not be reached"),
        }
    }
}

pub struct Response {
    pub status: u16,
    pub body: Decoded,
}

/// One ranged GET's reply (noevia#2191 round 8, [`crate::stream`]): the status, what the response's
/// headers declare, and the body bytes exactly as sent (never decoded as text).
pub struct Chunk {
    pub status: u16,
    /// The file's size as declared: a 206's `Content-Range` total, a 200's `Content-Length`.
    pub total: Option<u64>,
    /// The first and last byte of the range a 206 says it carries.
    pub first: Option<u64>,
    pub last: Option<u64>,
    pub bytes: Vec<u8>,
    /// More bytes were sent than the chunk cap; `bytes` holds the first [`crate::CHUNK_BYTES`].
    pub capped: bool,
    /// The response's `ETag` (else `OC-ETag`), when it has a usable one.
    pub etag: Option<String>,
}

/// An adapter MUST enforce attempt + body deadlines, decoded byte caps, drop on
/// overflow, no redirects/proxies/cookies/implicit retry and finite allocation.
/// No production implementation is supplied until TLS/allocation proof passes.
pub trait Transport {
    fn elapsed(&self) -> Duration;
    fn amz_date(&self) -> String;
    fn attempt(&mut self, request: Request) -> Result<Response, TransportFailure>;
    /// One ranged GET ([`chunk_request`]) answered as raw bytes; only a transport that runs the
    /// owned child provides it.
    fn attempt_chunk(&mut self, _request: Request) -> Result<Chunk, TransportFailure> {
        Err(TransportFailure::Other)
    }
    fn delay(&mut self, delay: Duration) -> Result<(), TransportFailure>;
}

pub(crate) fn encode_component(value: &str) -> String {
    let mut out = String::new();
    const HEX: &[u8] = b"0123456789ABCDEF";
    for b in value.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&b) {
            out.push(char::from(b));
        } else {
            out.push('%');
            out.push(char::from(
                HEX.get(usize::from(b >> 4)).copied().unwrap_or(b'0'),
            ));
            out.push(char::from(
                HEX.get(usize::from(b & 15)).copied().unwrap_or(b'0'),
            ));
        }
    }
    out
}

fn url_for(
    connection: &Connection,
    path: &str,
    list: bool,
    query: &[(&str, &str)],
) -> Result<String, Failure> {
    let base = connection.base_url.trim_end_matches('/');
    let encoded = path
        .split('/')
        .filter(|s| !s.is_empty())
        .map(encode_component)
        .collect::<Vec<_>>()
        .join("/");
    if connection.kind != Kind::S3 {
        return Ok(format!("{base}/{encoded}{}", if list { "/" } else { "" }));
    }
    let bucket = encode_component(connection.bucket.trim_end_matches('/'));
    let suffix = if encoded.is_empty() {
        bucket
    } else {
        format!("{bucket}/{encoded}")
    };
    let mut url = url::Url::parse(&format!("{base}/{suffix}"))
        .map_err(|_| Failure::new(502, "Invalid URL"))?;
    if !query.is_empty() {
        let mut values: Vec<(String, String)> = url
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        // URLSearchParams.set preserves first position, removing duplicates.
        for (k, v) in query {
            let first = values.iter().position(|(key, _)| key == k);
            if let Some(at) = first {
                let mut index = 0;
                values.retain_mut(|(key, value)| {
                    let keep = key != k || index == at;
                    if index == at {
                        *value = (*v).to_owned();
                    }
                    index += 1;
                    keep
                });
            } else {
                values.push(((*k).to_owned(), (*v).to_owned()));
            }
        }
        let mut pairs = url.query_pairs_mut();
        pairs.clear();
        for (k, v) in values {
            pairs.append_pair(&k, &v);
        }
    }
    Ok(url.into())
}

fn path_for(raw: &[u16], operation: Operation) -> Result<String, Failure> {
    // JS validation precedes URI encoding. A lone surrogate that survives the
    // path rule cannot be replaced with U+FFFD and sent to another destination.
    let mut trimmed = raw;
    while trimmed
        .first()
        .is_some_and(|u| char::from_u32(u32::from(*u)).is_some_and(dav_parse::is_js_whitespace))
    {
        trimmed = trimmed.get(1..).unwrap_or_default();
    }
    while trimmed
        .last()
        .is_some_and(|u| char::from_u32(u32::from(*u)).is_some_and(dav_parse::is_js_whitespace))
    {
        trimmed = trimmed
            .get(..trimmed.len().saturating_sub(1))
            .unwrap_or_default();
    }
    let invalid_rule = trimmed.len() > 500
        || trimmed.first().is_some_and(|u| *u == 47 || *u == 92)
        || trimmed.contains(&0)
        || trimmed
            .split(|u| *u == 47 || *u == 92)
            .any(|s| s == [46] || s == [46, 46]);
    let bare_dot = trimmed == [46];
    if invalid_rule && !(operation == Operation::List && bare_dot) {
        return Err(path_error(operation));
    }
    if operation == Operation::Read {
        let name = trimmed
            .rsplit(|u| *u == 47 || *u == 92)
            .find(|s| !s.is_empty())
            .unwrap_or_default();
        if name.is_empty() {
            return Err(path_error(operation));
        }
        let ext = name
            .iter()
            .rposition(|u| *u == 46)
            .and_then(|at| name.get(at..))
            .unwrap_or_default();
        let valid_ext = String::from_utf16(ext).is_ok_and(|s| supported(&s));
        if !valid_ext {
            let mut message = vec![34];
            message.extend_from_slice(name);
            message.extend("\" is not a supported text file".encode_utf16());
            return Err(Failure::units(400, message));
        }
    }
    let value = String::from_utf16(trimmed).map_err(|_| Failure::new(502, "URI malformed"))?;
    let path = storage_path::safe_relative_path(&value).map_err(|_| path_error(operation))?;
    if path.is_empty() && (operation == Operation::Read || (!trimmed.is_empty() && !bare_dot)) {
        return Err(path_error(operation));
    }
    Ok(path)
}
fn path_error(op: Operation) -> Failure {
    Failure::new(
        400,
        if op == Operation::List {
            "Use a relative path inside the storage folder."
        } else {
            "invalid path"
        },
    )
}
pub fn extension(name: &str) -> String {
    name.rfind('.')
        .and_then(|at| name.get(at..))
        .unwrap_or("")
        .to_lowercase()
}
fn supported(name: &str) -> bool {
    matches!(
        extension(name).as_str(),
        ".txt"
            | ".md"
            | ".markdown"
            | ".json"
            | ".csv"
            | ".yml"
            | ".yaml"
            | ".ts"
            | ".tsx"
            | ".js"
            | ".jsx"
            | ".py"
            | ".sh"
            | ".html"
            | ".css"
    )
}

fn request(
    c: &Connection,
    path: &str,
    op: Operation,
    query: &[(&str, &str)],
    date: &str,
    remaining: Duration,
) -> Result<Request, Failure> {
    let url = url_for(c, path, op == Operation::List, query)?;
    let mut headers = Vec::new();
    let method = if op == Operation::List && c.kind != Kind::S3 {
        Method::Propfind
    } else {
        Method::Get
    };
    if method == Method::Propfind {
        // undici sends `content-length: 0` on a body-less PROPFIND; so does this.
        headers.push(("Content-Length".into(), Zeroizing::new("0".into())));
        headers.push(("Depth".into(), Zeroizing::new("1".into())));
        headers.push((
            "Content-Type".into(),
            Zeroizing::new("application/xml".into()),
        ));
    }
    if c.kind == Kind::S3 {
        let target = url::Url::parse(&url).map_err(|_| Failure::new(502, "Invalid URL"))?;
        let host = match target.port() {
            Some(port) => format!("{}:{port}", target.host_str().unwrap_or("")),
            None => target.host_str().unwrap_or("").to_owned(),
        };
        let owned: Vec<_> = target
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        let pairs: Vec<_> = owned
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        let signed = s3_sign::sign(&s3_sign::Request {
            method: "GET",
            host: &host,
            pathname: target.path(),
            query: &pairs,
            payload: b"",
            access_key: &c.username,
            secret_key: &c.secret,
            region: &s3_sign::normalize_region(&c.region),
            session_token: "",
            amz_date: date,
        })
        .map_err(|_| Failure::new(502, "storage request could not be signed"))?;
        for (k, v) in signed.headers {
            headers.push((k.into(), Zeroizing::new(v)));
        }
    } else if !c.username.is_empty() {
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
    Ok(Request {
        method,
        url,
        headers,
        attempt_timeout: remaining.min(Duration::from_secs(if op == Operation::Read {
            20
        } else {
            15
        })),
        body_cap: if op == Operation::Read {
            TEXT_BYTES
        } else {
            LIST_BYTES
        },
        body: Zeroizing::new(Vec::new()),
    })
}

/// How many S3 listings were cut at their page or entry bound in this process (for the log's tests).
pub static CUT_LISTINGS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// A ranged GET of bytes `first..=last` of one file (connection-absolute `path`, already checked):
/// the read's request with a `Range` header, its reply the raw bytes behind a
/// [`crate::CHUNK_PREFIX`]-byte header summary. For S3 the key is joined under the corpus root as a
/// read does; the `Range` header is not signed (S3 accepts it unsigned).
pub(crate) fn chunk_request(
    c: &Connection,
    path: &str,
    first: u64,
    last: u64,
    if_range: Option<&str>,
    date: &str,
    remaining: Duration,
) -> Result<Request, Failure> {
    let remote = if c.kind == Kind::S3 {
        storage_path::join_root(&c.corpus_root, path)
            .map_err(|_| Failure::new(502, "storage path could not be checked"))?
    } else {
        path.to_owned()
    };
    let mut r = request(c, &remote, Operation::Read, &[], date, remaining)?;
    r.headers.push((
        "Range".into(),
        Zeroizing::new(format!("bytes={first}-{last}")),
    ));
    // `If-Range` (RFC 9110 13.1.5): the range is served only while the file still has the ETag the
    // first reply had; a changed file comes back whole, as a 200, which the stream refuses.
    if let Some(etag) = if_range {
        r.headers
            .push(("If-Range".into(), Zeroizing::new(etag.to_owned())));
    }
    r.body_cap = crate::CHUNK_FRAME;
    Ok(r)
}

fn fetch(
    c: &Connection,
    path: &str,
    op: Operation,
    query: &[(&str, &str)],
    transport: &mut impl Transport,
    deadline: Duration,
) -> Result<Decoded, Failure> {
    for attempt in 0..2 {
        let remaining = deadline
            .checked_sub(transport.elapsed())
            .filter(|d| !d.is_zero())
            .ok_or_else(|| TransportFailure::Timeout.public())?;
        let req = request(c, path, op, query, &transport.amz_date(), remaining)?;
        match transport.attempt(req) {
            Ok(response) => {
                if transport.elapsed() > deadline {
                    return Err(TransportFailure::Timeout.public());
                }
                if (300..400).contains(&response.status) {
                    return Err(TransportFailure::Redirect.public());
                }
                if !(200..300).contains(&response.status) {
                    return Err(Failure::new(
                        502,
                        format!("storage returned {}", response.status),
                    ));
                }
                // Defense at the seam; allocation enforcement belongs to adapter.
                if response.body.text.len() > LIST_BYTES * 3 {
                    return Err(TransportFailure::Body.public());
                }
                return Ok(response.body);
            }
            Err(error) if attempt == 0 && error.node_retries() => {
                if deadline.saturating_sub(transport.elapsed()) <= Duration::from_millis(250) {
                    return Err(TransportFailure::Timeout.public());
                }
                transport
                    .delay(Duration::from_millis(250))
                    .map_err(TransportFailure::public)?;
            }
            Err(error) => return Err(error.public()),
        }
    }
    Err(TransportFailure::Other.public())
}

struct Entry {
    name: String,
    is_dir: bool,
    size: Option<String>,
    ext: String,
}
fn listing_budget() -> Failure {
    Failure::new(502, "storage listing exceeds resource limit")
}
fn preflight(body: &str, target: Option<&str>) -> Result<(), Failure> {
    let mut total = 0usize;
    if let Some(target) = target {
        let blocks = dav_parse::element_texts(body, "response", dav_parse::MAX_RESPONSES + 1);
        if blocks.len() > dav_parse::MAX_RESPONSES {
            return Err(listing_budget());
        }
        for block in blocks {
            // Conservative URL resolution allowance includes the whole target
            // per candidate. Refusal is a named new resource qualification.
            total = total
                .checked_add(block.len())
                .and_then(|n| n.checked_add(target.len()))
                .ok_or_else(listing_budget)?;
            if total > crate::LIST_ENTRY_BYTES {
                return Err(listing_budget());
            }
        }
    } else {
        for tag in ["CommonPrefixes", "Contents"] {
            let blocks = dav_parse::element_texts(body, tag, s3_list_parse::MAX_BLOCKS + 1);
            if blocks.len() > s3_list_parse::MAX_BLOCKS {
                return Err(listing_budget());
            }
            for block in blocks {
                total = total.checked_add(block.len()).ok_or_else(listing_budget)?;
                if total > crate::LIST_ENTRY_BYTES {
                    return Err(listing_budget());
                }
            }
        }
    }
    Ok(())
}
fn check_entries(entries: &[Entry]) -> Result<(), Failure> {
    let mut total = 0usize;
    for e in entries {
        total = total
            .checked_add(e.name.len())
            .and_then(|n| n.checked_add(e.ext.len()))
            .and_then(|n| n.checked_add(e.size.as_ref().map_or(0, String::len)))
            .ok_or_else(listing_budget)?;
        if total > crate::LIST_ENTRY_BYTES {
            return Err(listing_budget());
        }
    }
    Ok(())
}
fn entries_json(mut entries: Vec<Entry>, path: &str) -> Result<String, Failure> {
    check_entries(&entries)?;
    let prefix = if path.is_empty() {
        0
    } else {
        json::quoted_str_bytes(path) - 2 + 1
    };
    let mut maximum = 2usize;
    for (index, entry) in entries.iter().enumerate() {
        let number = size_json(entry.size.as_deref());
        let fixed = if entry.is_dir {
            r#"{"name":,"path":,"isDir":true,"size":,"ext":}"#
        } else {
            r#"{"name":,"path":,"isDir":false,"size":,"ext":}"#
        };
        let size = json::quoted_str_bytes(&entry.name)
            .checked_mul(2)
            .and_then(|n| n.checked_add(json::quoted_str_bytes(&entry.ext)))
            .and_then(|n| {
                n.checked_add(prefix + fixed.len() + number.len() + usize::from(index > 0))
            })
            .ok_or_else(listing_budget)?;
        maximum = maximum.checked_add(size).ok_or_else(listing_budget)?;
        if maximum > crate::LIST_OUTPUT_BYTES {
            return Err(listing_budget());
        }
    }
    entries
        .sort_by_cached_key(|entry| entry.name.to_lowercase().encode_utf16().collect::<Vec<_>>());
    let mut out = String::new();
    out.try_reserve_exact(maximum)
        .map_err(|_| listing_budget())?;
    out.push('[');
    for (i, entry) in entries.into_iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str("{\"name\":");
        json::quote_str(&mut out, &entry.name);
        out.push_str(",\"path\":");
        let full = if path.is_empty() {
            entry.name
        } else {
            format!("{path}/{}", entry.name)
        };
        json::quote_str(&mut out, &full);
        out.push_str(if entry.is_dir {
            ",\"isDir\":true,\"size\":"
        } else {
            ",\"isDir\":false,\"size\":"
        });
        out.push_str(&size_json(entry.size.as_deref()));
        out.push_str(",\"ext\":");
        json::quote_str(&mut out, &entry.ext);
        out.push('}');
    }
    out.push(']');
    Ok(out)
}
fn size_json(size: Option<&str>) -> String {
    size.and_then(|s| s.parse::<f64>().ok())
        .filter(|n| n.is_finite())
        .map(js_json::number_to_string)
        .unwrap_or_else(|| "null".into())
}

/// One whole operation. Caller validates auth/CSRF, obtains admission and checks
/// endpoint policy before calling. No real network implementation exists yet.
pub fn execute(
    c: &Connection,
    raw: &[u16],
    op: Operation,
    transport: &mut impl Transport,
) -> Result<String, Failure> {
    // Snapshot contract bounds reuse the existing signer/path/envelope domains.
    // Adapter must separately qualify oversized legacy configuration refusal.
    if [&c.base_url, &c.username, &c.corpus_root, &c.bucket]
        .iter()
        .any(|value| value.len() > s3_sign::MAX_FIELD_BYTES)
        || c.region.len() > s3_sign::MAX_REGION_BYTES
        || c.secret.len() > 8 * 1024 * 1024
    {
        // A named private input bound (SNAPSHOT_FIELD_BUDGET), not a Node answer.
        return Err(Failure::new(502, "fetch failed"));
    }
    if c.kind == Kind::Local {
        return Err(Failure::new(
            400,
            if op == Operation::List {
                "no browsable storage connected (local storage needs no browsing — upload files directly)"
            } else {
                "no browsable storage connected"
            },
        ));
    }
    let path = path_for(raw, op)?;
    let budget = Duration::from_secs(if op == Operation::Read {
        41
    } else if c.kind == Kind::S3 {
        610
    } else {
        31
    });
    let deadline = transport
        .elapsed()
        .checked_add(budget)
        .ok_or_else(|| TransportFailure::Timeout.public())?;
    if op == Operation::Read {
        let name = path.rsplit('/').next().unwrap_or("");
        if !supported(name) {
            return Err(Failure::new(
                400,
                format!("\"{name}\" is not a supported text file"),
            ));
        }
        let remote = if c.kind == Kind::S3 {
            storage_path::join_root(&c.corpus_root, &path)
                .map_err(|_| Failure::new(502, "storage path could not be checked"))?
        } else {
            path.clone()
        };
        return fetch(c, &remote, op, &[], transport, deadline).map(|body| preview(name, body));
    }
    if c.kind != Kind::S3 {
        let target = url_for(c, &path, true, &[])?;
        let body = fetch(c, &path, op, &[], transport, deadline)?;
        preflight(&body.text, Some(&target))?;
        let entries = dav_parse::list_entries(&body.text, &target)
            .map_err(|_| Failure::new(502, "storage listing could not be read"))?;
        return entries_json(
            entries
                .into_iter()
                .map(|e| Entry {
                    ext: extension(&e.name),
                    name: e.name,
                    is_dir: e.is_dir,
                    size: e.size,
                })
                .collect(),
            &path,
        );
    }
    let root = storage_path::join_root(&c.corpus_root, &path)
        .map_err(|_| Failure::new(502, "storage path could not be checked"))?;
    let prefix = if root.is_empty() {
        root
    } else {
        format!("{root}/")
    };
    let mut token: Option<String> = None;
    let mut entries = Vec::new();
    // Whether the listing stopped at its page or entry bound with more still to list (noevia#2191
    // round 8 review): core's listing is cut the same way; it is now said in the log.
    let mut cut = false;
    for page_no in 0..MAX_PAGES {
        if entries.len() >= MAX_ENTRIES {
            cut = true;
            break;
        }
        let mut query = vec![
            ("list-type", "2"),
            ("prefix", prefix.as_str()),
            ("delimiter", "/"),
            ("max-keys", "1000"),
        ];
        if let Some(t) = &token {
            query.push(("continuation-token", t));
        }
        let body = fetch(c, "", op, &query, transport, deadline)?;
        preflight(&body.text, None)?;
        let page = s3_list_parse::parse_page(&body.text, &prefix)
            .map_err(|_| Failure::new(502, "storage listing could not be read"))?;
        entries.extend(page.entries.into_iter().map(|e| Entry {
            ext: if e.is_dir {
                String::new()
            } else {
                extension(&e.name)
            },
            name: e.name,
            is_dir: e.is_dir,
            size: e.size,
        }));
        check_entries(&entries)?;
        if !page.truncated {
            break;
        }
        match page.next {
            Some(next) if !next.is_empty() && token.as_ref() != Some(&next) => token = Some(next),
            _ => break,
        }
        cut = page_no + 1 == MAX_PAGES;
    }
    if entries.len() > MAX_ENTRIES {
        cut = true;
    }
    if cut {
        CUT_LISTINGS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        eprintln!(
            "noevia-server: an S3 listing was cut at its bound of {MAX_ENTRIES} entries or {MAX_PAGES} pages; later entries are not listed"
        );
    }
    entries.truncate(MAX_ENTRIES);
    entries_json(entries, &path)
}

/// Fixed fixture helper: transport implementors use this after streaming the
/// decoded response. Never allocate an unbounded remote response to call it.
pub fn decoded_fixture(bytes: &[u8], cap: usize) -> Result<Decoded, Failure> {
    let mut prefix = Prefix::new(cap)?;
    prefix.feed(bytes);
    Ok(prefix.finish())
}
