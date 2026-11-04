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
use js_json::{wtf16, JValue};

use crate::{reply, App};

pub const PATH: &str = "/api/models/downloads";
const FACTS: &str = "/internal/model-downloads-read/facts";

fn row_field(row: &JValue, key: &str) -> JValue {
    if matches!(row.get("infinite"), JValue::Arr(keys) if keys.iter().any(|v| v.as_str() == Some(key)))
    {
        // The sign does not affect truthiness or the final JSON representation.
        JValue::Num(f64::INFINITY)
    } else {
        row.get("fields").get(key).clone()
    }
}

fn first_truthy(row: &JValue, keys: &[&str]) -> JValue {
    keys.iter()
        .map(|key| row_field(row, key))
        .find(JValue::truthy)
        .unwrap_or_else(|| JValue::Str(String::new()))
}

/// The rows core's adapter chose (it applies the old `jobs || downloads || []` choice and sends only
/// that array). `None`: core's map would have thrown.
fn view(facts: &JValue) -> Option<JValue> {
    let JValue::Arr(rows) = facts.get("rows") else {
        return None;
    };
    rows.iter()
        .map(|row| {
            if !row.get("fields").is_object() {
                return None;
            }
            let progress = row_field(row, "percent")
                .as_num()
                .map(|n| n / 100.0)
                .or_else(|| row_field(row, "progress").as_num());
            Some(JValue::obj([
                ("id", first_truthy(row, &["id", "job_id"])),
                (
                    "model",
                    first_truthy(row, &["model_name", "model", "checkpoint"]),
                ),
                ("progress", progress.map_or(JValue::Null, JValue::Num)),
                ("status", first_truthy(row, &["status", "state"])),
            ]))
        })
        .collect::<Option<Vec<_>>>()
        .map(JValue::Arr)
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

/// The facts reply read whole. `Err`: the answer to send instead.
async fn facts(app: &App) -> Result<Vec<u8>, Response<Body>> {
    let Some(mut res) = app
        .config
        .research
        .node_open(FACTS, &serde_json::json!({}), Duration::from_secs(10))
        .await
    else {
        return Err(unavailable());
    };
    if res.status().as_u16() != 200 {
        return Err(unavailable());
    }
    let mut bytes = Vec::new();
    loop {
        match res.chunk().await {
            Ok(Some(chunk)) => bytes.extend_from_slice(&chunk),
            Ok(None) => return Ok(bytes),
            // The owner answered and then failed: a late fault, nothing of it is used.
            Err(_) => return Err(internal()),
        }
    }
}

pub async fn serve(app: &App) -> Response<Body> {
    let bytes = match facts(app).await {
        Ok(bytes) => bytes,
        Err(answer) => return answer,
    };
    let Some(facts) = std::str::from_utf8(&bytes)
        .ok()
        .and_then(|text| wtf16::parse(text).ok())
    else {
        return unavailable();
    };
    drop(bytes);
    let result = match facts.get("kind").as_str() {
        Some("disabled") => Some(JValue::Arr(Vec::new())),
        Some("listing") => view(&facts),
        Some("fault") => None,
        _ => return unavailable(),
    };
    drop(facts);
    let Some(body) = result.as_ref().and_then(wtf16::stringify) else {
        return internal();
    };
    drop(result);
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
