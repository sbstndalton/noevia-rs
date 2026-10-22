//! sbstndalton/noevia#2115, #2117: who may use the one chat-model slot of the inference engine, and
//! when another model may take it.
//!
//! The engine router (in router mode, `--models-max 1`) runs one chat model at a time.
//! Before this crate, every client of the router could evict the loaded chat model: the web
//! server's admission unloaded it even while it was streaming an answer for another chat, and the
//! Diary or any other client behind the relay loaded models without any admission at all. Two chats
//! on different routes then killed each other's generations and ping-ponged the two models.
//!
//! The inference gate (`bins/noevia-inference-gate`) is the only published entry to the router.
//! It classifies every request ([`classify`]) and asks this scheduler ([`Scheduler`]) whether a
//! request that may load a model can run now, must wait, or is refused. The scheduler:
//!
//! - never unloads a model with a request in flight or a live lease (only an explicit admin force
//!   does, and that is a separate, logged call);
//! - queues a request for another model in first-come groups (one group per model), serves a whole
//!   group at once, and switches only when the resident model is idle, so alternating routes cost
//!   one switch per group, never one per request;
//! - bounds everything: queue size, waiting time, lease idle and hard lifetimes, the wait for an
//!   unload to finish, and how long waiters are kept while the router is unreachable;
//! - never loads a model itself: it only ever asks for an unload of an idle model. A sleeping
//!   model is woken by the next request through the router, as before.
//!
//! The scheduler is a plain state machine over a millisecond clock supplied by the caller. It does
//! no I/O and holds no prompt content: only model ids, counts and times.

pub mod classify;
pub mod config;
pub mod listing;
pub mod scheduler;

pub use classify::{classify, Class};
pub use config::{Config, ConfigError};
pub use listing::{parse_listing, Listing, ModelRow, RowStatus};
pub use scheduler::{
    Action, Admission, Arrival, Counters, Grant, LeaseEnd, LeaseKey, QueuedGroup, Refusal,
    Scheduler, Status, Ticket, UnloadOutcome, WaitKind, WaiterKey,
};
