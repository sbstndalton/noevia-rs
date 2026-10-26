//! Bounded, read-only Diary file transport for an authenticated DAV principal.
//!
//! This is an API-only source leaf. The fixed endpoints are GET /api/files and
//! POST /api/file (the companion's read endpoint). No mutation URL is present.
//! This helper is not a server-dav Backend and does not call AuthGate.allowed.
//! A production adapter must recheck live authorization at every operation boundary.

use crate::BoundIdentity;
use base64::Engine as _;
use rand_core::{OsRng, RngCore};
use reqwest::{Client, StatusCode, Url};
use serde::de::{self, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fmt;
use std::net::SocketAddr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tenant_assertion::Request as AssertionRequest;
use zeroize::Zeroizing;

const MAX_LIST_BYTES: usize = 1024 * 1024;
const MAX_FILE_REPLY_BYTES: usize = 4 * 1024 * 1024;
const MAX_FILE_BYTES: usize = 512 * 1024;
const MAX_CHILDREN: usize = 1024;
const STORAGE_JSON: &str = r#"{"kind":"local"}"#;

/// Errors contain only a status or a fixed category, never request or secret text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadError {
    Configuration,
    InvalidPath,
    InvalidIdentity,
    Refused(u16),
    Redirect,
    TooLarge,
    Malformed,
    Timeout,
    Transport,
}

impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Diary read transport: {self:?}")
    }
}
impl std::error::Error for ReadError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiaryEntry {
    pub path: String,
    pub name: String,
    pub is_dir: bool,
    pub modified_unix: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiaryFile {
    pub content: Option<String>,
    pub version: Option<String>,
}

/// A fixed loopback transport. Keys and bearer tokens are redacted from Debug.
pub struct DiaryReadClient {
    origin: Url,
    http: Client,
    tenant_key: Zeroizing<String>,
    service_token: Zeroizing<String>,
    storage: String,
    timeout: Duration,
}

impl fmt::Debug for DiaryReadClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DiaryReadClient")
            .field("origin", &self.origin)
            .field("tenant_key", &"[redacted]")
            .field("service_token", &"[redacted]")
            .field("storage", &"local")
            .field("timeout", &self.timeout)
            .finish()
    }
}

impl DiaryReadClient {
    /// Only a numeric loopback socket may be used. No DNS, proxy, caller URL or
    /// incoming request header is accepted. The key is trimmed as in Python.
    pub fn new(
        target: SocketAddr,
        tenant_key: String,
        service_token: String,
        timeout: Duration,
    ) -> Result<Self, ReadError> {
        let key = tenant_key.trim();
        if !target.ip().is_loopback()
            || target.port() == 0
            || key.is_empty()
            || key.len() > 4096
            || key.chars().any(char::is_control)
            || service_token.is_empty()
            || service_token.len() > 4096
            || !service_token.bytes().all(|byte| byte.is_ascii_graphic())
            || timeout.is_zero()
            || timeout > Duration::from_secs(60)
        {
            return Err(ReadError::Configuration);
        }
        let origin =
            Url::parse(&format!("http://{target}/")).map_err(|_| ReadError::Configuration)?;
        let http = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .referer(false)
            .timeout(timeout)
            .connect_timeout(timeout.min(Duration::from_secs(5)))
            .pool_max_idle_per_host(0)
            .build()
            .map_err(|_| ReadError::Configuration)?;
        Ok(Self {
            origin,
            http,
            tenant_key: Zeroizing::new(key.to_owned()),
            service_token: Zeroizing::new(service_token),
            storage: base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(STORAGE_JSON),
            timeout,
        })
    }

    /// GET /api/files?path=...; no file contents are returned by this method.
    pub async fn list(
        &self,
        identity: &BoundIdentity,
        path: &str,
    ) -> Result<Vec<DiaryEntry>, ReadError> {
        let route = encode_list_path(path)?;
        let (wire_path, query) = route.split_once('?').ok_or(ReadError::Configuration)?;
        let response = self
            .request(
                identity,
                reqwest::Method::GET,
                wire_path,
                query.as_bytes(),
                None,
                MAX_LIST_BYTES,
            )
            .await?;
        parse_list(&response, path)
    }

    /// POST /api/file is the companion's read-only route, despite its HTTP verb.
    pub async fn read(&self, identity: &BoundIdentity, path: &str) -> Result<DiaryFile, ReadError> {
        let body = encode_file_read_body(path)?;
        let response = self
            .request(
                identity,
                reqwest::Method::POST,
                "/api/file",
                b"",
                Some(&body),
                MAX_FILE_REPLY_BYTES,
            )
            .await?;
        parse_file(&response, path)
    }

    async fn request(
        &self,
        identity: &BoundIdentity,
        method: reqwest::Method,
        path: &str,
        query: &[u8],
        body: Option<&[u8]>,
        response_cap: usize,
    ) -> Result<Vec<u8>, ReadError> {
        if !valid_user_id(identity.user_id()) || identity.credential_id().is_empty() {
            return Err(ReadError::InvalidIdentity);
        }
        let work = async {
            let mut url = self
                .origin
                .join(path)
                .map_err(|_| ReadError::Configuration)?;
            if !query.is_empty() {
                let query = std::str::from_utf8(query).map_err(|_| ReadError::Configuration)?;
                url.set_query(Some(query));
            }
            let wire_query = url.query().unwrap_or("").as_bytes();
            let mut nonce_bytes = [0u8; 16];
            OsRng.fill_bytes(&mut nonce_bytes);
            let nonce = hex(&nonce_bytes);
            let ts = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|_| ReadError::Configuration)?
                .as_secs();
            let body_hash = hex(&Sha256::digest(body.unwrap_or_default()));
            let assertion = tenant_assertion::sign(
                &AssertionRequest {
                    key: &self.tenant_key,
                    user_id: identity.user_id(),
                    assertion: "",
                    method: method.as_str(),
                    path,
                    query: wire_query,
                    body_hash: &body_hash,
                    storage: &self.storage,
                    legacy_owner: "",
                    blocked: "",
                    now: ts as f64,
                },
                ts,
                &nonce,
            )
            .ok_or(ReadError::Configuration)?;
            let mut builder = self
                .http
                .request(method, url)
                .header(
                    "authorization",
                    format!("Bearer {}", self.service_token.as_str()),
                )
                .header("x-cowork-user-id", identity.user_id())
                .header("x-cowork-storage", &self.storage)
                .header("x-cowork-tenant-assertion", assertion)
                .header("accept", "application/json");
            if let Some(body) = body {
                builder = builder
                    .header("content-type", "application/json")
                    .body(body.to_vec());
            }
            let mut response = builder.send().await.map_err(map_reqwest)?;
            let status = response.status();
            if status.is_redirection() {
                return Err(ReadError::Redirect);
            }
            if !status.is_success() {
                return Err(ReadError::Refused(status.as_u16()));
            }
            if status != StatusCode::OK {
                return Err(ReadError::Refused(status.as_u16()));
            }
            if response
                .content_length()
                .is_some_and(|n| n > response_cap as u64)
            {
                return Err(ReadError::TooLarge);
            }
            let mut bytes = Vec::new();
            while let Some(chunk) = response.chunk().await.map_err(map_reqwest)? {
                if chunk.len() > response_cap.saturating_sub(bytes.len()) {
                    return Err(ReadError::TooLarge);
                }
                bytes.extend_from_slice(&chunk);
            }
            Ok(bytes)
        };
        tokio::time::timeout(self.timeout, work)
            .await
            .map_err(|_| ReadError::Timeout)?
    }
}

fn map_reqwest(error: reqwest::Error) -> ReadError {
    if error.is_timeout() {
        ReadError::Timeout
    } else {
        ReadError::Transport
    }
}

fn valid_user_id(value: &str) -> bool {
    // Database-bound IDs need not be UUID-shaped (older and synthetic users
    // exist). Refuse values that cannot safely be a tenant header/signature.
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn valid_path(path: &str, directory: bool) -> bool {
    if path.len() > 500
        || path
            .chars()
            .any(|c| c == '\\' || c == '%' || c.is_control())
    {
        return false;
    }
    if path.is_empty() {
        return directory;
    }
    if !directory && !path.to_ascii_lowercase().ends_with(".md") {
        return false;
    }
    path.split('/')
        .all(|part| !part.is_empty() && !part.starts_with('.'))
}

/// Pure, fixed-endpoint encoder. `path` is a tenant-relative directory name.
pub fn encode_list_path(path: &str) -> Result<String, ReadError> {
    if !valid_path(path, true) {
        return Err(ReadError::InvalidPath);
    }
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("path", path)
        .finish();
    Ok(format!("/api/files?{query}"))
}

/// Pure JSON body encoder for the companion's fixed read-only POST endpoint.
pub fn encode_file_read_body(path: &str) -> Result<Vec<u8>, ReadError> {
    if !valid_path(path, false) {
        return Err(ReadError::InvalidPath);
    }
    serde_json::to_vec(&json!({"path": path})).map_err(|_| ReadError::Malformed)
}

/// The array visitor refuses a 1025th child before deserializing it into a
/// Value. The outer HTTP byte cap also bounds any single row or extra field.
struct BoundedRows(Vec<Value>);

impl<'de> Deserialize<'de> for BoundedRows {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Rows;
        impl<'de> Visitor<'de> for Rows {
            type Value = BoundedRows;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a bounded list of direct children")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut rows = Vec::new();
                while rows.len() < MAX_CHILDREN {
                    match seq.next_element::<Value>()? {
                        Some(row) => rows.push(row),
                        None => return Ok(BoundedRows(rows)),
                    }
                }
                if seq.next_element::<IgnoredAny>()?.is_some() {
                    return Err(de::Error::custom("diary-list-child-limit"));
                }
                Ok(BoundedRows(rows))
            }
        }
        deserializer.deserialize_seq(Rows)
    }
}

struct ListEnvelope(BoundedRows);

impl<'de> Deserialize<'de> for ListEnvelope {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Envelope;
        impl<'de> Visitor<'de> for Envelope {
            type Value = ListEnvelope;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a Diary file-list object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut rows = None;
                while let Some(key) = map.next_key::<String>()? {
                    if key == "files" {
                        if rows.is_some() {
                            return Err(de::Error::duplicate_field("files"));
                        }
                        rows = Some(map.next_value::<BoundedRows>()?);
                    } else {
                        map.next_value::<IgnoredAny>()?;
                    }
                }
                rows.map(ListEnvelope)
                    .ok_or_else(|| de::Error::missing_field("files"))
            }
        }
        deserializer.deserialize_map(Envelope)
    }
}

fn parse_list(bytes: &[u8], parent: &str) -> Result<Vec<DiaryEntry>, ReadError> {
    let envelope: ListEnvelope = serde_json::from_slice(bytes).map_err(|error| {
        if error.to_string().starts_with("diary-list-child-limit") {
            ReadError::TooLarge
        } else {
            ReadError::Malformed
        }
    })?;
    let rows = envelope.0 .0;
    let mut seen = HashSet::new();
    let mut output = Vec::with_capacity(rows.len());
    for row in rows {
        let name = row
            .get("name")
            .and_then(Value::as_str)
            .ok_or(ReadError::Malformed)?;
        if name.is_empty() || name.contains('/') || name.starts_with('.') {
            return Err(ReadError::Malformed);
        }
        let path = row
            .get("path")
            .and_then(Value::as_str)
            .ok_or(ReadError::Malformed)?;
        let is_dir = row
            .get("isDir")
            .and_then(Value::as_bool)
            .ok_or(ReadError::Malformed)?;
        let expected = if parent.is_empty() {
            name.to_owned()
        } else {
            format!("{parent}/{name}")
        };
        if path != expected || !valid_path(path, is_dir) || !seen.insert(path.to_owned()) {
            return Err(ReadError::Malformed);
        }
        let modified_unix = match row.get("modified") {
            None | Some(Value::Null) => None,
            Some(v) => {
                let n = v.as_f64().ok_or(ReadError::Malformed)?;
                if !n.is_finite() || n <= 0.0 || n >= u64::MAX as f64 {
                    return Err(ReadError::Malformed);
                }
                Some(n as u64)
            }
        };
        output.push(DiaryEntry {
            path: path.to_owned(),
            name: name.to_owned(),
            is_dir,
            modified_unix,
        });
    }
    Ok(output)
}

fn parse_file(bytes: &[u8], expected_path: &str) -> Result<DiaryFile, ReadError> {
    let value: Value = serde_json::from_slice(bytes).map_err(|_| ReadError::Malformed)?;
    if value.get("path").and_then(Value::as_str) != Some(expected_path) {
        return Err(ReadError::Malformed);
    }
    match (value.get("content"), value.get("version")) {
        (Some(Value::Null), Some(Value::Null)) => Ok(DiaryFile {
            content: None,
            version: None,
        }),
        (Some(Value::String(content)), Some(Value::String(version))) => {
            if content.len() > MAX_FILE_BYTES
                || version.len() != 64
                || !version
                    .bytes()
                    .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
                || hex(&Sha256::digest(content.as_bytes())) != *version
            {
                return Err(ReadError::Malformed);
            }
            Ok(DiaryFile {
                content: Some(content.clone()),
                version: Some(version.clone()),
            })
        }
        _ => Err(ReadError::Malformed),
    }
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(
            DIGITS.get((byte >> 4) as usize).copied().unwrap_or(b'0'),
        ));
        output.push(char::from(
            DIGITS.get((byte & 15) as usize).copied().unwrap_or(b'0'),
        ));
    }
    output
}
