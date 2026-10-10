//! M3 (NOEVIA_RUST_AUTH=1): the front answers sign-in and the account routes itself
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
            app.writes.switch()?,
        );
        let writer = app.writes.writer().ok()?;
        let a = Arc::new(Account::new(
            writer,
            Arc::new(Authenticator::new(auth)),
            settings,
            switch,
            now,
        ));
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
    let Ok(body) = read_body(body, BODY_LIMIT).await else {
        return reply::error(StatusCode::BAD_REQUEST, "invalid request body", true);
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
        Some((account.handle(&request, now), request))
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
