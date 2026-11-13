//! The native chat turn (track R8): noevia-core's `chat.cjs` turn from the point core hands a prepared
//! turn off (rust-chat-native.cjs, the `prepared` frame) to its saved end, replayed against recorded
//! Node turns (`tests/record_turns.cjs`, `tests/native_turns.rs`). Nothing calls it yet: the front's
//! executor, behind a per-account switch that is off by default, is the next step.
//!
//! - [`prepared`]: the hand-off frame, read strictly; [`handoff`]: core's stream split into frames until it;
//! - [`native`]: the continuation: the model's lease, the runtime limit and the context fit, the rounds
//!   (request with reasoning effort and thinking cap, stream, fallback, tool rounds), the close and the
//!   title; [`host`] is what it asks of the process that runs it;
//! - [`gate`]: one tool call through the per-call permission gate; [`skills`]: the instruction skills' checks;
//! - [`after`]: what a turn keeps when it ends (the interrupted reply, the partial);
//! - [`round`], [`sse`], [`cap`], [`effort`]: a model round's stream, its lines, the thinking cap, the
//!   request's reasoning effort;
//! - [`record`]: the turn record and the partial file's entry; [`title`]: the generated title;
//!   [`helpers`]: the search-query cleaner, the announced-action check, the final-answer helpers and the
//!   tool route notes; [`reduce`]: a tool result compacted for the model; [`sources`]: the injected
//!   files an answer drew on (#1793);
//! - [`sink`] and [`event`]: the stream's order rule, and typed events for tests; [`guard`]: the stream
//!   guard for tool-call arguments; [`js`]: JavaScript string and object semantics.
//!
//! Every behaviour is proven by fixtures recorded from the Node code; each generator has a `--check`
//! mode that records again at the pinned core (`tests/oracle-core.ref`) and compares bytes (CI runs them
//! from `.github/workflows/chat-turn.yml`, which comes with the front's wiring). Named differences are
//! listed in each module's doc.

pub mod after;
pub mod cap;
pub mod effort;
pub mod event;
pub mod gate;
pub mod guard;
pub mod handoff;
pub mod helpers;
pub mod host;
pub mod js;
pub mod native;
pub mod prepared;
pub mod record;
pub mod reduce;
pub mod round;
pub mod sink;
pub mod skills;
pub mod sources;
pub mod sse;
pub mod title;

pub use event::{Event, Phase, TimeToFirstToken};
pub use guard::{ArgGuard, Failure};
pub use record::{
    bounded, partial_file, TurnRecord, PARTIAL_CAP_UNITS, STOPPED_SENDER, TOOL_RESULT_LIMIT,
};
pub use sink::{OrderError, Turn};
