//! Native `GET /api/providers/chatgpt` (M6, sbstndalton/noevia#1373): the signed-in account's
//! ChatGPT connection status (crates/server-chatgpt-status). Node's session gate first (signed out
//! 401, as in Node), then the product flag (off: Node's 404 before any OAuth row or key), then the
//! owner's row, read-only. Only that exact path and GET are owned (routes.toml); every other method,
//! a trailing-slash path, every `/api/providers/chatgpt/*` sibling and every request without
//! NOEVIA_RUST_AUTH=1 go to Node (its old switch NOEVIA_RUST_CHATGPT_STATUS is retired, #1071). The front's usual session upkeep (main's Writer,
//! lib.rs) runs before this as for every routed request; the status read itself writes nothing.
//!
//! Keys (owner decision 1 on #1373): the current and previous secret keys are snapshotted once,
//! as Node's createSecretStore does at its start. If they cannot be loaded at startup (a first
//! boot where Node has not created `secrets.key` yet), a later request loads them, and the first
//! successful load is kept for the life of the process. Replacing or rotating a key file on disk
//! therefore needs a coordinated restart of the web container (Node and the front together).
use crate::{config::Config, identity, reply, App};
use axum::{
    body::Body,
    http::{Method, Request, Response, StatusCode},
    response::IntoResponse,
};
use server_auth::secrets::SecretKeys;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use tokio::sync::Semaphore;
use zeroize::Zeroizing;

/// Gate checks and reads (blocking SQLite) in flight at once; past that, 503 rather than a queue.
const PERMITS: usize = 16;

pub struct StatusLayer {
    feature_env: Option<bool>,
    data_dir: PathBuf,
    previous_key_env: Option<Zeroizing<String>>,
    keys: OnceLock<Arc<SecretKeys>>,
    gate: Arc<Semaphore>,
}

impl StatusLayer {
    /// `None` while NOEVIA_RUST_CHATGPT_STATUS is off (config.rs refuses it without UI_DATA_DIR).
    pub fn new(config: &Config) -> Option<Self> {
        let settings = config.rust_chatgpt_status.as_ref()?;
        let layer = StatusLayer {
            feature_env: settings.feature_env,
            data_dir: config.data_dir.clone()?,
            previous_key_env: settings.previous_key_env.0.clone().map(Zeroizing::new),
            keys: OnceLock::new(),
            gate: Arc::new(Semaphore::new(PERMITS)),
        };
        let _ = layer.keys();
        Some(layer)
    }

    /// The key snapshot, loaded at most until it first succeeds (see the module documentation).
    pub fn keys(&self) -> Option<Arc<SecretKeys>> {
        if let Some(k) = self.keys.get() {
            return Some(Arc::clone(k));
        }
        let loaded = server_auth::secrets::load_existing(
            &self.data_dir,
            self.previous_key_env.as_deref().map(String::as_str),
        )
        .ok()?;
        Some(Arc::clone(self.keys.get_or_init(|| Arc::new(loaded))))
    }

    /// Whether the key snapshot is taken.
    pub fn keys_loaded(&self) -> bool {
        self.keys.get().is_some()
    }
}

fn busy() -> Response<Body> {
    reply::error(
        StatusCode::SERVICE_UNAVAILABLE,
        "ChatGPT status is busy. Try again shortly.",
        true,
    )
}

pub async fn serve(
    app: &Arc<App>,
    conn: crate::serve::Conn,
    req: Request<Body>,
    path: String,
) -> Response<Body> {
    let Some(layer) = app.chatgpt_status.as_ref().map(Arc::clone) else {
        return app.proxy.forward(conn.peer, req).await;
    };
    // Node matches the exact pathname only; routes.toml matching also accepts a trailing slash.
    if path != server_chatgpt_status::ROUTE || req.method() != Method::GET {
        return app.proxy.forward(conn.peer, req).await;
    }
    let (parts, _body) = req.into_parts();
    let creds = identity::creds_of(&parts);
    if app.identity.authenticator().is_some() && !server_auth::request::could_authenticate(&creds) {
        return identity::Rejection::Refused(server_auth::Refusal::Unauthorized).into_response();
    }
    let Ok(permit) = Arc::clone(&layer.gate).try_acquire_owned() else {
        return busy();
    };
    let gate = Arc::clone(&app.identity);
    let now = identity::now_ms();
    let answered = tokio::task::spawn_blocking(move || {
        let _p = permit;
        let id = match gate.gate_blocking(&creds, server_chatgpt_status::ROUTE, now) {
            Ok(id) => id,
            Err(r) => return Err(r),
        };
        let Ok(store) = gate.store_blocking() else {
            return Ok(server_chatgpt_status::failed());
        };
        Ok(server_chatgpt_status::answer(
            &store,
            &id.user.id,
            layer.feature_env,
            &|| layer.keys(),
        ))
    })
    .await;
    match answered {
        Ok(Ok(a)) => reply::json_text(
            StatusCode::from_u16(a.status).unwrap_or(StatusCode::BAD_GATEWAY),
            a.body,
            true,
        ),
        Ok(Err(r)) => r.into_response(),
        Err(_) => reply::json_text(
            StatusCode::BAD_GATEWAY,
            server_chatgpt_status::failed().body,
            true,
        ),
    }
}
