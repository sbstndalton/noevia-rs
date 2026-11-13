//! The Diary's answering runtime (R9): the exchange deadline and the async model client, over the
//! I/O-free protocol of `diary-core::llm`. Nothing calls this crate in production yet; it is wired
//! behind `DIARY_RUST_ANSWER` (off by default) in a later step.

pub mod deadline;
pub mod exchange;
pub mod model;
