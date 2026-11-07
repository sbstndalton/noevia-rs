//! Per-model durable fences. Payload damage never prevents another model's reconciliation.
use serde::{Deserialize, Serialize};
use std::{
    io::Write,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    pub model: String,
    #[serde(deserialize_with = "required_revision")]
    pub revision: Option<String>,
    pub budget_gib: f64,
    pub at: f64,
    pub recorded: bool,
}

/// Written before the payload: damage must not erase its invalidation identity.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Context {
    #[serde(deserialize_with = "required_revision")]
    pub revision: Option<String>,
    pub revision_known: bool,
    pub budget_gib: Option<f64>,
}
impl Context {
    pub fn current(revision: Option<Option<String>>, budget: Option<f64>) -> Self {
        Self {
            revision_known: revision.is_some(),
            revision: revision.flatten(),
            budget_gib: budget.filter(|n| n.is_finite()),
        }
    }
    pub fn changed(&self, current: &Self) -> bool {
        (self.revision_known && current.revision_known && self.revision != current.revision)
            || self
                .budget_gib
                .zip(current.budget_gib)
                .is_some_and(|(a, b)| a != b)
    }
}
#[derive(Debug)]
pub enum Record {
    Healthy(Entry),
    Damaged { key: String, context: Context },
}
fn required_revision<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    Option::<String>::deserialize(d)
}
static JOURNAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
fn lock() -> std::io::Result<std::sync::MutexGuard<'static, ()>> {
    JOURNAL
        .lock()
        .map_err(|_| std::io::Error::other("fence lock poisoned"))
}
fn valid_revision(revision: &Option<String>) -> bool {
    revision.as_ref().is_none_or(|r| {
        r.len() == 64
            && r.bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
    })
}
fn valid_entry(entry: &Entry) -> bool {
    entry.budget_gib.is_finite()
        && entry.budget_gib > 0.0
        && entry.at.is_finite()
        && valid_revision(&entry.revision)
}
fn key(model: &str) -> String {
    super::super::ini::sha(model)
}
pub fn file(dir: &Path, model: &str) -> PathBuf {
    dir.join(format!("{}.json", key(model)))
}
fn identity(dir: &Path, hash: &str) -> PathBuf {
    dir.join(format!("{hash}.context"))
}
fn sync(dir: &Path) -> std::io::Result<()> {
    std::fs::File::open(dir)?.sync_all()
}
fn write(dir: &Path, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    if let Some(parent) = dir.parent() {
        sync(parent)?;
    }
    let temp = path.with_extension("pending");
    let mut f = std::fs::File::create(&temp)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    std::fs::rename(temp, path)?;
    sync(dir)
}
pub fn save(dir: &Path, entry: &Entry) -> std::io::Result<()> {
    let _guard = lock()?;
    let context = Context::current(Some(entry.revision.clone()), Some(entry.budget_gib));
    write(
        dir,
        &identity(dir, &key(&entry.model)),
        &serde_json::to_vec(&context)?,
    )?;
    write(dir, &file(dir, &entry.model), &serde_json::to_vec(entry)?)
}
fn names(dir: &Path, hash: &str) -> std::io::Result<Vec<PathBuf>> {
    let files = match std::fs::read_dir(dir) {
        Ok(files) => files,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut out = Vec::new();
    for f in files {
        let f = f?;
        let n = f.file_name().to_string_lossy().into_owned();
        if n == format!("{hash}.json")
            || n == format!("{hash}.context")
            || n.starts_with(&format!("{hash}.json.damaged-"))
        {
            out.push(f.path());
        }
    }
    Ok(out)
}
fn clear_key_inner(dir: &Path, hash: &str) -> std::io::Result<()> {
    // Keep the context until the payload and all archived damage are removed.
    let context = identity(dir, hash);
    let mut files = names(dir, hash)?;
    files.sort_by_key(|p| p == &context);
    for f in files {
        match std::fs::remove_file(f) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    if dir.exists() {
        sync(dir)?;
    }
    Ok(())
}
pub fn clear_key(dir: &Path, hash: &str) -> std::io::Result<()> {
    let _guard = lock()?;
    clear_key_inner(dir, hash)
}
pub fn remove(dir: &Path, model: &str) -> std::io::Result<()> {
    clear_key(dir, &key(model))
}
pub fn clear_damage(dir: &Path, model: &str) -> std::io::Result<()> {
    let _guard = lock()?;
    let hash = key(model);
    for f in names(dir, &hash)? {
        if f.file_name().is_some_and(|n| {
            n.to_string_lossy()
                .starts_with(&format!("{hash}.json.damaged-"))
        }) {
            std::fs::remove_file(f)?;
        }
    }
    sync(dir)
}
pub fn read(dir: &Path, model: &str) -> std::io::Result<Entry> {
    let _guard = lock()?;
    let e: Entry = serde_json::from_slice(&std::fs::read(file(dir, model))?)?;
    if e.model != model || !valid_entry(&e) {
        return Err(std::io::Error::other("invalid fence"));
    }
    Ok(e)
}
fn inspect(dir: &Path, hash: &str, current: &Context) -> std::io::Result<Record> {
    let path = dir.join(format!("{hash}.json"));
    if let Ok(bytes) = std::fs::read(&path) {
        if let Ok(entry) = serde_json::from_slice::<Entry>(&bytes) {
            if key(&entry.model) == hash && valid_entry(&entry) {
                let expected =
                    Context::current(Some(entry.revision.clone()), Some(entry.budget_gib));
                let saved = std::fs::read(identity(dir, hash))
                    .ok()
                    .and_then(|b| serde_json::from_slice::<Context>(&b).ok());
                if saved.as_ref() != Some(&expected) {
                    write(dir, &identity(dir, hash), &serde_json::to_vec(&expected)?)?;
                }
                return Ok(Record::Healthy(entry));
            }
        }
    }
    let mut context = std::fs::read(identity(dir, hash))
        .ok()
        .and_then(|b| serde_json::from_slice::<Context>(&b).ok())
        .filter(|c| valid_revision(&c.revision) && c.budget_gib.is_none_or(|n| n.is_finite()))
        .unwrap_or_else(|| current.clone());
    // Older damaged files had no sidecar. Capture the first known identity once, never refresh
    // it on restart; a subsequent revision or budget change safely invalidates that fence too.
    if !context.revision_known && current.revision_known {
        context.revision = current.revision.clone();
        context.revision_known = true;
    }
    if context.budget_gib.is_none() {
        context.budget_gib = current.budget_gib;
    }
    write(dir, &identity(dir, hash), &serde_json::to_vec(&context)?)?;
    let archived = names(dir, hash)?.iter().any(|p| {
        p.file_name().is_some_and(|n| {
            n.to_string_lossy()
                .starts_with(&format!("{hash}.json.damaged-"))
        })
    });
    if !archived || path.exists() {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let archive = dir.join(format!("{hash}.json.damaged-{ts}"));
        match std::fs::rename(&path, &archive) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                write(dir, &archive, b"")?;
            }
            Err(e) => return Err(e),
        }
        sync(dir)?;
        eprintln!("{}damaged-fence {hash}", super::LOG_PREFIX);
    }
    Ok(Record::Damaged {
        key: hash.to_owned(),
        context,
    })
}
/// An individual error is returned alongside its hash, never as a directory-wide failure.
pub type Scanned = (String, std::io::Result<Record>);
pub fn entries(dir: &Path, current: &Context) -> std::io::Result<Vec<Scanned>> {
    let _guard = lock()?;
    let files = match std::fs::read_dir(dir) {
        Ok(files) => files,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut keys = std::collections::BTreeSet::new();
    for f in files {
        let Ok(f) = f else {
            continue;
        };
        let n = f.file_name().to_string_lossy().into_owned();
        if let Some((hash, tail)) = n.split_once('.') {
            if hash.len() == 64
                && hash.bytes().all(|c| c.is_ascii_hexdigit())
                && (tail == "json" || tail == "context" || tail.starts_with("json.damaged-"))
            {
                keys.insert(hash.to_owned());
            }
        }
    }
    Ok(keys
        .into_iter()
        .map(|hash| {
            let record = inspect(dir, &hash, current);
            (hash, record)
        })
        .collect())
}
pub fn fenced(dir: &Path, model: &str) -> bool {
    names(dir, &key(model)).map_or(true, |n| !n.is_empty())
}
/// Admission can invalidate its own stale fence immediately, without waiting behind another model.
pub fn admission(dir: &Path, model: &str, current: &Context) -> bool {
    let Ok(_guard) = lock() else {
        return true;
    };
    if !fenced(dir, model) {
        return false;
    }
    let hash = key(model);
    let context = match inspect(dir, &hash, current) {
        Ok(Record::Healthy(e)) => Context::current(Some(e.revision), Some(e.budget_gib)),
        Ok(Record::Damaged { context, .. }) => context,
        Err(_) => return true,
    };
    if context.changed(current) {
        return clear_key_inner(dir, &hash).is_err();
    }
    true
}
