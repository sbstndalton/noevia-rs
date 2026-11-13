//! The pure rules of the project retrieval path (R4 head start, not wired anywhere yet).
//!
//! Ported from noevia-core `server/rag.cjs`, checked against outputs recorded from that file
//! on synthetic inputs (`tests/fixtures`, recorded by `tests/generate_fixtures.cjs`):
//! - [`names`]: the quoting of file names for system text and the question-naming rule (#1457).
//! - [`hits`]: the excerpt type a search hands to `filesContext` (the search itself is R4-2).
//! - [`context`]: `filesContext` when there is no index (manifest, notice, whole-file budget).
//!
//! Not here: the vector query (R4-2), the rerank client, image retrieval and any switch.

pub mod context;
pub mod hits;
pub mod names;
