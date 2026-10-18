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
use crate::hub_page::{parse_page, read_capped, PAGE_CAP};
use crate::model_updates::{self, py_float_repr, ModelUpdates};
use model_files::json::Value as Json;
use rusqlite::{types::Value, Connection, OpenFlags};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

/// `hf.HF_API`.
pub const HF_API: &str = crate::hub_page::HUB_API;
/// Most download records handled; more answers 500 instead of thousands of Hub requests.
pub const RECORD_CAP: usize = 4096;
/// httpx's `timeout=15.0` (connect and each read).
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);
/// A bound httpx does not have: the whole of one tree request.
const REQUEST_BUDGET: Duration = Duration::from_secs(120);
/// sqlite3's default `timeout=5.0`.
const BUSY_WAIT: Duration = Duration::from_millis(5000);

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
        let Some(body) = read_capped(response.bytes_stream(), PAGE_CAP).await else {
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
    collect(&list)
}

/// The `(path, date)` pairs of a parsed listing, built while the whole tree is still held
/// (noevia#2000: a check holds the tree and this output together).
fn collect(list: &[Json]) -> Result<Vec<(String, Json)>, Raised> {
    let mut out = Vec::new();
    for e in list {
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
    use crate::hub_page::measure::{
        bound, charged, filled, fixture_pages, heap, nodes, peak_bound, read_page, tree_bound_of,
        tree_per_node, GGUF_LARGE, MIB, PICKLE_IMPORTS,
    };
    use crate::hub_page::PAGE_NODES;

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

    // ---- noevia#1996, noevia#2000: memory under a hostile tree page -------------------------
    // The page limits themselves (read cap, nodes, strings, depth) are tested once, in
    // `hub_page`. What is the update check's: what it holds beside the tree.

    /// The page limits as the check applies them: a hostile page is a failed request (no dates),
    /// not a 500 and not a partial answer.
    #[tokio::test]
    async fn hostile_pages_record_no_dates() {
        let cap = PAGE_CAP;
        let hostile = [
            ("bare numbers", filled("[1", ",1", "]", cap)),
            ("deep nesting", filled("", "[", "", cap)),
            ("escaped text", filled("[\"", "\\u00e9", "\"]", cap)),
            ("object keys", filled("{\"k\": 1", ", \"k\": 1", "}", cap)),
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
            assert!(entries(body).unwrap().is_empty(), "{name}");
        }
        let mut over = filled("[", " ", "]", cap);
        over.push(b' ');
        assert!(read_page(&over).await.is_none());
    }

    /// Heap bytes of `entries()`' output: the vector's buffer (by capacity) and what each pair
    /// holds (the path and the cloned date).
    fn output_heap(out: &Vec<(String, Json)>) -> usize {
        charged(out.capacity() * std::mem::size_of::<(String, Json)>())
            + out
                .iter()
                .map(|(path, date)| charged(path.capacity()) + heap(date))
                .sum::<usize>()
    }

    /// One file entry of a page.
    fn file(path: &str, date: &str) -> String {
        format!(r#"{{"type":"file","path":{path},"lastCommit":{{"date":{date}}}}}"#)
    }

    /// noevia#2000: `entries()` builds its output while the whole tree is still held, so past the
    /// parse a check holds the tree plus the output (the raw page is gone by then: `parse_page`
    /// drops it, and while parsing a check holds the page plus the tree, 84 MiB at most, see
    /// `hub_page`). The output copies a path and a date per file entry, so the worst page for it
    /// has dates that are big values (copies of parts of the tree), beside strings that take the
    /// most heap in the tree (one character past a power of two, so the parse doubled them) and
    /// are copied whole. Measured by buffer capacities, with the output vector's previous buffer
    /// (kept while it grows), the shapes below hold 13 to 88 MiB, the worst being the page that
    /// combines both (a date of nested arrays beside long paths). The test bounds each shape at
    /// 90 MiB as a sanity check. The general bound (`peak_bound()`, under 111 MiB) is derived in
    /// `hub_page/measure.rs` (noevia#2013), not established by these shapes; see
    /// `the_derived_bound_holds_per_page`.
    #[tokio::test]
    async fn entries_beside_the_tree_are_bounded() {
        let nested_unit = format!("{}1{}, ", "[".repeat(500), "]".repeat(500));
        let nested = format!("[{}1]", nested_unit.repeat(PAGE_NODES / 501 - 12));
        let long = format!("\"{}\"", "\u{1F600}".repeat(8 * 1024 + 1));
        let escaped = format!("\"{}\"", "\\u00e9".repeat(10_000));
        let numbers = format!("[{}]", vec!["1"; PAGE_NODES - 16].join(","));
        let keys = format!("{{{}}}", vec!["\"k\": 1"; PAGE_NODES / 2 - 8].join(", "));
        let shapes: Vec<(&str, String)> = vec![
            // The most pairs: 9 nodes a file entry.
            (
                "minimal entries",
                format!(
                    "[{}]",
                    vec![file("\"a\"", "\"d\""); PAGE_NODES / 9 - 1].join(",")
                ),
            ),
            // The largest dates the node cap admits, each shape once.
            ("nested-array date", format!("[{}]", file("\"a\"", &nested))),
            (
                "number-array date",
                format!("[{}]", file("\"a\"", &numbers)),
            ),
            ("object date", format!("[{}]", file("\"a\"", &keys))),
            // The most string bytes: both fields one character past a power of two.
            (
                "long strings",
                format!("[{}]", vec![file(&long, &long); 240].join(",")),
            ),
            (
                "escaped strings",
                format!("[{}]", vec![file(&escaped, &escaped); 130].join(",")),
            ),
            (
                "nested-array date and long strings",
                format!(
                    "[{},{}]",
                    file("\"a\"", &nested),
                    vec![file(&long, &long); 220].join(",")
                ),
            ),
        ];
        let mut worst = 0;
        for (name, page) in shapes {
            assert!(page.len() <= PAGE_CAP, "{name}: {}", page.len());
            let body = read_page(page.as_bytes()).await.unwrap();
            let tree = parse_page(body).unwrap_or_else(|| panic!("{name} was refused"));
            let Json::Arr(list) = &tree else {
                panic!("{name}")
            };
            let out = collect(list).unwrap();
            assert!(!out.is_empty(), "{name}");
            // The vector grows by doubling: its previous buffer is held while it is copied.
            let growing = charged(out.capacity() / 2 * std::mem::size_of::<(String, Json)>());
            let (held, copied) = (heap(&tree), output_heap(&out) + growing);
            let together = held + copied;
            eprintln!(
                "{name}: page {:.1} MiB, tree {:.1} MiB, output {:.1} MiB ({} pairs), together {:.1} MiB",
                page.len() as f64 / MIB as f64,
                held as f64 / MIB as f64,
                copied as f64 / MIB as f64,
                out.len(),
                together as f64 / MIB as f64
            );
            assert!(together <= 90 * MIB, "{name}: {together}");
            worst = worst.max(together);
        }
        // The worst shape measured 88.3 MiB, past the 84 MiB of the parse (page + tree), which
        // left the output out. No lower bound is asserted: a leaner parser must not fail this.
        eprintln!("worst: {:.1} MiB", worst as f64 / MIB as f64);
    }

    /// noevia#2013: the derived bound, `bound(nodes, string bytes)`, holds for each of these
    /// small pages with its own node count, so a wrong per-node constant fails here and not only
    /// at the limits. Each shape is checked bare and as the `date` of a file entry (the output
    /// then copies it). The tree term is also checked from below on the shapes that stress it.
    #[tokio::test]
    async fn the_derived_bound_holds_per_page() {
        let nested =
            |depth: usize, leaf: &str| format!("{}{leaf}{}", "[".repeat(depth), "]".repeat(depth));
        let deep_obj = |depth: usize| format!("{}1{}", "{\"k\":".repeat(depth), "}".repeat(depth));
        let list = |item: String, n: usize| format!("[{}]", vec![item; n].join(","));
        // (name, page, the tree must come within 10 % of the bound's node term)
        let shapes: Vec<(&str, String, bool)> = vec![
            ("[1] repeated", list("[1]".into(), 5000), false),
            ("{\"\":1} repeated", list("{\"\":1}".into(), 5000), false),
            ("{\"k\":1} repeated", list("{\"k\":1}".into(), 5000), false),
            ("deep arrays", list(nested(500, "1"), 20), true),
            ("deep single-child arrays, one item", nested(500, "1"), true),
            ("deep objects", list(deep_obj(500), 20), false),
            ("numbers", list("1".into(), 20_000), false),
            (
                "long strings",
                list(format!("\"{}\"", "a".repeat(4097)), 50),
                false,
            ),
            (
                "escaped strings",
                list(format!("\"{}\"", "\\u00e9".repeat(300)), 50),
                false,
            ),
        ];
        let mut pages: Vec<(String, String, bool)> = Vec::new();
        for (name, shape, tight) in shapes {
            pages.push((name.to_owned(), shape.clone(), tight));
            pages.push((
                format!("{name} as a date"),
                format!("[{}]", file("\"a\"", &shape)),
                false,
            ));
        }
        for (name, fixture) in [("gguf", GGUF_LARGE), ("pickle", PICKLE_IMPORTS)] {
            for (i, page) in fixture_pages(fixture).into_iter().enumerate() {
                pages.push((
                    format!("{name} page {i}"),
                    String::from_utf8(page).unwrap(),
                    false,
                ));
            }
        }
        for (name, page, tight) in pages {
            let body = read_page(page.as_bytes()).await.unwrap();
            let tree = parse_page(body).unwrap_or_else(|| panic!("{name} was refused"));
            let n = nodes(&tree);
            let held = heap(&tree);
            let out = match &tree {
                Json::Arr(list) => collect(list).unwrap_or_default(),
                _ => Vec::new(),
            };
            let growing = charged(out.capacity() / 2 * std::mem::size_of::<(String, Json)>());
            let together = held + output_heap(&out) + growing;
            let limit = bound(n, page.len());
            assert!(held <= tree_bound_of(n, page.len()), "{name}: tree {held}");
            assert!(
                together <= limit,
                "{name}: {together} > {limit} ({n} nodes)"
            );
            if tight {
                // Single-child arrays are the shape the per-node term is derived from.
                assert!(
                    held * 10 >= n * tree_per_node() * 9,
                    "{name}: {held} for {n} nodes"
                );
            }
        }
        // The figure `docs/native-check-updates.md` states.
        assert!(peak_bound() > 110 * MIB && peak_bound() < 111 * MIB);
    }

    /// noevia#1999: every file entry of the real Hub pages (`tests/fixtures/hub-tree`) gives one
    /// path and date, as the check reads a page.
    #[tokio::test]
    async fn real_hub_pages_give_a_date_per_file() {
        for (name, fixture) in [("gguf", GGUF_LARGE), ("pickle", PICKLE_IMPORTS)] {
            let mut files = 0;
            let mut pairs = Vec::new();
            for page in fixture_pages(fixture) {
                let tree: serde_json::Value = serde_json::from_slice(&page).unwrap();
                files += tree
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|e| e["type"] == "file")
                    .count();
                pairs.extend(entries(read_page(&page).await.unwrap()).unwrap());
            }
            assert!(files > 0, "{name}");
            assert_eq!(pairs.len(), files, "{name}");
            assert!(
                pairs
                    .iter()
                    .all(|(_, date)| matches!(date, Json::Str(d) if d.len() == 24)),
                "{name}"
            );
        }
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
