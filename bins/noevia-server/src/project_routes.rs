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
use server_projects::{
    assets, chat_delete, chat_move, chats, context, context_window, free_chats, history, lifecycle,
    originals, retention, Ctx, Reply,
};
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// POST /api/projects/{id}/assets
    Upload {
        project: String,
    },
    /// GET or DELETE /api/projects/{id}/assets/{assetId}
    One {
        project: String,
        asset: String,
    },
    /// GET /api/projects/{id}/chats
    Chats {
        project: String,
    },
    /// POST /api/projects/{id}/chats
    SaveChats {
        project: String,
    },
    /// GET /api/freechats
    FreeChats,
    /// POST /api/freechats
    SaveFreeChats,
    /// GET (`create: false`) or POST /api/chats/{chatId}/context (#1264, permanently native under #1071)
    Context {
        chat: String,
        create: bool,
    },
    /// GET /api/chats/{chatId}/context-window (sbstndalton/noevia#1071, no switch)
    ContextWindow {
        chat: String,
    },
    /// GET /api/chats/{chatId}/history (sbstndalton/noevia#1071 slice 2, no switch)
    History {
        chat: String,
    },
    /// POST /api/chats/{chatId}/history (sbstndalton/noevia#1071 slice 5, no switch)
    SaveHistory {
        chat: String,
    },
    /// GET /api/account/retention (sbstndalton/noevia#1071, no switch; `PUT` is Node's)
    Retention,
    MoveChat {
        chat: String,
    },
    /// DELETE /api/freechats/{id} (sbstndalton/noevia#1071 slice 6)
    DeleteFreeChat {
        chat: String,
    },
    /// DELETE /api/projects/{id}/chats/{chatId} (sbstndalton/noevia#1071 slice 6)
    DeleteChat {
        project: String,
        chat: String,
    },
    /// GET /api/projects/{id}/sources/text (sbstndalton/noevia#2191, `rust-project-reads`)
    SourceText {
        project: String,
    },
    /// GET /api/projects/{id}/documents/original (sbstndalton/noevia#2191)
    DocumentOriginal {
        project: String,
    },
    /// GET /api/projects/{id}/uploads/original (sbstndalton/noevia#2191)
    UploadOriginal {
        project: String,
    },
    /// GET /api/projects/{id}/documents/pages (sbstndalton/noevia#2191 round 4,
    /// `rust-project-document-pages`)
    DocumentPages {
        project: String,
    },
    /// POST /api/import/conversations (sbstndalton/noevia#2191 round 4, `rust-conversation-import`)
    ImportConversations,
    /// POST /api/projects (sbstndalton/noevia#2191, `rust-project-lifecycle`)
    CreateProject,
    /// DELETE /api/projects/{projectId} (sbstndalton/noevia#2191, `rust-project-lifecycle`)
    DeleteProject {
        project: String,
    },
    /// DELETE /api/projects/{id}/files (sbstndalton/noevia#2191 round 3, `rust-project-files`)
    DeleteFile {
        project: String,
    },
    /// POST /api/projects/{id}/documents (sbstndalton/noevia#2191 round 5, `rust-project-documents`)
    Documents {
        project: String,
    },
    /// POST /api/projects/{id}/upload or .../sources/sync, answered here only with `background=1`
    /// (sbstndalton/noevia#2191 round 6, `rust-source-jobs`); the synchronous form is Node's, the
    /// front's worker call.
    BackgroundSource {
        project: String,
        sync: bool,
    },
    /// GET /api/projects/{id}/source-jobs/{jobId} (sbstndalton/noevia#2191 round 6)
    SourceJob {
        project: String,
        job: String,
    },
}

/// Node's matchers, `^\/api\/projects\/([^/]+)\/assets$`, `...\/assets\/([^/]+)$` and
/// `^\/api\/projects\/([^/]+)\/chats$`, and routes/chat-lists.cjs's `p === '/api/freechats'`,
/// with the methods its handlers answer; `None` sends the request on to Node.
pub fn route(method: &str, path: &str) -> Option<Route> {
    // routes/import.cjs `path === '/api/import/conversations'`, its POST only (Node answers 405).
    if path == server_projects::import::ROUTE {
        return (method == "POST").then_some(Route::ImportConversations);
    }
    // routes/account.cjs `path === '/api/account/retention'`, its GET only.
    if path == "/api/account/retention" {
        return (method == "GET").then_some(Route::Retention);
    }
    // routes/chat-lists.cjs `/^\/api\/freechats\/([^/]+)$/` for DELETE.
    if let Some(chat) = path.strip_prefix("/api/freechats/") {
        return (method == "DELETE" && !chat.is_empty() && !chat.contains('/')).then(|| {
            Route::DeleteFreeChat {
                chat: chat.to_string(),
            }
        });
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
            // routes/chat-lists.cjs `/^\/api\/chats\/([^/]+)\/history$/` for GET and POST.
            ("GET", [chat, "history"]) if !chat.is_empty() => Some(Route::History {
                chat: (*chat).to_string(),
            }),
            ("POST", [chat, "move"]) if !chat.is_empty() => Some(Route::MoveChat {
                chat: (*chat).to_string(),
            }),
            ("POST", [chat, "history"]) if !chat.is_empty() => Some(Route::SaveHistory {
                chat: (*chat).to_string(),
            }),
            _ => None,
        };
    }
    // routes/projects.cjs `p === '/api/projects'` for POST (sbstndalton/noevia#2191).
    if path == "/api/projects" {
        return (method == "POST").then_some(Route::CreateProject);
    }
    let rest = path.strip_prefix("/api/projects/")?;
    let parts: Vec<&str> = rest.split('/').collect();
    match (method, parts.as_slice()) {
        // routes/projects.cjs `/^\/api\/projects\/([^/]+)$/` for DELETE (sbstndalton/noevia#2191).
        ("DELETE", [project]) if !project.is_empty() => Some(Route::DeleteProject {
            project: (*project).to_string(),
        }),
        // routes/projects.cjs `projFileDel`, DELETE only (sbstndalton/noevia#2191 round 3).
        ("DELETE", [project, "files"]) if !project.is_empty() => Some(Route::DeleteFile {
            project: (*project).to_string(),
        }),
        // routes/projects.cjs `projDocs`, POST only (sbstndalton/noevia#2191 round 5).
        ("POST", [project, "documents"]) if !project.is_empty() => Some(Route::Documents {
            project: (*project).to_string(),
        }),
        // routes/projects.cjs's background wrapper, `/^\/api\/projects\/([^/]+)\/(upload|sources\/sync)$/`
        // and the poll, `/^\/api\/projects\/([^/]+)\/source-jobs\/([^/]+)$/` (round 6).
        ("POST", [project, "upload"]) if !project.is_empty() => Some(Route::BackgroundSource {
            project: (*project).to_string(),
            sync: false,
        }),
        ("POST", [project, "sources", "sync"]) if !project.is_empty() => {
            Some(Route::BackgroundSource {
                project: (*project).to_string(),
                sync: true,
            })
        }
        ("GET", [project, "source-jobs", job]) if !project.is_empty() && !job.is_empty() => {
            Some(Route::SourceJob {
                project: (*project).to_string(),
                job: (*job).to_string(),
            })
        }
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
        // routes/projects.cjs `sourceTextRoute`, `docRead` (its `original` half) and
        // `originalUpload`, GET only (sbstndalton/noevia#2191); `docRead`'s page reader since round 4.
        ("GET", [project, "sources", "text"]) if !project.is_empty() => Some(Route::SourceText {
            project: (*project).to_string(),
        }),
        ("GET", [project, "documents", "original"]) if !project.is_empty() => {
            Some(Route::DocumentOriginal {
                project: (*project).to_string(),
            })
        }
        ("GET", [project, "documents", "pages"]) if !project.is_empty() => {
            Some(Route::DocumentPages {
                project: (*project).to_string(),
            })
        }
        ("GET", [project, "uploads", "original"]) if !project.is_empty() => {
            Some(Route::UploadOriginal {
                project: (*project).to_string(),
            })
        }
        // routes/projects.cjs `/^\/api\/projects\/([^/]+)\/chats\/([^/]+)$/` for DELETE.
        ("DELETE", [project, "chats", chat]) if !project.is_empty() && !chat.is_empty() => {
            Some(Route::DeleteChat {
                project: (*project).to_string(),
                chat: (*chat).to_string(),
            })
        }
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
    let (mut parts, body) = req.into_parts();
    // `url.searchParams.get('background') === '1'`: the first `background`, form-decoded.
    let background = parts.uri.query().is_some_and(|q| {
        url::form_urlencoded::parse(q.as_bytes())
            .find(|(k, _)| k == "background")
            .is_some_and(|(_, v)| v == "1")
    });
    let route = route(parts.method.as_str(), &path)
        .filter(|r| background || !matches!(r, Route::BackgroundSource { .. }));
    let Some(route) = route else {
        // A synchronous upload or sync is Node's; only this front may ask for its worker form.
        parts.headers.remove(crate::source_job_routes::WORK_HEADER);
        return app
            .proxy
            .forward(conn.peer, Request::from_parts(parts, body))
            .await;
    };
    let switch = app.config.rust_projects;
    // The running-reply marker's start (#1071); none without Node beside the front.
    let start_id = app.config.start_id.clone().unwrap_or_default();
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
        Route::SaveHistory { .. } => Some(history::BODY_LIMIT),
        Route::MoveChat { .. } => Some(chat_move::BODY_LIMIT),
        Route::CreateProject => Some(lifecycle::BODY_LIMIT),
        Route::ImportConversations => Some(server_projects::import::MAX_BYTES),
        Route::DeleteFile { .. } => Some(server_projects::files::BODY_LIMIT),
        Route::Documents { .. } => Some(server_projects::project_documents::BODY_LIMIT),
        // The legacy proxy's caps, which these bodies met before (core's own readJson caps lie
        // within them).
        Route::BackgroundSource { sync: false, .. } => {
            Some(crate::legacy_proxy::UPLOAD_BODY_CAP as usize)
        }
        Route::BackgroundSource { sync: true, .. } => Some(crate::legacy_proxy::BODY_CAP as usize),
        Route::SourceJob { .. }
        | Route::DeleteProject { .. }
        | Route::One { .. }
        | Route::Chats { .. }
        | Route::FreeChats
        | Route::Context { .. }
        | Route::ContextWindow { .. }
        | Route::History { .. }
        | Route::Retention
        | Route::DeleteFreeChat { .. }
        | Route::DeleteChat { .. }
        | Route::SourceText { .. }
        | Route::DocumentOriginal { .. }
        | Route::DocumentPages { .. }
        | Route::UploadOriginal { .. } => None,
    };
    // `url.searchParams.get('name')`: the first `name`, form-decoded as WHATWG URLSearchParams.
    let name = parts.uri.query().and_then(|q| {
        url::form_urlencoded::parse(q.as_bytes())
            .find(|(k, _)| k == "name")
            .map(|(_, v)| v.into_owned())
    });
    let query = parts.uri.query().map(str::to_owned);
    let upload = match limit {
        Some(limit) => match crate::account_routes::read_body(body, limit).await {
            Ok(b) => Some(b),
            // routes/chat-lists.cjs answers a history save it could not read so.
            Err(()) if matches!(route, Route::MoveChat { .. }) => {
                return reply::error(StatusCode::BAD_REQUEST, "invalid JSON", true);
            }
            // routes/import.cjs: a body it could not read is the "not an export" 400.
            Err(()) if matches!(route, Route::ImportConversations) => {
                return reply::error(
                    StatusCode::BAD_REQUEST,
                    server_projects::import::NOT_EXPORT,
                    true,
                )
            }
            // routes/projects.cjs: a document body it could not read is `invalid JSON`.
            Err(()) if matches!(route, Route::Documents { .. }) => {
                return reply::error(StatusCode::BAD_REQUEST, "invalid JSON", true)
            }
            Err(()) if matches!(route, Route::BackgroundSource { .. }) => {
                return reply::error(StatusCode::BAD_REQUEST, "invalid JSON", true)
            }
            Err(()) if matches!(route, Route::SaveHistory { .. }) => {
                return reply::error(StatusCode::BAD_REQUEST, "could not read the chat", true)
            }
            Err(()) => return reply::error(StatusCode::BAD_REQUEST, "invalid request body", true),
        },
        None => None,
    };
    // The source jobs (round 6): the store side runs on the blocking pool, the operation is Node's.
    match &route {
        Route::SourceJob { project, job } => {
            let account = crate::source_job_routes::Account {
                data_dir: data_dir.clone(),
                user: authn.user.id.clone(),
                start: start_id.clone(),
                now_ms: now,
            };
            return crate::source_job_routes::poll(account, project.clone(), job.clone()).await;
        }
        Route::BackgroundSource { project, .. } => {
            let Some(b) = upload else {
                return reply::error(StatusCode::INTERNAL_SERVER_ERROR, "Internal error", true);
            };
            let account = crate::source_job_routes::Account {
                data_dir: data_dir.clone(),
                user: authn.user.id.clone(),
                start: start_id.clone(),
                now_ms: now,
            };
            let (layer, creds, gate_path, dir, user) = (
                Arc::clone(&regate_layer),
                regate_creds.clone(),
                path.clone(),
                data_dir.clone(),
                authn.user.id.clone(),
            );
            let again = move || {
                regate(&layer, &creds, &gate_path, now, &dir, &user)
                    .map_err(|r| Box::new(refusal(r)))
            };
            // The path as the client sent it (Node decodes the id itself).
            let raw_path = parts.uri.path().to_owned();
            return crate::source_job_routes::start(
                app,
                conn.peer,
                account,
                raw_path,
                project.clone(),
                parts.headers.clone(),
                b.bytes,
                b.over,
                again,
            )
            .await;
        }
        _ => {}
    }
    let method = parts.method.clone();
    // routes/projects.cjs: `authn?.user?.role === 'admin'` (a device token acts as a member).
    let admin = authn.user.role == server_auth::Role::Admin;
    // What a create reads besides the request (sbstndalton/noevia#2191): core's published toolbox
    // catalogue and whether the user's storage is browsable (folder reservation, #589); read on the
    // blocking pool with the rest.
    let create_inputs = matches!(route, Route::CreateProject | Route::ImportConversations)
        .then(|| (Arc::clone(app), authn.clone()));
    // A source file's delete writes to the user's storage (round 3, #2191).
    let file_inputs =
        matches!(route, Route::DeleteFile { .. }).then(|| (Arc::clone(app), authn.clone()));
    // A document upload extracts through the front's engine (round 5, #2191).
    let engine = matches!(route, Route::Documents { .. })
        .then(|| app.extraction.clone())
        .flatten();
    // A create or delete runs the user's queued storage sweep afterwards (round 3, #2191).
    let sweep_inputs = matches!(
        route,
        Route::CreateProject | Route::DeleteProject { .. } | Route::ImportConversations
    )
    .then(|| (Arc::clone(app), authn.clone(), data_dir.clone()));
    let user = authn.user.id;
    // The context GET writes too (getProject's self-heal), so it is re-gated as well; so do the
    // source reads, whose getProject may save the same way.
    let writes = matches!(
        route,
        Route::Upload { .. }
            | Route::SourceText { .. }
            | Route::DocumentOriginal { .. }
            | Route::DocumentPages { .. }
            | Route::UploadOriginal { .. }
            | Route::SaveChats { .. }
            | Route::SaveFreeChats
            | Route::SaveHistory { .. }
            | Route::MoveChat { .. }
            | Route::Context { .. }
            | Route::DeleteFreeChat { .. }
            | Route::DeleteChat { .. }
            | Route::CreateProject
            | Route::DeleteProject { .. }
            | Route::DeleteFile { .. }
            | Route::Documents { .. }
            | Route::ImportConversations
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
        let create_env = create_inputs.map(|(app, authn)| {
            (
                lifecycle::Catalogue::read_waiting(&data_dir, std::time::Duration::from_secs(10)),
                storage_kind(&app, &authn),
                lifecycle::project_root_folder(),
                (app, authn),
            )
        });
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
            (Route::History { chat }, _) => history::get(ctx, &chat, &start_id),
            (Route::SaveHistory { chat }, Some(b)) => {
                history::post(ctx, &chat, &b.bytes, b.over, &start_id)
            }
            (Route::Retention, _) => retention::get(ctx),
            (Route::MoveChat { chat }, Some(b)) => {
                chat_move::post(ctx, &chat, &b.bytes, b.over, &start_id)
            }
            (Route::DeleteFreeChat { chat }, _) => chat_delete::delete_free_route(ctx, &chat),
            (Route::DeleteChat { project, chat }, _) => {
                chat_delete::delete_project_route(ctx, &project, &chat)
            }
            (Route::SourceText { project }, _) => {
                server_projects::sources::get(ctx, &project, name.as_deref())
            }
            (Route::DocumentOriginal { project }, _) => {
                originals::document(ctx, &project, name.as_deref())
            }
            (Route::UploadOriginal { project }, _) => {
                originals::upload(ctx, &project, name.as_deref())
            }
            (Route::DocumentPages { project }, _) => {
                server_projects::document_pages::get(ctx, &project, query.as_deref())
            }
            (Route::CreateProject, Some(b)) => match &create_env {
                Some((catalogue, storage, root, (app, authn))) => {
                    let opener = opener(app, authn);
                    lifecycle::create(
                        ctx,
                        &b.bytes,
                        b.over,
                        &lifecycle::CreateEnv {
                            catalogue: catalogue.as_ref(),
                            storage: *storage,
                            root,
                            with_remote: Some(&opener),
                        },
                    )
                }
                None => Reply::internal(),
            },
            (Route::ImportConversations, Some(b)) => match &create_env {
                Some((catalogue, storage, root, (app, authn))) => {
                    let opener = opener(app, authn);
                    let env = lifecycle::CreateEnv {
                        catalogue: catalogue.as_ref(),
                        storage: *storage,
                        root,
                        with_remote: Some(&opener),
                    };
                    // conversation-import.cjs `ctx.createProject({ name, toolboxes: [] })`.
                    let create = |name: &str| {
                        let body = js_json::wtf16::stringify(&js_json::JValue::obj([
                            ("name", js_json::JValue::Str(name.to_string())),
                            ("toolboxes", js_json::JValue::Arr(Vec::new())),
                        ]))
                        .unwrap_or_default();
                        let made = lifecycle::create(ctx, body.as_bytes(), false, &env);
                        let id = match (&made.status, &made.body) {
                            (200, server_projects::Body::Json(text)) => js_json::wtf16::parse(text)
                                .ok()
                                .and_then(|v| v.get("id").as_str().map(str::to_owned)),
                            _ => None,
                        };
                        id.ok_or(if made.status == 400 { 400 } else { 503 })
                    };
                    let audit =
                        |result: &js_json::JValue| import_audit(app, &authn.user.id, result);
                    server_projects::import::post(
                        ctx,
                        &b.bytes,
                        b.over,
                        &server_projects::import::ImportEnv {
                            create: &create,
                            audit: &audit,
                        },
                    )
                }
                None => Reply::internal(),
            },
            (Route::DeleteProject { project }, _) => lifecycle::delete(ctx, &project),
            (Route::DeleteFile { project }, Some(b)) => match &file_inputs {
                Some((app, authn)) => {
                    let opener = opener(app, authn);
                    server_projects::files::delete(ctx, &project, &b.bytes, b.over, &opener)
                }
                None => Reply::internal(),
            },
            (Route::Documents { project }, Some(b)) => match &engine {
                Some(engine) => server_projects::project_documents::post(
                    ctx,
                    &project,
                    &b.bytes,
                    b.over,
                    engine.as_ref(),
                    &server_projects::extract::Limits::from_env(),
                ),
                None => Reply::error(503, "Document extraction is unavailable; retry shortly."),
            },
            (
                Route::CreateProject
                | Route::ImportConversations
                | Route::Documents { .. }
                | Route::DeleteFile { .. }
                | Route::Upload { .. }
                | Route::SaveChats { .. }
                | Route::SaveFreeChats
                | Route::SaveHistory { .. }
                | Route::MoveChat { .. },
                None,
            ) => Reply::internal(),
            // Answered above (source_job_routes).
            (Route::BackgroundSource { .. } | Route::SourceJob { .. }, _) => Reply::internal(),
        })
    })
    .await;
    if let (Some((app, authn, data_dir)), Ok(Ok(r))) = (sweep_inputs, &done) {
        if r.status == 200 {
            // Detached, as core's deleteProject never awaited its sweep; the job is durable in the
            // outbox, so a restart or a busy storage slot only delays it.
            tokio::task::spawn_blocking(move || sweep_queued(&app, &authn, &data_dir));
        }
    }
    match done {
        Ok(Ok(r)) => respond(r),
        Ok(Err(refused)) => refusal(refused),
        Err(_) => reply::error(StatusCode::INTERNAL_SERVER_ERROR, "Internal error", true),
    }
}

/// The create's storage opener (higher-ranked over the step, as `OpenRemote` needs).
/// The import's audit row: core auth.cjs `audit('import.conversations', actor, actor, { imported,
/// skipped: <count>, projectsCreated })`, appended to cowork.db's audit_events as Node appends it.
fn import_audit(app: &App, user: &str, result: &js_json::JValue) -> Result<(), ()> {
    let skipped = match result.get("skipped") {
        js_json::JValue::Arr(list) => list.len(),
        _ => 0,
    };
    let detail = js_json::wtf16::stringify(&js_json::JValue::obj([
        ("imported", result.get("imported").clone()),
        ("skipped", js_json::JValue::Num(skipped as f64)),
        ("projectsCreated", result.get("projectsCreated").clone()),
    ]))
    .ok_or(())?;
    let at = identity::now_ms();
    app.writes
        .writer()
        .and_then(|w| {
            w.write(|tx| {
                tx.execute(
                    "INSERT INTO audit_events(actor_user_id,target_user_id,action,detail,created_at) VALUES(?1,?2,?3,?4,?5)",
                    server_store::rusqlite::params![user, user, server_projects::import::AUDIT_ACTION, detail, at],
                )?;
                Ok(())
            })
        })
        .map_err(|_| ())
}

fn opener<'a>(
    app: &'a Arc<App>,
    authn: &'a server_auth::Identity,
) -> impl Fn(&mut server_projects::project_storage::StorageStep<'_>) + 'a {
    move |step| with_remote(app, authn, step)
}

/// Runs `step` with the user's storage opened for writes (none without the storage features).
#[cfg(feature = "rust-storage-browse")]
fn with_remote(
    app: &Arc<App>,
    authn: &server_auth::Identity,
    step: &mut dyn FnMut(&server_projects::project_storage::Remote<'_>),
) {
    let opened = crate::project_storage_io::open(app, authn);
    step(&opened.remote());
}

#[cfg(not(feature = "rust-storage-browse"))]
fn with_remote(
    _app: &Arc<App>,
    _authn: &server_auth::Identity,
    step: &mut dyn FnMut(&server_projects::project_storage::Remote<'_>),
) {
    step(&server_projects::project_storage::Remote::None);
}

/// The user's pending `sweep` jobs (a delete's storage sweep), run by the front since round 3.
fn sweep_queued(app: &Arc<App>, authn: &server_auth::Identity, data_dir: &std::path::Path) {
    let Ok(dir) = server_projects::store::user_dir(data_dir, authn.user.id.as_str()) else {
        return;
    };
    let queued = server_projects::outbox::jobs(&dir)
        .map(|rows| {
            rows.iter()
                .any(|r| r.kind == "sweep" && r.state != "failed")
        })
        .unwrap_or(false);
    if !queued {
        return;
    }
    let root = lifecycle::project_root_folder();
    #[cfg(feature = "rust-storage-browse")]
    {
        let opened = crate::project_storage_io::open(app, authn);
        if matches!(opened, crate::project_storage_io::Opened::Refused(..)) {
            return;
        }
        server_projects::project_storage::sweep_pending(
            &dir,
            &opened.remote(),
            &root,
            identity::now_ms(),
        );
    }
    #[cfg(not(feature = "rust-storage-browse"))]
    {
        let _ = (app, &root);
    }
}

/// The user's storage kind as core's createProject reads it (`authService.getStorage(userId, true)`
/// then `storageClient.isBrowsable`): `None` for local storage, and for a row whose credential
/// cannot be opened (Node's try/catch skips the folder reservation then).
#[cfg(not(feature = "rust-storage-reads"))]
fn storage_kind(_app: &Arc<App>, _authn: &server_auth::Identity) -> Option<lifecycle::Storage> {
    None
}

#[cfg(feature = "rust-storage-reads")]
fn storage_kind(app: &Arc<App>, authn: &server_auth::Identity) -> Option<lifecycle::Storage> {
    use server_storage_reads::{owner_secret, protocol::Kind};
    let data_dir = app.config.data_dir.as_ref()?;
    let previous = std::env::var("SECRETS_KEY_PREVIOUS")
        .ok()
        .map(zeroize::Zeroizing::new);
    let keys =
        owner_secret::load_existing_keys(data_dir, previous.as_deref().map(String::as_str)).ok();
    let store = app.identity.store_blocking().ok()?;
    let projected = owner_secret::project_connection_with(&store, authn, keys.as_ref()).ok()?;
    match projected.connection().kind {
        Kind::S3 => Some(lifecycle::Storage::S3),
        Kind::WebDav | Kind::Nextcloud => Some(lifecycle::Storage::Dav),
        Kind::Local => None,
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
            ("GET", "/api/freechats/c1"),
            ("POST", "/api/freechats/c1"),
            ("DELETE", "/api/freechats/"),
            ("DELETE", "/api/freechats/a/b"),
            ("DELETE", "/api/freechats/c1/"),
            ("GET", "/api/freechatsx"),
        ] {
            assert_eq!(route(m, p), None, "{m} {p}");
        }
        // routes/chat-lists.cjs `/^\/api\/freechats\/([^/]+)$/`, DELETE only (#1071 slice 6).
        assert_eq!(
            route("DELETE", "/api/freechats/c%201"),
            Some(Route::DeleteFreeChat {
                chat: "c%201".into()
            })
        );
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
    fn history_matchers() {
        assert_eq!(
            route("GET", "/api/chats/c%201/history"),
            Some(Route::History {
                chat: "c%201".into()
            })
        );
        assert_eq!(
            route("POST", "/api/chats/c%201/history"),
            Some(Route::SaveHistory {
                chat: "c%201".into()
            })
        );
        for (m, p) in [
            ("PUT", "/api/chats/c1/history"),
            ("POST", "/api/chats//history"),
            ("POST", "/api/chats/c1/history/"),
            ("DELETE", "/api/chats/c1/history"),
            ("GET", "/api/chats//history"),
            ("GET", "/api/chats/c1/history/"),
            ("GET", "/api/chats/a/b/history"),
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
        assert_eq!(
            route("DELETE", "/api/projects/p%201/chats/c%202"),
            Some(Route::DeleteChat {
                project: "p%201".into(),
                chat: "c%202".into()
            })
        );
        for (m, p) in [
            ("DELETE", "/api/projects/p/chats"),
            ("DELETE", "/api/projects/p/chats/"),
            ("DELETE", "/api/projects//chats/c"),
            ("DELETE", "/api/projects/p/chats/c/d"),
            ("PUT", "/api/projects/p/chats/c"),
        ] {
            assert_eq!(route(m, p), None, "{m} {p}");
        }
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
