//! Release-assembly capability detector, ported from noevia `deploy/tools/lib/rs-capability.cjs` and
//! `dockerfile-rs-features.cjs` (behaviour, verdicts and refusals; see `deploy/tools/lib/rs-capability.md` there).
//!
//! The assembler's ownership pairing guards ask whether a Rust source tree declares a protocol marker literal in its
//! crate-root `FEATURES` list, and whether a marker gated by `#[cfg(feature = "F")]` is compiled into the binary the
//! release image actually ships. [`detect`] reads the source without executing it; [`derive_features`] proves the
//! shipped build's feature set from the image recipe and the same-snapshot Cargo manifest, or refuses (`None`).
//! Nothing here is wired into release assembly yet: the shell adapter still calls the Node helper.

/// A lazily compiled pattern written in JavaScript regex terms (see `js::pattern`).
macro_rules! js_re {
    ($source:expr) => {{
        static CELL: std::sync::OnceLock<Option<regex::Regex>> = std::sync::OnceLock::new();
        $crate::js::pattern(&CELL, $source)
    }};
}

pub mod detect;
mod dockerfile;
mod features;
mod js;

pub use detect::{detect, Verdict};
pub use features::derive_features;

/// The active instructions of a Dockerfile as (keyword, arguments), for tests and diagnostics.
pub fn dockerfile_instructions(text: &str) -> Result<Vec<(String, String)>, String> {
    dockerfile::instructions(text).map(|list| list.into_iter().map(|i| (i.kw, i.args)).collect())
}
