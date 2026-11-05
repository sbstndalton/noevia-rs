//! The system notes of a turn (core chat.cjs `withSystemNote`, `withoutSystemNote`, #1553, #1604).
use crate::text::{as_str, get, set, spread};
use js_json::JValue;

/// #1604: appended to the system message of a turn routed to the Code role.
pub const CODE_ROUTE_NOTE: &str = "For commands and code: before answering, check that it does exactly what was asked and nothing more (for example, when files are asked for, directories must not appear in the output). When asked why something can mislead or fail, name the concrete mechanism in one sentence.";
/// #1553: a reply whose chat template cannot take tools.
pub const TOOLS_OFF_MODEL_NOTE: &str = "No tools can run in this reply: this model's chat template cannot use them. If the person asks for a web search, a file, a live lookup or any other tool, say so in one line at the start. Do not write results, sources, excerpts or simulated output for it.";

fn system_text(m: &JValue) -> Option<&str> {
    if as_str(get(m, "role")) == Some("system") {
        as_str(get(m, "content"))
    } else {
        None
    }
}

/// `withSystemNote(messages, note)`: the note joins the first system message with text, or leads as one.
pub fn with_system_note(messages: &[JValue], note: &str) -> Vec<JValue> {
    match messages.iter().position(|m| system_text(m).is_some()) {
        None => {
            let mut out = vec![JValue::Obj(vec![
                ("role".into(), JValue::Str("system".into())),
                ("content".into(), JValue::Str(note.into())),
            ])];
            out.extend(messages.iter().cloned());
            out
        }
        Some(at) => messages
            .iter()
            .enumerate()
            .map(|(i, m)| {
                if i == at {
                    let mut c = spread(m);
                    let was = system_text(m).unwrap_or_default().to_owned();
                    set(&mut c, "content", JValue::Str(format!("{was}\n\n{note}")));
                    c
                } else {
                    m.clone()
                }
            })
            .collect(),
    }
}

/// `withoutSystemNote(messages, note)`: the first `\n\n<note>` out of every system message that holds it.
pub fn without_system_note(messages: &[JValue], note: &str) -> Vec<JValue> {
    if note.is_empty() {
        return messages.to_vec();
    }
    let needle = format!("\n\n{note}");
    messages
        .iter()
        .map(|m| match system_text(m) {
            Some(t) if t.contains(&needle) => {
                let mut c = spread(m);
                set(&mut c, "content", JValue::Str(t.replacen(&needle, "", 1)));
                c
            }
            _ => m.clone(),
        })
        .collect()
}
