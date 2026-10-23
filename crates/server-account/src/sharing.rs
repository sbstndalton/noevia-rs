//! core routes/auth.cjs `GET` and `PUT /api/profile/sharing` over dav-settings.cjs (Settings ->
//! Sharing): whether the operator configured a file-sharing endpoint, the account's opt-in
//! scope, and whether it may opt in at all (Diary on, server-held local storage).
//!
//! Every input is persisted state or the deployment environment: `dav_settings` (created by core
//! dav-settings.cjs at boot, read here per request), `user_features.diary_enabled`,
//! `storage_connections.kind` and the COWORK_DAV_* variables. Differences from Node, all
//! deliberate (sbstndalton/noevia#1071):
//!
//! - the public address a `public` endpoint without COWORK_DAV_ORIGIN uses is read on every
//!   request (settings, then PUBLIC_ORIGIN, then setup), where Node computed it once at boot
//!   (sbstndalton/noevia#2143);
//! - an invalid COWORK_DAV_* environment answers 500 here (Node refuses to start);
//! - a failed save answers 500 `Internal error`, where Node answered 400 with the database's own
//!   message; the row and its audit event are written in one transaction (sbstndalton/noevia#2142);
//! - a body over the size limit keeps its 413 (Node's catch turned it into a 400).

use crate::users::audit;
use crate::{read_object, Account, Fault, Outcome, Reply, Request};
use js_json::JValue;
use server_auth::Identity;
use server_store::Reader;

/// The COWORK_DAV_* environment as read at startup. The proxy token itself is never kept: only
/// whether it has the shape dav-settings.cjs requires.
#[derive(Debug, Clone, Default)]
pub struct DavEnv {
    /// `Number(COWORK_DAV_PORT || 0)`.
    pub port: f64,
    /// `Number(UI_PORT || 8021)`.
    pub ui_port: f64,
    /// COWORK_DAV_SCOPE.
    pub scope: Option<String>,
    /// COWORK_DAV_ORIGIN.
    pub origin: Option<String>,
    /// COWORK_DAV_PROXY_TOKEN matches `^[a-f0-9]{64,128}$`.
    pub proxy_token_ok: bool,
    /// COWORK_DAV_HOST.
    pub host: Option<String>,
}

impl DavEnv {
    pub fn from_lookup(get: &impl Fn(&str) -> Option<String>) -> Self {
        let non_empty = |k: &str| get(k).filter(|v| !v.is_empty());
        let token = get("COWORK_DAV_PROXY_TOKEN").unwrap_or_default();
        DavEnv {
            port: crate::util::js_number(&non_empty("COWORK_DAV_PORT").unwrap_or_default()),
            ui_port: non_empty("UI_PORT").map_or(8021.0, |v| crate::util::js_number(&v)),
            scope: get("COWORK_DAV_SCOPE"),
            origin: non_empty("COWORK_DAV_ORIGIN"),
            proxy_token_ok: (64..=128).contains(&token.len())
                && token
                    .bytes()
                    .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')),
            host: non_empty("COWORK_DAV_HOST"),
        }
    }
}

/// dav-settings.cjs `configuration(env, publicOrigin)`, the fields the route reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DavConfig {
    NotConfigured,
    Available {
        port: u16,
        scope: String,
        origin: String,
        cleartext: bool,
    },
}

const NOT_CONFIGURED: &str = "The operator has not configured a file-sharing endpoint.";

/// dav-settings.cjs `configuration`: `Err` where it throws (Node then refuses to start).
pub fn configuration(env: &DavEnv, public_origin: &str) -> Result<DavConfig, String> {
    let port = env.port;
    // `if (!port)`: 0 and NaN are "not configured".
    if port == 0.0 || port.is_nan() {
        return Ok(DavConfig::NotConfigured);
    }
    if port.fract() != 0.0 || !(1024.0..=65535.0).contains(&port) || port == env.ui_port {
        return Err("Invalid separate COWORK_DAV_PORT".into());
    }
    let scope = env.scope.clone().unwrap_or_default();
    if scope != "lan" && scope != "public" {
        return Err("Set COWORK_DAV_SCOPE to lan or public".into());
    }
    let raw = match &env.origin {
        Some(o) => o.clone(),
        None if scope == "public" => public_origin.to_string(),
        None => String::new(),
    };
    let url = url::Url::parse(&raw).map_err(|_| "Configure COWORK_DAV_ORIGIN".to_string())?;
    let clean = matches!(url.scheme(), "http" | "https")
        && url.username().is_empty()
        && url.password().is_none()
        && url.path() == "/"
        && url.query().is_none_or(str::is_empty)
        && url.fragment().is_none_or(str::is_empty);
    if !clean {
        return Err("COWORK_DAV_ORIGIN must be a clean HTTP(S) origin".into());
    }
    let origin = url.origin().ascii_serialization();
    if scope == "public" && url.scheme() != "https" {
        return Err("Public DAV requires HTTPS".into());
    }
    if scope == "lan" && !public_origin.is_empty() {
        let public = url::Url::parse(public_origin)
            .map_err(|_| "Invalid public origin".to_string())?
            .origin()
            .ascii_serialization();
        if public == origin {
            return Err("LAN DAV requires an authority separate from the public app origin".into());
        }
    }
    if url.scheme() == "https" && !env.proxy_token_ok {
        return Err("HTTPS DAV requires a 32-byte or longer hex COWORK_DAV_PROXY_TOKEN".into());
    }
    if let Some(host) = &env.host {
        if host.parse::<std::net::IpAddr>().is_err() {
            return Err("COWORK_DAV_HOST must be an IP address".into());
        }
    }
    Ok(DavConfig::Available {
        port: port as u16,
        scope,
        origin,
        cleartext: url.scheme() == "http",
    })
}

/// JavaScript `encodeURIComponent` for a string without lone surrogates.
fn encode_uri_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&b) {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// dav-settings.cjs `scope(userId)`: 'off' without a row (or before core created the table).
fn scope(r: &Reader<'_>, uid: &str) -> Result<String, Fault> {
    if !table_exists(r)? {
        return Ok("off".into());
    }
    Ok(r.row(
        "SELECT scope FROM dav_settings WHERE user_id=?1",
        [uid],
        |row| row.get::<_, Option<String>>(0),
    )?
    .flatten()
    .filter(|s| !s.is_empty())
    .unwrap_or_else(|| "off".into()))
}

fn table_exists(r: &Reader<'_>) -> Result<bool, Fault> {
    Ok(r.row(
        "SELECT 1 FROM sqlite_master WHERE type='table' AND name='dav_settings'",
        [],
        |row| row.get::<_, i64>(0),
    )?
    .is_some())
}

/// `auth.getStorage(userId).kind === 'local'` (no row is local). Only the kind is read: the
/// secret is never decrypted here.
fn local_storage(r: &Reader<'_>, uid: &str) -> Result<bool, Fault> {
    let kind = r.row(
        "SELECT kind FROM storage_connections WHERE user_id=?1",
        [uid],
        |row| row.get::<_, Option<String>>(0),
    )?;
    Ok(match kind {
        None => true,
        Some(k) => k.as_deref() == Some("local"),
    })
}

struct View {
    config: DavConfig,
    scope: String,
    eligible: bool,
}

fn view(acct: &Account, authn: &Identity) -> Result<View, Fault> {
    let uid = authn.user.id.as_str();
    let (origin, scope, local) = acct
        .writer
        .read(|r| Ok((acct.origin(r), scope(r, uid), local_storage(r, uid))))?;
    let config = configuration(&acct.settings.dav, &origin?).map_err(|_| Fault::Internal)?;
    Ok(View {
        config,
        scope: scope?,
        eligible: authn.user.diary_enabled && local?,
    })
}

/// dav-settings.cjs `get(user)`; `undefined` fields are left out, as JSON.stringify does.
fn body(v: &View, username: &str) -> JValue {
    let mut items: Vec<(String, JValue)> = Vec::new();
    match &v.config {
        DavConfig::NotConfigured => {
            items.push(("available".into(), JValue::from(false)));
            items.push(("reasonId".into(), JValue::from("not-configured")));
            items.push(("reason".into(), JValue::from(NOT_CONFIGURED)));
            items.push(("scope".into(), JValue::from(v.scope.as_str())));
            items.push(("cleartext".into(), JValue::from(false)));
            items.push(("url".into(), JValue::from("")));
        }
        DavConfig::Available {
            port,
            scope,
            origin,
            cleartext,
        } => {
            items.push(("available".into(), JValue::from(true)));
            items.push(("scope".into(), JValue::from(v.scope.as_str())));
            items.push(("endpointScope".into(), JValue::from(scope.as_str())));
            items.push(("cleartext".into(), JValue::from(*cleartext)));
            items.push(("port".into(), JValue::from(f64::from(*port))));
            items.push((
                "url".into(),
                JValue::from(format!("{origin}/dav/{}/", encode_uri_component(username))),
            ));
        }
    }
    items.push(("eligible".into(), JValue::from(v.eligible)));
    JValue::Obj(items)
}

pub fn sharing(
    acct: &Account,
    req: &Request,
    authn: &Identity,
    now: i64,
) -> Result<Option<Outcome>, Fault> {
    if req.path != "/api/profile/sharing" {
        return Ok(None);
    }
    let reply = |r: Reply| Ok(Some(Outcome::Reply(r)));
    match req.method.as_str() {
        "GET" => {
            let v = view(acct, authn)?;
            reply(Reply::json(200, &body(&v, &authn.user.username)))
        }
        "PUT" => {
            let value = read_object(req)?;
            let v = view(acct, authn)?;
            let wanted = match value.get("scope").as_str() {
                Some(s @ ("off" | "lan" | "public")) => s.to_string(),
                _ => return reply(Reply::error(400, "Choose a sharing scope.")),
            };
            let acknowledged = matches!(value.get("acknowledgeCleartext"), JValue::Bool(true));
            if wanted != "off" {
                let (configured, cleartext) = match &v.config {
                    DavConfig::Available {
                        scope, cleartext, ..
                    } => (*scope == wanted, *cleartext),
                    DavConfig::NotConfigured => (false, false),
                };
                if !configured {
                    return reply(Reply::error(
                        400,
                        "This sharing scope is not configured by the operator.",
                    ));
                }
                if !v.eligible {
                    return reply(Reply::error(
                        400,
                        "Sharing requires enabled Diary with server-held local storage.",
                    ));
                }
                if cleartext && !acknowledged {
                    return reply(Reply::error(
                        400,
                        "Acknowledge that plain HTTP sends device credentials unencrypted.",
                    ));
                }
            }
            let uid = authn.user.id.as_str();
            if !acct.writer.read(|r| Ok(table_exists(r)))?? {
                // Core creates the table at boot; until it has, there is nowhere to save.
                return Err(Fault::Unavailable);
            }
            acct.writer.write(|tx| {
                tx.execute(
                    "INSERT INTO dav_settings(user_id,scope,acknowledged_at,updated_at) VALUES(?1,?2,?3,?4) ON CONFLICT(user_id) DO UPDATE SET scope=excluded.scope,acknowledged_at=excluded.acknowledged_at,updated_at=excluded.updated_at",
                    (uid, wanted.as_str(), acknowledged.then_some(now), now),
                )?;
                audit(
                    tx,
                    "dav.sharing",
                    Some(uid),
                    Some(uid),
                    &JValue::obj([("scope", JValue::from(wanted.as_str()))]),
                    now,
                )
            })?;
            let saved = View { scope: wanted, ..v };
            reply(Reply::json(200, &body(&saved, &authn.user.username)))
        }
        // Node's chain fell through for any other method.
        _ => Ok(Some(Outcome::Pass)),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn env(port: &str, scope: &str, origin: Option<&str>, token: bool) -> DavEnv {
        DavEnv {
            port: crate::util::js_number(port),
            ui_port: 8021.0,
            scope: Some(scope.into()),
            origin: origin.map(str::to_string),
            proxy_token_ok: token,
            host: None,
        }
    }

    #[test]
    fn not_configured_without_a_port() {
        assert_eq!(
            configuration(&DavEnv::default(), ""),
            Ok(DavConfig::NotConfigured)
        );
    }

    #[test]
    fn lan_cleartext_and_public_https() {
        assert_eq!(
            configuration(
                &env("8090", "lan", Some("http://10.0.0.5:8090"), false),
                "https://qa.example.test"
            ),
            Ok(DavConfig::Available {
                port: 8090,
                scope: "lan".into(),
                origin: "http://10.0.0.5:8090".into(),
                cleartext: true
            })
        );
        assert_eq!(
            configuration(
                &env("8090", "public", None, true),
                "https://qa.example.test"
            ),
            Ok(DavConfig::Available {
                port: 8090,
                scope: "public".into(),
                origin: "https://qa.example.test".into(),
                cleartext: false
            })
        );
    }

    #[test]
    fn refuses_what_node_refuses_at_boot() {
        let pub_o = "https://qa.example.test";
        for (e, msg) in [
            (
                env("80", "lan", Some("http://10.0.0.5"), false),
                "Invalid separate COWORK_DAV_PORT",
            ),
            (
                env("8021", "lan", Some("http://10.0.0.5"), false),
                "Invalid separate COWORK_DAV_PORT",
            ),
            (
                env("8090", "both", None, false),
                "Set COWORK_DAV_SCOPE to lan or public",
            ),
            (
                env("8090", "lan", None, false),
                "Configure COWORK_DAV_ORIGIN",
            ),
            (
                env("8090", "lan", Some("http://10.0.0.5/dav"), false),
                "COWORK_DAV_ORIGIN must be a clean HTTP(S) origin",
            ),
            (
                env("8090", "lan", Some("http://u:p@10.0.0.5"), false),
                "COWORK_DAV_ORIGIN must be a clean HTTP(S) origin",
            ),
            (
                env("8090", "public", Some("http://qa.example.test"), false),
                "Public DAV requires HTTPS",
            ),
            (
                env("8090", "lan", Some("https://qa.example.test"), true),
                "LAN DAV requires an authority separate from the public app origin",
            ),
            (
                env("8090", "public", None, false),
                "HTTPS DAV requires a 32-byte or longer hex COWORK_DAV_PROXY_TOKEN",
            ),
        ] {
            assert_eq!(configuration(&e, pub_o), Err(msg.to_string()));
        }
        let mut e = env("8090", "lan", Some("http://10.0.0.5"), false);
        e.host = Some("dav.local".into());
        assert_eq!(
            configuration(&e, pub_o),
            Err("COWORK_DAV_HOST must be an IP address".to_string())
        );
    }

    #[test]
    fn proxy_token_shape_is_checked_not_kept() {
        let hex = "a".repeat(64);
        let e =
            DavEnv::from_lookup(&|k: &str| (k == "COWORK_DAV_PROXY_TOKEN").then(|| hex.clone()));
        assert!(e.proxy_token_ok);
        assert!(!format!("{e:?}").contains(&hex));
        let e =
            DavEnv::from_lookup(&|k: &str| (k == "COWORK_DAV_PROXY_TOKEN").then(|| "A".repeat(64)));
        assert!(!e.proxy_token_ok);
    }

    #[test]
    fn username_is_uri_encoded() {
        assert_eq!(encode_uri_component("qa user/é"), "qa%20user%2F%C3%A9");
        assert_eq!(encode_uri_component("QA-tester_1.(x)"), "QA-tester_1.(x)");
    }
}
