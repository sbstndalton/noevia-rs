use super::{ini, shard, Budget, Error, Files};
use serde_json::{json, Value};
use std::io::{Read, Seek, SeekFrom};

struct Header<'a> {
    file: std::fs::File,
    read: usize,
    refused: bool,
    budget: &'a Budget,
}
impl Read for Header<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.read.saturating_add(buf.len()) > 64 * 1024 * 1024 || self.budget.work().is_err() {
            self.refused = true;
            return Err(std::io::Error::other("metadata budget"));
        }
        let n = self.file.read(buf)?;
        self.read += n;
        Ok(n)
    }
}
impl Seek for Header<'_> {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        self.file.seek(pos)
    }
}
fn finite(value: &gguf::Value) -> bool {
    match value {
        gguf::Value::Float(f) => f.is_finite(),
        gguf::Value::List(values) => values.iter().all(finite),
        gguf::Value::ArraySummary { sample, .. } => sample.iter().all(finite),
        _ => true,
    }
}
fn truthy(value: Option<&gguf::Json>) -> bool {
    match value {
        None | Some(gguf::Json::Null) => false,
        Some(gguf::Json::Bool(b)) => *b,
        Some(gguf::Json::Int(n)) => n.to_f64() != 0.0,
        Some(gguf::Json::Float(f)) => *f != 0.0,
        Some(gguf::Json::Str(s)) => !s.is_empty(),
        Some(gguf::Json::Array(v)) => !v.is_empty(),
        Some(gguf::Json::Object(v)) => !v.is_empty(),
    }
}
fn integer(value: Option<&gguf::Json>) -> Option<gguf::PyInt> {
    match value {
        Some(gguf::Json::Int(integer)) => Some(integer.clone()),
        Some(gguf::Json::Bool(b)) => Some(gguf::PyInt::Small(i128::from(*b))),
        _ => None,
    }
}
fn greater_than(value: &gguf::PyInt, minimum: i128) -> bool {
    match value {
        gguf::PyInt::Small(number) => *number > minimum,
        // Existing summary Big values are canonical decimals outside i128, produced
        // from finite f64. They contain at most 309 digits; positive Big exceeds 0/1.
        gguf::PyInt::Big(decimal) => !decimal.starts_with('-'),
    }
}
pub(super) fn gguf_suffix(name: &str) -> bool {
    std::path::Path::new(name)
        .extension()
        .and_then(std::ffi::OsStr::to_str)
        .is_some_and(|suffix| suffix.eq_ignore_ascii_case("gguf"))
}
fn pystr(value: Option<&gguf::Json>) -> String {
    match value {
        None | Some(gguf::Json::Null) => "None".into(),
        Some(gguf::Json::Str(s)) => s.clone(),
        Some(gguf::Json::Bool(true)) => "True".into(),
        Some(gguf::Json::Bool(false)) => "False".into(),
        Some(v) => v.to_string(),
    }
}
fn commas(number: &gguf::PyInt) -> String {
    let raw = number.to_string();
    let mut out = String::new();
    for (i, c) in raw.chars().enumerate() {
        if i > 0 && (raw.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}
pub fn suggest(
    summary: &gguf::Json,
    rope_text: &str,
    budget: &mut Budget,
) -> Result<(Value, Vec<String>), Error> {
    let model = summary.get("model");
    let field = |key: &str| model.and_then(|m| m.get(key));
    let mut fields = serde_json::Map::new();
    let mut hints = Vec::new();
    budget.charge(2048)?;
    if let Some(ctx) = integer(field("context_length")).filter(|i| greater_than(i, 0)) {
        fields.insert("ctx-size".into(), Value::String(ctx.to_string()));
    }
    for (k, v) in [
        ("ngl", "999"),
        ("flash-attn", "on"),
        ("cache-type-k", "q8_0"),
        ("cache-type-v", "q8_0"),
        ("parallel", "1"),
        ("split-mode", "layer"),
    ] {
        fields.insert(k.into(), json!(v));
    }
    if truthy(summary.get("chat_template")) {
        fields.insert("jinja".into(), json!("true"));
    }
    let arch = if truthy(field("arch")) {
        pystr(field("arch")).to_lowercase()
    } else {
        String::new()
    };
    let rope_type = if truthy(field("rope_scaling_type")) {
        ini::strip(rope_text).to_lowercase()
    } else {
        String::new()
    };
    let factor = match field("rope_scaling_factor") {
        Some(gguf::Json::Float(f)) => *f > 0.0,
        Some(gguf::Json::Int(i)) => i.to_f64() > 0.0,
        Some(gguf::Json::Bool(b)) => *b,
        _ => false,
    };
    if arch.starts_with("gemma3") || !["", "none"].contains(&rope_type.as_str()) || factor {
        if let Some(original) =
            integer(field("rope_scaling_original_context")).filter(|i| greater_than(i, 0))
        {
            budget.charge(rope_text.len() + 256)?;
            hints.push(format!("Model trained on {} ctx and scaled with {rope_text}; llama.cpp applies that scaling from the GGUF, so no rope keys are set.", commas(&original)));
        }
    }
    if let Some(experts) = integer(field("expert_count")).filter(|i| greater_than(i, 1)) {
        let used = if truthy(field("expert_used_count")) {
            pystr(field("expert_used_count"))
        } else {
            "?".into()
        };
        budget.charge(used.len() + 512)?;
        hints.push(format!("MoE model detected: {experts} experts, {used} active per token. If it doesn't fit in VRAM, add extras like `n-cpu-moe = <N>` to keep the first N layers' experts on CPU, or `cpu-moe = true` for all MoE weights on CPU."));
    }
    if matches!(summary.get("arch"),Some(gguf::Json::Str(s)) if s.to_lowercase().starts_with("gemma"))
    {
        hints.push("Gemma architecture uses sliding-window attention. If ctx feels tight in VRAM, try `swa-full = true` in advanced.".into());
    }
    budget.charge(1024)?;
    Ok((Value::Object(fields), hints))
}
impl Files {
    pub(super) fn defaults(
        &self,
        name: &str,
        items: &ini::Items,
        relative: Option<String>,
        budget: &mut Budget,
    ) -> Result<(Value, Vec<String>), Error> {
        let explicit = items
            .iter()
            .find(|(k, _)| k == "model")
            .map(|(_, v)| ini::strip(v))
            .unwrap_or_default();
        let mut model_rel = String::new();
        let chosen = if let Some(relative) = relative {
            if !explicit.is_empty() && self.models.file(&relative, false, budget)?.is_some() {
                if relative.contains('/') {
                    model_rel = explicit.to_owned();
                }
                Some(relative)
            } else if let Some((folder, _)) = relative.rsplit_once('/') {
                let mut files = self.models.entries(folder, budget)?;
                files.retain(|e| !e.directory && gguf_suffix(&e.name));
                files.sort_by(|a, b| a.name.cmp(&b.name));
                let main = files
                    .iter()
                    .find(|e| !e.name.to_lowercase().contains("mmproj"))
                    .or_else(|| files.first());
                if let Some(main) = main {
                    let path = format!("{folder}/{}", main.name);
                    model_rel = format!("/models/{path}");
                    Some(path)
                } else {
                    None
                }
            } else {
                Some(relative)
            }
        } else {
            Some(format!("{name}.gguf"))
        };
        let file = match chosen {
            Some(path) => self.models.file(&path, false, budget)?,
            None => None,
        };
        let Some(file) = file else {
            budget.charge(name.len() + 128)?;
            return Ok((
                json!({}),
                vec![format!(
                    "No GGUF found for `{name}` — cannot suggest defaults from metadata."
                )],
            ));
        };
        let mut header = Header {
            file,
            read: 0,
            refused: false,
            budget,
        };
        let raw = gguf::read_raw_stream_with_limits(
            &mut header,
            gguf::ReadLimits {
                header_bytes: 64 * 1024 * 1024,
                retained_string_bytes: 4 * 1024 * 1024,
                retained_values: 32768,
                kv_count: 4096,
                string_bytes: 65536,
                key_bytes: 4096,
            },
        );
        if header.refused {
            return Err(Error::Refused);
        }
        let (summary, rope_text) = match raw {
            Ok(raw) => {
                if !raw.values().all(finite) {
                    return Err(Error::Refused);
                }
                let summary = gguf::summarize(&raw).map_err(|_| Error::Refused)?;
                let arch = summary.get("arch").ok_or(Error::Refused)?;
                let gguf::Json::Str(arch) = arch else {
                    return Err(Error::Refused);
                };
                budget.charge(arch.len() + 65536 + 64)?;
                let key = format!("{arch}.rope.scaling.type");
                let value = raw.get(&key).unwrap_or(&gguf::Value::None);
                let text =
                    gguf::metadata_text_with_limit(value, 65536).map_err(|_| Error::Refused)?;
                (summary, text)
            }
            Err(gguf::LimitedError::Budget) => return Err(Error::Refused),
            Err(gguf::LimitedError::Metadata(error)) => {
                let text = error.to_string();
                budget.charge(text.len() + 32)?;
                return Ok((json!({}), vec![format!("Could not read GGUF: {text}")]));
            }
        };
        let (mut values, mut hints) = suggest(&summary, &rope_text, budget)?;
        if !model_rel.is_empty() {
            budget.charge(model_rel.len() + 512)?;
            let fields = values.as_object_mut().ok_or(Error::Refused)?;
            fields.insert("model".into(), json!(model_rel));
            let folder = model_rel
                .rsplit_once('/')
                .map(|(folder, _)| folder)
                .ok_or(Error::Refused)?;
            let relative_folder = folder.replacen("/models", "", 1);
            let relative_folder = relative_folder.trim_start_matches('/');
            // Preserve the existing unsorted first companion selection.
            let entries = self.models.entries(relative_folder, budget)?;
            self.fill_first_companion(
                entries,
                relative_folder,
                folder,
                fields,
                &mut hints,
                budget,
            )?;
            let filename = model_rel.rsplit('/').next().ok_or(Error::Refused)?;
            let (_, part, total) = shard(filename)?;
            hints.insert(0,if let Some(total) = total.filter(|t| part.is_some() && *t>1) {
                format!("Sharded model ({total} parts) → `model = {model_rel}` pre-filled to the first shard; llama-server auto-loads the rest.")
            } else { format!("Model lives in a subdir → `model = {model_rel}` pre-filled with the absolute path.") });
        }
        Ok((values, hints))
    }

    fn fill_first_companion(
        &self,
        entries: Vec<super::access::Entry>,
        relative_folder: &str,
        folder: &str,
        fields: &mut serde_json::Map<String, Value>,
        hints: &mut Vec<String>,
        budget: &mut Budget,
    ) -> Result<(), Error> {
        for entry in entries {
            if !entry.directory
                && gguf_suffix(&entry.name)
                && entry.name.to_lowercase().contains("mmproj")
            {
                let relative = format!("{relative_folder}/{}", entry.name);
                if self.models.file(&relative, false, budget)?.is_none() {
                    continue;
                }
                let mmproj = format!("{folder}/{}", entry.name);
                budget.charge(mmproj.len() + 128)?;
                fields.insert("mmproj".into(), json!(mmproj));
                hints.insert(
                    0,
                    format!("Companion mmproj found → `mmproj = {mmproj}` pre-filled."),
                );
                break;
            }
        }
        Ok(())
    }
}

#[cfg(all(test, target_os = "linux"))]
mod companion_order_tests {
    use super::super::access::Entry;
    use super::*;

    #[test]
    fn first_companion_preserves_both_explicit_orders() -> Result<(), std::io::Error> {
        for order in [
            ["a-mmproj.gguf", "z-mmproj.gguf"],
            ["z-mmproj.gguf", "a-mmproj.gguf"],
        ] {
            let dir = tempfile::tempdir()?;
            let models = dir.path().join("models");
            std::fs::create_dir_all(models.join("syn-folder"))?;
            for name in ["a-mmproj.gguf", "z-mmproj.gguf"] {
                std::fs::write(
                    models.join("syn-folder").join(name),
                    b"synthetic guarded file",
                )?;
            }
            let files = Files::new(&models, &models.join("models.ini"))
                .map_err(|error| std::io::Error::other(format!("{error:?}")))?;
            let entries = order
                .iter()
                .map(|name| Entry {
                    name: (*name).to_owned(),
                    directory: false,
                })
                .collect();
            let mut fields = serde_json::Map::new();
            let mut hints = vec!["existing hint".to_owned()];
            files
                .fill_first_companion(
                    entries,
                    "syn-folder",
                    "/models/syn-folder",
                    &mut fields,
                    &mut hints,
                    &mut Budget::default(),
                )
                .map_err(|error| std::io::Error::other(format!("{error:?}")))?;
            let expected = format!(
                "/models/syn-folder/{}",
                order
                    .first()
                    .ok_or_else(|| std::io::Error::other("fixture order"))?
            );
            assert_eq!(
                serde_json::Value::Object(fields),
                json!({"mmproj":expected.as_str()})
            );
            assert_eq!(
                hints,
                vec![
                    format!("Companion mmproj found → `mmproj = {expected}` pre-filled."),
                    "existing hint".to_owned()
                ]
            );
        }
        Ok(())
    }
}
