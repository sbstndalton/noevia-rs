//! Which requests to the engine router may load a model or end a generation.
//!
//! The router loads a model on any request that names it, unless the request asks for
//! `autoload=false` (the router's `is_autoload`: an absent parameter means the router's default,
//! which is on; otherwise only `true` or `1` load). A POST that names a model is a generation (or
//! an embedding) on that model, so it always needs admission, whatever its `autoload` says: the
//! gate must count it as in flight. Reads that cannot load (the listing, `autoload=false`
//! properties and metrics, stream resumes) pass through untouched.
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
    /// `explicit_load` is a `POST /models/load`.
    Model { model: String, explicit_load: bool },
    /// Ends `model`'s process: `POST /models/unload`, or `DELETE /models?model=` (`delete`).
    /// `force` asks to end it even while it is in use (admin only).
    Unload {
        model: String,
        force: bool,
        delete: bool,
    },
    /// The router re-reads its presets (`GET /models?reload=...`).
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
    let params = query.map(first_params).unwrap_or_default();
    let param = |key: &str| {
        params
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    };
    match method {
        "GET" | "HEAD" => {
            if path == "/models" && param("reload").is_some() {
                return Class::Reload;
            }
            match param("model") {
                Some(model) if autoload_loads(param("autoload")) => model_class(model, false),
                _ => Class::Passthrough,
            }
        }
        "DELETE" => {
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
        "POST" | "PUT" | "PATCH" => classify_body(path, body.unwrap_or_default()),
        _ => Class::Passthrough,
    }
}

fn classify_body(path: &str, body: &[u8]) -> Class {
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
        "/models/load" => model_class(&model, true),
        _ => model_class(&model, false),
    }
}

fn model_class(model: &str, explicit_load: bool) -> Class {
    if valid_model_id(model) {
        Class::Model {
            model: model.to_owned(),
            explicit_load,
        }
    } else {
        Class::Invalid(Refusal::InvalidModel)
    }
}

/// The router's `is_autoload`: absent means the router default (on); otherwise only `true`/`1`.
fn autoload_loads(value: Option<&str>) -> bool {
    matches!(value, None | Some("true" | "1"))
}

/// The first value of every query key, decoded as the router decodes it.
fn first_params(query: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for (key, value) in form_urlencoded::parse(query.as_bytes()) {
        if out.len() >= 64 {
            break;
        }
        if !out.iter().any(|(k, _)| k.as_str() == key.as_ref()) {
            out.push((key.into_owned(), value.into_owned()));
        }
    }
    out
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
