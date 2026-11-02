//! Public tool-approval decision policy (#2282). Pending chat Promises stay in Node until chat moves;
//! this module owns the JSON shape, JavaScript-compatible decision coercion and write/routing choices.
use js_json::{wtf16, JValue};

pub const ROUTE: &str = "/api/tool-approvals/{approvalId}";
pub const BODY_LIMIT: usize = 1024 * 1024;
pub const MISSING: &str = "no such pending approval";
pub const WRITE_CHOICES: &str = "decision must be 'approve', 'deny' or 'approve_all'";
pub const ROUTE_CHOICES: &str = "decision must be 'cloud' or 'local'";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub value: String,
    pub remember: bool,
}

pub fn parse(bytes: &[u8]) -> Result<JValue, &'static str> {
    let body = wtf16::parse(&String::from_utf8_lossy(bytes)).map_err(|_| "invalid JSON")?;
    if !body.is_object() {
        return Err("request body must be a JSON object");
    }
    Ok(body)
}

pub fn decision(body: &JValue) -> Result<Decision, &'static str> {
    let value = body.get("decision");
    let value = if value.truthy() {
        js_json::to_js_string(value).map_err(|_| "Internal error")?
    } else {
        String::new()
    };
    Ok(Decision {
        value,
        remember: body.get("remember") == &JValue::Bool(true),
    })
}

impl Decision {
    pub fn validate(&self, kind: &str) -> Result<(), &'static str> {
        let valid = match kind {
            "route" => ["cloud", "local"].contains(&self.value.as_str()),
            "write" => ["approve", "deny", "approve_all"].contains(&self.value.as_str()),
            _ => false,
        };
        if valid {
            Ok(())
        } else if kind == "route" {
            Err(ROUTE_CHOICES)
        } else {
            Err(WRITE_CHOICES)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn malformed_and_non_object_bodies_are_refused_before_any_pending_state() {
        for text in ["", "{", "not JSON"] {
            assert_eq!(parse(text.as_bytes()), Err("invalid JSON"));
        }
        for text in ["null", "[]", "1", "true", "\"approve\""] {
            assert_eq!(
                parse(text.as_bytes()),
                Err("request body must be a JSON object")
            );
        }
    }
    #[test]
    fn javascript_coercion_errors_are_preserved_after_owner_lookup() {
        assert_eq!(
            parse(br#"{"decision":{"toString":null}}"#).and_then(|input| decision(&input)),
            Err("Internal error")
        );
    }
    #[test]
    fn write_and_route_choices_are_distinct_and_remember_is_strictly_boolean() {
        for choice in ["approve", "deny", "approve_all"] {
            let d = Decision {
                value: choice.into(),
                remember: true,
            };
            assert_eq!(d.validate("write"), Ok(()));
            assert_eq!(d.validate("route"), Err(ROUTE_CHOICES));
        }
        for choice in ["local", "cloud"] {
            let d = Decision {
                value: choice.into(),
                remember: false,
            };
            assert_eq!(d.validate("route"), Ok(()));
            assert_eq!(d.validate("write"), Err(WRITE_CHOICES));
        }
        assert_eq!(
            parse(br#"{"decision":["approve"],"remember":1}"#).and_then(|v| decision(&v)),
            Ok(Decision {
                value: "approve".into(),
                remember: false
            })
        );
        assert_eq!(
            parse(br#"{"decision":"local","remember":true}"#).and_then(|v| decision(&v)),
            Ok(Decision {
                value: "local".into(),
                remember: true
            })
        );
    }
}
