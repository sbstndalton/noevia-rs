//! noevia#1318: the settings of `GET /api/v1/sections/{name}/autoconfig`, owned natively
//! (noevia#1071: the `MODEL_MANAGER_SECTION_AUTOCONFIG_IMPL` selector is retired and ignored).
//!
//! Startup is refused for settings this side cannot read exactly as the Python reference did: a
//! case-folded alias of a setting it reads (its settings fold names), a `MODELS_DIR` that is not
//! absolute and in pathlib's normal form (the projector paths are mapped onto it as text), a
//! `MODELS_INI_PATH` that is not an absolute file path, and cache-ram limits that are not plain
//! integers.
use std::ffi::OsString;

/// What the native route reads, when it is native.
#[derive(Debug, Clone, PartialEq, Eq)]
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

/// The native route, when it is native.
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

#[cfg(test)]
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
    fn defaults_and_the_retired_selector_is_ignored() {
        let expected = Settings {
            models: "/models".into(),
            ini: "/models/models.ini".into(),
            cache_cap: 1024,
        };
        assert_eq!(settings(&vars(&[])).unwrap(), expected);
        for value in ["python", "rust", "invalid", ""] {
            let with = settings(&vars(&[("MODEL_MANAGER_SECTION_AUTOCONFIG_IMPL", value)]));
            assert_eq!(with.unwrap(), expected, "{value:?}");
        }
    }

    #[test]
    fn settings_it_cannot_read_exactly_are_refused() {
        let with = |extra: &[(&str, &str)]| settings(&vars(extra));
        for fold in [
            ("models_dir", "/x"),
            ("Models_Ini_Path", "/x.ini"),
            ("llamacpp_cache_ram_hard_max_mib", "1"),
            ("Llamacpp_Autoconfig_Cache_Ram_Max_Mib", "1"),
        ] {
            assert!(with(&[fold]).is_err(), "{fold:?}");
        }
        for models in ["relative", "/a/", "/a//b", "/a/./b", "/a/../b", ""] {
            assert!(with(&[("MODELS_DIR", models)]).is_err(), "{models}");
        }
        for ini in ["relative.ini", "/dir/", ""] {
            assert!(with(&[("MODELS_INI_PATH", ini)]).is_err(), "{ini}");
        }
        for cap in ["1.5", " 1", "x", "", "1_0"] {
            assert!(
                with(&[("LLAMACPP_AUTOCONFIG_CACHE_RAM_MAX_MIB", cap)]).is_err(),
                "{cap}"
            );
        }
        let held = with(&[
            ("MODELS_DIR", "/srv/models"),
            ("MODELS_INI_PATH", "/srv/presets.ini"),
            ("LLAMACPP_CACHE_RAM_HARD_MAX_MIB", "512"),
            ("LLAMACPP_AUTOCONFIG_CACHE_RAM_MAX_MIB", "4096"),
        ])
        .unwrap();
        assert_eq!(
            held,
            Settings {
                models: "/srv/models".into(),
                ini: "/srv/presets.ini".into(),
                cache_cap: 512,
            }
        );
        let negative = with(&[("LLAMACPP_AUTOCONFIG_CACHE_RAM_MAX_MIB", "-5")]).unwrap();
        assert_eq!(negative.cache_cap, 0);
    }
}
