//! Pure pieces of the Diary language-model and retrieval core (R9 head start).
//!
//! Ported from `diary/agent/util.py` and `diary/agent/retrieval_trace.py` on the Python service.
//! Nothing here does I/O, reads the environment or touches the database; nothing calls this crate
//! in production yet, so it changes no behaviour.

pub mod retrieval_trace;
pub mod tokens;
