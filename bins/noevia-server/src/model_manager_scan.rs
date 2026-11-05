//! Native folder scan policy and G1 generation-fenced cache.
use crate::{identity, reply, App};
use axum::{
    body::Body,
    http::{header, HeaderValue, Method, Request, Response, StatusCode},
    response::IntoResponse,
};
use js_json::{wtf16, JValue};
use std::{sync::Arc, time::Duration};
pub const PATH: &str = "/api/model-manager/models";
const CAP: usize = 8 * 1024 * 1024;
#[derive(Clone)]
pub struct Loader {
    base: Option<String>,
    token: Option<String>,
    client: reqwest::Client,
}
// Config is Debug, but neither the configured URL nor its token may enter diagnostics.
impl std::fmt::Debug for Loader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Loader")
            .field("configured", &self.base.is_some())
            .finish_non_exhaustive()
    }
}
impl Loader {
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        Ok(Self {
            base: get("MODEL_LOADER_URL")
                .filter(|v| !v.is_empty())
                .map(|v| v.trim_end_matches('/').to_owned()),
            token: get("MODEL_LOADER_TOKEN").filter(|v| !v.is_empty()),
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .no_proxy()
                .build()
                .map_err(|_| "model scan client unavailable".to_owned())?,
        })
    }
    pub(crate) fn configured(&self) -> bool {
        self.base.is_some()
    }

    async fn get(&self, query: &str) -> Option<(StatusCode, bool, JValue)> {
        let (status, body) = self
            .get_bounded("/api/v1/models", query, Duration::from_secs(600))
            .await?;
        Some((status, status.is_success(), body?))
    }

    pub(crate) async fn idle_unload(&self) -> Option<(StatusCode, Option<JValue>)> {
        self.get_bounded("/api/v1/idle-unload", "", Duration::from_secs(30))
            .await
    }

    pub(crate) async fn facts_get(
        &self,
        path: &str,
        seconds: u64,
    ) -> Option<(StatusCode, Option<JValue>)> {
        self.get_bounded(path, "", Duration::from_secs(seconds))
            .await
    }
    async fn get_bounded(
        &self,
        path: &str,
        query: &str,
        timeout: Duration,
    ) -> Option<(StatusCode, Option<JValue>)> {
        let base = self.base.as_ref()?;
        let endpoint = if query.is_empty() {
            format!("{base}{path}")
        } else {
            format!("{base}{path}?{query}")
        };
        let url = reqwest::Url::parse(&endpoint).ok()?;
        // Node fetch refuses userinfo; never silently turn a URL into Basic credentials.
        if !url.username().is_empty() || url.password().is_some() {
            return None;
        }
        let mut request = self
            .client
            .get(url)
            .header(header::CONTENT_TYPE, "application/json")
            .timeout(timeout);
        if let Some(token) = &self.token {
            let mut value = HeaderValue::from_str(token).ok()?;
            value.set_sensitive(true);
            request = request.header("x-model-loader-token", value);
        }
        let mut res = request.send().await.ok()?;
        let status = res.status();
        if [301, 302, 303, 307, 308].contains(&status.as_u16())
            && res.headers().contains_key(header::LOCATION)
        {
            return None;
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = res.chunk().await.ok()? {
            if chunk.len() > CAP.saturating_sub(bytes.len()) {
                return Some((status, None));
            }
            bytes.extend_from_slice(&chunk);
        }
        let text = String::from_utf8_lossy(&bytes);
        let value = wtf16::parse(&text).unwrap_or_else(|_| JValue::Str(text.into_owned()));
        Some((status, Some(value)))
    }
}
fn error(status: StatusCode, message: &str) -> Response<Body> {
    reply::error(status, message, true)
}
fn unavailable() -> Response<Body> {
    error(
        StatusCode::SERVICE_UNAVAILABLE,
        "Model scan cache is unavailable. Try again shortly.",
    )
}
fn answer(status: StatusCode, body: &JValue, cached: bool) -> Response<Body> {
    let Some(text) = wtf16::stringify(body) else {
        return unavailable();
    };
    let mut res = reply::json_text(status, text, true);
    res.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    if cached {
        res.headers_mut()
            .insert("x-model-scan", HeaderValue::from_static("cached"));
    }
    res
}
fn stale(now: f64, at: f64) -> bool {
    now - at > 5000.0
}
fn opaque<'a>(v: &'a JValue, key: &str) -> Option<&'a str> {
    v.get(key).as_str().filter(|s| {
        s.len() == 64
            && s.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}
fn detail(body: JValue) -> JValue {
    if matches!(body, JValue::Obj(_) | JValue::Arr(_)) {
        body
    } else {
        let text = if body.truthy() {
            js_json::to_js_string(&body).unwrap_or_default()
        } else {
            String::new()
        };
        JValue::obj([("error", JValue::Str(text))])
    }
}
// Validate raw segments before WHATWG normalization or forwarding can hide traversal.
pub fn invalid_manager_path(path: &str) -> bool {
    let Some(rest) = path.strip_prefix("/api/model-manager/") else {
        return false;
    };
    if rest.contains("..")
        || !rest
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_./%:+@-".contains(&b))
    {
        return true;
    }
    rest.split('/').any(|part| {
        let mut decoded = Vec::new();
        let mut bytes = part.bytes();
        while let Some(b) = bytes.next() {
            if b == b'%' {
                let hex = |b: u8| (b as char).to_digit(16);
                let Some(a) = bytes.next().and_then(hex) else {
                    return true;
                };
                let Some(b) = bytes.next().and_then(hex) else {
                    return true;
                };
                decoded.push((a * 16 + b) as u8);
            } else {
                decoded.push(b);
            }
        }
        let Ok(text) = std::str::from_utf8(&decoded) else {
            return true;
        };
        text == "." || text == ".." || text.contains(['/', '\\'])
    })
}
pub async fn reject_path(app: &Arc<App>, req: Request<Body>) -> Response<Body> {
    let (parts, _) = req.into_parts();
    let creds = identity::creds_of(&parts);
    let gate = Arc::clone(&app.identity);
    let id = match tokio::task::spawn_blocking(move || {
        gate.gate_blocking(&creds, PATH, identity::now_ms())
    })
    .await
    {
        Ok(Ok(id)) => id,
        Ok(Err(r)) => return r.into_response(),
        Err(_) => return identity::Rejection::Unavailable.into_response(),
    };
    if id.user.role != server_auth::Role::Admin {
        return error(
            StatusCode::FORBIDDEN,
            "Administrator required for model management",
        );
    }
    if !app.config.model_manager_scan.configured() {
        return error(
            StatusCode::NOT_FOUND,
            "Model management service is not configured",
        );
    }
    error(StatusCode::BAD_REQUEST, "Invalid path")
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
    let creds = identity::creds_of(&parts);
    let gate = Arc::clone(&app.identity);
    let id = match tokio::task::spawn_blocking(move || {
        gate.gate_blocking(&creds, PATH, identity::now_ms())
    })
    .await
    {
        Ok(Ok(id)) => id,
        Ok(Err(r)) => return r.into_response(),
        Err(_) => return identity::Rejection::Unavailable.into_response(),
    };
    if id.user.role != server_auth::Role::Admin {
        return error(
            StatusCode::FORBIDDEN,
            "Administrator required for model management",
        );
    }
    let loader = &app.config.model_manager_scan;
    if !loader.configured() {
        return error(
            StatusCode::NOT_FOUND,
            "Model management service is not configured",
        );
    }
    let query = parts.uri.query().unwrap_or("");
    // A nonempty query bypasses the shared cache entirely. A bare '?' has URL.search == ''.
    let snapshot = if query.is_empty() {
        let Some(value) = app.config.model_facts.scan_snapshot() else {
            return unavailable();
        };
        if opaque(&value, "token").is_none() || opaque(&value, "entryToken").is_none() {
            return unavailable();
        }
        let hit = value.get("hit");
        if !matches!(hit, JValue::Null) {
            if !matches!(hit, JValue::Obj(_)) || matches!(hit.get("body"), JValue::Undefined) {
                return unavailable();
            }
            let (Some(now), Some(at)) = (value.get("now").as_num(), hit.get("at").as_num()) else {
                return unavailable();
            };
            if !now.is_finite() || !at.is_finite() {
                return unavailable();
            }
            if stale(now, at) {
                // The owner merely schedules its existing single in-flight refresher if this
                // exact cache entry is still current; it never chooses public freshness policy.
                if let Some(token) = opaque(&value, "entryToken").map(str::to_owned) {
                    let app = Arc::clone(app);
                    tokio::spawn(async move {
                        if let Some(generation) = app.config.model_facts.scan_refresh_token(&token)
                        {
                            let _ = crate::model_facts::shared_scan(&app, &generation, 15).await;
                        }
                    });
                }
            }
            return answer(StatusCode::OK, hit.get("body"), true);
        }
        Some(value)
    } else {
        None
    };
    let Some((status, ok, body)) = loader.get(query).await else {
        return error(
            StatusCode::BAD_GATEWAY,
            "The model management service is not responding.",
        );
    };
    let body = detail(body);
    if ok {
        if let Some(snapshot) = snapshot {
            // Publication is conditional and best effort. A restarted owner or intervening write
            // cannot accept the old epoch/generation. Never retry against a new instance/token.
            if let Some(token) = opaque(&snapshot, "token") {
                app.config.model_facts.scan_publish(token, body.clone());
            }
        }
        answer(status, &body, false)
    } else {
        let reason = if body.get("detail").truthy() {
            body.get("detail").clone()
        } else if body.get("error").truthy() {
            body.get("error").clone()
        } else {
            JValue::Str("Model management request failed.".into())
        };
        answer(status, &JValue::obj([("error", reason)]), false)
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    #[test]
    fn cache_freshness_matches_recorded_node_thresholds() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../tests/fixtures/model-manager-scan-node.json"
        ))
        .expect("oracle fixture");
        for row in fixture["rows"].as_array().expect("fixture rows") {
            let Some(age) = row["spec"]["age"].as_f64() else {
                continue;
            };
            let query = row["spec"]["search"].as_str().unwrap_or("");
            if (query.is_empty() || query == "?") && row["spec"]["method"].is_null() {
                assert_eq!(
                    stale(10_000.0, 10_000.0 - age),
                    row["refreshes"].as_u64() == Some(1)
                );
            }
        }
    }
    #[test]
    fn config_diagnostics_never_print_loader_url_or_credentials() {
        let loader = Loader::from_lookup(|key| match key {
            "MODEL_LOADER_URL" => Some(
                "http://synthetic-user:synthetic-password@synthetic-loader.invalid/private-target"
                    .into(),
            ),
            "MODEL_LOADER_TOKEN" => Some("must-stay-private".into()),
            _ => None,
        })
        .expect("synthetic loader config");
        let diagnostic = format!("{loader:?}");
        assert!(diagnostic.contains("configured: true"));
        assert!(!diagnostic.contains("synthetic-loader"));
        assert!(!diagnostic.contains("synthetic-user"));
        assert!(!diagnostic.contains("synthetic-password"));
        assert!(!diagnostic.contains("private-target"));
        assert!(!diagnostic.contains("must-stay-private"));
    }
}
