//! noevia#1318: native `POST /api/v1/models/check-updates`.
//!
//! Python (`api.check_updates`, `hf.check_updates_for`, `hf.repo_file_mtimes`):
//!
//! ```python
//! records = db.download_records()
//! if records:
//!     await hf.check_updates_for(records)
//! return {"checked": len(records), "status": _update_status_map(services.snapshot_models_dir())}
//! ```
//!
//! For each repo (in the order its first record appears), a repo id without `/` (a URL import)
//! gets an empty check; any other asks the Hub's tree listing once and records each file's
//! `lastCommit.date`, all with one `time.time()`. The status map is the native
//! `GET /api/v1/models/updates` answer, read after the writes. Every place Python's handler raises
//! answers FastAPI's plain 500, after the writes it had already made. Deliberate differences are
//! in `docs/native-check-updates.md`.
use crate::model_updates::{self, py_float_repr, ModelUpdates};
use model_files::json::{Limits, Value as Json};
use rusqlite::{types::Value, Connection, OpenFlags};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

/// `hf.HF_API`.
pub const HF_API: &str = "https://huggingface.co/api";
/// noevia#1996: largest tree listing the update check reads; a larger one is treated as a failed
/// request (no dates). The check reads one page per repository (the Hub pages the listing; real
/// pages are a few thousand entries at most, well under 1 MiB).
pub const UPDATE_PAGE_CAP: usize = 16 * 1024 * 1024;
/// noevia#1996: most JSON values and keys in that page (an `expand=true` entry is about 30-60, so
/// several thousand entries). A page of bare numbers at the cap would be about 8 million, each an
/// allocated big integer. Over it, a failed request.
pub const UPDATE_PAGE_NODES: usize = 256 * 1024;
/// noevia#1996: longest string in that page, in characters (paths, dates and commit titles are
/// far shorter). Over it, a failed request.
pub const UPDATE_PAGE_STRING_CHARS: usize = 64 * 1024;
/// Most download records handled; more answers 500 instead of thousands of Hub requests.
pub const RECORD_CAP: usize = 4096;
/// httpx's `timeout=15.0` (connect and each read).
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);
/// A bound httpx does not have: the whole of one tree request.
const REQUEST_BUDGET: Duration = Duration::from_secs(120);
/// sqlite3's default `timeout=5.0`.
const BUSY_WAIT: Duration = Duration::from_millis(5000);
/// CPython parses deeper JSON (until RecursionError); this bound is a failed request instead.
pub(crate) const JSON_DEPTH: usize = 512;

pub type Clock = fn() -> f64;

fn now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Json(Vec<u8>),
    /// Python's handler raised: FastAPI's plain 500.
    Failed,
    /// The native status walk could not answer: the model-updates 503 refusal.
    Refused,
}

pub struct CheckUpdates {
    database: PathBuf,
    updates: ModelUpdates,
    hf_api: String,
    clock: Clock,
    client: reqwest::Client,
    /// Lib tests: runs in each blocking task, before its work.
    #[cfg(test)]
    during: Option<Arc<dyn Fn() + Send + Sync>>,
}

/// Any reason Python's handler raises.
#[derive(Debug)]
struct Raised;

impl CheckUpdates {
    /// `models`, `ini` and `data` are Python's `MODELS_DIR`, `MODELS_INI_PATH` and `DATA_DIR`.
    pub fn new(models: &Path, ini: &Path, data: &Path) -> Result<Self, &'static str> {
        Self::with(models, ini, data, HF_API, now)
    }

    /// Tests point the Hub at a local fake and fix the clock.
    pub fn with(
        models: &Path,
        ini: &Path,
        data: &Path,
        hf_api: &str,
        clock: Clock,
    ) -> Result<Self, &'static str> {
        let client = reqwest::Client::builder()
            .connect_timeout(HTTP_TIMEOUT)
            .read_timeout(HTTP_TIMEOUT)
            .timeout(REQUEST_BUDGET)
            .redirect(reqwest::redirect::Policy::limited(20))
            .build()
            .map_err(|_| "Hugging Face client could not be built")?;
        Ok(Self {
            database: data.join("model_loader.db"),
            updates: ModelUpdates::new(models, ini, data),
            hf_api: hf_api.trim_end_matches('/').to_owned(),
            clock,
            client,
            #[cfg(test)]
            during: None,
        })
    }

    pub async fn post(&self) -> Outcome {
        let checked = match self.check().await {
            Ok(n) => n,
            Err(Raised) => return Outcome::Failed,
        };
        match self.updates.response().await {
            model_updates::Outcome::Json(bytes) => {
                splice(checked, &bytes).map_or(Outcome::Failed, Outcome::Json)
            }
            model_updates::Outcome::Failed => Outcome::Failed,
            model_updates::Outcome::Refused => Outcome::Refused,
        }
    }

    /// `db.download_records()` and `hf.check_updates_for(records)`; the number of records.
    async fn check(&self) -> Result<usize, Raised> {
        let database = self.database.clone();
        let (db, records, token) = self
            .blocking(move || {
                let db = open(&database)?;
                let records = download_records(&db)?;
                // `get_token()` runs when the first Hub request is built; its error surfaces there.
                let token = if records.is_empty() {
                    Ok(None)
                } else {
                    read_token(&db).map_err(|Raised| ())
                };
                Ok((db, records, token))
            })
            .await?;
        if records.is_empty() {
            return Ok(0);
        }
        let count = records.len();
        let db = Arc::new(Mutex::new(db));
        let now = (self.clock)();
        for (repo, files) in group(records) {
            // `"/" not in repo_id` raises for anything but str.
            let Value::Text(repo) = repo else {
                return Err(Raised);
            };
            let mtimes = if repo.contains('/') {
                let token = token.as_ref().map_err(|()| Raised)?;
                self.mtimes(&repo, token.as_ref()).await?
            } else {
                Vec::new()
            };
            let rows: Vec<(Value, Option<Json>)> = files
                .into_iter()
                .map(|f| {
                    let last = match &f {
                        Value::Text(name) => mtimes
                            .iter()
                            .rev()
                            .find(|(p, _)| p == name)
                            .map(|(_, last)| last.clone()),
                        _ => None,
                    };
                    (f, last)
                })
                .collect();
            let db = db.clone();
            let repo = Value::Text(repo);
            self.blocking(move || {
                let db = db.lock().map_err(|_| Raised)?;
                for (f, last) in rows {
                    // Bound as sqlite3 binds it when this file's row is written.
                    let last = last.as_ref().map_or(Ok(Value::Null), bind)?;
                    set_update_check(&db, f, repo.clone(), last, now)?;
                }
                Ok(())
            })
            .await?;
        }
        Ok(count)
    }

    /// `hf.repo_file_mtimes(repo_id)` inside `check_updates_for`'s `except httpx.HTTPError`:
    /// `Ok(pairs)` (empty for any failed request), `Err` where Python raises past it.
    async fn mtimes(
        &self,
        repo: &str,
        token: Option<&Token>,
    ) -> Result<Vec<(String, Json)>, Raised> {
        let header = match token.map(Token::header) {
            None => None,
            // httpx encodes header values as ASCII when the client is built: past the except.
            Some(Header::NotAscii) => return Err(Raised),
            // h11 refuses the value when sending: httpx.LocalProtocolError, an HTTPError.
            Some(Header::Refused) => return Ok(Vec::new()),
            Some(Header::Value(v)) => Some(v),
        };
        if !hub_id(repo) {
            return Ok(Vec::new());
        }
        let url = format!(
            "{}/models/{repo}/tree/main?recursive=true&expand=true",
            self.hf_api
        );
        let mut request = self.client.get(url);
        if let Some(value) = header {
            request = request.header(reqwest::header::AUTHORIZATION, value);
        }
        let Ok(response) = request.send().await else {
            return Ok(Vec::new());
        };
        if response.status() != reqwest::StatusCode::OK {
            return Ok(Vec::new());
        }
        let Some(body) = read_capped(response.bytes_stream(), UPDATE_PAGE_CAP).await else {
            return Ok(Vec::new());
        };
        self.blocking(move || entries(body)).await
    }

    /// noevia#1998: the store reads and writes and the tree parse run admitted: a dropped
    /// request keeps its admission permit until the blocking task has returned.
    async fn blocking<T: Send + 'static>(
        &self,
        f: impl FnOnce() -> Result<T, Raised> + Send + 'static,
    ) -> Result<T, Raised> {
        #[cfg(test)]
        let during = self.during.clone();
        crate::docker::spawn_blocking_admitted(move || {
            #[cfg(test)]
            if let Some(during) = during {
                during();
            }
            f()
        })
        .await
        .map_err(|_| Raised)?
    }
}

/// `{"status": …}` from the status read becomes `{"checked": n, "status": …}`.
fn splice(checked: usize, status: &[u8]) -> Option<Vec<u8>> {
    let rest = status.strip_prefix(b"{")?;
    if !rest.starts_with(b"\"status\":") {
        return None;
    }
    let mut out = format!("{{\"checked\":{checked},").into_bytes();
    out.extend_from_slice(rest);
    Some(out)
}

/// A response body of at most `cap` bytes (`None` past it or on a transport error). The buffer
/// grows by doubling but never beyond `cap` (noevia#1996).
async fn read_capped<S, E>(mut stream: S, cap: usize) -> Option<Vec<u8>>
where
    S: futures_util::Stream<Item = Result<bytes::Bytes, E>> + Unpin,
{
    use futures_util::StreamExt;
    let mut body: Vec<u8> = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.ok()?;
        let len = body.len().checked_add(chunk.len())?;
        if len > cap {
            return None;
        }
        if body.capacity() < len {
            let grown = body.capacity().saturating_mul(2).max(len).min(cap);
            body.try_reserve_exact(grown - body.len()).ok()?;
        }
        body.extend_from_slice(&chunk);
    }
    Some(body)
}

/// The parse limits of the update check's page (noevia#1996).
fn page_limits() -> Limits {
    Limits {
        bytes: UPDATE_PAGE_CAP,
        depth: JSON_DEPTH,
        nodes: UPDATE_PAGE_NODES,
        string_chars: UPDATE_PAGE_STRING_CHARS,
        ..Limits::default()
    }
}

/// `tree.json()` of the page: `None` where `json.loads` raises, and (noevia#1996) for a page over
/// the node or string limits, a failed request like a page over the size cap. A UTF-8 page is
/// parsed where it lies; a UTF-16 or UTF-32 one is decoded, its raw bytes dropped and the text
/// shrunk to its length before the parse. Only the tree outlives this call.
fn parse_page(body: Vec<u8>) -> Option<Json> {
    let decoded = match crate::prompts::decode_json_bytes(&body, UPDATE_PAGE_CAP).ok()? {
        std::borrow::Cow::Borrowed(_) => None,
        std::borrow::Cow::Owned(text) => Some(text),
    };
    match decoded {
        Some(mut text) => {
            drop(body);
            text.shrink_to_fit();
            model_files::json::parse_with_limits(&text, page_limits()).ok()
        }
        None => {
            let text = crate::prompts::decode_json_bytes(&body, UPDATE_PAGE_CAP).ok()?;
            model_files::json::parse_with_limits(&text, page_limits()).ok()
        }
    }
}

/// The store as Python's `_conn()` opens it for a write (never created here).
fn open(database: &Path) -> Result<Connection, Raised> {
    let db = Connection::open_with_flags(
        database,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| logged(&e))?;
    db.set_limit(
        rusqlite::limits::Limit::SQLITE_LIMIT_SQL_LENGTH,
        crate::sqlite_read::SQL_CAP,
    )
    .map_err(|e| logged(&e))?;
    db.busy_timeout(BUSY_WAIT).map_err(|e| logged(&e))?;
    Ok(db)
}

fn logged(error: &rusqlite::Error) -> Raised {
    crate::sqlite_read::log_once("check_updates", error);
    Raised
}

/// What sqlite3 hands Python; TEXT that is not UTF-8 raises.
fn sql_value(value: rusqlite::types::ValueRef<'_>) -> Result<Value, Raised> {
    use rusqlite::types::ValueRef;
    Ok(match value {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(n) => Value::Integer(n),
        ValueRef::Real(f) => Value::Real(f),
        ValueRef::Text(t) => Value::Text(std::str::from_utf8(t).map_err(|_| Raised)?.to_owned()),
        ValueRef::Blob(b) => Value::Blob(b.to_vec()),
    })
}

/// `db.download_records()`: `(filename, repo_id)` of the latest done record per pair.
fn download_records(db: &Connection) -> Result<Vec<(Value, Value)>, Raised> {
    let mut statement = db
        .prepare(
            "SELECT filename, repo_id, MAX(id) AS id FROM download_history \
             WHERE status = 'done' GROUP BY filename, repo_id",
        )
        .map_err(|e| logged(&e))?;
    let mut rows = statement.query([]).map_err(|e| logged(&e))?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().map_err(|e| logged(&e))? {
        if out.len() >= RECORD_CAP {
            return Err(Raised);
        }
        let filename = sql_value(row.get_ref(0).map_err(|e| logged(&e))?)?;
        let repo = sql_value(row.get_ref(1).map_err(|e| logged(&e))?)?;
        out.push((filename, repo));
    }
    Ok(out)
}

/// `by_repo.setdefault(r["repo_id"], []).append(r["filename"])`, in first-appearance order.
fn group(records: Vec<(Value, Value)>) -> Vec<(Value, Vec<Value>)> {
    let mut out: Vec<(Value, Vec<Value>)> = Vec::new();
    for (filename, repo) in records {
        match out.iter_mut().find(|(r, _)| same_key(r, &repo)) {
            Some((_, files)) => files.push(filename),
            None => out.push((repo, vec![filename])),
        }
    }
    out
}

/// Python dict-key equality for what sqlite3 hands back (`1 == 1.0`).
fn same_key(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Integer(x), Value::Real(y)) | (Value::Real(y), Value::Integer(x)) => {
            #[allow(clippy::cast_precision_loss)]
            let x = *x as f64;
            x == *y
        }
        _ => a == b,
    }
}

/// `db.get_setting("hf_token", "")`, kept as the Python value it would be.
#[derive(Debug, Clone)]
enum Token {
    Text(String),
    Int(i64),
    Real(f64),
    Blob(Vec<u8>),
}

enum Header {
    Value(reqwest::header::HeaderValue),
    NotAscii,
    Refused,
}

impl Token {
    /// `{"Authorization": f"Bearer {tok}"} if tok else {}`; `None` is the falsy token.
    fn text(&self) -> Option<String> {
        match self {
            Token::Text(t) if t.is_empty() => None,
            Token::Text(t) => Some(t.clone()),
            Token::Int(0) => None,
            Token::Int(n) => Some(n.to_string()),
            Token::Real(f) if *f == 0.0 => None,
            Token::Real(f) if f.is_nan() => Some("nan".into()),
            Token::Real(f) if f.is_infinite() => Some(if *f > 0.0 { "inf" } else { "-inf" }.into()),
            Token::Real(f) => Some(py_float_repr(*f)),
            Token::Blob(b) if b.is_empty() => None,
            Token::Blob(b) => Some(py_bytes_repr(b)),
        }
    }

    fn header(&self) -> Header {
        let Some(token) = self.text() else {
            // Unreachable: a falsy token is never kept.
            return Header::Refused;
        };
        let value = format!("Bearer {token}");
        if !value.is_ascii() {
            return Header::NotAscii;
        }
        if !h11_field_value(value.as_bytes()) {
            return Header::Refused;
        }
        // h11 also sends bytes 0x01-0x08, 0x0e-0x1f and 0x7f, which an HTTP library here refuses;
        // such a token is treated as a request that failed.
        reqwest::header::HeaderValue::from_str(&value).map_or(Header::Refused, Header::Value)
    }
}

/// noevia#1318 (downloads): the stored token as the `Authorization` value `hf._auth_headers`
/// would send, read fresh as `hf.get_token()` is for every request.
#[cfg(any(feature = "native-downloads", feature = "native-draft-heads"))]
pub(crate) enum HubAuth {
    /// A falsy token: no header.
    Absent,
    Value(reqwest::header::HeaderValue),
    /// httpx raises UnicodeEncodeError while encoding the header.
    NotAscii,
    /// h11 refuses the value (LocalProtocolError), or this HTTP library does.
    Refused,
    /// The store could not be read (`db.get_setting` raised).
    Failed,
}

#[cfg(any(feature = "native-downloads", feature = "native-draft-heads"))]
pub(crate) fn hub_auth(database: &Path) -> HubAuth {
    let Ok(db) = open(database) else {
        return HubAuth::Failed;
    };
    match read_token(&db) {
        Err(_) => HubAuth::Failed,
        Ok(None) => HubAuth::Absent,
        Ok(Some(token)) => match token.header() {
            Header::Value(v) => HubAuth::Value(v),
            Header::NotAscii => HubAuth::NotAscii,
            Header::Refused => HubAuth::Refused,
        },
    }
}

/// h11's `field_value`: `([^\x00\s]+([ \t]+[^\x00\s]+)*)?` on bytes.
fn h11_field_value(value: &[u8]) -> bool {
    let vchar = |b: u8| !matches!(b, 0 | b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c);
    if value.is_empty() {
        return true;
    }
    if !value.first().is_some_and(|b| vchar(*b)) || !value.last().is_some_and(|b| vchar(*b)) {
        return false;
    }
    value.iter().all(|b| vchar(*b) || matches!(b, b' ' | b'\t'))
}

/// `repr(bytes)`.
fn py_bytes_repr(bytes: &[u8]) -> String {
    let quote = if bytes.contains(&b'\'') && !bytes.contains(&b'"') {
        '"'
    } else {
        '\''
    };
    let mut out = format!("b{quote}");
    for &b in bytes {
        match b {
            b'\\' => out.push_str("\\\\"),
            b'\t' => out.push_str("\\t"),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b if char::from(b) == quote => {
                out.push('\\');
                out.push(quote);
            }
            0x20..=0x7e => out.push(char::from(b)),
            _ => out.push_str(&format!("\\x{b:02x}")),
        }
    }
    out.push(quote);
    out
}

fn read_token(db: &Connection) -> Result<Option<Token>, Raised> {
    use rusqlite::OptionalExtension;
    let value = db
        .query_row("SELECT value FROM kv WHERE key = ?1", ["hf_token"], |row| {
            Ok(sql_value(row.get_ref(0)?))
        })
        .optional()
        .map_err(|e| logged(&e))?
        .transpose()?;
    let token = match value {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::Text(t)) => Token::Text(t),
        Some(Value::Integer(n)) => Token::Int(n),
        Some(Value::Real(f)) => Token::Real(f),
        Some(Value::Blob(b)) => Token::Blob(b),
    };
    Ok(token.text().map(|_| token))
}

/// Only Hub-shaped ids (`owner/name`, letters, digits, `._-`) are asked about; Python would also
/// send a malformed id, which the Hub does not list.
pub(crate) fn hub_id(repo: &str) -> bool {
    repo.split('/').all(|segment| {
        !segment.is_empty()
            && segment != "."
            && segment != ".."
            && segment
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    })
}

/// `repo_file_mtimes`' parse of a 200 answer: `Ok(pairs)` in insertion order (a later path wins on
/// lookup), `Ok(empty)` for a body `json.loads` refuses, `Err` where the loop raises.
fn entries(body: Vec<u8>) -> Result<Vec<(String, Json)>, Raised> {
    let Some(parsed) = parse_page(body) else {
        return Ok(Vec::new());
    };
    let list = match parsed {
        Json::Arr(list) => list,
        // Iterating a dict or a str gives str items, whose `.get` raises; nothing to iterate is
        // an empty answer.
        Json::Obj(pairs) if pairs.is_empty() => return Ok(Vec::new()),
        Json::Str(s) if s.is_empty() => return Ok(Vec::new()),
        _ => return Err(Raised),
    };
    let mut out = Vec::new();
    for e in &list {
        let Json::Obj(_) = e else {
            return Err(Raised);
        };
        if e.get("type") != Some(&Json::Str("file".into())) {
            continue;
        }
        let path = e.get("path").cloned().unwrap_or(Json::Str(String::new()));
        let last = match e.get("lastCommit") {
            Some(commit) if commit.truthy() => match commit {
                Json::Obj(_) => commit.get("date").cloned().unwrap_or(Json::Null),
                _ => return Err(Raised),
            },
            _ => Json::Null,
        };
        if !path.truthy() || !last.truthy() {
            continue;
        }
        // `out[path] = last`: a list or dict path is unhashable.
        if matches!(path, Json::Arr(_) | Json::Obj(_)) {
            return Err(Raised);
        }
        // Only a str path can equal a stored file name; the value is bound when it is written.
        if let Json::Str(path) = path {
            out.push((path, last));
        }
    }
    Ok(out)
}

/// What sqlite3 stores for a `lastCommit.date` (already truthy).
fn bind(value: &Json) -> Result<Value, Raised> {
    Ok(match value {
        Json::Str(s) => Value::Text(s.clone()),
        Json::Bool(b) => Value::Integer(i64::from(*b)),
        // sqlite3 raises OverflowError past 64 bits.
        Json::Int(i) => Value::Integer(i.to_string().parse().map_err(|_| Raised)?),
        // SQLite stores NaN as NULL.
        Json::Float(f) if f.is_nan() => Value::Null,
        Json::Float(f) => Value::Real(*f),
        // "type 'list' is not supported"
        Json::Null | Json::Arr(_) | Json::Obj(_) => return Err(Raised),
    })
}

/// `db.set_update_check`.
fn set_update_check(
    db: &Connection,
    filename: Value,
    repo: Value,
    last: Value,
    checked_at: f64,
) -> Result<(), Raised> {
    db.execute(
        "INSERT INTO update_check(filename, repo_id, hf_last_modified, checked_at) VALUES(?, ?, ?, ?) \
         ON CONFLICT(filename) DO UPDATE SET repo_id = excluded.repo_id, \
         hf_last_modified = excluded.hf_last_modified, checked_at = excluded.checked_at",
        rusqlite::params![filename, repo, last, checked_at],
    )
    .map_err(|e| logged(&e))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
    use super::*;

    #[test]
    fn header_rules_follow_httpx_and_h11() {
        assert!(h11_field_value(b"Bearer abc"));
        assert!(h11_field_value(b"Bearer  a\tb"));
        assert!(!h11_field_value(b"Bearer abc "));
        assert!(!h11_field_value(b"Bearer a\nb"));
        assert!(!h11_field_value(b"Bearer a\x00"));
        assert!(h11_field_value(b"Bearer a\x01"));
        assert!(matches!(Token::Text("é".into()).header(), Header::NotAscii));
        assert!(matches!(Token::Text("a ".into()).header(), Header::Refused));
        assert!(matches!(
            Token::Text("a\x01".into()).header(),
            Header::Refused
        ));
        assert!(matches!(Token::Int(7).header(), Header::Value(v) if v == "Bearer 7"));
        assert!(matches!(Token::Real(1.5).header(), Header::Value(v) if v == "Bearer 1.5"));
    }

    /// noevia#1952: the Hub client speaks TLS. A client without a TLS backend refuses an
    /// `https://` URL before connecting; this one must open the connection with a ClientHello.
    #[tokio::test]
    async fn the_hub_client_opens_https_connections() {
        let dir = tempfile::tempdir().unwrap();
        let checker = CheckUpdates::with(
            dir.path(),
            &dir.path().join("models.ini"),
            dir.path(),
            HF_API,
            now,
        )
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let request = tokio::spawn({
            let client = checker.client.clone();
            async move {
                client
                    .get(format!("https://127.0.0.1:{port}/api/models"))
                    .send()
                    .await
            }
        });
        let accepted =
            tokio::time::timeout(std::time::Duration::from_secs(20), listener.accept()).await;
        assert!(
            accepted.is_ok(),
            "no connection: the client cannot open https URLs"
        );
        let (mut socket, _) = accepted.unwrap().unwrap();
        let mut hello = [0u8; 6];
        tokio::time::timeout(
            std::time::Duration::from_secs(20),
            tokio::io::AsyncReadExt::read_exact(&mut socket, &mut hello),
        )
        .await
        .unwrap()
        .unwrap();
        // TLS record type 22 (handshake), message type 1 (ClientHello).
        assert_eq!((hello[0], hello[5]), (0x16, 0x01));
        drop(socket);
        // Nothing here can complete a verified handshake.
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(20), request)
            .await
            .unwrap()
            .unwrap();
        assert!(outcome.is_err());
    }

    #[test]
    fn bytes_repr_is_python_repr() {
        assert_eq!(py_bytes_repr(b"ab"), "b'ab'");
        assert_eq!(py_bytes_repr(b"a'b"), "b\"a'b\"");
        assert_eq!(py_bytes_repr(b"a'\"b"), "b'a\\'\"b'");
        assert_eq!(py_bytes_repr(b"\x00\t\\\xff"), "b'\\x00\\t\\\\\\xff'");
    }

    #[test]
    fn hub_ids() {
        assert!(hub_id("owner/name-1.Q4_K"));
        assert!(!hub_id("owner/../x"));
        assert!(!hub_id("owner//x"));
        assert!(!hub_id("owner/na me"));
        assert!(!hub_id("owner/x?y"));
    }

    #[test]
    fn grouping_keeps_first_appearance_and_python_key_equality() {
        let t = |s: &str| Value::Text(s.into());
        let g = group(vec![
            (t("a"), t("r/1")),
            (t("b"), Value::Integer(1)),
            (t("c"), t("r/1")),
            (t("d"), Value::Real(1.0)),
        ]);
        assert_eq!(g.len(), 2);
        assert_eq!(g[0].1, vec![t("a"), t("c")]);
        assert_eq!(g[1].1, vec![t("b"), t("d")]);
    }

    // ---- noevia#1996: memory under a hostile tree page ------------------------------------

    const MIB: usize = 1024 * 1024;

    /// Heap bytes of a buffer, charged as glibc's malloc does (at least 24 usable bytes, an 8-byte
    /// header, 16-byte steps). Deterministic: capacities, no allocator hooks (`unsafe_code` is
    /// forbidden).
    fn charged(bytes: usize) -> usize {
        if bytes == 0 {
            0
        } else {
            (bytes.max(24) + 8).div_ceil(16) * 16
        }
    }

    fn heap(value: &Json) -> usize {
        match value {
            Json::Null | Json::Bool(_) | Json::Float(_) => 0,
            // `BigInt` keeps its digits in a `String` reserved exactly.
            Json::Int(i) => charged(i.to_string().trim_start_matches('-').len()),
            Json::Str(s) => charged(s.capacity()),
            Json::Arr(items) => {
                charged(items.capacity() * std::mem::size_of::<Json>())
                    + items.iter().map(heap).sum::<usize>()
            }
            Json::Obj(pairs) => {
                charged(pairs.capacity() * std::mem::size_of::<(String, Json)>())
                    + pairs
                        .iter()
                        .map(|(k, v)| charged(k.capacity()) + heap(v))
                        .sum::<usize>()
            }
        }
    }

    /// Per node a slot in its parent (an object pair is the larger) with doubling slack plus its
    /// own smallest allocation; every string byte twice (unescaped strings grow by doubling).
    fn tree_bound() -> usize {
        let per_node = 2 * std::mem::size_of::<(String, Json)>() + charged(1);
        UPDATE_PAGE_NODES * per_node + 2 * UPDATE_PAGE_CAP
    }

    /// `unit` repeated into exactly `len` bytes between `open` and `close`.
    fn filled(open: &str, unit: &str, close: &str, len: usize) -> Vec<u8> {
        let mut page = String::with_capacity(len);
        page.push_str(open);
        while page.len() + unit.len() + close.len() <= len {
            page.push_str(unit);
        }
        while page.len() + close.len() < len {
            page.push(' ');
        }
        page.push_str(close);
        assert_eq!(page.len(), len);
        page.into_bytes()
    }

    /// Streams `page` in 48 KiB chunks (not a power of two: plain doubling would reach 24 MiB).
    async fn read_page(page: &[u8]) -> Option<Vec<u8>> {
        let chunks: Vec<Result<bytes::Bytes, std::io::Error>> = page
            .chunks(48 * 1024)
            .map(|c| Ok(bytes::Bytes::copy_from_slice(c)))
            .collect();
        read_capped(futures_util::stream::iter(chunks), UPDATE_PAGE_CAP).await
    }

    /// noevia#1996: hostile pages at the cap are refused as a failed request (no dates) with the
    /// read buffer never past the cap, a page one byte over is not read, and the largest trees
    /// the limits admit (each shape) stay under `tree_bound()` (68 MiB): with the page, at most
    /// 84 MiB per check, where a 64 MiB page of bare numbers was about 2 GiB. The parse stops
    /// at the first node or character over a limit, so a refused page allocated less than an
    /// admitted one.
    #[tokio::test]
    async fn hostile_pages_are_refused_within_a_bound() {
        let cap = UPDATE_PAGE_CAP;
        let hostile = [
            ("bare numbers", filled("[1", ",1", "]", cap)),
            ("deep nesting", filled("", "[", "", cap)),
            ("escaped text", filled("[\"", "\\u00e9", "\"]", cap)),
            ("raw 4-byte text", filled("[\"", "\u{1F600}", "\"]", cap)),
            ("object keys", filled("{\"k\": 1", ", \"k\": 1", "}", cap)),
            ("empty objects", filled("[{}", ", {}", "]", cap)),
            (
                "file entries",
                filled(
                    "[{}",
                    r#", {"type": "file", "path": "a.gguf", "lastCommit": {"date": "d"}}"#,
                    "]",
                    cap,
                ),
            ),
        ];
        for (name, page) in hostile {
            let body = read_page(&page).await.unwrap();
            assert_eq!(body.len(), cap, "{name}");
            assert!(body.capacity() <= cap, "{name}: {}", body.capacity());
            assert!(parse_page(body.clone()).is_none(), "{name} was parsed");
            assert!(entries(body).unwrap().is_empty(), "{name}");
        }
        let mut over = filled("[", " ", "]", cap);
        over.push(b' ');
        assert!(read_page(&over).await.is_none());

        let bound = tree_bound();
        assert!(bound + cap <= 84 * MIB, "{bound}");
        let largest = [
            (
                "bare numbers",
                filled("[1", ",1", "]", 2 * UPDATE_PAGE_NODES - 1),
            ),
            (
                "object keys",
                format!(
                    "{{{}}}",
                    vec!["\"k\": 1"; UPDATE_PAGE_NODES / 2 - 1].join(", ")
                )
                .into_bytes(),
            ),
            (
                "nested arrays",
                filled(
                    "[",
                    &format!("{}1{}, ", "[".repeat(500), "]".repeat(500)),
                    "1]",
                    1003 * (UPDATE_PAGE_NODES / 501 - 1) + 3,
                ),
            ),
            (
                "long strings",
                filled(
                    "[\"\"",
                    &format!(", \"{}\"", "\u{1F600}".repeat(8 * 1024 + 1)),
                    "]",
                    cap,
                ),
            ),
        ];
        for (name, page) in largest {
            let body = read_page(&page).await.unwrap();
            assert!(body.capacity() <= cap, "{name}");
            let tree = parse_page(body).unwrap_or_else(|| panic!("{name}"));
            let held = heap(&tree);
            assert!(held <= bound, "{name}: {held} > {bound}");
        }
        let numbers = filled("[1", ",1", "]", 2 * UPDATE_PAGE_NODES + 1);
        assert!(parse_page(numbers).is_none());
        let long = format!("[\"{}\"]", "a".repeat(UPDATE_PAGE_STRING_CHARS + 1)).into_bytes();
        assert!(parse_page(long).is_none());
        let longest = format!("[\"{}\"]", "a".repeat(UPDATE_PAGE_STRING_CHARS)).into_bytes();
        assert!(parse_page(longest).is_some());
    }

    /// noevia#1996: an ordinary page reads exactly as before, also as UTF-16 (decoded, the raw
    /// bytes dropped and the text shrunk before the parse).
    #[test]
    fn ordinary_pages_keep_their_dates() {
        let page = r#"[{"type": "directory", "path": "d"},
            {"type": "file", "path": "a.gguf", "lastCommit": {"date": "2026-01-02T03:04:05.000Z"}},
            {"type": "file", "path": "b.gguf", "lastCommit": {"id": "x"}},
            {"type": "file", "path": "a.gguf", "lastCommit": {"date": 7}}]"#;
        let expected = vec![
            (
                "a.gguf".to_owned(),
                Json::Str("2026-01-02T03:04:05.000Z".into()),
            ),
            (
                "a.gguf".to_owned(),
                Json::Int(model_files::BigInt::from_u64(7)),
            ),
        ];
        assert_eq!(entries(page.as_bytes().to_vec()).unwrap(), expected);
        let utf16: Vec<u8> = page.encode_utf16().flat_map(u16::to_le_bytes).collect();
        assert_eq!(entries(utf16).unwrap(), expected);
        assert!(entries(b"[1]".to_vec()).is_err());
    }

    /// noevia#1998: a check dropped while one of its blocking tasks runs (a store read or write,
    /// a tree parse) keeps the request's admission permit until that task has returned.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_dropped_check_keeps_its_permit_until_its_blocking_work_ends() {
        use crate::test_support::{all_permits_back, dropped_while_blocked, test_state, Gate};
        let dir = tempfile::tempdir().unwrap();
        let mut checker = CheckUpdates::with(
            dir.path(),
            &dir.path().join("models.ini"),
            dir.path(),
            HF_API,
            now,
        )
        .unwrap();
        let gate = Gate::new();
        checker.during = Some(gate.hook.clone());
        let mut state = test_state(crate::LEGACY_BASE);
        state.check_updates = Some(Arc::new(checker));
        let state = Arc::new(state);
        let request = axum::http::Request::post("/api/v1/models/check-updates")
            .body(axum::body::Body::empty())
            .unwrap();
        let open = dropped_while_blocked(&state, request, gate).await;
        // The request is gone; its blocking task is not: the permit stays taken.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(state.permits.available_permits(), 3);
        open.send(()).unwrap();
        all_permits_back(&state).await;
    }

    #[test]
    fn splice_adds_checked_first() {
        assert_eq!(
            splice(3, br#"{"status":{}}"#).unwrap(),
            br#"{"checked":3,"status":{}}"#
        );
        assert!(splice(3, br#"{"other":1}"#).is_none());
    }
}
