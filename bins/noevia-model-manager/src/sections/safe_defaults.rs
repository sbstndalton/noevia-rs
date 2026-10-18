//! The read-only plan of api._safe_defaults_plan, run under the native writer's lock.
use super::*;
use crate::section_writes::{cfg::Dict, Plan};

fn refuse(status: u16, message: &str) -> Plan {
    Plan::Refused(status, message.to_owned())
}

pub(crate) fn plan(models: &Path, ini_path: &Path, name: &str) -> Result<Plan, Error> {
    // Existing settings win before model discovery, as the original helper checks them first.
    if let Some((_, parsed)) =
        crate::model_updates::read_ini(ini_path).map_err(|_| Error::Refused)?
    {
        if parsed.items(name, &mut Budget::default())?.is_some() {
            return Ok(refuse(409, "settings already exist for this model"));
        }
    }
    let draft = model_autoconfig::files::looks_like_draft(&format!("{name}.gguf"));
    if !models.exists() {
        return Ok(if draft {
            refuse(
                400,
                "draft heads and projectors are not registered on their own",
            )
        } else {
            refuse(404, "no downloaded GGUF matches this name")
        });
    }
    let files = Files::new(models, ini_path)?;
    files.plan(name, draft, models)
}

impl Files {
    fn plan(&self, name: &str, draft: bool, base: &Path) -> Result<Plan, Error> {
        let mut budget = Budget::default();
        let bytes = self.ini_root.ini(&self.ini_name, &mut budget)?;
        let text = std::str::from_utf8(&bytes)
            .map_err(|_| Error::Refused)?
            .replace("\r\n", "\n")
            .replace('\r', "\n");
        let parsed = ini::Ini::parse(&text, &mut budget)?;
        if parsed.items(name, &mut budget)?.is_some() {
            return Ok(refuse(409, "settings already exist for this model"));
        }
        let mut models = Vec::new();
        self.walk(".", 0, &mut models, &mut budget)?;
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
        let relative = stems.get(name).cloned();
        let chosen =
            if let Some((folder, _)) = relative.as_deref().and_then(|rel| rel.rsplit_once('/')) {
                let mut entries = self.models.entries(folder, &mut budget)?;
                entries.retain(|e| !e.directory && defaults::gguf_suffix(&e.name));
                entries.sort_by(|a, b| a.name.cmp(&b.name));
                entries
                    .iter()
                    .find(|e| !e.name.to_lowercase().contains("mmproj"))
                    .or(entries.first())
                    .map(|e| format!("{folder}/{}", e.name))
            } else {
                Some(relative.clone().unwrap_or_else(|| format!("{name}.gguf")))
            };
        let meta = match chosen.as_deref() {
            Some(path) => self
                .models
                .file(path, false, &mut budget)?
                .map(|f| f.metadata())
                .transpose()
                .map_err(|_| Error::Refused)?,
            None => None,
        };
        let large_build = chosen
            .as_deref()
            .and_then(|p| Path::new(p).file_stem())
            .and_then(|s| s.to_str())
            == Some(name)
            && meta
                .as_ref()
                .is_some_and(|m| m.len() > model_autoconfig::files::HEAD_MAX_BYTES as u64);
        if draft && !large_build {
            return Ok(refuse(
                400,
                "draft heads and projectors are not registered on their own",
            ));
        }
        let Some(relative) = relative.filter(|_| chosen.is_some()) else {
            return Ok(refuse(404, "no downloaded GGUF matches this name"));
        };
        for section in parsed.names() {
            let items = parsed.items(section, &mut budget)?.unwrap_or_default();
            if self
                .resolve(section, &items, &stems, &mut budget)?
                .as_deref()
                == Some(&relative)
            {
                return Ok(refuse(
                    409,
                    "another settings section already uses this file",
                ));
            }
        }
        let (defaults, _) =
            self.defaults(name, &Vec::new(), Some(relative.clone()), &mut budget)?;
        let mut values = Dict::default();
        for (key, value) in defaults.as_object().ok_or(Error::Refused)? {
            values.set(key, value.as_str().ok_or(Error::Refused)?);
        }
        if values.get("model").is_none_or(|v| v.is_empty()) {
            values.set("model", &format!("/models/{relative}"));
        }
        let native = values
            .get("ctx-size")
            .and_then(|v| model_autoconfig::pyval::int_str(v, "ctx").ok())
            .unwrap_or(0);
        values.set(
            "ctx-size",
            &if native > 0 { native.min(8192) } else { 8192 }.to_string(),
        );
        let subdir = relative.rsplit_once('/').map_or("", |(d, _)| d);
        let head = crate::draft_heads::find_mtp(&self.models, base, name, subdir)
            .map_err(|_| Error::Refused)?;
        let builtin = match chosen {
            Some(path) => {
                crate::draft_heads::builtin_layers(&self.models, base, &path)
                    .map_err(|_| Error::Refused)?
                    .parse::<i128>()
                    .map_err(|_| Error::Refused)?
                    > 0
            }
            None => false,
        };
        if !head.is_empty() {
            values.set("spec-type", "draft-mtp");
            values.set("spec-draft-model", &head);
        } else if builtin {
            values.set("spec-type", "draft-mtp");
        }
        Ok(Plan::Values(values, !head.is_empty() || builtin))
    }
}

#[cfg(test)]
mod tests;
