//! Slices of `noevia/deploy/tools/assemble-release.sh` in Rust (council p5, pool item 9).
//!
//! The shell assembler stays the release source. This crate ports the parts that do not depend on the
//! ownership pairing guards, with the shell as the oracle:
//! - [`lock`]: `lock_value` (`sed -n "s/^KEY=//p" | tail -n 1`) over `release/versions.lock`.
//! - [`plan`]: `resolve_component` and the noevia-rs block: which component is exported from the
//!   noevia commit, which is fetched at a pinned ref, and the exact refusal text of each bad lock.
//! - [`digest`]: `sha256_of` and the tarball checksum check of `fetch_remote`.
//! - [`tar`]: a fail-closed reader (`tar -xzf … --strip-components=N`) and a deterministic writer.
//! - [`source`]: `export_self` (git archive) and `fetch_remote` (curl, checksum, extract) as a [`plan::Source`].
//! - [`tree`]: the end of the assembly (ui-data prune, `release-refs`, `.dockerignore`, the required
//!   entries and the final `tar -czf`), plus a content manifest to compare two release tarballs.
//!
//! - [`guard`] (council p7): the ownership pairing guards' shared primitives (`routes_toml`,
//!   `project_routes_rs`, `switched_routes`, `project_routes_core`, `code_lines`, `code_has`'s comment
//!   filter, `move_capability`) and the first pairings built on them (feature, model reads).
//!
//! - [`guard`] (council p8) also: the four project pairings (source reads, create/delete, file delete,
//!   configuration); [`bridged`]: the calibration cancel and start/read, code egress issuer, deep research,
//!   task read and model scan read pairings, with `code_has` over real EREs.
//!
//! - [`guard`] (council p10) also: the document page reader, conversation import, document upload, public
//!   page and plugin directory pairings.
//!
//! - [`facts`] (council p10): the eight single-path pairings whose Core side is a refusal module plus a
//!   facts or state bridge (auto-tune settings, dismissal, resume, cancellation and untuned discovery;
//!   idle unload, model assignments and preset reads) as one table, and [`bridged::posix_ere`], which
//!   gives `code_has` POSIX bracket expressions (`[[]`, `[]]`, a literal `\`) as grep reads them.
//!
//! - [`core_requires`] (council p12): the node-backed `core_requires` (does a Core module really call
//!   `require('<module>')`) as a Rust tokenizer, so no pairing needs node; [`model_pairings`]: track R11's
//!   model writes, model manager proxy and embedding admin pairings on it, with `code_body`.
//!
//! - [`route_pairings`] (council p13): the project storage write pairing on `core_requires`, and the run of
//!   route pairings from the close-out routes to the evidence recheck (settings, storage, providers, ChatGPT
//!   sign-in, MCP credentials and views, Google Drive, off-site backup, model sizing, state reads,
//!   maintenance and recheck) as one table-driven shape plus their extra checks.
//!
//! - [`features_reader`] (council p15): the ten strict FEATURES readers over the front's lib.rs (MCP discovery,
//!   auto-roles write, maintenance lease, inference admission, chat mirror, residual fallbacks, source startup,
//!   DAV listener, index outbox, media background) without node, one tokenizer and one element splitter.
//!
//! Nothing in the release path calls this crate yet.

pub mod bridged;
pub mod core_requires;
pub mod digest;
pub mod facts;
pub mod features_reader;
pub mod guard;
pub mod lock;
pub mod model_pairings;
pub mod plan;
pub mod route_pairings;
pub mod source;
pub mod tar;
pub mod tree;
