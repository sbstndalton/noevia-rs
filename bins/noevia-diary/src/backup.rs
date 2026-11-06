//! Backup transport. All work is synchronous inside the server's detached blocking task:
//! its writer exclusion and Python-compatible flock outlive a disconnected HTTP caller.
use crate::{managed, server, workspace, write};
use axum::{body::Body, http::Response};
use diary_backup_run::R;
use reqwest::blocking::{Client, Response as RemoteResponse};
use serde_json::{json, Map, Value};
use std::{fs::OpenOptions, io::Read, path::Path, time::Duration};

struct Dav {
    client: Client,
    base: String,
    username: String,
    secret: String,
    gate: std::sync::Arc<std::sync::Mutex<diary_storage_backoff::StorageGate>>,
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
    fn send(&self, request: reqwest::blocking::RequestBuilder) -> R<RemoteResponse> {
        if self
            .gate
            .lock()
            .map_err(|_| "gate")?
            .check(mono())?
            .is_some()
        {
            return Err("cooldown".into());
        }
        let response = request.send()?;
        self.gate.lock().map_err(|_| "gate")?.record(
            mono(),
            write::now_epoch(),
            response.status().as_u16(),
            response
                .headers()
                .get("retry-after")
                .and_then(|h| h.to_str().ok()),
            false,
        )?;
        Ok(response)
    }
    fn request(&self, method: &str, path: &str) -> R<reqwest::blocking::RequestBuilder> {
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
        let response = self.send(self.request("GET", path)?)?;
        if response.status().as_u16() == 404 {
            return Ok(None);
        }
        let response = Self::checked(response)?;
        let mut data = Vec::new();
        response.take(limit as u64 + 1).read_to_end(&mut data)?;
        if data.len() > limit {
            return Err("size".into());
        }
        Ok(Some(data))
    }
    fn verify(&self, path: &str, data: &[u8]) -> R<bool> {
        let mut existing = self.get(path, data.len())?;
        if existing.is_none() {
            let parts: Vec<_> = path.split('/').collect();
            for end in 1..parts.len() {
                let directory = parts.get(..end).ok_or("path")?.join("/") + "/";
                let response = self.send(self.request("MKCOL", &directory)?)?;
                if !matches!(response.status().as_u16(), 201 | 405) {
                    Self::checked(response)?;
                }
            }
            let response = self.send(
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
    if !string("secretRef").is_empty() && string("secret").is_empty() {
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
            .timeout(Duration::from_secs(15))
            .build()?,
        base: string("baseUrl").trim_end_matches('/').to_string(),
        username: string("username"),
        secret: string("secret"),
        gate: gate(&string("baseUrl"), &string("username"), &string("secret")),
    };
    // URL segments append to the configured collection, preserving encoded names.
    let request = json!({"db":path,"tenant":uid,"destination":destination,
                         "now":write::now_epoch(),"corpusRoot":string("corpusRoot")});
    let result = diary_backup_run::backup(&request, |path, bytes| remote.verify(path, bytes))?;
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

type Gate = std::sync::Arc<std::sync::Mutex<diary_storage_backoff::StorageGate>>;
fn gate(base: &str, username: &str, secret: &str) -> Gate {
    use sha2::{Digest, Sha256};
    use std::sync::{Mutex, OnceLock};
    static GATES: OnceLock<Mutex<std::collections::HashMap<String, Gate>>> = OnceLock::new();
    let key = format!(
        "{:x}",
        Sha256::digest(format!("webdav\0{base}\0{username}\0{secret}"))
    );
    let mut gates = GATES
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if !gates.contains_key(&key) && gates.len() >= 256 {
        let oldest = gates
            .iter()
            .min_by(|a, b| {
                let remaining = |gate: &Gate| {
                    gate.lock()
                        .ok()
                        .and_then(|g| g.state(mono()).ok())
                        .map_or(0.0, |s| s.remaining)
                };
                remaining(a.1).total_cmp(&remaining(b.1))
            })
            .map(|(key, _)| key.clone());
        if let Some(key) = oldest {
            gates.remove(&key);
        }
    }
    gates
        .entry(key)
        .or_insert_with(|| {
            std::sync::Arc::new(Mutex::new(diary_storage_backoff::StorageGate::new()))
        })
        .clone()
}

fn mono() -> f64 {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    START
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_secs_f64()
}
