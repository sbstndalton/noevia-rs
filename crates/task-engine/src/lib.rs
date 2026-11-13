//! noevia-core's `server/jobs.cjs` job store and the task start, approve and cancel state machine,
//! in Rust, on the `task-lifecycle` crate (R7 head start).
//!
//! What this crate does:
//!
//! - [`JobStore`]: one append-only journal per job, in memory. [`JobStore::create`], `start`,
//!   `request_approval`, `decide_approval`, `complete`, `fail`, `cancel`, `interrupt`, `recover`,
//!   `prune`, `list`, `get`, and `load` for a journal read from disk.
//! - [`fold`]: the status machine and the view a read returns (jobs.cjs `derive`), with the task
//!   lifecycle moved through `task_lifecycle::transition`.
//! - [`to_jsonl`] / [`parse_jsonl`]: the JSON Lines form of a journal.
//!
//! What it does not do: file I/O, the SHA-256 hash chain, the lifecycle authority events
//! (`task.stage`, `task.revision`), plan and review bounding, the harness, the pipeline, the
//! approval registry and the network guard. Nothing calls this crate yet; see `handoff/foundation/R7.md`.
//!
//! Library code has no `unsafe`, no panics and no `unwrap`/`expect`; every input-derived value is
//! checked before use.

#![forbid(unsafe_code)]

mod codec;
mod error;
mod event;
mod fold;
mod store;

pub use codec::{parse_jsonl, to_jsonl};
pub use error::EngineError;
pub use event::{
    valid_job_id, Event, AUTHORITY_TYPES, EVENT_TYPES, MAX_EVENTS_PER_JOB, MAX_EVENT_DATA_BYTES,
};
pub use fold::{fold, JobView, Status};
pub use store::{JobStore, Limits, NewJob, DECISIONS};
