//! `POST /v1/projects/text-write` (track R14 step 5), on the front's internal project store: core
//! projects.cjs `writeProjectTextFile`, the write path of the internal MCP project tools, run by
//! the front ([`server_projects::text_write`]). Core calls it only when `NOEVIA_RUST_PROJECT_WRITE=1`
//! (core's switch; unset = core writes, as today), with the store's token.
//!
//! Body: `{ user, project, name, text }` and core's options when it set them (`expectName`,
//! `expectContent`, `expectAttachment`, `adoptTo`, `adoptAccount`; left out = `undefined`). Answers
//! 200 `{ file }` (the source as recorded) or 200 `{ error }` (core's refusal, for the model), 400
//! for a body core never sends, 404 `no such account` for an account that is gone or disabled
//! (core's `runAs` refuses it), 503 when the front cannot run it now.
//!
//! The write acts for the account core names, as core's MCP server does (`runAs`: `{ user, legacy:
//! false }`): the account's own role from the `users` table, which the storage endpoint policy
//! reads. The identity's credential is a placeholder, as for a Diary connector
//! (`server_diary::tenant::Tenant::load_user`): storage reads only the id and the role.
use std::sync::Arc;

use axum::body::Body;
use axum::http::{HeaderMap, Response, StatusCode};
use axum::routing::post;
use axum::Router;
use bytes::Bytes;
use js_json::{wtf16, JValue};
use server_auth::{Credential, Identity, PublicUser, Role};
use server_projects::text_write::{self, Edit};

use crate::project_store::{authorised, Store};
use crate::App;

pub const PATH: &str = "/v1/projects/text-write";
/// The body cap: core's upload cap for the text (25 MB) after JSON escaping (at most 6 bytes per
/// byte for control characters), plus the options.
const BODY_CAP: usize = 25 * 1024 * 1024 * 6 + 2 * 1024 * 1024;

fn answer(status: StatusCode, body: &JValue) -> Response<Body> {
    let mut res = Response::new(Body::from(wtf16::stringify(body).unwrap_or_default()));
    *res.status_mut() = status;
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

fn error(status: StatusCode, message: &str) -> Response<Body> {
    answer(
        status,
        &JValue::obj([("error", JValue::Str(message.to_owned()))]),
    )
}

pub fn route(router: Router, store: Store, app: Option<Arc<App>>) -> Router {
    router.route(
        PATH,
        post(move |headers: HeaderMap, raw: Bytes| {
            let (store, app) = (store.clone(), app.clone());
            async move { serve(store, app, headers, raw).await }
        })
        .layer(axum::extract::DefaultBodyLimit::max(BODY_CAP)),
    )
}

/// The account's own role, when the account is there and not disabled (core's `runAs`).
fn account_role(app: &App, user: &str) -> Result<Option<Role>, ()> {
    let writer = app.writes.writer().map_err(|_| ())?;
    let role = writer
        .read(|r| {
            r.row(
                "SELECT role FROM users WHERE id=?1 AND (disabled_at IS NULL OR disabled_at=0)",
                [user],
                |row| row.get::<_, String>(0),
            )
        })
        .map_err(|_| ())?;
    Ok(match role.as_deref() {
        Some("admin") => Some(Role::Admin),
        Some("member") => Some(Role::Member),
        _ => None,
    })
}

async fn serve(
    store: Store,
    app: Option<Arc<App>>,
    headers: HeaderMap,
    raw: Bytes,
) -> Response<Body> {
    if !authorised(&store, &headers) {
        return error(StatusCode::UNAUTHORIZED, "unauthorised");
    }
    let Some(app) = app else {
        return error(StatusCode::SERVICE_UNAVAILABLE, "unavailable");
    };
    if cfg!(not(feature = "rust-storage-browse")) {
        // Without the storage client the write could not reach the account's storage.
        return error(StatusCode::SERVICE_UNAVAILABLE, "unavailable");
    }
    // The body (up to ~150 MB) is parsed off the async runtime.
    let done = tokio::task::spawn_blocking(move || {
        let body = match wtf16::parse(&String::from_utf8_lossy(&raw)) {
            Ok(b @ JValue::Obj(_)) => b,
            _ => return error(StatusCode::BAD_REQUEST, "invalid JSON"),
        };
        let text_of = |k: &str| body.get(k).as_str().map(str::to_owned);
        let (Some(user), Some(project), Some(name), Some(text)) = (
            text_of("user").filter(|u| server_projects::store::user_dir(&store.data_dir, u).is_ok()),
            text_of("project").filter(|p| !p.is_empty()),
            text_of("name"),
            text_of("text"),
        ) else {
            return error(StatusCode::BAD_REQUEST, "user, project, name and text are required");
        };
        let edit = Edit {
            expect_name: body.get("expectName").clone(),
            expect_content: body.get("expectContent").clone(),
            expect_attachment: body.get("expectAttachment").clone(),
            adopt_to: body.get("adoptTo").clone(),
            adopt_account: body.get("adoptAccount").clone(),
        };
        let role = match account_role(&app, &user) {
            Ok(Some(role)) => role,
            Ok(None) => return error(StatusCode::NOT_FOUND, "no such account"),
            Err(()) => return error(StatusCode::SERVICE_UNAVAILABLE, "unavailable"),
        };
        let Some(engine) = app.extraction.clone() else {
            return error(StatusCode::SERVICE_UNAVAILABLE, "unavailable");
        };
        let identity = Identity {
            user: PublicUser {
                id: user.clone(),
                username: String::new(),
                display_name: String::new(),
                role,
                disabled: false,
                diary_enabled: false,
                onboarded: true,
            },
            account_role: role,
            // A placeholder: storage reads only the id and the role (see the header).
            credential: Credential::Legacy,
        };
        let ctx = server_projects::Ctx {
            data_dir: &store.data_dir,
            user_id: &user,
            switch: store.switch,
            now_ms: crate::identity::now_ms(),
        };
        let limits = server_projects::extract::Limits::from_env();
        let root = server_projects::lifecycle::project_root_folder();
        let written = crate::source_job_routes::with_storage(&app, &identity, |storage| {
            text_write::write_text(
                &text_write::Env {
                    ctx,
                    storage,
                    engine: engine.as_ref(),
                    limits: &limits,
                    project_root: &root,
                    lock_wait: server_projects::project_documents::FOREGROUND_LOCK_WAIT,
                },
                &project,
                &name,
                &text,
                &edit,
            )
        });
        match written {
            Ok(file) => answer(StatusCode::OK, &JValue::obj([("file", file)])),
            Err(message) => answer(
                StatusCode::OK,
                &JValue::obj([("error", JValue::Str(message))]),
            ),
        }
    })
    .await;
    done.unwrap_or_else(|_| error(StatusCode::INTERNAL_SERVER_ERROR, "internal error"))
}
