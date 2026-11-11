//! Backup transport. All work is synchronous inside the server's detached blocking task:
//! its writer exclusion and Python-compatible flock outlive a disconnected HTTP caller.
use crate::{managed, server, workspace, write};
use axum::{body::Body, http::Response};
use diary_backup_run::R;
use reqwest::{Client, Response as RemoteResponse};
use serde_json::{json, Map, Value};
use std::{fs::OpenOptions, path::Path, time::Duration};

struct Dav {
    client: Client,
    base: String,
    username: String,
    secret: String,
    gate: crate::storage_gate::Gate,
    db: std::path::PathBuf,
    runtime: tokio::runtime::Runtime,
}
impl Dav {
    fn url(&self, path: &str) -> R<reqwest::Url> {
        // Python quote(segment), shared with the workspace's signing transport donor.
        let path = path
            .split('/')
            .map(s3_sign::uri_encode)
            .collect::<Vec<_>>()
            .join("/");
        Ok(reqwest::Url::parse(&format!("{}/{path}", self.base))?)
    }
    fn send(
        &self,
        method: &str,
        path: &str,
        request: reqwest::RequestBuilder,
    ) -> R<RemoteResponse> {
        self.gate.check()?;
        // Write-ahead intent is durable before dispatch. A dropped process/transport has no
        // authority to clear it. Do not use GET/readback or later success to settle this intent.
        let mutation = method != "GET";
        let token = if mutation {
            Some(begin_effect(&self.db, method, path)?)
        } else {
            None
        };
        let response = self.runtime.block_on(request.send())?;
        let code = response.status().as_u16();
        if let Some(token) = token {
            let terminal = match method {
                "MKCOL" => matches!(code, 201 | 405 | 401 | 403),
                "PUT" => matches!(code, 200 | 201 | 204 | 401 | 403 | 405 | 409 | 412),
                _ => false,
            };
            if terminal {
                settle_effect(&self.db, &token)?;
            }
        }
        self.gate.record(
            code,
            response
                .headers()
                .get("retry-after")
                .and_then(|h| h.to_str().ok()),
        )?;
        Ok(response)
    }
    fn request(&self, method: &str, path: &str) -> R<reqwest::RequestBuilder> {
        let mut request = self
            .client
            .request(method.parse()?, self.url(path)?)
            .header("accept-encoding", "identity");
        if !self.username.is_empty() {
            request = request.basic_auth(&self.username, Some(&self.secret));
        }
        Ok(request)
    }
    fn checked(response: RemoteResponse) -> R<RemoteResponse> {
        let status = response.status();
        if matches!(status.as_u16(), 401 | 403) {
            return Err("loginRefused".into());
        }
        if !status.is_success() {
            return Err("remote".into());
        }
        Ok(response)
    }
    fn get(&self, path: &str, limit: usize) -> R<Option<Vec<u8>>> {
        let response = self.send("GET", path, self.request("GET", path)?)?;
        if response.status().as_u16() == 404 {
            return Ok(None);
        }
        let mut response = Self::checked(response)?;
        let data = self.runtime.block_on(async {
            let mut data = Vec::new();
            while let Some(chunk) = response.chunk().await? {
                if data.len().saturating_add(chunk.len()) > limit {
                    return Err("size".into());
                }
                data.extend_from_slice(&chunk);
            }
            Ok::<_, Box<dyn std::error::Error>>(data)
        })?;
        Ok(Some(data))
    }
    fn verify(&self, path: &str, data: &[u8]) -> R<bool> {
        let mut existing = self.get(path, data.len())?;
        if existing.is_none() {
            let parts: Vec<_> = path.split('/').collect();
            for end in 1..parts.len() {
                let directory = parts.get(..end).ok_or("path")?.join("/") + "/";
                let response =
                    self.send("MKCOL", &directory, self.request("MKCOL", &directory)?)?;
                if !matches!(response.status().as_u16(), 201 | 405) {
                    Self::checked(response)?;
                }
            }
            let response = self.send(
                "PUT",
                path,
                self.request("PUT", path)?
                    .header("if-none-match", "*")
                    .header("content-type", "text/markdown; charset=utf-8")
                    .body(data.to_vec()),
            )?;
            if !matches!(response.status().as_u16(), 405 | 409 | 412) {
                Self::checked(response)?;
            }
            existing = self.get(path, data.len())?;
        }
        if existing.as_deref() != Some(data) {
            return Err("conflict".into());
        }
        Ok(true)
    }
}

pub fn run(root: &Path, uid: &str, storage: Option<&Map<String, Value>>) -> Response<Body> {
    match run_inner(root, uid, storage) {
        Ok(response) => response,
        Err(_) => server::internal_error(), // No remote URL, body, header or credential is logged.
    }
}
fn run_inner(root: &Path, uid: &str, storage: Option<&Map<String, Value>>) -> R<Response<Body>> {
    let path = root.join("managed-diary.db");
    if !path.exists() {
        return Ok(server::json_response(200, &json!({"mode":"legacy"})));
    }
    if pending(root) {
        return Ok(uncertain());
    }
    let store = managed::backup_store(root, uid).map_err(|_| "store")?;
    let empty = Map::new();
    let storage = storage.unwrap_or(&empty);
    let destination = workspace::destination(storage).map_err(|_| "descriptor")?;
    let db = managed::open_ro(&path).map_err(|_| "store")?;
    let active = db
        .query_row("SELECT value FROM meta WHERE key='active'", [], |r| {
            r.get::<_, String>(0)
        })
        .unwrap_or_default()
        == "1";
    drop(db);
    let status = || workspace::storage_status(&store, Some(storage));
    let Some(destination) = destination.filter(|_| active) else {
        return Ok(server::reply(status()));
    };
    let string = |key: &str| {
        storage
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    if storage.get("secretRef").is_some_and(truthy) && string("secret").is_empty() {
        return Ok(server::json_response(
            428,
            &json!({"detail":{"code":"storage_credential_required"}}),
        ));
    }
    let lock = OpenOptions::new()
        .create(true)
        .append(true)
        .open(root.join("managed-backup.lock"))?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => return Ok(server::reply(status())),
        Err(e) => return Err(e.into()),
    }
    let remote = Dav {
        client: Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            // Python uses a per-read/connect inactivity limit, not a total-response deadline.
            .connect_timeout(Duration::from_secs(15))
            .read_timeout(Duration::from_secs(15))
            .build()?,
        base: string("baseUrl").trim_end_matches('/').to_string(),
        username: string("username"),
        secret: string("secret"),
        gate: crate::storage_gate::Gate::new(
            root.parent()
                .and_then(Path::parent)
                .ok_or("root")?
                .join("storage-gates.db"),
            &string("baseUrl"),
            &string("username"),
            &string("secret"),
        ),
        db: path.clone(),
        runtime: tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?,
    };
    if pending(root) {
        return Ok(uncertain());
    }
    // URL segments append to the configured collection, preserving encoded names.
    let request = json!({"db":path,"tenant":uid,"destination":destination,
                         "now":write::now_epoch(),"corpusRoot":string("corpusRoot")});
    let result = diary_backup_run::backup(&request, |path, bytes| remote.verify(path, bytes));
    if pending(root) {
        // Keep the kernel flock as well while this incarnation lives. After process loss,
        // the same durable intent is checked by every backup/write/delete admission path.
        use std::sync::{Mutex, OnceLock};
        static HOLDS: OnceLock<Mutex<Vec<std::fs::File>>> = OnceLock::new();
        HOLDS
            .get_or_init(Mutex::default)
            .lock()
            .map_err(|_| "hold")?
            .push(lock);
        return Ok(uncertain());
    }
    let result = result?;
    let mut response = status();
    if let workspace::Reply::Json(_, bytes) = &mut response {
        let mut value: Value = serde_json::from_slice(bytes)?;
        if let Some(map) = value.as_object_mut() {
            if result.get("result").and_then(Value::as_str) == Some("complete") {
                map.insert("storageContacted".into(), json!(true));
            }
            if result.get("loginRefused").and_then(Value::as_bool) == Some(true) {
                map.insert("loginRefused".into(), json!(true));
            }
        }
        *bytes = serde_json::to_vec(&value)?;
    }
    // File's Drop releases flock only after transport and final bookkeeping finish.
    Ok(server::reply(response))
}

const EFFECT: &str = "backup_effect_pending";
fn effect_db(path: &Path) -> R<rusqlite::Connection> {
    let db =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    db.busy_timeout(Duration::from_secs(30))?;
    db.execute_batch("PRAGMA synchronous=FULL")?;
    Ok(db)
}
fn begin_effect(path: &Path, method: &str, remote_path: &str) -> R<String> {
    let token =
        json!({"id":write::uuid4().map_err(|_| "id")?, "method":method, "path":remote_path})
            .to_string();
    let db = effect_db(path)?;
    if db.execute(
        "INSERT INTO meta(key,value) SELECT ?,? WHERE NOT EXISTS (SELECT 1 FROM meta WHERE key=?)",
        rusqlite::params![EFFECT, token, EFFECT],
    )? != 1
    {
        return Err("unsettled effect".into());
    }
    Ok(token)
}
fn settle_effect(path: &Path, token: &str) -> R<()> {
    if effect_db(path)?.execute(
        "DELETE FROM meta WHERE key=? AND value=?",
        rusqlite::params![EFFECT, token],
    )? != 1
    {
        return Err("effect identity".into());
    }
    Ok(())
}
/// Read-only and fail closed (a hot journal left by a crashed writer is rolled back through one
/// read-write open of the existing file). No timeout, exit, or readback retires an uncertain effect.
pub fn pending(root: &Path) -> bool {
    let path = root.join("managed-diary.db");
    if !path.exists() {
        return false;
    }
    let read = |flags: rusqlite::OpenFlags| -> rusqlite::Result<bool> {
        let db = rusqlite::Connection::open_with_flags(&path, flags)?;
        db.busy_timeout(Duration::from_secs(1))?;
        db.query_row(
            "SELECT EXISTS(SELECT 1 FROM meta WHERE key=?)",
            [EFFECT],
            |r| r.get(0),
        )
    };
    match read(rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY) {
        Ok(v) => v,
        // A writer killed inside a transaction left a hot journal, which a read-only handle may
        // not roll back. A read-write open of the existing file (never created here) lets
        // SQLite restore the last committed state; a committed intent survives it.
        Err(rusqlite::Error::SqliteFailure(e, _))
            if e.extended_code == rusqlite::ffi::SQLITE_READONLY_ROLLBACK =>
        {
            read(rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE).unwrap_or(true)
        }
        Err(_) => true,
    }
}
pub fn uncertain() -> Response<Body> {
    server::detail(
        503,
        "Diary backup settlement is uncertain. Saving and deletion remain paused.",
    )
}
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(v) => *v,
        Value::Number(v) => v.as_f64() != Some(0.0),
        Value::String(v) => !v.is_empty(),
        Value::Array(v) => !v.is_empty(),
        Value::Object(v) => !v.is_empty(),
    }
}

#[cfg(test)]
mod hot_journal_tests {
    use super::{pending, EFFECT};
    use std::path::Path;

    type T = Result<(), Box<dyn std::error::Error>>;
    const DB: &str = "managed-diary.db";

    fn seed(dir: &Path, committed_intent: bool) -> T {
        let db = rusqlite::Connection::open(dir.join(DB))?;
        db.execute_batch(
            "CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT); CREATE TABLE pad(b BLOB);",
        )?;
        if committed_intent {
            db.execute(
                "INSERT INTO meta(key,value) VALUES(?,'committed')",
                [EFFECT],
            )?;
        }
        Ok(())
    }

    /// Leaves `copy/` with the database and journal of a writer that died mid-transaction: the
    /// copy has a hot journal and no process holds its lock.
    fn crashed_copy(src: &Path, copy: &Path) -> T {
        let db = rusqlite::Connection::open(src.join(DB))?;
        db.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA cache_size=1; BEGIN IMMEDIATE;")?;
        db.execute(
            "INSERT OR REPLACE INTO meta(key,value) VALUES(?,'uncommitted')",
            [EFFECT],
        )?;
        for _ in 0..64 {
            db.execute("INSERT INTO pad(b) VALUES(zeroblob(8192))", [])?;
        }
        let journal = format!("{DB}-journal");
        assert!(
            src.join(&journal).exists(),
            "the open transaction keeps a rollback journal"
        );
        std::fs::copy(src.join(DB), copy.join(DB))?;
        std::fs::copy(src.join(&journal), copy.join(&journal))?;
        db.execute_batch("ROLLBACK")?;
        Ok(())
    }

    fn read_only_refusal(dir: &Path) -> Result<Option<i32>, rusqlite::Error> {
        let db = rusqlite::Connection::open_with_flags(
            dir.join(DB),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        Ok(
            match db.query_row("SELECT count(*) FROM meta", [], |r| r.get::<_, i64>(0)) {
                Err(rusqlite::Error::SqliteFailure(e, _)) => Some(e.extended_code),
                _ => None,
            },
        )
    }

    #[test]
    fn an_uncommitted_intent_left_by_a_crashed_writer_is_rolled_back_and_not_pending() -> T {
        let (src, copy) = (tempfile::tempdir()?, tempfile::tempdir()?);
        seed(src.path(), false)?;
        crashed_copy(src.path(), copy.path())?;
        assert_eq!(
            read_only_refusal(copy.path())?,
            Some(rusqlite::ffi::SQLITE_READONLY_ROLLBACK)
        );
        assert!(!pending(copy.path()));
        assert!(!copy.path().join(format!("{DB}-journal")).exists());
        Ok(())
    }

    #[test]
    fn a_committed_intent_survives_the_rollback_and_still_pauses_saves() -> T {
        let (src, copy) = (tempfile::tempdir()?, tempfile::tempdir()?);
        seed(src.path(), true)?;
        crashed_copy(src.path(), copy.path())?;
        assert!(pending(copy.path()));
        Ok(())
    }

    #[test]
    fn a_file_that_is_not_a_database_fails_closed() -> T {
        let dir = tempfile::tempdir()?;
        std::fs::write(dir.path().join(DB), b"not a database at all, just bytes")?;
        assert!(pending(dir.path()));
        Ok(())
    }
}
