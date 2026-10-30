//! `GET /api/connectors` and `PUT /api/connectors/nextcloud/policy` (sbstndalton/noevia#2202 round
//! 2), moved from core `routes/connectors.cjs`. Capability `rust-connectors`; core's
//! `rust-connectors.cjs` refuses them with `RUST_CONNECTORS_OWNED`. Settings -> Connectors, for every
//! signed-in account (each sees and changes only its own).
//!
//! The list has two connectors. Google Drive's half is `connector_routes::view` (this front's since
//! round 7). Nextcloud has no connect button of its own: its tools use the account's storage
//! connection (Settings -> Diary & storage), so this page says whether that credential can be used
//! (the account's `storage_connections` row, and the origin allow-list the operator declared in
//! `MCP_NEXTCLOUD_ORIGINS`, which Node publishes with the catalogue) and owns what each of its tools
//! may do (`tool_policies`, written here). Which tools the Nextcloud MCP server offers, and which of
//! them write, is the one thing only Node's live discovery knows: it comes from the snapshot
//! (mcp_discovery.rs). When that is not ready the page still lists Drive, and says why Nextcloud is
//! missing; a policy write waits for it (the tool names are checked against what is offered).
use crate::connector_routes::{
    answer, failed, mode_of, read_object, set_policy, view, wanted_tools,
};
use crate::gdrive::Fail;
use crate::mcp_discovery::Catalogue;
use crate::{identity, App};
use axum::body::Body;
use axum::http::{Request, Response};
use axum::response::IntoResponse;
use js_json::JValue;
use server_storage_reads::{owner_secret, protocol::Kind};
use std::sync::Arc;

const UNAVAILABLE: &str = "Google Drive is unavailable right now. Try again in a moment.";
const PREPARING: &str = "The Nextcloud tools are being prepared. Try again in a moment.";
const POLICY_UNSAVED: &str = "The tool permission could not be saved. Try again in a moment.";

fn s(v: &str) -> JValue {
    JValue::Str(v.to_owned())
}

/// The account's storage connection, as far as Nextcloud's tools care.
enum Storage {
    /// A Nextcloud or WebDAV connection with a user name and a credential that opens.
    Connected {
        username: String,
        base_url: String,
    },
    None,
}

fn storage_of(app: &App, identity: &server_auth::Identity) -> Storage {
    let Some(data_dir) = app.config.data_dir.as_ref() else {
        return Storage::None;
    };
    let previous = std::env::var("SECRETS_KEY_PREVIOUS")
        .ok()
        .map(zeroize::Zeroizing::new);
    let keys =
        owner_secret::load_existing_keys(data_dir, previous.as_deref().map(String::as_str)).ok();
    let Ok(store) = app.identity.store_blocking() else {
        return Storage::None;
    };
    let Ok(projected) = owner_secret::project_connection_with(&store, identity, keys.as_ref())
    else {
        return Storage::None;
    };
    let c = projected.connection();
    if matches!(c.kind, Kind::Nextcloud | Kind::WebDav)
        && !c.username.is_empty()
        && !c.secret.is_empty()
    {
        Storage::Connected {
            username: c.username.clone(),
            base_url: c.base_url.clone(),
        }
    } else {
        Storage::None
    }
}

/// core `createCredentialOriginCheck`: the operator's declared origins of the one Nextcloud the MCP
/// server talks to. A credential is forwarded only to one of them.
fn origin_allowed(origins: &[String], base_url: &str) -> bool {
    url::Url::parse(base_url)
        .map(|u| u.origin().ascii_serialization())
        .is_ok_and(|origin| origins.contains(&origin))
}

/// The Nextcloud connector object for one account (core's `nextcloudView`).
fn nextcloud_view(
    app: &Arc<App>,
    identity: &server_auth::Identity,
    cat: Option<&Catalogue>,
) -> JValue {
    let user = identity.user.id.as_str();
    let Some(cat) = cat else {
        return JValue::obj([
            ("id", s("nextcloud")),
            ("name", s("Nextcloud")),
            ("configured", JValue::Bool(true)),
            ("state", s("error")),
            ("account", JValue::Null),
            ("baseUrl", JValue::Null),
            ("message", s(PREPARING)),
            ("boxes", JValue::Arr(Vec::new())),
            ("tools", JValue::Arr(Vec::new())),
        ]);
    };
    let (configured, state, account, base_url, message): (
        bool,
        &str,
        Option<String>,
        Option<String>,
        String,
    ) = if !cat.nextcloud_configured {
        (
                false,
                "not-configured",
                None,
                None,
                "This server has no Nextcloud MCP server configured, so there are no Nextcloud tools to offer.".into(),
            )
    } else {
        match storage_of(app, identity) {
                Storage::None => (
                    true,
                    "disconnected",
                    None,
                    None,
                    "Connect Nextcloud under Settings → Diary & storage; its tools then use that same connection.".into(),
                ),
                Storage::Connected { username, base_url } => {
                    if origin_allowed(&cat.nextcloud_origins, &base_url) {
                        (true, "connected", Some(username), Some(base_url), String::new())
                    } else {
                        (
                            true,
                            "error",
                            Some(username),
                            Some(base_url),
                            "Your Nextcloud address is not on this server's allowed list (MCP_NEXTCLOUD_ORIGINS), so noevia will not send your credential to it.".into(),
                        )
                    }
                }
            }
    };
    let boxes: Vec<&crate::mcp_discovery::BoxInfo> = cat
        .boxes
        .iter()
        .filter(|b| b.server.as_deref() == Some("nextcloud"))
        .collect();
    JValue::obj([
        ("id", s("nextcloud")),
        ("name", s("Nextcloud")),
        ("configured", JValue::Bool(configured)),
        ("state", s(state)),
        ("account", account.as_deref().map_or(JValue::Null, s)),
        ("baseUrl", base_url.as_deref().map_or(JValue::Null, s)),
        ("message", s(&message)),
        (
            "boxes",
            if configured {
                JValue::Arr(
                    boxes
                        .iter()
                        .map(|b| {
                            JValue::obj([
                                ("id", s(&b.id)),
                                ("label", s(&b.label)),
                                ("toolCount", JValue::Num(b.tools.len() as f64)),
                            ])
                        })
                        .collect(),
                )
            } else {
                JValue::Arr(Vec::new())
            },
        ),
        (
            "tools",
            JValue::Arr(
                boxes
                    .iter()
                    .flat_map(|b| b.tools.iter())
                    .map(|t| {
                        JValue::obj([
                            ("name", s(&t.name)),
                            ("label", s(&t.name)),
                            ("write", JValue::Bool(t.write)),
                            ("mode", s(mode_of(app, user, &t.name, t.write))),
                        ])
                    })
                    .collect(),
            ),
        ),
    ])
}

/// Google Drive's half, or what core showed when its broker could not answer: the page still lists
/// Nextcloud, and says why this one is missing.
async fn drive_view(app: &Arc<App>, user: &str, admin: bool) -> JValue {
    match view(app, user, admin).await {
        Ok(v) => v,
        Err(_) => JValue::obj([
            ("id", s("gdrive")),
            ("name", s("Google Drive")),
            ("configured", JValue::Bool(true)),
            ("state", s("error")),
            ("email", JValue::Null),
            ("connectedAt", JValue::Null),
            ("message", s(UNAVAILABLE)),
            ("backup", JValue::Null),
            ("tools", JValue::Arr(Vec::new())),
        ]),
    }
}

pub async fn serve(app: &Arc<App>, req: Request<Body>, path: String) -> Response<Body> {
    let put = req.method() == axum::http::Method::PUT;
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
    let id = Arc::new(id);
    let admin = id.user.role == server_auth::Role::Admin;
    let user = id.user.id.clone();
    let catalogue = crate::mcp_discovery::read(app).await;
    if !put {
        let drive = drive_view(app, &user, admin).await;
        let (a, i) = (Arc::clone(app), Arc::clone(&id));
        let nextcloud =
            tokio::task::spawn_blocking(move || nextcloud_view(&a, &i, catalogue.as_deref()))
                .await
                .unwrap_or(JValue::Null);
        return answer(
            200,
            &JValue::obj([("connectors", JValue::Arr(vec![drive, nextcloud]))]),
        );
    }
    // PUT /api/connectors/nextcloud/policy
    let Some(cat) = catalogue else {
        return answer(503, &JValue::obj([("error", s(PREPARING))]));
    };
    let body = match read_object(body).await {
        Ok(b) => b,
        Err(refusal) => return *refusal,
    };
    let offered: Vec<(&str, bool)> = cat
        .boxes
        .iter()
        .filter(|b| b.server.as_deref() == Some("nextcloud"))
        .flat_map(|b| b.tools.iter())
        .map(|t| (t.name.as_str(), t.write))
        .collect();
    let tools = wanted_tools(&body, |t| offered.iter().any(|(n, _)| *n == t));
    let writes: Vec<bool> = tools
        .iter()
        .map(|t| offered.iter().any(|(n, w)| n == t && *w))
        .collect();
    let (a, u, mode) = (Arc::clone(app), user, body.get("mode").clone());
    let saved = tokio::task::spawn_blocking(move || set_policy(&a, &u, &tools, &mode, &writes))
        .await
        .unwrap_or_else(|_| Err(Fail::new(POLICY_UNSAVED, 502)));
    if let Err(e) = saved {
        return failed(&e);
    }
    let (a, i) = (Arc::clone(app), Arc::clone(&id));
    let view = tokio::task::spawn_blocking(move || nextcloud_view(&a, &i, Some(cat.as_ref())))
        .await
        .unwrap_or(JValue::Null);
    answer(200, &view)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn a_credential_goes_only_to_an_origin_the_operator_declared() {
        let origins = vec![
            "https://cloud.example.test".to_owned(),
            "http://10.0.0.5:8080".to_owned(),
        ];
        assert!(origin_allowed(
            &origins,
            "https://cloud.example.test/remote.php/dav"
        ));
        assert!(
            origin_allowed(&origins, "https://cloud.example.test:443/x"),
            "the default port is the origin"
        );
        assert!(origin_allowed(&origins, "http://10.0.0.5:8080/"));
        for no in [
            "https://evil.example.test/",
            "http://cloud.example.test/",
            "https://cloud.example.test.evil.test/",
            "not a url",
            "",
        ] {
            assert!(!origin_allowed(&origins, no), "{no}");
        }
        assert!(
            !origin_allowed(&[], "https://cloud.example.test/"),
            "no declared origin, no pass-through"
        );
    }
}
