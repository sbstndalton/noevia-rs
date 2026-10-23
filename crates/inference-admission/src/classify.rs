//! Which requests to the engine router may load a model or end a generation.
//!
//! The router loads a model on any request that names it, unless the request asks for
//! `autoload=false` (the router's `is_autoload`: an absent parameter means the router's default,
//! which is on; otherwise only `true` or `1` load). A request that names a model runs on it, so it
//! needs admission and is counted in flight; with `autoload=false` it cannot load the model, so it
//! never waits for a switch (the scheduler passes it on when the model is not resident). Two reads
//! neither load nor wake a sleeping model with `autoload=false` and pass through untouched:
//! properties and metrics (the router's child answers them without waking; `/slots` and the
//! adapter list do wake it, so they are counted). The listing passes through; a listing that asks
//! for a preset reload (`reload` with a value, on `/models` or `/v1/models`) is a reload. A stream
//! resume (`GET /v1/stream`) runs on the model that owns its session and is counted there; an
//! explicit stop (`DELETE /v1/stream`) ends the session's generation.
//! `POST /models` starts a download and loads nothing.
//!
//! The router decodes percent-escapes in paths and has no use for dot segments, so a path with
//! `%`, `\`, `//` or a dot segment is refused before it is forwarded: what the gate classifies must
//! be what the router routes.

use crate::config::valid_model_id;
use crate::scheduler::Refusal;
use serde::Deserialize;
use serde_json::value::RawValue;

/// What the gate does with one request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Class {
    /// Cannot load a model or end a generation: forwarded as is.
    Passthrough,
    /// May load `model`, or runs on it: admitted first and counted in flight while it runs.
    /// `explicit_load` is a `POST /models/load`; `autoload` is false when the request asks the
    /// router not to load the model.
    Model {
        model: String,
        explicit_load: bool,
        autoload: bool,
    },
    /// A stream resume (`GET /v1/stream?conv_id=`): runs on the session's model.
    Resume { conv_id: Option<String> },
    /// An explicit stop of a session generation (`DELETE /v1/stream?conv_id=`): forwarded as is;
    /// once the router accepts it the session no longer holds its model.
    Stop { conv_id: String },
    /// Ends `model`'s process: `POST /models/unload`, or `DELETE /models?model=` (`delete`).
    /// `force` asks to end it even while it is in use (admin only).
    Unload {
        model: String,
        force: bool,
        delete: bool,
    },
    /// The router re-reads its presets (`GET /models?reload=...` or `/v1/models?reload=...`).
    Reload,
    /// Refused before anything is forwarded.
    Invalid(Refusal),
}

/// The body fields the gate reads. Unknown fields are ignored; a repeated `model` is an error.
#[derive(Deserialize)]
struct BodyFields<'a> {
    #[serde(borrow, default, deserialize_with = "present")]
    model: Option<&'a RawValue>,
    #[serde(borrow, default, deserialize_with = "present")]
    force: Option<&'a RawValue>,
}

/// A field that is present, even as `null`, is `Some` (a `null` model is refused, not ignored).
fn present<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<&'de RawValue>, D::Error> {
    <&RawValue>::deserialize(d).map(Some)
}

/// Classifies a request. `query` is the raw query string without `?`; `body` is the complete body
/// of a POST/PUT/PATCH (other methods' bodies are not read).
pub fn classify(method: &str, path: &str, query: Option<&str>, body: Option<&[u8]>) -> Class {
    if !valid_path(path) {
        return Class::Invalid(Refusal::InvalidPath);
    }
    let params = match query.map(first_params) {
        None => Vec::new(),
        Some(Some(params)) => params,
        Some(None) => return Class::Invalid(Refusal::InvalidQuery),
    };
    let param = |key: &str| {
        params
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    };
    match method {
        "GET" | "HEAD" => {
            if matches!(path, "/models" | "/v1/models")
                && param("reload").is_some_and(|v| !v.is_empty())
            {
                return Class::Reload;
            }
            if path == "/v1/stream" {
                return Class::Resume {
                    conv_id: param("conv_id")
                        .filter(|c| !c.is_empty() && c.len() <= MAX_CONV_ID_BYTES)
                        .map(str::to_owned),
                };
            }
            let autoload = autoload_loads(param("autoload"));
            match param("model") {
                // Answered by the child without waking it, and cannot load with autoload off.
                Some(_) if !autoload && matches!(path, "/props" | "/metrics") => Class::Passthrough,
                Some(model) => model_class(model, false, autoload),
                None => Class::Passthrough,
            }
        }
        "DELETE" => {
            if path == "/v1/stream" {
                return match param("conv_id")
                    .filter(|c| !c.is_empty() && c.len() <= MAX_CONV_ID_BYTES)
                {
                    Some(conv_id) => Class::Stop {
                        conv_id: conv_id.to_owned(),
                    },
                    None => Class::Passthrough,
                };
            }
            if path != "/models" {
                return Class::Passthrough;
            }
            match param("model") {
                Some(model) if valid_model_id(model) => Class::Unload {
                    model: model.to_owned(),
                    force: matches!(param("force"), Some("true" | "1")),
                    delete: true,
                },
                _ => Class::Invalid(Refusal::InvalidModel),
            }
        }
        "POST" | "PUT" | "PATCH" => classify_body(
            path,
            body.unwrap_or_default(),
            autoload_loads(param("autoload")),
        ),
        _ => Class::Passthrough,
    }
}

fn classify_body(path: &str, body: &[u8], autoload: bool) -> Class {
    // A download of a new model: loads nothing.
    if path == "/models" {
        return Class::Passthrough;
    }
    let management = matches!(path, "/models/load" | "/models/unload");
    if body.iter().all(u8::is_ascii_whitespace) {
        return if management {
            Class::Invalid(Refusal::InvalidModel)
        } else {
            Class::Passthrough
        };
    }
    // Only an object can carry a model; anything else that is still JSON cannot load one.
    let first = body.iter().find(|b| !b.is_ascii_whitespace());
    if first != Some(&b'{') {
        return match serde_json::from_slice::<serde::de::IgnoredAny>(body) {
            Ok(_) if !management => Class::Passthrough,
            Ok(_) => Class::Invalid(Refusal::InvalidModel),
            Err(_) => Class::Invalid(Refusal::InvalidJson),
        };
    }
    let fields: BodyFields<'_> = match serde_json::from_slice(body) {
        Ok(fields) => fields,
        Err(_) => return Class::Invalid(Refusal::InvalidJson),
    };
    let model = match fields.model {
        None if management => return Class::Invalid(Refusal::InvalidModel),
        None => return Class::Passthrough,
        Some(raw) => match serde_json::from_str::<String>(raw.get()) {
            Ok(model) => model,
            Err(_) => return Class::Invalid(Refusal::InvalidModel),
        },
    };
    match path {
        "/models/unload" => {
            if !valid_model_id(&model) {
                return Class::Invalid(Refusal::InvalidModel);
            }
            let force = match fields.force {
                None => false,
                Some(raw) => match serde_json::from_str::<bool>(raw.get()) {
                    Ok(force) => force,
                    Err(_) => return Class::Invalid(Refusal::InvalidJson),
                },
            };
            Class::Unload {
                model,
                force,
                delete: false,
            }
        }
        "/models/load" => model_class(&model, true, true),
        _ => model_class(&model, false, autoload),
    }
}

fn model_class(model: &str, explicit_load: bool, autoload: bool) -> Class {
    if valid_model_id(model) {
        Class::Model {
            model: model.to_owned(),
            explicit_load,
            autoload,
        }
    } else {
        Class::Invalid(Refusal::InvalidModel)
    }
}

/// The router's `is_autoload`: absent means the router default (on); otherwise only `true`/`1`.
fn autoload_loads(value: Option<&str>) -> bool {
    matches!(value, None | Some("true" | "1"))
}

/// Longest query string the gate checks (bytes); a longer one is refused.
pub const MAX_QUERY_BYTES: usize = 8192;
/// Most distinct query keys the gate checks; a query with more is refused, never half-read.
pub const MAX_QUERY_KEYS: usize = 64;
/// Longest conversation id the gate tracks (bytes).
pub const MAX_CONV_ID_BYTES: usize = 256;

/// The first value of every query key, decoded as the router decodes it. `None` when the query is
/// longer or has more distinct keys than the gate checks: the router reads every key, so a key
/// past a cap must not go unseen.
fn first_params(query: &str) -> Option<Vec<(String, String)>> {
    if query.len() > MAX_QUERY_BYTES {
        return None;
    }
    let mut out: Vec<(String, String)> = Vec::new();
    for (key, value) in form_urlencoded::parse(query.as_bytes()) {
        if !out.iter().any(|(k, _)| k.as_str() == key.as_ref()) {
            if out.len() >= MAX_QUERY_KEYS {
                return None;
            }
            out.push((key.into_owned(), value.into_owned()));
        }
    }
    Some(out)
}

/// A path the router routes exactly as written: absolute, printable ASCII, no escapes, no
/// backslashes, no empty or dot segments.
pub fn valid_path(path: &str) -> bool {
    if !path.starts_with('/') || path.len() > 2048 {
        return false;
    }
    if path
        .bytes()
        .any(|b| !(0x21..=0x7e).contains(&b) || b == b'%' || b == b'\\')
    {
        return false;
    }
    if path == "/" {
        return true;
    }
    path.split('/')
        .skip(1)
        .all(|seg| !seg.is_empty() && seg != "." && seg != "..")
}
