//! `server/shared-context.cjs`, the chat side: [`read`] is `read(project)` and [`for_chat`] is
//! `forChat(project, tasks)`, the system part a project chat gets about the project's recent Code
//! tasks when the project shares into Chat. The tasks are `codeTasksFor(project) || []` (`[]` when
//! it throws), read by the caller from the same workspace.

use crate::sources::string_of;
use crate::{js, Thrown};
use js_json::JValue;

pub const MAX_TASKS: usize = 6;
pub const MAX_BLOCK: usize = 6000;

/// `read(project)`: `{ chat, code }`, each true only when `project.sharedContext[mode] === true`.
pub fn read(project: &JValue) -> (bool, bool) {
    let value = project.get("sharedContext");
    let on = |mode: &str| {
        matches!(value, JValue::Obj(_) | JValue::Arr(_)) && value.get(mode) == &JValue::Bool(true)
    };
    (on("chat"), on("code"))
}

/// `clip(text, n)`: `String(text || '')` with each run of white space one space, trimmed, and cut
/// to `n - 1` units plus `…` when longer than `n`.
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

/// `cap(text, max)`: cut to `max - 1` units plus `…` when longer than `max`.
fn cap(text: &str, max: usize) -> String {
    if js::len(text) > max {
        format!("{}…", js::slice(text, max.saturating_sub(1)))
    } else {
        text.to_string()
    }
}

/// `(t.updatedAt || 0)` as the sort reads it. Code tasks store a number; a non-number is a named
/// difference (the crate doc): Node coerces it with `ToNumber` (and a `NaN` leaves the order to
/// V8's sort), the port sorts it as 0 (`true` as 1).
fn updated(t: &JValue) -> f64 {
    match t.get("updatedAt") {
        JValue::Num(n) if n.is_finite() => *n,
        JValue::Bool(true) => 1.0,
        _ => 0.0,
    }
}

/// `framed(kind, label, body)`: the body capped so the framed block stays within [`MAX_BLOCK`].
fn framed(kind: &str, label: &str, body: &str) -> String {
    let frame = js::len(&js::frame_untrusted(kind, label, ""));
    js::frame_untrusted(kind, label, &cap(body, MAX_BLOCK.saturating_sub(frame)))
}

/// `forChat(project, tasks)`: the six most recently updated tasks that have a `task`, framed as
/// untrusted data; `None` when sharing into Chat is off or no task qualifies.
pub fn for_chat(project: &JValue, tasks: &[JValue]) -> Result<Option<String>, Thrown> {
    if !read(project).0 {
        return Ok(None);
    }
    let mut recent: Vec<&JValue> = tasks
        .iter()
        .filter(|t| t.truthy() && t.get("task").truthy())
        .collect();
    // `sort((a, b) => (b.updatedAt || 0) - (a.updatedAt || 0))`: stable, newest first.
    recent.sort_by(|a, b| {
        updated(b)
            .partial_cmp(&updated(a))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    recent.truncate(MAX_TASKS);
    if recent.is_empty() {
        return Ok(None);
    }
    let mut lines = Vec::with_capacity(recent.len());
    for t in recent {
        let status = t.get("status");
        let mut bits = vec![if status.truthy() {
            string_of(status)?
        } else {
            "unknown".to_string()
        }];
        if t.get("branch").truthy() {
            bits.push(format!("branch {}", string_of(t.get("branch"))?));
        }
        if t.get("error").truthy() {
            bits.push(format!("error: {}", clip(t.get("error"), 160)?));
        }
        lines.push(format!(
            "- \"{}\" ({})",
            clip(t.get("task"), 160)?,
            bits.join(", ")
        ));
    }
    Ok(Some(framed(
        "recent Code tasks",
        "diffs not visible",
        &lines.join("\n"),
    )))
}
