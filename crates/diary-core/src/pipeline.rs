//! The log pipeline: whether and how one exchange is saved (port of `diary/agent/pipeline.py`).
//!
//! 1. skip classifier (aux model): LOG or SKIP; a failure keeps the entry.
//! 2. summariser (aux model): the reply as third-person prose; a failure keeps the reply verbatim.
//! 3. the durable write (the caller's journal and storage).
//! 4. standing-section maintenance (gate then edit, aux model): opportunistic, never affects the save.
//!
//! The aux calls and the write go through traits; their deadlines (`AUX_STEP_DEADLINE_S` per call,
//! the exchange budget for steps 1-2, a fresh aux-step budget for step 4) belong to the caller's
//! [`Aux`] implementation, which reports which budget a call runs under.

use serde_json::{json, Map, Value};

use crate::context::is_python_space;

pub const THINKING_MODEL_HINT: &str = "Output must be the final answer only. Do not show reasoning; do not wrap the answer in quotes or code fences.";

/// Which deadline an aux call runs under (Python passes the exchange budget, or a fresh one).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuxBudget {
    /// Steps 1 and 2 share the exchange's budget.
    Exchange,
    /// Step 4 gets its own aux-step budget after the save.
    Maintenance,
}

/// One aux model call: (system prompt, user prompt) → the reply text, or a failure of any kind
/// (model error, deadline). Implementations send temperature 0.1, max_tokens 2048, no stop.
pub trait Aux {
    fn call(&mut self, budget: AuxBudget, system: &str, prompt: &str) -> Result<String, ()>;
}

/// The corpus side of a save.
pub trait Journal {
    /// The durable write; the exchange id. `Err(true)` is a native write that may already hold a
    /// pending intent (Python `NativeWriteUnavailable`, re-raised); `Err(false)` any other failure,
    /// with a message for the outcome.
    fn log_exchange(
        &mut self,
        day: &str,
        sub_header: &str,
        me_text: &str,
        claude_text: &str,
    ) -> Result<String, (bool, String)>;
    fn index_enabled(&self) -> bool;
    fn standing_sections_text(&mut self, max_chars: i64) -> Result<String, ()>;
    /// Python `update_standing_sections`: Ok(Some(id)) when INDEX.md changed.
    fn update_standing_sections(
        &mut self,
        open_questions: &Value,
        timeline: &Value,
        today: &str,
    ) -> Result<Option<String>, ()>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogOutcome {
    /// "logged" | "skipped" | "error"
    pub decision: &'static str,
    pub xid: Option<String>,
    pub reason: String,
}

/// The write may already hold a pending intent: the caller answers 503 and records no history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeWriteUnavailable;

/// Python `str.format(**fields)` for templates that use `{name}` fields and `{{` / `}}` only.
/// Anything else (a format spec, an index, an unknown name, a lone brace) is an error, as it is
/// for the Python call.
pub fn format_template(template: &str, fields: &[(&str, &str)]) -> Result<String, ()> {
    let mut out = String::with_capacity(template.len());
    let mut chars = template.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' if chars.peek() == Some(&'{') => {
                chars.next();
                out.push('{');
            }
            '}' if chars.peek() == Some(&'}') => {
                chars.next();
                out.push('}');
            }
            '}' => return Err(()),
            '{' => {
                let mut name = String::new();
                loop {
                    match chars.next() {
                        Some('}') => break,
                        Some(c) if c.is_alphanumeric() || c == '_' => name.push(c),
                        _ => return Err(()),
                    }
                }
                let (_, value) = fields.iter().find(|(k, _)| *k == name).ok_or(())?;
                out.push_str(value);
            }
            c => out.push(c),
        }
    }
    Ok(out)
}

fn strip(s: &str) -> &str {
    s.trim_matches(is_python_space)
}

/// Python `str.splitlines()[0]` of a non-empty string.
fn first_line(s: &str) -> &str {
    let end = s
        .char_indices()
        .find(|(_, c)| {
            matches!(
                c,
                '\n' | '\r'
                    | '\u{0b}'
                    | '\u{0c}'
                    | '\u{1c}'
                    | '\u{1d}'
                    | '\u{1e}'
                    | '\u{85}'
                    | '\u{2028}'
                    | '\u{2029}'
            )
        })
        .map_or(s.len(), |(i, _)| i);
    s.get(..end).unwrap_or(s)
}

/// The classifier's verdict: true keeps the exchange. Judged by its first token, so leading
/// whitespace or a phrase like "I would not skip this" cannot misfire; anything else keeps it.
pub fn keeps(verdict: &str) -> bool {
    let v = strip(verdict);
    let line = if v.is_empty() { "" } else { first_line(v) };
    let Some(token) = line.split(is_python_space).find(|t| !t.is_empty()) else {
        return true;
    };
    token.trim_matches(|c| ".,:;!".contains(c)).to_uppercase() != "SKIP"
}

/// The summary as saved: stripped, one pair of surrounding double quotes removed.
pub fn summary_prose(out: &str) -> String {
    let prose = strip(out);
    if prose.len() >= 2 && prose.starts_with('"') && prose.ends_with('"') {
        let inner = prose.get(1..prose.len() - 1).unwrap_or("");
        return strip(inner).to_owned();
    }
    prose.to_owned()
}

/// Python `_json_extract`: the first JSON object of a model reply (defensive against prose and
/// fences).
pub fn json_extract(text: &str) -> Result<String, ()> {
    let mut text = strip(text).to_owned();
    if text.starts_with("```") {
        text = text.trim_matches('`').to_owned();
        if text.to_lowercase().starts_with("json") {
            text = text.chars().skip(4).collect();
        }
    }
    let start = text.find('{').ok_or(())?;
    let end = text.rfind('}').ok_or(())?;
    if end <= start {
        return Err(());
    }
    text.get(start..=end).map(str::to_owned).ok_or(())
}

/// Python `data.get(key) or []` on a parsed reply (a non-object reply fails like `AttributeError`).
fn ops<'a>(data: &'a Value, key: &str, empty: &'a Value) -> Result<&'a Value, ()> {
    let obj = data.as_object().ok_or(())?;
    Ok(match obj.get(key) {
        Some(v) if crate::pyconv::truthy(v) => v,
        _ => empty,
    })
}

/// The templates (`config/prompts/logging.md`, a YAML mapping read as JSON).
pub struct Pipeline {
    pub templates: Map<String, Value>,
}

impl Pipeline {
    fn template(&self, key: &str) -> Result<&str, ()> {
        self.templates.get(key).and_then(Value::as_str).ok_or(())
    }

    fn aux(
        &self,
        aux: &mut dyn Aux,
        budget: AuxBudget,
        key: &str,
        fields: &[(&str, &str)],
    ) -> Result<String, ()> {
        let prompt = format_template(self.template(key)?, fields)?;
        aux.call(budget, THINKING_MODEL_HINT, &prompt)
    }

    /// Step 1: true => log.
    pub fn classify(&self, aux: &mut dyn Aux, user_message: &str, assistant_message: &str) -> bool {
        match self.aux(
            aux,
            AuxBudget::Exchange,
            "skip_classifier",
            &[
                ("user_message", user_message),
                ("assistant_message", assistant_message),
            ],
        ) {
            Ok(verdict) => keeps(&verdict),
            Err(()) => true, // a classifier failure must never lose a diary entry
        }
    }

    /// Step 2.
    pub fn summarize(&self, aux: &mut dyn Aux, assistant_message: &str) -> String {
        match self.aux(
            aux,
            AuxBudget::Exchange,
            "summarizer",
            &[("assistant_message", assistant_message)],
        ) {
            Ok(out) => summary_prose(&out),
            Err(()) => strip(assistant_message).to_owned(),
        }
    }

    /// Step 4: gate, then edit INDEX.md standing sections. True when INDEX.md changed.
    pub fn maintain_index(
        &self,
        aux: &mut dyn Aux,
        store: &mut dyn Journal,
        user_message: &str,
        summary: &str,
        today: &str,
    ) -> bool {
        if !store.index_enabled() {
            return false;
        }
        let run = |aux: &mut dyn Aux, store: &mut dyn Journal| -> Result<bool, ()> {
            let current = store.standing_sections_text(4000)?;
            let fields = [
                ("user_message", user_message),
                ("assistant_summary", summary),
                ("current_sections", current.as_str()),
            ];
            let gate = self.aux(aux, AuxBudget::Maintenance, "index_maintenance", &fields)?;
            if !strip(&gate).to_uppercase().starts_with("UPDATE") {
                return Ok(false);
            }
            let fields = [
                ("user_message", user_message),
                ("assistant_summary", summary),
                ("current_sections", current.as_str()),
                ("today", today),
            ];
            let raw = self.aux(aux, AuxBudget::Maintenance, "index_edit", &fields)?;
            let data: Value = serde_json::from_str(&json_extract(&raw)?).map_err(|_| ())?;
            let empty = json!([]);
            let changed = store.update_standing_sections(
                ops(&data, "open_questions", &empty)?,
                ops(&data, "timeline", &empty)?,
                today,
            )?;
            Ok(changed.is_some())
        };
        run(aux, store).unwrap_or(false)
    }

    /// One exchange's save (Python `log_exchange`). `progress` receives the status events.
    #[allow(clippy::too_many_arguments)]
    pub fn log_exchange(
        &self,
        aux: &mut dyn Aux,
        store: &mut dyn Journal,
        user_message: &str,
        assistant_message: &str,
        topic: &str,
        day: &str,
        progress: &mut dyn FnMut(Value),
    ) -> Result<LogOutcome, NativeWriteUnavailable> {
        progress(json!({"type": "status", "text": "Checking whether to save this exchange…"}));
        if !self.classify(aux, user_message, assistant_message) {
            return Ok(LogOutcome {
                decision: "skipped",
                xid: None,
                reason: "meta/administrative or non-substantive".into(),
            });
        }
        progress(json!({"type": "status", "text": "Summarizing the saved diary entry…"}));
        let summary = self.summarize(aux, assistant_message);
        progress(json!({"type": "status", "text": "Saving diary entry…"}));
        let xid = match store.log_exchange(day, topic, user_message, &summary) {
            Ok(xid) => xid,
            Err((true, _)) => return Err(NativeWriteUnavailable),
            Err((false, reason)) => {
                return Ok(LogOutcome {
                    decision: "error",
                    xid: None,
                    reason,
                })
            }
        };
        progress(json!({"type": "status", "text": "Checking diary memory updates…"}));
        self.maintain_index(aux, store, user_message, &summary, day);
        Ok(LogOutcome {
            decision: "logged",
            xid: Some(xid),
            reason: String::new(),
        })
    }
}
