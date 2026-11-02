//! #2287: public per-model auto-tune settings policy. The live tune actor supplies facts and is
//! still the sole writer of its job/history/settings file; its conditional primitive has no HTTP policy.
use js_json::{wtf16, JValue};
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    pub model: String,
    pub allow_q5: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    Model,
    Boolean,
    System,
    Configured,
    Busy,
    State,
}
impl Fault {
    pub fn status(self) -> u16 {
        match self {
            Self::Configured => 404,
            Self::Busy => 409,
            Self::State => 503,
            _ => 400,
        }
    }
    pub fn message(self) -> &'static str {
        match self {
        Self::Model => "Choose a model.", Self::Boolean => "allowQ5Kv must be true or false.",
        Self::System => "System routing model — not tuned", Self::Configured => "Choose a configured chat model.",
        Self::Busy => "This model is in an unfinished tune. Let it finish, or start a new tune, before changing this setting.",
        Self::State => "Auto-tune state is unavailable. Try again shortly.",
    }
    }
}
pub fn parse(body: &JValue) -> Result<Change, Fault> {
    let raw = body.get("model");
    let model = if raw.truthy() {
        js_json::to_js_string(raw).map_err(|_| Fault::Model)?
    } else {
        String::new()
    };
    if model.is_empty() || wtf16::len(&model) > 200 {
        return Err(Fault::Model);
    }
    let allow_q5 = body.get("allowQ5Kv").as_bool().ok_or(Fault::Boolean)?;
    Ok(Change { model, allow_q5 })
}
/// The order is Core's: input, system role, configured preset, then an unfinished/resumable item.
pub fn validate(change: &Change, facts: &JValue) -> Result<(), Fault> {
    let guard = facts.get("guard");
    if guard.get("system").as_bool().ok_or(Fault::State)? {
        return Err(Fault::System);
    }
    if !guard.get("configured").as_bool().ok_or(Fault::State)? {
        return Err(Fault::Configured);
    }
    let job = guard.get("job");
    if matches!(
        job.get("status").as_str(),
        Some("running" | "cancelled" | "interrupted" | "failed")
    ) {
        if let JValue::Arr(models) = job.get("models") {
            for model in models {
                if !matches!(model, JValue::Obj(_)) {
                    return Err(Fault::State);
                }
                if (model.get("model").as_str() == Some(change.model.as_str())
                    || model.get("base").as_str() == Some(change.model.as_str()))
                    && model.get("status").as_str() != Some("passed")
                {
                    return Err(Fault::Busy);
                }
            }
        }
    }
    Ok(())
}
pub fn response(change: &Change, facts: &JValue) -> Result<JValue, Fault> {
    let guard = facts.get("guard");
    if guard.get("allowQ5Kv").as_bool() != Some(change.allow_q5) {
        return Err(Fault::State);
    }
    let below = guard.get("allowBelowQ5Kv").as_bool().ok_or(Fault::State)?;
    let mut kv = vec![JValue::from("bf16"), JValue::from("q8_0")];
    if change.allow_q5 {
        kv.extend([JValue::from("q5_1"), JValue::from("q5_0")]);
        if below {
            kv.push(JValue::from("q4_0"));
        }
    }
    for field in ["history", "longHistory"] {
        if matches!(facts.get(field), JValue::Undefined) {
            return Err(Fault::State);
        }
    }
    Ok(JValue::obj([
        ("job", facts.get("job").clone()),
        ("history", facts.get("history").clone()),
        ("kvCandidates", JValue::Arr(kv)),
        (
            "settings",
            JValue::obj([("allowQ5Kv", JValue::Bool(change.allow_q5))]),
        ),
        (
            "modes",
            JValue::obj([
                (
                    "fast",
                    JValue::obj([
                        ("defaultSeconds", JValue::Num(120.0)),
                        ("maxSeconds", JValue::Num(1800.0)),
                    ]),
                ),
                (
                    "long",
                    JValue::obj([
                        ("defaultSeconds", JValue::Num(1800.0)),
                        ("maxSeconds", JValue::Num(1800.0)),
                    ]),
                ),
            ]),
        ),
        ("longId", JValue::Str(format!("{}-long", change.model))),
        ("longHistory", facts.get("longHistory").clone()),
    ]))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    #[test]
    fn validation_and_status_match_real_node_without_running_a_tune() {
        let fixture = wtf16::parse(include_str!(
            "../tests/fixtures/autotune-settings-node.json"
        ))
        .unwrap();
        let JValue::Arr(cases) = fixture else {
            panic!("fixture array");
        };
        for case in cases {
            let checked = parse(case.get("body")).and_then(|change| {
                validate(&change, case.get("facts"))?;
                Ok(change)
            });
            match checked {
                Ok(change) => {
                    assert_eq!(case.get("expected").get("status").as_num(), Some(200.0));
                    let body = response(&change, case.get("finalFacts")).unwrap();
                    assert_eq!(
                        wtf16::stringify(&body),
                        wtf16::stringify(case.get("expected").get("body"))
                    );
                    assert!(!wtf16::stringify(&body).unwrap().contains("never-expose"));
                }
                Err(fault) => {
                    assert_eq!(
                        Some(f64::from(fault.status())),
                        case.get("expected").get("status").as_num()
                    );
                    assert_eq!(
                        Some(fault.message()),
                        case.get("expected").get("body").get("error").as_str()
                    );
                }
            }
        }
    }
    #[test]
    fn malformed_client_coercion_and_ambiguous_actor_facts_fail_closed() {
        assert_eq!(
            parse(&wtf16::parse(r#"{"model":{"toString":null},"allowQ5Kv":true}"#).unwrap()),
            Err(Fault::Model)
        );
        let change = Change {
            model: "synthetic".into(),
            allow_q5: true,
        };
        assert_eq!(validate(&change, &JValue::Null), Err(Fault::State));
        let bad = wtf16::parse(r#"{"guard":{"system":false,"configured":true,"job":{"status":"running","models":[null]}}}"#).unwrap();
        assert_eq!(validate(&change, &bad), Err(Fault::State));
        assert_eq!(response(&change, &JValue::Null), Err(Fault::State));
        let utf16 = JValue::obj([
            ("model", JValue::Str("😀".repeat(100))),
            ("allowQ5Kv", JValue::Bool(true)),
        ]);
        assert!(parse(&utf16).is_ok());
    }
}
