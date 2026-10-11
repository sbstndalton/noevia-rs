//! A recorded corpus on disk: `<dir>/exchanges/*.json` (or `<dir>/*.json`) plus an optional
//! `<dir>/manifest.json` naming the inputs a replay must supply.

use crate::at;
use serde_json::Value;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Exchange {
    pub file: String,
    pub value: Value,
}

impl Exchange {
    pub fn request(&self) -> &Value {
        at(&self.value, "request")
    }
    pub fn response(&self) -> &Value {
        at(&self.value, "response")
    }
    pub fn method(&self) -> &str {
        at(self.request(), "method").as_str().unwrap_or("GET")
    }
    pub fn path(&self) -> &str {
        at(self.request(), "path").as_str().unwrap_or("/")
    }
}

/// Where the value for an input placeholder comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Input {
    Value(String),
    /// A file in the server's data directory, read (and trimmed) when first needed.
    File(PathBuf),
}

#[derive(Debug, Clone, Default)]
pub struct Corpus {
    pub utc_calendar: bool,
    pub exchanges: Vec<Exchange>,
    pub inputs: Vec<(String, Input)>,
    pub dropped: usize,
}

pub fn load(dir: &Path) -> Result<Corpus, String> {
    let ex_dir = if dir.join("exchanges").is_dir() {
        dir.join("exchanges")
    } else {
        dir.to_path_buf()
    };
    let mut names: Vec<String> = std::fs::read_dir(&ex_dir)
        .map_err(|e| format!("read {}: {e}", ex_dir.display()))?
        .filter_map(Result::ok)
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    names.sort();
    let mut corpus = Corpus::default();
    for name in names {
        if name.ends_with(".dropped") {
            corpus.dropped += 1;
            continue;
        }
        if !name.ends_with(".json") || name == "manifest.json" {
            continue;
        }
        let path = ex_dir.join(&name);
        let text =
            std::fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
        let value: Value = serde_json::from_str(&text).map_err(|e| format!("{name}: {e}"))?;
        if at(&value, "v").as_u64() != Some(1)
            || !at(&value, "request").is_object()
            || !at(&value, "response").is_object()
        {
            return Err(format!("{name}: not a v1 contract exchange"));
        }
        corpus.exchanges.push(Exchange { file: name, value });
    }
    let manifest = dir.join("manifest.json");
    if manifest.is_file() {
        let text = std::fs::read_to_string(&manifest)
            .map_err(|e| format!("read {}: {e}", manifest.display()))?;
        let m: Value = serde_json::from_str(&text).map_err(|e| format!("manifest.json: {e}"))?;
        if let Some(calendar) = m.get("calendarContract") {
            if calendar != &serde_json::json!({"version":1,"timeZone":"UTC","usageWindowDays":365})
            {
                return Err("unsupported manifest calendar contract".into());
            }
            corpus.utc_calendar = true;
        }
        if let Some(inputs) = at(&m, "inputs").as_object() {
            for (k, v) in inputs {
                let input = if let Some(s) = at(v, "value").as_str() {
                    Input::Value(s.to_string())
                } else if let Some(f) = at(v, "file").as_str() {
                    let p = PathBuf::from(f);
                    if p.is_absolute()
                        || p.components()
                            .any(|c| matches!(c, std::path::Component::ParentDir))
                    {
                        return Err(format!(
                            "manifest input {k}: file must stay inside the data dir"
                        ));
                    }
                    Input::File(p)
                } else {
                    return Err(format!("manifest input {k}: needs value or file"));
                };
                corpus.inputs.push((k.clone(), input));
            }
        }
    }
    Ok(corpus)
}

#[cfg(test)]
mod calendar_manifest_tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    #[test]
    fn checked_in_native_http_corpus_requires_fixed_host_provenance() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../contracts/http/corpus");
        let provenance: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.join("SOURCE_ORACLE.json")).unwrap())
                .unwrap();
        assert_eq!(
            provenance.get("core").and_then(serde_json::Value::as_str),
            Some(include_str!("../../../contracts/http/core.ref").trim())
        );
        assert_eq!(
            provenance.get("hostInput"),
            Some(
                &serde_json::json!({"module":"server/inference-budget.cjs","collaborator":"readMeminfo","memTotalKib":8388608})
            )
        );
        let response: serde_json::Value = serde_json::from_slice(
            &std::fs::read(dir.join("exchanges/000139-GET-api-models-inference-budget.json"))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            response.pointer("/response/body/json/hostRamGib"),
            Some(&serde_json::json!(8))
        );
    }
    #[test]
    fn checked_in_native_http_corpus_requires_calendar_contract() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../contracts/http/corpus");
        let corpus = load(&dir).unwrap();
        assert!(
            corpus.utc_calendar,
            "native HTTP corpus must retain its strict UTC calendar contract"
        );
        assert_eq!(corpus.exchanges.len(), 193);
    }
    #[test]
    fn calendar_contract_is_explicit_and_exact() {
        let dir =
            std::env::temp_dir().join(format!("replay-calendar-manifest-{}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        for (manifest, accepted, enabled) in [
            (serde_json::json!({}), true, false),
            (
                serde_json::json!({"calendarContract":{"version":1,"timeZone":"UTC","usageWindowDays":365}}),
                true,
                true,
            ),
            (
                serde_json::json!({"calendarContract":{"version":1,"timeZone":"Europe/Berlin","usageWindowDays":365}}),
                false,
                false,
            ),
            (
                serde_json::json!({"calendarContract":{"version":1,"timeZone":"UTC","usageWindowDays":364}}),
                false,
                false,
            ),
            (
                serde_json::json!({"calendarContract":{"version":1,"timeZone":"UTC","usageWindowDays":365,"ignoreCounters":true}}),
                false,
                false,
            ),
        ] {
            std::fs::write(dir.join("manifest.json"), manifest.to_string()).unwrap();
            let result = load(&dir);
            assert_eq!(result.is_ok(), accepted);
            if let Ok(corpus) = result {
                assert_eq!(corpus.utc_calendar, enabled);
            }
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}
