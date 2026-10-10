//! M4 of the full-Rust migration (docs/adr-0001-rust-and-repo-split.md "Amendment 2026-10-10" in
//! sbstndalton/noevia): the project routes the Rust front takes over from noevia-core, slice by
//! slice, behind the deployment switch `NOEVIA_RUST_PROJECTS=1` ([`server_store::RustProjects`]).
//!
//! - [`lock`] and [`store`]: `UI_DATA_DIR/users/<id>/projects.json`, which Node still writes too
//!   (the chat routes keep chat metas in it). Rust reads it fresh under the shared lock for every
//!   change and writes Node's exact `atomicJson` bytes; Node merges its own saves under the same
//!   lock (core `server/rust-projects.cjs`). No cache: a request never writes from a stale copy.
//! - [`assets`]: the first slice, a project's images (core routes/projects.cjs: `POST
//!   /api/projects/{id}/assets`, `GET` and `DELETE /api/projects/{id}/assets/{assetId}`), answer
//!   for answer. Untrusted input it hardens: the project and image ids from the URL (percent
//!   decoding as `decodeURIComponent`, the image id reduced to `[A-Za-z0-9_-]` before it names a
//!   file, the project id reduced the same way for its directory), the JSON body (Node's coercions,
//!   its lenient base64, the 8 MiB image cap and the 12-image limit, both rechecked under the lock).
//! - [`js`]: the JavaScript built-ins those handlers rely on.
//!
//! The caller (bins/noevia-server) has already run Node's request gate (session, origin, CSRF);
//! every path here is inside the signed-in user's own directory, so tenant scope is the user id
//! the gate returned and nothing from the request picks another user's files.

pub mod assets;
pub mod js;
pub mod lock;
pub mod store;

/// A handler's answer: Node's `json(res, status, body)` or a binary body with its headers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    pub status: u16,
    pub body: Body,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Body {
    /// `JSON.stringify(body)`, sent as http.cjs json() does.
    Json(String),
    /// Bytes with the extra headers the route sets (name, value), in Node's order.
    Bytes {
        headers: Vec<(&'static str, String)>,
        bytes: Vec<u8>,
    },
}

impl Reply {
    pub fn json(status: u16, text: String) -> Self {
        Reply {
            status,
            body: Body::Json(text),
        }
    }

    /// `{"error": message}`.
    pub fn error(status: u16, message: &str) -> Self {
        let mut text = String::from("{\"error\":");
        js_json::quote(message, &mut text);
        text.push('}');
        Reply::json(status, text)
    }

    /// What an exception that carries no 4xx status becomes in Node (http.cjs errorResponse).
    pub fn internal() -> Self {
        Reply::error(500, "Internal error")
    }
}

/// The signed-in request a handler serves.
#[derive(Debug, Clone, Copy)]
pub struct Ctx<'a> {
    /// UI_DATA_DIR.
    pub data_dir: &'a std::path::Path,
    /// The user the request gate returned (`authn.user.id`).
    pub user_id: &'a str,
    pub switch: server_store::RustProjects,
    /// `Date.now()`.
    pub now_ms: i64,
}
