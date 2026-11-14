//! The chat transport of the account sign-in provider, ported from noevia-core's sign-in provider
//! module (the half that stays in the chat call path; the device sign-in, the token store and the
//! refresh are already the front's broker in noevia-server). tests/record_transport.cjs names the
//! exact core file it records.
//!
//! An unused leaf: nothing calls it yet. The front wiring (a native turn on the sign-in provider,
//! which `chat-upstream` refuses today) is a later, switched step.
//!
//! - [`request`]: a `/chat/completions` body to the Responses request the backend takes, the
//!   catalogue allowlist, the reasoning-effort check and the model defaults (`fetchFor`).
//! - [`events`]: the backend's SSE stream decoded chunk by chunk and turned into chat parts
//!   (`sseEvents`, `chatParts`), in order, with the rebuild rules for text and tool calls.
//! - [`reply`]: the parts as a `/chat/completions` stream (`chatStream`) or one completion
//!   (`chatCompletion`), and the error answers (`errorResponse`, `upstreamMessage`).
//! - [`session`]: the per-user access-token cache and the refresh-once-on-401 rule
//!   (`session`, `authorised`), sans I/O.
//! - [`texts`]: every person-facing line. Provider and backend names are the caller's; no name is
//!   built in.
//! - [`js`]: the JavaScript coercions the port needs (`Number()`, `Date#toISOString`).

pub mod events;
pub mod js;
pub mod reply;
pub mod request;
pub mod session;
pub mod texts;
