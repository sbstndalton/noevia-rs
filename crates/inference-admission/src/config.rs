//! The scheduler's limits. Every one is bounded; the gate reads them from its environment.

use std::collections::BTreeSet;

/// Limits of the admission scheduler. All durations are milliseconds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// Models that never take the chat slot (embedding and reranking models served beside the chat
    /// model). Rows whose router arguments carry `--embedding`/`--rerank` are exempt as well.
    pub exempt: BTreeSet<String>,
    /// Most requests and lease requests waiting at once; more are refused (`inference_queue_full`).
    pub max_waiters: usize,
    /// Most live leases at once; more are refused (`inference_queue_full`).
    pub max_leases: usize,
    /// Longest a waiter waits for the slot before it is refused (`inference_queue_timeout`).
    pub max_wait_ms: u64,
    /// After a group for another model has waited this long, new work for the resident model no
    /// longer rides along with its active work: it queues behind that group.
    pub fair_hold_ms: u64,
    /// After a request without a lease ends, its model stays at least this long, so a client that
    /// makes several calls without a lease is not switched away between them.
    pub residency_grace_ms: u64,
    /// A lease with no request in flight ends after this long without one.
    pub lease_idle_ms: u64,
    /// A lease ends this long after it was granted, whatever it does.
    pub lease_hard_ms: u64,
    /// An unload the router has not finished after this long is reported as stuck; no other model
    /// is let in until the listing shows it gone.
    pub unload_timeout_ms: u64,
    /// Waiters are refused once the router has been unreachable this long.
    pub router_down_grace_ms: u64,
}

/// Why a configuration was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigError(pub String);

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ConfigError {}

/// The upper bound of every duration (one day): larger values are configuration mistakes.
pub const MAX_DURATION_MS: u64 = 24 * 60 * 60 * 1000;
/// The upper bound of the waiter and lease counts.
pub const MAX_COUNT: usize = 10_000;
/// Longest model id the gate accepts (bytes).
pub const MAX_MODEL_ID_BYTES: usize = 256;

impl Default for Config {
    fn default() -> Self {
        Self {
            exempt: BTreeSet::new(),
            max_waiters: 64,
            max_leases: 64,
            max_wait_ms: 600_000,
            fair_hold_ms: 60_000,
            residency_grace_ms: 20_000,
            lease_idle_ms: 30_000,
            lease_hard_ms: 1_800_000,
            unload_timeout_ms: 45_000,
            router_down_grace_ms: 30_000,
        }
    }
}

impl Config {
    /// Checks that every limit is positive and bounded.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let durations = [
            ("max wait", self.max_wait_ms),
            ("fair hold", self.fair_hold_ms),
            ("lease idle", self.lease_idle_ms),
            ("lease hard limit", self.lease_hard_ms),
            ("unload timeout", self.unload_timeout_ms),
            ("router down grace", self.router_down_grace_ms),
        ];
        for (name, value) in durations {
            if value == 0 || value > MAX_DURATION_MS {
                return Err(ConfigError(format!(
                    "{name} must be between 1 ms and one day"
                )));
            }
        }
        if self.residency_grace_ms > MAX_DURATION_MS {
            return Err(ConfigError(
                "residency grace must be at most one day".into(),
            ));
        }
        if self.lease_idle_ms > self.lease_hard_ms {
            return Err(ConfigError(
                "lease idle limit must not exceed the lease hard limit".into(),
            ));
        }
        for (name, value) in [
            ("max waiters", self.max_waiters),
            ("max leases", self.max_leases),
        ] {
            if value == 0 || value > MAX_COUNT {
                return Err(ConfigError(format!(
                    "{name} must be between 1 and {MAX_COUNT}"
                )));
            }
        }
        for model in &self.exempt {
            if !valid_model_id(model) {
                return Err(ConfigError("an exempt model id is not valid".into()));
            }
        }
        Ok(())
    }
}

/// A model id the gate passes on: 1 to 256 bytes, no control characters.
pub fn valid_model_id(model: &str) -> bool {
    !model.is_empty() && model.len() <= MAX_MODEL_ID_BYTES && !model.chars().any(char::is_control)
}
