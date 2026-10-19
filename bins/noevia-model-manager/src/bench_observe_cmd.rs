//! Startup ownership for benchmark observations; Python compatibility is the default.
use std::ffi::OsString;
pub fn select(values: &[(OsString, OsString)], ready: bool) -> Result<bool, String> {
    let raw = values
        .iter()
        .rev()
        .find(|(k, _)| k == "MODEL_MANAGER_BENCHMARK_OBSERVE_IMPL");
    let value = match raw {
        Some((_, v)) => v
            .to_str()
            .ok_or("benchmark observer selector must be UTF-8")?,
        None => "python",
    };
    match value {
        "python" => Ok(false),
        "rust" if !cfg!(feature = "native-bench-observe") => {
            Err("benchmark observation requires compiled native-bench-observe".into())
        }
        "rust" if !ready => {
            Err("benchmark observation requires native benchmark runner and backend sampler".into())
        }
        "rust" => Ok(true),
        _ => Err("MODEL_MANAGER_BENCHMARK_OBSERVE_IMPL must be python or rust".into()),
    }
}
#[cfg(test)]
mod tests {
    #[test]
    fn ownership_requires_both_runtime_and_compiled_capability() {
        let vars = |v: &str| vec![("MODEL_MANAGER_BENCHMARK_OBSERVE_IMPL".into(), v.into())];
        assert_eq!(super::select(&[], false), Ok(false));
        assert_eq!(super::select(&vars("python"), false), Ok(false));
        assert!(super::select(&vars("rust"), false).is_err());
        assert_eq!(
            super::select(&vars("rust"), true).is_ok(),
            cfg!(feature = "native-bench-observe")
        );
        assert!(super::select(&vars(""), true).is_err());
        assert!(super::select(&vars("Rust"), true).is_err());
    }
}
