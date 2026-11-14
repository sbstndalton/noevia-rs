//! The write side of core's project index (`server/rag.cjs` and `server/vector-slots.cjs`).
//!
//! `project-retrieval` reads an index; this crate writes one, in the same on-disk format, so either
//! side can open what the other wrote:
//!
//! - [`chunk`]: `chunkText` (page-aware, 1200-unit windows, 150-unit stride) and the chunk hash, on
//!   UTF-16 code units as JavaScript counts them.
//! - [`slots`]: the per-model vector slots (`vec_meta` records, the legacy table, `vec0` tables and
//!   their hash maps): find, ensure, write vectors, drop chunks, adopt the legacy table.
//! - [`index`]: open an index (schema and column migrations), and index, tail-append and delete one
//!   file, with the embedding call behind the [`index::Embed`] trait.
//! - [`state`]: the in-process per-file state (`fileGenerations`, `fileVersions`, `pruneFileState`)
//!   that decides which runs are superseded and which versions a search must filter by.
//! - [`indexer`]: the process-level facade with the per-file queue, over a [`indexer::Host`].
//! - [`embed`]: the batch rules of `embedFramed` (order by `index`, fall back to one text per call).
//! - [`request`]: the JSON contract of the store route for Node's three writes (file, tail, delete).
//! - [`http_embed`]: `embedOnce`'s `/v1/embeddings` request over the embedding worker's guarded HTTP.
//!
//! Nothing here is reachable from a server yet: it ships unused, and the wiring goes behind a switch
//! of its own (`NOEVIA_RUST_RETRIEVAL` already serves the read side in production).

pub mod chunk;
pub mod embed;
pub mod http_embed;
pub mod index;
pub mod indexer;
pub mod request;
pub mod slots;
pub mod state;
