//! The supervisor's view of its environment: a snapshot it edits (scrubs) and hands to every
//! process it starts, so an inherited value never reaches a child the script would have unset.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};

/// Values only the supervisor may set; an inherited copy is dropped before anything runs.
/// (Same list as build/web-supervisor.sh's `unset` line.)
pub const SCRUBBED: &[&str] = &[
    "NOEVIA_RUST_AUTH_CONFIRMED",
    "NOEVIA_RUST_PROJECTS_CONFIRMED",
    "NOEVIA_RUST_PROJECT_CHATS_CONFIRMED",
    "NOEVIA_RUST_FREE_CHATS_CONFIRMED",
    "NOEVIA_RUST_USAGE_CONFIRMED",
    "NOEVIA_RUST_SAMPLING_SETTINGS_CONFIRMED",
    "NOEVIA_RUST_REASONING_SETTINGS_CONFIRMED",
    "NOEVIA_RUST_ROUTING_MODE_CONFIRMED",
    "NOEVIA_RUST_ROUTING_DEFAULT_CONFIRMED",
    "NOEVIA_RUST_STATS_CONFIRMED",
    "NOEVIA_RUST_EXPORT_CONFIRMED",
    "NOEVIA_RUST_STORAGE_READS_CONFIRMED",
    "NOEVIA_RUST_STORAGE_BROWSE_CONFIRMED",
    "NOEVIA_RUST_CHATGPT_STATUS_CONFIRMED",
    "NOEVIA_RUST_PROJECT_WRITES_CONFIRMED",
    "NOEVIA_RUST_PROJECT_STORE_CONFIRMED",
    "NOEVIA_PROJECT_STORE_PORT",
    "NOEVIA_PROJECT_STORE_TOKEN",
    "NOEVIA_FRONT_CLIENT_IP",
    "NOEVIA_LEGACY_CLAIM_BIN",
    "NOEVIA_START_ID",
];

/// Retired deployment switches (sbstndalton/noevia#1071): a leftover only warns and reaches no process.
pub const RETIRED: &[&str] = &[
    "NOEVIA_RUST_AUTH",
    "NOEVIA_RUST_USAGE",
    "NOEVIA_RUST_SAMPLING_SETTINGS",
    "NOEVIA_RUST_REASONING_SETTINGS",
    "NOEVIA_RUST_ROUTING_MODE",
    "NOEVIA_RUST_ROUTING_DEFAULT",
    "NOEVIA_RUST_STATS",
    "NOEVIA_RUST_EXPORT",
    "NOEVIA_RUST_CHATGPT_STATUS",
    "NOEVIA_RUST_STORAGE_READS",
    "NOEVIA_RUST_STORAGE_BROWSE",
    "NOEVIA_RUST_PROJECTS",
    "NOEVIA_RUST_PROJECT_WRITES",
];

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Env {
    vars: BTreeMap<OsString, OsString>,
}

impl Env {
    pub fn from_process() -> Self {
        Self {
            vars: std::env::vars_os().collect(),
        }
    }

    pub fn from_pairs<K: Into<OsString>, V: Into<OsString>>(
        pairs: impl IntoIterator<Item = (K, V)>,
    ) -> Self {
        Self {
            vars: pairs
                .into_iter()
                .map(|(k, v)| (k.into(), v.into()))
                .collect(),
        }
    }

    /// `${NAME+set}`: present, even when empty.
    pub fn is_set(&self, name: &str) -> bool {
        self.vars.contains_key(OsStr::new(name))
    }

    /// `${NAME:-}`: the value, or empty when unset. Values that are not UTF-8 read as empty.
    pub fn get(&self, name: &str) -> &str {
        self.vars
            .get(OsStr::new(name))
            .and_then(|v| v.to_str())
            .unwrap_or("")
    }

    /// `${NAME:-default}`: unset or empty both give the default.
    pub fn get_or<'a>(&'a self, name: &str, default: &'a str) -> &'a str {
        match self.get(name) {
            "" => default,
            v => v,
        }
    }

    pub fn remove(&mut self, name: &str) {
        self.vars.remove(OsStr::new(name));
    }

    pub fn vars(&self) -> impl Iterator<Item = (&OsString, &OsString)> {
        self.vars.iter()
    }

    /// The script's prologue: drop the supervisor-only values, warn about and drop each retired
    /// switch. Returns the warnings in the script's order.
    pub fn scrub(&mut self) -> Vec<String> {
        for name in SCRUBBED {
            self.remove(name);
        }
        let mut warnings = Vec::new();
        for name in RETIRED {
            if self.is_set(name) {
                warnings.push(format!(
                    "web-supervisor: {name} is retired (sbstndalton/noevia#1071) and ignored; remove it from the environment"
                ));
                self.remove(name);
            }
        }
        warnings
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn empty_and_unset_read_the_same_for_defaults_but_not_for_is_set() {
        let env = Env::from_pairs([("A", ""), ("B", "x")]);
        assert!(env.is_set("A"));
        assert!(!env.is_set("C"));
        assert_eq!(env.get_or("A", "d"), "d");
        assert_eq!(env.get_or("C", "d"), "d");
        assert_eq!(env.get_or("B", "d"), "x");
    }

    #[test]
    fn scrub_drops_confirmations_and_warns_for_each_retired_switch_even_empty() {
        let mut env = Env::from_pairs([
            ("NOEVIA_RUST_AUTH_CONFIRMED", "1"),
            ("NOEVIA_START_ID", "spoof"),
            ("NOEVIA_RUST_AUTH", ""),
            ("NOEVIA_RUST_PROJECTS", "1"),
            ("KEEP", "1"),
        ]);
        let warnings = env.scrub();
        assert_eq!(warnings.len(), 2);
        assert!(warnings[0].contains("NOEVIA_RUST_AUTH is retired"));
        assert!(warnings[1].contains("NOEVIA_RUST_PROJECTS is retired"));
        assert!(!env.is_set("NOEVIA_RUST_AUTH_CONFIRMED"));
        assert!(!env.is_set("NOEVIA_START_ID"));
        assert!(!env.is_set("NOEVIA_RUST_AUTH"));
        assert!(env.is_set("KEEP"));
    }
}
