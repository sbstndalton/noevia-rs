//! M3 (permanent since sbstndalton/noevia#1071): the front answers sign-in and the account routes itself
//! (crates/server-account) instead of proxying them. What the route module would hand on to a
//! later Node module ([`server_account::Outcome::Pass`]) still goes to Node, with the body the
//! front already read.
//!
//! The client address is the front's own peer (or, with TRUST_PROXY, the rightmost
//! X-Forwarded-For entry), so per-address sign-in limits and audit addresses are per client even
//! without TRUST_PROXY (sbstndalton/noevia#1245): Node behind the front saw only loopback.

use crate::reply;
use crate::serve::Conn;
use crate::App;
use axum::body::Body;
use axum::http::{header, HeaderValue, Request, Response, StatusCode};
use axum::response::IntoResponse;
use http_body_util::BodyExt;
use server_account::{Account, Outcome, Settings, BODY_LIMIT};
use server_auth::{Authenticator, Creds};
use std::sync::{Arc, Mutex};

pub struct AccountLayer {
    settings: Option<Settings>,
    auth: Option<server_auth::AuthConfig>,
    /// Built on first use, once the writer opens, and kept: it holds the rate limiters.
    slot: Mutex<Option<Arc<Account>>>,
}

impl AccountLayer {
    pub fn new(config: &crate::config::Config) -> Self {
        AccountLayer {
            settings: config.account.clone(),
            auth: config.auth.clone().ok(),
            slot: Mutex::new(None),
        }
    }

    fn account(&self, app: &App, now: i64) -> Option<Arc<Account>> {
        let mut slot = self.slot.lock().ok()?;
        if let Some(a) = slot.as_ref() {
            return Some(Arc::clone(a));
        }
        let (settings, auth, switch) = (
            self.settings.clone()?,
            self.auth.clone()?,
            app.writes.switch(),
        );
        let writer = app.writes.writer().ok()?;
        let a = Account::new(
            writer,
            Arc::new(Authenticator::new(auth)),
            settings,
            switch,
            now,
        );
        let a = Arc::new(match &app.feature_context {
            Some(context) => a.with_feature_context(context.controller()),
            None => a,
        });
        *slot = Some(Arc::clone(&a));
        Some(a)
    }
}

fn first_header(h: &axum::http::HeaderMap, name: header::HeaderName) -> String {
    h.get(name)
        .map(|v| server_auth::request::latin1(v.as_bytes()))
        .unwrap_or_default()
}

/// Reads at most `limit` + 1 bytes of the body.
pub(crate) async fn read_body(body: Body, limit: usize) -> Result<server_account::Body, ()> {
    let mut out = server_account::Body::default();
    let mut body = body;
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|_| ())?;
        if let Ok(data) = frame.into_data() {
            out.bytes.extend_from_slice(&data);
            if out.bytes.len() > limit {
                out.over = true;
                out.bytes.truncate(limit + 1);
                break;
            }
        }
    }
    Ok(out)
}

/// One request for a Rust-owned account route; `path` is the WHATWG pathname.
pub async fn serve(app: &Arc<App>, conn: Conn, req: Request<Body>, path: String) -> Response<Body> {
    let now = crate::identity::now_ms();
    let (parts, body) = req.into_parts();
    let framing = path == "/api/chat-framing/preferences";
    let features = server_account::features::owns(&path);
    if framing || features {
        let layer = Arc::clone(&app.identity);
        let creds = crate::identity::creds_of(&parts);
        let gate_path = path.clone();
        match tokio::task::spawn_blocking(move || layer.gate_current_blocking(&creds, &gate_path))
            .await
        {
            Ok(Ok(identity)) => {
                if features
                    && path != "/api/features"
                    && identity.user.role != server_auth::Role::Admin
                {
                    return reply::error(StatusCode::FORBIDDEN, "Administrator required", true);
                }
            }
            Ok(Err(rejected)) => return rejected.into_response(),
            Err(_) => return crate::identity::Rejection::Unavailable.into_response(),
        }
    }
    if features
        && path.starts_with("/api/admin/features/")
        && server_account::features::decode_suffix(&path).is_err()
    {
        return reply::error(StatusCode::INTERNAL_SERVER_ERROR, "Internal error", true);
    }
    // Node never calls readJson for framing GET or unsupported methods. Header
    // admission precedes PUT body polling, including transport failures.
    let feature_body =
        features && path.starts_with("/api/admin/features/") && parts.method == "PUT";
    let body = if (framing && parts.method != "PUT") || (features && !feature_body) {
        server_account::Body::default()
    } else {
        match read_body(body, BODY_LIMIT).await {
            Ok(body) => body,
            Err(()) => {
                return reply::error(
                    StatusCode::BAD_REQUEST,
                    if features {
                        "invalid JSON"
                    } else if framing {
                        "Invalid framing preferences"
                    } else {
                        "invalid request body"
                    },
                    true,
                )
            }
        }
    };
    let forwarded = parts
        .headers
        .get_all("x-forwarded-for")
        .iter()
        .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned())
        .collect::<Vec<_>>()
        .join(", ");
    let request = server_account::Request {
        method: parts.method.as_str().to_string(),
        path,
        creds: Creds::from_headers(
            parts.method.as_str(),
            parts
                .headers
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_bytes())),
        ),
        user_agent: first_header(&parts.headers, header::USER_AGENT),
        content_type: first_header(&parts.headers, header::CONTENT_TYPE),
        client_ip: server_account::util::client_address(
            conn.peer.ip(),
            (!forwarded.is_empty()).then_some(forwarded.as_str()),
            app.config.trust_proxy,
        ),
        body,
    };
    let layer = Arc::clone(&app.accounts);
    let app2 = Arc::clone(app);
    let outcome = tokio::task::spawn_blocking(move || {
        let account = layer.account(&app2, now)?;
        let outcome = if framing || features || server_account::device_route(&request.path) {
            // The live clock is first sampled INSIDE public Account entry,
            // after body and lazy Account initialization, then before/after
            // all initial auth and final writer admission waits.
            account.handle_with_clock(&request, crate::identity::now_ms)
        } else {
            account.handle(&request, now)
        };
        Some((outcome, request))
    })
    .await
    .ok()
    .flatten();
    let Some((outcome, request)) = outcome else {
        return reply::error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Sign-in is unavailable right now. Try again shortly.",
            true,
        );
    };
    match outcome {
        Outcome::Reply(r) => {
            let status =
                StatusCode::from_u16(r.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            let mut res = reply::json_text(status, r.body, true);
            let h = res.headers_mut();
            for c in r.cookies {
                if let Ok(v) = HeaderValue::from_str(&c) {
                    h.append(header::SET_COOKIE, v);
                }
            }
            for (k, v) in r.headers {
                if let (Ok(k), Ok(v)) = (
                    header::HeaderName::from_bytes(k.as_bytes()),
                    HeaderValue::from_str(&v),
                ) {
                    h.insert(k, v);
                }
            }
            res
        }
        Outcome::Pass => {
            if request.body.over {
                return reply::error(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "Request exceeds size limit",
                    true,
                );
            }
            let req = Request::from_parts(parts, Body::from(request.body.bytes));
            app.proxy.forward(conn.peer, req).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Bytes;
    use server_store::{db_path, rusqlite::Connection};
    use std::{
        pin::Pin,
        sync::atomic::{AtomicBool, Ordering},
        task::{Context, Poll},
    };
    type Error = Box<dyn std::error::Error>;
    struct Pending(Arc<AtomicBool>);
    impl http_body::Body for Pending {
        type Data = Bytes;
        type Error = std::io::Error;
        fn poll_frame(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
        ) -> Poll<Option<Result<http_body::Frame<Bytes>, Self::Error>>> {
            self.0.store(true, Ordering::SeqCst);
            Poll::Pending
        }
    }
    #[tokio::test]
    async fn all_feature_namespaces_preserve_header_and_method_before_body_order(
    ) -> Result<(), Error> {
        let dir = tempfile::tempdir()?;
        let db = Connection::open(db_path(dir.path()))?;
        db.execute_batch(include_str!(
            "../../../crates/server-store/tests/fixtures/node-schema.sql"
        ))?;
        let now = crate::identity::now_ms();
        let uid = "00000000-0000-4000-8000-000000000001";
        db.execute("INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,created_at,updated_at) VALUES(?1,'Synthetic','synthetic','Synthetic','admin','unused','wa',?2,?2)",(uid,now))?;
        db.execute("INSERT INTO sessions(id_hash,user_id,csrf_hash,created_at,last_seen_at,expires_at,user_agent,ip) VALUES(?1,?2,?3,?4,?4,?5,'Synthetic','127.0.0.1')",(server_auth::js::digest("synthetic-session"),uid,server_auth::js::digest("synthetic-csrf"),now,now+60_000))?;
        let config = crate::config::Config::from_lookup(|key| match key {
            "NOEVIA_LEGACY_UPSTREAM" => Some("http://127.0.0.1:9".into()),
            "UI_DATA_DIR" => Some(dir.path().to_string_lossy().into_owned()),
            "NOEVIA_START_ID" => Some("11111111111111111111111111111111".into()),
            "PUBLIC_ORIGIN" => Some("https://noevia.example.test".into()),
            _ => None,
        })?;
        let app = App::new(config);
        for (method, path, signed, member, status) in [
            ("GET", "/api/features", false, false, 401),
            ("PUT", "/api/admin/features/previews", false, false, 401),
            ("GET", "/api/features", true, false, 503),
            ("POST", "/api/features", true, false, 405),
            ("PUT", "/api/admin/features", true, false, 405),
            ("GET", "/api/admin/features/previews", true, false, 405),
            ("GET", "/api/admin/features/%", true, false, 500),
            ("PUT", "/api/admin/features/previews", true, true, 403),
        ] {
            db.execute(
                "UPDATE users SET role=?1",
                [if member { "member" } else { "admin" }],
            )?;
            let polled = Arc::new(AtomicBool::new(false));
            let mut request = Request::builder()
                .method(method)
                .uri(path)
                .header("origin", "https://noevia.example.test");
            if signed {
                request = request
                    .header(
                        "cookie",
                        "cowork_session=synthetic-session; cowork_csrf=synthetic-csrf",
                    )
                    .header("x-csrf-token", "synthetic-csrf");
            }
            let response = tokio::time::timeout(
                std::time::Duration::from_secs(3),
                serve(
                    &app,
                    Conn {
                        peer: "127.0.0.1:12345".parse()?,
                        local: None,
                    },
                    request.body(Body::new(Pending(Arc::clone(&polled))))?,
                    path.into(),
                ),
            )
            .await?;
            assert_eq!(response.status().as_u16(), status, "{method} {path}");
            assert!(!polled.load(Ordering::SeqCst), "{method} {path}");
        }
        Ok(())
    }
}
