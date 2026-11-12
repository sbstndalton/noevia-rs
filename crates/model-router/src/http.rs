//! The OpenAI-compatible front: `GET /health`, `GET /v1/models`, `POST /v1/*` (proxied, streamed
//! as it arrives), `GET /router/status` and `POST /router/unload`.

use crate::router::{AcquireError, Router};
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::{header, HeaderMap, HeaderName, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Json;
use futures_util::StreamExt;
use serde_json::{json, Value};

/// Hop-by-hop headers (RFC 9110 7.6.1), never forwarded either way.
const HOP: [&str; 9] = [
    "connection",
    "keep-alive",
    "proxy-connection",
    "transfer-encoding",
    "te",
    "trailer",
    "upgrade",
    "content-length",
    "host",
];

fn is_hop(name: &HeaderName) -> bool {
    HOP.contains(&name.as_str())
}

pub fn app(router: Router) -> axum::Router {
    let limit = router.config().max_body_bytes;
    axum::Router::new()
        .route("/health", get(health))
        .route("/v1/models", get(models))
        .route("/router/status", get(status))
        .route("/router/unload", post(unload))
        .route("/v1/{*rest}", post(proxy))
        .fallback(not_found)
        .method_not_allowed_fallback(not_allowed)
        .layer(DefaultBodyLimit::max(limit))
        .with_state(router)
}

fn error(code: StatusCode, message: impl Into<String>, kind: &str) -> Response {
    let message: String = message.into();
    (
        code,
        Json(json!({"error": {"message": message, "type": kind, "code": code.as_u16()}})),
    )
        .into_response()
}

async fn not_found(method: Method, uri: axum::http::Uri) -> Response {
    error(
        StatusCode::NOT_FOUND,
        format!("no route {method} {}", uri.path()),
        "invalid_request_error",
    )
}

async fn not_allowed(method: Method, uri: axum::http::Uri) -> Response {
    error(
        StatusCode::METHOD_NOT_ALLOWED,
        format!("{method} is not allowed on {}", uri.path()),
        "invalid_request_error",
    )
}

async fn health(State(r): State<Router>) -> Response {
    Json(json!({"status": "ok", "loaded": r.loaded_name()})).into_response()
}

async fn models(State(r): State<Router>) -> Response {
    let loaded = r.loaded_name();
    let cfg = r.config();
    let data: Vec<Value> = cfg
        .names
        .iter()
        .filter_map(|(id, i)| cfg.models.get(*i).map(|m| (id, m)))
        .filter(|(_, m)| !m.unlisted)
        .map(|(id, m)| {
            let state = if loaded.as_deref() == Some(m.name.as_str()) {
                "loaded"
            } else {
                "unloaded"
            };
            json!({"id": id, "object": "model", "owned_by": m.engine, "status": {"value": state}})
        })
        .collect();
    Json(json!({"object": "list", "data": data})).into_response()
}

async fn status(State(r): State<Router>) -> Response {
    Json(r.status()).into_response()
}

async fn unload(State(r): State<Router>) -> Response {
    match r.unload().await {
        Ok(name) => Json(json!({"unloaded": name, "reason": "request"})).into_response(),
        Err(e) => error(
            StatusCode::SERVICE_UNAVAILABLE,
            e.to_string(),
            "backend_unavailable",
        ),
    }
}

async fn proxy(State(r): State<Router>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let limit = r.config().max_body_bytes;
    let body = match axum::body::to_bytes(body, limit).await {
        Ok(b) => b,
        Err(_) => {
            return error(
                StatusCode::PAYLOAD_TOO_LARGE,
                format!("request body over {limit} bytes"),
                "invalid_request_error",
            )
        }
    };
    let mut doc: Value = if body.is_empty() {
        json!({})
    } else {
        match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(_) => {
                return error(
                    StatusCode::BAD_REQUEST,
                    "body is not JSON",
                    "invalid_request_error",
                )
            }
        }
    };
    let Some(obj) = doc.as_object_mut() else {
        return error(
            StatusCode::BAD_REQUEST,
            "body is not a JSON object",
            "invalid_request_error",
        );
    };
    let asked = obj
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let cfg = r.config();
    let Some(index) = cfg.names.get(&asked).copied() else {
        return error(
            StatusCode::NOT_FOUND,
            format!("unknown model {asked:?}; GET /v1/models lists them"),
            "model_not_found",
        );
    };
    let mut body = body;
    if let Some(m) = cfg.models.get(index) {
        if !m.upstream_model.is_empty() {
            obj.insert("model".into(), Value::String(m.upstream_model.clone()));
            match serde_json::to_vec(&doc) {
                Ok(v) => body = Bytes::from(v),
                Err(e) => {
                    return error(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        e.to_string(),
                        "router_error",
                    )
                }
            }
        }
    }
    let lease = match r.acquire(index).await {
        Ok(l) => l,
        Err(AcquireError::ShuttingDown) => {
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "router is shutting down",
                "backend_unavailable",
            )
        }
        Err(AcquireError::Failed(e)) => {
            return error(StatusCode::SERVICE_UNAVAILABLE, e, "backend_unavailable")
        }
    };
    let model = lease.model();
    let path_q = parts
        .uri
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("/");
    let url = format!("{}{}", model.proxy, path_q);
    let mut headers = HeaderMap::new();
    for (k, v) in parts.headers.iter() {
        if !is_hop(k) {
            headers.append(k.clone(), v.clone());
        }
    }
    let upstream = match r
        .client()
        .post(&url)
        .headers(headers)
        .body(body)
        .send()
        .await
    {
        Ok(u) => u,
        Err(e) => {
            return error(
                StatusCode::BAD_GATEWAY,
                format!("{}: upstream error {e}", model.name),
                "backend_error",
            )
        }
    };
    let mut resp = Response::builder().status(upstream.status().as_u16());
    for (k, v) in upstream.headers() {
        if !is_hop(k) {
            resp = resp.header(k, v);
        }
    }
    if let Some(len) = upstream.headers().get(header::CONTENT_LENGTH) {
        resp = resp.header(header::CONTENT_LENGTH, len);
    }
    // The lease lives as long as the body: when the client goes away hyper drops the body, which
    // drops the upstream response (closing that connection cancels the generation) and the lease.
    let stream = upstream.bytes_stream().map(move |chunk| {
        let _held = &lease;
        chunk
    });
    resp.body(Body::from_stream(stream)).unwrap_or_else(|e| {
        error(
            StatusCode::INTERNAL_SERVER_ERROR,
            e.to_string(),
            "router_error",
        )
    })
}
