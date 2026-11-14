//! Every person-facing line of the transport, and the upstream error reading (`knownError`,
//! `errorObject`, `errorText`). The provider's display name, the backend's name and the lines that
//! name the sign-in itself are the caller's ([`Texts`]); the sentences around them are core's.

use crate::js;
use js_json::{js_slice, JValue};

/// The caller's names and the lines core keeps as whole constants (`MESSAGES`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Texts {
    /// The provider's display name, as in "{label} returned 500".
    pub label: String,
    /// `MESSAGES.disconnected`.
    pub disconnected: String,
    /// `MESSAGES.reconnect`.
    pub reconnect: String,
    /// `MESSAGES.transient`.
    pub transient: String,
    /// The `usage_not_included` line (it names the plan, the backend and the sign-in).
    pub not_included: String,
    /// The `source` of an error frame inside a stream.
    pub source: String,
}

impl Texts {
    pub fn only_chat(&self) -> String {
        format!("Only chat completions are available through {}.", self.label)
    }
    pub fn not_json(&self) -> String {
        "The chat request was not valid JSON.".to_string()
    }
    pub fn choose_model(&self) -> String {
        format!("Choose a {} model in the model popup.", self.label)
    }
    pub fn not_on_account(&self, model80: &str, first8: &str) -> String {
        format!("{model80} is not available on your {} account. Choose one of: {first8}.", self.label)
    }
    pub fn effort_not_offered(&self, shown: &str, model80: &str, offered: &str) -> String {
        format!("{} does not offer \"{shown}\" reasoning for {model80}. It offers {offered}, or Default.", self.label)
    }
    pub fn effort_not_listed(&self, shown: &str, model80: &str) -> String {
        format!("\"{shown}\" reasoning is not in your account's list for {model80}. Choose Default, Low, Medium or High.")
    }
    pub fn list_unreadable(&self, shown: &str, model80: &str) -> String {
        format!(
            "{}'s model list could not be read, so \"{shown}\" reasoning cannot be checked for {model80}. Choose Default, Low, Medium or High.",
            self.label
        )
    }
    pub fn usage_limit(&self, plan: &str, when: &str) -> String {
        format!("{} usage limit reached{plan}.{when} Pick another provider in the model popup until then.", self.label)
    }
    pub fn no_quota(&self) -> String {
        format!("Your {} account has no usage quota left.", self.label)
    }
    pub fn could_not_finish(&self) -> String {
        format!("{} could not finish this reply.", self.label)
    }
    pub fn returned_error(&self) -> String {
        format!("{} returned an error.", self.label)
    }
    pub fn ended_early(&self) -> String {
        format!("{} ended the reply early.", self.label)
    }
    pub fn stream_failed(&self, why: &str) -> String {
        format!("{} stream failed: {}", self.label, js_slice(why, 200))
    }

    /// `knownError(e, nowMs)`: a person-readable line for a known upstream error kind.
    pub fn known_error(&self, e: &JValue, now_ms: f64) -> Option<String> {
        if !e.is_object() {
            return None;
        }
        let empty = JValue::Str(String::new());
        let kind = js::to_string(first_truthy(&[e.get("type"), e.get("code")]).unwrap_or(&empty));
        let message = e.get("message").as_str().or_else(|| e.get("detail").as_str()).unwrap_or("");
        if kind == "usage_limit_reached" || contains_ci(message, "usage limit") {
            let resets_at = js::to_number(e.get("resets_at"));
            let resets_in = js::to_number(e.get("resets_in_seconds"));
            let at = if resets_at > 0.0 {
                Some(resets_at * 1000.0)
            } else if resets_in > 0.0 {
                Some(now_ms + resets_in * 1000.0)
            } else {
                None
            };
            let plan = e
                .get("plan_type")
                .as_str()
                .filter(|p| (1..=20).contains(&p.len()) && p.bytes().all(|b| b.is_ascii_alphabetic() || b == b'_'))
                .map(|p| {
                    let mut c = p.chars();
                    let head = c.next().map(|h| h.to_ascii_uppercase().to_string()).unwrap_or_default();
                    format!(" on your {head}{} plan", c.as_str())
                })
                .unwrap_or_default();
            // Node throws here (RangeError: Invalid time value) for a date past JS's range, which ends
            // the reply as a stream failure; the port leaves the time out instead (noevia bug, see
            // the crate's report).
            let when = at
                .and_then(js::iso_string)
                .map(|iso| format!(" It resets at {} UTC.", js_slice(&iso, 16).replacen('T', " ", 1)))
                .unwrap_or_default();
            return Some(self.usage_limit(&plan, &when));
        }
        if kind == "usage_not_included" {
            return Some(self.not_included.clone());
        }
        if ["insufficient_quota", "credit_balance_exhausted", "organization_usage_limit_exceeded"].contains(&kind.as_str()) {
            return Some(self.no_quota());
        }
        None
    }

    /// `errorText(value, fallback)`.
    pub fn error_text(&self, value: &JValue, fallback: &str, now_ms: f64) -> String {
        let e = error_object(value);
        let text = match e.as_ref().and_then(|e| self.known_error(e, now_ms)) {
            Some(known) => known,
            None => match e.as_ref().map(|e| first_truthy(&[e.get("message"), e.get("detail")])) {
                Some(Some(v)) => js::to_string(v),
                _ => fallback.to_string(),
            },
        };
        js_slice(&text, 300)
    }

    /// `upstreamMessage(text, status)`: the line for a non-2xx upstream answer.
    pub fn upstream_message(&self, text: &str, status: u16, now_ms: f64) -> String {
        let parsed = js_json::parse(text).unwrap_or(JValue::Undefined);
        let e = error_object(&parsed);
        let known = match &e {
            Some(e) => self.known_error(e, now_ms),
            None if contains_ci(text, "usage limit") => {
                self.known_error(&JValue::obj([("type", JValue::from("usage_limit_reached"))]), now_ms)
            }
            None => None,
        };
        if let Some(known) = known {
            return js_slice(&known, 300);
        }
        let detail = e
            .as_ref()
            .map(|e| match (e.get("detail").as_str(), e.get("message").as_str()) {
                (Some(d), _) => d.to_string(),
                (None, Some(m)) => m.to_string(),
                _ => String::new(),
            })
            .unwrap_or_default();
        let detail = if detail.is_empty() { String::new() } else { format!(": {detail}") };
        let line = if status == 429 {
            format!("{} is rate-limiting this account{detail}. Try again shortly.", self.label)
        } else {
            format!("{} returned {status}{detail}", self.label)
        };
        js_slice(&line, 300)
    }
}

/// `a || b || …`: the first truthy value.
fn first_truthy<'a>(values: &[&'a JValue]) -> Option<&'a JValue> {
    values.iter().copied().find(|v| v.truthy())
}

/// `/needle/i.test(hay)` for an ASCII needle.
fn contains_ci(hay: &str, needle: &str) -> bool {
    hay.to_ascii_lowercase().contains(needle)
}

/// `errorObject(value)`: `{ ...value, ...value.error }` when `value.error` is an object, `value`
/// when it is one, otherwise none.
pub fn error_object(value: &JValue) -> Option<JValue> {
    match (value, value.get("error")) {
        (JValue::Obj(outer), JValue::Obj(inner)) => {
            let mut merged = outer.clone();
            for (k, v) in inner {
                assign(&mut merged, k, v.clone());
            }
            Some(JValue::Obj(merged))
        }
        (JValue::Obj(_), _) => Some(value.clone()),
        _ => None,
    }
}

/// `obj[key] = value`: in place when present, appended otherwise (JS property order).
pub fn assign(items: &mut Vec<(String, JValue)>, key: &str, value: JValue) {
    match items.iter_mut().find(|(k, _)| k == key) {
        Some((_, slot)) => *slot = value,
        None => items.push((key.to_string(), value)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts() -> Texts {
        Texts {
            label: "Service".into(),
            disconnected: "d".into(),
            reconnect: "r".into(),
            transient: "t".into(),
            not_included: "n".into(),
            source: "s".into(),
        }
    }

    #[test]
    fn usage_limit_line() {
        let e = js_json::parse(r#"{"type":"usage_limit_reached","plan_type":"plus","resets_at":1800000000}"#).unwrap_or(JValue::Null);
        assert_eq!(
            texts().known_error(&e, 0.0).as_deref(),
            Some("Service usage limit reached on your Plus plan. It resets at 2027-01-15 08:00 UTC. Pick another provider in the model popup until then.")
        );
        // Past the JS date range: Node throws, the port leaves the time out.
        let far = js_json::parse(r#"{"type":"usage_limit_reached","resets_at":1e300}"#).unwrap_or(JValue::Null);
        assert_eq!(
            texts().known_error(&far, 0.0).as_deref(),
            Some("Service usage limit reached. Pick another provider in the model popup until then.")
        );
    }
}
