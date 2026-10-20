//! noevia#1318: the settings of `GET /api/v1/models` and `GET /api/v1/overview`, both owned natively
//! (noevia#1071: the `MODEL_MANAGER_MODELS_IMPL` and `MODEL_MANAGER_OVERVIEW_IMPL` selectors are
//! retired and ignored).
//!
//! Startup is refused for settings this side cannot read exactly as the Python reference did: a
//! case-folded alias of a setting it reads (its settings fold names), a `MODELS_DIR` that is not
//! absolute and in pathlib's normal form, an empty `MODELS_INI_PATH` or `DATA_DIR`, and a value
//! over its limit.
use std::ffi::OsString;

/// The settings one native route reads, named as the child reads them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Roots {
    pub(crate) models: String,
    pub(crate) ini: String,
    /// `DATA_DIR` (the list's badge store).
    pub(crate) data: String,
    /// `MODELS_HOST_PATH` (the overview's `hostPath`).
    pub(crate) host: String,
}

/// Which of the two routes are native (`Some`), with what they read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Selection {
    pub(crate) models: Option<Roots>,
    pub(crate) overview: Option<Roots>,
}

type Vars = [(OsString, OsString)];

fn value(values: &Vars, key: &str, default: &str) -> Result<String, &'static str> {
    match values.iter().rev().find(|(k, _)| k == key) {
        Some((_, v)) => v
            .to_str()
            .map(str::to_owned)
            .ok_or("environment setting must be UTF-8"),
        None => Ok(default.to_owned()),
    }
}

/// A setting the child reads, refused when a case-folded alias is set too.
fn canonical(values: &Vars, name: &str, default: &str) -> Result<String, &'static str> {
    if values.iter().any(|(k, _)| {
        k.to_str()
            .is_some_and(|k| k != name && k.eq_ignore_ascii_case(name))
    }) {
        return Err("a case-folded alias of MODELS_DIR, MODELS_INI_PATH, DATA_DIR or MODELS_HOST_PATH is set");
    }
    let found = value(values, name, default)?;
    if found.len() > 4096 {
        return Err("a models setting exceeded its limit");
    }
    Ok(found)
}

fn roots(values: &Vars) -> Result<Roots, &'static str> {
    let models = canonical(values, "MODELS_DIR", "/models")?;
    // Answered as configured (`str(Path(MODELS_DIR))`), so it must already be pathlib's form.
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
    let ini = canonical(values, "MODELS_INI_PATH", "/models/models.ini")?;
    if ini.is_empty() {
        return Err("MODELS_INI_PATH is empty");
    }
    let data = canonical(values, "DATA_DIR", "/data")?;
    if data.is_empty() {
        return Err("DATA_DIR is empty");
    }
    Ok(Roots {
        models,
        ini,
        data,
        host: canonical(values, "MODELS_HOST_PATH", "")?,
    })
}

/// Mandatory production configuration; implementation selectors have no meaning here.
pub(crate) fn native(values: &Vars) -> Result<Selection, String> {
    let roots = roots(values)?;
    Ok(Selection {
        models: Some(roots.clone()),
        overview: Some(roots),
    })
}

/// The native routes' state, when either is native.
pub(crate) fn state(
    selection: &Selection,
) -> Option<std::sync::Arc<noevia_model_manager::model_list::ModelList>> {
    use noevia_model_manager::model_list::{statvfs_usage, ModelList, Settings};
    let roots = selection.models.as_ref().or(selection.overview.as_ref())?;
    Some(std::sync::Arc::new(ModelList::new(
        Settings {
            models: roots.models.clone().into(),
            ini: roots.ini.clone().into(),
            data: selection.models.as_ref().map(|r| r.data.clone().into()),
            host_path: selection.overview.as_ref().map(|r| r.host.clone()),
        },
        statvfs_usage(),
    )))
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
    fn both_routes_read_the_same_defaults_or_explicit_roots() {
        let default = native(&vars(&[])).unwrap();
        let roots = Roots {
            models: "/models".into(),
            ini: "/models/models.ini".into(),
            data: "/data".into(),
            host: String::new(),
        };
        assert_eq!(default.models.as_ref(), Some(&roots));
        assert_eq!(default.overview.as_ref(), Some(&roots));
        // Retired implementation selectors change nothing.
        let ignored = native(&vars(&[
            ("MODEL_MANAGER_MODELS_IMPL", "python"),
            ("MODEL_MANAGER_OVERVIEW_IMPL", "invalid"),
        ]))
        .unwrap();
        assert_eq!(ignored, default);
        let explicit = native(&vars(&[
            ("MODELS_DIR", "/srv/models"),
            ("MODELS_INI_PATH", "/srv/models/presets.ini"),
            ("DATA_DIR", "/srv/data"),
            ("MODELS_HOST_PATH", "/mnt/synthetic"),
        ]))
        .unwrap();
        let roots = Roots {
            models: "/srv/models".into(),
            ini: "/srv/models/presets.ini".into(),
            data: "/srv/data".into(),
            host: "/mnt/synthetic".into(),
        };
        assert_eq!(explicit.models.as_ref(), Some(&roots));
        assert_eq!(explicit.overview.as_ref(), Some(&roots));
    }

    #[test]
    fn settings_it_cannot_read_exactly_refuse_startup() {
        for fold in [
            ("models_dir", "/x"),
            ("Models_Ini_Path", "/x.ini"),
            ("models_host_path", "/x"),
            ("data_dir", "/x"),
        ] {
            assert!(native(&vars(&[fold])).is_err(), "{fold:?}");
        }
        for models in ["relative", "/a/", "/a//b", "/a/./b", "/a/../b", ""] {
            assert!(
                native(&vars(&[("MODELS_DIR", models)])).is_err(),
                "{models}"
            );
        }
        assert!(native(&vars(&[("MODELS_DIR", "/")])).is_ok());
        assert!(native(&vars(&[("MODELS_INI_PATH", "")])).is_err());
        assert!(native(&vars(&[("DATA_DIR", "")])).is_err());
        let long = "/".to_owned() + &"x".repeat(4096);
        assert!(native(&vars(&[("MODELS_DIR", &long)])).is_err());
    }
}
