//! M4 of the full-Rust migration (docs/adr-0001-rust-and-repo-split.md "Amendment 2026-10-10" in
//! sbstndalton/noevia): the project routes the Rust front takes over from noevia-core, slice by
//! slice, permanent since sbstndalton/noevia#1071 ([`server_store::RustProjects`]; the deployment
//! switch `NOEVIA_RUST_PROJECTS` is retired).
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
//! - [`chats`]: the second slice, a project's chat list (core routes/projects.cjs `GET` and `POST
//!   /api/projects/{id}/chats`), with [`skills`] (instruction-skills.cjs `reconcile`, which every
//!   save runs) and [`frame`] (chat-framing.cjs `normalizeFrame`). Untrusted input it hardens: the
//!   project id from the URL and the JSON body (1 MiB, 1,000 metas), see the module.
//! - [`free_chats`]: the third slice, the free-chat list (core routes/chat-lists.cjs `GET` and
//!   `POST /api/freechats`), over `free-chats.json`, which Node's own writers (a chat's move, a free
//!   chat's delete, the routing flags) still write: changed only under `free-chats.json.lock`
//!   ([`store::update_free`]), which Node shares while the slice is on.
//! - [`context`]: the first project-writes slice of sbstndalton/noevia#1264, a free chat's
//!   attachments project (core routes/projects.cjs `GET` and `POST /api/chats/{chatId}/context`),
//!   permanently native under #1071 (required cargo feature `rust-project-writes`).
//! - [`context_window`]: a chat's last context meter (core routes/chat-lists.cjs `GET
//!   /api/chats/{chatId}/context-window`), a read with no switch (sbstndalton/noevia#1071).
//! - [`history`]: a chat's transcript and its interrupted partial reply (core routes/chat-lists.cjs
//!   `GET /api/chats/{chatId}/history`, slice 2) and its save (`POST`, slice 5, through
//!   [`transcript`]), with no switch (sbstndalton/noevia#1071).
//! - [`retention`]: Settings → Data → Delete old chats, its read (core routes/account.cjs `GET
//!   /api/account/retention`), a read with no switch (sbstndalton/noevia#1071); `PUT` stays Node's.
//! - [`partial`]: a chat's saved partial reply as another process reads it, running or cut off by
//!   a restart (the `running: { start }` marker core chat-partial.cjs writes; sbstndalton/noevia#1071),
//!   read by [`history`].
//! - [`transcript`]: a chat's transcript file, which Rust alone writes (sbstndalton/noevia#1071
//!   slice 4): locked compare-and-swap saves, the interrupted-reply append and the import's
//!   publish, each re-checking the tombstones under the lock; [`history_sanitize`] bounds what is
//!   stored as Node's `sanitizeHistory` does.
//! - [`journal`]: each user's chat change journal (`chat-changes.jsonl`), one entry per transcript
//!   mutation, written under the transcript lock; Node tails it to run the chat mirror and brain
//!   (sbstndalton/noevia#2153).
//! - [`chat_delete`]: a chat's delete (core `DELETE /api/freechats/{id}` and
//!   `DELETE /api/projects/{id}/chats/{chatId}`, and Node's remaining delete paths through the
//!   internal store), the whole cascade in one hold of the transcript lock: tombstone, list save,
//!   then the transcript, context meter, brain note and partial reply (sbstndalton/noevia#1071
//!   slice 6, #2147, #2154). The attachments project stays Node's to free.
//! - [`chat_move`]: slice7 foundation, canonical generations, tagged grant contexts, strict
//!   prepared metadata moves and pending-intent fences for every participating list writer.
//!   The public move route and Node grant enforcement are intentionally unchanged in this slice.
//! - [`lifecycle`]: a project's create and delete (core `POST /api/projects`, `DELETE
//!   /api/projects/{projectId}`, sbstndalton/noevia#2191 round 2), with the purge of its files, RAG
//!   index, chats and their attachments projects; [`outbox`]: the durable index outbox those writes
//!   leave for core's RAG and storage engines (`users/<id>/index-outbox.db`).
//! - [`js`]: the JavaScript built-ins those handlers rely on.
//!
//! The caller (bins/noevia-server) has already run Node's request gate (session, origin, CSRF);
//! every path here is inside the signed-in user's own directory, so tenant scope is the user id
//! the gate returned and nothing from the request picks another user's files.

pub mod assets;
pub mod chat_delete;
pub mod chat_move;
pub mod chats;
pub mod config;
pub mod context;
pub mod context_window;
pub mod document_ingest;
pub mod document_pages;
pub mod extract;
pub mod extract_queue;
pub mod files;
pub mod folder_import;
pub mod frame;
pub mod free_chats;
pub mod history;
pub mod history_sanitize;
pub mod import;
pub mod instruction_skills;
pub mod journal;
pub mod js;
pub mod legacy_claim;
pub mod lifecycle;
pub mod lock;
pub mod originals;
pub mod outbox;
pub mod partial;
pub mod project_documents;
pub mod project_storage;
pub mod provider_rows;
pub mod research_jobs;
pub mod retention;
pub mod skill_routes;
pub mod skills;
pub mod source_jobs;
pub mod source_limits;
pub mod source_ops;
pub mod source_reply;
pub mod source_storage;
pub mod source_work;
pub mod sources;
pub mod staging;
pub mod storage_latch;
pub mod store;
pub mod store_api;
pub mod sync;
pub mod transcript;
pub mod upload;
pub mod uploads_ingest;
pub mod workspace;
pub mod workspace_scan;

/// The whole-file cap of a streamed storage read (96 MiB, core's upload limit and the default of
/// `PROJECT_DOCUMENT_READ_BYTES`); noevia-server checks it equals server-storage-reads' `FILE_CAP`.
pub const SOURCE_FILE_CAP: u64 = 96 * 1024 * 1024;

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

/// The wall clock now, in epoch milliseconds: for what is written when it happens (a latch hold, an
/// operation's listing). [`Ctx::now_ms`] is the moment the request arrived, hours old by the end of a
/// long sync or a job that waited its turn.
pub fn wall_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
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
