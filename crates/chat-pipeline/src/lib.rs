//! The chat turn's generation pipeline (core `server/chat.cjs`, `POST /api/chat`), moved to the Rust
//! front stage by stage. Each stage keeps Node's answers exactly (strings as JavaScript holds them:
//! UTF-16 code units, lone surrogates kept through [`js_json::wtf16`]); `tests/differential.rs` replays
//! the answers core's own code gave on synthetic conversations (`tests/fixtures/record.cjs`).
//!
//! - [`replay`]: the saved history a turn replays to the model: which items are kept, how tool
//!   results, applied and declined writes are folded, and the role alternation strict chat
//!   templates need (core `normalizeReplayHistory` and the history mapping of `createChatHandler`).
//! - [`notes`]: the system notes a turn adds or removes (`withSystemNote`, `withoutSystemNote`).
pub mod notes;
pub mod replay;
mod text;

/// A JavaScript `TypeError` the Node code would have thrown here (a value whose `String()`
/// coercion throws, or a property read on `null`): the turn fails as Node's did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Thrown;
