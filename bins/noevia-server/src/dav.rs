//! Dedicated inbound sharing service. The existing signed Diary API remains the
//! corpus authority; no request is delegated to the legacy application runtime.
use crate::{serve, App};
use axum::{body::Body, extract::ConnectInfo, http::Request, Router};
use serde_json::{json, Value};
use server_dav::{
    Backend, BackendFuture, Capabilities, Entry, Fault, File, Identity, Scope, WriteCondition,
};
use server_store::{Store, Writer};
use std::{path::PathBuf, sync::Arc, time::Duration};

pub struct NativeBackend {
    store: Arc<Store>,
    writer: Arc<Writer>,
    sidecar: server_diary::Sidecar,
    data: PathBuf,
    scope: Scope,
    hashing: Arc<tokio::sync::Semaphore>,
}
fn now() -> i64 {
    server_projects::wall_ms()
}
impl NativeBackend {
    pub fn new(
        store: Arc<Store>,
        writer: Arc<Writer>,
        config: server_diary::DiaryConfig,
        data: PathBuf,
        scope: Scope,
    ) -> Result<Self, String> {
        Ok(Self {
            store,
            writer,
            sidecar: server_diary::Sidecar::new(config)?,
            data,
            scope,
            hashing: Arc::new(tokio::sync::Semaphore::new(4)),
        })
    }
    pub async fn eligible(&self, identity: &Identity, scope: Scope) -> Result<bool, Fault> {
        if scope != self.scope {
            return Ok(false);
        }
        let store = Arc::clone(&self.store);
        let id = identity.clone();
        let scope = scope.as_str().to_owned();
        tokio::task::spawn_blocking(move || store.read(|r| r.row(
            "SELECT 1 FROM app_passwords a JOIN users u ON u.id=a.user_id JOIN dav_settings d ON d.user_id=u.id JOIN user_features f ON f.user_id=u.id LEFT JOIN storage_connections s ON s.user_id=u.id WHERE a.id=?1 AND a.user_id=?2 AND a.scope=?3 AND u.disabled_at IS NULL AND d.scope=?3 AND f.diary_enabled=1 AND (s.kind IS NULL OR s.kind='local')",
            [id.credential_id, id.user_id, scope], |r| r.get::<_, i64>(0)).map(|v| v.is_some())))
            .await.map_err(|_| Fault::AuthorityUnavailable)?.map_err(|_| Fault::AuthorityUnavailable)
    }
    async fn call(
        &self,
        id: &Identity,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
        workspace: bool,
    ) -> Result<Value, Fault> {
        if !self.eligible(id, self.scope).await? {
            return Err(Fault::Forbidden);
        }
        let store = Arc::clone(&self.store);
        let user = id.user_id.clone();
        let data = self.data.clone();
        let config = self.sidecar.config().clone();
        let tenant = tokio::task::spawn_blocking(move || {
            let role = store
                .read(|r| {
                    r.row(
                        "SELECT role FROM users WHERE id=?1 AND disabled_at IS NULL",
                        [&user],
                        |r| r.get::<_, String>(0),
                    )
                })
                .map_err(|_| Fault::Unavailable)?;
            let role = match role.as_deref() {
                Some("admin") => server_auth::Role::Admin,
                Some(_) => server_auth::Role::Member,
                None => return Err(Fault::Forbidden),
            };
            server_diary::Tenant::load_user(&user, role, &store, &data, &config)
                .map_err(|_| Fault::Unavailable)
        })
        .await
        .map_err(|_| Fault::Unavailable)??;
        // Never sign a remote descriptor captured during a local-storage change.
        // Recheck the live grant after constructing the exact outbound tenant.
        if tenant
            .storage_identity()
            .is_none_or(|storage| storage.kind != "local")
            || !self.eligible(id, self.scope).await?
        {
            return Err(Fault::Forbidden);
        }
        let bytes = body.map(|v| bytes::Bytes::from(v.to_string()));
        let response = self
            .sidecar
            .fetch_json(&tenant, method, path, bytes, Duration::from_secs(15))
            .await
            .map_err(|_| {
                if workspace {
                    Fault::Ambiguous
                } else {
                    Fault::Unavailable
                }
            })?;
        let value = response
            .body
            .and_then(|v| js_json::stringify(&v))
            .and_then(|v| serde_json::from_str(&v).ok())
            .unwrap_or(Value::Null);
        if !response.ok {
            return Err(Fault::Remote {
                status: response.status,
                message: value
                    .get("detail")
                    .and_then(Value::as_str)
                    .unwrap_or("Diary file request failed")
                    .to_owned(),
                retry_after: None,
            });
        }
        Ok(value)
    }
}
impl Backend for NativeBackend {
    fn capabilities(&self) -> Capabilities {
        Capabilities { mkdir: true }
    }
    fn workspace_enabled(&self) -> bool {
        true
    }
    fn authenticate<'a>(
        &'a self,
        username: &'a str,
        password: &'a str,
        scope: Scope,
    ) -> BackendFuture<'a, Result<Option<Identity>, Fault>> {
        Box::pin(async move {
            if scope != self.scope {
                return Ok(None);
            }
            // The hashing permit moves into the blocking task, so cancellation at
            // its await cannot admit more actual Argon2 work than the four slots.
            let permit = Arc::clone(&self.hashing)
                .acquire_owned()
                .await
                .map_err(|_| Fault::AuthorityUnavailable)?;
            let store = Arc::clone(&self.store);
            let writer = Arc::clone(&self.writer);
            let username = username.to_owned();
            let password = password.to_owned();
            tokio::task::spawn_blocking(move || {
                let _held = permit;
                server_auth::app_passwords::verify_dav_and_touch(
                    &store,
                    &writer,
                    &username,
                    &password,
                    scope.as_str(),
                    now,
                )
                .map(|v| {
                    v.map(|i| Identity {
                        user_id: i.user_id,
                        credential_id: i.credential_id,
                    })
                })
                .map_err(|_| Fault::AuthorityUnavailable)
            })
            .await
            .map_err(|_| Fault::AuthorityUnavailable)?
        })
    }
    fn allowed<'a>(
        &'a self,
        id: &'a Identity,
        scope: Scope,
    ) -> BackendFuture<'a, Result<bool, Fault>> {
        Box::pin(self.eligible(id, scope))
    }
    fn list<'a>(
        &'a self,
        id: &'a Identity,
        parent: &'a str,
    ) -> BackendFuture<'a, Result<Vec<Entry>, Fault>> {
        Box::pin(async move {
            let path = format!(
                "/api/files?{}",
                url::form_urlencoded::Serializer::new(String::new())
                    .append_pair("path", parent)
                    .finish()
            );
            let value = self
                .call(id, reqwest::Method::GET, &path, None, false)
                .await?;
            let rows = value
                .get("files")
                .and_then(Value::as_array)
                .ok_or(Fault::Unavailable)?;
            rows.iter()
                .map(|r| {
                    Ok(Entry {
                        path: r
                            .get("path")
                            .and_then(Value::as_str)
                            .ok_or(Fault::Unavailable)?
                            .into(),
                        name: r
                            .get("name")
                            .and_then(Value::as_str)
                            .ok_or(Fault::Unavailable)?
                            .into(),
                        is_dir: r
                            .get("isDir")
                            .and_then(Value::as_bool)
                            .ok_or(Fault::Unavailable)?,
                        modified_unix: r
                            .get("modified")
                            .and_then(Value::as_f64)
                            .filter(|v| v.is_finite() && *v > 0.0)
                            .map(|v| v as u64),
                    })
                })
                .collect()
        })
    }
    fn read<'a>(
        &'a self,
        id: &'a Identity,
        path: &'a str,
    ) -> BackendFuture<'a, Result<Option<File>, Fault>> {
        Box::pin(async move {
            let v = self
                .call(
                    id,
                    reqwest::Method::POST,
                    "/api/file",
                    Some(json!({"path":path})),
                    false,
                )
                .await?;
            if v.get("content") == Some(&Value::Null) {
                return Ok(None);
            }
            Ok(Some(File {
                content: v
                    .get("content")
                    .and_then(Value::as_str)
                    .ok_or(Fault::Unavailable)?
                    .into(),
                version: v
                    .get("version")
                    .and_then(Value::as_str)
                    .ok_or(Fault::Unavailable)?
                    .into(),
            }))
        })
    }
    fn write<'a>(
        &'a self,
        id: &'a Identity,
        path: &'a str,
        content: &'a str,
        condition: WriteCondition,
    ) -> BackendFuture<'a, Result<String, Fault>> {
        Box::pin(async move {
            let version = match condition {
                WriteCondition::Match(v) => Some(v),
                WriteCondition::Create => None,
            };
            let v = self
                .call(
                    id,
                    reqwest::Method::PUT,
                    "/api/file",
                    Some(json!({"path":path,"content":content,"version":version})),
                    false,
                )
                .await?;
            Ok(v.get("version")
                .and_then(Value::as_str)
                .ok_or(Fault::Unavailable)?
                .into())
        })
    }
    fn mkdir<'a>(
        &'a self,
        id: &'a Identity,
        path: &'a str,
    ) -> BackendFuture<'a, Result<(), Fault>> {
        Box::pin(async move {
            self.call(
                id,
                reqwest::Method::POST,
                "/api/directory",
                Some(json!({"path":path})),
                false,
            )
            .await
            .map(|_| ())
        })
    }
    fn workspace<'a>(
        &'a self,
        id: &'a Identity,
        body: Value,
    ) -> BackendFuture<'a, Result<(u16, Value), Fault>> {
        Box::pin(async move {
            self.call(
                id,
                reqwest::Method::POST,
                "/api/workspace-ops",
                Some(body),
                true,
            )
            .await
            .map(|v| (200, v))
        })
    }
    fn audit<'a>(
        &'a self,
        id: &'a Identity,
        action: &'a str,
        mut detail: Value,
    ) -> BackendFuture<'a, Result<(), Fault>> {
        Box::pin(async move {
            let writer = Arc::clone(&self.writer);
            let user = id.user_id.clone();
            let credential = id.credential_id.clone();
            let action = action.to_owned();
            detail
                .as_object_mut()
                .ok_or(Fault::Unavailable)?
                .insert("credentialId".into(), json!(credential));
            tokio::task::spawn_blocking(move ||writer.write(|tx|tx.execute("INSERT INTO audit_events(actor_user_id,target_user_id,action,detail,created_at) VALUES(?1,?1,?2,?3,?4)",server_store::rusqlite::params![user,action,detail.to_string(),now()]).map(|_|())))
            .await.map_err(|_|Fault::Unavailable)?.map_err(|_|Fault::Unavailable)
        })
    }
}

/// Limit reception, not storage processing after the body is complete. Socket
/// inactivity still bounds response work, as on the original dedicated server.
fn request_body(body: Body, bound: Duration) -> (Body, Arc<std::sync::atomic::AtomicBool>) {
    use http_body::Body as _;
    let complete = Arc::new(std::sync::atomic::AtomicBool::new(body.is_end_stream()));
    let finished = Arc::clone(&complete);
    use futures_util::{stream, StreamExt};
    let deadline = tokio::time::Instant::now() + bound;
    let input = Box::pin(body.into_data_stream());
    let timed = stream::unfold(
        (input, deadline, false, finished),
        |(mut input, deadline, done, finished)| async move {
            if done {
                return None;
            }
            match tokio::time::timeout_at(deadline, input.next()).await {
                Ok(Some(Ok(bytes))) => Some((Ok(bytes), (input, deadline, false, finished))),
                Ok(Some(Err(error))) => Some((
                    Err(std::io::Error::other(error)),
                    (input, deadline, true, finished),
                )),
                Ok(None) => {
                    finished.store(true, std::sync::atomic::Ordering::Release);
                    None
                }
                Err(_) => Some((
                    Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "Request timed out",
                    )),
                    (input, deadline, true, finished),
                )),
            }
        },
    );
    (Body::from_stream(timed), complete)
}

/// Exact Node bind fallback; per-user opt-in never changes where the port binds.
pub fn bind_host(get: &impl Fn(&str) -> Option<String>) -> String {
    get("COWORK_DAV_HOST")
        .filter(|s| !s.is_empty())
        .or_else(|| get("UI_HOST").filter(|s| !s.is_empty()))
        .unwrap_or_else(|| "0.0.0.0".into())
}
pub fn router(app: &Arc<App>, service: Arc<server_dav::DavService<NativeBackend>>) -> Router {
    let guard = Arc::clone(&app.code_net);
    Router::new().fallback(
        move |ConnectInfo(conn): ConnectInfo<serve::Conn>, request: Request<Body>| {
            let guard = Arc::clone(&guard);
            let service = Arc::clone(&service);
            async move {
                if guard.enabled()
                    && match conn.local {
                        Some(local) => guard.refuses(local).await,
                        None => true,
                    }
                {
                    return guard.deny_listener("file-sharing");
                }
                let (parts, body) = request.into_parts();
                let (body, complete) = request_body(body, Duration::from_secs(60));
                let mut response = service
                    .handle(Request::from_parts(parts, body), conn.peer.ip())
                    .await;
                // Do not drain an ignored/oversize request indefinitely after its
                // response. Every retained connection has received its full body.
                if !complete.load(std::sync::atomic::Ordering::Acquire) {
                    response.headers_mut().insert(
                        axum::http::header::CONNECTION,
                        axum::http::HeaderValue::from_static("close"),
                    );
                }
                response
            }
        },
    )
}
/// Validated before any listener starts. No configured endpoint means no bind.
pub async fn start(
    app: &Arc<App>,
    mut stop: tokio::sync::watch::Receiver<bool>,
) -> Result<Option<tokio::task::JoinHandle<()>>, String> {
    let env = server_account::DavEnv::from_lookup(&|k| std::env::var(k).ok());
    if env.port == 0.0 || env.port.is_nan() {
        return Ok(None);
    }
    let a = Arc::clone(app);
    let (store, writer, public) = tokio::task::spawn_blocking(move || -> Result<_, String> {
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        let (store, writer) = loop {
            if let (Ok(store), Ok(writer)) = (a.identity.store_blocking(), a.writes.writer()) {
                break (store, writer);
            }
            if std::time::Instant::now() >= deadline {
                return Err("DAV account store unavailable".into());
            }
            std::thread::sleep(Duration::from_millis(100));
        };
        let public = store
            .read(|r| {
                for key in ["public_origin_admin", "public_origin"] {
                    if key == "public_origin" {
                        if let Some(value) = a
                            .config
                            .account
                            .as_ref()
                            .map(|s| s.public_origin.clone())
                            .filter(|s| !s.is_empty())
                        {
                            return Ok(value);
                        }
                    }
                    let v = r.row("SELECT value FROM settings WHERE key=?1", [key], |r| {
                        r.get::<_, String>(0)
                    })?;
                    if let Some(v) = v.filter(|v| !v.is_empty()) {
                        return Ok(v);
                    }
                }
                Ok(a.config
                    .account
                    .as_ref()
                    .map(|s| s.public_origin.clone())
                    .unwrap_or_default())
            })
            .map_err(|_| "DAV origin unavailable")?;
        Ok((store, writer, public))
    })
    .await
    .map_err(|_| "DAV initialization failed")??;
    let config = server_account::dav_configuration(&env, &public)?;
    let server_account::DavConfig::Available {
        port,
        scope,
        origin,
        ..
    } = config
    else {
        return Ok(None);
    };
    let scope = if scope == "lan" {
        Scope::Lan
    } else {
        Scope::Public
    };
    let config =
        server_dav::Config::new(&origin, scope, std::env::var("COWORK_DAV_PROXY_TOKEN").ok())
            .map_err(str::to_owned)?
            .with_deadline(Duration::from_secs(60))
            .map_err(str::to_owned)?;
    let data = app
        .config
        .data_dir
        .clone()
        .ok_or("DAV data directory unavailable")?;
    let backend = NativeBackend::new(store, writer, app.config.diary.clone(), data, scope)?;
    let service = Arc::new(server_dav::DavService::new(Arc::new(backend), config));
    let listener =
        tokio::net::TcpListener::bind((bind_host(&|k| std::env::var(k).ok()).as_str(), port))
            .await
            .map_err(|_| "DAV listener bind failed")?;
    println!(
        "Diary file sharing listener on port {port}; scope {}; per-user opt-in required",
        scope.as_str()
    );
    let router = router(app, service);
    Ok(Some(tokio::spawn(serve::serve_router_with_socket_timeout(
        listener,
        router,
        serve::Limits {
            header_read: Duration::from_secs(15),
            keep_alive_idle: Duration::from_secs(5),
            max_connections: 4096,
        },
        Some(Duration::from_secs(60)),
        async move {
            let _ = stop.wait_for(|s| *s).await;
        },
    ))))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn pending_request_body_expires_without_a_detached_receiver() {
        let body = Body::from_stream(futures_util::stream::pending::<
            Result<bytes::Bytes, std::io::Error>,
        >());
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            axum::body::to_bytes(request_body(body, Duration::from_millis(10)).0, 1024),
        )
        .await;
        assert!(matches!(result, Ok(Err(_))));
    }
}
