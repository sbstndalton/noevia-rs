//! Native configuration reads. Python remains the sole file writer.
pub(crate) mod access;
mod defaults;
pub mod ini;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    io::{self, Write},
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

pub const INI_CAP: usize = 1024 * 1024;
pub const RESPONSE_CAP: usize = 8 * 1024 * 1024;
pub const REFUSAL: &str = "Native section configuration is unavailable";
#[derive(Debug, Clone, Copy)]
pub enum Error {
    Refused,
    InvalidName,
}
pub struct Budget {
    start: Instant,
    entries: usize,
    output: usize,
}
impl Default for Budget {
    fn default() -> Self {
        Self {
            start: Instant::now(),
            entries: 0,
            output: 0,
        }
    }
}
impl Budget {
    pub fn work(&self) -> Result<(), Error> {
        if self.start.elapsed() > Duration::from_secs(2) {
            Err(Error::Refused)
        } else {
            Ok(())
        }
    }
    pub fn entry(&mut self) -> Result<(), Error> {
        self.work()?;
        self.entries = self.entries.checked_add(1).ok_or(Error::Refused)?;
        if self.entries > 16384 {
            Err(Error::Refused)
        } else {
            Ok(())
        }
    }
    // Charge a worst-case JSON string expansion before allocating response values.
    pub fn charge(&mut self, bytes: usize) -> Result<(), Error> {
        self.output = self
            .output
            .checked_add(bytes.checked_mul(6).ok_or(Error::Refused)?)
            .ok_or(Error::Refused)?;
        if self.output > RESPONSE_CAP {
            Err(Error::Refused)
        } else {
            self.work()
        }
    }
}
struct Capped {
    bytes: Vec<u8>,
}
impl Write for Capped {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.bytes.len().saturating_add(bytes.len()) > RESPONSE_CAP {
            return Err(io::Error::other("response cap"));
        }
        self.bytes
            .try_reserve(bytes.len())
            .map_err(io::Error::other)?;
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
fn encode(value: &Value) -> Result<Vec<u8>, Error> {
    let mut out = Capped { bytes: Vec::new() };
    serde_json::to_writer(&mut out, value).map_err(|_| Error::Refused)?;
    Ok(out.bytes)
}
fn schema() -> Result<&'static Value, Error> {
    static SCHEMA: std::sync::OnceLock<Result<Value, ()>> = std::sync::OnceLock::new();
    SCHEMA
        .get_or_init(|| serde_json::from_str(include_str!("schema.json")).map_err(|_| ()))
        .as_ref()
        .map_err(|_| Error::Refused)
}
fn kind(key: &str) -> Option<&'static str> {
    schema()
        .ok()?
        .as_array()?
        .iter()
        .flat_map(|tier| {
            tier.get("fields")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
        })
        .find(|field| field.get("key").and_then(Value::as_str) == Some(key))?
        .get("kind")?
        .as_str()
}
/// `ini.ALL_FIELDS` as (key, kind), in schema order (the schema is generated from it).
pub fn field_kinds() -> Vec<(&'static str, &'static str)> {
    let Ok(schema) = schema() else {
        return Vec::new();
    };
    schema
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|tier| {
            tier.get("fields")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
        })
        .filter_map(|f| Some((f.get("key")?.as_str()?, f.get("kind")?.as_str()?)))
        .collect()
}
#[derive(Clone, Debug)]
pub struct Model {
    pub relative: String,
    pub size: u64,
}
pub trait Read: Send + Sync {
    fn response(&self, name: Option<&str>, defaults: bool) -> Result<Vec<u8>, Error>;
}
pub struct Sections {
    reader: Arc<dyn Read>,
    permits: Arc<tokio::sync::Semaphore>,
}
impl Sections {
    pub fn new(reader: Arc<dyn Read>) -> Self {
        Self {
            reader,
            permits: Arc::new(tokio::sync::Semaphore::new(2)),
        }
    }
    pub async fn response(&self, name: Option<String>, defaults: bool) -> Result<Vec<u8>, Error> {
        let permit = self
            .permits
            .clone()
            .try_acquire_owned()
            .map_err(|_| Error::Refused)?;
        let reader = self.reader.clone();
        // The blocking operation retains admission even when this future is dropped/timed out.
        let task = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            reader.response(name.as_deref(), defaults)
        });
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .map_err(|_| Error::Refused)?
            .map_err(|_| Error::Refused)?
    }
}
pub struct Files {
    models: access::Root,
    ini_root: access::Root,
    ini_name: String,
}
impl Files {
    pub fn new(models: &Path, ini_path: &Path) -> Result<Self, Error> {
        let name = ini_path
            .file_name()
            .and_then(|n| n.to_str())
            .filter(|n| n.len() <= 256 && !n.contains(['*', '?', '[', ']']))
            .ok_or(Error::Refused)?;
        Ok(Self {
            models: access::Root::new(models)?,
            ini_root: access::Root::new(ini_path.parent().ok_or(Error::Refused)?)?,
            ini_name: name.to_owned(),
        })
    }
    fn walk(
        &self,
        relative: &str,
        depth: usize,
        out: &mut Vec<Model>,
        budget: &mut Budget,
    ) -> Result<(), Error> {
        let mut entries = self.models.entries(relative, budget)?;
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        // os.walk yields the current directory's sorted files before sorted descendants.
        for entry in &entries {
            if entry.directory || !entry.name.to_lowercase().ends_with(".gguf") {
                continue;
            }
            let path = if relative == "." {
                entry.name.clone()
            } else {
                format!("{relative}/{}", entry.name)
            };
            if let Some(file) = self.models.file(&path, false, budget)? {
                if out.len() >= 4096 {
                    return Err(Error::Refused);
                }
                out.try_reserve(1).map_err(|_| Error::Refused)?;
                out.push(Model {
                    relative: path,
                    size: file.metadata().map_err(|_| Error::Refused)?.len(),
                });
            }
        }
        if depth < 4 {
            for entry in entries {
                if entry.directory
                    && !entry.name.starts_with('.')
                    && !["blobs", "refs", ".locks", ".cache", ".git", "__pycache__"]
                        .contains(&entry.name.as_str())
                {
                    let path = if relative == "." {
                        entry.name
                    } else {
                        format!("{relative}/{}", entry.name)
                    };
                    self.walk(&path, depth + 1, out, budget)?;
                }
            }
        }
        Ok(())
    }
    fn resolve(
        &self,
        name: &str,
        items: &ini::Items,
        stems: &HashMap<String, String>,
        budget: &mut Budget,
    ) -> Result<Option<String>, Error> {
        if let Some((_, model)) = items
            .iter()
            .find(|(key, value)| key == "model" && !value.trim().is_empty())
        {
            let model = model.trim();
            let relative = model.replacen("/models/", "", 1);
            let relative = relative.trim_start_matches('/');
            if model.starts_with('/') && !model.starts_with("/models/") {
                return Err(Error::Refused);
            }
            if self.models.file(relative, false, budget)?.is_some() {
                return Ok(Some(relative.to_owned()));
            }
        }
        Ok(stems.get(name).cloned())
    }
    fn backups(&self, budget: &mut Budget) -> Result<Value, Error> {
        let prefix = format!("{}.bak-", self.ini_name);
        let mut candidates = self.ini_root.entries(".", budget)?;
        candidates.retain(|e| e.name.starts_with(&prefix));
        if candidates.len() > 1024 {
            return Err(Error::Refused);
        }
        candidates.sort_by(|a, b| b.name.cmp(&a.name));
        let mut rows = Vec::new();
        for entry in candidates {
            let Some(stat) = self.ini_root.stat(&entry.name, budget)? else {
                break;
            };
            let time = stat.modified().map_err(|_| Error::Refused)?;
            let mtime = match time.duration_since(std::time::UNIX_EPOCH) {
                Ok(d) => d.as_secs_f64(),
                Err(e) => -e.duration().as_secs_f64(),
            };
            if !mtime.is_finite() {
                return Err(Error::Refused);
            }
            budget.charge(entry.name.len() + 64)?;
            rows.push(json!([entry.name, mtime, stat.len()]));
        }
        Ok(Value::Array(rows))
    }
}
fn shard(filename: &str) -> Result<(String, Option<u32>, Option<u32>), Error> {
    static SHARD: std::sync::OnceLock<Result<regex::Regex, regex::Error>> =
        std::sync::OnceLock::new();
    let regex = SHARD
        .get_or_init(|| regex::Regex::new(r"-(\d{5})-of-(\d{5})(\.[^.]+)$"))
        .as_ref()
        .map_err(|_| Error::Refused)?;
    let Some(caps) = regex.captures(filename) else {
        return Ok((filename.to_owned(), None, None));
    };
    let Some(m) = caps.get(0) else {
        return Err(Error::Refused);
    };
    let suffix = caps.get(3).map(|m| m.as_str()).ok_or(Error::Refused)?;
    Ok((
        format!(
            "{}{suffix}",
            filename.get(..m.start()).ok_or(Error::Refused)?
        ),
        caps.get(1).and_then(|m| m.as_str().parse().ok()),
        caps.get(2).and_then(|m| m.as_str().parse().ok()),
    ))
}
fn companion(filename: &str, size: u64) -> bool {
    let low = filename.to_lowercase();
    low.contains("mmproj")
        || low.contains("imatrix")
        || (model_autoconfig::files::looks_like_draft(filename) && size <= 2 * 1024 * 1024 * 1024)
}
fn valid_name(name: &str) -> bool {
    let name = name.strip_suffix('\n').unwrap_or(name);
    !name.is_empty()
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"._-+".contains(&c))
}
impl Read for Files {
    fn response(&self, name: Option<&str>, defaults: bool) -> Result<Vec<u8>, Error> {
        let mut budget = Budget::default();
        if name.is_some_and(|n| !valid_name(n)) {
            return Err(Error::InvalidName);
        }
        let bytes = self.ini_root.ini(&self.ini_name, &mut budget)?;
        let decoded = std::str::from_utf8(&bytes).map_err(|_| Error::Refused)?;
        // Path.read_text uses universal newline decoding; revision still hashes raw bytes.
        let normalized = decoded.replace("\r\n", "\n").replace('\r', "\n");
        let text = normalized.as_str();
        let parsed = ini::Ini::parse(text, &mut budget)?;
        let current = match name {
            Some(name) => parsed.items(name, &mut budget)?,
            None => None,
        };
        let mut models = Vec::new();
        if name.is_none() || defaults || current.is_none() {
            self.walk(".", 0, &mut models, &mut budget)?;
        }
        let mut stems = HashMap::new();
        stems
            .try_reserve(models.len())
            .map_err(|_| Error::Refused)?;
        for model in &models {
            let filename = model.relative.rsplit('/').next().ok_or(Error::Refused)?;
            if companion(filename, model.size) {
                continue;
            }
            let (base, _, _) = shard(filename)?;
            let stem = if base.to_lowercase().ends_with(".gguf") {
                base.get(..base.len().saturating_sub(5))
                    .ok_or(Error::Refused)?
            } else {
                &base
            };
            stems.insert(stem.to_owned(), model.relative.clone());
        }
        budget.charge(include_str!("schema.json").len() + 128)?;
        let revision = format!("{:x}", Sha256::digest(&bytes));
        let response = if let Some(name) = name {
            let (values, extras, hints) = if defaults || current.is_none() {
                let empty = Vec::new();
                let items = current.as_ref().unwrap_or(&empty);
                let relative = self.resolve(name, items, &stems, &mut budget)?;
                let (values, hints) = self.defaults(name, items, relative, &mut budget)?;
                (values, String::new(), hints)
            } else {
                let mut form = serde_json::Map::new();
                let mut extras = String::new();
                for (key, value) in current.as_ref().ok_or(Error::Refused)? {
                    budget.charge(key.len() + value.len() + 8)?;
                    if kind(key).is_some() {
                        form.insert(key.clone(), Value::String(value.clone()));
                    } else {
                        if !extras.is_empty() {
                            extras.push('\n');
                        }
                        extras.push_str(key);
                        extras.push_str(" = ");
                        extras.push_str(value);
                    }
                }
                (Value::Object(form), extras, Vec::new())
            };
            json!({"name":name,"exists":current.is_some(),"values":values,"extras":extras,"hints":hints,"revision":revision,"schema":schema()?})
        } else {
            let mut rows = Vec::new();
            let mut claimed = HashSet::new();
            let mut names = HashSet::new();
            claimed
                .try_reserve(ini::MAX_SECTIONS)
                .map_err(|_| Error::Refused)?;
            names
                .try_reserve(ini::MAX_SECTIONS)
                .map_err(|_| Error::Refused)?;
            for name in parsed.names() {
                budget.work()?;
                let items = parsed.items(name, &mut budget)?.ok_or(Error::Refused)?;
                budget.charge(name.len().saturating_mul(5) + 64)?;
                let matched = self.resolve(name, &items, &stems, &mut budget)?;
                let cli = ini::cli(name, &items, kind)?;
                budget.charge(
                    name.len()
                        + cli.len()
                        + items
                            .iter()
                            .map(|(k, v)| k.len() + v.len() + 8)
                            .sum::<usize>()
                        + matched.as_ref().map_or(0, String::len)
                        + 128,
                )?;
                if let Some(file) = &matched {
                    claimed.insert(file.clone());
                }
                names.insert(name);
                rows.try_reserve(1).map_err(|_| Error::Refused)?;
                rows.push(json!({"name":name,"items":items,"hasFile":matched.is_some(),"file":matched,"cli":cli}));
            }
            let mut unregistered = Vec::new();
            for (stem, relative) in stems {
                if !names.contains(stem.as_str()) && !claimed.contains(&relative) {
                    budget.charge(stem.len() + 8)?;
                    unregistered.try_reserve(1).map_err(|_| Error::Refused)?;
                    unregistered.push(stem);
                }
            }
            unregistered.sort();
            budget.charge(text.len())?;
            json!({"revision":revision,"schema":schema()?,"sections":rows,"unregistered":unregistered,"backups":self.backups(&mut budget)?,"raw":text})
        };
        budget.work()?;
        encode(&response)
    }
}

#[cfg(test)]
mod tests;
