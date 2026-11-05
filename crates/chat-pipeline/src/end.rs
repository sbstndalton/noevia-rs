//! How a turn ends (core chat.cjs after the round loop, and the loop's pauses): the `paused` notes
//! (#666 declined, #658 step supervision, #779 a sensitive tool result held from the cloud), the
//! sources re-sent when the answer drew on a whole injected file (#1793), the fallback text of a
//! reply with no answer (#1693) and the `telemetry` `complete` event.
use crate::rounds;
use crate::sources;
use crate::text::trim;
use js_json::JValue;

fn obj(pairs: Vec<(&str, JValue)>) -> JValue {
    JValue::Obj(pairs.into_iter().map(|(k, v)| (k.to_owned(), v)).collect())
}

fn s(v: &str) -> JValue {
    JValue::Str(v.into())
}

fn saved(n: usize) -> String {
    if n == 1 {
        "1 change was".into()
    } else {
        format!("{n} changes were")
    }
}

fn unique(names: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for n in names {
        if !out.contains(n) {
            out.push(n.clone());
        }
    }
    out
}

/// #666: the person declined a write; the reply ends with this fixed note.
pub fn declined_pause(applied: usize, declined: &[String]) -> JValue {
    let names = unique(declined);
    let joined = names.join(", ");
    let text = if applied > 0 {
        format!(
            "{} saved. You declined {joined}, so nothing else was changed.",
            saved(applied)
        )
    } else {
        format!("No change was made: you declined {joined}.")
    };
    obj(vec![
        ("type", s("paused")),
        ("reason", s("declined")),
        ("applied", JValue::Num(applied as f64)),
        (
            "declined",
            JValue::Arr(names.into_iter().map(JValue::Str).collect()),
        ),
        ("text", JValue::Str(text)),
    ])
}

/// #658: step checks paused the reply after a tool round whose writes are real.
pub fn supervision_pause(applied: usize) -> JValue {
    let text = if applied > 0 {
        format!(
            "{} saved. Step checks paused this reply before any further steps.",
            saved(applied)
        )
    } else {
        "Step checks paused this reply before any further steps. Nothing was changed.".into()
    };
    obj(vec![
        ("type", s("paused")),
        ("reason", s("supervision")),
        ("applied", JValue::Num(applied as f64)),
        ("text", JValue::Str(text)),
    ])
}

/// #779: a tool result that looks sensitive was not sent on to the cloud model.
pub fn sensitive_pause(flag: &JValue, applied: usize) -> JValue {
    obj(vec![
        ("type", s("paused")),
        ("reason", s("sensitive-tool-result")),
        ("flag", flag.clone()),
        ("applied", JValue::Num(applied as f64)),
        ("text", s("A tool result looks sensitive and was not sent to the cloud model. Switch this chat to local or resend.")),
    ])
}

/// What the end of the turn reads.
pub struct Ending<'a> {
    pub compact_only: bool,
    pub paused: bool,
    pub round_has_content: bool,
    pub round_content: &'a str,
    pub round_reasoning: &'a str,
    pub final_pass: bool,
    pub message: &'a JValue,
    pub injected: &'a JValue,
    pub placed: &'a [JValue],
    pub readable_files: &'a JValue,
    /// `replySources.length` so far.
    pub reply_sources: usize,
    pub gathered_sources: &'a [String],
    pub model: &'a JValue,
    pub first_token_ms: Option<f64>,
}

/// The events after the loop, in order (the `sources` re-send, a fallback `delta`, `telemetry`
/// `complete`), and the reply's sources when they were replaced.
pub fn ending(e: &Ending<'_>) -> (Vec<JValue>, Option<Vec<JValue>>) {
    let mut out = Vec::new();
    let mut replaced = None;
    let injected_len = match e.injected {
        JValue::Arr(a) => a.len(),
        _ => 0,
    };
    if !e.compact_only && e.round_has_content && injected_len > 0 {
        let placed = JValue::Arr(e.placed.to_vec());
        let used = sources::used_injected(
            e.injected,
            &placed,
            &JValue::Str(e.round_content.into()),
            e.message,
        );
        if !used.is_empty() {
            let mut all = e.placed.to_vec();
            all.extend(used);
            let list = sources::build_sources(&all, e.readable_files);
            if list.len() > e.reply_sources {
                out.push(obj(vec![
                    ("type", s("sources")),
                    ("sources", JValue::Arr(list.clone())),
                ]));
                replaced = Some(list);
            }
        }
    }
    if !e.paused && !e.round_has_content && e.final_pass {
        out.push(obj(vec![
            ("type", s("delta")),
            (
                "text",
                JValue::Str(rounds::fallback_text(e.gathered_sources)),
            ),
        ]));
    } else if !e.paused && !e.round_has_content && !trim(e.round_reasoning).is_empty() {
        out.push(obj(vec![
            ("type", s("delta")),
            ("text", s("\n\nThe model returned reasoning without a final answer. Try again or choose another model.")),
        ]));
    }
    out.push(obj(vec![
        ("type", s("telemetry")),
        ("phase", s("complete")),
        ("model", e.model.clone()),
        (
            "timeToFirstToken",
            e.first_token_ms
                .map_or(JValue::Null, |ms| JValue::Num(ms / 1000.0)),
        ),
    ]));
    (out, replaced)
}
