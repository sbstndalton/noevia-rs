//! `GET /api/auto-roles` (sbstndalton/noevia#2190 close-out, capability `rust-auto-roles`), ported
//! from core routes/models.cjs: the Fast/Smart/Vision/Code/Creative role to model configuration, which
//! of its models the engine no longer serves, and the models chat roles' other machines serve.
//!
//! Rust owns persistence for PUT and deletion cleanup. Node reads the file fresh and
//! keeps the existing engine warming trigger behind a private authenticated bridge.
//! Reads and mutations take the same per-user file lock: a read begun during a save
//! cannot publish a pre-save value after that save completes. No roles cache exists.
//!
//! What only Node knows is published as `auto-roles-snapshot.json` (snapshot.rs; core
//! `rust-auto-roles.cjs`): the served catalogue (each model's `name` and `missingFile`, or null when
//! the engine could not be read), the roles other machines serve (`machineRoles`, which are not checked
//! against this machine's catalogue, #1840) and the models those machines serve that this catalogue
//! does not list (`machineModels`, #1849/#1855). An unusable or stale snapshot reads as core reads an
//! engine it cannot reach: the catalogue is unknown, so nothing is reported missing and no machine
//! models are listed (`machineModels` is left out of the answer). Nothing old is ever shown as
//! current. Node publishes only while someone asks: this route touches the demand marker, and when
//! Node was idle it waits up to two seconds for a fresh file before answering without one.
//!
//! Order follows core: the session gate (401/403), then the answer. Like every route that reads the
//! account's directory, the first request of an account makes it (project_routes::ensure_account).

use crate::{identity, project_routes, reply, snapshot, App};
use axum::body::Body;
use axum::http::{Method, Request, Response, StatusCode};
use axum::response::IntoResponse;
use js_json::{wtf16, JValue};
use std::path::Path;
use std::sync::Arc;

pub const PATH: &str = "/api/auto-roles";
pub const SNAPSHOT_FILE: &str = "auto-roles-snapshot.json";
pub const SNAPSHOT_KIND: &str = "auto-roles";
const FILE: &str = "auto-roles.json";
/// core auto-roles-check.cjs `LABELS`, in its key order.
const ROLES: [&str; 5] = ["fast", "smart", "vision", "code", "creative"];

/// What the snapshot says: the served catalogue (None: unknown), the roles on other machines and the
/// machine models to list.
#[derive(Debug, Default, PartialEq)]
pub struct Published {
    pub catalogue: Option<Vec<String>>,
    pub machine_roles: Vec<String>,
    pub machine_models: Vec<JValue>,
}

/// The published state at `now_ms`; the default (catalogue unknown, nothing on machines) when the
/// snapshot is unusable or its shape is not the one Node writes.
pub fn published(data_dir: &Path, now_ms: i64) -> Published {
    published_checked(data_dir, now_ms).unwrap_or_default()
}

/// [`published`], `None` when the snapshot is unusable or its shape is not the one Node writes.
pub fn published_checked(data_dir: &Path, now_ms: i64) -> Option<Published> {
    let Ok(snap) = snapshot::read(&data_dir.join(SNAPSHOT_FILE), SNAPSHOT_KIND, now_ms) else {
        return None;
    };
    // Order matters for `machineModels` (core's `{ model, machine, roles }`), so the checked bytes
    // are read again with the key-order-keeping parser.
    let Ok(text) = String::from_utf8(snap.raw) else {
        return None;
    };
    let Ok(doc) = wtf16::parse(&text) else {
        return None;
    };
    let strings = |v: &JValue| -> Option<Vec<String>> {
        match v {
            JValue::Arr(items) => items
                .iter()
                .map(|i| i.as_str().map(str::to_owned))
                .collect(),
            _ => None,
        }
    };
    // `missingFile` rows are not served ("a preset whose model file is gone is listed but cannot be
    // served", #545): only the names that are served are published as such.
    let catalogue = match doc.get("catalogue") {
        JValue::Null => None,
        JValue::Arr(rows) => {
            let mut names = Vec::new();
            for row in rows {
                let name = row.get("name").as_str()?;
                match row.get("missingFile") {
                    JValue::Bool(false) | JValue::Null | JValue::Undefined => {
                        names.push(name.to_owned())
                    }
                    JValue::Bool(true) => {}
                    _ => return None,
                }
            }
            Some(names)
        }
        _ => return None,
    };
    let (Some(machine_roles), JValue::Arr(machine_models)) =
        (strings(doc.get("machineRoles")), doc.get("machineModels"))
    else {
        return None;
    };
    Some(Published {
        catalogue,
        machine_roles,
        machine_models: machine_models.clone(),
    })
}

/// core auto-roles-check.cjs `missingRoles(localRoles(roles), catalogue)`.
pub fn missing(roles: &JValue, published: &Published) -> Vec<JValue> {
    let (true, Some(names)) = (roles.truthy(), published.catalogue.as_ref()) else {
        return Vec::new();
    };
    ROLES
        .iter()
        .filter(|role| !published.machine_roles.iter().any(|m| m == **role))
        .filter_map(|role| {
            let model = roles.get(role);
            let served = model.as_str().is_some_and(|m| names.iter().any(|n| n == m));
            (model.truthy() && !served)
                .then(|| JValue::obj([("role", JValue::from(*role)), ("model", model.clone())]))
        })
        .collect()
}

/// The roles file as core's `readJson(file, null)` reads it.
pub(crate) fn read_roles(dir: &Path) -> JValue {
    std::fs::read(dir.join(FILE))
        .ok()
        .and_then(|bytes| wtf16::parse(&String::from_utf8_lossy(&bytes)).ok())
        .unwrap_or(JValue::Null)
}

/// The answer's JSON.
pub fn answer(roles: JValue, published: &Published) -> String {
    let configured = roles.truthy();
    let missing = missing(&roles, published);
    let mut items = vec![
        ("configured".to_owned(), JValue::Bool(configured)),
        (
            "roles".to_owned(),
            if configured { roles } else { JValue::Null },
        ),
        ("missing".to_owned(), JValue::Arr(missing)),
    ];
    if !published.machine_models.is_empty() {
        items.push((
            "machineModels".to_owned(),
            JValue::Arr(published.machine_models.clone()),
        ));
    }
    wtf16::stringify(&JValue::Obj(items)).unwrap_or_default()
}

pub async fn serve(
    app: &Arc<App>,
    conn: crate::serve::Conn,
    req: Request<Body>,
    path: String,
) -> Response<Body> {
    if path != PATH || !matches!(*req.method(), Method::GET | Method::PUT) {
        return app.proxy.forward(conn.peer, req).await;
    }
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
    // Node's workspace load makes the account's directory on its first request.
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
    let user = id.user.id.clone();
    if parts.method == Method::PUT {
        return put(
            app,
            &data_dir,
            &user,
            id.user.role == server_auth::Role::Admin,
            body,
        )
        .await;
    }
    // Asks Node to publish (the demand marker); when it was idle the snapshot is waited for, up to
    // two seconds, while the roles file is read.
    let published_dir = data_dir.clone();
    let state = app.demand.read(data_dir.clone(), move |now| {
        published_checked(&published_dir, now)
    });
    let state = state.await.unwrap_or_default();
    let done = tokio::task::spawn_blocking(move || {
        let dir = server_projects::store::user_dir(&data_dir, &user).ok()?;
        let _held = server_projects::lock::acquire(&dir.join(FILE)).ok()?;
        Some(answer(read_roles(&dir), &state))
    })
    .await;
    match done {
        Ok(Some(text)) => reply::json_text(StatusCode::OK, text, true),
        _ => reply::error(StatusCode::INTERNAL_SERVER_ERROR, "Internal error", true),
    }
}

const BRIDGE: &str = "/internal/auto-roles/";
async fn node(app: &App, op: &str, ask: &serde_json::Value) -> Option<JValue> {
    let (status, bytes) = app
        .config
        .research
        .node_call(
            &format!("{BRIDGE}{op}"),
            ask,
            std::time::Duration::from_secs(30),
        )
        .await?;
    if status != 200 {
        return None;
    }
    wtf16::parse(&String::from_utf8_lossy(&bytes)).ok()
}

fn bad(message: &str) -> Response<Body> {
    reply::error(StatusCode::BAD_REQUEST, message, true)
}
fn unavailable() -> Response<Body> {
    reply::error(
        StatusCode::SERVICE_UNAVAILABLE,
        "Auto-roles state is unavailable.",
        true,
    )
}

// A shallow parse keeps unknown deeply nested fields and out-of-range numbers
// from changing a valid request. Only role strings have meaning to this writer.
fn input(bytes: &[u8]) -> Result<JValue, &'static str> {
    use prompt_framing::json::Value;
    // The shared ECMAScript scanner validates the whole document while retaining
    // only the root's fields. Unknown depth, numeric overflow and lone keys are valid.
    let doc = prompt_framing::json::parse_utf8(bytes, 1).ok_or("invalid JSON")?;
    if !matches!(doc, Value::Obj(_)) {
        return Err("request body must be a JSON object");
    }
    Ok(JValue::Obj(
        ROLES
            .iter()
            .filter_map(|key| {
                doc.get(key).map(|v| {
                    let value = match v {
                        Value::Str(units) => JValue::Str(
                            char::decode_utf16(units.iter().copied())
                                .map(|c| {
                                    c.unwrap_or_else(|e| {
                                        wtf16::lone_char(e.unpaired_surrogate())
                                            .unwrap_or(char::REPLACEMENT_CHARACTER)
                                    })
                                })
                                .collect(),
                        ),
                        _ => JValue::Null,
                    };
                    ((*key).to_owned(), value)
                })
            })
            .collect(),
    ))
}

fn prepared(
    body: &JValue,
    previous: &JValue,
    catalogue: &JValue,
    section_roles: &std::collections::HashMap<String, String>,
) -> Result<JValue, String> {
    let mut items = Vec::new();
    for role in ROLES {
        let value = body.get(role);
        let name = match value {
            JValue::Str(s) => s.trim_matches(crate::mcp_broker::js_ws).to_owned(),
            JValue::Undefined if role == "creative" => {
                previous.get(role).as_str().unwrap_or("").to_owned()
            }
            _ => String::new(),
        };
        if !name.is_empty() || role == "fast" || role == "smart" {
            items.push((role.to_owned(), JValue::from(name)));
        }
    }
    let roles = JValue::Obj(items);
    if !roles.get("fast").truthy() || !roles.get("smart").truthy() {
        return Err("both fast and smart model names are required".into());
    }
    let rows = match catalogue {
        JValue::Arr(rows) => rows.as_slice(),
        _ => &[],
    };
    if matches!(catalogue, JValue::Arr(_)) {
        let unknown: Vec<_> = ROLES
            .iter()
            .filter_map(|role| roles.get(role).as_str())
            .filter(|name| !rows.iter().any(|r| r.get("name").as_str() == Some(*name)))
            .collect();
        if !unknown.is_empty() {
            return Err(format!("unknown model(s): {}", unknown.join(", ")));
        }
    }
    for role in ROLES {
        let Some(name) = roles.get(role).as_str() else {
            continue;
        };
        let labels = rows
            .iter()
            .find(|r| r.get("name").as_str() == Some(name))
            .and_then(|r| match r.get("labels") {
                JValue::Arr(labels) => Some(
                    labels
                        .iter()
                        .filter_map(|v| js_json::to_js_string(v).ok())
                        .collect::<Vec<_>>(),
                ),
                _ => None,
            });
        if !crate::model_kind::is_chat_generation_model(section_roles, name, labels.as_deref()) {
            return Err(format!("The {role} role needs a chat model; {name} is an embedding, reranking or routing model."));
        }
    }
    Ok(roles)
}

fn persist(dir: &Path, roles: &JValue) -> Result<(), ()> {
    // write_owned_text requires an existing directory; it never revives a deleted account.
    let text = wtf16::stringify_pretty(roles).ok_or(())?;
    server_store::json::write_auto_roles(dir, &text).map_err(|_| ())
}

async fn put(
    app: &Arc<App>,
    data_dir: &Path,
    user: &str,
    admin: bool,
    body: Body,
) -> Response<Body> {
    let bytes = match tokio::time::timeout(
        std::time::Duration::from_secs(10),
        axum::body::to_bytes(body, 1024 * 1024),
    )
    .await
    {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(e)) => {
            return if e.into_inner().is::<http_body_util::LengthLimitError>() {
                reply::error(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "Request exceeds size limit",
                    true,
                )
            } else {
                bad("invalid JSON")
            }
        }
        _ => return bad("invalid JSON"),
    };
    let body = match input(&bytes) {
        Ok(body) => body,
        Err(e) => return bad(e),
    };
    if ["fast", "smart"].iter().any(|k| {
        body.get(k)
            .as_str()
            .is_none_or(|s| s.trim_matches(crate::mcp_broker::js_ws).is_empty())
    }) {
        return bad("both fast and smart model names are required");
    }
    let (app, data_dir, user) = (Arc::clone(app), data_dir.to_path_buf(), user.to_owned());
    // A client disconnect cannot cancel a partially committed save or its final fence.
    tokio::spawn(async move { save(&app, &data_dir, &user, admin, body).await })
        .await
        .unwrap_or_else(|_| unavailable())
}
async fn save(
    app: &Arc<App>,
    data_dir: &Path,
    user: &str,
    admin: bool,
    body: JValue,
) -> Response<Body> {
    let Ok(dir) = server_projects::store::user_dir(data_dir, user) else {
        return unavailable();
    };
    let lock_path = dir.join(FILE);
    let held = match tokio::task::spawn_blocking(move || server_projects::lock::acquire(&lock_path))
        .await
    {
        Ok(Ok(held)) => held,
        _ => return unavailable(),
    };
    // The lock is held through both invalidations. Every native read and cleanup
    // takes this same user fence; Node has no snapshot of per-user roles to republish.
    if node(app, "invalidate", &serde_json::json!({}))
        .await
        .is_none()
    {
        return unavailable();
    }
    let response = async {
        let facts = match node(app, "facts", &serde_json::json!({})).await {
            Some(f) => f,
            None => return unavailable(),
        };
        let read_dir = dir.clone();
        let previous = match tokio::task::spawn_blocking(move || read_roles(&read_dir)).await {
            Ok(r) => r,
            _ => return unavailable(),
        };
        let section_roles = crate::model_kind::roles_beside(app.config.sizing.presets_path());
        let roles = match prepared(&body, &previous, facts.get("catalogue"), &section_roles) {
            Ok(r) => r,
            Err(e) => return bad(&e),
        };
        let saved = roles.clone();
        if !matches!(
            tokio::task::spawn_blocking(move || persist(&dir, &saved)).await,
            Ok(Ok(()))
        ) {
            return unavailable();
        }
        if admin {
            let _ = node(app, "warm", &serde_json::json!({"user":user})).await;
        }
        reply::json_text(
            StatusCode::OK,
            wtf16::stringify(&JValue::obj([
                ("configured", JValue::Bool(true)),
                ("roles", roles),
            ]))
            .unwrap_or_default(),
            true,
        )
    }
    .await;
    let invalidated = node(app, "invalidate", &serde_json::json!({}))
        .await
        .is_some();
    drop(held);
    if invalidated {
        response
    } else {
        unavailable()
    }
}

/// Deletion cleanup, under the same per-user lock as PUT. Scan disk so dormant
/// accounts are included; return only the requesting user's cleared roles.
fn clear_all(data_dir: &Path, user: &str, name: &str) -> Result<Vec<String>, ()> {
    let mut cleared = Vec::new();
    for entry in std::fs::read_dir(data_dir.join("users")).map_err(|_| ())? {
        let entry = entry.map_err(|_| ())?;
        let id = entry.file_name().to_string_lossy().into_owned();
        let Ok(dir) = server_projects::store::user_dir(data_dir, &id) else {
            continue;
        };
        if !entry.file_type().map_err(|_| ())?.is_dir() {
            continue;
        }
        let _held = server_projects::lock::acquire(&dir.join(FILE)).map_err(|_| ())?;
        let JValue::Obj(mut roles) = read_roles(&dir) else {
            continue;
        };
        let mut changed = Vec::new();
        for role in ROLES {
            if !roles
                .iter()
                .any(|(k, v)| k == role && v.as_str() == Some(name))
            {
                continue;
            }
            changed.push(role.to_owned());
            if matches!(role, "fast" | "smart") {
                let other = if role == "fast" { "smart" } else { "fast" };
                let fallback = roles
                    .iter()
                    .find(|(k, v)| k == other && v.truthy() && v.as_str() != Some(name))
                    .map(|(_, v)| v.clone())
                    .unwrap_or(JValue::from(""));
                wtf16::set(&mut roles, role, fallback);
            } else {
                roles.retain(|(k, _)| k != role);
            }
        }
        if changed.is_empty() {
            continue;
        }
        let value = JValue::Obj(roles);
        let value =
            if value.get("fast").as_str() == Some("") && value.get("smart").as_str() == Some("") {
                JValue::Null
            } else {
                value
            };
        persist(&dir, &value)?;
        if id == user {
            cleared = changed;
        }
    }
    Ok(cleared)
}

pub fn store_route(store: crate::project_store::Store, app: Option<Arc<App>>) -> axum::Router {
    let proof_store = store.clone();
    axum::Router::new()
        .route(
            "/v1/auto-roles/proof",
            axum::routing::post(move |headers: axum::http::HeaderMap| {
                let store = proof_store.clone();
                async move {
                    if !crate::project_store::authorised(&store, &headers) {
                        return reply::error(StatusCode::UNAUTHORIZED, "unauthorized", true);
                    }
                    reply::json_text(
                        StatusCode::OK,
                        r#"{"capability":"rust-auto-roles-write"}"#.to_owned(),
                        true,
                    )
                }
            }),
        )
        .route(
            "/v1/auto-roles/clear",
            axum::routing::post(move |headers: axum::http::HeaderMap, raw: bytes::Bytes| {
                let store = store.clone();
                let app = app.clone();
                async move {
                    if !crate::project_store::authorised(&store, &headers) {
                        return reply::error(StatusCode::UNAUTHORIZED, "unauthorized", true);
                    }
                    let ask = match input_clear(&raw) {
                        Some(v) => v,
                        None => return bad("Invalid auto-roles operation."),
                    };
                    let Some(app) = app else {
                        return unavailable();
                    };
                    if node(&app, "invalidate", &serde_json::json!({}))
                        .await
                        .is_none()
                    {
                        return unavailable();
                    }
                    let done = tokio::task::spawn_blocking(move || {
                        clear_all(&store.data_dir, &ask.0, &ask.1)
                    })
                    .await;
                    if node(&app, "invalidate", &serde_json::json!({}))
                        .await
                        .is_none()
                    {
                        return unavailable();
                    }
                    match done {
                        Ok(Ok(cleared)) => reply::json_text(
                            StatusCode::OK,
                            serde_json::json!({"cleared":cleared}).to_string(),
                            true,
                        ),
                        _ => unavailable(),
                    }
                }
            }),
        )
}
fn input_clear(raw: &[u8]) -> Option<(String, String)> {
    let ask = wtf16::parse(&String::from_utf8_lossy(raw)).ok()?;
    let user = ask.get("user").as_str()?;
    let name = ask.get("name").as_str()?;
    if server_projects::store::user_dir(Path::new("."), user).is_err() || name.is_empty() {
        return None;
    }
    Some((user.to_owned(), name.to_owned()))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn roles(text: &str) -> JValue {
        wtf16::parse(text).unwrap()
    }

    fn published_of(names: &[&str], machine_roles: &[&str]) -> Published {
        Published {
            catalogue: Some(names.iter().map(|n| (*n).to_owned()).collect()),
            machine_roles: machine_roles.iter().map(|n| (*n).to_owned()).collect(),
            machine_models: Vec::new(),
        }
    }

    #[test]
    fn a_role_the_catalogue_does_not_serve_is_missing_in_core_order() {
        let p = published_of(&["a", "b"], &[]);
        let r = roles(r#"{"smart":"gone","fast":"a","creative":"x","code":"b"}"#);
        assert_eq!(
            wtf16::stringify(&JValue::Arr(missing(&r, &p))).unwrap(),
            r#"[{"role":"smart","model":"gone"},{"role":"creative","model":"x"}]"#
        );
    }

    #[test]
    fn a_role_on_a_machine_an_unknown_catalogue_and_no_roles_report_nothing() {
        let r = roles(r#"{"fast":"a","smart":"gone"}"#);
        assert!(missing(&r, &published_of(&["a"], &["smart"])).is_empty());
        assert!(missing(&r, &Published::default()).is_empty());
        assert!(missing(&JValue::Null, &published_of(&[], &[])).is_empty());
        // A model that is not a string is never served.
        let odd = roles(r#"{"fast":5,"smart":""}"#);
        assert_eq!(missing(&odd, &published_of(&["5"], &[])).len(), 1);
    }

    #[test]
    fn the_answer_has_cores_shape() {
        let mut p = published_of(&["a"], &[]);
        assert_eq!(
            answer(roles(r#"{"fast":"a","smart":"a"}"#), &p),
            r#"{"configured":true,"roles":{"fast":"a","smart":"a"},"missing":[]}"#
        );
        assert_eq!(
            answer(JValue::Null, &p),
            r#"{"configured":false,"roles":null,"missing":[]}"#
        );
        // Anything falsy is "not configured" and answers `roles: null`.
        assert_eq!(
            answer(roles("0"), &p),
            r#"{"configured":false,"roles":null,"missing":[]}"#
        );
        p.machine_models = vec![roles(
            r#"{"model":"m","machine":{"id":"i","name":"n"},"roles":["fast"]}"#,
        )];
        assert_eq!(
            answer(roles(r#"{"fast":"a","smart":"b"}"#), &p),
            r#"{"configured":true,"roles":{"fast":"a","smart":"b"},"missing":[{"role":"smart","model":"b"}],"machineModels":[{"model":"m","machine":{"id":"i","name":"n"},"roles":["fast"]}]}"#
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod writer_tests {
    use super::*;
    #[test]
    fn cleanup_includes_dormant_users_falls_back_and_unsets_without_changing_other_roles() {
        let root = tempfile::tempdir().unwrap();
        let a = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
        let b = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
        let c = "cccccccc-cccc-4ccc-8ccc-cccccccccccc";
        for (user, text) in [
            (
                a,
                r#"{"fast":"gone","smart":"stay","vision":"gone","code":"stay","creative":"gone"}"#,
            ),
            (b, r#"{"fast":"gone","smart":"gone"}"#),
            (c, r#"{"fast":"stay","smart":"stay"}"#),
        ] {
            let dir = root.path().join("users").join(user);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(FILE), text).unwrap();
        }
        assert_eq!(
            clear_all(root.path(), a, "gone").unwrap(),
            vec!["fast", "vision", "creative"]
        );
        assert_eq!(
            read_roles(&root.path().join("users").join(a)),
            wtf16::parse(r#"{"fast":"stay","smart":"stay","code":"stay"}"#).unwrap()
        );
        assert_eq!(read_roles(&root.path().join("users").join(b)), JValue::Null);
        assert!(clear_all(root.path(), a, "gone").unwrap().is_empty());
        assert_eq!(
            std::fs::read_to_string(root.path().join("users").join(c).join(FILE)).unwrap(),
            r#"{"fast":"stay","smart":"stay"}"#
        );
    }
    #[test]
    fn every_role_requires_chat_kind_even_when_unknown_names_are_permitted() {
        let rows =
            wtf16::parse(r#"[{"name":"a"},{"name":"vectors","labels":["embedding"]}]"#).unwrap();
        for role in ROLES {
            let body = JValue::obj([("fast", JValue::from("a")), ("smart", JValue::from("a"))]);
            let JValue::Obj(mut body) = body else {
                return;
            };
            wtf16::set(&mut body, role, JValue::from("vectors"));
            assert_eq!(prepared(&JValue::Obj(body),&JValue::Null,&rows,&Default::default()).unwrap_err(),format!("The {role} role needs a chat model; vectors is an embedding, reranking or routing model."));
        }
        let body = wtf16::parse(r#"{"fast":"embedding-synthetic","smart":"a"}"#).unwrap();
        assert!(prepared(&body, &JValue::Null, &JValue::Null, &Default::default()).is_err());
        let body = wtf16::parse(r#"{"fast":"a","smart":"a","vision":"a","code":"a"}"#).unwrap();
        assert!(prepared(&body, &JValue::Null, &rows, &Default::default()).is_ok());
    }
}
