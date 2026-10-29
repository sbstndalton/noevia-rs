//! `POST /api/integrations/storage/{folder,test,nextcloud/start,nextcloud/poll,nextcloud/cancel}`
//! (sbstndalton/noevia#2190), ported from core routes/storage.cjs, nextcloud-flows.cjs and the
//! parts of storage-client.cjs they call (`checkLogin`, `createFolder`). Permanent, no switch;
//! core rust-storage-integrations.cjs refuses the five routes with
//! `RUST_STORAGE_INTEGRATIONS_OWNED` before authentication.
//!
//! - The pending Nextcloud sign-ins (Login Flow v2) move here wholesale: the same file
//!   (`UI_DATA_DIR/nextcloud-login-flows.json`, `{version:1,flows:{id:{userId,expires,sealed}}}`),
//!   the poll endpoint, token, origin and nonce sealed with the credential key bound to the owning
//!   account (secrets.cjs `encrypt`, `enc:v2:`), the 10-minute lifetime, the 100-flow limit, one
//!   pending sign-in per account, and the nonce that lets a cancelled flow's in-flight poll revoke
//!   the app password instead of saving it (#1433). Node no longer reads or writes the file, so
//!   flows started before an upgrade still complete here.
//! - A completed sign-in saves the account's `storage_connections` row with core auth.cjs
//!   `saveStorage`'s exact statement (server-store `save_storage_connection`), the app password
//!   sealed as Node seals it, and the `storage.update` audit row, in one transaction. Node reads
//!   the row on every use (auth.cjs `getStorage`), so nothing in Node caches it.
//! - The storage latch (core storage-latch.cjs, #1429/#1589) stays in Node with its automatic
//!   callers. A folder or test call here that works with the saved credentials writes the proof
//!   Node's `loginProven` used to apply directly: `UI_DATA_DIR/storage-login-proofs.json`,
//!   `{version:1,proofs:{<userId>:<epoch ms>}}`. Node re-reads it by its file signal and drops a
//!   hold that was set before the proof. No credential or fingerprint is written.
//!
//! Outbound policy is Node's: the member-origin rule (core ssrf.cjs `createEndpointApproved`, an
//! administrator any http(s) origin, a member only MEMBER_OUTBOUND_ORIGINS, read per request) on
//! every target before it is contacted, no redirect followed (undici `redirect:'error'`), no
//! environment proxy, Node's deadlines (10 s; 15 s per MKCOL attempt, one retry after 250 ms on a
//! transport failure) and the 64 KiB cap on a Nextcloud reply.

use crate::{identity, reply, App};
use axum::body::Body;
use axum::http::{Method, Request, Response, StatusCode};
use axum::response::IntoResponse;
use base64::Engine;
use http_body_util::BodyExt;
use js_json::JValue;
use rand_core::{OsRng, RngCore};
use server_auth::secrets::SecretKeys;
use server_storage_reads::{owner_secret, protocol};
use server_store::{RustStorageIntegrations, StoreError};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use zeroize::Zeroizing;

pub const FOLDER: &str = "/api/integrations/storage/folder";
pub const TEST: &str = "/api/integrations/storage/test";
pub const NEXTCLOUD_START: &str = "/api/integrations/storage/nextcloud/start";
pub const NEXTCLOUD_POLL: &str = "/api/integrations/storage/nextcloud/poll";
pub const NEXTCLOUD_CANCEL: &str = "/api/integrations/storage/nextcloud/cancel";
pub const ROUTES: &[&str] = &[
    FOLDER,
    TEST,
    NEXTCLOUD_START,
    NEXTCLOUD_POLL,
    NEXTCLOUD_CANCEL,
];

/// core nextcloud-flows.cjs: where the pending sign-ins are kept.
pub const FLOWS_FILE: &str = "nextcloud-login-flows.json";
/// The proof a saved login worked (core storage-latch.cjs reads it).
pub const PROOFS_FILE: &str = "storage-login-proofs.json";

const BODY_LIMIT: usize = 1024 * 1024;
const BODY_DEADLINE: Duration = Duration::from_secs(10);
const NEXTCLOUD_TIMEOUT: Duration = Duration::from_secs(10);
const CHECK_TIMEOUT: Duration = Duration::from_secs(10);
const MKCOL_TIMEOUT: Duration = Duration::from_secs(15);
const RETRY_PAUSE: Duration = Duration::from_millis(250);
const NEXTCLOUD_REPLY_CAP: usize = 64 * 1024;
const FLOW_TTL_MS: i64 = 10 * 60 * 1000;
const MAX_FLOWS: usize = 100;
const CLIENT_NAME: &str = "Noevia";
/// undici's own User-Agent, which Node sends where it sets none.
const NODE_AGENT: &str = "node";

const PRIVATE_URL: &str = protocol::ENDPOINT_REFUSAL;
const LOGIN_REJECTED_NEW: &str =
    "Nextcloud refused the new app password for WebDAV. Sign in again.";
const STORAGE_UNVERIFIED: &str =
    "Saved, but the storage server could not be reached to check the login.";
const STORAGE_TIMED_OUT: &str = "Saved, but the storage server could not be reached within 10 seconds to check the login. Nextcloud slows logins down after repeated failures; press Test again in a few minutes.";
const HTTPS_REQUIRED: &str =
    "Enter the https:// address you open Nextcloud with, for example https://cloud.example.com.";
const FLOW_CANCELLED: &str = "Sign-in was cancelled. Nothing was changed.";
const SECRET_NEEDED: &str = "Enter a secret to test a different server or storage type.";
const S3_NO_FOLDERS: &str =
    "S3 has no folders \u{2014} a prefix appears once a file is stored under it";
/// undici's messages, which Node's `/test` passes on as `e.message`.
const FETCH_FAILED: &str = "fetch failed";
const ABORTED_TIMEOUT: &str = "The operation was aborted due to timeout";

// ---------------------------------------------------------------------------------------------
// JavaScript coercions.

/// `String(v)`; a value JS could not coerce reads as ''.
fn js_string(v: &JValue) -> String {
    js_json::to_js_string(v).unwrap_or_default()
}

/// `String(v || '')`.
fn or_empty(v: &JValue) -> String {
    if v.truthy() {
        js_string(v)
    } else {
        String::new()
    }
}

fn js_trim(s: &str) -> &str {
    s3_sign::js_trim(s)
}

/// `s.replace(/\/+$/, '')`.
fn strip_slashes(s: &str) -> &str {
    s.trim_end_matches('/')
}

/// `encodeURIComponent` (its input is valid UTF-8 here: js-json refuses lone surrogates).
pub fn encode_uri_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&b) {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// `new URL(raw)` for an absolute URL.
fn parse_url(raw: &str) -> Option<url::Url> {
    url::Url::parse(raw).ok()
}

/// `new URL(url).host`, or `'that address'`.
fn host_of(raw: &str) -> String {
    parse_url(raw).map_or_else(|| "that address".to_string(), |u| url_host(&u))
}

fn url_host(u: &url::Url) -> String {
    let host = u.host_str().unwrap_or("");
    match u.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_string(),
    }
}

/// `new URL(url).origin`, or ''.
fn origin_of(raw: &str) -> String {
    parse_url(raw).map_or_else(String::new, |u| u.origin().ascii_serialization())
}

fn is_https(raw: &str) -> bool {
    parse_url(raw).is_some_and(|u| u.scheme() == "https")
}

/// core routes/storage.cjs `nextcloudServerRoot` (#1427): the scheme, host, port and any
/// sub-path Nextcloud is installed under; '' for a non-URL or one with a login in it.
pub fn nextcloud_server_root(raw: &str) -> String {
    let Some(url) = parse_url(js_trim(raw)) else {
        return String::new();
    };
    if !url.username().is_empty() || url.password().is_some() {
        return String::new();
    }
    let path = url.path();
    let lower = path.to_ascii_lowercase();
    let mut cut = path.len();
    for (i, _) in lower.match_indices('/') {
        let rest = lower.get(i + 1..).unwrap_or("");
        let hit = [
            "remote.php",
            "index.php",
            "apps",
            "ocs",
            "login",
            "status.php",
        ]
        .iter()
        .any(|name| {
            rest.strip_prefix(name)
                .is_some_and(|after| after.is_empty() || after.starts_with('/'))
        });
        if hit {
            cut = i;
            break;
        }
    }
    let kept = strip_slashes(path.get(..cut).unwrap_or(""));
    format!("{}://{}{kept}", url.scheme(), url_host(&url))
}

/// core ssrf.cjs `createEndpointApproved()(authn, rawUrl)`, MEMBER_OUTBOUND_ORIGINS read per
/// request as Node reads `process.env`.
fn endpoint_approved(admin: bool, raw: &str) -> bool {
    let members = std::env::var("MEMBER_OUTBOUND_ORIGINS").unwrap_or_default();
    protocol::endpoint_approved(raw, admin, &members)
}

// ---------------------------------------------------------------------------------------------
// Answers.

fn answer(status: StatusCode, body: &JValue) -> Response<Body> {
    reply::json_text(status, js_json::stringify(body).unwrap_or_default(), true)
}

fn error(status: u16, message: &str) -> Response<Body> {
    answer(
        StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
        &JValue::obj([("error", JValue::Str(message.into()))]),
    )
}

fn coded(status: u16, message: &str, code: &str) -> Response<Body> {
    answer(
        StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
        &JValue::obj([
            ("error", JValue::Str(message.into())),
            ("code", JValue::Str(code.into())),
        ]),
    )
}

fn unavailable() -> Response<Body> {
    error(503, "Storage integrations unavailable")
}

/// Node's last-resort answer for a throw without a 4xx status (http.cjs `errorResponse`).
fn internal() -> Response<Body> {
    error(500, "Internal error")
}

fn origin_mismatch(expected: &str, got: &str) -> JValue {
    JValue::obj([
        ("error", JValue::Str(format!("Nextcloud answered with the address {}, not {} where sign-in started. Check Nextcloud's overwritehost and overwrite.cli.url settings, or sign in at the address Nextcloud reports.", host_of(got), host_of(expected)))),
        ("code", JValue::Str("nextcloudOriginMismatch".into())),
    ])
}

/// `Number(retry-after)`, positive and finite, rounded up and capped at a day.
fn retry_after(value: Option<&str>) -> Option<f64> {
    let wait = js_number(value?);
    (wait.is_finite() && wait > 0.0).then(|| wait.ceil().min(86_400.0))
}

/// `Number(text)` for a header value.
fn js_number(text: &str) -> f64 {
    let t = js_trim(text);
    if t.is_empty() {
        return 0.0;
    }
    if let Some(hex) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        return u64::from_str_radix(hex, 16).map_or(f64::NAN, |n| n as f64);
    }
    match t {
        "Infinity" | "+Infinity" => f64::INFINITY,
        "-Infinity" => f64::NEG_INFINITY,
        _ if t
            .bytes()
            .any(|b| b.is_ascii_alphabetic() && b != b'e' && b != b'E') =>
        {
            f64::NAN
        }
        _ => t.parse::<f64>().unwrap_or(f64::NAN),
    }
}

/// core `nextcloudStatusFailure(status, host, headers)`.
fn status_failure(status: u16, host: &str, retry: Option<f64>) -> (u16, JValue) {
    if status == 429 {
        let mut items = vec![
            ("error".to_string(), JValue::Str(format!("Nextcloud at {host} is throttling sign-ins from noevia after too many failed attempts. Wait a few minutes and try again."))),
            ("code".to_string(), JValue::Str("nextcloudThrottled".into())),
        ];
        if let Some(wait) = retry {
            items.push(("retryAfter".into(), JValue::Num(wait)));
        }
        return (429, JValue::Obj(items));
    }
    if status == 404 {
        return (
            502,
            JValue::obj([
                (
                    "error",
                    JValue::Str(format!(
                        "No Nextcloud sign-in was found at {host} (404). Check the address."
                    )),
                ),
                ("code", JValue::Str("nextcloudNotFound".into())),
                ("status", JValue::Num(f64::from(status))),
            ]),
        );
    }
    (
        502,
        JValue::obj([
            ("error", JValue::Str(format!("Nextcloud returned {status}"))),
            ("code", JValue::Str("nextcloudError".into())),
            ("status", JValue::Num(f64::from(status))),
        ]),
    )
}

/// core `nextcloudFailure(err, host)`.
fn transport_failure(e: Fetch, host: &str) -> (u16, JValue) {
    let (status, message, reason) = match e {
        Fetch::Timeout => (
            504,
            format!("Nextcloud at {host} did not answer within 10 seconds."),
            "timeout",
        ),
        Fetch::Redirect => (502, format!("Nextcloud at {host} redirected the request. Use the exact https:// address you open Nextcloud with."), "redirect"),
        Fetch::Network => (502, format!("Could not reach Nextcloud at {host}."), "network"),
    };
    (
        status,
        JValue::obj([
            ("error", JValue::Str(message)),
            ("code", JValue::Str("nextcloudUnreachable".into())),
            ("reason", JValue::Str(reason.into())),
        ]),
    )
}

fn send(pair: (u16, JValue)) -> Response<Body> {
    answer(
        StatusCode::from_u16(pair.0).unwrap_or(StatusCode::BAD_GATEWAY),
        &pair.1,
    )
}

// ---------------------------------------------------------------------------------------------
// Outbound requests (undici `fetch` with `redirect:'error'`).

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fetch {
    Timeout,
    Redirect,
    Network,
}

struct Reply {
    status: u16,
    retry_after: Option<String>,
    response: reqwest::Response,
    deadline: Instant,
}

impl Reply {
    fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// core http.cjs `readCappedJson(response, cap)`: `Err(true)` over the cap, `Err(false)` for
    /// anything else that is not JSON (the deadline included).
    async fn json(mut self, cap: usize) -> Result<JValue, bool> {
        let deadline = self.deadline;
        let read = async {
            let mut body = Vec::new();
            while let Some(chunk) = self.response.chunk().await.map_err(|_| false)? {
                if body.len() + chunk.len() > cap {
                    return Err(true);
                }
                body.extend_from_slice(&chunk);
            }
            Ok(body)
        };
        let body = tokio::time::timeout_at(deadline.into(), read)
            .await
            .map_err(|_| false)??;
        js_json::parse(&String::from_utf8_lossy(&body)).map_err(|_| false)
    }
}

/// The outbound client: no redirect followed, no environment proxy, the public roots (plus a QA
/// run's synthetic root).
fn build_client(qa_root: Option<&[u8]>) -> Option<reqwest::Client> {
    let mut builder = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(NODE_AGENT);
    if let Some(der) = qa_root {
        builder = builder.add_root_certificate(reqwest::Certificate::from_der(der).ok()?);
    }
    builder.build().ok()
}

async fn fetch(
    client: Option<&reqwest::Client>,
    method: &str,
    url: &str,
    headers: &[(&str, &str)],
    body: Option<String>,
    timeout: Duration,
) -> Result<Reply, Fetch> {
    let deadline = Instant::now() + timeout;
    let client = client.ok_or(Fetch::Network)?;
    let method = reqwest::Method::from_bytes(method.as_bytes()).map_err(|_| Fetch::Network)?;
    let get = method == reqwest::Method::GET;
    let mut request = client.request(method, url);
    for (k, v) in headers {
        request = request.header(*k, *v);
    }
    // undici sends `content-length: 0` on a body-less request other than GET.
    request = match body {
        Some(b) => request.body(b),
        None if get => request,
        None => request.body(Vec::new()),
    };
    let response = match tokio::time::timeout_at(deadline.into(), request.send()).await {
        Err(_) => return Err(Fetch::Timeout),
        Ok(Err(e)) if e.is_timeout() => return Err(Fetch::Timeout),
        Ok(Err(_)) => return Err(Fetch::Network),
        Ok(Ok(r)) => r,
    };
    let status = response.status().as_u16();
    if matches!(status, 301 | 302 | 303 | 307 | 308) {
        return Err(Fetch::Redirect);
    }
    let retry_after = response
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    Ok(Reply {
        status,
        retry_after,
        response,
        deadline,
    })
}

/// Releases a reply without reading it (`response.body?.cancel?.()`).
fn discard(reply: Reply) {
    drop(reply.response);
}

fn basic(username: &str, secret: &str) -> Zeroizing<String> {
    let pair = Zeroizing::new(format!("{username}:{secret}"));
    Zeroizing::new(format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(pair.as_bytes())
    ))
}

/// core storage-client.cjs `checkLogin`.
#[derive(Debug, Clone, PartialEq)]
enum Check {
    Ok,
    Rejected,
    Throttled(Option<f64>),
    Unverified(&'static str, Option<u16>),
}

async fn check_login(
    client: Option<&reqwest::Client>,
    base_url: &str,
    username: &str,
    secret: &str,
) -> Check {
    let auth = basic(username, secret);
    let mut headers = vec![("Depth", "0"), ("Content-Type", "application/xml")];
    if !username.is_empty() {
        headers.push(("Authorization", auth.as_str()));
    }
    let url = format!("{}/", strip_slashes(base_url));
    let reply = match fetch(client, "PROPFIND", &url, &headers, None, CHECK_TIMEOUT).await {
        Ok(r) => r,
        Err(Fetch::Timeout) => return Check::Unverified("timeout", None),
        Err(Fetch::Redirect) => return Check::Unverified("redirect", None),
        Err(Fetch::Network) => return Check::Unverified("network", None),
    };
    let status = reply.status;
    let wait = retry_after(reply.retry_after.as_deref());
    let ok = reply.ok();
    discard(reply);
    match status {
        401 => Check::Rejected,
        429 => Check::Throttled(wait),
        _ if ok || status == 207 => Check::Ok,
        _ => Check::Unverified("status", Some(status)),
    }
}

// ---------------------------------------------------------------------------------------------
// Files in UI_DATA_DIR (core nextcloud-flows.cjs `writeAtomic`).

fn write_atomic(file: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    if let Some(dir) = file.parent() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
    }
    let mut tmp = file.as_os_str().to_owned();
    tmp.push(format!(".{}.tmp", std::process::id()));
    let tmp = PathBuf::from(tmp);
    let mut out = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    out.write_all(text.as_bytes())?;
    out.sync_all()?;
    drop(out);
    std::fs::rename(&tmp, file)
}

fn read_json_file(file: &Path) -> Result<Option<JValue>, ()> {
    match std::fs::read(file) {
        Ok(bytes) => Ok(js_json::parse(&String::from_utf8_lossy(&bytes)).ok()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(()),
    }
}

/// One pending sign-in as stored: `{userId, expires}` in clear and the sealed fields.
#[derive(Debug, Clone, PartialEq)]
struct Record {
    user_id: String,
    expires: f64,
    sealed: String,
}

/// What the poll needs from a flow (core `toView`).
#[derive(Clone)]
struct Flow {
    user_id: String,
    expires: f64,
    endpoint: String,
    token: Zeroizing<String>,
    origin: String,
    nonce: String,
}

/// core nextcloud-flows.cjs `createNextcloudFlowStore({file, secrets})`, loaded on first use.
#[derive(Default)]
struct Flows {
    loaded: bool,
    records: Vec<(String, Record)>,
}

impl Flows {
    fn open(keys: &SecretKeys, record: &Record) -> Option<Flow> {
        let (plain, _) = keys.open(&record.sealed, Some(&record.user_id)).ok()?;
        let fields = js_json::parse(&plain).ok()?;
        let text = |k: &str| fields.get(k).as_str().map(str::to_string);
        Some(Flow {
            user_id: record.user_id.clone(),
            expires: record.expires,
            endpoint: text("endpoint")?,
            token: Zeroizing::new(text("token")?),
            origin: text("origin")?,
            nonce: text("nonce")?,
        })
    }

    fn load(&mut self, file: &Path, keys: &SecretKeys, now: f64) {
        if self.loaded {
            return;
        }
        self.loaded = true;
        let raw = match read_json_file(file) {
            Ok(Some(v)) => v,
            Ok(None) => return,
            Err(()) => {
                eprintln!("[nextcloud] pending sign-ins could not be read; they are dropped");
                return;
            }
        };
        let flows = raw.get("flows");
        let JValue::Obj(items) = flows else {
            return;
        };
        for (id, r) in items {
            let (Some(user_id), Some(expires), Some(sealed)) = (
                r.get("userId").as_str(),
                r.get("expires").as_num(),
                r.get("sealed").as_str(),
            ) else {
                continue;
            };
            self.records.push((
                id.clone(),
                Record {
                    user_id: user_id.into(),
                    expires,
                    sealed: sealed.into(),
                },
            ));
        }
        self.prune(keys, now);
    }

    /// Drops expired flows and any no key can open; true when something went.
    fn prune(&mut self, keys: &SecretKeys, now: f64) -> bool {
        let before = self.records.len();
        self.records
            .retain(|(_, r)| r.expires >= now && Self::open(keys, r).is_some());
        self.records.len() != before
    }

    fn persist(&self, file: &Path) -> std::io::Result<()> {
        let flows = self
            .records
            .iter()
            .map(|(id, r)| {
                (
                    id.clone(),
                    JValue::obj([
                        ("userId", JValue::Str(r.user_id.clone())),
                        ("expires", JValue::Num(r.expires)),
                        ("sealed", JValue::Str(r.sealed.clone())),
                    ]),
                )
            })
            .collect();
        let doc = JValue::obj([("version", JValue::Num(1.0)), ("flows", JValue::Obj(flows))]);
        write_atomic(file, &js_json::stringify(&doc).unwrap_or_default())
    }

    fn position(&self, id: &str) -> Option<usize> {
        self.records.iter().position(|(k, _)| k == id)
    }

    /// `get(flowId)`: an expired flow is deleted (and the file rewritten); one no key opens
    /// reads as missing.
    fn get(
        &mut self,
        file: &Path,
        keys: &SecretKeys,
        id: &str,
        now: f64,
    ) -> std::io::Result<Option<Flow>> {
        let Some(at) = self.position(id) else {
            return Ok(None);
        };
        let Some((_, record)) = self.records.get(at) else {
            return Ok(None);
        };
        if record.expires < now {
            self.records.remove(at);
            self.persist(file)?;
            return Ok(None);
        }
        Ok(Self::open(keys, record))
    }

    fn delete(&mut self, file: &Path, id: &str) -> std::io::Result<bool> {
        let Some(at) = self.position(id) else {
            return Ok(false);
        };
        self.records.remove(at);
        self.persist(file)?;
        Ok(true)
    }

    /// `dropExpiredOr(userId)`.
    fn drop_expired_or(
        &mut self,
        file: &Path,
        keys: &SecretKeys,
        user: &str,
        now: f64,
    ) -> std::io::Result<()> {
        let before = self.records.len();
        self.records.retain(|(_, r)| {
            !(r.expires < now || r.user_id == user || Self::open(keys, r).is_none())
        });
        if self.records.len() != before {
            self.persist(file)?;
        }
        Ok(())
    }

    /// `size` (prunes first, without rewriting the file, as Node).
    fn size(&mut self, keys: &SecretKeys, now: f64) -> usize {
        self.prune(keys, now);
        self.records.len()
    }

    /// `set(flowId, flow)`: a fresh nonce per set, the fields sealed for the owner.
    #[allow(clippy::too_many_arguments)]
    fn set(
        &mut self,
        file: &Path,
        keys: &SecretKeys,
        id: &str,
        user: &str,
        endpoint: &str,
        token: &JValue,
        origin: &str,
        expires: f64,
    ) -> Result<(), ()> {
        let nonce = hex(&random::<16>());
        let fields = JValue::obj([
            ("endpoint", JValue::Str(endpoint.into())),
            ("token", token.clone()),
            ("origin", JValue::Str(origin.into())),
            ("nonce", JValue::Str(nonce)),
        ]);
        let plain = Zeroizing::new(js_json::stringify(&fields).ok_or(())?);
        let sealed = keys
            .seal(&plain, Some(user), &random::<12>())
            .map_err(|_| ())?;
        let record = Record {
            user_id: user.into(),
            expires,
            sealed,
        };
        match self.position(id) {
            Some(at) => {
                if let Some(slot) = self.records.get_mut(at) {
                    slot.1 = record;
                }
            }
            None => self.records.push((id.into(), record)),
        }
        self.persist(file).map_err(|_| ())
    }
}

fn random<const N: usize>() -> [u8; N] {
    let mut out = [0u8; N];
    OsRng.fill_bytes(&mut out);
    out
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// `crypto.randomUUID()`.
fn uuid_v4() -> String {
    let mut b = random::<16>();
    if let Some(x) = b.get_mut(6) {
        *x = (*x & 0x0f) | 0x40;
    }
    if let Some(x) = b.get_mut(8) {
        *x = (*x & 0x3f) | 0x80;
    }
    let h = hex(&b);
    format!(
        "{}-{}-{}-{}-{}",
        h.get(0..8).unwrap_or(""),
        h.get(8..12).unwrap_or(""),
        h.get(12..16).unwrap_or(""),
        h.get(16..20).unwrap_or(""),
        h.get(20..32).unwrap_or("")
    )
}

/// The in-process state of these routes (App).
pub struct State {
    flows: Mutex<Flows>,
    proofs: Mutex<()>,
    client: Option<reqwest::Client>,
}

impl State {
    pub fn new(qa_root: Option<&[u8]>) -> Self {
        State {
            flows: Mutex::new(Flows::default()),
            proofs: Mutex::new(()),
            client: build_client(qa_root),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The account's saved connection (core auth.cjs `getStorage(userId, true)`).

/// The five fields core storage-latch.cjs `fingerprintOf` hashes, as `String(c.x || '')`.
#[derive(PartialEq, Eq)]
struct Fingerprint([String; 5]);

impl Fingerprint {
    fn of(v: &JValue) -> Self {
        Fingerprint(["kind", "baseUrl", "bucket", "username", "secret"].map(|k| or_empty(v.get(k))))
    }
}

/// The saved connection as Node's `getStorage(userId, true)` object (the secret included).
fn saved_object(p: &owner_secret::ProjectedConnection) -> JValue {
    let c = p.connection();
    let kind = match c.kind {
        protocol::Kind::WebDav => "webdav",
        protocol::Kind::Nextcloud => "nextcloud",
        protocol::Kind::S3 => "s3",
        protocol::Kind::Local => "local",
    };
    let mut items = vec![
        ("kind".to_string(), JValue::Str(kind.into())),
        ("baseUrl".into(), JValue::Str(c.base_url.clone())),
        ("bucket".into(), JValue::Str(c.bucket.clone())),
        ("region".into(), JValue::Str(c.region.clone())),
        ("username".into(), JValue::Str(c.username.clone())),
        ("corpusRoot".into(), JValue::Str(c.corpus_root.clone())),
    ];
    if p.row_present() {
        items.push((
            "secret".into(),
            JValue::Str(String::from_utf8_lossy(&c.secret).into_owned()),
        ));
        items.push((
            "secretConfigured".into(),
            JValue::Bool(p.secret_configured()),
        ));
        items.push((
            "secretNeedsReauth".into(),
            JValue::Bool(p.secret_needs_reauth()),
        ));
    }
    JValue::Obj(items)
}

/// What a request needs from the store: the signed-in account's identity and keys.
struct Ctx {
    app: Arc<App>,
    id: server_auth::Identity,
    data_dir: PathBuf,
}

impl Ctx {
    fn admin(&self) -> bool {
        self.id.user.role == server_auth::Role::Admin
    }

    fn user(&self) -> String {
        self.id.user.id.clone()
    }

    fn client(&self) -> Option<&reqwest::Client> {
        self.app.storage_integrations.client.as_ref()
    }

    fn keys(&self) -> Option<SecretKeys> {
        let previous = std::env::var("SECRETS_KEY_PREVIOUS")
            .ok()
            .map(Zeroizing::new);
        owner_secret::load_existing_keys(&self.data_dir, previous.as_deref().map(String::as_str))
            .ok()
    }

    /// `authService.getStorage(userId, true)`; `None` when it cannot be read.
    async fn saved(self: &Arc<Self>) -> Option<JValue> {
        let ctx = Arc::clone(self);
        tokio::task::spawn_blocking(move || {
            let keys = ctx.keys();
            let store = ctx.app.identity.store_blocking().ok()?;
            let projected =
                owner_secret::project_connection_with(&store, &ctx.id, keys.as_ref()).ok()?;
            Some(saved_object(&projected))
        })
        .await
        .ok()
        .flatten()
    }

    /// core routes/storage.cjs `loginProven(authn, connection)`: when the call used the saved
    /// credentials, the proof Node's latch clears its hold by.
    async fn login_proven(self: &Arc<Self>, used: &JValue) {
        let Some(saved) = self.saved().await else {
            return;
        };
        if Fingerprint::of(&saved) != Fingerprint::of(used) {
            return;
        }
        let ctx = Arc::clone(self);
        let _ = tokio::task::spawn_blocking(move || {
            let Ok(_guard) = ctx.app.storage_integrations.proofs.lock() else {
                return;
            };
            let file = ctx.data_dir.join(PROOFS_FILE);
            let mut proofs = match read_json_file(&file) {
                Ok(Some(doc)) => match doc.get("proofs") {
                    JValue::Obj(items) => items.clone(),
                    _ => Vec::new(),
                },
                _ => Vec::new(),
            };
            js_json::wtf16::set(
                &mut proofs,
                &ctx.user(),
                JValue::Num(identity::now_ms() as f64),
            );
            let doc = JValue::obj([
                ("version", JValue::Num(1.0)),
                ("proofs", JValue::Obj(proofs)),
            ]);
            if write_atomic(&file, &js_json::stringify(&doc).unwrap_or_default()).is_err() {
                eprintln!("noevia-server: storage login proof could not be written");
            }
        })
        .await;
    }
}

// ---------------------------------------------------------------------------------------------
// HTTP.

enum BodyError {
    TooLarge,
    Failed,
}

async fn read_body(mut body: Body) -> Result<Vec<u8>, BodyError> {
    let read = async {
        let mut out = Vec::new();
        while let Some(frame) = body.frame().await {
            if let Ok(data) = frame.map_err(|_| BodyError::Failed)?.into_data() {
                if out.len() + data.len() > BODY_LIMIT {
                    return Err(BodyError::TooLarge);
                }
                out.extend_from_slice(&data);
            }
        }
        Ok(out)
    };
    tokio::time::timeout(BODY_DEADLINE, read)
        .await
        .map_err(|_| BodyError::Failed)?
}

/// `requireJsonObject(readJson)(req)`: `{}` for an empty body; anything but an object is a 400.
async fn read_object(body: Body) -> Result<JValue, Box<Response<Body>>> {
    let bytes = match read_body(body).await {
        Ok(b) => b,
        Err(BodyError::TooLarge) => return Err(Box::new(error(413, "Request exceeds size limit"))),
        Err(BodyError::Failed) => return Err(Box::new(error(400, "invalid JSON"))),
    };
    if bytes.is_empty() {
        return Ok(JValue::Obj(Vec::new()));
    }
    let value = js_json::parse(&String::from_utf8_lossy(&bytes))
        .map_err(|_| Box::new(error(400, "invalid JSON")))?;
    if !value.is_object() {
        return Err(Box::new(error(400, "request body must be a JSON object")));
    }
    Ok(value)
}

pub async fn serve(
    app: &Arc<App>,
    conn: crate::serve::Conn,
    req: Request<Body>,
    path: String,
) -> Response<Body> {
    // Node compared the exact pathnames, and only POST.
    if !ROUTES.contains(&path.as_str()) || req.method() != Method::POST {
        return app.proxy.forward(conn.peer, req).await;
    }
    let (parts, body) = req.into_parts();
    let creds = identity::creds_of(&parts);
    let layer = Arc::clone(&app.identity);
    let now = identity::now_ms();
    let gate_path = path.clone();
    let id = match tokio::task::spawn_blocking(move || layer.gate_blocking(&creds, &gate_path, now))
        .await
    {
        Ok(Ok(id)) => id,
        Ok(Err(rejection)) => return rejection.into_response(),
        Err(_) => return identity::Rejection::Unavailable.into_response(),
    };
    let Some(data_dir) = app.config.data_dir.clone() else {
        return unavailable();
    };
    let body = match read_object(body).await {
        Ok(v) => v,
        Err(r) => return *r,
    };
    let ctx = Arc::new(Ctx {
        app: Arc::clone(app),
        id,
        data_dir,
    });
    match path.as_str() {
        FOLDER => folder(&ctx, &body).await,
        TEST => test(&ctx, &body).await,
        NEXTCLOUD_START => start(&ctx, &body).await,
        NEXTCLOUD_CANCEL => cancel(&ctx, &body).await,
        _ => poll(&ctx, &body).await,
    }
}

// ---------------------------------------------------------------------------------------------
// POST /folder: MKCOL only (storage-client.cjs `createFolder`).

async fn folder(ctx: &Arc<Ctx>, body: &JValue) -> Response<Body> {
    let Some(saved) = ctx.saved().await else {
        return unavailable();
    };
    let kind = saved.get("kind").as_str().unwrap_or("local");
    if !matches!(kind, "webdav" | "nextcloud" | "s3") {
        return error(400, "no browsable storage connected");
    }
    let base = or_empty(saved.get("baseUrl"));
    if !endpoint_approved(ctx.admin(), &base) {
        return error(400, PRIVATE_URL);
    }
    // storage-path.cjs `safeRelativePath(String(raw || ''))`; input it refuses to take is its 400.
    let Ok(path) = storage_path::safe_relative_path(&or_empty(body.get("path"))) else {
        return error(400, "storage path could not be checked");
    };
    if path.is_empty() {
        return error(400, "invalid folder path");
    }
    if kind == "s3" {
        return error(400, S3_NO_FOLDERS);
    }
    let encoded: Vec<String> = path
        .split('/')
        .filter(|s| !s.is_empty())
        .map(encode_uri_component)
        .collect();
    let url = format!("{}/{}/", strip_slashes(&base), encoded.join("/"));
    let username = or_empty(saved.get("username"));
    let auth = basic(&username, &or_empty(saved.get("secret")));
    let mut headers = Vec::new();
    if !username.is_empty() {
        headers.push(("Authorization", auth.as_str()));
    }
    let mut attempt = fetch(ctx.client(), "MKCOL", &url, &headers, None, MKCOL_TIMEOUT).await;
    if matches!(attempt, Err(Fetch::Network | Fetch::Redirect)) {
        tokio::time::sleep(RETRY_PAUSE).await;
        attempt = fetch(ctx.client(), "MKCOL", &url, &headers, None, MKCOL_TIMEOUT).await;
    }
    let reply = match attempt {
        Ok(r) => r,
        Err(Fetch::Timeout) => return error(504, "the storage server did not answer in time"),
        Err(_) => return error(502, "the storage server could not be reached"),
    };
    let status = reply.status;
    let ok = reply.ok();
    discard(reply);
    let existed = match status {
        405 => true,
        _ if ok => false,
        409 => return error(400, &format!("could not create \"{path}\" ({status})")),
        _ => return error(502, &format!("could not create \"{path}\" ({status})")),
    };
    ctx.login_proven(&saved).await;
    answer(
        StatusCode::OK,
        &JValue::obj([
            ("path", JValue::Str(path)),
            ("existed", JValue::Bool(existed)),
        ]),
    )
}

// ---------------------------------------------------------------------------------------------
// POST /test.

async fn test(ctx: &Arc<Ctx>, body: &JValue) -> Response<Body> {
    if body.get("kind").as_str() == Some("local") {
        return answer(StatusCode::OK, &JValue::obj([("ok", JValue::Bool(true))]));
    }
    let mut saved = if body.get("useSaved").truthy() {
        match ctx.saved().await {
            Some(s) => s,
            None => return unavailable(),
        }
    } else {
        body.clone()
    };
    if body.get("useSavedSecret").as_bool() == Some(true) {
        let Some(stored) = ctx.saved().await else {
            return unavailable();
        };
        // Only this session's credential, and never forwarded to a new origin.
        let same_origin = match (
            parse_url(&js_string(body.get("baseUrl"))),
            parse_url(&js_string(stored.get("baseUrl"))),
        ) {
            (Some(target), Some(saved_url)) => {
                target.username().is_empty()
                    && target.password().is_none()
                    && target.origin() == saved_url.origin()
            }
            _ => false,
        };
        let same_kind = match (body.get("kind"), stored.get("kind")) {
            (JValue::Str(a), JValue::Str(b)) => a == b,
            _ => false,
        };
        if !same_origin || !same_kind || !stored.get("secret").truthy() {
            return error(400, SECRET_NEEDED);
        }
        let mut items = match body {
            JValue::Obj(items) => items.clone(),
            _ => Vec::new(),
        };
        js_json::wtf16::set(&mut items, "secret", stored.get("secret").clone());
        saved = JValue::Obj(items);
    }
    let kind = saved.get("kind");
    if kind.as_str() != Some("local")
        && !endpoint_approved(ctx.admin(), &js_string(saved.get("baseUrl")))
    {
        return error(400, PRIVATE_URL);
    }
    if kind.as_str() == Some("s3") {
        return test_s3(ctx, &saved).await;
    }
    // WebDAV / Nextcloud: PROPFIND Depth 0 on the corpus root.
    let root: Vec<String> = or_empty(saved.get("corpusRoot"))
        .split('/')
        .map(encode_uri_component)
        .collect();
    let target = format!(
        "{}/{}",
        strip_slashes(&js_string(saved.get("baseUrl"))),
        root.join("/")
    );
    if parse_url(&target).is_none() {
        return error(502, &format!("Failed to parse URL from {target}"));
    }
    let auth = basic(
        &js_string(saved.get("username")),
        &js_string(saved.get("secret")),
    );
    let headers = [("Authorization", auth.as_str()), ("Depth", "0")];
    match fetch(
        ctx.client(),
        "PROPFIND",
        &target,
        &headers,
        None,
        CHECK_TIMEOUT,
    )
    .await
    {
        Ok(reply) => {
            let (status, ok) = (reply.status, reply.ok());
            discard(reply);
            if ok || status == 207 {
                ctx.login_proven(&saved).await;
                answer(StatusCode::OK, &JValue::obj([("ok", JValue::Bool(true))]))
            } else {
                error(502, &format!("WebDAV returned {status}"))
            }
        }
        Err(Fetch::Timeout) => error(502, ABORTED_TIMEOUT),
        Err(_) => error(502, FETCH_FAILED),
    }
}

/// The S3 probe: a signed bucket listing proves the endpoint, the bucket and the credentials.
async fn test_s3(ctx: &Arc<Ctx>, saved: &JValue) -> Response<Body> {
    let endpoint = or_empty(saved.get("baseUrl"));
    let endpoint = strip_slashes(&endpoint);
    if !(endpoint.starts_with("http://") || endpoint.starts_with("https://")) {
        return error(400, "Endpoint URL must start with http:// or https://");
    }
    let bucket = or_empty(saved.get("bucket"));
    let bucket = js_trim(&bucket);
    if bucket.is_empty() {
        return error(400, "Bucket is required");
    }
    let target = format!(
        "{endpoint}/{}?list-type=2&max-keys=1",
        encode_uri_component(bucket)
    );
    let Some(url) = parse_url(&target) else {
        return error(502, "Invalid URL");
    };
    let pairs: Vec<(String, String)> = url
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    let query: Vec<(&str, &str)> = pairs
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let host = url_host(&url);
    let secret = Zeroizing::new(or_empty(saved.get("secret")));
    let access = or_empty(saved.get("username"));
    let region = s3_sign::normalize_region(&or_empty(saved.get("region")));
    let date = server_storage_reads::transport::amz_date(std::time::SystemTime::now());
    let Ok(signed) = s3_sign::sign(&s3_sign::Request {
        method: "GET",
        host: &host,
        pathname: url.path(),
        query: &query,
        payload: b"",
        access_key: &access,
        secret_key: secret.as_bytes(),
        region: &region,
        session_token: "",
        amz_date: &date,
    }) else {
        return error(502, "storage request could not be signed");
    };
    let headers: Vec<(&str, &str)> = signed
        .headers
        .iter()
        .map(|(k, v)| (*k, v.as_str()))
        .collect();
    match fetch(ctx.client(), "GET", &target, &headers, None, CHECK_TIMEOUT).await {
        Ok(reply) => {
            let (status, ok) = (reply.status, reply.ok());
            discard(reply);
            if ok {
                ctx.login_proven(saved).await;
                return answer(StatusCode::OK, &JValue::obj([("ok", JValue::Bool(true))]));
            }
            let detail = match status {
                403 => " \u{2014} check the access key and secret",
                404 => " \u{2014} no such bucket",
                400 => " \u{2014} server rejected the request (unsupported endpoint?)",
                _ => "",
            };
            error(502, &format!("S3 returned {status}{detail}"))
        }
        Err(Fetch::Timeout) => error(502, ABORTED_TIMEOUT),
        Err(_) => error(502, FETCH_FAILED),
    }
}

// ---------------------------------------------------------------------------------------------
// The Nextcloud Login Flow v2 (#1427, #1431, #1432, #1433).

/// Runs `f` on the flow store, loaded, on a blocking thread. `None` when the keys or the thread
/// are unavailable.
async fn with_flows<T: Send + 'static>(
    ctx: &Arc<Ctx>,
    f: impl FnOnce(&mut Flows, &Path, &SecretKeys, f64) -> T + Send + 'static,
) -> Option<T> {
    let ctx = Arc::clone(ctx);
    tokio::task::spawn_blocking(move || {
        let keys = ctx.keys()?;
        let mut flows = ctx.app.storage_integrations.flows.lock().ok()?;
        let file = ctx.data_dir.join(FLOWS_FILE);
        let now = identity::now_ms() as f64;
        flows.load(&file, &keys, now);
        Some(f(&mut flows, &file, &keys, now))
    })
    .await
    .ok()
    .flatten()
}

async fn start(ctx: &Arc<Ctx>, body: &JValue) -> Response<Body> {
    let base = nextcloud_server_root(&or_empty(body.get("baseUrl")));
    if !base.starts_with("https://") {
        return coded(400, HTTPS_REQUIRED, "nextcloudHttpsRequired");
    }
    if !endpoint_approved(ctx.admin(), &base) {
        return error(400, PRIVATE_URL);
    }
    let host = host_of(&base);
    let headers = [("User-Agent", CLIENT_NAME)];
    let url = format!("{base}/index.php/login/v2");
    let reply = match fetch(
        ctx.client(),
        "POST",
        &url,
        &headers,
        None,
        NEXTCLOUD_TIMEOUT,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => return send(transport_failure(e, &host)),
    };
    if !reply.ok() {
        let wait = retry_after(reply.retry_after.as_deref());
        return send(status_failure(reply.status, &host, wait));
    }
    let Ok(payload) = reply.json(NEXTCLOUD_REPLY_CAP).await else {
        return coded(
            502,
            &format!("{host} answered, but not like a Nextcloud server. Check the address."),
            "nextcloudNotFound",
        );
    };
    let flow_id = uuid_v4();
    let user = ctx.user();
    let full = {
        let user = user.clone();
        with_flows(ctx, move |flows, file, keys, now| {
            flows.drop_expired_or(file, keys, &user, now)?;
            Ok::<bool, std::io::Error>(flows.size(keys, now) >= MAX_FLOWS)
        })
        .await
    };
    match full {
        Some(Ok(false)) => {}
        Some(Ok(true)) => return error(429, "Too many pending connections"),
        Some(Err(_)) => return internal(),
        None => return unavailable(),
    }
    let login = payload.get("login");
    let login_https = !login.is_nullish() && is_https(&js_string(login));
    let JValue::Str(endpoint) = payload.get("poll").get("endpoint").clone() else {
        return error(400, "Invalid connection URLs");
    };
    if !login_https {
        return error(400, "Invalid connection URLs");
    }
    if !is_https(&endpoint) {
        return coded(400, HTTPS_REQUIRED, "nextcloudHttpsRequired");
    }
    let origin = origin_of(&base);
    if origin_of(&endpoint) != origin {
        return answer(StatusCode::BAD_REQUEST, &origin_mismatch(&base, &endpoint));
    }
    if !endpoint_approved(ctx.admin(), &endpoint) {
        return error(400, "Invalid connection URLs");
    }
    let token = payload.get("poll").get("token").clone();
    let expires = (identity::now_ms() + FLOW_TTL_MS) as f64;
    let stored = {
        let (id, user) = (flow_id.clone(), user.clone());
        with_flows(ctx, move |flows, file, keys, _| {
            flows.set(file, keys, &id, &user, &endpoint, &token, &origin, expires)
        })
        .await
    };
    match stored {
        Some(Ok(())) => {}
        Some(Err(())) => return internal(),
        None => return unavailable(),
    }
    answer(
        StatusCode::OK,
        &JValue::obj([
            ("flowId", JValue::Str(flow_id)),
            ("loginUrl", login.clone()),
            (
                "expiresAt",
                JValue::Num((identity::now_ms() + FLOW_TTL_MS) as f64),
            ),
        ]),
    )
}

/// #1433: Cancel in Settings ends the flow here too, so a poll already on its way cannot save.
async fn cancel(ctx: &Arc<Ctx>, body: &JValue) -> Response<Body> {
    let flow_id = or_empty(body.get("flowId"));
    let user = ctx.user();
    let done = with_flows(ctx, move |flows, file, keys, now| {
        let flow = flows.get(file, keys, &flow_id, now)?;
        let cancelled = flow.is_some_and(|f| f.user_id == user);
        if cancelled {
            flows.delete(file, &flow_id)?;
        }
        Ok::<bool, std::io::Error>(cancelled)
    })
    .await;
    match done {
        Some(Ok(cancelled)) => answer(
            StatusCode::OK,
            &JValue::obj([("cancelled", JValue::Bool(cancelled))]),
        ),
        Some(Err(_)) => internal(),
        None => unavailable(),
    }
}

async fn poll(ctx: &Arc<Ctx>, body: &JValue) -> Response<Body> {
    let flow_id = or_empty(body.get("flowId"));
    let looked = {
        let id = flow_id.clone();
        with_flows(ctx, move |flows, file, keys, now| {
            flows.get(file, keys, &id, now)
        })
        .await
    };
    let flow = match looked {
        Some(Ok(flow)) => flow,
        Some(Err(_)) => return internal(),
        None => return unavailable(),
    };
    let now = identity::now_ms() as f64;
    let Some(flow) = flow.filter(|f| f.user_id == ctx.user() && f.expires >= now) else {
        return coded(400, "login flow expired", "nextcloudFlowExpired");
    };
    // The poll endpoint came from the remote server's own reply: guarded too.
    if !endpoint_approved(ctx.admin(), &flow.endpoint) {
        return error(400, PRIVATE_URL);
    }
    let host = host_of(&flow.endpoint);
    let form = Zeroizing::new(
        url::form_urlencoded::Serializer::new(String::new())
            .append_pair("token", &flow.token)
            .finish(),
    );
    let headers = [
        ("Content-Type", "application/x-www-form-urlencoded"),
        ("User-Agent", CLIENT_NAME),
    ];
    let reply = match fetch(
        ctx.client(),
        "POST",
        &flow.endpoint,
        &headers,
        Some(form.to_string()),
        NEXTCLOUD_TIMEOUT,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => return send(transport_failure(e, &host)),
    };
    if reply.status == 404 {
        discard(reply);
        return answer(
            StatusCode::ACCEPTED,
            &JValue::obj([("pending", JValue::Bool(true))]),
        );
    }
    if !reply.ok() {
        let wait = retry_after(reply.retry_after.as_deref());
        return send(status_failure(reply.status, &host, wait));
    }
    let credentials = match reply.json(NEXTCLOUD_REPLY_CAP).await {
        Ok(c) => c,
        Err(true) => return error(502, "Nextcloud reply too large"),
        Err(false) => return error(502, "Nextcloud returned an unreadable reply"),
    };
    // Nextcloud hands the app password out once: the flow is spent whatever happens next. A flow
    // that is no longer the stored one was cancelled (or replaced) while this poll was in flight.
    let spent = {
        let (id, nonce) = (flow_id.clone(), flow.nonce.clone());
        with_flows(ctx, move |flows, file, keys, now| {
            let current = flows.get(file, keys, &id, now)?;
            let wanted = current.is_some_and(|f| f.nonce == nonce);
            flows.delete(file, &id)?;
            Ok::<bool, std::io::Error>(wanted)
        })
        .await
    };
    let still_wanted = match spent {
        Some(Ok(w)) => w,
        Some(Err(_)) => return internal(),
        None => return unavailable(),
    };
    let server = nextcloud_server_root(&or_empty(credentials.get("server")));
    let login_name = or_empty(credentials.get("loginName"));
    let app_password = Zeroizing::new(or_empty(credentials.get("appPassword")));
    if server.is_empty() || login_name.is_empty() || app_password.is_empty() {
        return error(502, "Nextcloud returned an unreadable reply");
    }
    if !is_https(&server) {
        return coded(502, HTTPS_REQUIRED, "nextcloudHttpsRequired");
    }
    if origin_of(&server) != flow.origin {
        return answer(
            StatusCode::BAD_GATEWAY,
            &origin_mismatch(&flow.origin, &server),
        );
    }
    if !endpoint_approved(ctx.admin(), &server) {
        return error(403, PRIVATE_URL);
    }
    let auth = basic(&login_name, &app_password);
    // From here the server is trusted (https, same origin, approved): every exit that does not
    // save the app password asks Nextcloud to delete it, best effort.
    let revoke = || async {
        let headers = [
            ("Authorization", auth.as_str()),
            ("OCS-APIRequest", "true"),
            ("User-Agent", CLIENT_NAME),
        ];
        let url = format!("{server}/ocs/v2.php/core/apppassword");
        if let Ok(reply) = fetch(
            ctx.client(),
            "DELETE",
            &url,
            &headers,
            None,
            NEXTCLOUD_TIMEOUT,
        )
        .await
        {
            discard(reply);
        }
    };
    if !still_wanted {
        revoke().await;
        return coded(409, FLOW_CANCELLED, "nextcloudFlowCancelled");
    }
    // #1427: the WebDAV home is /files/<user id>; ask Nextcloud who this app password is.
    let mut user_id = String::new();
    let who_headers = [
        ("Authorization", auth.as_str()),
        ("OCS-APIRequest", "true"),
        ("Accept", "application/json"),
        ("User-Agent", CLIENT_NAME),
    ];
    let who_url = format!("{server}/ocs/v2.php/cloud/user?format=json");
    if let Ok(who) = fetch(
        ctx.client(),
        "GET",
        &who_url,
        &who_headers,
        None,
        NEXTCLOUD_TIMEOUT,
    )
    .await
    {
        if who.status == 401 {
            discard(who);
            revoke().await;
            return coded(
                502,
                "Nextcloud issued an app password and then refused it. Sign in again.",
                "storageLoginRejected",
            );
        }
        if who.status == 429 {
            let wait = retry_after(who.retry_after.as_deref());
            discard(who);
            revoke().await;
            return send(status_failure(429, &host, wait));
        }
        if who.ok() {
            if let Ok(v) = who.json(NEXTCLOUD_REPLY_CAP).await {
                user_id = or_empty(v.get("ocs").get("data").get("id"));
            }
        } else {
            discard(who);
        }
    }
    let home = if user_id.is_empty() {
        &login_name
    } else {
        &user_id
    };
    let base_url = format!(
        "{server}/remote.php/dav/files/{}",
        encode_uri_component(home)
    );
    if !endpoint_approved(ctx.admin(), &base_url) {
        revoke().await;
        return error(403, PRIVATE_URL);
    }
    // Keep the folder the person already uses; a fresh setup gets the default.
    let mut corpus_root = match body.get("corpusRoot") {
        JValue::Str(s) => js_trim(s).to_string(),
        _ => String::new(),
    };
    if corpus_root.is_empty() {
        corpus_root = ctx
            .saved()
            .await
            .map(|s| or_empty(s.get("corpusRoot")))
            .unwrap_or_default();
    }
    if corpus_root.is_empty() {
        corpus_root = "Cowork/Diary".into();
    }
    // Same check as a manual save (#770): a refused login never replaces the working row.
    let check = check_login(ctx.client(), &base_url, &login_name, &app_password).await;
    match check {
        Check::Rejected => {
            revoke().await;
            return answer(
                StatusCode::BAD_GATEWAY,
                &JValue::obj([
                    ("error", JValue::Str(LOGIN_REJECTED_NEW.into())),
                    ("code", JValue::Str("storageLoginRejected".into())),
                    ("status", JValue::Num(401.0)),
                ]),
            );
        }
        Check::Throttled(wait) => {
            revoke().await;
            return send(status_failure(429, &host, wait));
        }
        _ => {}
    }
    let saved = match save(ctx, &base_url, &login_name, &app_password, &corpus_root).await {
        Some(s) => s,
        None => return unavailable(),
    };
    let mut items = match saved {
        JValue::Obj(items) => items,
        _ => Vec::new(),
    };
    if let Check::Unverified(reason, status) = check {
        items.push((
            "warningCode".into(),
            JValue::Str("storageUnverified".into()),
        ));
        match status {
            Some(status) => {
                items.push(("status".into(), JValue::Num(f64::from(status))));
                items.push((
                    "warning".into(),
                    JValue::Str(format!(
                        "Saved, but the login could not be checked (the storage server answered {status})."
                    )),
                ));
            }
            None => {
                items.push(("reason".into(), JValue::Str(reason.into())));
                let warning = if reason == "timeout" {
                    STORAGE_TIMED_OUT
                } else {
                    STORAGE_UNVERIFIED
                };
                items.push(("warning".into(), JValue::Str(warning.into())));
            }
        }
    }
    answer(StatusCode::OK, &JValue::Obj(items))
}

/// core auth.cjs `saveStorage(userId, {kind:'nextcloud', baseUrl, username, secret, corpusRoot})`
/// and its `storage.update` audit row in one transaction; answers `getStorage(userId)`.
async fn save(
    ctx: &Arc<Ctx>,
    base_url: &str,
    username: &str,
    secret: &str,
    corpus_root: &str,
) -> Option<JValue> {
    let ctx2 = Arc::clone(ctx);
    let (base_url, username, corpus_root) = (
        strip_slashes(base_url).to_string(),
        username.to_string(),
        corpus_root.to_string(),
    );
    let secret = Zeroizing::new(secret.to_string());
    let written = tokio::task::spawn_blocking(move || -> Result<(), StoreError> {
        let keys = ctx2.keys().ok_or(StoreError::Poisoned)?;
        let user = ctx2.user();
        let sealed = keys
            .seal(&secret, Some(&user), &random::<12>())
            .map_err(|_| StoreError::Poisoned)?;
        let now = identity::now_ms();
        ctx2.app.writes.writer()?.write(|tx| {
            tx.save_storage_connection(
                RustStorageIntegrations::owned(),
                &user,
                "nextcloud",
                &base_url,
                "",
                s3_sign::DEFAULT_REGION,
                &username,
                &sealed,
                &corpus_root,
                now,
            )?;
            tx.execute(
                "INSERT INTO audit_events(actor_user_id,target_user_id,action,detail,created_at) VALUES(?1,?1,'storage.update',?2,?3)",
                server_store::rusqlite::params![user, r#"{"kind":"nextcloud"}"#, now],
            )?;
            Ok(())
        })
    })
    .await
    .ok()?;
    if written.is_err() {
        eprintln!("noevia-server: storage sign-in could not be saved");
        return None;
    }
    let mut saved = ctx.saved().await?;
    // getStorage(userId) without the secret.
    if let JValue::Obj(items) = &mut saved {
        items.retain(|(k, _)| k != "secret");
    }
    Some(saved)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn server_root_is_core_nextcloud_server_root() {
        for (raw, want) in [
            ("https://cloud.example.com", "https://cloud.example.com"),
            (
                "  https://cloud.example.com/  ",
                "https://cloud.example.com",
            ),
            (
                "https://cloud.example.com/index.php/apps/files/?dir=/",
                "https://cloud.example.com",
            ),
            (
                "https://cloud.example.com/nc/remote.php/dav/files/alice",
                "https://cloud.example.com/nc",
            ),
            ("https://x.test:8443/Apps", "https://x.test:8443"),
            ("https://x.test/appsfoo", "https://x.test/appsfoo"),
            ("https://x.test/a/login", "https://x.test/a"),
            ("https://x.test:443/status.php", "https://x.test"),
            ("http://x.test/ocs/v2.php", "http://x.test"),
            ("https://u:p@x.test", ""),
            ("cloud.example.com", ""),
            ("", ""),
        ] {
            assert_eq!(nextcloud_server_root(raw), want, "{raw}");
        }
    }

    #[test]
    fn encode_uri_component_is_js() {
        assert_eq!(encode_uri_component("a b/c"), "a%20b%2Fc");
        assert_eq!(encode_uri_component("A-z_0.!~*'()"), "A-z_0.!~*'()");
        assert_eq!(encode_uri_component("é@+"), "%C3%A9%40%2B");
    }

    #[test]
    fn retry_after_is_core_number_rule() {
        assert_eq!(retry_after(Some("30")), Some(30.0));
        assert_eq!(retry_after(Some(" 1.2 ")), Some(2.0));
        assert_eq!(retry_after(Some("999999")), Some(86_400.0));
        assert_eq!(retry_after(Some("0")), None);
        assert_eq!(retry_after(Some("-3")), None);
        assert_eq!(retry_after(Some("soon")), None);
        assert_eq!(retry_after(Some("")), None);
        assert_eq!(retry_after(None), None);
    }

    #[test]
    fn uuid_is_v4() {
        let id = uuid_v4();
        assert_eq!(id.len(), 36);
        assert_eq!(id.get(14..15), Some("4"));
        assert!(matches!(id.get(19..20), Some("8" | "9" | "a" | "b")));
    }

    #[test]
    fn fingerprint_uses_core_fields_only() {
        let a = js_json::parse(
            r#"{"kind":"webdav","baseUrl":"https://d","bucket":"","username":"u","secret":"s","corpusRoot":"x"}"#,
        )
        .unwrap();
        let b = js_json::parse(
            r#"{"kind":"webdav","baseUrl":"https://d","username":"u","secret":"s","region":"eu"}"#,
        )
        .unwrap();
        assert!(Fingerprint::of(&a) == Fingerprint::of(&b));
        let c = js_json::parse(
            r#"{"kind":"webdav","baseUrl":"https://d/","username":"u","secret":"s"}"#,
        )
        .unwrap();
        assert!(Fingerprint::of(&a) != Fingerprint::of(&c));
    }
}
