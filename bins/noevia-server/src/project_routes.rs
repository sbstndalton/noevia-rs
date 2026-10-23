//! M4 (permanent since sbstndalton/noevia#1071; NOEVIA_RUST_PROJECTS is retired): the front answers a project's image
//! routes (crates/server-projects `assets`), its chat list (`chats`) and the free-chat list
//! (`free_chats`) itself instead of proxying them. The request passes
//! Node's gate first (server-auth: session, origin + CSRF for writes, device-token limits), here
//! on the WHATWG pathname Node routes on; the user it returns is the only one whose files are
//! touched. A path the routes.toml pattern matches but Node's own route regex would not (a
//! trailing slash, an empty segment) still goes to Node, as before.
//!
//! A write passes the gate again right before it touches a file (sbstndalton/noevia#1425): the
//! body is read between the two, for as long as the client takes, and an account removed (or a
//! session revoked) meanwhile must not write. The gate only reads the store, so running it twice
//! changes nothing. The store underneath never recreates a removed user's directory either; only the
//! first gate, which has just confirmed the session, makes a missing one, as Node's workspace load
//! did (#2026).

use crate::identity::{self, Rejection};
use crate::reply;
use crate::App;
use axum::body::Body;
use axum::http::{HeaderName, HeaderValue, Request, Response, StatusCode};
use axum::response::IntoResponse;
use server_projects::{assets, chats, context, context_window, free_chats, retention, Ctx, Reply};
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// POST /api/projects/{id}/assets
    Upload { project: String },
    /// GET or DELETE /api/projects/{id}/assets/{assetId}
    One { project: String, asset: String },
    /// GET /api/projects/{id}/chats
    Chats { project: String },
    /// POST /api/projects/{id}/chats
    SaveChats { project: String },
    /// GET /api/freechats
    FreeChats,
    /// POST /api/freechats
    SaveFreeChats,
    /// GET (`create: false`) or POST /api/chats/{chatId}/context (#1264, permanently native under #1071)
    Context { chat: String, create: bool },
    /// GET /api/chats/{chatId}/context-window (sbstndalton/noevia#1071, no switch)
    ContextWindow { chat: String },
    /// GET /api/account/retention (sbstndalton/noevia#1071, no switch; `PUT` is Node's)
    Retention,
}

/// Node's matchers, `^\/api\/projects\/([^/]+)\/assets$`, `...\/assets\/([^/]+)$` and
/// `^\/api\/projects\/([^/]+)\/chats$`, and routes/chat-lists.cjs's `p === '/api/freechats'`,
/// with the methods its handlers answer; `None` sends the request on to Node.
pub fn route(method: &str, path: &str) -> Option<Route> {
    // routes/account.cjs `path === '/api/account/retention'`, its GET only.
    if path == "/api/account/retention" {
        return (method == "GET").then_some(Route::Retention);
    }
    if path == "/api/freechats" {
        return match method {
            "GET" => Some(Route::FreeChats),
            "POST" => Some(Route::SaveFreeChats),
            _ => None,
        };
    }
    // routes/projects.cjs `/^\/api\/chats\/([^/]+)\/context$/` for GET and POST.
    if let Some(rest) = path.strip_prefix("/api/chats/") {
        return match (method, rest.split('/').collect::<Vec<_>>().as_slice()) {
            ("GET" | "POST", [chat, "context"]) if !chat.is_empty() => Some(Route::Context {
                chat: (*chat).to_string(),
                create: method == "POST",
            }),
            // routes/chat-lists.cjs `/^\/api\/chats\/([^/]+)\/context-window$/` for GET.
            ("GET", [chat, "context-window"]) if !chat.is_empty() => Some(Route::ContextWindow {
                chat: (*chat).to_string(),
            }),
            _ => None,
        };
    }
    let rest = path.strip_prefix("/api/projects/")?;
    let parts: Vec<&str> = rest.split('/').collect();
    match (method, parts.as_slice()) {
        ("POST", [project, "assets"]) if !project.is_empty() => Some(Route::Upload {
            project: (*project).to_string(),
        }),
        ("GET" | "DELETE", [project, "assets", asset])
            if !project.is_empty() && !asset.is_empty() =>
        {
            Some(Route::One {
                project: (*project).to_string(),
                asset: (*asset).to_string(),
            })
        }
        ("GET", [project, "chats"]) if !project.is_empty() => Some(Route::Chats {
            project: (*project).to_string(),
        }),
        ("POST", [project, "chats"]) if !project.is_empty() => Some(Route::SaveChats {
            project: (*project).to_string(),
        }),
        _ => None,
    }
}

fn respond(r: Reply) -> Response<Body> {
    let status = StatusCode::from_u16(r.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    match r.body {
        server_projects::Body::Json(text) => reply::json_text(status, text, true),
        server_projects::Body::Bytes { headers, bytes } => {
            let mut res = Response::new(Body::from(bytes));
            *res.status_mut() = status;
            let h = res.headers_mut();
            reply::security_headers(h, true);
            for (k, v) in headers {
                // The route's own headers win, as in Node's writeHead (its sandbox CSP).
                let (Ok(k), Ok(v)) = (
                    HeaderName::from_bytes(k.as_bytes()),
                    HeaderValue::from_str(&v),
                ) else {
                    return reply::error(StatusCode::INTERNAL_SERVER_ERROR, "Internal error", true);
                };
                h.insert(k, v);
            }
            res
        }
    }
}

/// One request for a Rust-owned project route; `path` is the WHATWG pathname.
pub async fn serve(
    app: &Arc<App>,
    conn: crate::serve::Conn,
    req: Request<Body>,
    path: String,
) -> Response<Body> {
    let (parts, body) = req.into_parts();
    let Some(route) = route(parts.method.as_str(), &path) else {
        return app
            .proxy
            .forward(conn.peer, Request::from_parts(parts, body))
            .await;
    };
    let switch = app.config.rust_projects;
    let Some(data_dir) = app.config.data_dir.clone() else {
        return Rejection::Unavailable.into_response();
    };
    let creds = identity::creds_of(&parts);
    let layer = Arc::clone(&app.identity);
    let now = identity::now_ms();
    let gate_path = path.clone();
    let (regate_layer, regate_creds) = (Arc::clone(&layer), creds.clone());
    let authn =
        match tokio::task::spawn_blocking(move || layer.gate_blocking(&creds, &gate_path, now))
            .await
        {
            Ok(Ok(id)) => id,
            Ok(Err(rejection)) => return rejection.into_response(),
            Err(_) => return Rejection::Unavailable.into_response(),
        };
    // Node's workspace load makes the directory of every signed-in user on that user's first
    // request, whichever route it is (core index.cjs handleRequestInner). The stores here never
    // do, a missing directory there being a removed account (#1425), so a new account's first
    // request needs it made, once, right after the gate has confirmed the session and before
    // the body is read. Node refuses a removed account in process (`removed`); here the account
    // may be removed between the gate and the mkdir, so the gate runs once more after it, and a
    // directory this request made for an account that is gone is taken away again. Only the
    // gate's own user id reaches the path, as `users/<uuid>` (#2026).
    let step = {
        let (data_dir, user) = (data_dir.clone(), authn.user.id.clone());
        let (layer, creds, path) = (
            Arc::clone(&regate_layer),
            regate_creds.clone(),
            path.clone(),
        );
        let (auth_writes, expected_id, expected_admin) = (
            Arc::clone(&app.writes),
            authn.user.id.clone(),
            authn.account_role == server_auth::Role::Admin,
        );
        tokio::task::spawn_blocking(move || {
            #[cfg(debug_assertions)]
            hooks::before_ensure(&user);
            let made = match server_projects::store::ensure_user_dir(&data_dir, &user) {
                Ok(server_projects::store::Ensured::Created(dir)) => Some(dir),
                Ok(server_projects::store::Ensured::Existed(_)) => None,
                Ok(server_projects::store::Ensured::LegacyPending) => {
                    if !expected_admin
                        || claim_legacy_in_transaction(
                            &auth_writes,
                            &layer,
                            &creds,
                            &path,
                            now,
                            &data_dir,
                            &expected_id,
                        )
                        .is_err()
                    {
                        return Step::SettingUp;
                    }
                    match server_projects::store::ensure_user_dir(&data_dir, &user) {
                        Ok(server_projects::store::Ensured::Created(dir)) => Some(dir),
                        Ok(server_projects::store::Ensured::Existed(_)) => None,
                        Ok(server_projects::store::Ensured::LegacyPending) | Err(_) => {
                            return Step::SettingUp
                        }
                    }
                }
                Err(_) => return Step::Failed,
            };
            match regate(&layer, &creds, &path, now, &data_dir, &user) {
                Ok(()) => Step::Go,
                Err(refused) => {
                    let (undo, refused) = after_second_gate(made.is_some(), refused);
                    if let (true, Some(dir)) = (undo, made) {
                        // Not recursive: a directory this call has just made holds nothing.
                        let _ = std::fs::remove_dir(&dir);
                    }
                    Step::Refused(refused)
                }
            }
        })
        .await
    };
    match step {
        Ok(Step::Go) => {}
        Ok(Step::SettingUp) => {
            return reply::error(
                StatusCode::SERVICE_UNAVAILABLE,
                "Your account is still being set up; try again in a moment.",
                true,
            )
        }
        Ok(Step::Refused(refused)) => return refusal(refused),
        Ok(Step::Failed) | Err(_) => {
            return reply::error(StatusCode::INTERNAL_SERVER_ERROR, "Internal error", true)
        }
    }
    let limit = match &route {
        Route::Upload { .. } => Some(assets::BODY_LIMIT),
        Route::SaveChats { .. } | Route::SaveFreeChats => Some(chats::BODY_LIMIT),
        Route::One { .. }
        | Route::Chats { .. }
        | Route::FreeChats
        | Route::Context { .. }
        | Route::ContextWindow { .. }
        | Route::Retention => None,
    };
    let upload = match limit {
        Some(limit) => match crate::account_routes::read_body(body, limit).await {
            Ok(b) => Some(b),
            Err(()) => return reply::error(StatusCode::BAD_REQUEST, "invalid request body", true),
        },
        None => None,
    };
    let method = parts.method.clone();
    // routes/projects.cjs: `authn?.user?.role === 'admin'` (a device token acts as a member).
    let admin = authn.user.role == server_auth::Role::Admin;
    let user = authn.user.id;
    // The context GET writes too (getProject's self-heal), so it is re-gated as well.
    let writes = matches!(
        route,
        Route::Upload { .. }
            | Route::SaveChats { .. }
            | Route::SaveFreeChats
            | Route::Context { .. }
    ) || (matches!(route, Route::One { .. }) && method == "DELETE");
    let done = tokio::task::spawn_blocking(move || {
        if writes {
            regate(&regate_layer, &regate_creds, &path, now, &data_dir, &user)?;
        }
        let ctx = Ctx {
            data_dir: &data_dir,
            user_id: &user,
            switch,
            now_ms: now,
        };
        Ok(match (route, upload) {
            (Route::Upload { project }, Some(b)) => assets::post(ctx, &project, &b.bytes, b.over),
            (Route::One { project, asset }, _) if method == "DELETE" => {
                assets::delete(ctx, &project, &asset)
            }
            (Route::One { project, asset }, _) => assets::get(ctx, &project, &asset),
            (Route::Chats { project }, _) => chats::get(ctx, &project),
            (Route::SaveChats { project }, Some(b)) => {
                chats::post(ctx, &project, &b.bytes, b.over, admin)
            }
            (Route::FreeChats, _) => free_chats::get(ctx),
            (Route::SaveFreeChats, Some(b)) => free_chats::post(ctx, &b.bytes, b.over, admin),
            (Route::Context { chat, create: true }, _) => context::post(ctx, &chat),
            (
                Route::Context {
                    chat,
                    create: false,
                },
                _,
            ) => context::get(ctx, &chat),
            (Route::ContextWindow { chat }, _) => context_window::get(ctx, &chat),
            (Route::Retention, _) => retention::get(ctx),
            (Route::Upload { .. } | Route::SaveChats { .. } | Route::SaveFreeChats, None) => {
                Reply::internal()
            }
        })
    })
    .await;
    match done {
        Ok(Ok(r)) => respond(r),
        Ok(Err(refused)) => refusal(refused),
        Err(_) => reply::error(StatusCode::INTERNAL_SERVER_ERROR, "Internal error", true),
    }
}

/// The answer to a request the gate refused on a later pass.
fn refusal(refused: Refused) -> Response<Body> {
    match refused {
        Refused::Removed => reply::json(
            StatusCode::GONE,
            &serde_json::json!({"error": "account no longer exists"}),
            true,
        ),
        Refused::Gate(rejection) => rejection.into_response(),
    }
}

/// Claims the legacy root only while the same live administrator is serialized against
/// Node's deleteUser transaction by the shared auth database's IMMEDIATE writer lock.
fn claim_legacy_in_transaction(
    writes: &crate::upkeep::WriterLayer,
    identity: &crate::identity::IdentityLayer,
    creds: &server_auth::Creds,
    path: &str,
    now_ms: i64,
    data_dir: &std::path::Path,
    expected_id: &str,
) -> Result<bool, server_store::StoreError> {
    let auth = identity
        .authenticator()
        .ok_or_else(|| server_store::StoreError::NotReady("auth unavailable".into()))?;
    let writer = writes.writer()?;
    writer.write(|tx| {
        let reader = tx.reader();
        let live = match auth.gate_in(&reader, creds, path, now_ms)? {
            Ok(user)
                if user.user.id == expected_id && user.account_role == server_auth::Role::Admin =>
            {
                user
            }
            _ => return Ok(false),
        };
        let _same_account = live;
        server_projects::legacy_claim::claim_root(data_dir, expected_id).map_err(|e| {
            server_store::StoreError::NotReady(format!("legacy claim unavailable: {e:?}"))
        })
    })
}

/// How the first pass after the first gate ended.
enum Step {
    Go,
    /// Node's legacy claim for this user is still to come: nothing is served yet (503).
    SettingUp,
    Refused(Refused),
    Failed,
}

/// What the second gate's refusal means for the directory this request just made. Only a session
/// the gate no longer knows (Unauthorized: the account was removed) takes it away, and that is the
/// removed account's 410; a transient refusal (the gate unavailable, a rate limit) leaves it, since a
/// concurrent request of the same user may already have seen it as existing and be using it.
/// Returns whether to remove the directory, and the answer.
fn after_second_gate(created: bool, refused: Refused) -> (bool, Refused) {
    match refused {
        Refused::Gate(Rejection::Refused(server_auth::Refusal::Unauthorized)) if created => {
            (true, Refused::Removed)
        }
        other => (false, other),
    }
}

/// Test hooks, compiled in debug builds only (`cargo test` and the CI replays); a release build has
/// neither the module nor the lock. (A cargo feature turned on by a dev-dependency of the package
/// on itself would also have changed which features `--features noevia-server/...` builds select.)
#[cfg(debug_assertions)]
#[doc(hidden)]
pub mod hooks {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    type Hook = Arc<dyn Fn() + Send + Sync>;
    static BEFORE_ENSURE: Mutex<Option<HashMap<String, Hook>>> = Mutex::new(None);

    /// Runs `hook` when this user's request has passed the first gate and not yet made the
    /// user's directory (a test removes the account there).
    pub fn on_before_ensure(user: &str, hook: impl Fn() + Send + Sync + 'static) {
        if let Ok(mut map) = BEFORE_ENSURE.lock() {
            map.get_or_insert_with(HashMap::new)
                .insert(user.to_string(), Arc::new(hook));
        }
    }

    pub(crate) fn before_ensure(user: &str) {
        let hook = BEFORE_ENSURE
            .lock()
            .ok()
            .and_then(|map| map.as_ref().and_then(|m| m.get(user).cloned()));
        if let Some(hook) = hook {
            hook();
        }
    }
}

/// Why a write was refused on its second pass through the gate.
#[derive(Debug, Clone, Copy)]
enum Refused {
    /// The gate refuses the session and the user's directory is gone: the account was removed
    /// meanwhile. Node's `assertActive` answer, 410 (core http.cjs errorResponse).
    Removed,
    /// Any other refusal: the gate's own answer.
    Gate(Rejection),
}

/// The gate once more, for the user it let through before the body was read.
fn regate(
    layer: &crate::identity::IdentityLayer,
    creds: &server_auth::Creds,
    path: &str,
    now: i64,
    data_dir: &std::path::Path,
    user: &str,
) -> Result<(), Refused> {
    let refused = match layer.gate_blocking(creds, path, now) {
        Ok(id) if id.user.id == user => return Ok(()),
        Ok(_) => Rejection::Refused(server_auth::Refusal::Unauthorized),
        Err(rejection) => rejection,
    };
    let dir_gone = server_projects::store::user_dir(data_dir, user).is_ok_and(|d| !d.exists());
    if dir_gone
        && matches!(
            refused,
            Rejection::Refused(server_auth::Refusal::Unauthorized)
        )
    {
        Err(Refused::Removed)
    } else {
        Err(Refused::Gate(refused))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_unknown_session_takes_the_new_directory_away() {
        let (undo, answer) = after_second_gate(
            true,
            Refused::Gate(Rejection::Refused(server_auth::Refusal::Unauthorized)),
        );
        assert!(undo && matches!(answer, Refused::Removed));
        // Transient or other refusals keep it: a concurrent request may be using it.
        for refusal in [
            Rejection::Unavailable,
            Rejection::Refused(server_auth::Refusal::Csrf),
        ] {
            let (undo, answer) = after_second_gate(true, Refused::Gate(refusal));
            assert!(!undo && matches!(answer, Refused::Gate(_)));
        }
        // A directory that was already there is never taken away.
        let (undo, _) = after_second_gate(
            false,
            Refused::Gate(Rejection::Refused(server_auth::Refusal::Unauthorized)),
        );
        assert!(!undo);
    }

    #[test]
    fn free_chat_list_matchers() {
        assert_eq!(route("GET", "/api/freechats"), Some(Route::FreeChats));
        assert_eq!(route("POST", "/api/freechats"), Some(Route::SaveFreeChats));
        for (m, p) in [
            ("DELETE", "/api/freechats"),
            ("PUT", "/api/freechats"),
            ("GET", "/api/freechats/"),
            ("DELETE", "/api/freechats/c1"),
            ("GET", "/api/freechatsx"),
        ] {
            assert_eq!(route(m, p), None, "{m} {p}");
        }
    }

    #[test]
    fn chat_context_matchers() {
        assert_eq!(
            route("GET", "/api/chats/c%201/context"),
            Some(Route::Context {
                chat: "c%201".into(),
                create: false
            })
        );
        assert_eq!(
            route("POST", "/api/chats/c1/context"),
            Some(Route::Context {
                chat: "c1".into(),
                create: true
            })
        );
        for (m, p) in [
            ("PUT", "/api/chats/c1/context"),
            ("DELETE", "/api/chats/c1/context"),
            ("GET", "/api/chats//context"),
            ("GET", "/api/chats/c1/context/"),
            ("GET", "/api/chats/a/b/context"),
        ] {
            assert_eq!(route(m, p), None, "{m} {p}");
        }
    }

    #[test]
    fn retention_matcher_is_get_only() {
        assert_eq!(
            route("GET", "/api/account/retention"),
            Some(Route::Retention)
        );
        for (m, p) in [
            ("PUT", "/api/account/retention"),
            ("POST", "/api/account/retention"),
            ("DELETE", "/api/account/retention"),
            ("GET", "/api/account/retention/"),
            ("GET", "/api/account/retention/x"),
        ] {
            assert_eq!(route(m, p), None, "{m} {p}");
        }
    }

    #[test]
    fn context_window_matchers() {
        assert_eq!(
            route("GET", "/api/chats/c%201/context-window"),
            Some(Route::ContextWindow {
                chat: "c%201".into()
            })
        );
        for (m, p) in [
            ("POST", "/api/chats/c1/context-window"),
            ("DELETE", "/api/chats/c1/context-window"),
            ("GET", "/api/chats//context-window"),
            ("GET", "/api/chats/c1/context-window/"),
            ("GET", "/api/chats/a/b/context-window"),
        ] {
            assert_eq!(route(m, p), None, "{m} {p}");
        }
    }

    #[test]
    fn chat_list_matchers() {
        assert_eq!(
            route("GET", "/api/projects/p%201/chats"),
            Some(Route::Chats {
                project: "p%201".into()
            })
        );
        assert_eq!(
            route("POST", "/api/projects/p/chats"),
            Some(Route::SaveChats {
                project: "p".into()
            })
        );
    }

    #[test]
    fn nodes_matchers_and_methods() {
        assert_eq!(
            route("POST", "/api/projects/p%201/assets"),
            Some(Route::Upload {
                project: "p%201".into()
            })
        );
        assert_eq!(
            route("DELETE", "/api/projects/p/assets/img-1"),
            Some(Route::One {
                project: "p".into(),
                asset: "img-1".into()
            })
        );
        for (m, p) in [
            ("POST", "/api/projects/p/assets/"),
            ("POST", "/api/projects//assets"),
            ("GET", "/api/projects/p/assets"),
            ("GET", "/api/projects/p/assets/"),
            ("GET", "/api/projects/p/assets/a/b"),
            ("PUT", "/api/projects/p/assets/a"),
            ("POST", "/api/projects/p/assets/a"),
            ("GET", "/api/projectsx/p/assets/a"),
            ("GET", "/api/projects//chats"),
            ("GET", "/api/projects/p/chats/"),
            ("PUT", "/api/projects/p/chats"),
            ("GET", "/api/projects/p/chats/c"),
        ] {
            assert_eq!(route(m, p), None, "{m} {p}");
        }
    }
}
