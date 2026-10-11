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
    app.storage_metadata
        .run(move || {
            // The permit is already owned inside this job, before authentication or file IO.
            let id = match layer.gate_blocking(&creds, PATH, now) {
                Ok(id) => id,
                Err(rejection) => return rejection.into_response(),
            };
            let Some(data_dir) = data_dir else {
                return unavailable();
            };
            let previous = std::env::var("SECRETS_KEY_PREVIOUS")
                .ok()
                .map(zeroize::Zeroizing::new);
            let keys = match server_storage_reads::owner_secret::load_existing_keys(
                &data_dir,
                previous.as_deref().map(String::as_str),
            ) {
                Ok(keys) => keys,
                Err(_) => return unavailable(),
            };
            let store = match layer.store_blocking() {
                Ok(store) => store,
                Err(_) => return unavailable(),
            };
            match server_storage_reads::owner_secret::project_connection(&store, &id, &keys) {
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
