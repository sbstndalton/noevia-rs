//! Host projections for the existing browser-policy authority. Engine DOM facts never
//! acquire approval authority, and unknown policy answers retain the human approval floor.
use js_json::{wtf16, JValue};
fn object(rows: Vec<(&str, JValue)>) -> JValue {
    JValue::Obj(rows.into_iter().map(|(k, v)| (k.into(), v)).collect())
}
fn string(value: &str) -> JValue {
    JValue::Str(value.into())
}
fn text(value: &JValue) -> Option<String> {
    js_json::to_js_string(value).ok()
}
fn truthy_text(value: &JValue) -> Option<String> {
    if value.truthy() {
        text(value)
    } else {
        Some(String::new())
    }
}
fn domains(value: &JValue) -> Option<JValue> {
    if !value.truthy() {
        return Some(JValue::Arr(vec![]));
    }
    let JValue::Arr(rows) = value else {
        return None;
    };
    Some(JValue::Arr(
        rows.iter()
            .map(|v| text(v).map(|s| string(&s)))
            .collect::<Option<Vec<_>>>()?,
    ))
}
fn call(op: u8, args: Vec<JValue>) -> Option<JValue> {
    let mut input = vec![op];
    input.extend_from_slice(wtf16::stringify(&JValue::Arr(args))?.as_bytes());
    let (status, output) = browser_policy::call(&input);
    if status != 0 {
        return None;
    }
    wtf16::parse(std::str::from_utf8(&output).ok()?).ok()
}
const UNCHECKED_ACTION: &str = "This action could not be double-checked, so it asks first.";
const UNCHECKED_ADDRESS: &str = "This address could not be double-checked, so it is not opened.";
/// Classify the exact DOM projection, with Node's coercion and strict fallback.
pub fn classify(action: &JValue, page: &JValue) -> JValue {
    let kind = truthy_text(action.get("type")).unwrap_or_default();
    let projected = (|| {
        let a = object(vec![
            ("type", string(&kind)),
            (
                "url",
                if kind == "navigate" {
                    string(&text(action.get("url"))?)
                } else {
                    JValue::Null
                },
            ),
            (
                "method",
                if kind == "navigate" {
                    let m = action.get("method");
                    string(&if m.truthy() { text(m)? } else { "GET".into() })
                } else {
                    JValue::Null
                },
            ),
            (
                "key",
                if kind == "press" {
                    let k = action.get("key");
                    string(&if matches!(k, JValue::Null | JValue::Undefined) {
                        String::new()
                    } else {
                        text(k)?
                    })
                } else {
                    JValue::Null
                },
            ),
        ]);
        let el = action.get("element");
        let mut fields = vec![];
        for key in ["tag", "type", "role", "name", "text", "value"] {
            fields.push((key, string(&truthy_text(el.get(key))?)));
        }
        fields.push(("inForm", JValue::Bool(el.get("inForm").truthy())));
        let origin = page.get("origin");
        let origin = match origin {
            JValue::Str(_) => origin.clone(),
            _ if origin.truthy() => JValue::Null,
            _ => string(""),
        };
        let p = object(vec![
            ("origin", origin),
            (
                "allowedDomains",
                if kind == "navigate" {
                    domains(page.get("allowedDomains"))?
                } else {
                    JValue::Arr(vec![])
                },
            ),
        ]);
        call(1, vec![a, object(fields), p])
    })();
    if let Some(answer) = projected {
        if matches!(
            answer.get("status").as_str(),
            Some("allow" | "needs_approval" | "blocked")
        ) && answer.get("reason").as_str().is_some()
        {
            return answer;
        }
    }
    object(vec![
        (
            "status",
            string(if kind == "navigate" {
                "blocked"
            } else {
                "needs_approval"
            }),
        ),
        (
            "reason",
            string(if kind == "navigate" {
                UNCHECKED_ADDRESS
            } else {
                UNCHECKED_ACTION
            }),
        ),
    ])
}
/// The navigation authority refuses unknown addresses; it never opens an approval card.
pub fn navigation(url: &JValue, allowed_domains: &JValue) -> JValue {
    let answer = (|| call(2, vec![string(&text(url)?), domains(allowed_domains)?]))();
    answer.unwrap_or_else(|| {
        object(vec![
            ("ok", JValue::Bool(false)),
            ("reason", string(UNCHECKED_ADDRESS)),
        ])
    })
}
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    #[test]
    fn node_browser_host_projections() {
        let fixture =
            wtf16::parse(include_str!("../tests/fixtures/browser-policy-node.json")).unwrap();
        let JValue::Arr(rows) = fixture else {
            panic!("fixture");
        };
        for (n, row) in rows.iter().enumerate() {
            let actual = if row.get("kind").as_str() == Some("navigation") {
                navigation(row.get("url"), row.get("domains"))
            } else {
                classify(row.get("action"), row.get("page"))
            };
            assert_eq!(actual, row.get("result").clone(), "case {n}");
        }
    }
}
