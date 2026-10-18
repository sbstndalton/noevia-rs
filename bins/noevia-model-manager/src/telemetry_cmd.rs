//! noevia#1318 step 3d: the startup selector of the section autoconfig's telemetry
//! (`MODEL_MANAGER_TELEMETRY_IMPL`, `python` by default).
//!
//! `python`: the child ingests the backend logs and owns the telemetry tables, as before. `rust`:
//! this process ingests and reads them, the route no longer asks the child, and the child is told
//! (`NOEVIA_NATIVE_MODEL_MANAGER_TELEMETRY_CONFIRMED=1`) so that it never writes them. One writer
//! per selector value. `rust` needs the compiled `native-telemetry` feature, the native section
//! autoconfig and the native backend sampler; without them startup is refused, never silently
//! downgraded (two writers would be the silent failure).
use std::ffi::OsString;

const KEY: &str = "MODEL_MANAGER_TELEMETRY_IMPL";
pub(crate) const CHILD_KEY: &str = "NOEVIA_NATIVE_MODEL_MANAGER_TELEMETRY_CONFIRMED";

/// `true`: native. `ready`: the native section autoconfig and the sampler are both on.
pub(crate) fn select(values: &[(OsString, OsString)], ready: bool) -> Result<bool, String> {
    let value = match values.iter().rev().find(|(k, _)| k == KEY) {
        Some((_, v)) => v
            .to_str()
            .ok_or_else(|| format!("{KEY} must be UTF-8"))?
            .to_owned(),
        None => "python".to_owned(),
    };
    match value.as_str() {
        "python" => Ok(false),
        "rust" if !cfg!(feature = "native-telemetry") => Err(format!(
            "{KEY}=rust requires the compiled native-telemetry feature"
        )),
        "rust" if !ready => Err(format!(
            "{KEY}=rust requires MODEL_MANAGER_SECTION_AUTOCONFIG_IMPL=rust and MODEL_MANAGER_BACKENDS_IMPL=rust"
        )),
        "rust" => Ok(true),
        _ => Err(format!("{KEY} must be python or rust")),
    }
}

/// The child is told who owns the telemetry tables (`1` native, `0` otherwise, also over an
/// inherited value).
pub(crate) fn child(command: &mut tokio::process::Command, native: bool) {
    command.env(CHILD_KEY, if native { "1" } else { "0" });
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).into(), (*v).into()))
            .collect()
    }

    #[test]
    fn python_is_the_default_and_rust_needs_its_parts() {
        assert!(!select(&vars(&[]), true).unwrap());
        assert!(!select(&vars(&[(KEY, "python")]), false).unwrap());
        assert!(select(&vars(&[(KEY, "rust")]), false).is_err());
        assert!(select(&vars(&[(KEY, "Rust")]), true).is_err());
        assert!(select(&vars(&[(KEY, "")]), true).is_err());
        assert_eq!(
            select(&vars(&[(KEY, "rust")]), true).is_ok(),
            cfg!(feature = "native-telemetry")
        );
        if cfg!(feature = "native-telemetry") {
            assert!(select(&vars(&[(KEY, "rust")]), true).unwrap());
        }
    }
}
