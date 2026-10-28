//! Existing text sources through the original fallback framing and bounded read tool.
//! Vector retrieval and PDF/skill-specific readers are separate, not simulated here.
use crate::{Control, Fault, Result};
use serde_json::{json, Value};
use std::path::Path;
fn quote(name: &str) -> Result<String> {
    Ok(serde_json::to_string(name)?.chars().map(|c|if matches!(c,'\u{0085}'|'\u{200e}'|'\u{200f}'|'\u{2028}'..='\u{202e}'|'\u{2066}'..='\u{2069}') { format!("\\u{:04x}",c as u32) } else { c.to_string() }).collect())
}
fn content(data: &Path, user: &str, project: &Value, file: &Value) -> Result<String> {
    let dir = server_projects::store::user_dir(data, user).map_err(|_| Fault::Forbidden)?;
    let id = project
        .get("id")
        .and_then(Value::as_str)
        .ok_or(Fault::Invalid)?;
    let f = js_json::wtf16::parse(&file.to_string()).map_err(|_| Fault::Invalid)?;
    let text = server_projects::sources::text(&dir, id, &f).map_err(|_| Fault::Missing)?;
    let name = file
        .get("name")
        .and_then(Value::as_str)
        .ok_or(Fault::Invalid)?;
    if server_projects::skills::is_front_matter_skill(name, &text) {
        return Err(Fault::Unavailable);
    }
    Ok(text)
}
fn frame(kind: &str, name: &str, text: &str) -> Result<String> {
    let mut input = Vec::new();
    for value in [kind, name] {
        let units: Vec<u16> = value.encode_utf16().collect();
        input.extend_from_slice(
            &u32::try_from(units.len())
                .map_err(|_| Fault::Capacity)?
                .to_le_bytes(),
        );
        input.extend_from_slice(&prompt_framing::to_le(&units));
    }
    input.extend_from_slice(&prompt_framing::to_le(
        &text.encode_utf16().collect::<Vec<_>>(),
    ));
    let output = prompt_framing::frame(&input).map_err(|_| Fault::Invalid)?;
    if output.len() % 2 != 0 {
        return Err(Fault::Protocol);
    }
    let units = output
        .chunks_exact(2)
        .map(|b| {
            u16::from_le_bytes([
                b.first().copied().unwrap_or(0),
                b.get(1).copied().unwrap_or(0),
            ])
        })
        .collect::<Vec<_>>();
    String::from_utf16(&units).map_err(|_| Fault::Invalid)
}
pub fn supported(project: &Value) -> bool {
    project
        .get("files")
        .and_then(Value::as_array)
        .is_none_or(|files| {
            files.iter().all(|f| {
                f.get("name").is_some_and(Value::is_string)
                    && !f
                        .get("skillHeader")
                        .is_some_and(|v| v == &Value::Bool(true))
                    && f.get("document").is_none_or(Value::is_null)
                    && f.get("attachment").is_none_or(Value::is_null)
            })
        })
        && project
            .get("instructionSkills")
            .and_then(Value::as_array)
            .is_none_or(Vec::is_empty)
}
pub fn fallback(
    data: &Path,
    user: &str,
    project: &Value,
    control: &Control,
) -> Result<Option<String>> {
    let Some(files) = project
        .get("files")
        .and_then(Value::as_array)
        .filter(|f| !f.is_empty())
    else {
        return Ok(None);
    };
    if !supported(project) || files.len() > 5000 {
        return Err(Fault::Unavailable);
    }
    let names = files
        .iter()
        .map(|f| quote(f.get("name").and_then(Value::as_str).unwrap_or("")))
        .collect::<Result<Vec<_>>>()?;
    let mut manifest=format!("Sources attached to this project ({}): {}. Excerpts of the relevant ones follow; ask to read a file in full if you need more of it.",files.len(),names.join(", "));
    let mut candidates = Vec::new();
    let mut total = 0usize;
    for file in files {
        control.check()?;
        let name = file
            .get("name")
            .and_then(Value::as_str)
            .ok_or(Fault::Invalid)?;
        let text = content(data, user, project, file)?;
        total = total.checked_add(text.len()).ok_or(Fault::Capacity)?;
        if total > 64 * 1024 * 1024 {
            return Err(Fault::Capacity);
        }
        let large = text.encode_utf16().count() > 2400;
        let clipped = String::from_utf16_lossy(
            &text
                .encode_utf16()
                .take(if large { 24000 } else { 2400 })
                .collect::<Vec<_>>(),
        );
        candidates.push((
            name,
            frame(if large { "file excerpts" } else { "file" }, name, &clipped)?,
        ));
    }
    candidates.sort_by_key(|(_, text)| text.encode_utf16().count());
    let mut budget = std::env::var("FILES_CONTEXT_MAX_CHARS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(120000)
        .clamp(4000, 2000000);
    let mut parts = Vec::new();
    let mut omitted = Vec::new();
    for (name, text) in candidates {
        let len = text.encode_utf16().count();
        if len <= budget {
            budget -= len;
            parts.push(text)
        } else {
            omitted.push(quote(name)?)
        }
    }
    if !omitted.is_empty() {
        manifest.push_str(&format!(
            " {} more file{} not included (prompt size limit): {}.",
            omitted.len(),
            if omitted.len() == 1 { "" } else { "s" },
            omitted.join(", ")
        ));
    }
    let coverage="Source completeness: Legacy text sources have no page completeness metadata.\nContext may contain excerpts only. Use read_project_file to read the attached text. Do not treat missing excerpts as evidence of absence.";
    Ok(Some(format!(
        "{manifest}\n\n{coverage}\n\n{}",
        parts.join("\n\n")
    )))
}
pub fn read(
    data: &Path,
    user: &str,
    project: &Value,
    args: &Value,
    control: &Control,
) -> Result<Value> {
    control.check()?;
    if !supported(project) {
        return Err(Fault::Unavailable);
    }
    let files = project
        .get("files")
        .and_then(Value::as_array)
        .ok_or(Fault::Missing)?;
    let names = files
        .iter()
        .map(|f| {
            f.get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .encode_utf16()
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    let raw = args
        .get("name")
        .and_then(Value::as_str)
        .ok_or(Fault::Invalid)?
        .encode_utf16()
        .collect::<Vec<_>>();
    let resolved = project_file_names::resolve(&names, &raw).map_err(|_| Fault::Invalid)?;
    let project_file_names::Resolved::File(at) = resolved else {
        return Err(match resolved {
            project_file_names::Resolved::Missing => Fault::Missing,
            _ => Fault::Invalid,
        });
    };
    let file = files.get(at).ok_or(Fault::Protocol)?;
    let text = content(data, user, project, file)?;
    let len = text.encode_utf16().count();
    let clipped = String::from_utf16_lossy(&text.encode_utf16().take(8000).collect::<Vec<_>>());
    Ok(json!(format!(
        "File \"{}\" ({len} chars):\n\n{clipped}{}",
        file.get("name")
            .and_then(Value::as_str)
            .ok_or(Fault::Invalid)?,
        if len > 8000 { "\n…[truncated]" } else { "" }
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn existing_text_framing_resolver_and_refusals(
    ) -> std::result::Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let project = json!({"id":"p","files":[{"name":"docs/note.md","content":"source evidence"},{"name":"other/note.md","content":"other evidence"}]});
        let control = Control::new(std::time::Duration::from_secs(5))?;
        let context = fallback(
            dir.path(),
            "11111111-2222-4333-8444-555555555555",
            &project,
            &control,
        )?
        .ok_or("context")?;
        assert!(context.contains("source evidence"));
        assert!(context.contains("untrusted"));
        assert!(read(
            dir.path(),
            "11111111-2222-4333-8444-555555555555",
            &project,
            &json!({"name":"docs/note.md"}),
            &control
        )?
        .as_str()
        .ok_or("text")?
        .contains("source evidence"));
        assert_eq!(
            read(
                dir.path(),
                "11111111-2222-4333-8444-555555555555",
                &project,
                &json!({"name":"note.md"}),
                &control
            ),
            Err(Fault::Invalid)
        );
        assert_eq!(
            read(
                dir.path(),
                "11111111-2222-4333-8444-555555555555",
                &project,
                &json!({"name":"../note.md"}),
                &control
            ),
            Err(Fault::Invalid)
        );
        assert_eq!(
            read(
                dir.path(),
                "11111111-2222-4333-8444-555555555555",
                &project,
                &json!({"name":"absent.md"}),
                &control
            ),
            Err(Fault::Missing)
        );
        let missing = json!({"id":"p","files":[{"name":"missing.md","textRef":"a".repeat(64)}]});
        assert_eq!(
            fallback(
                dir.path(),
                "11111111-2222-4333-8444-555555555555",
                &missing,
                &control
            ),
            Err(Fault::Missing)
        );
        control.cancel();
        assert_eq!(
            fallback(
                dir.path(),
                "11111111-2222-4333-8444-555555555555",
                &project,
                &control
            ),
            Err(Fault::Cancelled)
        );
        Ok(())
    }
}
