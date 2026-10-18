//! The database bootstrap is default off and needs the existing secure native prompt store.
use std::ffi::OsString;
pub(crate) fn select(values: &[(OsString, OsString)]) -> Result<bool, &'static str> {
    let get = |name: &str| -> Result<&str, &'static str> {
        values
            .iter()
            .rev()
            .find(|(key, _)| key == name)
            .map(|(_, value)| {
                value
                    .to_str()
                    .ok_or("database initializer setting must be UTF-8")
            })
            .unwrap_or(Ok("python"))
    };
    match get("MODEL_MANAGER_DB_INIT_IMPL")? {
        "python" => Ok(false),
        "rust" => {
            if !cfg!(feature = "native-db-init") {
                return Err("native DB initialization requires compiled native-db-init feature");
            }
            if get("MODEL_MANAGER_PROMPTS_IMPL")? != "rust" {
                return Err("native DB initialization requires MODEL_MANAGER_PROMPTS_IMPL=rust");
            }
            Ok(true)
        }
        _ => Err("MODEL_MANAGER_DB_INIT_IMPL must be python or rust"),
    }
}
pub(crate) fn child(command: &mut tokio::process::Command, selected: bool) {
    command.env(
        "NOEVIA_NATIVE_MODEL_MANAGER_DB_INIT_CONFIRMED",
        if selected { "1" } else { "0" },
    );
}
#[cfg(test)]
mod tests {
    use super::*;
    fn values(mode: &str, prompts: &str) -> Vec<(OsString, OsString)> {
        vec![
            ("MODEL_MANAGER_DB_INIT_IMPL".into(), mode.into()),
            ("MODEL_MANAGER_PROMPTS_IMPL".into(), prompts.into()),
        ]
    }
    #[test]
    fn the_selector_is_default_off_requires_the_compiled_feature_and_native_prompts() {
        assert_eq!(select(&[]), Ok(false));
        for mode in ["", "Rust", "on", " rust", "true"] {
            assert!(select(&values(mode, "rust")).is_err());
        }
        assert!(select(&values("rust", "python")).is_err());
        assert_eq!(
            select(&values("rust", "rust")).is_ok(),
            cfg!(feature = "native-db-init")
        );
    }
    #[test]
    fn the_retired_parent_selector_cannot_affect_database_ownership() {
        let without = values("rust", "rust");
        let expected = select(&without);
        for retired in ["python", "rust", "", "arbitrary-retired-value"] {
            let mut input = without.clone();
            input.push(("MODEL_MANAGER_IMPL".into(), retired.into()));
            assert_eq!(select(&input), expected);
        }
    }
    #[test]
    fn child_confirmation_is_overwritten_for_both_owners() {
        for selected in [false, true] {
            let mut command = tokio::process::Command::new("qa-child");
            command.env("NOEVIA_NATIVE_MODEL_MANAGER_DB_INIT_CONFIRMED", "hostile");
            child(&mut command, selected);
            let value = command
                .as_std()
                .get_envs()
                .find(|(key, _)| *key == "NOEVIA_NATIVE_MODEL_MANAGER_DB_INIT_CONFIRMED")
                .and_then(|(_, value)| value);
            assert_eq!(
                value,
                Some(std::ffi::OsStr::new(if selected { "1" } else { "0" }))
            );
        }
    }
}
