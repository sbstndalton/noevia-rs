//! The pure rules of the project retrieval path (R4 head start, not wired anywhere yet).
//!
//! Ported from noevia-core `server/rag.cjs`, checked against outputs recorded from that file
//! on synthetic inputs (`tests/fixtures`, recorded by `tests/generate_fixtures.cjs`):
//! - [`names`]: the quoting of file names for system text and the question-naming rule (#1457).
//! - [`hits`]: the excerpt type a search hands to `filesContext` (the search itself is R4-2).
//! - [`context`]: `filesContext` when there is no index (manifest, notice, whole-file budget).
//! - [`search`]: the vector half of `searchProject` over an existing index (slot, cosine SQL,
//!   score floor, version filter), read-only, checked against searches recorded from Node over
//!   indexes Node wrote (`tests/generate_search_fixtures.cjs`).
//! - [`policy`]: whether the reranker is on and whether embedding is off, the two decisions a
//!   search makes before it embeds the query (`tests/generate_policy_fixtures.cjs`).
//! - [`rerank`]: the optional reranker over the cosine pool (`tests/generate_rerank_fixtures.cjs`).
//! - [`captions`]: the image captions an index caches (`readCaption`, `writeCaption`).
//!
//! Not here: embedding the query, the rerank client, image retrieval and any switch.

pub mod captions;
pub mod context;
pub mod hits;
pub mod names;
pub mod policy;
pub mod rerank;
pub mod search;
