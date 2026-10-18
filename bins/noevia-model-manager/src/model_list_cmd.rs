//! noevia#1318: the startup selectors of `GET /api/v1/models` (`MODEL_MANAGER_MODELS_IMPL`) and
//! `GET /api/v1/overview` (`MODEL_MANAGER_OVERVIEW_IMPL`), both `python` by default.
//!
//! Like every switch, startup is refused for a value other than `python` or `rust`, and for
//! `rust` without the compiled `native-model-list` feature. Past that, settings this side cannot
//! read exactly as the Python child does keep that route with Python, with one log line, instead
//! of refusing: a case-folded alias of a setting the child reads (its settings fold names), a
//! `MODELS_DIR` that is not absolute and in pathlib's normal form, and for the list a `TZ` other
//! than unset or `UTC` (`modified` is formatted in the child's zone; the child is given UTC).
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
#[derive(Debug, Clone, PartialEq, Eq, Default)]
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

#[cfg(test)]
fn mode(values: &Vars, key: &'static str) -> Result<bool, String> {
    match value(values, key, "python")?.as_str() {
        "python" => Ok(false),
        "rust" if cfg!(feature = "native-model-list") => Ok(true),
        "rust" => Err(format!(
            "{key}=rust requires rust model manager and compiled native-model-list feature"
        )),
        _ => Err(format!("{key} must be python or rust")),
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
#[cfg(feature = "native-routing")]
pub(crate) fn native(values: &Vars) -> Result<Selection, String> {
    let roots = roots(values)?;
    Ok(Selection {
        models: Some(roots.clone()),
        overview: Some(roots),
    })
}

/// The two selectors. `Err` refuses startup; a fallback is logged and keeps Python serving.
#[cfg(test)]
pub(crate) fn select(values: &Vars) -> Result<Selection, String> {
    let models = mode(values, "MODEL_MANAGER_MODELS_IMPL")?;
    let overview = mode(values, "MODEL_MANAGER_OVERVIEW_IMPL")?;
    let mut selection = Selection::default();
    if models {
        let tz = value(values, "TZ", "")?;
        let chosen = if tz.is_empty() || tz == "UTC" {
            roots(values)
        } else {
            Err("TZ is set to a zone other than UTC")
        };
        match chosen {
            Ok(roots) => selection.models = Some(roots),
            Err(reason) => {
                eprintln!("native-model-list: {reason}; GET /api/v1/models stays with python");
            }
        }
    }
    if overview {
        match roots(values) {
            Ok(roots) => selection.overview = Some(roots),
            Err(reason) => {
                eprintln!("native-model-list: {reason}; GET /api/v1/overview stays with python");
            }
        }
    }
    Ok(selection)
}

/// The child refuses a route only when the native parent owns it (`1`; `0` otherwise, also over
/// an inherited value), serves the matching private call, and reads the same settings.
#[cfg(test)]
pub(crate) fn child(command: &mut tokio::process::Command, selection: &Selection) {
    command.env(
        "NOEVIA_NATIVE_MODEL_MANAGER_MODELS_CONFIRMED",
        if selection.models.is_some() { "1" } else { "0" },
    );
    command.env(
        "NOEVIA_NATIVE_MODEL_MANAGER_OVERVIEW_CONFIRMED",
        if selection.overview.is_some() {
            "1"
        } else {
            "0"
        },
    );
    if let Some(roots) = &selection.models {
        command
            .env("TZ", "UTC")
            .env("MODELS_DIR", &roots.models)
            .env("MODELS_INI_PATH", &roots.ini)
            .env("DATA_DIR", &roots.data);
    }
    if let Some(roots) = &selection.overview {
        command
            .env("MODELS_DIR", &roots.models)
            .env("MODELS_INI_PATH", &roots.ini)
            .env("MODELS_HOST_PATH", &roots.host);
    }
}

/// The native routes' state, when either is native.
#[cfg(feature = "native-model-list")]
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

// Gated like the other slices' bin tests: the diagnosis mutation check pins the bin's test count.
#[cfg(all(test, feature = "native-model-list"))]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
        pairs
            .iter()
            .map(|(k, v)| (OsString::from(k), OsString::from(v)))
            .collect()
    }

    fn native() -> bool {
        cfg!(feature = "native-model-list")
    }

    #[test]
    fn python_is_the_default_and_bad_values_refuse_startup() {
        assert_eq!(select(&vars(&[])).unwrap(), Selection::default());
        for key in ["MODEL_MANAGER_MODELS_IMPL", "MODEL_MANAGER_OVERVIEW_IMPL"] {
            for bad in ["", "Rust", "native", " rust"] {
                let error = select(&vars(&[(key, bad)])).unwrap_err();
                assert_eq!(error, format!("{key} must be python or rust"));
            }
            let rust = select(&vars(&[(key, "rust")]));
            assert_eq!(rust.is_ok(), native(), "{key}");
            if !native() {
                assert!(rust
                    .unwrap_err()
                    .contains("compiled native-model-list feature"));
            }
        }
    }

    #[test]
    fn each_route_is_selected_on_its_own() {
        if !native() {
            return;
        }
        let both = select(&vars(&[
            ("MODEL_MANAGER_MODELS_IMPL", "rust"),
            ("MODEL_MANAGER_OVERVIEW_IMPL", "rust"),
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
        assert_eq!(both.models.as_ref(), Some(&roots));
        assert_eq!(both.overview.as_ref(), Some(&roots));
        let list = select(&vars(&[("MODEL_MANAGER_MODELS_IMPL", "rust")])).unwrap();
        assert!(list.models.is_some() && list.overview.is_none());
        assert_eq!(list.models.unwrap().models, "/models");
        let overview = select(&vars(&[("MODEL_MANAGER_OVERVIEW_IMPL", "rust")])).unwrap();
        assert!(overview.models.is_none());
        assert_eq!(overview.overview.unwrap().host, "");
    }

    #[test]
    fn settings_it_cannot_read_exactly_keep_python() {
        if !native() {
            return;
        }
        let both = |extra: &[(&str, &str)]| {
            let mut all = vec![
                ("MODEL_MANAGER_MODELS_IMPL", "rust"),
                ("MODEL_MANAGER_OVERVIEW_IMPL", "rust"),
            ];
            all.extend_from_slice(extra);
            select(&vars(&all)).unwrap()
        };
        for fold in [
            ("models_dir", "/x"),
            ("Models_Ini_Path", "/x.ini"),
            ("models_host_path", "/x"),
        ] {
            let selection = both(&[fold]);
            assert!(selection.models.is_none(), "{fold:?}");
            assert!(selection.overview.is_none(), "{fold:?}");
        }
        // DATA_DIR is read by the list only, but its alias is refused for both.
        assert_eq!(both(&[("data_dir", "/x")]), Selection::default());
        for models in ["relative", "/a/", "/a//b", "/a/./b", "/a/../b", ""] {
            assert_eq!(
                both(&[("MODELS_DIR", models)]),
                Selection::default(),
                "{models}"
            );
        }
        assert!(both(&[("MODELS_DIR", "/")]).models.is_some());
        let zone = both(&[("TZ", "Europe/Berlin")]);
        assert!(zone.models.is_none());
        assert!(zone.overview.is_some());
        assert!(both(&[("TZ", "UTC")]).models.is_some());
        assert!(both(&[("TZ", "")]).models.is_some());
        let long = "/".to_owned() + &"x".repeat(4096);
        assert_eq!(both(&[("MODELS_DIR", &long)]), Selection::default());
    }

    #[test]
    fn confirmations_override_inherited_values() {
        let roots = Roots {
            models: "/srv/models".into(),
            ini: "/srv/models.ini".into(),
            data: "/srv/data".into(),
            host: String::new(),
        };
        for (models, overview) in [(false, false), (true, false), (false, true), (true, true)] {
            let mut command = tokio::process::Command::new("uvicorn");
            command
                .env("NOEVIA_NATIVE_MODEL_MANAGER_MODELS_CONFIRMED", "hostile")
                .env("NOEVIA_NATIVE_MODEL_MANAGER_OVERVIEW_CONFIRMED", "hostile");
            let selection = Selection {
                models: models.then(|| roots.clone()),
                overview: overview.then(|| roots.clone()),
            };
            child(&mut command, &selection);
            let envs: std::collections::BTreeMap<_, _> = command.as_std().get_envs().collect();
            let get = |k: &str| {
                envs.get(std::ffi::OsStr::new(k))
                    .copied()
                    .flatten()
                    .map(|v| v.to_str().unwrap().to_owned())
            };
            let flag = |on: bool| Some(if on { "1" } else { "0" }.to_owned());
            assert_eq!(
                get("NOEVIA_NATIVE_MODEL_MANAGER_MODELS_CONFIRMED"),
                flag(models)
            );
            assert_eq!(
                get("NOEVIA_NATIVE_MODEL_MANAGER_OVERVIEW_CONFIRMED"),
                flag(overview)
            );
            assert_eq!(get("TZ"), models.then(|| "UTC".to_owned()));
            assert_eq!(get("DATA_DIR"), models.then(|| "/srv/data".to_owned()));
            assert_eq!(get("MODELS_HOST_PATH"), overview.then(String::new));
            assert_eq!(
                get("MODELS_DIR"),
                (models || overview).then(|| "/srv/models".to_owned())
            );
        }
    }
}
