//! `GET /api/providers/{id}/models[?refresh=1]` (sbstndalton/noevia#2190 round 5, group B1), ported
//! from core routes/providers.cjs `providerModels` (#1009): a stored provider's model ids for the
//! routing pickers, fetched server-side with the stored key and cached per account. Permanent, no
//! switch; Node refuses the route (core rust-provider-models.cjs `RUST_PROVIDER_MODELS_OWNED`), so the
//! web supervisor pairs this front's `rust-provider-models` capability with that Core.
//!
//! Order and words follow core: the session gate (401/403/CSRF), then on the exact path
//! `/api/providers/<id>/models` (any other shape, and `<id>` = `chatgpt`, whose GET is the token
//! broker's static route, go to Node): a method other than GET is 405; `<id>` that
//! `decodeURIComponent` refuses is 400 `no such provider`; the default (and its `lemonade` alias) is
//! 400; then the row from the files Node writes (`shared-providers.json`, `users/<id>/providers.json`,
//! the workspace view's merge order, keys `enc:v1`), 404 without one.
//!
//! - A Sign in with ChatGPT row (kind `chatgpt-oauth`) lists the broker's catalogue
//!   (chatgpt_oauth_routes.rs): 404 while the flag is off or for a shared row; a failure keeps its
//!   status when 401 or 429, else 502. With no stored row yet, the `chatgpt-oauth` id is read as the
//!   account's private row when the broker holds its tokens (core adds the row at its next listing).
//! - Other rows: `http(s)` addresses only (400 `this provider has no address`); a member's own row
//!   must still pass the member-origin rule (a shared row is an administrator's and used as it is);
//!   then `GET <base minus /v1>/v1/models` with the bearer key when it is real, as core's
//!   `fetchJson(..., { redirect: 'error' }, 8000)` (provider_test_routes.rs `probe`: no redirect, no
//!   environment proxy, 8 s, 8 MiB), model ids as `modelIdsFrom(body)` (200 at most).
//!
//! The cache is core's: per (account, provider id, address, key) for ten minutes, 500 entries; a
//! refresh within ten seconds of the account's last one for that provider is served from the cache
//! and marked `throttled`; concurrent fetches of one list share one request; a new list replaces
//! the account's older lists of that provider. Keys are kept as a digest only.
use crate::provider_test_routes::{
    answer, default_provider_id, endpoint_approved, error, find_row, model_ids, probe, Probe,
};
use crate::{identity, App};
use axum::body::Body;
use axum::http::{Method, Request, Response, StatusCode};
use axum::response::IntoResponse;
use js_json::JValue;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use zeroize::Zeroizing;

pub const ROUTE: &str = "/api/providers/{providerId}/models{*?}";
const TTL_MS: i64 = 10 * 60 * 1000;
const REFRESH_GAP_MS: i64 = 10 * 1000;
const MAX_ENTRIES: usize = 500;
const LIST_MAX: usize = 200;
const CHATGPT_KIND: &str = "chatgpt-oauth";
const CHATGPT_ID: &str = "chatgpt-oauth";
const OFF_MESSAGE: &str = "Sign in with ChatGPT is turned off on this server.";

type Fetched = Result<(Arc<Vec<String>>, i64), Failure>;

#[derive(Clone)]
enum Failure {
    Status(u16),
    Unreachable,
}

struct Entry {
    owner: String,
    models: Arc<Vec<String>>,
    until: i64,
    fetched_at: i64,
}

#[derive(Default)]
struct Mem {
    lists: HashMap<String, Entry>,
    last_refresh: HashMap<String, i64>,
    fetched: HashMap<String, (u64, Fetched)>,
    seq: u64,
}

/// The per-process cache (core's `modelLists`, `inflight`, `lastRefresh`).
#[derive(Default)]
pub struct Lists {
    mem: Mutex<Mem>,
    locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl Lists {
    fn mem(&self) -> std::sync::MutexGuard<'_, Mem> {
        self.mem.lock().unwrap_or_else(|e| e.into_inner())
    }
    fn lock_for(&self, key: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut m = self.locks.lock().unwrap_or_else(|e| e.into_inner());
        m.retain(|_, l| Arc::strong_count(l) > 1);
        Arc::clone(
            m.entry(key.to_owned())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))),
        )
    }
}

fn digest(parts: &[&str]) -> String {
    let mut h = Sha256::new();
    for p in parts {
        h.update((p.len() as u64).to_le_bytes());
        h.update(p.as_bytes());
    }
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// JS `decodeURIComponent`: `%XX` escapes to UTF-8, which must be valid; a stray `%` throws.
fn decode_component(s: &str) -> Option<String> {
    let mut out = Vec::with_capacity(s.len());
    let mut bytes = s.bytes();
    while let Some(b) = bytes.next() {
        if b == b'%' {
            let hi = char::from(bytes.next()?).to_digit(16)?;
            let lo = char::from(bytes.next()?).to_digit(16)?;
            out.push(u8::try_from(hi * 16 + lo).ok()?);
        } else {
            out.push(b);
        }
    }
    String::from_utf8(out).ok()
}

fn models_answer(models: &[String], cached: bool, fetched_at: i64, throttled: bool) -> JValue {
    let mut pairs = vec![
        (
            "models",
            JValue::Arr(models.iter().cloned().map(JValue::Str).collect()),
        ),
        ("cached", JValue::Bool(cached)),
        ("fetchedAt", JValue::Num(fetched_at as f64)),
    ];
    if throttled {
        pairs.push(("throttled", JValue::Bool(true)));
    }
    JValue::obj(pairs)
}

pub async fn serve(
    app: &Arc<App>,
    conn: crate::serve::Conn,
    req: Request<Body>,
    path: String,
) -> Response<Body> {
    // core: `p.match(/^\/api\/providers\/([^/]+)\/models$/)` with the segment not `chatgpt`.
    let segment = path
        .strip_prefix("/api/providers/")
        .and_then(|rest| rest.strip_suffix("/models"))
        .filter(|s| !s.is_empty() && !s.contains('/') && *s != "chatgpt")
        .map(str::to_owned);
    let Some(segment) = segment else {
        return app.proxy.forward(conn.peer, req).await;
    };
    let refresh = req
        .uri()
        .query()
        .and_then(|q| {
            url::form_urlencoded::parse(q.as_bytes())
                .find(|(k, _)| k == "refresh")
                .map(|(_, v)| v == "1")
        })
        .unwrap_or(false);
    let method = req.method().clone();
    let (parts, _body) = req.into_parts();
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
    if method != Method::GET {
        return error(405, "method not allowed");
    }
    let Some(provider) = decode_component(&segment) else {
        return error(400, "no such provider");
    };
    let default = default_provider_id();
    if provider == default || provider == "lemonade" {
        return error(
            400,
            "the default provider lists its models under Settings → Models",
        );
    }
    let user = id.user.id.clone();
    let admin = id.user.role == server_auth::Role::Admin;
    let Some(data_dir) = app.config.data_dir.clone() else {
        return error(404, "no such provider");
    };
    let (p, u) = (provider.clone(), user.clone());
    let found = tokio::task::spawn_blocking(move || {
        let previous = std::env::var("SECRETS_KEY_PREVIOUS")
            .ok()
            .map(Zeroizing::new);
        let keys =
            server_auth::secrets::load_existing(&data_dir, previous.as_deref().map(String::as_str))
                .ok();
        find_row(&data_dir, &u, &p, keys.as_ref())
            .map(|r| (r.raw, Zeroizing::new(r.key.to_string())))
    })
    .await
    .ok()
    .flatten();
    let chatgpt_row = |raw: &JValue| raw.get("kind").as_str() == Some(CHATGPT_KIND);
    match &found {
        Some((raw, _)) if chatgpt_row(raw) => {
            return chatgpt(app, &user, raw.get("shared").truthy(), refresh).await
        }
        None if provider == CHATGPT_ID => {
            // The broker finished a sign-in core has not listed yet: its private row.
            if connected(app, &user).await {
                return chatgpt(app, &user, false, refresh).await;
            }
            return error(404, "no such provider");
        }
        None => return error(404, "no such provider"),
        Some(_) => {}
    }
    let Some((raw, key)) = found else {
        return error(404, "no such provider");
    };
    let base_url = raw.get("baseUrl");
    let base = match base_url {
        JValue::Str(s) => s.clone(),
        v => crate::provider_test_routes::or_empty(v),
    };
    if !(base.starts_with("http://") || base.starts_with("https://")) {
        return error(400, "this provider has no address");
    }
    if !raw.get("shared").truthy() && !endpoint_approved(admin, &base) {
        return error(
            400,
            "Provider origin is not approved for member connections; contact an administrator.",
        );
    }
    let real = !key.is_empty() && key.as_str() != "local";
    let key_used = if real { key.as_str() } else { "" };
    let cache_key = digest(&[&user, &provider, &base, key_used]);
    let owner = digest(&[&user, &provider]);
    let lists = &app.provider_models;
    let seen = {
        let mut m = lists.mem();
        let now = identity::now_ms();
        let hit = m.lists.get(&cache_key);
        let throttled = refresh
            && hit.is_some()
            && now - m.last_refresh.get(&owner).copied().unwrap_or(0) < REFRESH_GAP_MS;
        if let Some(h) = hit {
            if (!refresh || throttled) && h.until > now {
                return answer(
                    StatusCode::OK,
                    &models_answer(&h.models, true, h.fetched_at, throttled),
                );
            }
        }
        if refresh {
            m.last_refresh.insert(owner.clone(), now);
        }
        m.seq
    };
    let lock = lists.lock_for(&cache_key);
    let _held = lock.lock().await;
    let shared = {
        let m = lists.mem();
        m.fetched
            .get(&cache_key)
            .filter(|(seq, _)| *seq > seen)
            .map(|(_, r)| r.clone())
    };
    let result = match shared {
        Some(r) => r,
        None => {
            let url = format!("{}/v1/models", base.strip_suffix("/v1").unwrap_or(&base));
            let fetched: Fetched = match probe(&url, key_used).await {
                Probe::Reply { ok: true, body, .. } => {
                    let models = Arc::new(model_ids(&body, LIST_MAX));
                    let fetched_at = identity::now_ms();
                    let mut m = lists.mem();
                    m.lists.retain(|_, e| e.owner != owner);
                    m.lists.insert(
                        cache_key.clone(),
                        Entry {
                            owner: owner.clone(),
                            models: Arc::clone(&models),
                            until: fetched_at + TTL_MS,
                            fetched_at,
                        },
                    );
                    while m.lists.len() > MAX_ENTRIES {
                        let Some(oldest) = m
                            .lists
                            .iter()
                            .min_by_key(|(_, e)| e.fetched_at)
                            .map(|(k, _)| k.clone())
                        else {
                            break;
                        };
                        m.lists.remove(&oldest);
                    }
                    while m.last_refresh.len() > MAX_ENTRIES {
                        let Some(oldest) = m
                            .last_refresh
                            .iter()
                            .min_by_key(|(_, at)| **at)
                            .map(|(k, _)| k.clone())
                        else {
                            break;
                        };
                        m.last_refresh.remove(&oldest);
                    }
                    Ok((models, fetched_at))
                }
                Probe::Reply { status, .. } => Err(Failure::Status(status)),
                Probe::Thrown(_) => Err(Failure::Unreachable),
            };
            let mut m = lists.mem();
            m.seq += 1;
            let seq = m.seq;
            m.fetched.insert(cache_key.clone(), (seq, fetched.clone()));
            while m.fetched.len() > MAX_ENTRIES {
                let Some(oldest) = m
                    .fetched
                    .iter()
                    .min_by_key(|(_, (s, _))| *s)
                    .map(|(k, _)| k.clone())
                else {
                    break;
                };
                m.fetched.remove(&oldest);
            }
            fetched
        }
    };
    match result {
        Ok((models, fetched_at)) => answer(
            StatusCode::OK,
            &models_answer(&models, false, fetched_at, false),
        ),
        Err(Failure::Status(status)) => error(502, &format!("provider returned {status}")),
        Err(Failure::Unreachable) => error(502, "The provider could not be reached."),
    }
}

#[cfg(feature = "rust-chatgpt-status")]
async fn connected(app: &Arc<App>, user: &str) -> bool {
    let Some(broker) = app.chatgpt_oauth.as_ref() else {
        return false;
    };
    broker.has_tokens(app, user).await
}

#[cfg(not(feature = "rust-chatgpt-status"))]
async fn connected(_app: &Arc<App>, _user: &str) -> bool {
    false
}

/// A Sign in with ChatGPT row: the broker's catalogue (routes/providers.cjs's ChatGPT branch).
#[cfg(feature = "rust-chatgpt-status")]
async fn chatgpt(app: &Arc<App>, user: &str, shared: bool, refresh: bool) -> Response<Body> {
    let Some(broker) = app.chatgpt_oauth.as_ref().map(Arc::clone) else {
        return error(404, OFF_MESSAGE);
    };
    if shared || !broker.enabled(app).await {
        return error(404, OFF_MESSAGE);
    }
    match broker.model_list(app, user, refresh).await {
        Ok(v) => answer(StatusCode::OK, &v),
        Err(e) => error(
            if e.status == 401 || e.status == 429 {
                e.status
            } else {
                502
            },
            &e.message,
        ),
    }
}

#[cfg(not(feature = "rust-chatgpt-status"))]
async fn chatgpt(_app: &Arc<App>, _user: &str, _shared: bool, _refresh: bool) -> Response<Body> {
    error(404, OFF_MESSAGE)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn decode_component_is_decode_uri_component() {
        assert_eq!(decode_component("mine").as_deref(), Some("mine"));
        assert_eq!(decode_component("a%20b").as_deref(), Some("a b"));
        assert_eq!(decode_component("%E2%9C%93").as_deref(), Some("✓"));
        assert_eq!(decode_component("a+b").as_deref(), Some("a+b"));
        assert_eq!(decode_component("%"), None);
        assert_eq!(decode_component("%zz"), None);
        assert_eq!(decode_component("%C3"), None, "invalid UTF-8 throws");
    }

    #[test]
    fn digests_separate_their_parts() {
        assert_ne!(digest(&["ab", "c"]), digest(&["a", "bc"]));
    }
}
