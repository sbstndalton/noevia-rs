//! Web-address GETs and the two administrator POST writers. Node only reads the resulting
//! settings; writes, legacy passkey RP freeze and audit share one Rust transaction.
use crate::{account_routes, identity, reply, App};
use axum::body::Body;
use axum::http::{Request, Response, StatusCode};
use axum::response::IntoResponse;
use server_auth::Role;
use server_store::{Reader, StoreError, WriteTx};
use std::sync::Arc;
use std::time::Duration;

const FULL_ADDRESS: &str = "Enter a full address, such as https://noevia.example.com.";
const ONLY_ADDRESS: &str =
    "Enter just the address, without a path, such as https://noevia.example.com.";
const HTTPS_OR_PRIVATE: &str = "Use https://, or a private-network address over http://.";
const CHANGED: &str = "Web address changed while checking; try again.";

fn setting(reader: &Reader<'_>, key: &str) -> Result<Option<String>, StoreError> {
    reader.row("SELECT value FROM settings WHERE key=?1", [key], |row| {
        row.get(0)
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Address {
    origin: String,
    source: &'static str,
    previous: Vec<String>,
    rp_id: String,
}

fn previous(reader: &Reader<'_>) -> Result<Vec<String>, StoreError> {
    let raw = setting(reader, "previous_origins")?.unwrap_or_default();
    Ok(serde_json::from_str::<serde_json::Value>(&raw)
        .ok()
        .and_then(|value| value.as_array().cloned())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|value| value.as_str().map(ToOwned::to_owned))
        .collect())
}

fn address(reader: &Reader<'_>, public_origin: &str, rp_env: &str) -> Result<Address, StoreError> {
    let admin = setting(reader, "public_origin_admin")?.unwrap_or_default();
    let setup = setting(reader, "public_origin")?.unwrap_or_default();
    let (origin, source) = if !admin.is_empty() {
        (admin, "settings")
    } else if !public_origin.is_empty() {
        (public_origin.to_owned(), "environment")
    } else if !setup.is_empty() {
        (setup, "setup")
    } else {
        (String::new(), "none")
    };
    let previous = previous(reader)?;
    let rp_id = server_account::util::rp_for(&origin, rp_env)
        .map_err(|_| StoreError::NotReady("invalid public origin".into()))?;
    Ok(Address {
        origin,
        source,
        previous,
        rp_id,
    })
}

fn related(address: &Address, setup: Option<String>) -> Vec<String> {
    let mut origins = Vec::new();
    let current = if address.origin.is_empty() {
        setup.unwrap_or_default()
    } else {
        address.origin.clone()
    };
    for origin in std::iter::once(current).chain(address.previous.iter().cloned()) {
        if !origin.is_empty() && !origins.contains(&origin) {
            origins.push(origin);
        }
    }
    origins
}

fn view_text(address: &Address) -> String {
    format!(
        r#"{{"origin":{},"source":"{}","previous":{},"rpId":{}}}"#,
        serde_json::to_string(&address.origin).unwrap_or_default(),
        address.source,
        serde_json::to_string(&address.previous).unwrap_or_default(),
        serde_json::to_string(&address.rp_id).unwrap_or_default(),
    )
}

fn removed_text(address: &Address, removed: &str, affected: i64) -> String {
    let view = view_text(address);
    format!(
        r#"{},"removed":{},"passkeysAffected":{}}}"#,
        view.trim_end_matches('}'),
        serde_json::to_string(removed).unwrap_or_default(),
        affected
    )
}

fn unavailable(api: bool) -> Response<Body> {
    reply::error(
        StatusCode::SERVICE_UNAVAILABLE,
        "Web address is unavailable right now.",
        api,
    )
}

fn js_string(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => String::new(),
        serde_json::Value::Bool(false) => String::new(),
        serde_json::Value::Bool(true) => "true".into(),
        serde_json::Value::Number(number) if number.as_f64() == Some(0.0) => String::new(),
        serde_json::Value::Number(number) => number.to_string(),
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(items) => {
            items.iter().map(js_string).collect::<Vec<_>>().join(",")
        }
        serde_json::Value::Object(_) => "[object Object]".into(),
    }
}

fn js_truthy(value: Option<&serde_json::Value>) -> bool {
    match value {
        None | Some(serde_json::Value::Null | serde_json::Value::Bool(false)) => false,
        Some(serde_json::Value::Number(n)) => n.as_f64().is_some_and(|n| n != 0.0),
        Some(serde_json::Value::String(s)) => !s.is_empty(),
        Some(_) => true,
    }
}

fn clean(raw: &str) -> String {
    raw.trim().trim_end_matches('/').to_owned()
}

fn acceptable(url: &url::Url) -> bool {
    if url.scheme() == "https" {
        return true;
    }
    if url.scheme() != "http" {
        return false;
    }
    let host = url.host_str().unwrap_or_default();
    if host == "localhost" || host.ends_with(".localhost") || host == "127.0.0.1" {
        return true;
    }
    if host.starts_with("10.") || host.starts_with("192.168.") {
        return true;
    }
    if let Some(rest) = host.strip_prefix("172.") {
        if let Some(octet) = rest.split('.').next().and_then(|v| v.parse::<u8>().ok()) {
            if (16..=31).contains(&octet) {
                return true;
            }
        }
    }
    !host.is_empty() && !host.contains(['.', ':'])
}

fn candidate(raw: &str) -> Result<(String, url::Url), &'static str> {
    let clean = clean(raw);
    let parsed = url::Url::parse(&clean).map_err(|_| FULL_ADDRESS)?;
    if parsed.origin().ascii_serialization() != clean {
        return Err(ONLY_ADDRESS);
    }
    if !acceptable(&parsed) {
        return Err(HTTPS_OR_PRIVATE);
    }
    Ok((clean, parsed))
}

fn host_of(url: &url::Url) -> String {
    url[url::Position::BeforeHost..url::Position::AfterPort].to_owned()
}

async fn reaches(url: &url::Url, instance_id: &str) -> Option<String> {
    let host = host_of(url);
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(8))
        .build();
    let Ok(client) = client else {
        return Some(format!(
            "{host} can’t be reached yet. Check its DNS or tunnel route, then try again."
        ));
    };
    let target = format!("{}/api/instance", url.origin().ascii_serialization());
    let Ok(mut response) = client.get(target).send().await else {
        return Some(format!(
            "{host} can’t be reached yet. Check its DNS or tunnel route, then try again."
        ));
    };
    if !response.status().is_success() {
        return Some(format!(
            "{host} answered {}, not noevia.",
            response.status().as_u16()
        ));
    }
    let mut bytes = Vec::new();
    loop {
        let Ok(chunk) = response.chunk().await else {
            return Some(format!(
                "{host} reaches a different server, not this noevia."
            ));
        };
        let Some(chunk) = chunk else { break };
        if bytes.len().saturating_add(chunk.len()) > 64 * 1024 {
            return Some(format!(
                "{host} reaches a different server, not this noevia."
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    let id = serde_json::from_slice::<serde_json::Value>(&bytes)
        .ok()
        .and_then(|body| {
            body.get("id")
                .and_then(|value| value.as_str())
                .map(ToOwned::to_owned)
        });
    if id.as_deref() == Some(instance_id) {
        None
    } else {
        Some(format!(
            "{host} reaches a different server, not this noevia."
        ))
    }
}

enum Written {
    Gate(identity::Rejection),
    Error(StatusCode, &'static str),
    View(Address),
    Removed(Address, String, i64),
}

fn written_response(outcome: Written) -> Response<Body> {
    match outcome {
        Written::Gate(rejection) => rejection.into_response(),
        Written::Error(status, message) => reply::error(status, message, true),
        Written::View(view) => reply::json_text(StatusCode::OK, view_text(&view), true),
        Written::Removed(view, removed, affected) => reply::json_text(
            StatusCode::OK,
            removed_text(&view, &removed, affected),
            true,
        ),
    }
}

fn write_address(
    tx: &WriteTx<'_>,
    baseline: &Address,
    next: &str,
    actor: &str,
    public_origin: &str,
    rp_env: &str,
    now: i64,
) -> Result<Written, StoreError> {
    let current = address(&tx.reader(), public_origin, rp_env)?;
    if &current != baseline {
        return Ok(Written::Error(StatusCode::CONFLICT, CHANGED));
    }
    if current.origin == next {
        return Ok(Written::View(current));
    }
    let mut prior = Vec::new();
    if !current.origin.is_empty() {
        prior.push(current.origin.clone());
    }
    prior.extend(current.previous);
    let mut earlier = Vec::new();
    for item in prior {
        if item != next && !earlier.contains(&item) {
            earlier.push(item);
        }
        if earlier.len() == 5 {
            break;
        }
    }
    tx.set_setting("public_origin_admin", next)?;
    tx.set_setting(
        "previous_origins",
        &serde_json::to_string(&earlier).unwrap_or_default(),
    )?;
    tx.execute(
        "UPDATE passkeys SET rp_id=?1 WHERE rp_id IS NULL",
        [&current.rp_id],
    )?;
    tx.execute("INSERT INTO audit_events(actor_user_id,target_user_id,action,detail,created_at) VALUES(?1,?2,'settings.public_origin','{}',?3)", server_store::rusqlite::params![actor, actor, now])?;
    Ok(Written::View(address(&tx.reader(), public_origin, rp_env)?))
}

fn remove_address(
    tx: &WriteTx<'_>,
    baseline: &Address,
    next: &str,
    actor: &str,
    public_origin: &str,
    rp_env: &str,
    now: i64,
) -> Result<Written, StoreError> {
    let current = address(&tx.reader(), public_origin, rp_env)?;
    if &current != baseline {
        return Ok(Written::Error(StatusCode::CONFLICT, CHANGED));
    }
    if !next.is_empty() && current.origin == next {
        return Ok(Written::Error(
            StatusCode::BAD_REQUEST,
            "The current address cannot be removed from the earlier addresses.",
        ));
    }
    if !current.previous.iter().any(|p| p == next) {
        return Ok(Written::Error(
            StatusCode::NOT_FOUND,
            "That address is not one of the earlier addresses.",
        ));
    }
    let earlier = current
        .previous
        .into_iter()
        .filter(|p| p != next)
        .collect::<Vec<_>>();
    let host = url::Url::parse(next)
        .ok()
        .and_then(|u| u.host_str().map(ToOwned::to_owned))
        .unwrap_or_default();
    let affected = if host.is_empty() {
        0
    } else {
        tx.reader()
            .row(
                "SELECT count(*) FROM passkeys WHERE rp_id=?1",
                [&host],
                |row| row.get::<_, i64>(0),
            )?
            .unwrap_or(0)
    };
    tx.set_setting(
        "previous_origins",
        &serde_json::to_string(&earlier).unwrap_or_default(),
    )?;
    let detail = serde_json::json!({"origin":next,"passkeysAffected":affected}).to_string();
    tx.execute("INSERT INTO audit_events(actor_user_id,target_user_id,action,detail,created_at) VALUES(?1,?2,'settings.public_origin.earlier_removed',?3,?4)", server_store::rusqlite::params![actor, actor, detail, now])?;
    Ok(Written::Removed(
        address(&tx.reader(), public_origin, rp_env)?,
        next.to_owned(),
        affected,
    ))
}

pub async fn serve(app: &Arc<App>, req: Request<Body>, path: String) -> Response<Body> {
    let (parts, body) = req.into_parts();
    let creds = identity::creds_of(&parts);
    let layer = Arc::clone(&app.identity);
    let settings = app.config.account.clone();
    let api = path.starts_with("/api/");
    let is_write = parts.method == axum::http::Method::POST;
    if !is_write {
        return tokio::task::spawn_blocking(move || {
            let Some(settings) = settings else {
                return unavailable(api);
            };
            if path == "/api/admin/web-address" {
                let id = match layer.gate_blocking(&creds, &path, identity::now_ms()) {
                    Ok(id) => id,
                    Err(rejection) => return rejection.into_response(),
                };
                if id.user.role != Role::Admin {
                    return reply::error(StatusCode::FORBIDDEN, "Administrator required", true);
                }
            }
            let Ok(store) = layer.store_blocking() else {
                return unavailable(api);
            };
            match path.as_str() {
                "/api/instance" => {
                    let Ok(Some(id)) = store.read(|reader| setting(reader, "instance_id")) else {
                        return unavailable(api);
                    };
                    if id.is_empty() {
                        return unavailable(api);
                    }
                    reply::json_text(
                        StatusCode::OK,
                        format!(
                            r#"{{"id":{}}}"#,
                            serde_json::to_string(&id).unwrap_or_default()
                        ),
                        api,
                    )
                }
                "/.well-known/webauthn" => {
                    let Ok(origins) = store.read(|reader| {
                        let view =
                            address(reader, &settings.public_origin, &settings.webauthn_rp_id)?;
                        let setup = setting(reader, "public_origin")?;
                        Ok(related(&view, setup))
                    }) else {
                        return unavailable(api);
                    };
                    reply::json(
                        StatusCode::OK,
                        &serde_json::json!({"origins": origins}),
                        api,
                    )
                }
                "/api/admin/web-address" => {
                    let Ok(view) = store.read(|reader| {
                        address(reader, &settings.public_origin, &settings.webauthn_rp_id)
                    }) else {
                        return unavailable(api);
                    };
                    reply::json_text(StatusCode::OK, view_text(&view), api)
                }
                _ => unavailable(api),
            }
        })
        .await
        .unwrap_or_else(|_| unavailable(api));
    }

    let Some(settings) = settings else {
        return unavailable(true);
    };
    let path2 = path.clone();
    let creds2 = creds.clone();
    let id = match tokio::task::spawn_blocking(move || {
        layer.gate_blocking(&creds2, &path2, identity::now_ms())
    })
    .await
    {
        Ok(Ok(id)) => id,
        Ok(Err(rejection)) => return rejection.into_response(),
        _ => return unavailable(true),
    };
    if id.user.role != Role::Admin {
        return reply::error(StatusCode::FORBIDDEN, "Administrator required", true);
    }
    let body = account_routes::read_body(body, server_account::BODY_LIMIT)
        .await
        .unwrap_or_default();
    let data = if body.over {
        None
    } else if body.bytes.is_empty() {
        Some(serde_json::json!({}))
    } else {
        serde_json::from_slice::<serde_json::Value>(&body.bytes).ok()
    };
    let data = data.unwrap_or_else(|| serde_json::json!({}));
    let actor = id.user.id;
    let remove = path == "/api/admin/web-address/remove";
    if !data.is_object() {
        return reply::error(
            StatusCode::BAD_REQUEST,
            if remove {
                "Choose an earlier address to remove."
            } else {
                FULL_ADDRESS
            },
            true,
        );
    }
    let next;
    let parsed;
    if remove {
        let Some(origin) = data.get("origin").and_then(|v| v.as_str()) else {
            return reply::error(
                StatusCode::BAD_REQUEST,
                "Choose an earlier address to remove.",
                true,
            );
        };
        next = clean(origin);
        parsed = None;
    } else {
        let origin = data.get("origin").map(js_string).unwrap_or_default();
        match candidate(&origin) {
            Ok((value, url)) => {
                next = value;
                parsed = Some(url);
            }
            Err(message) => return reply::error(StatusCode::BAD_REQUEST, message, true),
        }
    }
    let force = js_truthy(data.get("force"));
    let layer = Arc::clone(&app.identity);
    let public_origin = settings.public_origin;
    let rp_env = settings.webauthn_rp_id;
    let baseline = match tokio::task::spawn_blocking({
        let layer = Arc::clone(&layer);
        let public_origin = public_origin.clone();
        let rp_env = rp_env.clone();
        move || {
            layer
                .store_blocking()?
                .read(|reader| address(reader, &public_origin, &rp_env))
        }
    })
    .await
    {
        Ok(Ok(value)) => value,
        _ => return unavailable(true),
    };
    let mut probed_instance = None;
    if !remove && !force && baseline.origin != next {
        let instance = match tokio::task::spawn_blocking({
            let layer = Arc::clone(&layer);
            move || {
                layer
                    .store_blocking()?
                    .read(|reader| setting(reader, "instance_id"))
            }
        })
        .await
        {
            Ok(Ok(Some(value))) if !value.is_empty() => value,
            _ => return unavailable(true),
        };
        if let Some(url) = parsed.as_ref() {
            if let Some(problem) = reaches(url, &instance).await {
                return reply::json(
                    StatusCode::CONFLICT,
                    &serde_json::json!({"error":problem,"unreachable":true}),
                    true,
                );
            }
            probed_instance = Some(instance);
        }
    }
    let app2 = Arc::clone(app);
    let result = tokio::task::spawn_blocking(move || {
        let writer = app2.writes.writer()?;
        let auth = app2
            .identity
            .authenticator()
            .ok_or_else(|| StoreError::NotReady("auth unavailable".into()))?;
        writer.write(|tx| {
            let current = match auth.gate_in(&tx.reader(), &creds, &path, identity::now_ms())? {
                Ok(id) => id,
                Err(refusal) => return Ok(Written::Gate(identity::Rejection::Refused(refusal))),
            };
            if current.user.role != Role::Admin {
                return Ok(Written::Error(
                    StatusCode::FORBIDDEN,
                    "Administrator required",
                ));
            }
            if current.user.id != actor {
                return Ok(Written::Error(StatusCode::CONFLICT, CHANGED));
            }
            if let Some(expected) = probed_instance.as_deref() {
                if setting(&tx.reader(), "instance_id")?.as_deref() != Some(expected) {
                    return Ok(Written::Error(StatusCode::CONFLICT, CHANGED));
                }
            }
            let now = identity::now_ms();
            if remove {
                remove_address(tx, &baseline, &next, &actor, &public_origin, &rp_env, now)
            } else {
                write_address(tx, &baseline, &next, &actor, &public_origin, &rp_env, now)
            }
        })
    })
    .await;
    match result {
        Ok(Ok(outcome)) => written_response(outcome),
        _ => unavailable(true),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn related_origins_dedup_current_and_prior_only() {
        let view = Address {
            origin: "https://new.example".into(),
            source: "settings",
            previous: vec![
                "https://old.example".into(),
                "https://new.example".into(),
                "https://old.example".into(),
            ],
            rp_id: "new.example".into(),
        };
        assert_eq!(
            related(&view, None),
            ["https://new.example", "https://old.example"]
        );
    }
    #[test]
    fn exact_origin_and_private_http_before_probe() {
        for (input, expected) in [
            ("http://[::1]", HTTPS_OR_PRIVATE),
            ("https://host.test/path", ONLY_ADDRESS),
            ("https://user:pass@host.test", ONLY_ADDRESS),
            ("https://host.test/#f", ONLY_ADDRESS),
            ("https://host.test:443", ONLY_ADDRESS),
            ("https://bücher.test", ONLY_ADDRESS),
            ("http://public.test", HTTPS_OR_PRIVATE),
        ] {
            assert!(
                matches!(candidate(input), Err(message) if message == expected),
                "{input}"
            );
        }
        for input in [
            "https://xn--bcher-kva.test",
            "https://host.test:8443",
            "http://127.0.0.1:9000",
            "http://myserver",
        ] {
            assert_eq!(
                candidate(input).map(|(clean, _)| clean),
                Ok(input.to_owned()),
                "{input}"
            );
        }
    }
}
