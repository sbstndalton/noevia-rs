//! Conversation replay rule for the context assembler (port of `completed_exchanges` in
//! `diary/agent/context.py`, #1505).
//!
//! The model is shown only exchanges that were answered: a user turn is kept together with the
//! reply that followed it. An unanswered turn, a reply with no question before it, and any other
//! role are dropped.

use serde_json::{json, Value};

/// Roles whose turns count as an answer (`REPLY_ROLES` in Python).
const REPLY_ROLES: [&str; 2] = ["assistant", "companion"];

/// Python `str.strip()` treats `\x1c`..`\x1f` as whitespace, Rust `char::is_whitespace` does not.
fn is_python_space(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

/// True when `value` is a string with at least one non-whitespace character (Python
/// `isinstance(content, str) and content.strip()`).
fn has_text(value: Option<&Value>) -> bool {
    value
        .and_then(Value::as_str)
        .is_some_and(|text| !text.trim_matches(is_python_space).is_empty())
}

/// Keep only the answered exchanges, in order. Each kept pair is `{role: "user", content}` then
/// `{role, content}` with the reply's own role. Non-object turns are skipped. Callers pass an empty
/// slice for a missing conversation (Python `turns or []`).
pub fn completed_exchanges(turns: &[Value]) -> Vec<Value> {
    let mut out = Vec::new();
    // Index of the user turn waiting for its reply, if it has usable text.
    let mut pending: Option<&Value> = None;
    for turn in turns {
        if !turn.is_object() {
            continue;
        }
        let role = turn.get("role").and_then(Value::as_str);
        let content = turn.get("content");
        match role {
            Some("user") => {
                pending = if has_text(content) { Some(turn) } else { None };
            }
            Some(role) if REPLY_ROLES.contains(&role) => {
                if let Some(question) = pending {
                    if has_text(content) {
                        out.push(json!({"role": "user", "content": question.get("content")}));
                        out.push(json!({"role": role, "content": content}));
                    }
                }
                pending = None;
            }
            _ => {}
        }
    }
    out
}
