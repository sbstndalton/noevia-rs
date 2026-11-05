//! Row C PARTIAL: ordinary GET forwarding, directly to the native manager service.
//! Scan, autoconfig budget policy and benchmark evidence publication retain their named owners.
use crate::{identity, model_manager_scan, reply, App};
use axum::{
    body::Body,
    http::{Request, Response, StatusCode},
    response::IntoResponse,
};
use js_json::JValue;
use std::sync::Arc;

const PREFIX: &str = "/api/model-manager/";

// decodeURIComponent rejects incomplete escapes and invalid UTF-8 (URL form decoding does not).
fn decode(s: &str) -> Option<String> {
    let mut bytes = Vec::new();
    let mut it = s.bytes();
    while let Some(c) = it.next() {
        if c == b'%' {
            let hi = char::from(it.next()?).to_digit(16)?;
            let lo = char::from(it.next()?).to_digit(16)?;
            bytes.push((hi * 16 + lo) as u8);
        } else {
            bytes.push(c);
        }
    }
    String::from_utf8(bytes).ok()
}
fn decoded(path: &str) -> Option<String> {
    path.strip_prefix(PREFIX)?
        .split('/')
        .map(decode)
        .collect::<Option<Vec<_>>>()
        .map(|v| v.join("/"))
}
pub fn owned(method: &str, path: &str) -> bool {
    if method != "GET" || !path.starts_with(PREFIX) || path == model_manager_scan::PATH {
        return false;
    }
    let Some(rest) = decoded(path) else {
        return true;
    };
    let segments: Vec<_> = rest.split('/').collect();
    !matches!(segments.as_slice(), ["sections", name, "autoconfig"] if !name.is_empty())
        && !matches!(segments.as_slice(), ["benchmark", "runs", id] if !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()))
}
fn valid(path: &str) -> bool {
    let Some(raw) = path.strip_prefix(PREFIX) else {
        return false;
    };
    if raw.contains("..")
        || !raw
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_./%:+@-".contains(&b))
    {
        return false;
    }
    raw.split('/')
        .all(|s| decode(s).is_some_and(|s| s != "." && s != ".." && !s.contains(['/', '\\'])))
}
pub async fn serve(
    app: &Arc<App>,
    conn: crate::serve::Conn,
    req: Request<Body>,
    path: String,
) -> Response<Body> {
    if !owned(req.method().as_str(), &path) {
        return app.proxy.forward(conn.peer, req).await;
    }
    let (parts, _) = req.into_parts();
    let creds = identity::creds_of(&parts);
    let gate = Arc::clone(&app.identity);
    let gate_path = path.clone();
    let id = match tokio::task::spawn_blocking(move || {
        gate.gate_blocking(&creds, &gate_path, identity::now_ms())
    })
    .await
    {
        Ok(Ok(id)) => id,
        Ok(Err(r)) => return r.into_response(),
        Err(_) => return identity::Rejection::Unavailable.into_response(),
    };
    let error = |status, text| reply::error(status, text, true);
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
    if !valid(&path) {
        return error(StatusCode::BAD_REQUEST, "Invalid path");
    }
    let raw = path.strip_prefix(PREFIX).unwrap_or_default();
    let Some((status, body)) = loader
        .get_bounded(
            &format!("/api/v1/{raw}"),
            parts.uri.query().unwrap_or(""),
            std::time::Duration::from_secs(600),
        )
        .await
    else {
        return model_manager_scan::answer(
            StatusCode::BAD_GATEWAY,
            &JValue::obj([(
                "error",
                JValue::Str("The model management service is not responding.".into()),
            )]),
            false,
        );
    };
    let Some(body) = body else {
        return model_manager_scan::answer(
            StatusCode::BAD_GATEWAY,
            &JValue::obj([(
                "error",
                JValue::Str("The model management service is not responding.".into()),
            )]),
            false,
        );
    };
    let body = model_manager_scan::detail(body);
    let body = if status.is_success() {
        body
    } else {
        let reason = if body.get("detail").truthy() {
            body.get("detail").clone()
        } else if body.get("error").truthy() {
            body.get("error").clone()
        } else {
            JValue::Str("Model management request failed.".into())
        };
        JValue::obj([("error", reason)])
    };
    model_manager_scan::answer(status, &body, false)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn scope_and_path_validation() {
        assert!(owned("GET", "/api/model-manager/sections"));
        assert!(!owned("POST", "/api/model-manager/sections"));
        assert!(!owned("GET", "/api/model-manager"));
        assert!(!owned("GET", "/api/model-manager/sections/a/%61utoconfig"));
        for raw in ["a%2fb", "a%5cb", "%ff", "%", "a..b", "%2e", "x!"] {
            assert!(!valid(&format!("{PREFIX}{raw}")));
        }
        assert!(valid("/api/model-manager/sections/a%20b"));
    }
}
