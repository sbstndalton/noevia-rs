//! Core pull policy, used by the native public mutation lifecycle.
//!
//! This module does not claim the public route. The router owns transfer/deduplication;
//! its shared progress tracker must be notified under the same active registration as
//! the start. An exclusive Maint hold is not a substitute for that registration.
use super::v8_json::{self, W};

pub struct Answer {
    pub status: u16,
    pub body: String,
}

fn render(status: u16, body: W) -> Answer {
    let mut text = String::new();
    v8_json::stringify(&body, &mut text);
    Answer { status, body: text }
}
fn error(status: u16, message: &str) -> Answer {
    render(
        status,
        W::Obj(vec![("error".encode_utf16().collect(), W::str(message))]),
    )
}

/// Checkpoint is kept as a JSON value: Core's regex coerces it, but its POST sends
/// the original value. In particular `["org/repo"]` passes and remains an array.
pub struct Pull {
    pub checkpoint: W,
}

/// Route parse/required/disabled checks, then the llama.cpp adapter's validation.
/// A null body throws in Core before the checkpoint-required branch (generic 500).
pub fn route_prepare(raw: &str, enabled: bool) -> Result<Pull, Answer> {
    let Ok(mut body) = v8_json::parse(&raw.encode_utf16().collect::<Vec<_>>()) else {
        return Err(error(400, "invalid JSON"));
    };
    if matches!(body, W::Null) {
        return Err(error(500, "Internal error"));
    }
    if !body.get("checkpoint").truthy() {
        return Err(error(400, "checkpoint required"));
    }
    if !enabled {
        return Err(error(404, "model management is disabled"));
    }
    if !body.get("modelName").truthy() && v8_json::to_js_string(body.get("checkpoint")).is_err() {
        return Err(error(500, "Internal error"));
    }
    let checkpoint = body.take("checkpoint");
    Ok(Pull { checkpoint })
}

pub fn prepare(raw: &str, enabled: bool) -> Result<Pull, Answer> {
    let plan = route_prepare(raw, enabled)?;
    plan.validate()?;
    Ok(plan)
}

impl Pull {
    pub fn validate(&self) -> Result<(), Answer> {
        let units =
            v8_json::to_js_string(&self.checkpoint).map_err(|_| error(500, "Internal error"))?;
        let valid = String::from_utf16(&units)
            .ok()
            .is_some_and(|s| valid_checkpoint(&s));
        if !valid {
            return Err(error(
                502,
                "Choose a Hugging Face repository and quantization",
            ));
        }
        Ok(())
    }
}

fn valid_checkpoint(s: &str) -> bool {
    fn component(s: &str) -> bool {
        !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
    }
    let (repo, variant) = s.split_once(':').map_or((s, None), |(r, v)| (r, Some(v)));
    let Some((owner, name)) = repo.split_once('/') else {
        return false;
    };
    component(owner) && component(name) && variant.is_none_or(component)
}

impl Pull {
    /// The existing native router download owner receives exactly Core's payload.
    pub fn request_body(&self) -> String {
        let mut text = String::from("{\"model\":");
        v8_json::stringify(&self.checkpoint, &mut text);
        text.push('}');
        text
    }

    /// Core's adapter replaces upstream id/modelName with the original checkpoint.
    /// On failure the public route uses the upstream error's JS truthiness verbatim.
    pub fn finish(mut self, ok: bool, status: u16, mut upstream: W) -> Answer {
        if ok {
            let mut checkpoint = String::new();
            v8_json::stringify(&self.checkpoint, &mut checkpoint);
            return Answer {
                status: 200,
                body: format!("{{\"jobId\":{checkpoint},\"modelName\":{checkpoint}}}"),
            };
        }
        let value = if upstream.get("error").truthy() {
            upstream.take("error")
        } else {
            W::str(&format!("pull failed: {status}"))
        };
        // Move, never recursively clone, arbitrary JSON response data.
        self.checkpoint = W::Undefined;
        render(502, W::Obj(vec![("error".encode_utf16().collect(), value)]))
    }
}
