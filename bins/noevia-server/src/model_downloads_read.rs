//! Exact GET download status; the selected engine adapter still owns its tracker.
//!
//! Core never capped this answer, so neither the owner's facts nor the answer have a size cap here.
//! Everything that can fail happens before a header: the facts are read whole, checked and turned into
//! the one finished answer, and only then is a response made. Its body is that fixed buffer handed out
//! in [`CHUNK`]-byte frames and cannot fail, so a fault while staging is the generic 500 with no byte of
//! a listing sent. A client that leaves drops the handler future, which drops the owner's reply
//! (closing that loopback connection) or the staged buffer; nothing else is held.
use std::convert::Infallible;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::{body::Body, http::StatusCode, response::Response};
use bytes::Bytes;
use http_body::{Frame, SizeHint};
use js_json::JValue;
use serde::Deserialize;
use serde_json::value::RawValue;
use std::collections::BTreeMap;

use crate::{reply, App};

pub const PATH: &str = "/api/models/downloads";
const FACTS: &str = "/internal/model-downloads-read/facts";

// Keep arbitrary selected values as validated raw JSON. Parsing a selected value
// into JValue would impose its reserved-Unicode and recursion limits on legacy
// output; RawValue validates iteratively and borrows the owner's canonical bytes.
#[derive(Deserialize)]
struct Facts<'a> {
    kind: String,
    #[serde(borrow)]
    rows: Option<&'a RawValue>,
}

#[derive(Deserialize)]
struct Row<'a> {
    #[serde(borrow)]
    fields: BTreeMap<String, &'a RawValue>,
    #[serde(default)]
    infinite: Vec<String>,
}

impl Row<'_> {
    fn infinite(&self, key: &str) -> bool {
        self.infinite.iter().any(|name| name == key)
    }

    fn number(&self, key: &str) -> Option<f64> {
        if self.infinite(key) {
            return Some(f64::INFINITY);
        }
        self.fields.get(key)?.get().parse().ok()
    }

    fn first_truthy(&self, keys: &[&str]) -> &str {
        for key in keys {
            if self.infinite(key) {
                return "null";
            }
            if let Some(raw) = self.fields.get(*key) {
                let text = raw.get();
                let truthy = match text.as_bytes().first() {
                    Some(b'{' | b'[' | b't') => true,
                    Some(b'"') => text != "\"\"",
                    Some(b'n' | b'f') => false,
                    _ => text.parse::<f64>().is_ok_and(|n| n != 0.0 && !n.is_nan()),
                };
                if truthy {
                    return text;
                }
            }
        }
        "\"\""
    }
}

/// The owner chose the listing; all public selection/normalization stays here.
/// Values are borrowed raw slices, so nested values need no recursive allocation
/// or destruction and lone surrogates and every valid Unicode scalar survive.
fn view(raw: &RawValue) -> Option<String> {
    let rows: Vec<Option<Row<'_>>> = serde_json::from_str(raw.get()).ok()?;
    let mut body = String::from("[");
    for (index, row) in rows.iter().enumerate() {
        let row = row.as_ref()?;
        if index != 0 {
            body.push(',');
        }
        let progress = row
            .number("percent")
            .map(|n| n / 100.0)
            .or_else(|| row.number("progress"));
        let progress = js_json::stringify(&progress.map_or(JValue::Null, JValue::Num))?;
        body.push_str("{\"id\":");
        body.push_str(row.first_truthy(&["id", "job_id"]));
        body.push_str(",\"model\":");
        body.push_str(row.first_truthy(&["model_name", "model", "checkpoint"]));
        body.push_str(",\"progress\":");
        body.push_str(&progress);
        body.push_str(",\"status\":");
        body.push_str(row.first_truthy(&["status", "state"]));
        body.push('}');
    }
    body.push(']');
    Some(body)
}

/// The most one frame of the answer carries.
const CHUNK: usize = 64 * 1024;

/// The staged answer: immutable, complete before any header, sent in bounded frames.
struct Staged(Bytes);

impl Staged {
    fn new(body: String) -> Self {
        Staged(Bytes::from(body))
    }
}

impl http_body::Body for Staged {
    type Data = Bytes;
    type Error = Infallible;
    fn poll_frame(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        if self.0.is_empty() {
            return Poll::Ready(None);
        }
        let n = self.0.len().min(CHUNK);
        Poll::Ready(Some(Ok(Frame::data(self.0.split_to(n)))))
    }
    fn is_end_stream(&self) -> bool {
        self.0.is_empty()
    }
    fn size_hint(&self) -> SizeHint {
        SizeHint::with_exact(u64::try_from(self.0.len()).unwrap_or(u64::MAX))
    }
}

fn unavailable() -> Response<Body> {
    reply::error(StatusCode::BAD_GATEWAY, "Download status unavailable", true)
}

fn internal() -> Response<Body> {
    reply::error(StatusCode::INTERNAL_SERVER_ERROR, "Internal error", true)
}

enum FactsFailure {
    Unavailable,
    Internal,
}

/// The facts reply read whole. `Err`: the failure to send instead.
async fn facts(app: &App) -> Result<Vec<u8>, FactsFailure> {
    let Some(mut res) = app
        .config
        .research
        .node_open(FACTS, &serde_json::json!({}), Duration::from_secs(10))
        .await
    else {
        return Err(FactsFailure::Unavailable);
    };
    if res.status().as_u16() != 200 {
        return Err(FactsFailure::Unavailable);
    }
    let mut bytes = Vec::new();
    loop {
        match res.chunk().await {
            Ok(Some(chunk)) => bytes.extend_from_slice(&chunk),
            Ok(None) => return Ok(bytes),
            // The owner answered and then failed: a late fault, nothing of it is used.
            Err(_) => return Err(FactsFailure::Internal),
        }
    }
}

pub async fn serve(app: &App) -> Response<Body> {
    let bytes = match facts(app).await {
        Ok(bytes) => bytes,
        Err(FactsFailure::Unavailable) => return unavailable(),
        Err(FactsFailure::Internal) => return internal(),
    };
    let Ok(facts) = serde_json::from_slice::<Facts<'_>>(&bytes) else {
        return unavailable();
    };
    let result = match facts.kind.as_str() {
        "disabled" => Some(String::from("[]")),
        "listing" => facts.rows.and_then(view),
        "fault" => None,
        _ => return unavailable(),
    };
    let Some(body) = result else {
        return internal();
    };
    drop(facts);
    drop(bytes);
    // The same headers as reply::json_text; only the body is the staged one.
    let mut res = reply::json_text(StatusCode::OK, String::new(), true);
    *res.body_mut() = Body::new(Staged::new(body));
    res
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body::Body as _;

    fn frames(mut body: Staged) -> Vec<Bytes> {
        let waker = std::task::Waker::noop();
        let mut cx = Context::from_waker(waker);
        let mut out = Vec::new();
        while let Poll::Ready(Some(Ok(frame))) = Pin::new(&mut body).poll_frame(&mut cx) {
            out.push(frame.into_data().unwrap_or_default());
        }
        assert!(body.is_end_stream());
        out
    }

    #[test]
    fn a_staged_answer_is_sent_whole_in_bounded_frames() {
        let text = "a".repeat(CHUNK * 3 + 7);
        let staged = Staged::new(text.clone());
        assert_eq!(staged.size_hint().exact(), Some(text.len() as u64));
        let sent = frames(staged);
        assert_eq!(sent.len(), 4);
        assert!(sent.iter().all(|f| !f.is_empty() && f.len() <= CHUNK));
        assert_eq!(sent.concat(), text.as_bytes());
        assert!(frames(Staged::new(String::new())).is_empty());
    }
}
