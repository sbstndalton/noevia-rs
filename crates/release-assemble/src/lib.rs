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
//! Nothing in the release path calls this crate yet.

pub mod digest;
pub mod lock;
pub mod plan;
pub mod source;
pub mod tar;
pub mod tree;
