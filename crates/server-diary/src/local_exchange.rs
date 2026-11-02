//! #2284: local exchange response policy. The sidecar remains the only exchange executor.
use js_json::JValue;

const MODEL: &str = "Generating companion response…";
const EARLY: [&str; 3] = [
    "Opening diary context…",
    "Reading diary entries and memory…",
    MODEL,
];
const UNKNOWN: &str = "The diary connection was interrupted. The entry may still be saving; check the saved diary before sending again.";
#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Stage {
    #[default]
    Start,
    Context,
    Model,
    Save,
}
#[derive(Default)]
pub struct StreamPolicy {
    stage: Stage,
    pub completed: bool,
}
fn reason(v: &JValue) -> bool {
    v.as_str().is_some_and(|s| {
        !s.is_empty() && s.len() <= 40 && s.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
    })
}
pub fn not_saved(code: &str) -> JValue {
    let text = match code {
        "storageLoginRejected" => "Diary storage refused the saved login — nothing was saved. Reconnect storage, then resend.",
        "storageThrottled" => "Diary storage is limiting sign-ins right now — nothing was saved. Wait a moment, then resend.",
        "contextFailed" => "Couldn't read your diary — nothing was saved. You can resend.",
        "modelFailed" => "Couldn't reach the local model — nothing was saved. You can resend.",
        _ => "Couldn't reach the Diary companion — nothing was saved. You can resend.",
    };
    JValue::obj([
        ("type", JValue::from("error")),
        ("code", JValue::from(code)),
        ("saved", JValue::Bool(false)),
        ("text", JValue::from(text)),
    ])
}
pub fn interrupted() -> JValue {
    JValue::obj([
        ("type", JValue::from("error")),
        ("text", JValue::from(UNKNOWN)),
    ])
}
impl StreamPolicy {
    /// Preserve unknown frames verbatim in the caller; only recognised JSON events pass here.
    pub fn event(&mut self, input: &JValue) -> JValue {
        if input.get("type").as_str() == Some("status") {
            if let Some(text) = input.get("text").as_str() {
                if !EARLY.contains(&text) {
                    self.stage = Stage::Save;
                } else if self.stage != Stage::Save {
                    if text == MODEL {
                        self.stage = Stage::Model;
                    } else if self.stage == Stage::Start {
                        self.stage = Stage::Context;
                    }
                }
            }
        }
        if input.get("type").as_str() == Some("done") {
            self.completed = true;
        }
        if input.get("type").as_str() != Some("error") {
            return input.clone();
        }
        let mut event = if matches!(input.get("saved"), JValue::Undefined)
            && self.stage != Stage::Start
            && self.stage != Stage::Save
        {
            let model = self.stage == Stage::Model;
            let mut event = not_saved(if model {
                "modelFailed"
            } else {
                "contextFailed"
            });
            if model
                && (matches!(input.get("code"), JValue::Undefined)
                    || input.get("code").as_str() == Some("modelFailed"))
                && reason(input.get("reason"))
            {
                if let JValue::Obj(fields) = &mut event {
                    fields.push(("reason".into(), input.get("reason").clone()));
                }
            }
            event
        } else {
            input.clone()
        };
        let allowed =
            event.get("code").as_str() == Some("modelFailed") && reason(event.get("reason"));
        if !allowed {
            if let JValue::Obj(fields) = &mut event {
                fields.retain(|(k, _)| k != "reason");
            }
        }
        event
    }
}

fn js_number(value: &JValue) -> f64 {
    match value {
        JValue::Num(n) => *n,
        JValue::Null => 0.0,
        JValue::Bool(b) => {
            if *b {
                1.0
            } else {
                0.0
            }
        }
        JValue::Undefined | JValue::Obj(_) => f64::NAN,
        other => {
            let Ok(s) = js_json::to_js_string(other) else {
                return f64::NAN;
            };
            let s = s.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}');
            if s.is_empty() {
                return 0.0;
            }
            for (prefix, radix) in [("0x", 16), ("0b", 2), ("0o", 8)] {
                if s.to_ascii_lowercase().starts_with(prefix) {
                    return s
                        .get(2..)
                        .filter(|s| !s.is_empty())
                        .and_then(|digits| {
                            digits.chars().try_fold(0.0, |n, c| {
                                c.to_digit(radix)
                                    .map(|d| n * f64::from(radix) + f64::from(d))
                            })
                        })
                        .unwrap_or(f64::NAN);
                }
            }
            s.parse().unwrap_or(f64::NAN)
        }
    }
}
fn retry_after(value: &JValue) -> Option<u64> {
    let n = js_number(value);
    (n.is_finite() && n > 0.0).then(|| n.ceil().min(86400.0) as u64)
}
pub fn http_stream_failure(
    status: u16,
    detail: &JValue,
    retry_header: Option<&str>,
) -> Option<JValue> {
    let code = if detail.get("code").as_str() == Some("storageLoginRejected") || status == 424 {
        "storageLoginRejected"
    } else if detail.get("code").as_str() == Some("storageThrottled") {
        "storageThrottled"
    } else if detail.get("code").as_str() == Some("storageUpstream")
        || [400, 401, 403, 404, 410, 424, 428, 429, 503].contains(&status)
    {
        "diaryUnavailable"
    } else {
        return None;
    };
    let mut event = not_saved(code);
    if code == "storageThrottled" {
        let header = retry_header.map_or(JValue::Undefined, JValue::from);
        let value = if matches!(detail.get("retryAfter"), JValue::Null | JValue::Undefined) {
            &header
        } else {
            detail.get("retryAfter")
        };
        if let (Some(wait), JValue::Obj(fields)) = (retry_after(value), &mut event) {
            fields.push(("retryAfter".into(), JValue::Num(wait as f64)));
        }
    }
    Some(event)
}
/// Non-streamed failures intentionally retain only Core's two storage codes and bounded wait.
pub fn json_failure(body: &JValue) -> (JValue, Option<u64>) {
    let error = [body.get("detail"), body.get("error")]
        .into_iter()
        .find(|v| v.truthy())
        .cloned()
        .unwrap_or_else(|| JValue::from("Diary storage request failed"));
    let mut fields = vec![("error".to_owned(), error)];
    let code = body.get("code").as_str();
    let mut wait = None;
    if matches!(code, Some("storageThrottled" | "storageLoginRejected")) {
        fields.push(("code".into(), body.get("code").clone()));
        if code == Some("storageThrottled") {
            wait = retry_after(body.get("retryAfter"));
            if let Some(n) = wait {
                fields.push(("retryAfter".into(), JValue::Num(n as f64)));
            }
        }
    }
    (JValue::Obj(fields), wait)
}
fn spread(fields: &mut Vec<(String, JValue)>, value: &JValue) {
    use js_json::wtf16;
    match value {
        JValue::Obj(items) => {
            for (k, v) in items {
                wtf16::set(fields, k, v.clone());
            }
        }
        JValue::Arr(items) => {
            for (i, v) in items.iter().enumerate() {
                wtf16::set(fields, &i.to_string(), v.clone());
            }
        }
        JValue::Str(s) => {
            let mut i = 0;
            for c in s.chars() {
                let units = if let Some(unit) = wtf16::lone_unit(c) {
                    vec![unit]
                } else {
                    c.to_string().encode_utf16().collect()
                };
                for unit in units {
                    let c = char::from_u32(u32::from(unit)).or_else(|| wtf16::lone_char(unit));
                    if let Some(c) = c {
                        wtf16::set(fields, &i.to_string(), JValue::Str(c.to_string()));
                    }
                    i += 1;
                }
            }
        }
        _ => {}
    }
}
pub struct LegacyEvents {
    pub events: Vec<JValue>,
    pub completed: bool,
}
/// Compatibility with the older companion that answers one JSON object instead of an event stream.
pub fn legacy_events(full: &JValue) -> LegacyEvents {
    let mut events = Vec::new();
    let choice = match full.get("choices") {
        JValue::Arr(a) => a.first().map(|v| v.get("message")),
        _ => None,
    };
    if let Some(choice) = choice.filter(|v| v.truthy()) {
        if choice.get("reasoning_content").truthy() {
            events.push(JValue::obj([
                ("type", JValue::from("reasoning")),
                ("text", choice.get("reasoning_content").clone()),
            ]));
        }
        if !choice.get("content").truthy() {
            return LegacyEvents {
                events,
                completed: false,
            };
        }
        events.push(JValue::obj([
            ("type", JValue::from("answer")),
            ("text", choice.get("content").clone()),
        ]));
        let mut diary = vec![("type".into(), JValue::from("diary"))];
        spread(&mut diary, full.get("diary"));
        events.push(JValue::Obj(diary));
    } else {
        if full.get("reply").as_str().is_none() {
            return LegacyEvents {
                events,
                completed: false,
            };
        }
        if full.get("reasoning").truthy() {
            events.push(JValue::obj([
                ("type", JValue::from("reasoning")),
                ("text", full.get("reasoning").clone()),
            ]));
        }
        events.push(JValue::obj([
            ("type", JValue::from("answer")),
            ("text", full.get("reply").clone()),
        ]));
        let mut diary = vec![("type".into(), JValue::from("diary"))];
        spread(&mut diary, full);
        events.push(JValue::Obj(diary));
    }
    events.push(JValue::obj([("type", JValue::from("done"))]));
    LegacyEvents {
        events,
        completed: true,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use js_json::wtf16;
    fn fixtures() -> JValue {
        wtf16::parse(include_str!("../tests/fixtures/local-exchange-node.json")).unwrap()
    }
    fn array(v: &JValue) -> &[JValue] {
        let JValue::Arr(a) = v else {
            panic!("array");
        };
        a
    }
    #[test]
    fn all_stream_stages_http_refusals_and_legacy_replies_match_real_node() {
        for case in array(fixtures().get("streams")) {
            let mut out = vec![JValue::obj([
                ("type", JValue::from("status")),
                ("text", JValue::from("Connecting to diary companion…")),
            ])];
            if matches!(case.get("events"), JValue::Arr(_)) {
                let mut policy = StreamPolicy::default();
                for event in array(case.get("events")) {
                    out.push(policy.event(event));
                }
            } else if !matches!(case.get("json"), JValue::Undefined) {
                let legacy = legacy_events(case.get("json"));
                out.extend(legacy.events);
                if !legacy.completed {
                    out.push(interrupted());
                }
            } else {
                let status = case.get("http").as_num().unwrap() as u16;
                out.push(
                    http_stream_failure(
                        status,
                        case.get("detail"),
                        case.get("retryHeader").as_str(),
                    )
                    .unwrap_or_else(interrupted),
                );
            }
            assert_eq!(
                wtf16::stringify(&JValue::Arr(out)),
                wtf16::stringify(case.get("expected"))
            );
        }
    }
    #[test]
    fn nonstream_error_codes_details_and_number_coercion_match_real_node() {
        for case in array(fixtures().get("failures")) {
            let (body, wait) = json_failure(case.get("body"));
            assert_eq!(
                wtf16::stringify(&body),
                wtf16::stringify(case.get("expected").get("body"))
            );
            let expected_wait = case
                .get("expected")
                .get("headers")
                .get("Retry-After")
                .as_str()
                .map(|s| s.parse::<u64>().unwrap());
            assert_eq!(wait, expected_wait);
        }
    }
}
