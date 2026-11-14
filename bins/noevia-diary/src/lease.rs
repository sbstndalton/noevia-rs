//! R9 main mode (front.rs), S3: the Python child's tenant writer lock is a LEASE on this process's
//! writer slot (`write::Writers`), so a writer tenant has one writer across both processes. Control
//! token routes on the control port:
//! - `POST /internal/lease?tenant=<uuid>&wait_ms=<n>&holder=<nonce>`: 200 `{"lease":"<id>"}` once
//!   the slot is the lease's, 503 `{"detail":"busy"}` when another holder kept it for `wait_ms` (at
//!   most 60 s). A lease granted after the request went away is returned at once;
//! - `DELETE /internal/lease?lease=<id>` (or `?holder=<nonce>`: every lease granted to that nonce,
//!   for a caller whose reply was lost): 200, or 404 when nothing was live;
//! - `GET /internal/leases`: the live leases (tenant, seconds held), longest first.
//!
//! A caller holding a lease names it in `x-noevia-diary-lease` on the routes it calls while holding
//! it (`/internal/reindex`, `/internal/fence`): they join it instead of waiting for themselves.

use crate::py;
use crate::server::{is_uuid, App, CONTROL_HEADER};
use axum::body::Body;
use axum::http::{HeaderMap, Method, Response};
use serde_json::json;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use subtle::ConstantTimeEq;

pub const LEASE_HEADER: &str = "x-noevia-diary-lease";
const MAX_WAIT: Duration = Duration::from_secs(60);

#[derive(Default)]
struct Handoff {
    granted: Option<u64>,
    abandoned: bool,
}

/// Dropped with the request future: a lease granted but not yet taken is returned.
struct Abandon {
    app: Arc<App>,
    handoff: Arc<Mutex<Handoff>>,
}

impl Drop for Abandon {
    fn drop(&mut self) {
        let mut h = self.handoff.lock().unwrap_or_else(|e| e.into_inner());
        h.abandoned = true;
        if let Some(id) = h.granted.take() {
            drop(h);
            self.app.writers.lease_release(id);
        }
    }
}

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
            let holder = py::query_get(query, "holder").unwrap_or_default();
            // The grant is handed over through `Handoff`: when this request future is dropped
            // (the caller went away) before taking it, the lease is returned, whenever it comes.
            let handoff = Arc::new(Mutex::new(Handoff::default()));
            let guard = Abandon {
                app: app.clone(),
                handoff: handoff.clone(),
            };
            let (app2, h2) = (app.clone(), handoff.clone());
            let task = tokio::task::spawn_blocking(move || {
                let r = app2.writers.lease_acquire_for(&uid, wait, &holder);
                if let Ok(id) = r {
                    let mut h = h2.lock().unwrap_or_else(|e| e.into_inner());
                    if h.abandoned {
                        drop(h);
                        app2.writers.lease_release(id);
                    } else {
                        h.granted = Some(id);
                    }
                }
                r.is_ok()
            });
            let ok = task.await.unwrap_or(false);
            let id = handoff
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .granted
                .take();
            drop(guard);
            match (ok, id) {
                (true, Some(id)) => status(200, json!({"lease": id.to_string()})),
                (false, _) => status(503, json!({"detail": "busy"})),
                (true, None) => status(500, json!({})),
            }
        }
        ("DELETE", "/internal/lease") => {
            let holder = py::query_get(query, "holder").unwrap_or_default();
            let released = match py::query_get(query, "lease").and_then(|v| v.parse::<u64>().ok()) {
                Some(id) => app.writers.lease_release(id),
                None => app.writers.lease_release_holder(&holder) > 0,
            };
            if released {
                status(200, json!({}))
            } else {
                status(404, json!({}))
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
