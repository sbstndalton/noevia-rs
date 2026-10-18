//! noevia#1318: the startup selector of `GET /api/v1/sections/{name}/autoconfig`
//! (`MODEL_MANAGER_SECTION_AUTOCONFIG_IMPL`, `python` by default).
//!
//! Like every switch, startup is refused for a value other than `python` or `rust`, and for
//! `rust` without the compiled `native-section-autoconfig` feature. Past that, settings this side
//! cannot read exactly as the Python child does keep the route with Python, with one log line:
//! a case-folded alias of a setting the child reads (its settings fold names), a `MODELS_DIR`
//! that is not absolute and in pathlib's normal form (the projector paths are mapped onto it as
//! text), a `MODELS_INI_PATH` that is not absolute, and cache-ram limits that are not plain
//! integers.
use std::ffi::OsString;

/// What the native route reads, when it is native.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Selection {
    pub(crate) settings: Option<Settings>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Settings {
    pub(crate) models: String,
    pub(crate) ini: String,
    /// `settings.cache_ram_limits[0]`.
    pub(crate) cache_cap: i64,
}

type Vars = [(OsString, OsString)];

#[cfg(test)]
const KEY: &str = "MODEL_MANAGER_SECTION_AUTOCONFIG_IMPL";

fn value(values: &Vars, key: &str) -> Result<Option<String>, &'static str> {
    match values.iter().rev().find(|(k, _)| k == key) {
        Some((_, v)) => v
            .to_str()
            .map(|v| Some(v.to_owned()))
            .ok_or("environment setting must be UTF-8"),
        None => Ok(None),
    }
}

/// A setting the child reads, refused when a case-folded alias is set too.
fn canonical(values: &Vars, name: &str) -> Result<Option<String>, &'static str> {
    if values.iter().any(|(k, _)| {
        k.to_str()
            .is_some_and(|k| k != name && k.eq_ignore_ascii_case(name))
    }) {
        return Err("a case-folded alias of a setting the child reads is set");
    }
    let found = value(values, name)?;
    if found.as_ref().is_some_and(|v| v.len() > 4096) {
        return Err("a models setting exceeded its limit");
    }
    Ok(found)
}

fn plain_int(values: &Vars, name: &str, default: i64) -> Result<i64, &'static str> {
    match canonical(values, name)? {
        None => Ok(default),
        Some(v) => {
            let digits = v.strip_prefix('-').unwrap_or(&v);
            if digits.is_empty() || digits.len() > 18 || !digits.bytes().all(|b| b.is_ascii_digit())
            {
                return Err("cache-ram limits are not plain integers");
            }
            v.parse()
                .map_err(|_| "cache-ram limits are not plain integers")
        }
    }
}

pub(crate) fn settings(values: &Vars) -> Result<Settings, &'static str> {
    let models = canonical(values, "MODELS_DIR")?.unwrap_or_else(|| "/models".to_owned());
    let normal = models.starts_with('/')
        && (models == "/"
            || (!models.ends_with('/')
                && models
                    .split('/')
                    .skip(1)
                    .all(|segment| !matches!(segment, "" | "." | ".."))));
    if !normal {
        return Err("MODELS_DIR is not absolute and normalised");
    }
    let ini =
        canonical(values, "MODELS_INI_PATH")?.unwrap_or_else(|| "/models/models.ini".to_owned());
    if !ini.starts_with('/') || ini.ends_with('/') {
        return Err("MODELS_INI_PATH is not an absolute file path");
    }
    let hard = plain_int(values, "LLAMACPP_CACHE_RAM_HARD_MAX_MIB", 2048)?.max(0);
    let cap = plain_int(values, "LLAMACPP_AUTOCONFIG_CACHE_RAM_MAX_MIB", 1024)?
        .max(0)
        .min(hard);
    Ok(Settings {
        models,
        ini,
        cache_cap: cap,
    })
}

/// The selector. `Err` refuses startup; a fallback is logged and keeps Python serving.
#[cfg(test)]
pub(crate) fn select(values: &Vars) -> Result<Selection, String> {
    match value(values, KEY)?.as_deref().unwrap_or("python") {
        "python" => Ok(Selection::default()),
        "rust" if cfg!(feature = "native-section-autoconfig") => match settings(values) {
            Ok(settings) => Ok(Selection {
                settings: Some(settings),
            }),
            Err(reason) => {
                eprintln!(
                    "native-section-autoconfig: {reason}; GET /api/v1/sections/{{name}}/autoconfig stays with python"
                );
                Ok(Selection::default())
            }
        },
        "rust" => Err(format!(
            "{KEY}=rust requires rust model manager and compiled native-section-autoconfig feature"
        )),
        _ => Err(format!("{KEY} must be python or rust")),
    }
}

/// The child refuses the route only when the native parent owns it (`1`; `0` otherwise, also
/// over an inherited value), serves the private call then, and reads the same models settings.
#[cfg(test)]
pub(crate) fn child(command: &mut tokio::process::Command, selection: &Selection) {
    command.env(
        "NOEVIA_NATIVE_MODEL_MANAGER_SECTION_AUTOCONFIG_CONFIRMED",
        if selection.settings.is_some() {
            "1"
        } else {
            "0"
        },
    );
    if let Some(settings) = &selection.settings {
        command
            .env("MODELS_DIR", &settings.models)
            .env("MODELS_INI_PATH", &settings.ini);
    }
}

/// The native route, when it is native.
#[cfg(all(feature = "native-section-autoconfig", feature = "native-routing"))]
pub(crate) fn state(
    selection: &Selection,
) -> Option<std::sync::Arc<noevia_model_manager::section_autoconfig::SectionAutoconfig>> {
    use noevia_model_manager::section_autoconfig::{SectionAutoconfig, Settings as Native};
    let settings = selection.settings.as_ref()?;
    Some(std::sync::Arc::new(SectionAutoconfig::new(Native {
        models: settings.models.clone(),
        ini: settings.ini.clone(),
        cache_ram_cap_mib: settings.cache_cap,
    })))
}

// Gated like the other slices' bin tests: the diagnosis mutation check pins the bin's test count.
#[cfg(all(test, feature = "native-section-autoconfig"))]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
        pairs
            .iter()
            .map(|(k, v)| (OsString::from(k), OsString::from(v)))
            .collect()
    }

    #[test]
    fn python_is_the_default_and_bad_values_refuse_startup() {
        assert_eq!(select(&vars(&[])).unwrap(), Selection::default());
        for bad in ["", "Rust", "native", " rust"] {
            let error = select(&vars(&[(KEY, bad)])).unwrap_err();
            assert_eq!(error, format!("{KEY} must be python or rust"));
        }
        let rust = select(&vars(&[(KEY, "rust")])).unwrap();
        assert_eq!(
            rust.settings,
            Some(Settings {
                models: "/models".into(),
                ini: "/models/models.ini".into(),
                cache_cap: 1024,
            })
        );
    }

    #[test]
    fn settings_it_cannot_read_exactly_keep_python() {
        let with = |extra: &[(&str, &str)]| {
            let mut all = vec![(KEY, "rust")];
            all.extend_from_slice(extra);
            select(&vars(&all)).unwrap()
        };
        for fold in [
            ("models_dir", "/x"),
            ("Models_Ini_Path", "/x.ini"),
            ("llamacpp_cache_ram_hard_max_mib", "1"),
            ("Llamacpp_Autoconfig_Cache_Ram_Max_Mib", "1"),
        ] {
            assert_eq!(with(&[fold]), Selection::default(), "{fold:?}");
        }
        for models in ["relative", "/a/", "/a//b", "/a/./b", "/a/../b", ""] {
            assert_eq!(
                with(&[("MODELS_DIR", models)]),
                Selection::default(),
                "{models}"
            );
        }
        for ini in ["relative.ini", "/dir/", ""] {
            assert_eq!(
                with(&[("MODELS_INI_PATH", ini)]),
                Selection::default(),
                "{ini}"
            );
        }
        for cap in ["1.5", " 1", "x", "", "1_0"] {
            let both = with(&[("LLAMACPP_AUTOCONFIG_CACHE_RAM_MAX_MIB", cap)]);
            assert_eq!(both, Selection::default(), "{cap}");
        }
        let held = with(&[
            ("MODELS_DIR", "/srv/models"),
            ("MODELS_INI_PATH", "/srv/presets.ini"),
            ("LLAMACPP_CACHE_RAM_HARD_MAX_MIB", "512"),
            ("LLAMACPP_AUTOCONFIG_CACHE_RAM_MAX_MIB", "4096"),
        ]);
        assert_eq!(
            held.settings,
            Some(Settings {
                models: "/srv/models".into(),
                ini: "/srv/presets.ini".into(),
                cache_cap: 512,
            })
        );
        let negative = with(&[("LLAMACPP_AUTOCONFIG_CACHE_RAM_MAX_MIB", "-5")]);
        assert_eq!(negative.settings.unwrap().cache_cap, 0);
    }

    #[test]
    fn confirmation_overrides_an_inherited_value() {
        let settings = Settings {
            models: "/srv/models".into(),
            ini: "/srv/models.ini".into(),
            cache_cap: 1024,
        };
        for on in [false, true] {
            let mut command = tokio::process::Command::new("uvicorn");
            command.env(
                "NOEVIA_NATIVE_MODEL_MANAGER_SECTION_AUTOCONFIG_CONFIRMED",
                "hostile",
            );
            let selection = Selection {
                settings: on.then(|| settings.clone()),
            };
            child(&mut command, &selection);
            let envs: std::collections::BTreeMap<_, _> = command.as_std().get_envs().collect();
            let get = |k: &str| {
                envs.get(std::ffi::OsStr::new(k))
                    .copied()
                    .flatten()
                    .map(|v| v.to_str().unwrap().to_owned())
            };
            assert_eq!(
                get("NOEVIA_NATIVE_MODEL_MANAGER_SECTION_AUTOCONFIG_CONFIRMED"),
                Some(if on { "1" } else { "0" }.to_owned())
            );
            assert_eq!(get("MODELS_DIR"), on.then(|| "/srv/models".to_owned()));
            assert_eq!(
                get("MODELS_INI_PATH"),
                on.then(|| "/srv/models.ini".to_owned())
            );
        }
    }
}
