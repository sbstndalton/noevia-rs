//! `POST /api/providers/test` (sbstndalton/noevia#2190 round 4), ported from core
//! routes/providers.cjs: probe an OpenAI-compatible endpoint's `/v1/models` before a provider is
//! saved, for any signed-in account. Permanent, no switch; Node refuses the route (core
//! rust-provider-test.cjs `RUST_PROVIDER_TEST_OWNED`), so the web supervisor pairs this front's
//! `rust-provider-test` capability with that Core. Every other method on the path stays Node's
//! (its `PUT|DELETE /api/providers/{id}` handler reads `test` as a provider id).
//!
//! Order and words follow core: the session gate (401/403/CSRF); the body (1 MiB, `invalid JSON`;
//! an empty body is `{}`); `valid baseUrl required` unless the trimmed address, trailing slashes
//! removed, starts with `http://` or `https://`; `baseUrl is too long` past 2048 UTF-16 units; the
//! member-origin rule (core ssrf.cjs `createEndpointApproved`: an administrator any http(s)
//! origin without credentials, a member only MEMBER_OUTBOUND_ORIGINS, read per request).
//!
//! The key: the one typed (`String(body.apiKey || '')`), or, with none typed and a `providerId`,
//! that provider's stored key, only when the caller may edit that row (not the default or its
//! `lemonade` alias, not a sign-in-managed or `external` row, a shared row only for an
//! administrator), the key is real (not empty, not `local`) and the address has the stored row's
//! origin. The rows are read from disk on every request, as Node's workspace view merges them:
//! `shared-providers.json` first, then `users/<id>/providers.json` (a `lemonade` row read as the
//! default, the first of each id, the default and shared rows dropped); keys are `enc:v1`
//! (secrets.cjs `decrypt` without an account), an unreadable key reads as none.
//!
//! Outbound, as Node's `fetchJson(url, { headers, redirect: 'error' }, 8000)`: `GET
//! <baseUrl minus a trailing /v1>/v1/models` with `Content-Type: application/json` and the
//! bearer key, no redirect followed (a 3xx is undici's `fetch failed`), no environment proxy, 8 s
//! for the whole exchange (`This operation was aborted`), an 8 MiB reply cap (over it the probe
//! failed: `provider returned <status>`). A 2xx answers `{ ok: true, models }` (core
//! `modelIdsFrom(body, 100)`), anything else 502 `{ error }`.
//!
//! One Node bug is fixed on the way: a `null` body answered 500 (`body.baseUrl` of null); here it
//! reads as an object without an address, as an array or a number already did:
//! `valid baseUrl required`.

use crate::{identity, reply, App};
use axum::body::Body;
use axum::http::{Method, Request, Response, StatusCode};
use axum::response::IntoResponse;
use http_body_util::BodyExt;
use js_json::JValue;
use server_auth::secrets::SecretKeys;
use std::path::Path;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use zeroize::Zeroizing;

pub const TEST: &str = "/api/providers/test";

const BODY_LIMIT: usize = 1024 * 1024;
const BODY_DEADLINE: Duration = Duration::from_secs(10);
const DEADLINE: Duration = Duration::from_secs(8);
/// core http.cjs `DEFAULT_MAX_RESPONSE_BYTES`.
const REPLY_CAP: usize = 8 * 1024 * 1024;
/// #1209: provider addresses are short.
const MAX_BASE_URL: usize = 2048;
const MODELS_MAX: usize = 100;
/// core chatgpt-oauth.cjs `KIND`.
const CHATGPT_KIND: &str = "chatgpt-oauth";
const NOT_APPROVED: &str =
    "Provider origin is not approved for member connections; contact an administrator.";
/// undici's messages, which Node passes on as `e.message`.
const FETCH_FAILED: &str = "fetch failed";
const ABORTED: &str = "This operation was aborted";
const TERMINATED: &str = "terminated";
/// undici's own User-Agent.
const NODE_AGENT: &str = "node";

pub(crate) fn answer(status: StatusCode, body: &JValue) -> Response<Body> {
    reply::json_text(status, js_json::stringify(body).unwrap_or_default(), true)
}

pub(crate) fn error(status: u16, message: &str) -> Response<Body> {
    answer(
        StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
        &JValue::obj([("error", JValue::Str(message.into()))]),
    )
}

/// `String(v || '')`.
pub(crate) fn or_empty(v: &JValue) -> String {
    if v.truthy() {
        js_json::to_js_string(v).unwrap_or_default()
    } else {
        String::new()
    }
}

/// `s.trim().replace(/\/+$/, '')` (JavaScript's whitespace set).
fn trimmed_address(s: &str) -> String {
    s3_sign::js_trim(s).trim_end_matches('/').to_string()
}

fn http_prefixed(s: &str) -> bool {
    s.starts_with("http://") || s.starts_with("https://")
}

/// `new URL(url).origin`, or `None` when it does not parse.
fn origin_of(raw: &str) -> Option<String> {
    url::Url::parse(raw)
        .ok()
        .map(|u| u.origin().ascii_serialization())
}

/// core ssrf.cjs `createEndpointApproved()(authn, rawUrl)`.
pub(crate) fn endpoint_approved(admin: bool, raw: &str) -> bool {
    let Ok(u) = url::Url::parse(raw) else {
        return false;
    };
    if !matches!(u.scheme(), "http" | "https") || !u.username().is_empty() || u.password().is_some()
    {
        return false;
    }
    if admin {
        return true;
    }
    let origin = u.origin().ascii_serialization();
    std::env::var("MEMBER_OUTBOUND_ORIGINS")
        .unwrap_or_default()
        .split(',')
        .any(|x| s3_sign::js_trim(x) == origin)
}

/// core index.cjs `DEFAULT_PROVIDER_ID`.
pub(crate) fn default_provider_id() -> String {
    std::env::var("DEFAULT_PROVIDER_ID")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "default".into())
}

/// One stored provider row as the routes read it: its fields and the opened key.
pub(crate) struct Row {
    pub(crate) id: JValue,
    pub(crate) raw: JValue,
    pub(crate) key: Zeroizing<String>,
}

/// workspace.cjs `readJson(file, { providers: [] }).providers || []`, each key opened as
/// `openProviderKey` does (an unreadable key is '').
fn read_rows(file: &Path, keys: Option<&SecretKeys>) -> Vec<Row> {
    let Ok(bytes) = std::fs::read(file) else {
        return Vec::new();
    };
    let Ok(doc) = js_json::parse(&String::from_utf8_lossy(&bytes)) else {
        return Vec::new();
    };
    let JValue::Arr(rows) = doc.get("providers").clone() else {
        return Vec::new();
    };
    rows.into_iter()
        .map(|raw| {
            let stored = or_empty(raw.get("apiKey"));
            let key = match keys {
                Some(k) => k
                    .open(&stored, None)
                    .map(|(plain, _)| plain)
                    .unwrap_or_else(|_| Zeroizing::new(String::new())),
                None => Zeroizing::new(String::new()),
            };
            Row {
                id: raw.get("id").clone(),
                raw,
                key,
            }
        })
        .collect()
}

fn is_id(v: &JValue, id: &str) -> bool {
    v.as_str() == Some(id)
}

/// The workspace's merged view (workspace.cjs `mergeProviders`), the first row with `id`.
pub(crate) fn find_row(
    data_dir: &Path,
    user: &str,
    id: &str,
    keys: Option<&SecretKeys>,
) -> Option<Row> {
    let default = default_provider_id();
    if id == default {
        // The env-derived default row is first in the view; it is never editable here.
        return Some(Row {
            id: JValue::Str(default),
            raw: JValue::obj([("shared", JValue::Bool(true))]),
            key: Zeroizing::new(String::new()),
        });
    }
    let shared = read_rows(&data_dir.join("shared-providers.json"), keys);
    if let Some(row) = shared
        .into_iter()
        .filter(|r| !is_id(&r.id, &default))
        .find(|r| is_id(&r.id, id))
    {
        return Some(row);
    }
    let user_ok = user.len() == 36 && user.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-');
    if !user_ok {
        return None;
    }
    let mut private = read_rows(
        &data_dir.join("users").join(user).join("providers.json"),
        keys,
    );
    for row in &mut private {
        if is_id(&row.id, "lemonade") {
            row.id = JValue::Str(default.clone());
        }
    }
    let mut seen: Vec<JValue> = Vec::new();
    private
        .into_iter()
        .filter(|r| {
            let first = !seen.contains(&r.id);
            seen.push(r.id.clone());
            first
        })
        .filter(|r| !is_id(&r.id, &default) && !r.raw.get("shared").truthy())
        .find(|r| is_id(&r.id, id))
}

/// core routes/providers.cjs `editRefusal(id, row, authn)` is null.
fn editable(id: &str, row: &Row, admin: bool) -> bool {
    if id == default_provider_id() || id == "lemonade" {
        return false;
    }
    if row.raw.get("kind").as_str() == Some(CHATGPT_KIND)
        || matches!(row.raw.get("external"), JValue::Bool(true))
    {
        return false;
    }
    !(row.raw.get("shared").truthy() && !admin)
}

/// core routes/providers.cjs `modelIdsFrom(body, max)`.
pub fn model_ids(body: &JValue, max: usize) -> Vec<String> {
    let JValue::Arr(rows) = body.get("data") else {
        return Vec::new();
    };
    let mut ids: Vec<String> = Vec::new();
    for m in rows {
        let id = match m.get("id") {
            JValue::Str(s) => s3_sign::js_trim(s).to_string(),
            _ => String::new(),
        };
        let units = id.encode_utf16().count();
        if !id.is_empty()
            && units <= 200
            && !id.chars().any(|c| (c as u32) < 0x20 || c as u32 == 0x7f)
            && !ids.contains(&id)
        {
            ids.push(id);
        }
        if ids.len() >= max {
            break;
        }
    }
    ids
}

fn client() -> Option<&'static reqwest::Client> {
    static CLIENT: OnceLock<Option<reqwest::Client>> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .user_agent(NODE_AGENT)
                .build()
                .ok()
        })
        .as_ref()
}

/// What the probe got: Node's `{ ok, status, body }` or the thrown message.
pub(crate) enum Probe {
    Reply { ok: bool, status: u16, body: JValue },
    Thrown(&'static str),
}

pub(crate) async fn probe(url: &str, key: &str) -> Probe {
    let deadline = Instant::now() + DEADLINE;
    let Some(client) = client() else {
        return Probe::Thrown(FETCH_FAILED);
    };
    let mut request = client.get(url).header("Content-Type", "application/json");
    if !key.is_empty() {
        request = request.header("Authorization", format!("Bearer {key}"));
    }
    let mut response = match tokio::time::timeout_at(deadline.into(), request.send()).await {
        Err(_) => return Probe::Thrown(ABORTED),
        Ok(Err(e)) if e.is_timeout() => return Probe::Thrown(ABORTED),
        Ok(Err(_)) => return Probe::Thrown(FETCH_FAILED),
        Ok(Ok(r)) => r,
    };
    let status = response.status().as_u16();
    if matches!(status, 301 | 302 | 303 | 307 | 308) {
        return Probe::Thrown(FETCH_FAILED);
    }
    let read = async {
        let mut body = Vec::new();
        loop {
            match response.chunk().await {
                Ok(Some(chunk)) => {
                    if body.len() + chunk.len() > REPLY_CAP {
                        return Err(None);
                    }
                    body.extend_from_slice(&chunk);
                }
                Ok(None) => return Ok(body),
                Err(_) => return Err(Some(TERMINATED)),
            }
        }
    };
    match tokio::time::timeout_at(deadline.into(), read).await {
        Err(_) => Probe::Thrown(ABORTED),
        Ok(Err(Some(message))) => Probe::Thrown(message),
        // `{ ok: false, status, error: 'response too large' }`.
        Ok(Err(None)) => Probe::Reply {
            ok: false,
            status,
            body: JValue::Undefined,
        },
        Ok(Ok(bytes)) => {
            let text = String::from_utf8_lossy(&bytes).into_owned();
            let body = js_json::parse(&text).unwrap_or(JValue::Str(text));
            Probe::Reply {
                ok: (200..300).contains(&status),
                status,
                body,
            }
        }
    }
}

enum BodyError {
    TooLarge,
    Failed,
}

async fn read_body(mut body: Body) -> Result<Vec<u8>, BodyError> {
    let read = async {
        let mut out = Vec::new();
        while let Some(frame) = body.frame().await {
            if let Ok(data) = frame.map_err(|_| BodyError::Failed)?.into_data() {
                if out.len() + data.len() > BODY_LIMIT {
                    return Err(BodyError::TooLarge);
                }
                out.extend_from_slice(&data);
            }
        }
        Ok(out)
    };
    tokio::time::timeout(BODY_DEADLINE, read)
        .await
        .map_err(|_| BodyError::Failed)?
}

pub async fn serve(
    app: &Arc<App>,
    conn: crate::serve::Conn,
    req: Request<Body>,
    path: String,
) -> Response<Body> {
    // Node compared the exact pathname, and only POST.
    if path != TEST || req.method() != Method::POST {
        return app.proxy.forward(conn.peer, req).await;
    }
    let (parts, body) = req.into_parts();
    let creds = identity::creds_of(&parts);
    let layer = Arc::clone(&app.identity);
    let now = identity::now_ms();
    let gate_path = path.clone();
    let id = match tokio::task::spawn_blocking(move || layer.gate_blocking(&creds, &gate_path, now))
        .await
    {
        Ok(Ok(id)) => id,
        Ok(Err(rejection)) => return rejection.into_response(),
        Err(_) => return identity::Rejection::Unavailable.into_response(),
    };
    // core http.cjs readJson: an empty body is {}.
    let body = match read_body(body).await {
        Ok(b) if b.is_empty() => JValue::Obj(Vec::new()),
        Ok(b) => match js_json::parse(&String::from_utf8_lossy(&b)) {
            Ok(v) => v,
            Err(_) => return error(400, "invalid JSON"),
        },
        Err(BodyError::TooLarge) => return error(413, "Request exceeds size limit"),
        Err(BodyError::Failed) => return error(400, "invalid JSON"),
    };
    let admin = id.user.role == server_auth::Role::Admin;
    let base_url = trimmed_address(&or_empty(body.get("baseUrl")));
    if !http_prefixed(&base_url) {
        return error(400, "valid baseUrl required");
    }
    if base_url.encode_utf16().count() > MAX_BASE_URL {
        return error(400, "baseUrl is too long");
    }
    // The test route must not become a prober for internal addresses on behalf of a member.
    if !endpoint_approved(admin, &base_url) {
        return error(400, NOT_APPROVED);
    }
    let mut key = Zeroizing::new(or_empty(body.get("apiKey")));
    // Editing (#535): with no key typed, an existing provider's stored key, for an editor of that
    // row and the origin it is stored for only.
    if let (true, JValue::Str(provider)) = (key.is_empty(), body.get("providerId")) {
        if !provider.is_empty() {
            let (provider, user, data_dir) = (
                provider.clone(),
                id.user.id.clone(),
                app.config.data_dir.clone(),
            );
            let target = origin_of(&base_url);
            let stored = tokio::task::spawn_blocking(move || {
                let data_dir = data_dir?;
                let previous = std::env::var("SECRETS_KEY_PREVIOUS")
                    .ok()
                    .map(Zeroizing::new);
                let keys = server_auth::secrets::load_existing(
                    &data_dir,
                    previous.as_deref().map(String::as_str),
                )
                .ok();
                let row = find_row(&data_dir, &user, &provider, keys.as_ref())?;
                let real = !row.key.is_empty() && row.key.as_str() != "local";
                let same_origin = target.is_some()
                    && row
                        .raw
                        .get("baseUrl")
                        .as_str()
                        .and_then(origin_of)
                        .is_some_and(|o| Some(o) == target);
                (editable(&provider, &row, admin) && real && same_origin).then_some(row.key)
            })
            .await
            .ok()
            .flatten();
            if let Some(stored) = stored {
                key = stored;
            }
        }
    }
    let url = format!(
        "{}/v1/models",
        base_url.strip_suffix("/v1").unwrap_or(&base_url)
    );
    match probe(&url, &key).await {
        Probe::Reply { ok: true, body, .. } => {
            let models = model_ids(&body, MODELS_MAX)
                .into_iter()
                .map(JValue::Str)
                .collect();
            answer(
                StatusCode::OK,
                &JValue::obj([("ok", JValue::Bool(true)), ("models", JValue::Arr(models))]),
            )
        }
        Probe::Reply { status, .. } => error(502, &format!("provider returned {status}")),
        Probe::Thrown(message) => error(502, message),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn model_ids_are_core_model_ids_from() {
        let body = js_json::parse(
            r#"{"data":[{"id":" a "},{"id":"a"},{"id":""},{"id":7},null,{"id":"b\u0007"},{"id":"c"}]}"#,
        )
        .unwrap();
        assert_eq!(model_ids(&body, 100), ["a", "c"]);
        assert_eq!(model_ids(&body, 1), ["a"]);
        let long = format!(
            r#"{{"data":[{{"id":"{}"}},{{"id":"{}"}}]}}"#,
            "x".repeat(200),
            "y".repeat(201)
        );
        assert_eq!(
            model_ids(&js_json::parse(&long).unwrap(), 100),
            ["x".repeat(200)]
        );
        assert!(model_ids(&JValue::Str("nope".into()), 100).is_empty());
        assert!(model_ids(&js_json::parse(r#"{"data":{}}"#).unwrap(), 100).is_empty());
    }

    #[test]
    fn origins_and_approval_are_core_ssrf() {
        assert_eq!(
            origin_of("https://Example.TEST:443/v1").as_deref(),
            Some("https://example.test")
        );
        assert_eq!(
            origin_of("http://x.test:8080/a").as_deref(),
            Some("http://x.test:8080")
        );
        assert_eq!(origin_of("nope"), None);
        assert!(endpoint_approved(true, "http://10.0.0.1/v1"));
        assert!(!endpoint_approved(true, "http://u:p@10.0.0.1/v1"));
        assert!(!endpoint_approved(true, "ftp://x.test"));
        assert!(!endpoint_approved(false, "http://10.0.0.1/v1"));
        assert_eq!(
            trimmed_address("  https://x.test/v1//  "),
            "https://x.test/v1"
        );
    }
}
