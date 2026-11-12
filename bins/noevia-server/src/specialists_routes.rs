//! Coworkers and Spaces, slice 1 (sbstndalton/noevia#2446, capability `rust-specialists`): the
//! account's specialists, Spaces and Space pages. Rust alone reads and writes them
//! (crates/server-specialists); Node never sees these paths.
//!
//! | Path | Methods |
//! |---|---|
//! | `/api/specialists` | GET, POST |
//! | `/api/specialists/{id}` | GET, PATCH, DELETE |
//! | `/api/spaces` | GET, POST |
//! | `/api/spaces/{id}` | GET, PATCH, DELETE |
//! | `/api/spaces/{id}/pages` | GET, POST |
//! | `/api/spaces/{id}/pages/{pageId}` | GET, PUT, DELETE |
//!
//! Order: the session gate (401; CSRF 403 on writes), the method (405 with `Allow`), the ids (a
//! path segment that is not a lowercase UUID is 404 before any store is opened), then the store.
//! Another account's ids are 404, never 403: every store is the signed-in account's own directory.
//! Errors are `{error, code}`; a stale write is 409 with the current `revision` (lists) or
//! `version` (pages) beside them.

use crate::{identity, project_routes, reply, App};
use axum::body::Body;
use axum::http::{header, HeaderValue, Method, Request, Response, StatusCode};
use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use server_specialists::{Error, Store};
use std::sync::Arc;

pub const SPECIALISTS: &str = "/api/specialists";
pub const SPECIALIST: &str = "/api/specialists/{id}";
pub const SPACES: &str = "/api/spaces";
pub const SPACE: &str = "/api/spaces/{id}";
pub const PAGES: &str = "/api/spaces/{id}/pages";
pub const PAGE: &str = "/api/spaces/{id}/pages/{pageId}";
pub const ROUTES: &[&str] = &[SPECIALISTS, SPECIALIST, SPACES, SPACE, PAGES, PAGE];

/// Bodies other than a page's: a few small fields.
const SMALL_BODY: usize = 64 * 1024;
/// A page's content may be 512 KiB; JSON escaping can nearly double it.
const PAGE_BODY: usize = 1024 * 1024 + 64 * 1024;

#[derive(Debug, PartialEq)]
enum Target {
    Specialists,
    Specialist(String),
    Spaces,
    Space(String),
    Pages(String),
    Page(String, String),
}

impl Target {
    fn allowed(&self) -> &'static [Method] {
        match self {
            Target::Specialists | Target::Spaces | Target::Pages(_) => &[Method::GET, Method::POST],
            Target::Specialist(_) | Target::Space(_) => {
                &[Method::GET, Method::PATCH, Method::DELETE]
            }
            Target::Page(..) => &[Method::GET, Method::PUT, Method::DELETE],
        }
    }
}

/// The target of a path, or `None` for one that names nothing: a trailing slash, an extra segment.
/// The ids are not checked here.
fn target(path: &str) -> Option<Target> {
    let rest = path.strip_prefix("/api/")?;
    let seg: Vec<&str> = rest.split('/').collect();
    if seg.iter().any(|s| s.is_empty()) {
        return None;
    }
    match seg.as_slice() {
        ["specialists"] => Some(Target::Specialists),
        ["specialists", id] => Some(Target::Specialist((*id).to_owned())),
        ["spaces"] => Some(Target::Spaces),
        ["spaces", id] => Some(Target::Space((*id).to_owned())),
        ["spaces", id, "pages"] => Some(Target::Pages((*id).to_owned())),
        ["spaces", id, "pages", page] => Some(Target::Page((*id).to_owned(), (*page).to_owned())),
        _ => None,
    }
}

fn ids_ok(t: &Target) -> bool {
    match t {
        Target::Specialists | Target::Spaces => true,
        Target::Specialist(a) | Target::Space(a) | Target::Pages(a) => server_specialists::is_id(a),
        Target::Page(a, b) => server_specialists::is_id(a) && server_specialists::is_id(b),
    }
}

fn failure(status: StatusCode, message: &str, code: &str) -> Response<Body> {
    reply::json(status, &json!({"error": message, "code": code}), true)
}

fn not_found() -> Response<Body> {
    failure(StatusCode::NOT_FOUND, "Not found", "not_found")
}

fn invalid(message: &str) -> Response<Body> {
    failure(StatusCode::BAD_REQUEST, message, "invalid_request")
}

fn method_not_allowed(allowed: &[Method]) -> Response<Body> {
    let mut res = failure(
        StatusCode::METHOD_NOT_ALLOWED,
        "method not allowed",
        "method_not_allowed",
    );
    let allow = allowed
        .iter()
        .map(Method::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    if let Ok(v) = HeaderValue::from_str(&allow) {
        res.headers_mut().insert(header::ALLOW, v);
    }
    res
}

/// The store error as the answer. A stale write carries what the client needs to resolve it.
fn answer(e: &Error) -> Response<Body> {
    let mut body = serde_json::Map::new();
    body.insert("error".into(), json!(e.to_string()));
    body.insert("code".into(), json!(e.code()));
    let status = match e {
        Error::NotFound => StatusCode::NOT_FOUND,
        Error::Invalid(_) => StatusCode::BAD_REQUEST,
        Error::Limit { .. } | Error::SpaceNotEmpty => StatusCode::CONFLICT,
        Error::PageTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
        Error::Stale { revision, version } => {
            if let Some(r) = revision {
                body.insert("revision".into(), json!(r));
            }
            if let Some(v) = version {
                body.insert("version".into(), json!(v));
            }
            StatusCode::CONFLICT
        }
        Error::Corrupt(_) | Error::Io(_) => {
            return reply::error(StatusCode::INTERNAL_SERVER_ERROR, "Internal error", true)
        }
    };
    reply::json(status, &Value::Object(body), true)
}

fn ok(status: StatusCode, body: &Value) -> Response<Body> {
    reply::json(status, body, true)
}

fn page_json(row: &server_specialists::PageRow) -> Value {
    serde_json::to_value(row).unwrap_or(Value::Null)
}

async fn read_body(body: Body, max: usize) -> Result<Vec<u8>, Box<Response<Body>>> {
    match tokio::time::timeout(
        std::time::Duration::from_secs(10),
        axum::body::to_bytes(body, max),
    )
    .await
    {
        Ok(Ok(bytes)) => Ok(bytes.to_vec()),
        Ok(Err(e)) => {
            if e.into_inner().is::<http_body_util::LengthLimitError>() {
                Err(Box::new(failure(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "Request exceeds size limit",
                    if max == PAGE_BODY {
                        "page_too_large"
                    } else {
                        "invalid_request"
                    },
                )))
            } else {
                Err(Box::new(invalid("invalid JSON")))
            }
        }
        Err(_) => Err(Box::new(invalid("invalid JSON"))),
    }
}

async fn json_body<T: DeserializeOwned>(body: Body, max: usize) -> Result<T, Box<Response<Body>>> {
    let bytes = read_body(body, max).await?;
    serde_json::from_slice(&bytes).map_err(|e| Box::new(invalid(&format!("invalid request: {e}"))))
}

fn query(parts: &axum::http::request::Parts) -> Vec<(String, String)> {
    url::form_urlencoded::parse(parts.uri.query().unwrap_or("").as_bytes())
        .into_owned()
        .collect()
}

fn q_get<'a>(q: &'a [(String, String)], key: &str) -> Option<&'a str> {
    q.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

/// A `revision` from the query or a JSON body (a DELETE may carry either).
fn revision_of(q: &[(String, String)], body: &Value) -> Option<u64> {
    body.get("revision")
        .and_then(Value::as_u64)
        .or_else(|| q_get(q, "revision").and_then(|v| v.parse().ok()))
}

/// `withPages` from the query or a JSON body.
fn with_pages(q: &[(String, String)], body: &Value) -> bool {
    body.get("withPages")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || q_get(q, "withPages").is_some_and(|v| v == "true" || v == "1")
}

pub async fn serve(
    app: &Arc<App>,
    _conn: crate::serve::Conn,
    req: Request<Body>,
    path: String,
) -> Response<Body> {
    let (parts, body) = req.into_parts();
    let Some(data_dir) = app.config.data_dir.clone() else {
        return identity::Rejection::Unavailable.into_response();
    };
    let creds = identity::creds_of(&parts);
    let layer = Arc::clone(&app.identity);
    let now = identity::now_ms();
    let (gate_path, gate_creds) = (path.clone(), creds.clone());
    let id = match tokio::task::spawn_blocking(move || {
        layer.gate_blocking(&gate_creds, &gate_path, now)
    })
    .await
    {
        Ok(Ok(id)) => id,
        Ok(Err(rejection)) => return rejection.into_response(),
        Err(_) => return identity::Rejection::Unavailable.into_response(),
    };
    let Some(target) = target(&path) else {
        return not_found();
    };
    if !target.allowed().contains(&parts.method) {
        return method_not_allowed(target.allowed());
    }
    if !ids_ok(&target) {
        return not_found();
    }
    if let Err(response) = project_routes::ensure_account(
        app,
        &data_dir,
        &creds,
        &path,
        now,
        &id.user.id,
        id.account_role == server_auth::Role::Admin,
    )
    .await
    {
        return *response;
    }
    let Ok(dir) = server_projects::store::user_dir(&data_dir, &id.user.id) else {
        return reply::error(StatusCode::INTERNAL_SERVER_ERROR, "Internal error", true);
    };
    let store = Store::new(dir).with_backing(server_specialists::Backing {
        data_dir: data_dir.clone(),
        user_id: id.user.id.clone(),
    });
    let q = query(&parts);
    let method = parts.method;
    // A client that goes away cannot cancel a write half-way: the blocking task runs to its end.
    let result = match (target, method) {
        (Target::Specialists, Method::GET) => {
            run(store, |s| {
                let l = s.list_specialists()?;
                Ok((
                    StatusCode::OK,
                    json!({"revision": l.revision, "specialists": l.specialists}),
                ))
            })
            .await
        }
        (Target::Specialists, _) => match json_body(body, SMALL_BODY).await {
            Ok(new) => {
                let new: server_specialists::NewSpecialist = new;
                let store = match with_toolboxes(app, store, new.toolboxes.as_deref()).await {
                    Ok(s) => s,
                    Err(r) => return *r,
                };
                run(store, move |s| {
                    let (revision, made) = s.create_specialist(new)?;
                    Ok((
                        StatusCode::CREATED,
                        json!({"revision": revision, "specialist": made}),
                    ))
                })
                .await
            }
            Err(r) => *r,
        },
        (Target::Specialist(id), Method::GET) => {
            run(store, move |s| {
                let (revision, v) = s.get_specialist(&id)?;
                Ok((
                    StatusCode::OK,
                    json!({"revision": revision, "specialist": v}),
                ))
            })
            .await
        }
        (Target::Specialist(id), Method::PATCH) => match json_body(body, SMALL_BODY).await {
            Ok(patch) => {
                let patch: server_specialists::SpecialistPatch = patch;
                let store = match with_toolboxes(app, store, patch.toolboxes.as_deref()).await {
                    Ok(s) => s,
                    Err(r) => return *r,
                };
                run(store, move |s| {
                    let (revision, v) = s.update_specialist(&id, patch)?;
                    Ok((
                        StatusCode::OK,
                        json!({"revision": revision, "specialist": v}),
                    ))
                })
                .await
            }
            Err(r) => *r,
        },
        (Target::Specialist(id), _) => {
            let body = optional_body(body).await;
            let Some(rev) = revision_of(&q, &body) else {
                return invalid("revision is required");
            };
            run(store, move |s| {
                let revision = s.delete_specialist(&id, rev)?;
                Ok((StatusCode::OK, json!({"ok": true, "revision": revision})))
            })
            .await
        }
        (Target::Spaces, Method::GET) => {
            run(store, |s| {
                let l = s.list_spaces()?;
                Ok((
                    StatusCode::OK,
                    json!({"revision": l.revision, "spaces": l.spaces}),
                ))
            })
            .await
        }
        (Target::Spaces, _) => match json_body(body, SMALL_BODY).await {
            Ok(new) => {
                run(store, move |s| {
                    let (revision, v) = s.create_space(new)?;
                    Ok((
                        StatusCode::CREATED,
                        json!({"revision": revision, "space": v}),
                    ))
                })
                .await
            }
            Err(r) => *r,
        },
        (Target::Space(id), Method::GET) => {
            run(store, move |s| {
                let (revision, v) = s.get_space(&id)?;
                Ok((StatusCode::OK, json!({"revision": revision, "space": v})))
            })
            .await
        }
        (Target::Space(id), Method::PATCH) => match json_body(body, SMALL_BODY).await {
            Ok(patch) => {
                run(store, move |s| {
                    let (revision, v) = s.update_space(&id, patch)?;
                    Ok((StatusCode::OK, json!({"revision": revision, "space": v})))
                })
                .await
            }
            Err(r) => *r,
        },
        (Target::Space(id), _) => {
            let body = optional_body(body).await;
            let Some(rev) = revision_of(&q, &body) else {
                return invalid("revision is required");
            };
            let with = with_pages(&q, &body);
            run(store, move |s| {
                let revision = s.delete_space(&id, rev, with)?;
                Ok((StatusCode::OK, json!({"ok": true, "revision": revision})))
            })
            .await
        }
        (Target::Pages(space), Method::GET) => {
            let limit = match q_get(&q, "limit") {
                None => None,
                Some(v) => match v.parse::<usize>() {
                    Ok(n) => Some(n),
                    Err(_) => return invalid("limit must be a number"),
                },
            };
            let offset = match q_get(&q, "offset") {
                None => 0,
                Some(v) => match v.parse::<usize>() {
                    Ok(n) => n,
                    Err(_) => return invalid("offset must be a number"),
                },
            };
            let needle = q_get(&q, "q").map(str::to_owned);
            run(store, move |s| {
                let l = s.list_pages(&space, needle.as_deref(), limit, offset)?;
                let pages: Vec<Value> = l.pages.iter().map(page_json).collect();
                Ok((
                    StatusCode::OK,
                    json!({"pages": pages, "truncated": l.truncated, "total": l.total}),
                ))
            })
            .await
        }
        (Target::Pages(space), _) => {
            #[derive(serde::Deserialize)]
            #[serde(deny_unknown_fields)]
            struct New {
                title: String,
                content: String,
            }
            match json_body::<New>(body, PAGE_BODY).await {
                Ok(new) => {
                    run(store, move |s| {
                        let w = s.create_page(&space, &new.title, &new.content, "you")?;
                        Ok((StatusCode::CREATED, json!({"page": page_json(&w.page)})))
                    })
                    .await
                }
                Err(r) => *r,
            }
        }
        (Target::Page(space, page), Method::GET) => {
            run(store, move |s| {
                let (row, content) = s.get_page(&space, &page)?;
                Ok((
                    StatusCode::OK,
                    json!({"page": page_json(&row), "content": content}),
                ))
            })
            .await
        }
        (Target::Page(space, page), Method::PUT) => {
            match json_body::<server_specialists::PagePut>(body, PAGE_BODY).await {
                Ok(put) => {
                    run(store, move |s| {
                        let w = s.put_page(&space, &page, put, "you")?;
                        let status = if w.created {
                            StatusCode::CREATED
                        } else {
                            StatusCode::OK
                        };
                        Ok((
                            status,
                            json!({"page": page_json(&w.page), "changed": w.changed}),
                        ))
                    })
                    .await
                }
                Err(r) => *r,
            }
        }
        (Target::Page(space, page), _) => {
            run(store, move |s| {
                s.delete_page(&space, &page)?;
                Ok((StatusCode::OK, json!({"ok": true})))
            })
            .await
        }
    };
    result
}

/// A DELETE's optional JSON body: `{}` when it has none or it is not an object.
async fn optional_body(body: Body) -> Value {
    match read_body(body, SMALL_BODY).await {
        Ok(bytes) if !bytes.is_empty() => serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        _ => Value::Null,
    }
}

/// The store told which toolboxes the account may give a coworker, when a request names any: the
/// catalogue's boxes except the connectors (a connector's on/off is Settings', as in core's
/// `sanitizeToolboxes`). A catalogue that is not ready is a 503 asking to retry, never a guess.
async fn with_toolboxes(
    app: &Arc<App>,
    store: Store,
    wanted: Option<&[String]>,
) -> Result<Store, Box<Response<Body>>> {
    if wanted.is_none_or(<[String]>::is_empty) {
        return Ok(store);
    }
    let Some(catalogue) = crate::mcp_discovery::read(app).await else {
        let mut res = failure(
            StatusCode::SERVICE_UNAVAILABLE,
            "The tool list is being prepared. Try again in a moment.",
            "tools_unavailable",
        );
        res.headers_mut()
            .insert("retry-after", HeaderValue::from_static("2"));
        return Err(Box::new(res));
    };
    let ids = catalogue
        .boxes
        .iter()
        .filter(|b| !catalogue.connector_boxes.contains(&b.id))
        .map(|b| b.id.clone())
        .collect();
    Ok(store.with_known_toolboxes(ids))
}

async fn run(
    store: Store,
    f: impl FnOnce(&Store) -> Result<(StatusCode, Value), Error> + Send + 'static,
) -> Response<Body> {
    match tokio::task::spawn_blocking(move || f(&store)).await {
        Ok(Ok((status, body))) => ok(status, &body),
        Ok(Err(e)) => answer(&e),
        Err(_) => reply::error(StatusCode::INTERNAL_SERVER_ERROR, "Internal error", true),
    }
}

use axum::response::IntoResponse;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_name_targets_exactly() {
        assert_eq!(target("/api/specialists"), Some(Target::Specialists));
        assert_eq!(
            target("/api/spaces/x/pages/y"),
            Some(Target::Page("x".into(), "y".into()))
        );
        for bad in [
            "/api/specialists/",
            "/api/spaces//pages",
            "/api/spaces/x/pages/y/z",
            "/api/spaces/x/other",
            "/api/specialist",
        ] {
            assert_eq!(target(bad), None, "{bad}");
        }
    }

    #[test]
    fn only_uuid_ids_pass() {
        assert!(!ids_ok(&Target::Space("..".into())));
        assert!(!ids_ok(&Target::Page(
            "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa".into(),
            "a%2fb".into()
        )));
        assert!(ids_ok(&Target::Space(
            "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa".into()
        )));
    }
}
