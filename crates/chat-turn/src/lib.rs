//! The native chat turn, first pieces (head start for R8; see handoff/foundation/R8.md).
//!
//! Beside noevia-core's `chat.cjs` (2,497 lines), not called by anything yet. It holds:
//! - [`event`]: the events a turn streams, as the JSON Node sends them;
//! - [`sink`]: the order those events must keep (one terminal, `title` only after `done`);
//! - [`guard`]: the stream guard for tool-call arguments, over the `stream-guard` crate;
//! - [`tools`]: the dispatch trait the tool engines implement, and the 300-character result cut;
//! - [`partial`]: the interrupted-turn record;
//! - [`reduce`]: a tool result compacted for the model (`tool-result-reduce.cjs`);
//! - [`relay`]: one model round turned into events, with the reasoning-only note and the
//!   telemetry the client needs.
//!
//! Not here: context and framing assembly, routing and decision calls, persistence through the
//! store, approvals, the Diary stream, rounds with tool results fed back to the model.

pub mod cap;
pub mod effort;
pub mod event;
pub mod gate;
pub mod guard;
pub mod host;
pub mod js;
pub mod native;
pub mod partial;
pub mod prepared;
pub mod reduce;
pub mod relay;
pub mod round;
pub mod sink;
pub mod sse;
pub mod tools;

pub use event::{Event, Phase, TimeToFirstToken};
pub use guard::{ArgGuard, Failure};
pub use partial::{Disposition, Outcome, Partial, ToolStep};
pub use relay::{relay, Relayed, Upstream};
pub use sink::{OrderError, Turn};
pub use tools::{cap_result, ToolCall, ToolDispatch, ToolOutcome};
