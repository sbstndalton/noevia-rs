//! The reply shapes Node uses for natively answered requests: the security headers
//! index.cjs sets on every response (HSTS and Permissions-Policy included, #1480), X-Noevia-API on /api/ paths, and http.cjs json().

use axum::body::Body;
use axum::http::{header, HeaderMap, HeaderValue, Response, StatusCode};

pub const HSTS: &str = "max-age=15552000";
pub const PERMISSIONS_POLICY: &str = "camera=(), microphone=(), geolocation=(), payment=(), usb=(), serial=(), bluetooth=(), hid=(), midi=(), magnetometer=(), gyroscope=(), accelerometer=()";
pub const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; font-src 'self'; img-src 'self' data:; connect-src 'self'; frame-ancestors 'none'; base-uri 'self'; form-action 'self'";

/// index.cjs handleRequestScoped / handleRequest: set before any route answers.
pub fn security_headers(h: &mut HeaderMap, api: bool) {
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("same-origin"),
    );
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CSP),
    );
    // #1480: the same two headers as index.cjs (six months, no includeSubDomains; features off).
    h.insert(
        header::STRICT_TRANSPORT_SECURITY,
        HeaderValue::from_static(HSTS),
    );
    h.insert(
        "permissions-policy",
        HeaderValue::from_static(PERMISSIONS_POLICY),
    );
    if api {
        h.insert("x-noevia-api", HeaderValue::from_static("1"));
    }
}

/// http.cjs json(): Content-Type application/json, Cache-Control no-store.
pub fn json(status: StatusCode, body: &serde_json::Value, api: bool) -> Response<Body> {
    json_text(status, body.to_string(), api)
}

/// [`json`] with the body already serialised, for bodies whose key order must be Node's
/// (`JSON.stringify` keeps insertion order; serde_json's map sorts keys).
pub fn json_text(status: StatusCode, body: String, api: bool) -> Response<Body> {
    let mut res = Response::new(Body::from(body));
    *res.status_mut() = status;
    let h = res.headers_mut();
    security_headers(h, api);
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    res
}

/// A JSON error from the front itself (no Node involved), e.g. the body cap or Node down.
pub fn error(status: StatusCode, message: &str, api: bool) -> Response<Body> {
    json(status, &serde_json::json!({ "error": message }), api)
}

/// code-net-guard.cjs deny(): 403, no body, no detail. The guard runs before index.cjs sets its
/// headers, so these three are the only ones.
pub fn code_net_refused() -> Response<Body> {
    let mut res = Response::new(Body::empty());
    *res.status_mut() = StatusCode::FORBIDDEN;
    let h = res.headers_mut();
    h.insert(header::CONTENT_LENGTH, HeaderValue::from_static("0"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(header::CONNECTION, HeaderValue::from_static("close"));
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    // #1480: every natively answered reply carries HSTS (six months, never includeSubDomains) and a
    // Permissions-Policy that switches off the features the app never calls, as index.cjs does.
    #[test]
    fn security_headers_carry_hsts_and_permissions_policy() {
        for api in [false, true] {
            let mut h = HeaderMap::new();
            security_headers(&mut h, api);
            assert_eq!(
                h.get("strict-transport-security").map(|v| v.as_bytes()),
                Some(HSTS.as_bytes())
            );
            assert!(!HSTS.contains("includeSubDomains"));
            assert!(HSTS
                .strip_prefix("max-age=")
                .and_then(|n| n.parse::<u64>().ok())
                .is_some_and(|n| n >= 15_552_000));
            assert_eq!(
                h.get("permissions-policy").map(|v| v.as_bytes()),
                Some(PERMISSIONS_POLICY.as_bytes())
            );
            for feature in ["camera", "microphone", "geolocation", "payment", "usb"] {
                assert!(
                    PERMISSIONS_POLICY.contains(&format!("{feature}=()")),
                    "{feature}"
                );
            }
        }
    }
}
