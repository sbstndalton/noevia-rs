//! The three read-only web-address GETs. Node still owns address changes and earlier-address
//! removal; every response reads its current settings through the existing read-only store.
use crate::{identity, reply, App};
use axum::body::Body;
use axum::http::{Request, Response, StatusCode};
use axum::response::IntoResponse;
use server_auth::Role;
use server_store::{Reader, StoreError};
use std::sync::Arc;

fn setting(reader: &Reader<'_>, key: &str) -> Result<Option<String>, StoreError> {
    reader.row("SELECT value FROM settings WHERE key=?1", [key], |row| {
        row.get(0)
    })
}

#[derive(Debug, PartialEq, Eq)]
struct Address {
    origin: String,
    source: &'static str,
    previous: Vec<String>,
    rp_id: String,
}

fn address(reader: &Reader<'_>, public_origin: &str, rp_env: &str) -> Result<Address, StoreError> {
    let admin = setting(reader, "public_origin_admin")?.unwrap_or_default();
    let setup = setting(reader, "public_origin")?.unwrap_or_default();
    let (origin, source) = if !admin.is_empty() {
        (admin, "settings")
    } else if !public_origin.is_empty() {
        (public_origin.to_owned(), "environment")
    } else if !setup.is_empty() {
        (setup, "setup")
    } else {
        (String::new(), "none")
    };
    let raw = setting(reader, "previous_origins")?.unwrap_or_default();
    // auth.cjs refreshOrigin: malformed JSON is [], and non-string array members are dropped.
    let previous = serde_json::from_str::<serde_json::Value>(&raw)
        .ok()
        .and_then(|value| value.as_array().cloned())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|value| value.as_str().map(ToOwned::to_owned))
        .collect();
    let rp_id = server_account::util::rp_for(&origin, rp_env)
        .map_err(|_| StoreError::NotReady("invalid public origin".into()))?;
    Ok(Address {
        origin,
        source,
        previous,
        rp_id,
    })
}

fn related(address: &Address, setup: Option<String>) -> Vec<String> {
    let mut origins = Vec::new();
    // auth.cjs passkeyOrigins: current origin or setup, then earlier origins, distinct and truthy.
    let current = if address.origin.is_empty() {
        setup.unwrap_or_default()
    } else {
        address.origin.clone()
    };
    for origin in std::iter::once(current).chain(address.previous.iter().cloned()) {
        if !origin.is_empty() && !origins.contains(&origin) {
            origins.push(origin);
        }
    }
    origins
}

fn unavailable(api: bool) -> Response<Body> {
    reply::error(
        StatusCode::SERVICE_UNAVAILABLE,
        "Web address is unavailable right now.",
        api,
    )
}

pub async fn serve(app: &Arc<App>, req: Request<Body>, path: String) -> Response<Body> {
    let (parts, _) = req.into_parts();
    let creds = identity::creds_of(&parts);
    let layer = Arc::clone(&app.identity);
    let settings = app.config.account.clone();
    let api = path.starts_with("/api/");
    tokio::task::spawn_blocking(move || {
        let Some(settings) = settings else { return unavailable(api) };
        if path == "/api/admin/web-address" {
            let id = match layer.gate_blocking(&creds, &path, identity::now_ms()) {
                Ok(id) => id,
                Err(rejection) => return rejection.into_response(),
            };
            if id.user.role != Role::Admin {
                return reply::error(StatusCode::FORBIDDEN, "Administrator required", true);
            }
        }
        let Ok(store) = layer.store_blocking() else { return unavailable(api) };
        match path.as_str() {
            "/api/instance" => {
                let Ok(Some(id)) = store.read(|reader| setting(reader, "instance_id")) else {
                    return unavailable(api);
                };
                if id.is_empty() { return unavailable(api) }
                reply::json_text(StatusCode::OK, format!(r#"{{"id":{}}}"#, serde_json::to_string(&id).unwrap_or_default()), api)
            }
            "/.well-known/webauthn" => {
                let Ok(origins) = store.read(|reader| {
                    let view = address(reader, &settings.public_origin, &settings.webauthn_rp_id)?;
                    let setup = setting(reader, "public_origin")?;
                    Ok(related(&view, setup))
                }) else { return unavailable(api) };
                reply::json(StatusCode::OK, &serde_json::json!({"origins": origins}), api)
            }
            "/api/admin/web-address" => {
                let Ok(view) = store.read(|reader| address(reader, &settings.public_origin, &settings.webauthn_rp_id)) else {
                    return unavailable(api);
                };
                let origin = serde_json::to_string(&view.origin).unwrap_or_default();
                let previous = serde_json::to_string(&view.previous).unwrap_or_default();
                let rp_id = serde_json::to_string(&view.rp_id).unwrap_or_default();
                reply::json_text(StatusCode::OK, format!(r#"{{"origin":{origin},"source":"{}","previous":{previous},"rpId":{rp_id}}}"#, view.source), api)
            }
            _ => unavailable(api),
        }
    }).await.unwrap_or_else(|_| unavailable(api))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn related_origins_dedup_current_and_prior_only() {
        let view = Address {
            origin: "https://new.example".into(),
            source: "settings",
            previous: vec![
                "https://old.example".into(),
                "https://new.example".into(),
                "https://old.example".into(),
            ],
            rp_id: "new.example".into(),
        };
        assert_eq!(
            related(&view, None),
            ["https://new.example", "https://old.example"]
        );
    }
}
