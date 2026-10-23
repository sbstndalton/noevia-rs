//! The gate's own answers.

use axum::body::Body;
use axum::http::{HeaderValue, StatusCode};
use axum::response::Response;
use inference_admission::Refusal;

/// A JSON answer with `status`.
pub(crate) fn json(status: u16, value: &serde_json::Value) -> Response {
    let mut response = Response::new(Body::from(value.to_string()));
    *response.status_mut() =
        StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    response
        .headers_mut()
        .insert("content-type", HeaderValue::from_static("application/json"));
    response
}

/// A refusal in the chat-completions error shape clients already read, with a stable code.
pub(crate) fn refusal(refusal: Refusal) -> Response {
    let body = serde_json::json!({
        "error": {
            "code": refusal.code(),
            "message": refusal.message(),
            "type": "gate_error",
            "retryable": refusal.retryable(),
        }
    });
    let mut response = json(refusal.http_status(), &body);
    if let Ok(code) = HeaderValue::from_str(refusal.code()) {
        response.headers_mut().insert("x-noevia-gate-refusal", code);
    }
    if refusal.retryable() {
        response
            .headers_mut()
            .insert("retry-after", HeaderValue::from_static("2"));
    }
    response
}

/// A plain status with no body.
pub(crate) fn empty(status: StatusCode) -> Response {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = status;
    response
}
