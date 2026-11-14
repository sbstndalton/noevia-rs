//! `shared-context.cjs` `forCode(project, chats)`: what a Code task is told about its project when
//! the project shares into Code (goal, instructions, memories and its recent chats' titles), framed
//! as untrusted reference data; and `projects.cjs` `loadChats` (`sanitizeChats(project.chats)`),
//! which core evaluates first, whether or not the project shares.

use crate::Thrown;
use chat_turn::js;
use js_json::JValue;

const MAX_CHATS: usize = 8;
const MAX_BLOCK: usize = 6000;

/// `sanitizeChats(project.chats)`: the chat rows that are objects with a text id. `Err` where
/// `(chats || []).filter` throws (a value that is set but not a list).
pub fn load_chats(project: &JValue) -> Result<Vec<JValue>, Thrown> {
    match project.get("chats") {
        JValue::Arr(chats) => Ok(chats
            .iter()
            .filter(|c| matches!(c, JValue::Obj(_)) && c.get("id").as_str().is_some())
            .cloned()
            .collect()),
        other if !other.truthy() => Ok(Vec::new()),
        _ => Err(Thrown),
    }
}

/// `read(project).code`: `project.sharedContext.code === true` on an object.
fn shares_into_code(project: &JValue) -> bool {
    project.get("sharedContext").get("code") == &JValue::Bool(true)
}

/// `String(v)`.
fn string_of(v: &JValue) -> Result<String, Thrown> {
    js_json::to_js_string(v).map_err(|_| Thrown)
}

/// `cap(text, max)`: cut to `max - 1` units plus `…` when longer than `max`.
fn cap(text: &str, max: usize) -> String {
    if js::len(text) > max {
        format!("{}…", js::slice(text, max.saturating_sub(1)))
    } else {
        text.to_string()
    }
}

/// `clip(text, n)`: `String(text || '')` with each run of white space one space, trimmed, and cut
/// by [`cap`].
fn clip(text: &JValue, n: usize) -> Result<String, Thrown> {
    let s = if text.truthy() {
        string_of(text)?
    } else {
        String::new()
    };
    let mut collapsed = String::with_capacity(s.len());
    let mut in_space = false;
    for c in s.chars() {
        if js::is_js_space(c) {
            if !in_space {
                collapsed.push(' ');
            }
            in_space = true;
        } else {
            collapsed.push(c);
            in_space = false;
        }
    }
    Ok(cap(js::trim(&collapsed), n))
}

/// `frameUntrusted(kind, label, text)` (prompt-framing, over code units).
fn frame_untrusted(kind: &str, label: &str, text: &str) -> String {
    js::from_units(&prompt_framing::framing::frame_untrusted(
        &js::to_units(kind),
        &js::to_units(label),
        &js::to_units(text),
    ))
}

/// `framed(kind, label, body)`: the body capped so the framed block stays within [`MAX_BLOCK`].
fn framed(kind: &str, label: &str, body: &str) -> String {
    let frame = js::len(&frame_untrusted(kind, label, ""));
    frame_untrusted(kind, label, &cap(body, MAX_BLOCK.saturating_sub(frame)))
}

/// `(c.updatedAt || 0)` as the sort reads it; see the crate's named difference.
fn updated(c: &JValue) -> f64 {
    match c.get("updatedAt") {
        JValue::Num(n) if n.is_finite() => *n,
        JValue::Bool(true) => 1.0,
        _ => 0.0,
    }
}

/// `forCode(project, chats)`: `''` when sharing into Code is off or there is nothing to say.
pub fn for_code(project: &JValue, chats: &[JValue]) -> Result<String, Thrown> {
    if !shares_into_code(project) {
        return Ok(String::new());
    }
    let mut parts = Vec::new();
    if project.get("goal").truthy() {
        parts.push(format!("Project goal: {}", clip(project.get("goal"), 600)?));
    }
    let instructions = project.get("instructions");
    if instructions.truthy() {
        parts.push(format!(
            "Project instructions:\n{}",
            js::slice(&string_of(instructions)?, 3000)
        ));
    }
    let memories: Vec<&str> = match project.get("memories") {
        JValue::Arr(items) => items
            .iter()
            .filter_map(JValue::as_str)
            .filter(|m| !js::trim(m).is_empty())
            .take(20)
            .collect(),
        other if !other.truthy() => Vec::new(),
        _ => return Err(Thrown),
    };
    if !memories.is_empty() {
        let mut lines = Vec::with_capacity(memories.len());
        for m in memories {
            lines.push(format!("- {}", clip(&JValue::Str(m.to_string()), 300)?));
        }
        parts.push(format!("Project memories:\n{}", lines.join("\n")));
    }
    let mut recent: Vec<&JValue> = chats
        .iter()
        .filter(|c| c.truthy() && c.get("title").truthy())
        .collect();
    // `sort((a, b) => (b.updatedAt || 0) - (a.updatedAt || 0))`: stable, newest first.
    recent.sort_by(|a, b| {
        updated(b)
            .partial_cmp(&updated(a))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    recent.truncate(MAX_CHATS);
    if !recent.is_empty() {
        let mut lines = Vec::with_capacity(recent.len());
        for c in recent {
            let preview = c.get("preview");
            let tail = if preview.truthy() {
                format!(" — {}", clip(preview, 160)?)
            } else {
                String::new()
            };
            lines.push(format!("- {}{tail}", clip(c.get("title"), 100)?));
        }
        parts.push(format!(
            "Recent chats in this project (titles and last lines, for background):\n{}",
            lines.join("\n")
        ));
    }
    if parts.is_empty() {
        return Ok(String::new());
    }
    Ok(framed(
        "shared project context",
        &clip(project.get("name"), 80)?,
        &parts.join("\n\n"),
    ))
}
