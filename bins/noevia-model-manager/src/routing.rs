//! Native public framework routing. ASGI path decoding occurs exactly once.
use crate::StateData;
use axum::{
    body::Body,
    http::{Method, Uri},
    response::Response,
    Extension, Router,
};
use std::sync::Arc;
#[derive(Clone, Copy)]
pub(crate) struct Enabled;
#[derive(Clone)]
pub struct Routed {
    pub original: Uri,
    pub decoded: String,
    pub backend: Option<(String, &'static str)>,
}
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Route {
    Match {
        canonical: String,
        backend: Option<(String, &'static str)>,
    },
    Method(&'static str),
    Redirect(String),
    NotFound,
}
// API registration order matters: Starlette answers the first partial match on405.
const ROUTES: &[(&str, &str)] = &[
    ("GET", "/api/v1/overview"),
    ("GET", "/api/v1/models"),
    ("GET", "/api/v1/models/detail"),
    ("POST", "/api/v1/models/delete"),
    ("GET", "/api/v1/sections"),
    ("GET", "/api/v1/sections/{name}"),
    ("PUT", "/api/v1/sections/{name}"),
    ("PUT", "/api/v1/models-ini"),
    ("POST", "/api/v1/sections/{name}/safe-defaults"),
    ("GET", "/api/v1/sections/{name}/draft-heads"),
    ("POST", "/api/v1/sections/{name}/rename"),
    ("DELETE", "/api/v1/sections/{name}"),
    ("GET", "/api/v1/sections/{name}/autoconfig"),
    ("GET", "/api/v1/backends"),
    ("POST", "/api/v1/backends/{name}/restart"),
    ("POST", "/api/v1/backends/{name}/test"),
    ("GET", "/api/v1/host"),
    ("GET", "/api/v1/backends/{name}/diagnose"),
    ("GET", "/api/v1/settings"),
    ("PUT", "/api/v1/settings"),
    ("GET", "/api/v1/search"),
    ("GET", "/api/v1/search/repo"),
    ("GET", "/api/v1/downloads"),
    ("GET", "/api/v1/download-targets"),
    ("POST", "/api/v1/sections/{name}/draft-heads/download"),
    ("POST", "/api/v1/downloads"),
    ("POST", "/api/v1/downloads/{job_id}/cancel"),
    ("POST", "/api/v1/downloads/clear"),
    ("POST", "/api/v1/models/check-updates"),
    ("GET", "/api/v1/models/updates"),
    ("GET", "/api/v1/prompts"),
    ("POST", "/api/v1/prompts"),
    ("DELETE", "/api/v1/prompts/{pid}"),
    ("GET", "/api/v1/benchmark"),
    ("POST", "/api/v1/benchmark/start"),
    ("POST", "/api/v1/benchmark/sweep"),
    ("GET", "/api/v1/benchmark/progress"),
    ("POST", "/api/v1/benchmark/cancel"),
    ("GET", "/api/v1/benchmark/runs/{run_id}"),
    ("PUT", "/api/v1/badges"),
    ("DELETE", "/api/v1/badges"),
    // Existing parent extensions. Their availability is checked separately by dispatch.
    ("GET", "/api/v1/health"),
    ("GET", "/api/v1/backends/{name}/logs"),
    ("GET", "/api/v1/embedding-service"),
    ("PUT", "/api/v1/embedding-service"),
    ("GET", "/api/v1/idle-unload"),
    ("PUT", "/api/v1/idle-unload"),
];
fn matches(pattern: &str, path: &str) -> Option<String> {
    // Python regex '$' also matches before one final newline. A final dynamic
    // parameter greedily captures that newline instead; preserve its value.
    let path = if !pattern.ends_with('}') {
        path.strip_suffix('\n').unwrap_or(path)
    } else {
        path
    };
    let mut parts = path.split('/');
    let mut out = Vec::new();
    for expected in pattern.split('/') {
        let actual = parts.next()?;
        if expected.starts_with('{') {
            if actual.is_empty() {
                return None;
            }
        } else if actual != expected {
            return None;
        }
        out.push(actual);
    }
    parts.next().is_none().then(|| out.join("/"))
}
fn backend(path: &str) -> Option<(String, &'static str)> {
    let rest = path.strip_prefix("/api/v1/backends/")?;
    let (name, action) = rest.split_once('/')?;
    match action {
        "restart" => Some((name.into(), "restart")),
        "test" => Some((name.into(), "test")),
        _ => None,
    }
}
pub(crate) fn classify(method: &Method, decoded: &str, embedding: bool, idle: bool) -> Route {
    let mut partial = None;
    let available = |pattern: &str| {
        (!pattern.contains("/embedding-service") || embedding)
            && (!pattern.contains("/idle-unload") || idle)
    };
    for &(allowed, pattern) in ROUTES {
        if !available(pattern) {
            continue;
        }
        if let Some(canonical) = matches(pattern, decoded) {
            if method.as_str() == allowed {
                let backend = backend(&canonical);
                return Route::Match { canonical, backend };
            }
            if partial.is_none() {
                partial = Some(allowed);
            }
        }
    }
    if let Some(allowed) = partial {
        return Route::Method(allowed);
    }
    if decoded != "/" {
        let alternate = if decoded.ends_with('/') {
            decoded.trim_end_matches('/').to_owned()
        } else {
            format!("{decoded}/")
        };
        if ROUTES
            .iter()
            .any(|(_, pattern)| available(pattern) && matches(pattern, &alternate).is_some())
        {
            return Route::Redirect(alternate);
        }
    }
    Route::NotFound
}
/// Re-encode every non-unreserved byte except structural slash. A literal percent
/// must remain protected when existing endpoint parsers decode their input once.
pub(crate) fn canonical_path(decoded: &str) -> String {
    quote(decoded, false)
}
fn quote(value: &str, location: bool) -> String {
    let mut out = String::new();
    for b in value.bytes() {
        if b.is_ascii_alphanumeric()
            || b"-._~/".contains(&b)
            || (location && b":%#?=@[]!$&'()*+,;".contains(&b))
        {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}
pub(crate) fn location(path: &str, original: &Uri, host: &str) -> String {
    let query = original
        .query()
        .map(|q| format!("?{q}"))
        .unwrap_or_default();
    quote(&format!("http://{host}{path}{query}"), true)
}
pub(crate) fn framework(
    status: axum::http::StatusCode,
    text: &'static str,
    allow: Option<&'static str>,
    method: &Method,
) -> Response {
    let mut response = crate::detail(status, text);
    if let Some(allow) = allow {
        response.headers_mut().insert(
            axum::http::header::ALLOW,
            axum::http::HeaderValue::from_static(allow),
        );
    }
    if *method == Method::HEAD {
        *response.body_mut() = Body::empty();
    }
    response
}
pub fn router(state: Arc<StateData>) -> Result<Router, &'static str> {
    validate_owners(&state)?;
    Ok(crate::router(state).layer(Extension(Enabled)))
}
#[cfg(test)]
mod tests;

pub fn validate_owners(state: &StateData) -> Result<(), &'static str> {
    let owners = [
        state.diagnosis.is_some(),
        state.prompts.is_some(),
        state.settings.is_some(),
        state.model_updates.is_some(),
        state.model_detail.is_some(),
        state.model_list.is_some(),
        state.download_targets.is_some(),
        state.badges.is_some(),
        state.bench_runs.is_some(),
        state.section_writes.is_some(),
        state.settings_write.is_some(),
        state.backends.is_some(),
        state.benchmark_overview.is_some(),
        state.check_updates.is_some(),
        state.downloads.is_some(),
        state.model_delete.is_some(),
        state.draft_heads.is_some(),
        state.search.is_some(),
        state.backend_stats.is_some(),
        state.bench_runner.is_some(),
        state.sections.is_some(),
        state.host.is_some(),
        state.section_autoconfig.is_some(),
    ];
    if !owners.iter().all(|v| *v) {
        return Err("native routing requires every public native owner");
    }
    let list = state
        .model_list
        .as_ref()
        .ok_or("native routing needs native model lists")?;
    if list.route(&Method::GET, "/api/v1/models").is_none()
        || list.route(&Method::GET, "/api/v1/overview").is_none()
    {
        return Err("native routing requires both model list routes");
    }
    if !state
        .downloads
        .as_ref()
        .is_some_and(|d| d.planner().is_some())
        || !state
            .section_writes
            .as_ref()
            .is_some_and(|w| w.has_native_defaults())
    {
        return Err("native routing requires installed native planners");
    }
    if !state.search.as_ref().is_some_and(|s| s.routing_ready())
        || !state
            .section_autoconfig
            .as_ref()
            .is_some_and(|s| s.routing_ready())
        || !state
            .bench_runner
            .as_ref()
            .is_some_and(|s| s.routing_ready())
        || !state
            .settings_write
            .as_ref()
            .is_some_and(|s| s.routing_ready())
    {
        return Err("native routing requires installed native runtime helpers and nonempty trusted publisher sets");
    }
    Ok(())
}

/// Bounded read-only trust preflight, before store initialization or startup control.
pub fn trusted_preflight(
    path: &std::path::Path,
) -> Result<crate::search::live::Trusted, &'static str> {
    let trusted = crate::search::live::load_trusted(path)?;
    if !trusted.routing_ready() {
        return Err("native routing requires nonempty trusted publisher sets");
    }
    Ok(trusted)
}
