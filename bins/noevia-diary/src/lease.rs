//! R9 main mode (front.rs), S3: the Python child's tenant writer lock is a LEASE on this process's
//! writer slot (`write::Writers`), so a writer tenant has one writer across both processes. Control
//! token routes on the control port:
//! - `POST /internal/lease?tenant=<uuid>&wait_ms=<n>`: 200 `{"lease":"<id>"}` once the slot is the
//!   lease's, 503 `{"detail":"busy"}` when another holder kept it for `wait_ms` (at most 60 s);
//! - `DELETE /internal/lease?lease=<id>`: 200, or 404 when the lease is not live;
//! - `GET /internal/leases`: the live leases (tenant, seconds held), longest first.
//!
//! A caller holding a lease names it in `x-noevia-diary-lease` on the routes it calls while holding
//! it (`/internal/reindex`, `/internal/fence`): they join it instead of waiting for themselves.

use crate::py;
use crate::server::{is_uuid, App, CONTROL_HEADER};
use axum::body::Body;
use axum::http::{HeaderMap, Method, Response};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use subtle::ConstantTimeEq;

pub const LEASE_HEADER: &str = "x-noevia-diary-lease";
const MAX_WAIT: Duration = Duration::from_secs(60);

/// The lease a request says its caller holds.
pub fn lease_of(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(LEASE_HEADER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
}

fn status(code: u16, body: serde_json::Value) -> Response<Body> {
    crate::server::json_response(code, &body)
}

/// The lease routes, or None when `path` is not one of them.
pub async fn internal(
    app: &Arc<App>,
    method: &Method,
    path: &str,
    query: &[u8],
    headers: &HeaderMap,
) -> Option<Response<Body>> {
    if path != "/internal/lease" && path != "/internal/leases" {
        return None;
    }
    let supplied = headers
        .get(CONTROL_HEADER)
        .map(|v| v.as_bytes().to_vec())
        .unwrap_or_default();
    if app.control_token.is_empty()
        || !bool::from(supplied.as_slice().ct_eq(app.control_token.as_bytes()))
    {
        return Some(status(403, json!({})));
    }
    Some(match (method.as_str(), path) {
        ("POST", "/internal/lease") => {
            let uid = py::query_get(query, "tenant")
                .unwrap_or_default()
                .to_ascii_lowercase();
            if !is_uuid(&uid) {
                return Some(status(400, json!({"detail": "invalid tenant"})));
            }
            let wait = py::query_get(query, "wait_ms")
                .and_then(|v| v.parse::<u64>().ok())
                .map(Duration::from_millis)
                .unwrap_or(Duration::ZERO)
                .min(MAX_WAIT);
            let app2 = app.clone();
            match tokio::task::spawn_blocking(move || app2.writers.lease_acquire(&uid, wait)).await
            {
                Ok(Ok(id)) => status(200, json!({"lease": id.to_string()})),
                Ok(Err(_)) => status(503, json!({"detail": "busy"})),
                Err(_) => status(500, json!({})),
            }
        }
        ("DELETE", "/internal/lease") => {
            match py::query_get(query, "lease").and_then(|v| v.parse::<u64>().ok()) {
                Some(id) if app.writers.lease_release(id) => status(200, json!({})),
                _ => status(404, json!({})),
            }
        }
        ("GET", "/internal/leases") => status(
            200,
            json!({"leases": app
                .writers
                .lease_report()
                .into_iter()
                .map(|(tenant, held_s)| json!({"tenant": tenant, "held_s": held_s}))
                .collect::<Vec<_>>()}),
        ),
        _ => status(405, json!({})),
    })
}
