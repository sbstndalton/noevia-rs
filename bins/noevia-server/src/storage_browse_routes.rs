//! Connected-storage list and text read (M4, sbstndalton/noevia#1368), behind
//! NOEVIA_RUST_STORAGE_BROWSE:
//!
//! * `GET /api/integrations/storage/files[/<path>]` answers `{"entries":[...]}`;
//! * `POST /api/integrations/storage/file` with `{"path":...}` answers the capped text preview.
//!
//! Order follows core `routes/storage.cjs`: the gate (auth, CSRF for the POST) first, then the
//! POST body, the caller's own connection row, the browsable-kind check, the endpoint policy
//! (`endpointApproved`: an admin may use any http(s) origin, a member only the origins in
//! MEMBER_OUTBOUND_ORIGINS, read per request) and only then outbound work. Credentials come only
//! from the caller's stored row, never from the request.
//!
//! Bounds: at most four operations at once (the engine's `Admission`) and one per account (a request waits up to
//! [`SLOT_WAIT`] for a slot); each outbound attempt runs in a fresh owned child process with a
//! capped reply (crate `transport`); a listing keeps at most one 4 MiB page plus 16 MiB of
//! entries and 16 MiB of JSON; a file read keeps at most 800 000 bytes and 200 000 UTF-16 units of
//! preview. Overall deadlines are the engine's (31 s DAV list, 610 s S3 list, 41 s read).
//! A cancelled request kills the child.
//!
//! Memory per request in this process (the 64 MiB budget, #1368): the POST body is capped at
//! 1 MiB; one reply frame of at most cap + 19 bytes is decoded into text of at most three times
//! the cap (U+FFFD expansion; the engine refuses more), and the frame is freed before parsing.
//! A listing then holds that page text (12 MiB at most), the parsed entries (16 MiB) and the
//! response JSON (16 MiB): about 44 MiB at the peak, less once the page is dropped; a file read
//! holds at most 800 000 bytes of text, about 2.4 MB decoded, and a 200 000-unit preview. The
//! child is a separate process with a 256 MiB address-space limit set before READY.
use crate::{identity, reply, App};
use axum::{
    body::Body,
    http::{Method, Request, Response, StatusCode},
    response::IntoResponse,
};
use server_storage_reads::{
    admission::{Admission, Permit},
    owner_secret, protocol, Failure,
};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

pub const FILES: &str = "/api/integrations/storage/files";
pub const FILE: &str = "/api/integrations/storage/file";
/// How long a signed-in request waits for an operation slot before a 503.
pub const SLOT_WAIT: Duration = Duration::from_secs(2);
const BODY_WAIT: Duration = Duration::from_secs(10);

/// The child mode this binary runs (main.rs).
pub fn child_command() -> Option<std::path::PathBuf> {
    std::env::current_exe().ok()
}

fn failure_reply(status: u16, failure: &Failure) -> Response<Body> {
    reply::json_text(
        StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
        failure.json(),
        true,
    )
}
fn unavailable() -> Response<Body> {
    reply::error(
        StatusCode::SERVICE_UNAVAILABLE,
        "Storage reads unavailable",
        true,
    )
}

/// Sets the cancellation flag when the request future is dropped, so the owned child is killed.
struct CancelOnDrop(Arc<AtomicBool>);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

async fn admit(admission: &Admission, owner: &str) -> Option<Permit> {
    let end = Instant::now() + SLOT_WAIT;
    loop {
        if let Ok(permit) = admission.try_acquire(owner) {
            return Some(permit);
        }
        if Instant::now() >= end {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// `decodeURIComponent` of the list suffix: percent-decoded bytes that must be UTF-8.
fn decode_component(raw: &str) -> Result<Vec<u16>, Failure> {
    let malformed = || Failure::new(502, "URI malformed");
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while let Some(&b) = bytes.get(i) {
        if b == b'%' {
            let hex = bytes.get(i + 1..i + 3).ok_or_else(malformed)?;
            let text = std::str::from_utf8(hex).map_err(|_| malformed())?;
            out.push(u8::from_str_radix(text, 16).map_err(|_| malformed())?);
            i += 3;
        } else {
            out.push(b);
            i += 1;
        }
    }
    let text = String::from_utf8(out).map_err(|_| malformed())?;
    Ok(text.encode_utf16().collect())
}

pub async fn serve(
    app: &Arc<App>,
    conn: crate::serve::Conn,
    req: Request<Body>,
    path: String,
) -> Response<Body> {
    let list_suffix = path
        .strip_prefix(FILES)
        .filter(|rest| rest.is_empty() || rest.starts_with('/'));
    let operation = match (req.method(), list_suffix) {
        (&Method::GET, Some(_)) => protocol::Operation::List,
        (&Method::POST, None) if path == FILE => protocol::Operation::Read,
        _ => return app.proxy.forward(conn.peer, req).await,
    };
    if !app.config.rust_storage_browse {
        return app.proxy.forward(conn.peer, req).await;
    }
    let (parts, body) = req.into_parts();
    let creds = identity::creds_of(&parts);
    let layer = Arc::clone(&app.identity);
    let now = identity::now_ms();
    let gate_layer = Arc::clone(&layer);
    let gate_path = path.clone();
    let id = match tokio::task::spawn_blocking(move || {
        gate_layer.gate_blocking(&creds, &gate_path, now)
    })
    .await
    {
        Ok(Ok(id)) => id,
        Ok(Err(rejection)) => return rejection.into_response(),
        Err(_) => return unavailable(),
    };
    // The requested path: the POST body's `path`, or the decoded list suffix. Decoding failures
    // are answered after the connection checks, where Node's try/catch reports them.
    let requested: Result<Vec<u16>, Failure> = match operation {
        protocol::Operation::Read => {
            let read = tokio::time::timeout(
                BODY_WAIT,
                crate::account_routes::read_body(body, server_storage_reads::BODY_BYTES),
            )
            .await;
            let Ok(Ok(body)) = read else {
                return reply::error(StatusCode::BAD_REQUEST, "invalid JSON", true);
            };
            match server_storage_reads::json::path(&body.bytes) {
                Ok(units) => Ok(units),
                // Body errors are thrown by readJson before the connection is read.
                Err(failure) => return failure_reply(failure.status, &failure),
            }
        }
        protocol::Operation::List => {
            drop(body);
            let suffix = list_suffix
                .and_then(|rest| rest.strip_prefix('/'))
                .unwrap_or("");
            decode_component(suffix)
        }
    };
    let Some(permit) = admit(&app.storage_browse, id.user.id.as_str()).await else {
        return unavailable();
    };
    let Some(program) = app.storage_child.clone() else {
        return unavailable();
    };
    let data_dir = app.config.data_dir.clone();
    let cancelled = Arc::new(AtomicBool::new(false));
    let _cancel = CancelOnDrop(Arc::clone(&cancelled));
    let job = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let Some(data_dir) = data_dir else {
            return unavailable();
        };
        let previous = std::env::var("SECRETS_KEY_PREVIOUS")
            .ok()
            .map(zeroize::Zeroizing::new);
        let keys =
            owner_secret::load_existing_keys(&data_dir, previous.as_deref().map(String::as_str))
                .ok();
        let Ok(store) = layer.store_blocking() else {
            return unavailable();
        };
        let Ok(projected) = owner_secret::project_connection_with(&store, &id, keys.as_ref())
        else {
            return unavailable();
        };
        drop(store);
        let connection = projected.connection();
        let status_of = |failure: &Failure| match operation {
            protocol::Operation::List if failure.status == 400 => 400,
            protocol::Operation::List => 502,
            protocol::Operation::Read => failure.status,
        };
        if connection.kind != protocol::Kind::Local {
            let admin = projected.effective_role() == server_auth::Role::Admin;
            let members = std::env::var("MEMBER_OUTBOUND_ORIGINS").unwrap_or_default();
            if !protocol::endpoint_approved(&connection.base_url, admin, &members) {
                return failure_reply(400, &Failure::new(400, protocol::ENDPOINT_REFUSAL));
            }
        }
        let units = match requested {
            Ok(units) => units,
            Err(failure) if connection.kind != protocol::Kind::Local => {
                return failure_reply(status_of(&failure), &failure)
            }
            // Node checks the kind before decoding the path; execute does the same.
            Err(_) => Vec::new(),
        };
        let mut transport = server_storage_reads::transport::ChildTransport::new(
            program,
            &[server_storage_reads::transport::CHILD_ARG],
            id.user.id.as_str(),
            &cancelled,
        );
        match protocol::execute(connection, &units, operation, &mut transport) {
            Ok(body) => {
                let body = if operation == protocol::Operation::List {
                    format!("{{\"entries\":{body}}}")
                } else {
                    body
                };
                reply::json_text(StatusCode::OK, body, true)
            }
            Err(failure) => failure_reply(status_of(&failure), &failure),
        }
    });
    job.await.unwrap_or_else(|_| unavailable())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    #[test]
    fn decode_component_is_decode_uri_component() {
        let s = |v: &str| String::from_utf16(&decode_component(v).unwrap()).unwrap();
        assert_eq!(s(""), "");
        assert_eq!(s("docs/a%20b"), "docs/a b");
        assert_eq!(s("%2Fx"), "/x");
        assert_eq!(s("%C3%A9"), "é");
        assert_eq!(s("a+b"), "a+b");
        for bad in ["%", "%2", "%zz", "%C3", "%FF", "%ED%A0%80"] {
            let failure = decode_component(bad).unwrap_err();
            assert_eq!(
                (failure.status, failure.message.as_str()),
                (502, "URI malformed")
            );
        }
    }
}
