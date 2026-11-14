//! `embedding-served.cjs` (#1660 review): does an embeddings server really serve a model name? A
//! single-model server answers `/v1/embeddings` for any name with its own vectors, so a target it
//! does not list would be embedded by another model and stored under the wrong identity.
//!
//! `name` and `name:latest` are the same model, compared case-insensitively. A listed id is never
//! turned into a name: a server that lists only a file path is refused with a message saying how
//! to fix it.

use serde_json::{json, Value};

use crate::js;
use crate::target::Refusal;

pub const MAX_ECHO: usize = 20;
pub const MAX_ECHO_CHARS: usize = 120;

/// `canonical`: trimmed, lower case, without one trailing `:latest`.
fn canonical(s: &str) -> String {
    let lower = js::trim(s).to_lowercase();
    lower
        .strip_suffix(":latest")
        .map(str::to_owned)
        .unwrap_or(lower)
}

/// `looksLikePath(s)`: an absolute, home, relative or drive path, or a weights file name.
pub fn looks_like_path(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    let starts = s.starts_with('/')
        || s.starts_with('~')
        || s.starts_with("./")
        || s.starts_with("../")
        || {
            let mut c = s.chars();
            matches!((c.next(), c.next(), c.next()), (Some(d), Some(':'), Some('\\' | '/')) if d.is_ascii_alphabetic())
        };
    starts
        || [".gguf", ".bin", ".safetensors"]
            .iter()
            .any(|ext| lower.ends_with(ext))
}

/// `servesModel(ids, model)`.
pub fn serves_model(ids: &[Value], model: &str) -> bool {
    let want = canonical(model);
    !want.is_empty()
        && ids
            .iter()
            .any(|id| id.as_str().is_some_and(|id| canonical(id) == want))
}

/// The first `n` UTF-16 units of `s` (`String.prototype.slice(0, n)`), never splitting a pair.
pub fn slice_units(s: &str, n: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        used += c.len_utf16();
        if used > n {
            break;
        }
        out.push(c);
    }
    out
}

/// `echoIds(ids)`: at most 20 ids, each `String(id)` cut to 120 units.
pub fn echo_ids(ids: &[Value]) -> Vec<String> {
    ids.iter()
        .take(MAX_ECHO)
        .map(|id| slice_units(&js::js_string(Some(id)), MAX_ECHO_CHARS))
        .collect()
}

/// `checkServed(servedModels, target)`'s decision, given what `/v1/models` listed (None: the
/// server could not be asked, or did not answer a list). Ok, or the 409 refusal core throws
/// (`endpointUnverified` / `modelNotServed`, with `served` when ids were listed).
pub fn check_served(ids: Option<&[Value]>, model: &str) -> Result<(), Refusal> {
    let model = js::trim(model);
    let shown = slice_units(model, MAX_ECHO_CHARS);
    let Some(ids) = ids else {
        return Err(Refusal::new(
            409,
            "endpointUnverified",
            format!("Could not confirm that the embeddings server serves {shown}."),
        ));
    };
    if serves_model(ids, model) {
        return Ok(());
    }
    let served = json!(echo_ids(ids));
    let all_paths = !ids.is_empty()
        && ids
            .iter()
            .all(|id| looks_like_path(&js::js_string(Some(id))));
    let message = if all_paths {
        format!("The embeddings server lists only a model file, not model names, so it cannot be confirmed that it serves {shown}. Start it with --alias {shown} (or load {shown} there).")
    } else {
        format!("The embeddings server does not serve {shown}.")
    };
    Err(Refusal::new(409, "modelNotServed", message).with("served", served))
}

/// The JSON of a failed check as `routes/embedding.cjs` sends it: `{ error, code, served? }`.
pub fn refusal_json(r: &Refusal) -> Value {
    let mut out = serde_json::Map::new();
    out.insert("error".into(), json!(r.message));
    if let Some(code) = r.code {
        out.insert("code".into(), json!(code));
    }
    if let Some(served) = r.extra.get("served") {
        out.insert("served".into(), served.clone());
    }
    Value::Object(out)
}
