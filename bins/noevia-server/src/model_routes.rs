//! The model reads that need nothing of Node's memory (sbstndalton/noevia#2187, batch M1):
//! `GET /api/models/capabilities` and `GET /api/models/hardware`, permanent and with no switch.
//! Node refuses both GETs (core server/rust-models.cjs `RUST_MODELS_OWNED`), so the web supervisor
//! and the release assembly refuse a front without `rust-model-reads` beside such a Core, and the
//! reverse. Every other method of both paths stays Node's (routes.toml `methods = ["GET"]`).
//!
//! Both answers are configuration only (the engine kind, the preset file, the model management
//! service; see [`ModelReads`]); nothing from the request reaches an engine. Order follows core:
//! the session gate (401/403, device tokens act as members), then the answer.
//!
//! - capabilities (core routes/models.cjs): `{kind, enabled, admin, ...adapter capabilities,
//!   modelManagement}` in Node's key order. `admin` and `modelManagement` follow the caller's own
//!   role (`authn.user.role`: a device token is a member).
//! - hardware: with no model manager, Node's 404. With llama.cpp, `{supported: false}`: the adapter
//!   cannot discover hardware (its `systemInfo` answered 501), which #629 made a capability answer,
//!   so no engine is asked. The Lemonade adapter (the only one that read hardware, from its
//!   `/v1/system-info`) never reaches this front: config.rs refuses to start with it.
//!
//! Kept on Node (each needs its memory or its job state), with the rest of the 22 model routes:
//! `/api/model-manager` (the folder scan cache), `/api/models/autotune*` and
//! `/api/models/calibration*` (the job runners, M2), `/api/models/evidence` (the identity cache it
//! fills), `/api/models/estimate` and `/api/models/inference-budget` (the boot-time budget fault,
//! the inference budget store and the shared installed list), `/api/models/installed` (the shared
//! installed list), `/api/models/machines` (probe and tune caches), the writes and `/api/models/{*}`.

use crate::{identity, reply, App};
use axum::body::Body;
use axum::http::{Method, Request, Response, StatusCode};
use axum::response::IntoResponse;
use server_auth::Role;
use std::sync::Arc;

pub const CAPABILITIES: &str = "/api/models/capabilities";
pub const HARDWARE: &str = "/api/models/hardware";

/// The model reads this front answers (GET only; routes.toml).
pub const ROUTES: &[&str] = &[CAPABILITIES, HARDWARE];

/// What the model reads answer from: the keys core index.cjs read at startup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelReads {
    pub kind: server_stats::Kind,
    /// `LLAMACPP_PRESET_PATH` is set: the llama.cpp adapter has a preset store, and with it
    /// auto-tune (index.cjs always gives it a state file).
    pub presets: bool,
    /// `MODEL_LOADER_URL` is set: the model management service is configured.
    pub model_loader: bool,
}

impl ModelReads {
    pub fn from_lookup(kind: server_stats::Kind, get: &impl Fn(&str) -> Option<String>) -> Self {
        let set = |k: &str| get(k).is_some_and(|v| !v.is_empty());
        ModelReads {
            kind,
            presets: set("LLAMACPP_PRESET_PATH"),
            model_loader: set("MODEL_LOADER_URL"),
        }
    }

    /// The capabilities answer for a caller who is (`admin`) or is not an administrator, as the
    /// JSON text Node writes (its key order).
    pub fn capabilities(&self, admin: bool) -> String {
        use server_stats::Kind;
        let b = |v: bool| if v { "true" } else { "false" };
        let management = b(self.model_loader && admin);
        match self.kind {
            Kind::LlamaCpp => format!(
                "{{\"kind\":\"llamacpp\",\"enabled\":true,\"admin\":{},\"routing\":true,\"load\":true,\"unload\":true,\"download\":true,\"deleteCached\":true,\"runtimeOptions\":false,\"hardware\":false,\"presets\":{p},\"autotune\":{p},\"modelManagement\":{}}}",
                b(admin),
                management,
                p = b(self.presets),
            ),
            // model-manager.cjs: `presets: false` and no `autotune` key for both other kinds.
            Kind::Lemonade => format!(
                "{{\"kind\":\"lemonade\",\"enabled\":true,\"admin\":{},\"routing\":false,\"load\":true,\"unload\":true,\"download\":true,\"deleteCached\":true,\"runtimeOptions\":true,\"hardware\":true,\"presets\":false,\"modelManagement\":{}}}",
                b(admin),
                management,
            ),
            Kind::None => format!(
                "{{\"kind\":\"none\",\"enabled\":false,\"admin\":{},\"routing\":false,\"load\":false,\"unload\":false,\"download\":false,\"deleteCached\":false,\"runtimeOptions\":false,\"hardware\":false,\"presets\":false,\"modelManagement\":{}}}",
                b(admin),
                management,
            ),
        }
    }

    /// The hardware answer: (status, JSON text). `None` for Lemonade, which this front never runs
    /// with (config.rs).
    pub fn hardware(&self) -> Option<(StatusCode, String)> {
        use server_stats::Kind;
        match self.kind {
            Kind::None => Some((
                StatusCode::NOT_FOUND,
                r#"{"error":"Model manager is disabled. Enter a memory plan manually."}"#.into(),
            )),
            Kind::LlamaCpp => Some((StatusCode::OK, r#"{"supported":false}"#.into())),
            Kind::Lemonade => None,
        }
    }
}

pub async fn serve(
    app: &Arc<App>,
    conn: crate::serve::Conn,
    req: Request<Body>,
    path: String,
) -> Response<Body> {
    // Node matches the exact pathname and GET only; routes.toml matching also accepts a trailing
    // slash, and every other method is Node's.
    if !ROUTES.contains(&path.as_str()) || req.method() != Method::GET {
        return app.proxy.forward(conn.peer, req).await;
    }
    let (parts, _body) = req.into_parts();
    let creds = identity::creds_of(&parts);
    let layer = Arc::clone(&app.identity);
    let now = identity::now_ms();
    let gate_path = path.clone();
    let id = match tokio::task::spawn_blocking(move || layer.gate_blocking(&creds, &gate_path, now))
        .await
    {
        Ok(Ok(id)) => id,
        Ok(Err(rejection)) => return rejection.into_response(),
        Err(_) => return identity::Rejection::Unavailable.into_response(),
    };
    let reads = app.config.models;
    if path == CAPABILITIES {
        return reply::json_text(
            StatusCode::OK,
            reads.capabilities(id.user.role == Role::Admin),
            true,
        );
    }
    match reads.hardware() {
        Some((status, body)) => reply::json_text(status, body, true),
        None => reply::error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Model hardware is unavailable.",
            true,
        ),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use server_stats::Kind;

    fn parse(text: &str) -> serde_json::Value {
        serde_json::from_str(text).unwrap()
    }

    #[test]
    fn capabilities_are_nodes_per_kind_and_role() {
        let none = ModelReads {
            kind: Kind::None,
            presets: true,
            model_loader: true,
        };
        // The corpus exchange (contracts/http/corpus 000136), byte for byte in key order.
        assert_eq!(
            none.capabilities(true),
            r#"{"kind":"none","enabled":false,"admin":true,"routing":false,"load":false,"unload":false,"download":false,"deleteCached":false,"runtimeOptions":false,"hardware":false,"presets":false,"modelManagement":true}"#
        );
        assert_eq!(parse(&none.capabilities(false))["modelManagement"], false);
        assert_eq!(parse(&none.capabilities(false))["admin"], false);
        let native = ModelReads {
            kind: Kind::LlamaCpp,
            presets: true,
            model_loader: false,
        };
        let v = parse(&native.capabilities(true));
        assert_eq!(v["kind"], "llamacpp");
        assert_eq!(v["enabled"], true);
        assert_eq!(v["presets"], true);
        assert_eq!(v["autotune"], true);
        assert_eq!(v["routing"], true);
        assert_eq!(v["runtimeOptions"], false);
        assert_eq!(v["modelManagement"], false, "no model management service");
        let bare = ModelReads {
            presets: false,
            ..native
        };
        let v = parse(&bare.capabilities(false));
        assert_eq!(
            (v["presets"].clone(), v["autotune"].clone()),
            (false.into(), false.into())
        );
        let lemonade = ModelReads {
            kind: Kind::Lemonade,
            presets: true,
            model_loader: true,
        };
        let v = parse(&lemonade.capabilities(true));
        assert_eq!(v["presets"], false);
        assert!(v.get("autotune").is_none());
        assert_eq!(v["hardware"], true);
    }

    #[test]
    fn hardware_is_a_capability_answer_never_a_gateway_failure() {
        let read = |kind| ModelReads {
            kind,
            presets: false,
            model_loader: false,
        };
        assert_eq!(
            read(Kind::None).hardware(),
            Some((
                StatusCode::NOT_FOUND,
                r#"{"error":"Model manager is disabled. Enter a memory plan manually."}"#.into()
            ))
        );
        assert_eq!(
            read(Kind::LlamaCpp).hardware(),
            Some((StatusCode::OK, r#"{"supported":false}"#.into()))
        );
        assert_eq!(read(Kind::Lemonade).hardware(), None);
    }

    #[test]
    fn the_configuration_is_read_from_nodes_keys() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| {
                pairs
                    .iter()
                    .find(|(key, _)| *key == k)
                    .map(|(_, v)| (*v).to_owned())
            }
        };
        let r = ModelReads::from_lookup(
            Kind::LlamaCpp,
            &env(&[
                ("LLAMACPP_PRESET_PATH", "/models.ini"),
                ("MODEL_LOADER_URL", ""),
            ]),
        );
        assert!(r.presets);
        assert!(!r.model_loader, "an empty value is unset, as in Node");
        let r = ModelReads::from_lookup(Kind::None, &env(&[("MODEL_LOADER_URL", "http://ml:1")]));
        assert!(!r.presets);
        assert!(r.model_loader);
    }
}
