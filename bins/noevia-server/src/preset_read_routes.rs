//! GET `/api/models/preset`: the parsed preset facts, from the native authority (G2).
//!
//! The facts come from [`Store::load`](crate::model_maint::ini::Store::load), the reader the
//! conditional writes check their revision with, over the `llamacpp-presets` crate's parse (the one
//! Node's wasm module runs too). There is no second parser and no call to Node: a revision this
//! route returns is exactly the one a write compares against.
//!
//! The outcomes stay apart ([`Outcome`]): a file that is missing, a path or file that cannot be used
//! (not a regular file, duplicate sections, over the limit, unreadable), and a valid file with
//! nothing in it, which is an ordinary answer (`exists: false`, no options, the hash of no text).
//! Core answered a missing and an unreadable file alike (500 "Internal error"); the public answers
//! are Core's, and the log line says which it was.
use axum::{body::Body, http::StatusCode, response::Response};
use js_json::JValue;
use llamacpp_presets::read;

use crate::model_maint::ini::Store;
use crate::model_sizing::{Presets, ReadFault};
use crate::{reply, App};

pub const PATH: &str = "/api/models/preset";

/// What one read of the preset file answers.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// The facts of one snapshot: the JSON body.
    Facts(String),
    /// No llama.cpp model management: 404.
    Unavailable,
    /// llama.cpp, but no preset file configured (`LLAMACPP_PRESET_PATH`): 501.
    Unconfigured,
    /// Nothing at the configured path.
    Missing,
    /// The path or its content cannot be read.
    Unreadable,
    /// Not a regular file, or a symlink: 409.
    NonRegular,
    /// A section name twice: 409.
    Duplicate,
    /// Over the editor limit: 413.
    Oversized,
}

fn array(names: impl IntoIterator<Item = String>) -> JValue {
    JValue::Arr(names.into_iter().map(JValue::Str).collect())
}

fn object(options: &[(&'static str, String)]) -> JValue {
    JValue::obj(
        options
            .iter()
            .map(|(k, v)| (*k, JValue::Str(v.clone())))
            .collect::<Vec<_>>(),
    )
}

/// The facts of one snapshot for `model`: Core's `get(model)` view.
fn facts_body(presets: &Presets, model: &str) -> Option<String> {
    let (exists, options) = presets
        .span(model)
        .map_or((false, &[][..]), |(_, _, o)| (true, o));
    let defaults = presets.span("*").map_or(&[][..], |(_, _, o)| o);
    js_json::stringify(&JValue::obj([
        ("model", JValue::Str(model.to_owned())),
        ("revision", JValue::Str(presets.revision.clone())),
        ("exists", JValue::Bool(exists)),
        ("options", object(options)),
        ("defaults", object(defaults)),
        ("fields", array(read::field_names().map(str::to_owned))),
    ]))
}

/// Reads the preset file (blocking) and answers for `model`.
pub fn read_outcome(kind: server_stats::Kind, store: Option<&Store>, model: &str) -> Outcome {
    if kind != server_stats::Kind::LlamaCpp {
        return Outcome::Unavailable;
    }
    let Some(store) = store else {
        return Outcome::Unconfigured;
    };
    match store.load() {
        Ok(presets) => facts_body(&presets, model).map_or(Outcome::Unreadable, Outcome::Facts),
        Err(fault) => fault_outcome(&fault),
    }
}

fn fault_outcome(fault: &ReadFault) -> Outcome {
    match fault {
        ReadFault::Missing(_) => Outcome::Missing,
        ReadFault::Unreadable(_) => Outcome::Unreadable,
        ReadFault::NonRegular => Outcome::NonRegular,
        ReadFault::Duplicate => Outcome::Duplicate,
        ReadFault::Oversized => Outcome::Oversized,
    }
}

pub async fn serve(app: &App, admin: bool, model: Option<String>) -> Response<Body> {
    if !admin {
        return reply::error(
            StatusCode::FORBIDDEN,
            "Administrator required for shared model profiles",
            true,
        );
    }
    let model = model.unwrap_or_default();
    let kind = app.config.maint.kind;
    let store = app.config.maint.ini.clone();
    let Ok(outcome) =
        tokio::task::spawn_blocking(move || read_outcome(kind, store.as_ref(), &model)).await
    else {
        return reply::error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Preset state is unavailable. Try again shortly.",
            true,
        );
    };
    respond(&outcome)
}

/// The public answer for an outcome: Core's statuses and messages.
pub fn respond(outcome: &Outcome) -> Response<Body> {
    let (status, message) = match outcome {
        Outcome::Facts(body) => return reply::json_text(StatusCode::OK, body.clone(), true),
        Outcome::Unavailable => (StatusCode::NOT_FOUND, "Native presets are unavailable"),
        Outcome::Unconfigured => (
            StatusCode::NOT_IMPLEMENTED,
            "Native preset editing is not exposed by this llama.cpp adapter",
        ),
        Outcome::NonRegular => (StatusCode::CONFLICT, "Preset path must be a regular file"),
        Outcome::Duplicate => (
            StatusCode::CONFLICT,
            "Duplicate preset sections require operator repair",
        ),
        Outcome::Oversized => (
            StatusCode::PAYLOAD_TOO_LARGE,
            "Preset file exceeds the editor limit",
        ),
        Outcome::Missing | Outcome::Unreadable => {
            eprintln!(
                "[preset-read] the preset file could not be read: {}",
                if *outcome == Outcome::Missing {
                    "missing"
                } else {
                    "unreadable"
                }
            );
            (StatusCode::INTERNAL_SERVER_ERROR, "Internal error")
        }
    };
    reply::error(status, message, true)
}
