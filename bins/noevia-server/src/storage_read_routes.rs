//! Dark exact-GET metadata slice. Authentication uses real headers inside the owned job.
use crate::{identity, reply, App};
use axum::{
    body::Body,
    http::{Request, Response, StatusCode},
    response::IntoResponse,
};
use std::sync::Arc;
const PATH: &str = "/api/integrations/storage";
fn unavailable() -> Response<Body> {
    reply::error(StatusCode::SERVICE_UNAVAILABLE, "Storage unavailable", true)
}
pub async fn serve(
    app: &Arc<App>,
    conn: crate::serve::Conn,
    req: Request<Body>,
    path: String,
) -> Response<Body> {
    if path != PATH || req.method() != axum::http::Method::GET || !app.config.rust_storage_reads {
        return app.proxy.forward(conn.peer, req).await;
    }
    let (parts, _body) = req.into_parts();
    let creds = identity::creds_of(&parts);
    let layer = Arc::clone(&app.identity);
    let data_dir = app.config.data_dir.clone();
    let now = identity::now_ms();
    // Authenticate before taking a worker slot (noevia#1827): an unauthenticated flood is
    // answered here and never holds a slot an authenticated request needs.
    let gate_layer = Arc::clone(&layer);
    let id = match tokio::task::spawn_blocking(move || gate_layer.gate_blocking(&creds, PATH, now))
        .await
    {
        Ok(Ok(id)) => id,
        Ok(Err(rejection)) => return rejection.into_response(),
        Err(_) => return unavailable(),
    };
    app.storage_metadata
        .run(move || {
            let Some(data_dir) = data_dir else {
                return unavailable();
            };
            let previous = std::env::var("SECRETS_KEY_PREVIOUS")
                .ok()
                .map(zeroize::Zeroizing::new);
            // Missing or invalid keys refuse only owners with a stored connection; an owner with
            // none gets the normal no-connection answer, as Node does (noevia#1827).
            let keys = server_storage_reads::owner_secret::load_existing_keys(
                &data_dir,
                previous.as_deref().map(String::as_str),
            )
            .ok();
            let store = match layer.store_blocking() {
                Ok(store) => store,
                Err(_) => return unavailable(),
            };
            match server_storage_reads::owner_secret::project_connection_with(
                &store,
                &id,
                keys.as_ref(),
            ) {
                Ok(projected) => reply::json_text(
                    StatusCode::OK,
                    server_storage_reads::owner_secret::metadata_json(&projected),
                    true,
                ),
                Err(_) => unavailable(),
            }
        })
        .await
        .unwrap_or_else(|_| unavailable())
}
