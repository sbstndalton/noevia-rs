//! The MCP credential routes (sbstndalton/noevia#2202), ported from core `routes/mcp-directory.cjs`.
//! Capability `rust-mcp-credentials`; core's `rust-mcp-credentials.cjs` refuses them with
//! `RUST_MCP_CREDENTIALS_OWNED`.
//!
//! Public routes (the session gate first: device tokens refused, CSRF checked on writes):
//!
//! - `GET    /api/mcp-keys/servers`          the servers whose key each person supplies
//! - `PUT    /api/mcp-keys/{id}` `{ headers }` this account's key, proven against the server first
//! - `DELETE /api/mcp-keys/{id}`
//! - `GET    /api/mcp-oauth/servers`         the sign-in servers and whether this account is connected
//! - `POST   /api/mcp-oauth/{id}/connect`    begin this account's sign-in -> `{ signIn }`
//! - `DELETE /api/mcp-oauth/{id}`            this account's sign-in is dropped (and revoked)
//! - `GET    /api/mcp-oauth/callback`        the service's return address (an HTML page)
//!
//! An id is `[a-z0-9-]+`, as core matched it; any other shape is Node's (its 404).
//!
//! Internal operations for Node's chat and admin routes (`POST /v1/mcp/<op>` on the store listener,
//! behind the store secret, loopback only):
//!
//! - `session { user, server, rejected? }` -> `{ accessToken, expiresAt, generation }` or 401/503
//!   `{ error, code, status }` (`disconnected`, `reconnect`, `transient`)
//! - `key { user, server }` -> `{ headers }`: the account's own key headers for ONE call
//! - `reject { user, server, generation }` -> `{ marked }`
//! - `start { user, server, serverUrl, challenge?, purpose? }` -> `{ signIn }`
//! - `client { server, serverUrl, clientId, clientSecret?, challenge? }` -> `{ ok }`; `client-info
//!   { server }` -> `{ client }`
//! - `forget { server }` or `forget { user }` -> `{ ok }`; `key-set { user, server, headers }` -> `{ ok }`
//! - `reseal {}` -> `{ ok, tables, failures }` (secrets rotation)
use crate::mcp_broker::{auth_error, Fail};
use crate::{identity, reply, App};
use axum::body::Body;
use axum::http::{Request, Response, StatusCode};
use axum::response::IntoResponse;
use http_body_util::BodyExt;
use js_json::JValue;
use std::sync::Arc;
use std::time::Duration;

pub const KEYS_SERVERS: &str = "/api/mcp-keys/servers";
pub const KEY: &str = "/api/mcp-keys/{*}";
pub const OAUTH_SERVERS: &str = "/api/mcp-oauth/servers";
pub const OAUTH_ONE: &str = "/api/mcp-oauth/{id}";
pub const OAUTH_CONNECT: &str = "/api/mcp-oauth/{id}/connect";
pub const OAUTH_CALLBACK: &str = "/api/mcp-oauth/callback";
/// The routes this module answers (routes.toml gives Rust exactly these methods).
pub const ROUTES: &[&str] = &[
    KEYS_SERVERS,
    KEY,
    OAUTH_SERVERS,
    OAUTH_ONE,
    OAUTH_CONNECT,
    OAUTH_CALLBACK,
];

const BODY_LIMIT: usize = 1024 * 1024;
const BODY_DEADLINE: Duration = Duration::from_secs(10);

/// core's id shape in these routes: `[a-z0-9-]+`.
fn id_ok(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

enum Op {
    KeysList,
    KeyPut(String),
    KeyDelete(String),
    OauthList,
    Connect(String),
    Drop(String),
    Callback,
}

/// What `method path` is, or `None` (Node's).
fn op_for(method: &str, path: &str) -> Option<Op> {
    // No trailing slash is tolerated: core matched these paths with anchored regexes.
    if let Some(rest) = path.strip_prefix("/api/mcp-keys/") {
        return match (method, rest) {
            ("GET", "servers") => Some(Op::KeysList),
            ("PUT", id) if id_ok(id) => Some(Op::KeyPut(id.to_owned())),
            ("DELETE", id) if id_ok(id) => Some(Op::KeyDelete(id.to_owned())),
            _ => None,
        };
    }
    let rest = path.strip_prefix("/api/mcp-oauth/")?;
    match (method, rest) {
        ("GET", "servers") => Some(Op::OauthList),
        ("GET", "callback") => Some(Op::Callback),
        ("POST", r) => r
            .strip_suffix("/connect")
            .filter(|id| id_ok(id))
            .map(|id| Op::Connect(id.to_owned())),
        ("DELETE", id) if id_ok(id) => Some(Op::Drop(id.to_owned())),
        _ => None,
    }
}

pub(crate) fn answer(status: u16, body: &JValue) -> Response<Body> {
    reply::json_text(
        StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
        js_json::stringify(body).unwrap_or_else(|| "{}".into()),
        true,
    )
}

pub(crate) fn error(status: u16, message: &str) -> Response<Body> {
    answer(
        status,
        &JValue::obj([("error", JValue::Str(message.into()))]),
    )
}

/// A failure as core's routes answered it: `e.status || 502`, `{ error }`.
pub(crate) fn failed(e: &Fail) -> Response<Body> {
    let status = if (400..600).contains(&e.status) {
        e.status
    } else {
        502
    };
    error(status, &e.message)
}

/// core http.cjs `readJson` + `readObject`: an empty body is `{}`; anything that is not a JSON
/// object is `Err` (the 400 `invalid JSON`).
pub(crate) async fn read_object(mut body: Body) -> Result<JValue, ()> {
    let read = async {
        let mut out = Vec::new();
        while let Some(frame) = body.frame().await {
            let Ok(frame) = frame else { return None };
            if let Ok(data) = frame.into_data() {
                if out.len() + data.len() > BODY_LIMIT {
                    return None;
                }
                out.extend_from_slice(&data);
            }
        }
        Some(out)
    };
    match tokio::time::timeout(BODY_DEADLINE, read).await {
        Ok(Some(bytes)) if bytes.is_empty() => Ok(JValue::Obj(Vec::new())),
        Ok(Some(bytes)) => match js_json::parse(&String::from_utf8_lossy(&bytes)) {
            Ok(v @ JValue::Obj(_)) => Ok(v),
            _ => Err(()),
        },
        _ => Err(()),
    }
}

pub async fn serve(
    app: &Arc<App>,
    conn: crate::serve::Conn,
    req: Request<Body>,
    path: String,
) -> Response<Body> {
    let Some(mcp) = app.mcp.as_ref().map(Arc::clone) else {
        return app.proxy.forward(conn.peer, req).await;
    };
    let Some(op) = op_for(req.method().as_str(), &path) else {
        return app.proxy.forward(conn.peer, req).await;
    };
    let query = req.uri().query().unwrap_or("").to_owned();
    let (parts, body) = req.into_parts();
    let creds = identity::creds_of(&parts);
    let gate = Arc::clone(&app.identity);
    let now = identity::now_ms();
    let gate_path = path.clone();
    let id = match tokio::task::spawn_blocking(move || gate.gate_blocking(&creds, &gate_path, now))
        .await
    {
        Ok(Ok(id)) => id,
        Ok(Err(rejection)) => return rejection.into_response(),
        Err(_) => return identity::Rejection::Unavailable.into_response(),
    };
    let user = id.user.id.clone();
    mcp.sweep_once(app).await;
    match op {
        Op::KeysList => match mcp.key_servers(app, &user).await {
            Ok(v) => answer(200, &v),
            Err(e) => failed(&e),
        },
        Op::OauthList => match mcp.oauth_servers(app, &user).await {
            Ok(v) => answer(200, &v),
            Err(e) => failed(&e),
        },
        Op::KeyPut(server_id) => {
            let server = match mcp.find_server(app, &server_id).await {
                Ok(Some(s)) if s.personal => s,
                Ok(_) => return error(404, "No such server."),
                Err(e) => return failed(&e),
            };
            let Ok(body) = read_object(body).await else {
                return error(400, "invalid JSON");
            };
            let headers = match body.get("headers") {
                h if h.truthy() => h.clone(),
                _ => JValue::Obj(Vec::new()),
            };
            match mcp.put_key(app, &user, &server, &headers).await {
                Ok(()) => answer(200, &JValue::obj([("ok", JValue::Bool(true))])),
                Err(e) => failed(&e),
            }
        }
        Op::KeyDelete(server_id) => match mcp.delete_key(app, &user, &server_id).await {
            Ok(()) => answer(200, &JValue::obj([("ok", JValue::Bool(true))])),
            Err(e) => failed(&e),
        },
        Op::Connect(server_id) => {
            let server = match mcp.find_server(app, &server_id).await {
                Ok(Some(s)) if s.oauth => s,
                Ok(_) => return error(404, "No such sign-in server."),
                Err(e) => return failed(&e),
            };
            let (_, challenge) = mcp.egress().probe_auth(&server.url).await;
            match mcp
                .start(app, &user, &server.id, &server.url, &challenge, "connect")
                .await
            {
                Ok(sign_in) => answer(200, &JValue::obj([("signIn", JValue::Str(sign_in))])),
                Err(e) if e.needs_client => error(
                    e.status,
                    "An administrator has to finish setting this server up before anyone can sign in.",
                ),
                Err(e) => failed(&e),
            }
        }
        Op::Drop(server_id) => match mcp.disconnect(app, &user, &server_id).await {
            Ok(()) => answer(200, &JValue::obj([("ok", JValue::Bool(true))])),
            Err(e) => failed(&e),
        },
        Op::Callback => callback(app, &mcp, &user, &query).await,
    }
}

/// The page the browser lands on: core's markup, headers and wording.
fn page(ok: bool, text: &str) -> Response<Body> {
    let escaped: String = text
        .chars()
        .map(|c| match c {
            '&' => "&amp;".to_owned(),
            '<' => "&lt;".to_owned(),
            '>' => "&gt;".to_owned(),
            '"' => "&quot;".to_owned(),
            c => c.to_string(),
        })
        .collect();
    let html = format!(
        "<!doctype html><meta name=viewport content=\"width=device-width\"><title>noevia sign-in</title><body style=\"font:16px system-ui;padding:32px;max-width:32em\"><h1 style=\"font-size:20px\">{}</h1><p>{escaped}</p><p>You can close this tab and return to noevia.</p><script>setTimeout(()=>{{try{{window.close()}}catch{{}}}},1200)</script>",
        if ok { "Signed in" } else { "Sign-in did not finish" }
    );
    let mut res = Response::new(Body::from(html));
    *res.status_mut() = if ok {
        StatusCode::OK
    } else {
        StatusCode::BAD_REQUEST
    };
    let h = res.headers_mut();
    reply::security_headers(h, true);
    h.insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("text/html; charset=utf-8"),
    );
    h.insert(
        axum::http::header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store"),
    );
    h.insert(
        axum::http::header::CONTENT_SECURITY_POLICY,
        axum::http::HeaderValue::from_static(
            "default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline'",
        ),
    );
    res
}

async fn callback(
    app: &Arc<App>,
    mcp: &Arc<crate::mcp_broker::Mcp>,
    user: &str,
    query: &str,
) -> Response<Body> {
    let param = |name: &str| {
        url::form_urlencoded::parse(query.as_bytes())
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.into_owned())
    };
    if let Some(err) = param("error").filter(|e| !e.is_empty()) {
        let said = param("error_description")
            .filter(|d| !d.is_empty())
            .unwrap_or(err);
        return page(
            false,
            &format!(
                "The sign-in service said: {}",
                js_json::js_slice(&said, 200)
            ),
        );
    }
    let state = param("state").unwrap_or_default();
    let code = param("code").unwrap_or_default();
    let iss = param("iss");
    match mcp.finish(app, user, &state, &code, iss.as_deref()).await {
        Ok(done) => page(
            true,
            if done.purpose == "add" {
                "The server is added. Choose it under a project’s Tools; each person signs in from Plugins → Connected."
            } else {
                "Your account is connected. Its tools are now offered in projects that chose this server."
            },
        ),
        Err(e) => page(false, &e.message),
    }
}

// ---------------------------------------------------------------------------------------------
// The internal operations (project_store.rs routes them after its secret check).

fn internal(status: u16, body: &JValue) -> Response<Body> {
    let mut res = Response::new(Body::from(js_json::stringify(body).unwrap_or_default()));
    *res.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    res.headers_mut().insert(
        "content-type",
        axum::http::HeaderValue::from_static("application/json"),
    );
    res.headers_mut().insert(
        "cache-control",
        axum::http::HeaderValue::from_static("no-store"),
    );
    res
}

/// A refusal that is not a credential failure (a malformed call, an unknown operation). Like
/// [`internal_fail`] it carries its own `status`: Node's client treats an answer without one as
/// "unusable, try again" (503), which would hide a 400 or 404 as an outage.
fn internal_error(status: u16, message: &str) -> Response<Body> {
    internal(
        status,
        &JValue::obj([
            ("error", JValue::Str(message.into())),
            ("status", JValue::Num(f64::from(status))),
        ]),
    )
}

fn internal_fail(e: &Fail) -> Response<Body> {
    let mut pairs = vec![("error", JValue::Str(e.message.clone()))];
    if let Some(code) = e.code {
        pairs.push(("code", JValue::Str(code.into())));
    }
    if e.needs_client {
        pairs.push(("needsClient", JValue::Bool(true)));
    }
    if let Some(issuer) = &e.issuer {
        pairs.push(("issuer", JValue::Str(issuer.clone())));
    }
    pairs.push(("status", JValue::Num(f64::from(e.status))));
    let status = if (400..600).contains(&e.status) {
        e.status
    } else {
        502
    };
    internal(status, &JValue::obj(pairs))
}

fn word(body: &JValue, key: &str, max: usize) -> Option<String> {
    match body.get(key) {
        JValue::Str(s) if !s.is_empty() && s.len() <= max => Some(s.clone()),
        _ => None,
    }
}

/// `POST /v1/mcp/<op>` after the store secret was checked.
pub async fn internal_op(app: Option<Arc<App>>, op: &str, raw: &[u8]) -> Response<Body> {
    let Some(app) = app else {
        return internal_error(503, "unavailable");
    };
    let Some(mcp) = app.mcp.as_ref().map(Arc::clone) else {
        return internal_error(503, "unavailable");
    };
    let body = match js_json::parse(&String::from_utf8_lossy(raw)) {
        Ok(b @ JValue::Obj(_)) => b,
        _ => return internal_error(400, "invalid JSON"),
    };
    mcp.sweep_once(&app).await;
    let user = word(&body, "user", 256);
    let server = word(&body, "server", 256);
    let ok = || internal(200, &JValue::obj([("ok", JValue::Bool(true))]));
    match op {
        "session" | "key" | "reject" | "start" | "key-set" => {}
        "client" | "client-info" | "forget" | "reseal" => {}
        _ => return internal_error(404, "not found"),
    }
    match op {
        "session" => {
            let (Some(user), Some(server)) = (user, server) else {
                return internal_error(400, "invalid user or server");
            };
            let rejected = body.get("rejected").as_str().map(str::to_owned);
            match mcp.session(&app, &user, &server, rejected.as_deref()).await {
                Ok(t) => internal(
                    200,
                    &JValue::obj([
                        ("accessToken", JValue::Str(t.access.to_string())),
                        (
                            "expiresAt",
                            t.expires_at.map(JValue::Num).unwrap_or(JValue::Null),
                        ),
                        ("generation", JValue::Str(t.generation)),
                    ]),
                ),
                Err(e) => internal_fail(&e),
            }
        }
        "key" => {
            let (Some(user), Some(server)) = (user, server) else {
                return internal_error(400, "invalid user or server");
            };
            match mcp.key_headers(&app, &user, &server).await {
                Ok(h) => internal(
                    200,
                    &JValue::obj([(
                        "headers",
                        JValue::obj(h.into_iter().map(|(k, v)| (k, JValue::Str(v)))),
                    )]),
                ),
                Err(e) => internal_fail(&e),
            }
        }
        "reject" => {
            let (Some(user), Some(server)) = (user, server) else {
                return internal_error(400, "invalid user or server");
            };
            let generation = body.get("generation").as_str().unwrap_or("").to_owned();
            let marked = mcp.reject(&app, &user, &server, &generation).await;
            internal(200, &JValue::obj([("marked", JValue::Bool(marked))]))
        }
        "start" => {
            let (Some(user), Some(server), Some(url)) =
                (user, server, word(&body, "serverUrl", 4096))
            else {
                return internal_error(400, "invalid user, server or serverUrl");
            };
            // The address is the directory's own, not whatever a caller names.
            match mcp.find_server(&app, &server).await {
                Ok(Some(s)) if s.oauth && s.url == url => {}
                Ok(_) => return internal_error(404, "No such sign-in server."),
                Err(e) => return internal_fail(&e),
            }
            let challenge = body.get("challenge").as_str().unwrap_or("").to_owned();
            let purpose = if body.get("purpose").as_str() == Some("add") {
                "add"
            } else {
                "connect"
            };
            match mcp
                .start(&app, &user, &server, &url, &challenge, purpose)
                .await
            {
                Ok(sign_in) => internal(200, &JValue::obj([("signIn", JValue::Str(sign_in))])),
                Err(e) => internal_fail(&e),
            }
        }
        "client" => {
            let (Some(server), Some(url)) = (server, word(&body, "serverUrl", 4096)) else {
                return internal_error(400, "invalid server or serverUrl");
            };
            match mcp.find_server(&app, &server).await {
                Ok(Some(s)) if s.oauth && s.url == url => {}
                Ok(_) => return internal_error(404, "No such sign-in server."),
                Err(e) => return internal_fail(&e),
            }
            let text = |k: &str| match body.get(k) {
                JValue::Str(s) => s.clone(),
                _ => String::new(),
            };
            match mcp
                .set_client(
                    &app,
                    &server,
                    &url,
                    &text("clientId"),
                    &text("clientSecret"),
                    &text("challenge"),
                )
                .await
            {
                Ok(()) => ok(),
                Err(e) => internal_fail(&e),
            }
        }
        "client-info" => {
            let Some(server) = server else {
                return internal_error(400, "invalid server");
            };
            match mcp.client_info(&app, &server).await {
                Ok(v) => internal(200, &JValue::obj([("client", v)])),
                Err(e) => internal_fail(&e),
            }
        }
        "forget" => match (server, user) {
            (Some(server), None) => match mcp.forget_server(&app, &server).await {
                Ok(()) => ok(),
                Err(e) => internal_fail(&e),
            },
            (None, Some(user)) => match mcp.forget_user(&app, &user).await {
                Ok(()) => ok(),
                Err(e) => internal_fail(&e),
            },
            _ => internal_error(400, "forget needs a server or a user"),
        },
        "key-set" => {
            let (Some(user), Some(server)) = (user, server) else {
                return internal_error(400, "invalid user or server");
            };
            // The adding administrator's key for a server they just added as personal. Node has
            // already checked the values against the declared headers; they are one-line strings.
            let JValue::Obj(items) = body.get("headers") else {
                return internal_error(400, "headers must be an object");
            };
            let mut headers = Vec::new();
            for (k, v) in items {
                match v.as_str() {
                    Some(s)
                        if !s.is_empty() && s.len() <= 4096 && !s.contains(['\r', '\n', '\0']) =>
                    {
                        headers.push((k.clone(), s.to_owned()));
                    }
                    _ => return internal_error(400, "every header must be one short line"),
                }
            }
            if headers.is_empty() || headers.len() > 32 {
                return internal_error(400, "enter the key");
            }
            match mcp.find_server(&app, &server).await {
                Ok(Some(s)) if s.personal => {}
                Ok(_) => return internal_error(404, "No such server."),
                Err(e) => return internal_fail(&e),
            }
            let (m, a) = (Arc::clone(&mcp), Arc::clone(&app));
            let done = tokio::task::spawn_blocking(move || {
                m.store_key(&a, &user, &server, &headers)?;
                m.publish(&a);
                Ok::<(), Fail>(())
            })
            .await;
            match done {
                Ok(Ok(())) => ok(),
                Ok(Err(e)) => internal_fail(&e),
                Err(_) => internal_fail(&auth_error("transient")),
            }
        }
        _ => match mcp.reseal(&app).await {
            Ok(report) => internal(200, &report.json()),
            Err(e) => internal_fail(&e),
        },
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn op(method: &str, path: &str) -> &'static str {
        match op_for(method, path) {
            None => "node",
            Some(Op::KeysList) => "keys-list",
            Some(Op::KeyPut(_)) => "key-put",
            Some(Op::KeyDelete(_)) => "key-delete",
            Some(Op::OauthList) => "oauth-list",
            Some(Op::Connect(_)) => "connect",
            Some(Op::Drop(_)) => "drop",
            Some(Op::Callback) => "callback",
        }
    }

    #[test]
    fn paths_are_core_regexes() {
        assert_eq!(op("GET", "/api/mcp-keys/servers"), "keys-list");
        assert_eq!(op("PUT", "/api/mcp-keys/dir-notes"), "key-put");
        assert_eq!(op("DELETE", "/api/mcp-keys/dir-notes"), "key-delete");
        // `servers` is also an id for the writes, as in core.
        assert_eq!(op("PUT", "/api/mcp-keys/servers"), "key-put");
        assert_eq!(op("GET", "/api/mcp-oauth/servers"), "oauth-list");
        assert_eq!(op("POST", "/api/mcp-oauth/dir-x/connect"), "connect");
        assert_eq!(op("DELETE", "/api/mcp-oauth/dir-x"), "drop");
        assert_eq!(op("GET", "/api/mcp-oauth/callback"), "callback");
        // Anything else is Node's: another method, an id out of `[a-z0-9-]+`, a longer path.
        for (m, p) in [
            ("POST", "/api/mcp-keys/servers"),
            ("GET", "/api/mcp-keys/dir-x"),
            ("PUT", "/api/mcp-keys/Dir-X"),
            ("PUT", "/api/mcp-keys/dir_x"),
            ("PUT", "/api/mcp-keys/a/b"),
            ("POST", "/api/mcp-oauth/Dir/connect"),
            ("GET", "/api/mcp-oauth/dir-x/connect"),
            ("POST", "/api/mcp-oauth/a/b/connect"),
            ("DELETE", "/api/mcp-oauth/a/b"),
            ("POST", "/api/mcp-oauth/callback"),
        ] {
            assert_eq!(op(m, p), "node", "{m} {p}");
        }
    }

    #[tokio::test]
    async fn the_page_escapes_what_a_service_said() {
        let page = page(false, "<script>alert(\"x\")</script> & more");
        assert_eq!(page.status(), 400);
        let body = String::from_utf8(
            page.into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes()
                .to_vec(),
        )
        .unwrap();
        // What the service said is text on the page, never markup: no script of its own, only the
        // page's own closing script, which is the one `setTimeout` line.
        assert!(
            body.contains("&lt;script&gt;alert(&quot;x&quot;)&lt;/script&gt; &amp; more"),
            "{body}"
        );
        assert!(!body.contains("<script>alert"), "{body}");
        assert_eq!(body.matches("<script>").count(), 1, "{body}");
    }
}
