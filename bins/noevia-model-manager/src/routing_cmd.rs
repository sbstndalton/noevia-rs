//! Explicit ownership of public framework routing; compatibility remains default.
use std::ffi::OsString;
pub const REQUIRED: &[&str] = &[
    "MODEL_MANAGER_DB_INIT_IMPL",
    "MODEL_MANAGER_TOKEN_TEST_IMPL",
    "MODEL_MANAGER_SAFE_DEFAULTS_IMPL",
    "MODEL_MANAGER_BENCHMARK_OBSERVE_IMPL",
    "MODEL_MANAGER_DOWNLOAD_PLAN_IMPL",
    "MODEL_MANAGER_TELEMETRY_IMPL",
    "MODEL_MANAGER_HOST_IMPL",
    "MODEL_MANAGER_SECTIONS_IMPL",
    "MODEL_MANAGER_PROMPTS_IMPL",
    "MODEL_MANAGER_DIAGNOSE_IMPL",
    "MODEL_MANAGER_SETTINGS_IMPL",
    "MODEL_MANAGER_MODEL_UPDATES_IMPL",
    "MODEL_MANAGER_MODEL_DETAIL_IMPL",
    "MODEL_MANAGER_DOWNLOAD_TARGETS_IMPL",
    "MODEL_MANAGER_BADGES_IMPL",
    "MODEL_MANAGER_BACKEND_CONTROL_IMPL",
    "MODEL_MANAGER_SETTINGS_WRITE_IMPL",
    "MODEL_MANAGER_SECTION_WRITES_IMPL",
    "MODEL_MANAGER_CHECK_UPDATES_IMPL",
    "MODEL_MANAGER_BENCHMARK_OVERVIEW_IMPL",
    "MODEL_MANAGER_BENCHMARK_RUNNER_IMPL",
    "MODEL_MANAGER_DOWNLOADS_IMPL",
    "MODEL_MANAGER_MODEL_DELETE_IMPL",
    "MODEL_MANAGER_DRAFT_HEADS_IMPL",
    "MODEL_MANAGER_BACKENDS_IMPL",
    "MODEL_MANAGER_SEARCH_IMPL",
    "MODEL_MANAGER_BENCH_RUNS_IMPL",
    "MODEL_MANAGER_MODELS_IMPL",
    "MODEL_MANAGER_OVERVIEW_IMPL",
    "MODEL_MANAGER_SECTION_AUTOCONFIG_IMPL",
];
pub fn select(values: &[(OsString, OsString)]) -> Result<bool, &'static str> {
    let get = |key: &str| -> Result<&str, &'static str> {
        values
            .iter()
            .rev()
            .find(|(k, _)| k == key)
            .map_or(Ok("python"), |(_, v)| {
                v.to_str().ok_or("routing selector must be UTF-8")
            })
    };
    match get("MODEL_MANAGER_ROUTING_IMPL")? {
        "python" => Ok(false),
        "rust" if !cfg!(feature = "native-routing") => {
            Err("native routing requires compiled native-routing")
        }
        "rust" => {
            for key in REQUIRED {
                if get(key)? != "rust" {
                    return Err("native routing requires every public owner and private helper selected natively");
                }
            }
            Ok(true)
        }
        _ => Err("MODEL_MANAGER_ROUTING_IMPL must be python or rust"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn selection_is_explicit_and_every_private_helper_is_required() {
        assert_eq!(select(&[]), Ok(false));
        let mut values = vec![("MODEL_MANAGER_ROUTING_IMPL".into(), "rust".into())];
        for key in REQUIRED {
            assert!(select(&values).is_err());
            values.push(((*key).into(), "rust".into()));
        }
        assert_eq!(select(&values).is_ok(), cfg!(feature = "native-routing"));
        values.push(("MODEL_MANAGER_ROUTING_IMPL".into(), "invalid".into()));
        assert!(select(&values).is_err());
    }
}
