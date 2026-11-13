//! Embedding profile, configuration and address guard (ported from `diary/agent/embedding.py`,
//! #1636 / #1638 / #1759).
//!
//! What is ported, with the Python names in Rust style:
//!
//! * [`text`]: `clean_title`, `frame`, `split_text`, `_mean` and `embed_fitting` (a document the
//!   embeddings server refuses as too long is cut and the piece vectors are averaged).
//! * [`target`]: `normalize_target`, `_normalize_base_url`, `_MODEL_RE` and `seed_from_cfg`.
//! * [`config`]: `EmbeddingConfig` (active / next / previous, the saved
//!   `embedding-config.json`, seeded refusals) and `state_path_for`.
//! * [`address`]: `non_local_reason` and `check_embedding_address`, with CPython 3.12's
//!   `ipaddress` and `urlsplit` rules reproduced and proven against recorded samples.
//! * [`guard`]: `guard_seeded_addresses` as a pure decision plus a `recheck` step; timers, threads
//!   and logging stay with the caller.
//!
//! Not ported on purpose: `Embedder`, `EmbeddingView`, `shared_for` and `view_for` (the HTTP
//! client wiring comes later).
//!
//! # Concurrency
//!
//! The Python `EmbeddingConfig` guards itself with an `RLock`. Here every method that changes it
//! takes `&mut self` and the caller wraps the value in a `Mutex` when several threads share it
//! (the shared holder is the not-yet-ported `shared_for`). The guard never holds that lock while
//! it checks addresses or sleeps: it works from a [`config::Snapshot`] and returns decisions,
//! which the caller applies under the lock.
//!
//! # Known differences from the Python module
//!
//! Each of these has a case in `embedding-fixtures.json` (`known_differences`) that pins what the
//! Rust code answers instead.
//!
//! 1. **JSON files.** The saved state is read with `serde_json`. Python's `json.loads` also
//!    accepts the literals `NaN`, `Infinity`, `-Infinity`, numbers beyond `f64` range (as `inf`),
//!    integers of more than 64 bits (kept exactly) and lone `\u` surrogates; here such a file is
//!    "damaged" or the number is read as an `f64`. The reason is the same shape of message, but
//!    see 2.
//! 2. **Exception text in the damaged message.** The message is
//!    `The saved embedding configuration <path> cannot be read (<text>). Fix or delete it.`. The
//!    text is Python's exact text for everything this port raises itself (`unknown version`, the
//!    [`EmbeddingConfigError`] messages, `'list' object has no attribute 'get'`, and an OS error
//!    as `[Errno N] <strerror>: '<path>'`), but for a JSON syntax error or invalid UTF-8 it is
//!    the `serde_json` / `std` text, not `Expecting value: line 1 column 1 (char 0)`. The
//!    differential test compares those messages with the text between the parentheses masked.
//! 3. **`str()` of non-string JSON values.** A model or profile that is not a string is
//!    converted the way Python's `str(value or "")` does (`5` is the model `5`, `true` is
//!    `True`, a float uses `repr`). Three corners differ: a dict with several keys is printed in
//!    key order here (Python: the order of the file), a string inside a list is quoted without
//!    escaping non-ASCII format, private-use or unassigned characters, and `-0` inside a list is
//!    `-0.0`.
//! 4. **URL host with NFKC.** Python rejects a netloc whose NFKC form contains `/ ? # @ :`
//!    (`urllib.parse._checknetloc`). The characters that do so are a fixed table (Unicode 15.0,
//!    recorded from CPython 3.12); a character added by a later Unicode version is not in it.
//! 5. **Lower-casing** of a host name uses Rust's Unicode tables, Python's `str.lower` those of
//!    its Unicode 15.0 database.
//! 6. **Mismatched argument lengths.** Python raises `IndexError` when `kind` or `titles` is
//!    shorter than `texts`, when a send returns no vector for a piece, or when vectors differ in
//!    width; here those are [`EmbeddingConfigError`] / [`text::FitError::Malformed`] values.
//! 7. **Logging** of the split in `embed_fitting` is not done; the guard returns its log lines
//!    as data ([`guard::LogLine`]).
//! 8. **Re-check scheduling (a Python defect, not ported).** Python's `again` re-schedules itself
//!    through the name `again`, which the start-up loop has rebound to the last held-back address
//!    by then. With both addresses held back, the first one is re-checked once and never again
//!    (the last one is checked twice per round), so it can stay refused until a restart even after
//!    it resolves. [`guard::recheck_seeded`] steps one address and the caller re-schedules that
//!    same address; the differential test reproduces Python's loop to compare the recorded runs
//!    and has a separate test for the sound loop.
//! 9. **Vectors are `f64`.** Python's `_mean` also accepts `int` vector items (and sums them
//!    exactly); the fixtures and the callers use floats.

pub mod address;
pub mod config;
pub mod guard;
pub mod pystr;
pub mod target;
pub mod text;

pub use address::{
    check_embedding_address, non_local_reason, system_resolver, AddressCheck, ResolveError,
};
pub use config::{
    state_path_for, ChangeError, EmbeddingConfig, SeededReasons, Snapshot, Source, State, Which,
};
pub use guard::{
    guard_seeded_addresses, recheck_seeded, CheckFailure, GuardOutcome, GuardRefusal, LogLevel,
    LogLine, Pending, RecheckNext, RecheckStep, RECHECK_S, RETRY_DELAYS_S,
};
pub use target::{
    model_matches, normalize_base_url, normalize_target, normalize_target_from, seed_from_cfg,
    seed_warning,
};
pub use text::{
    clean_title, embed_fitting, frame, mean, split_text, FitError, Kinds, SendError, SendFn,
    SPLIT_MARGIN, SPLIT_MAX_DEPTH, SPLIT_MAX_PARTS, SPLIT_MIN_CHARS,
};

pub const PROFILES: [&str; 2] = ["nomic-v1", "embeddinggemma-2"];
pub const KINDS: [&str; 3] = ["query", "document", "similarity"];

/// Python `EmbeddingConfigError(ValueError)`: a refusal with a code and an HTTP-style status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddingConfigError {
    pub message: String,
    pub code: &'static str,
    pub status: u16,
}

impl EmbeddingConfigError {
    /// `EmbeddingConfigError(message)`: code `invalidTarget`, status 400.
    pub fn invalid_target(message: impl Into<String>) -> Self {
        Self::new(message, "invalidTarget", 400)
    }

    pub fn new(message: impl Into<String>, code: &'static str, status: u16) -> Self {
        Self {
            message: message.into(),
            code,
            status,
        }
    }
}

impl std::fmt::Display for EmbeddingConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for EmbeddingConfigError {}

/// `", ".join(PROFILES)`, used in several messages.
pub(crate) fn known_profiles() -> String {
    PROFILES.join(", ")
}
