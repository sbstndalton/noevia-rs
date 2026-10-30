//! `GET /api/auto-roles` (sbstndalton/noevia#2190 close-out, capability `rust-auto-roles`), ported
//! from core routes/models.cjs: the Fast/Smart/Vision/Code/Creative role to model configuration, which
//! of its models the engine no longer serves, and the models chat roles' other machines serve.
//!
//! Only the GET is the front's. The PUT stays Node's: a save warms the roles in the engine
//! (`ensureRolesLoaded`), which is chat-live state. Node also remains the writer of
//! `users/<id>/auto-roles.json`, which this route reads as it is on disk (core's `readJson(file,
//! null)`: a missing, unreadable or damaged file is "not configured"). The other methods of the path
//! are Node's.
//!
//! What only Node knows is published as `auto-roles-snapshot.json` (snapshot.rs; core
//! `rust-auto-roles.cjs`): the served catalogue (each model's `name` and `missingFile`, or null when
//! the engine could not be read), the roles other machines serve (`machineRoles`, which are not checked
//! against this machine's catalogue, #1840) and the models those machines serve that this catalogue
//! does not list (`machineModels`, #1849/#1855). An unusable or stale snapshot reads as core reads an
//! engine it cannot reach: the catalogue is unknown, so nothing is reported missing and no machine
//! models are listed. Nothing old is ever shown as current.
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
    let Ok(snap) = snapshot::read(&data_dir.join(SNAPSHOT_FILE), SNAPSHOT_KIND, now_ms) else {
        return Published::default();
    };
    // Order matters for `machineModels` (core's `{ model, machine, roles }`), so the checked bytes
    // are read again with the key-order-keeping parser.
    let Ok(text) = String::from_utf8(snap.raw) else {
        return Published::default();
    };
    let Ok(doc) = wtf16::parse(&text) else {
        return Published::default();
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
                let Some(name) = row.get("name").as_str() else {
                    return Published::default();
                };
                match row.get("missingFile") {
                    JValue::Bool(false) | JValue::Null | JValue::Undefined => {
                        names.push(name.to_owned())
                    }
                    JValue::Bool(true) => {}
                    _ => return Published::default(),
                }
            }
            Some(names)
        }
        _ => return Published::default(),
    };
    let (Some(machine_roles), JValue::Arr(machine_models)) =
        (strings(doc.get("machineRoles")), doc.get("machineModels"))
    else {
        return Published::default();
    };
    Published {
        catalogue,
        machine_roles,
        machine_models: machine_models.clone(),
    }
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
fn read_roles(dir: &Path) -> JValue {
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
    if path != PATH || req.method() != Method::GET {
        return app.proxy.forward(conn.peer, req).await;
    }
    let (parts, _body) = req.into_parts();
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
    let done = tokio::task::spawn_blocking(move || {
        let dir = server_projects::store::user_dir(&data_dir, &user).ok()?;
        let roles = read_roles(&dir);
        Some(answer(roles, &published(&data_dir, identity::now_ms())))
    })
    .await;
    match done {
        Ok(Some(text)) => reply::json_text(StatusCode::OK, text, true),
        _ => reply::error(StatusCode::INTERNAL_SERVER_ERROR, "Internal error", true),
    }
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
