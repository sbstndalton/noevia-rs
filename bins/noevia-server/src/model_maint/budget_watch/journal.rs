//! Durable admission fence, installed synchronously before the first unload byte is sent.
//! Core's budget-watch-pending.cjs reads the same files; only this worker writes them.
use serde::{Deserialize, Serialize};
use std::{
    io::Write,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    pub model: String,
    pub revision: Option<String>,
    pub budget_gib: f64,
    pub at: f64,
    pub recorded: bool,
}
pub fn file(dir: &Path, model: &str) -> PathBuf {
    dir.join(format!("{}.json", super::super::ini::sha(model)))
}
pub fn save(dir: &Path, entry: &Entry) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    if let Some(parent) = dir.parent() {
        std::fs::File::open(parent)?.sync_all()?;
    }
    let path = file(dir, &entry.model);
    let temp = path.with_extension("pending");
    let bytes = serde_json::to_vec(entry)?;
    let mut f = std::fs::File::create(&temp)?;
    f.write_all(&bytes)?;
    f.sync_all()?;
    std::fs::rename(temp, path)?;
    std::fs::File::open(dir)?.sync_all()
}
pub fn remove(dir: &Path, model: &str) -> std::io::Result<()> {
    match std::fs::remove_file(file(dir, model)) {
        Ok(()) => std::fs::File::open(dir)?.sync_all(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}
pub fn read(dir: &Path, model: &str) -> std::io::Result<Entry> {
    Ok(serde_json::from_slice(&std::fs::read(file(dir, model))?)?)
}
pub fn entries(dir: &Path) -> std::io::Result<Vec<Entry>> {
    let files = match std::fs::read_dir(dir) {
        Ok(files) => files,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut entries = Vec::new();
    for f in files {
        let p = f?.path();
        if p.extension().is_some_and(|e| e == "json") {
            entries.push(serde_json::from_slice(&std::fs::read(p)?)?);
        }
    }
    Ok(entries)
}
