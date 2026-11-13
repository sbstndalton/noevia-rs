//! Exact residual responses, including the early ownership refusals, in Node gate order. Substantive methods retain their owners.
use crate::{identity, reply, routes, App};
use axum::{
    body::Body,
    http::{Request, Response, StatusCode},
    response::IntoResponse,
};
use server_auth::Role;
use std::sync::Arc;

pub(crate) enum Kind {
    Missing,
    #[cfg(feature = "rust-storage-reads")]
    CredentialBypass,
    Method,
    SharedProfilesMethod,
    LowerMethod,
    Downloads,
    #[cfg(all(feature = "rust-chatgpt-status", feature = "rust-storage-reads"))]
    Provider,
}

pub(crate) fn classify(method: &str, path: &str) -> Option<Kind> {
    // Node's exact model mounts: method casing in the response is part of the contract.
    match path {
        "/api/models/installed" if method != "GET" => return Some(Kind::Method),
        "/api/models/downloads" if method != "GET" => return Some(Kind::Downloads),
        "/api/models/capabilities" if method != "GET" => return Some(Kind::Missing),
        "/api/models/hardware" if method != "GET" => return Some(Kind::Method),
        "/api/models/estimate" | "/api/models/preset/suggest" if method != "GET" => {
            return Some(Kind::SharedProfilesMethod)
        }
        _ => {}
    }
    if path == "/api/models/assignments" && method != "GET" {
        return Some(Kind::LowerMethod);
    }
    if path == "/api/models/prune-missing" && method != "POST" {
        return Some(Kind::LowerMethod);
    }
    if path == "/api/models/reranker" && !["GET", "PUT", "DELETE"].contains(&method) {
        return Some(Kind::LowerMethod);
    }
    if let Some(kind) = provider(method, path) {
        return Some(kind);
    }
    if path.starts_with("/api/models/")
        && path.ends_with('/')
        && routes::route_for(path)
            .is_some_and(|r| !r.path.contains('{') && path.trim_end_matches('/') == r.path)
    {
        return Some(Kind::Missing);
    }
    if [
        "/api/models/pull",
        "/api/models/delete",
        "/api/models/load",
        "/api/models/unload",
    ]
    .contains(&path)
        && method != "POST"
    {
        return Some(Kind::Missing);
    }
    // The single-segment model wildcard also covers two substantive Node handlers.
    // All other paths and methods that land on it reach the static fallback.
    if routes::route_for(path).is_some_and(|r| r.path == "/api/models/{*}")
        && !((path == "/api/models/pull" && method == "POST")
            || (path == "/api/models/mtp-artifact" && method == "GET"))
    {
        return Some(Kind::Missing);
    }
    if path.starts_with("/api/models/") && routes::route_for(path).is_none() {
        return Some(Kind::Missing);
    }
    #[cfg(feature = "rust-storage-reads")]
    if ["/api/mcp-keys", "/api/mcp-oauth"]
        .iter()
        .any(|p| path == *p || path.starts_with(&format!("{p}/")))
    {
        if (path == "/api/mcp-keys/servers" && ["PUT", "DELETE"].contains(&method))
            || (["/api/mcp-oauth/servers", "/api/mcp-oauth/callback"].contains(&path)
                && method == "DELETE")
        {
            return Some(Kind::CredentialBypass);
        }
        return (!crate::mcp_routes::owns(method, path)).then_some(Kind::Missing);
    }
    if path.starts_with("/api/integrations/storage/files")
        && (path == "/api/integrations/storage/files"
            || path.starts_with("/api/integrations/storage/files/"))
        && method != "GET"
    {
        return Some(Kind::Missing);
    }
    if (path == "/api/integrations/storage/files"
        || path.starts_with("/api/integrations/storage/files/"))
        && method == "GET"
    {
        return None;
    }
    // These families have no remaining public Node executor. A method excluded by the table,
    // or a trailing slash on a static exact mount, reaches the static JSON 404 in Node.
    let integration = [
        "/api/connectors",
        "/api/integrations/storage",
        "/api/plugins/directory",
        "/api/toolboxes",
        "/api/admin/offsite-backup",
    ]
    .iter()
    .any(|base| path == *base || path.starts_with(&format!("{base}/")));
    if !integration {
        return None;
    }
    let Some(route) = routes::route_for(path) else {
        return Some(Kind::Missing);
    };
    if route.catch_all {
        return Some(Kind::Missing);
    }
    // Dynamic routes have their own validation and feature ordering; leave them to their owner.
    if route.path.contains('{') {
        return None;
    }
    if path != route.path || (!route.methods.is_empty() && !route.methods.contains(&method)) {
        return Some(Kind::Missing);
    }
    None
}

pub(crate) async fn serve(
    app: &Arc<App>,
    req: Request<Body>,
    path: String,
    kind: Kind,
) -> Response<Body> {
    #[cfg(feature = "rust-storage-reads")]
    if matches!(kind, Kind::CredentialBypass) {
        return credential_bypass();
    }
    let (parts, _) = req.into_parts();
    let method = parts.method;
    let creds = server_auth::Creds::from_headers(
        method.as_str(),
        parts
            .headers
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_bytes())),
    );
    let layer = Arc::clone(&app.identity);
    let gate_path = path.clone();
    let id = match tokio::task::spawn_blocking(move || {
        layer.gate_blocking(&creds, &gate_path, identity::now_ms())
    })
    .await
    {
        Ok(Ok(id)) => id,
        Ok(Err(e)) => return e.into_response(),
        Err(_) => return identity::Rejection::Unavailable.into_response(),
    };
    if path.starts_with("/api/models/")
        && (!["GET", "HEAD"].contains(&method.as_str()) || path == "/api/models/reranker")
        && id.user.role != Role::Admin
    {
        return reply::error(StatusCode::FORBIDDEN, "administrator required", true);
    }
    if path.starts_with("/api/admin/") && id.user.role != Role::Admin {
        return reply::error(StatusCode::FORBIDDEN, "administrator required", true);
    }
    match kind {
        #[cfg(feature = "rust-storage-reads")]
        Kind::CredentialBypass => credential_bypass(),
        Kind::Missing => reply::error(StatusCode::NOT_FOUND, "not found", true),
        Kind::SharedProfilesMethod => {
            if id.user.role != Role::Admin {
                reply::error(
                    StatusCode::FORBIDDEN,
                    "Administrator required for shared model profiles",
                    true,
                )
            } else {
                reply::error(StatusCode::METHOD_NOT_ALLOWED, "Method not allowed", true)
            }
        }
        Kind::Method => reply::error(StatusCode::METHOD_NOT_ALLOWED, "Method not allowed", true),
        Kind::LowerMethod => {
            reply::error(StatusCode::METHOD_NOT_ALLOWED, "method not allowed", true)
        }
        #[cfg(all(feature = "rust-chatgpt-status", feature = "rust-storage-reads"))]
        Kind::Provider => {
            let a = Arc::clone(app);
            let enabled = match tokio::task::spawn_blocking(move || {
                a.chatgpt_oauth.as_ref().map_or(Ok(false), |b| b.flag(&a))
            })
            .await
            {
                Ok(Ok(enabled)) => enabled,
                _ => return reply::error(StatusCode::SERVICE_UNAVAILABLE, "Internal error", true),
            };
            if enabled {
                reply::error(StatusCode::METHOD_NOT_ALLOWED, "method not allowed", true)
            } else {
                reply::error(
                    StatusCode::NOT_FOUND,
                    server_chatgpt_status::OFF_MESSAGE,
                    true,
                )
            }
        }
        Kind::Downloads => crate::model_downloads_read::serve(app).await,
    }
}

#[cfg(all(feature = "rust-chatgpt-status", feature = "rust-storage-reads"))]
fn provider(method: &str, path: &str) -> Option<Kind> {
    if path != "/api/providers" && !path.starts_with("/api/providers/") {
        return None;
    }
    if path == "/api/providers" && ["GET", "POST"].contains(&method) {
        return None;
    }
    if path == "/api/providers/test" && method == "POST" {
        return None;
    }
    if let Some(rest) = path.strip_prefix("/api/providers/") {
        if !rest.is_empty()
            && !rest.contains('/')
            && (method == "DELETE" || (method == "PUT" && path != server_chatgpt_status::ROUTE))
        {
            return None;
        }
        if let Some(id) = rest.strip_suffix("/models") {
            if !id.is_empty() && !id.contains('/') && path != crate::chatgpt_oauth_routes::MODELS {
                return None;
            }
        }
    }
    if (path == server_chatgpt_status::ROUTE || path == crate::chatgpt_oauth_routes::MODELS)
        && method == "GET"
    {
        return None;
    }
    if [
        crate::chatgpt_oauth_routes::DEVICE,
        crate::chatgpt_oauth_routes::POLL,
        crate::chatgpt_oauth_routes::CANCEL,
    ]
    .contains(&path)
        && method == "POST"
    {
        return None;
    }
    Some(
        if path == server_chatgpt_status::ROUTE
            || path.starts_with(&format!("{}/", server_chatgpt_status::ROUTE))
        {
            Kind::Provider
        } else {
            Kind::Missing
        },
    )
}
#[cfg(not(all(feature = "rust-chatgpt-status", feature = "rust-storage-reads")))]
fn provider(_method: &str, _path: &str) -> Option<Kind> {
    None
}

#[cfg(feature = "rust-storage-reads")]
fn credential_bypass() -> Response<Body> {
    reply::json_text(StatusCode::SERVICE_UNAVAILABLE, r#"{"error":"MCP server keys and sign-ins are owned by the Rust front.","code":"RUST_MCP_CREDENTIALS_OWNED"}"#.to_string(), true)
}

#[cfg(test)]
mod tests {
    use super::classify;

    #[test]
    fn substantive_model_wildcard_handlers_keep_their_owner() {
        for (method, path) in [
            ("POST", "/api/models/pull"),
            ("GET", "/api/models/mtp-artifact"),
        ] {
            assert!(classify(method, path).is_none(), "{method} {path}");
        }
    }
}
