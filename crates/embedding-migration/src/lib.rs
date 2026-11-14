//! noevia-core's embedding configuration writer in Rust (#1636 / #1638 / #1660 / #1672).
//!
//! - [`target`]: `embedding-config.cjs`'s target rules: [`target::normalize_target`] (the 400
//!   `invalidTarget` refusals), identity and sameness, and the environment seed.
//! - [`served`]: `embedding-served.cjs`: does an embeddings server list a model name.
//! - [`config`]: `createEmbeddingConfig` / `loadEmbeddingConfig`: active / next / previous, saved
//!   atomically to `embedding-config.json` under the same flock (`<file>.lock`) the native embedding
//!   worker and core's `embedding-config-lock.cjs` take, the damaged-file fallback and the
//!   start-up refusal of seeded addresses.
//! - [`migration`]: the actions of `embedding-migration.cjs` as production runs them (the native
//!   embedding worker owns the re-index pass): set-next, clear-next, switch, rollback and finish,
//!   with their served-model, coverage and endpoint-parity checks, the dropped vector slots and the
//!   status object `GET /api/embedding` returns as `projects`.
//!
//! Network, index files and the worker are behind [`migration::Host`]; nothing here opens a socket.
//! Profile names are data (the caller passes core's list, [`target::Profiles`]). Nothing calls this
//! crate yet: the route, its switch and core's refusal are the next steps.

#![forbid(unsafe_code)]

pub mod js;
pub mod target;
