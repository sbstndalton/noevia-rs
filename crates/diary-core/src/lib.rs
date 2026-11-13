//! Pure pieces of the Diary language-model and retrieval core (R9 head start).
//!
//! Ported from `diary/agent/util.py` and `diary/agent/retrieval_trace.py` on the Python service.
//! Nothing here does I/O, reads the environment or touches the database; nothing calls this crate
//! in production yet, so it changes no behaviour.

pub mod assembler;
pub mod context;
pub mod follow_up;
pub mod llm;
pub mod pipeline;
pub mod pyconv;
pub mod retrieval_trace;
pub mod streaming;
pub mod tokens;
mod unicode15;
