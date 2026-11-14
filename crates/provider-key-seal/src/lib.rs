//! The seal-on-load migration of a plaintext provider key: the one write noevia-core still makes
//! to `users/<id>/providers.json` (core `server/workspace.cjs`: `isPlaintextKey`, the load's
//! `legacyKeys` flag read by `needsKeySeal`, and `sealLegacyProviders`). The Rust front writes the
//! file everywhere else (provider_routes.rs seals every row it saves); a key left in plaintext
//! by a file from before sealing, a restore or a hand edit is sealed by Node on the next request
//! that loads the workspace, through the front's project store (compare-and-swap on the file's
//! version). This crate is that migration without I/O, so the front can run it and the Node
//! write can go:
//!
//! - [`is_plaintext_key`]: the one predicate the load and the migration share (#L5 in core: a
//!   non-empty string that is not an `enc:v<digits>:` envelope).
//! - [`needs_key_seal`]: what the load reports for a file's text (`legacyKeys`), including the
//!   cases where core's load throws and fails closed to "nothing to seal".
//! - [`plan`]: one attempt's read-modify step: the text to write, or why nothing is written.
//! - [`Gate`]: single flight plus the 30-second quiet window after a failure or a compare-and-swap
//!   that never lands.
//! - [`seal_legacy`]: the attempt loop (at most [`MAX_ATTEMPTS`]) over a [`Store`] and a
//!   [`Sealer`].
//!
//! Nothing calls it yet (inert). The recorded oracle is `tests/record-node-seal.cjs`, which runs
//! core's own `createWorkspaceStore` against scripted stores; `tests/replay.rs` replays it.
//!
//! Differences from core, on purpose (both Node sides are sbstndalton/noevia#2814):
//! - without a sealer core still "changes" every plaintext row (to itself) and writes the file on
//!   every request; [`plan`] needs a sealer and writes nothing without one ([`Plan::NoSealer`]);
//! - text nested deeper than js-json follows (128 levels) or holding a character of the lone
//!   surrogate range is refused by the parser: no write, where core would seal ([`Plan::NotJson`]
//!   with [`Unparsed::Unsupported`]). Core never writes such a file.
//!
//! No key, plaintext or ciphertext is ever in an error, a warning or a `Debug` output here.

use std::future::Future;

use js_json::wtf16;
use js_json::JValue;

/// Core's attempt bound: `for (let attempt = 0; attempt < 4; attempt++)`.
pub const MAX_ATTEMPTS: usize = 4;

/// Core's quiet window after a failure: `Date.now() - sealFailedAt < 30000`.
pub const QUIET_MS: u64 = 30_000;

/// The file the migration reads and writes, relative to the account's folder.
pub const FILE: &str = "providers.json";

/// Core's warning when every attempt met a newer version (no detail is added).
pub const WARN_CONFLICT: &str =
    "[providers] could not seal legacy provider keys: the file kept changing; trying again shortly";

/// Core's warning prefix when the store or the sealer failed. Core appends `e.message`; this crate
/// appends only a fixed code ([`Failure::code`]) so no value can leak into the log.
pub const WARN_FAILED: &str =
    "[providers] could not seal legacy provider keys through the project store:";

/// core `isPlaintextKey`: `typeof value === 'string' && value !== '' && !/^enc:v\d+:/.test(value)`.
pub fn is_plaintext_key(value: &JValue) -> bool {
    match value {
        JValue::Str(s) => !s.is_empty() && !is_envelope(s),
        _ => false,
    }
}

/// `/^enc:v\d+:/`: the prefix, at least one ASCII digit, then a colon.
fn is_envelope(s: &str) -> bool {
    let Some(rest) = s.strip_prefix("enc:v") else {
        return false;
    };
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    digits > 0 && rest.as_bytes().get(digits) == Some(&b':')
}

/// Whether a JS value is an object (or array): what `row.apiKey = …` can be assigned on in strict
/// mode, and what `row &&` lets through.
fn is_object_like(v: &JValue) -> bool {
    matches!(v, JValue::Obj(_) | JValue::Arr(_))
}

/// What core's load reports as `legacyKeys` for the file's text (`None`: the file is missing or
/// unreadable), with a sealer present (core always builds the workspace with one).
///
/// Core: `readJson(file, { providers: [] }).providers || []`, then for each row
/// `if (isPlaintextKey(provider.apiKey)) plaintext = true; openProviderKey(provider, secrets)`.
/// `openProviderKey` assigns `row.apiKey`, which throws in strict mode on a primitive row (and
/// reading `.apiKey` of `null` throws); a thrown load is the reread's fallback, which sets
/// `legacyKeys = false`. So:
/// - missing, unreadable or unparseable file: `false` (readJson's fallback has no rows);
/// - top level `null`: `false` (`null.providers` throws);
/// - top level array or primitive, or a falsy `providers`: `false` (no rows);
/// - `providers` that is not an array (an object, a non-empty string, `true`, a number): `false`
///   (the iteration or the first row's assignment throws);
/// - any row that is `null` or a primitive: `false` (the load throws there and every private row
///   is dropped: sbstndalton/noevia#2814 item 1, to be fixed here and in core together);
/// - else: whether any row's `apiKey` is a plaintext key.
pub fn needs_key_seal(text: Option<&str>) -> bool {
    let Some(text) = text else {
        return false;
    };
    let Ok(doc) = wtf16::parse(text) else {
        return false;
    };
    let JValue::Obj(_) = &doc else {
        return false;
    };
    let JValue::Arr(rows) = doc.get("providers") else {
        return false;
    };
    if !rows.iter().all(is_object_like) {
        return false;
    }
    rows.iter().any(|row| is_plaintext_key(row.get("apiKey")))
}

/// Why a file's text did not parse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unparsed {
    /// `JSON.parse` throws too.
    Invalid,
    /// `JSON.parse` would accept it; the parser here does not (see the crate docs).
    Unsupported,
}

/// One attempt's decision for the text the store returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// `text === null`: there is no file. Core returns (nothing to do).
    NoFile,
    /// `JSON.parse` threw. Core returns.
    NotJson(Unparsed),
    /// `!doc || !Array.isArray(doc.providers)`. Core returns.
    NoRows,
    /// No row (an object or array, as `row &&` allows) holds a plaintext key. Core returns.
    NothingToSeal,
    /// There are plaintext keys but no sealer (see the crate docs). Nothing is written.
    NoSealer,
    /// Write this text (`JSON.stringify(doc, null, 2)`) at the version that was read. `sealed`
    /// is how many keys it seals.
    Write { text: String, sealed: usize },
}

/// Seals one provider key under the current credential key (core `secrets.encrypt(value)`: an
/// `enc:v1:` envelope, never bound to an account).
pub trait Sealer {
    /// `plaintext` is the key as UTF-8 bytes the way Node's `Buffer.from(value, 'utf8')` makes them
    /// (a lone surrogate becomes U+FFFD). Returns the envelope text. An error is a fixed code.
    fn seal(&self, plaintext: &[u8]) -> Result<String, Failure>;
}

/// A failure, as a fixed code only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    /// The sealer could not seal (core: `secrets.encrypt` threw).
    Seal,
    /// The store's read failed or answered something unusable.
    StoreRead,
    /// The store's write failed (not a version conflict).
    StoreWrite,
}

impl Failure {
    /// The code written after [`WARN_FAILED`].
    pub fn code(self) -> &'static str {
        match self {
            Failure::Seal => "seal_failed",
            Failure::StoreRead => "store_read_failed",
            Failure::StoreWrite => "store_write_failed",
        }
    }
}

/// The bytes Node's `Buffer.from(s, 'utf8')` makes of a (js-json wtf16) string: a mapped lone
/// surrogate is written as U+FFFD, everything else as its UTF-8.
pub fn node_utf8(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    let mut buf = [0u8; 4];
    for c in s.chars() {
        let c = if wtf16::lone_unit(c).is_some() {
            char::REPLACEMENT_CHARACTER
        } else {
            c
        };
        out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
    }
    out
}

/// One attempt's read-modify step over the text the store returned (`None`: no file).
///
/// Core, per attempt: return on no text, on a parse error, or on `!doc || !Array.isArray(
/// doc.providers)`; seal `row.apiKey` of every row with `row && isPlaintextKey(row.apiKey)`;
/// return if nothing changed; else write `JSON.stringify(doc, null, 2)`. A sealer error ends the
/// whole migration (core's `encrypt` throws out of the async body), so it is returned as `Err`.
pub fn plan(text: Option<&str>, sealer: Option<&dyn Sealer>) -> Result<Plan, Failure> {
    let Some(text) = text else {
        return Ok(Plan::NoFile);
    };
    let mut doc = match wtf16::parse(text) {
        Ok(doc) => doc,
        Err(wtf16::Refusal::Invalid) => return Ok(Plan::NotJson(Unparsed::Invalid)),
        Err(wtf16::Refusal::Unsupported) => return Ok(Plan::NotJson(Unparsed::Unsupported)),
    };
    // `!doc`: null, false, 0, "" (and NaN, which JSON cannot hold). An array or a primitive has no
    // own `providers`, so `doc.providers` is undefined, not an array.
    let JValue::Obj(fields) = &mut doc else {
        return Ok(Plan::NoRows);
    };
    let Some(JValue::Arr(rows)) = fields
        .iter_mut()
        .find(|(k, _)| k == "providers")
        .map(|(_, v)| v)
    else {
        return Ok(Plan::NoRows);
    };
    let plaintext = rows
        .iter()
        .filter(|row| is_object_like(row) && is_plaintext_key(row.get("apiKey")))
        .count();
    if plaintext == 0 {
        return Ok(Plan::NothingToSeal);
    }
    let Some(sealer) = sealer else {
        return Ok(Plan::NoSealer);
    };
    let mut sealed = 0;
    for row in rows.iter_mut() {
        // An array row never has an own `apiKey` from JSON; only objects can hold one.
        let JValue::Obj(fields) = row else { continue };
        let Some((_, key)) = fields.iter_mut().find(|(k, _)| k == "apiKey") else {
            continue;
        };
        if !is_plaintext_key(key) {
            continue;
        }
        let JValue::Str(plain) = key else { continue };
        let envelope = sealer.seal(&node_utf8(plain))?;
        *key = JValue::Str(envelope);
        sealed += 1;
    }
    // An object always stringifies; `None` is only for `undefined`.
    let Some(text) = wtf16::stringify_pretty(&doc) else {
        return Ok(Plan::NoRows);
    };
    Ok(Plan::Write { text, sealed })
}

/// What the store's read returned: the file's version and its text (`None`: no file).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Read {
    pub version: String,
    pub text: Option<String>,
}

/// The front's project store as core's project-store-client.cjs uses it for this file
/// (`read(user, file)` and `write(user, file, version, text)`, a compare-and-swap).
pub trait Store {
    /// The file as of now.
    fn read(&self, user: &str, file: &str) -> impl Future<Output = Result<Read, Failure>>;
    /// Writes `text` only if the file is still at `version`: `Ok(true)` written, `Ok(false)` a
    /// newer version was there (core: `saved.ok` false), `Err` the store failed.
    fn write(
        &self,
        user: &str,
        file: &str,
        version: &str,
        text: &str,
    ) -> impl Future<Output = Result<bool, Failure>>;
}

/// How one migration ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// A write landed; `sealed` keys were sealed and `attempts` attempts were made.
    Sealed { sealed: usize, attempts: usize },
    /// An attempt found nothing to write (the plan says why).
    Nothing { plan: Plan, attempts: usize },
    /// Every attempt met a newer version: core's [`WARN_CONFLICT`] and the quiet window.
    Conflicted,
    /// The store or the sealer failed: core's [`WARN_FAILED`] and the quiet window.
    Failed(Failure),
}

impl Outcome {
    /// Whether core starts the quiet window (`sealFailedAt = Date.now()`).
    pub fn quiets(&self) -> bool {
        matches!(self, Outcome::Conflicted | Outcome::Failed(_))
    }

    /// The one warning core logs for this outcome, if any.
    pub fn warning(&self) -> Option<String> {
        match self {
            Outcome::Conflicted => Some(WARN_CONFLICT.to_owned()),
            Outcome::Failed(f) => Some(format!("{WARN_FAILED} {}", f.code())),
            _ => None,
        }
    }
}

/// The migration for one account: core `sealLegacyProviders`'s async body.
pub async fn seal_legacy<S: Store>(store: &S, sealer: Option<&dyn Sealer>, user: &str) -> Outcome {
    for attempt in 1..=MAX_ATTEMPTS {
        let read = match store.read(user, FILE).await {
            Ok(read) => read,
            Err(f) => return Outcome::Failed(f),
        };
        let (text, sealed) = match plan(read.text.as_deref(), sealer) {
            Ok(Plan::Write { text, sealed }) => (text, sealed),
            Ok(plan) => {
                return Outcome::Nothing {
                    plan,
                    attempts: attempt,
                }
            }
            Err(f) => return Outcome::Failed(f),
        };
        match store.write(user, FILE, &read.version, &text).await {
            Ok(true) => {
                return Outcome::Sealed {
                    sealed,
                    attempts: attempt,
                }
            }
            Ok(false) => continue,
            Err(f) => return Outcome::Failed(f),
        }
    }
    Outcome::Conflicted
}

/// Single flight and the quiet window, per account workspace (core's `sealing` promise and
/// `sealFailedAt`).
///
/// Core: `if (sealing || Date.now() - sealFailedAt < 30000) return sealing || Promise.resolve();`
/// `sealFailedAt` starts at 0, so the first call is never quiet.
#[derive(Debug, Clone, Default)]
pub struct Gate {
    running: bool,
    failed_at: Option<u64>,
}

/// What [`Gate::begin`] decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admit {
    /// Run the migration now, then call [`Gate::end`].
    Run,
    /// One is already running: wait for it (core returns the same promise).
    Join,
    /// Within the quiet window: nothing runs.
    Quiet,
}

impl Gate {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether a migration may start at `now_ms` (milliseconds since the epoch, core's
    /// `Date.now()`). A running one is joined even inside the quiet window, as core's `sealing ||`
    /// comes first.
    pub fn begin(&mut self, now_ms: u64) -> Admit {
        if self.running {
            return Admit::Join;
        }
        if let Some(at) = self.failed_at {
            // A clock that went back counts as inside the window (JS: a negative difference < 30000).
            if now_ms.saturating_sub(at) < QUIET_MS {
                return Admit::Quiet;
            }
        }
        self.running = true;
        Admit::Run
    }

    /// Records how the migration started by [`Gate::begin`] ended (`.finally(() => { sealing =
    /// null; })`; the quiet window starts on [`Outcome::quiets`]).
    pub fn end(&mut self, outcome: &Outcome, now_ms: u64) {
        self.running = false;
        if outcome.quiets() {
            self.failed_at = Some(now_ms);
        }
    }

    /// Whether a migration is running.
    pub fn running(&self) -> bool {
        self.running
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
mod tests;
