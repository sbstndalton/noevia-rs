//! Start-up guard for `LLM_EMBED_BASE_URL` / `LLM_EMBED_NEXT_BASE_URL` (`guard_seeded_addresses`).
//!
//! A refused value is not used (in memory only); it never stops the service from starting. A name
//! that does not resolve is retried with back-off, then held back and re-checked every
//! [`RECHECK_S`], and used again as soon as it resolves to local addresses.
//!
//! The Python function blocks, sleeps, mutates the configuration and schedules daemon timers.
//! Here that is split so the caller owns the side effects:
//!
//! 1. [`guard_seeded_addresses`] works from a [`Snapshot`], calls `check` and `sleep` for the
//!    first round (the caller runs it on a thread and holds no lock) and returns a
//!    [`GuardOutcome`]: what to refuse (for good or transiently), what to check again, and the log
//!    lines.
//! 2. [`GuardOutcome::apply`] makes the refusals on the configuration (under the caller's lock).
//! 3. For every [`Pending`], the caller schedules a re-check after [`RECHECK_S`], in two halves so
//!    no lock is held across the DNS call: [`recheck_check`] with the configuration unlocked,
//!    then [`apply_recheck`] with it locked (it reads `source` again). A step answering
//!    [`RecheckNext::Again`] is scheduled once more for the SAME [`Pending`], anything else ends it. (Python re-schedules the last held-back address instead, whichever one ran:
//!    known difference 8 in the module docs.)

use super::address::AddressCheck;
use super::config::{EmbeddingConfig, SeededReasons, Snapshot, Source, Which};

/// Back-off before each retry of a name that did not resolve, after the immediate first check.
pub const RETRY_DELAYS_S: [f64; 5] = [1.0, 2.0, 4.0, 8.0, 15.0];
/// How often a held-back name is checked again.
pub const RECHECK_S: f64 = 60.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLine {
    pub level: LogLevel,
    pub message: String,
}

/// The exception a `check` raised (its Python type name); the guard treats it like Python does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckFailure {
    pub type_name: String,
}

/// One refusal to make with [`EmbeddingConfig::refuse_seeded`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuardRefusal {
    pub which: Which,
    pub reason: String,
    pub transient: bool,
}

/// An address to check again after [`RECHECK_S`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    pub which: Which,
    pub url: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GuardOutcome {
    /// In the order Python makes them: refusals for good first, then each held-back name.
    pub refusals: Vec<GuardRefusal>,
    /// Held-back names, in the order Python schedules their re-checks.
    pub rechecks: Vec<Pending>,
    pub logs: Vec<LogLine>,
}

impl GuardOutcome {
    /// Makes every refusal on `config`, one call each like Python.
    pub fn apply(&self, config: &mut EmbeddingConfig) {
        for r in &self.refusals {
            config.refuse_seeded(&SeededReasons::only(r.which, r.reason.clone()), r.transient);
        }
    }
}

fn env_name(which: Which) -> &'static str {
    match which {
        Which::Active => "LLM_EMBED_BASE_URL",
        Which::Next => "LLM_EMBED_NEXT_BASE_URL",
    }
}

fn refusal_for_good(which: Which, url: &str, r: &AddressCheck) -> (GuardRefusal, LogLine) {
    let name = env_name(which);
    (
        GuardRefusal {
            which,
            reason: format!("{name} {url} refused: {}", r.reason),
            transient: false,
        },
        LogLine {
            level: LogLevel::Error,
            message: format!(
                "{name} {url} refused: {}. The embeddings server must be on this network.",
                r.reason
            ),
        },
    )
}

/// The first round of the start-up guard.
///
/// `check(url)` is `check_embedding_address` (an `Err` is an exception it raised: Python logs it
/// and decides nothing). `sleep(seconds)` is called before every retry. Does nothing unless the
/// state came from the environment.
pub fn guard_seeded_addresses(
    state: &Snapshot,
    check: &mut dyn FnMut(&str) -> Result<AddressCheck, CheckFailure>,
    sleep: &mut dyn FnMut(f64),
    retry_delays: &[f64],
    recheck_s: f64,
) -> GuardOutcome {
    let mut outcome = GuardOutcome::default();
    if state.source != Source::Environment {
        return outcome;
    }
    let urls: Vec<(Which, String)> = [
        (Which::Active, Some(&state.active)),
        (Which::Next, state.next.as_ref()),
    ]
    .into_iter()
    .filter_map(|(which, t)| {
        let url = t?.base_url.as_ref().filter(|u| !u.is_empty())?;
        Some((which, url.clone()))
    })
    .collect();
    let url_of = |which: Which| {
        urls.iter()
            .find(|(w, _)| *w == which)
            .map_or("", |(_, u)| u.as_str())
    };

    let mut pending: Vec<Which> = urls.iter().map(|(w, _)| *w).collect();
    let mut results: Vec<(Which, AddressCheck)> = Vec::new();
    let delays = std::iter::once(0.0).chain(retry_delays.iter().copied());
    for delay in delays {
        if delay != 0.0 {
            sleep(delay);
        }
        for which in pending.clone() {
            let r = match check(url_of(which)) {
                Ok(r) => r,
                Err(failure) => {
                    return GuardOutcome {
                        logs: vec![LogLine {
                            level: LogLevel::Error,
                            message: format!(
                                "could not check the seeded embeddings address ({})",
                                failure.type_name
                            ),
                        }],
                        ..GuardOutcome::default()
                    };
                }
            };
            let done = r.ok || !r.transient;
            match results.iter_mut().find(|(w, _)| *w == which) {
                Some(slot) => slot.1 = r,
                None => results.push((which, r)),
            }
            if done {
                pending.retain(|w| *w != which);
            }
        }
        if pending.is_empty() {
            break;
        }
    }

    for (which, r) in &results {
        if !r.ok && !pending.contains(which) {
            let (refusal, log) = refusal_for_good(*which, url_of(*which), r);
            outcome.refusals.push(refusal);
            outcome.logs.push(log);
        }
    }
    for which in pending {
        let reason = results
            .iter()
            .find(|(w, _)| *w == which)
            .map_or("", |(_, r)| r.reason.as_str());
        let (name, url) = (env_name(which), url_of(which));
        outcome.logs.push(LogLine {
            level: LogLevel::Warning,
            message: format!(
                "{name} {url} not used yet: {reason}. Checking again every {} s.",
                recheck_s as i64
            ),
        });
        outcome.refusals.push(GuardRefusal {
            which,
            reason: format!("{name} {url} not used yet: {reason}"),
            transient: true,
        });
        outcome.rechecks.push(Pending {
            which,
            url: url.to_owned(),
        });
    }
    outcome
}

/// What to do after a re-check step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecheckNext {
    /// Schedule the same step again after [`RECHECK_S`].
    Again,
    /// Nothing more to do (released, refused for good, or an administrator has chosen since).
    Stop,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecheckStep {
    pub next: RecheckNext,
    pub logs: Vec<LogLine>,
}

/// The lock-free half of a scheduled re-check: the (DNS) check itself. Call it with no lock on
/// the configuration held. Python skips the check once an administrator has chosen; a caller that
/// wants the same reads [`EmbeddingConfig::source`] under the lock first and checks only while it
/// is [`Source::Environment`].
pub fn recheck_check(
    pending: &Pending,
    check: &mut dyn FnMut(&str) -> Result<AddressCheck, CheckFailure>,
) -> Result<AddressCheck, CheckFailure> {
    check(&pending.url)
}

/// The locked half of a scheduled re-check (`again` in Python): applies `result` to `config`, which
/// the caller holds under its lock for this short, non-blocking call. The source is read again
/// here, so a choice made while the check was running wins: nothing is released or refused. A
/// name that now resolves to this network is released, a permanent refusal is made, and an `Err`
/// or a transient answer is tried again at the next round.
pub fn apply_recheck(
    config: &mut EmbeddingConfig,
    pending: &Pending,
    result: Result<AddressCheck, CheckFailure>,
) -> RecheckStep {
    let stop = |logs| RecheckStep {
        next: RecheckNext::Stop,
        logs,
    };
    if config.source() != Source::Environment {
        return stop(Vec::new()); // an administrator has chosen since: the seed no longer matters
    }
    let name = env_name(pending.which);
    if let Ok(r) = result {
        if r.ok {
            config.release_seeded(pending.which);
            return stop(vec![LogLine {
                level: LogLevel::Info,
                message: format!(
                    "{name} {} resolves to this network now; in use.",
                    pending.url
                ),
            }]);
        }
        if !r.transient {
            let (refusal, log) = refusal_for_good(pending.which, &pending.url, &r);
            config.refuse_seeded(&SeededReasons::only(refusal.which, refusal.reason), false);
            return stop(vec![log]);
        }
    }
    RecheckStep {
        next: RecheckNext::Again,
        logs: Vec::new(),
    }
}
