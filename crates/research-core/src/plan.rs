//! core research-plan.cjs (spec §3.2): a model proposes 3 to 7 sub-questions as JSON; the user may edit,
//! delete, add or skip them. The plan is task context only: it never adds tools, raises budgets or names
//! other projects.
use context_fit::{js_trim, tokens};
use js_json::{wtf16, JValue};

pub const PLAN_SYSTEM: &str = "You plan a research investigation. Reply with JSON only: {\"subQuestions\": [\"...\", ...]} holding 3 to 7 short, self-contained questions that together answer the user question. No commentary, no tools, no URLs.";
pub const MAX_ITEMS: usize = 7;
pub const REPLY_TOKENS: u64 = 600;
const MAX_CHARS: usize = 200;

/// A refusal with the words to show and its status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Readable {
    pub status: u16,
    pub message: String,
}

fn readable(message: impl Into<String>, status: u16) -> Readable {
    Readable {
        status,
        message: message.into(),
    }
}

/// core `sanitizePlan`: trimmed, single-line, bounded, de-duplicated. `min` 1 is a user's list.
pub fn sanitize_plan(list: &JValue, min: usize) -> Result<Vec<String>, Readable> {
    let JValue::Arr(items) = list else {
        return Err(readable("The plan must be a list of questions.", 400));
    };
    let mut seen = Vec::new();
    let mut out = Vec::new();
    for raw in items {
        let Some(raw) = raw.as_str() else {
            return Err(readable("Each plan item must be text.", 400));
        };
        let q = js_trim(&crate::sources::pack_spaces(raw)).to_owned();
        if q.is_empty() {
            continue;
        }
        if wtf16::len(&q) > MAX_CHARS {
            return Err(readable(
                format!("Keep each question under {MAX_CHARS} characters."),
                400,
            ));
        }
        let key = q.to_lowercase();
        if !seen.contains(&key) {
            seen.push(key);
            out.push(q);
        }
    }
    if out.len() < min {
        return Err(readable(
            if min == 1 {
                "Add at least one question, or skip planning.".to_owned()
            } else {
                format!("The plan needs at least {min} questions.")
            },
            400,
        ));
    }
    if out.len() > MAX_ITEMS {
        return Err(readable(
            format!("A plan holds at most {MAX_ITEMS} questions."),
            400,
        ));
    }
    Ok(out)
}

const UNUSABLE: &str = "The model did not return a usable plan. Try again or skip planning.";

/// core `parsePlan`: the model's reply, a fence or commentary around it allowed.
pub fn parse_plan(text: &str) -> Result<Vec<String>, Readable> {
    let mut raw = js_trim(text);
    // `/^```(?:json)?\s*|\s*```$/g`.
    if let Some(rest) = raw.strip_prefix("```") {
        raw = js_trim_start(rest.strip_prefix("json").unwrap_or(rest));
    }
    if let Some(rest) = raw.strip_suffix("```") {
        raw = js_trim_end(rest);
    }
    let slice = match (raw.find('{'), raw.rfind('}')) {
        (Some(start), Some(end)) if end > start => raw.get(start..=end).unwrap_or(raw),
        _ => raw,
    };
    let Ok(parsed) = wtf16::parse(slice) else {
        return Err(readable(UNUSABLE, 502));
    };
    sanitize_plan(parsed.get("subQuestions"), 3)
}

fn js_trim_start(s: &str) -> &str {
    s.trim_start_matches(is_space)
}

fn js_trim_end(s: &str) -> &str {
    s.trim_end_matches(is_space)
}

fn is_space(c: char) -> bool {
    js_trim(c.encode_utf8(&mut [0u8; 4])).is_empty()
}

/// core `plan(question)`'s checks: the question as it is asked, and the two messages to send.
pub fn plan_messages(question: &str, window_tokens: u64) -> Result<JValue, Readable> {
    let q = js_trim(question);
    if q.is_empty() {
        return Err(readable("Write a research question first.", 400));
    }
    if wtf16::len(q) > 2000 {
        return Err(readable(
            "Keep the research question under 2000 characters.",
            400,
        ));
    }
    let messages = JValue::Arr(vec![message("system", PLAN_SYSTEM), message("user", q)]);
    let need = tokens(&messages) + REPLY_TOKENS;
    if need > window_tokens {
        return Err(readable(
            format!("Planning needs about {need} tokens but the model window is {window_tokens}."),
            400,
        ));
    }
    Ok(messages)
}

/// `{ role, content }`.
pub fn message(role: &str, content: &str) -> JValue {
    JValue::obj([
        ("role", JValue::Str(role.to_owned())),
        ("content", JValue::Str(content.to_owned())),
    ])
}
