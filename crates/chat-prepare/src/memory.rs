//! `server/account-memory.cjs`: Settings, Personalization, Memory. [`read`] is `read(dir)` given
//! the bytes of `account-memory.json`; [`part`] is `systemPart(settings, projectMemories)`, the
//! memory block of the system message.

use crate::js;
use js_json::JValue;

pub const MAX_ITEMS: usize = 50;
pub const MAX_ITEM_CHARS: usize = 300;

const LEAD: &str = "Things you know about the user (persistent memory, apply silently):";

/// `empty()`: the record of a missing or unreadable file.
pub fn empty() -> JValue {
    JValue::obj([
        ("memories", JValue::Arr(Vec::new())),
        ("useProjectMemories", JValue::from(true)),
        ("updatedAt", JValue::Null),
    ])
}

/// `read(dir)` given the file's bytes (`None`: no file): the string memories, trimmed, cut to 300
/// units, at most 50 of them.
pub fn read(file: Option<&[u8]>) -> JValue {
    let Some(data) = file.and_then(js::parse_file) else {
        return empty();
    };
    if data.is_nullish() {
        return empty(); // data.memories of null throws
    }
    let memories = match data.get("memories") {
        JValue::Arr(items) => items
            .iter()
            .filter_map(JValue::as_str)
            .filter(|m| !js::trim(m).is_empty())
            .map(|m| JValue::Str(js::slice(js::trim(m), MAX_ITEM_CHARS)))
            .take(MAX_ITEMS)
            .collect(),
        _ => Vec::new(),
    };
    let updated = match data.get("updatedAt") {
        JValue::Num(n) if n.is_finite() => JValue::Num(*n),
        _ => JValue::Null,
    };
    JValue::obj([
        ("memories", JValue::Arr(memories)),
        (
            "useProjectMemories",
            JValue::from(data.get("useProjectMemories") != &JValue::Bool(false)),
        ),
        ("updatedAt", updated),
    ])
}

/// `systemPart(settings, projectMemories)`: the account's lines, then the project's own (unless
/// the account turned those off), each once, as `- line`; `None` when there are none. `settings`
/// is a [`read`] record; `project_memories` is `project?.memories` (`Undefined` for a turn with no
/// project; anything but an array adds nothing).
pub fn part(settings: &JValue, project_memories: &JValue) -> Option<String> {
    let mut lines: Vec<&str> = Vec::new();
    let account: Vec<&str> = match settings.get("memories") {
        JValue::Arr(items) => items.iter().filter_map(JValue::as_str).collect(),
        _ => Vec::new(),
    };
    let project: Vec<&str> = match project_memories {
        JValue::Arr(items) if settings.get("useProjectMemories").truthy() => items
            .iter()
            .filter_map(JValue::as_str)
            .filter(|m| !js::trim(m).is_empty())
            .collect(),
        _ => Vec::new(),
    };
    // [...new Set([...settings.memories, ...project])]
    let mut seen = std::collections::HashSet::new();
    for m in account.into_iter().chain(project) {
        if seen.insert(m) {
            lines.push(m);
        }
    }
    if lines.is_empty() {
        return None;
    }
    let body: Vec<String> = lines.iter().map(|m| format!("- {m}")).collect();
    Some(format!("{LEAD}\n{}", body.join("\n")))
}
