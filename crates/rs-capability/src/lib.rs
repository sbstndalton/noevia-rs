//! Release-assembly capability detector, ported from noevia `deploy/tools/lib/rs-capability.cjs` and
//! `dockerfile-rs-features.cjs` (behaviour, verdicts and refusals; see `deploy/tools/lib/rs-capability.md` there).
//!
//! The assembler's ownership pairing guards ask whether a Rust source tree declares a protocol marker literal in its
//! crate-root `FEATURES` list, and whether a marker gated by `#[cfg(feature = "F")]` is compiled into the binary the
//! release image actually ships. [`detect`] reads the source without executing it; [`derive_features`] proves the
//! shipped build's feature set from the image recipe and the same-snapshot Cargo manifest, or refuses (`None`).
//! Nothing here is wired into release assembly yet: the shell adapter still calls the Node helper.

pub mod detect;
mod js;

pub use detect::{detect, Verdict};
