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
