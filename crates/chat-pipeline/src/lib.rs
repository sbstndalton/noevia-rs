//! The chat turn's generation pipeline (core `server/chat.cjs`, `POST /api/chat`), moved to the Rust
//! front stage by stage. Each stage keeps Node's answers exactly (strings as JavaScript holds them:
//! UTF-16 code units, lone surrogates kept through [`js_json::wtf16`]); `tests/differential.rs` replays
//! the answers core's own code gave on synthetic conversations (`tests/fixtures/record.cjs`).
//!
//! - [`replay`]: the saved history a turn replays to the model: which items are kept, how tool
//!   results, applied and declined writes are folded, and the role alternation strict chat
//!   templates need (core `normalizeReplayHistory` and the history mapping of `createChatHandler`).
//! - [`rounds`]: the loop's checks of a reply's text (an announced but untaken tool action, the final-answer pass).
//! - [`sources`]: the sources a reply was grounded in, and which files can be read.
//! - [`supervise`]: step supervision between tool rounds, and the image probe.
//! - [`system`]: the turn's system message (account, date, project, skills, sources, frame, brains).
//! - [`stream`]: one round's engine stream turned into the chat events the turn sends.
//! - [`search`]: a search tool's query without the request wrapper (#1523).
//! - [`title`]: the chat's generated title (#1785).
//! - [`usage`]: the exchange's usage and first-token telemetry across tool rounds.
//! - [`validate`]: what a turn refuses before anything is read or written.
//! - [`gate`]: one tool call through the exchange, the permission gate, the card, the grant and the executor.
//! - [`route`]: where a turn goes (routing modes, provider, Auto roles, model) or why it is refused.
//! - [`end`]: how a turn ends (pauses, the last sources, the fallback text, telemetry).
//! - [`failure`]: the words a failed engine reply shows.
//! - [`writes`]: the writes already done in a chat (#658 repeat flag).
//! - [`record`]: the reply a turn keeps when its client went away (#679, #1507).
//! - [`reduce`]: what a tool result becomes before the model reads it.
//! - [`toolbox`]: which tools a turn offers (requestToolboxes, resolveTools, more_tools).
//! - [`tools_mode`]: Automatic or Manual tools, and what Automatic adds for one message (#1550).
//! - [`preload`]: the #1863 pre-load of the shared Fast/Smart model beside the routing decision.
//! - [`notes`]: the system notes a turn adds or removes (`withSystemNote`, `withoutSystemNote`).
pub mod end;
pub mod failure;
pub mod gate;
pub mod notes;
pub mod preload;
pub mod record;
pub mod reduce;
pub mod replay;
pub mod rounds;
pub mod route;
pub mod search;
pub mod sources;
pub mod stream;
pub mod supervise;
pub mod system;
mod text;
pub mod title;
pub mod toolbox;
pub mod tools_mode;
pub mod usage;
pub mod validate;
pub mod writes;

/// A JavaScript `TypeError` the Node code would have thrown here (a value whose `String()`
/// coercion throws, or a property read on `null`): the turn fails as Node's did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Thrown;
