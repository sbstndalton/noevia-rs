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
/// Secret material stays in native memory and is sent only to the private engine channel.
/// No Debug/serialization implementation can expose it to cards, events, or logs.
pub struct Secret {
    pub name: String,
    pub domains: Vec<String>,
    value: zeroize::Zeroizing<String>,
}
impl Secret {
    pub fn new(name: String, domains: Vec<String>, value: String) -> Self {
        Self {
            name,
            domains,
            value: zeroize::Zeroizing::new(value),
        }
    }
}
pub struct Typed {
    value: zeroize::Zeroizing<String>,
    pub used: Vec<String>,
}
impl Typed {
    pub fn value(&self) -> &str {
        &self.value
    }
}
const UNCHECKED_SECRETS: &str =
    "The secrets in this value could not be double-checked, so nothing was typed.";
fn secret_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
}
fn placeholders(source: &str) -> Vec<(usize, usize, &str)> {
    let mut found = vec![];
    let mut offset = 0;
    while let Some(start) = source[offset..].find("{{secret:") {
        let start = offset + start;
        let name_start = start + 9;
        if let Some(end) = source[name_start..].find("}}") {
            let name_end = name_start + end;
            let name = &source[name_start..name_end];
            if secret_name(name) {
                found.push((start, name_end + 2, name));
                offset = name_end + 2;
                continue;
            }
        }
        offset = name_start;
    }
    found
}
/// Existing policy sees only names and domain bindings, never secret values. Replacement
/// happens only after its ordered used-name list exactly matches the native parser.
pub fn substitute(source: &str, secrets: &[Secret], origin: &str) -> Result<Typed, String> {
    let bindings = JValue::Arr(
        secrets
            .iter()
            .filter(|s| secret_name(&s.name))
            .map(|s| {
                JValue::Arr(vec![
                    string(&s.name),
                    JValue::Arr(s.domains.iter().map(|d| string(d)).collect()),
                ])
            })
            .collect(),
    );
    let answer =
        call(3, vec![string(source), bindings, string(origin)]).ok_or(UNCHECKED_SECRETS)?;
    if answer.get("ok") != &JValue::Bool(true) {
        return Err(answer
            .get("reason")
            .as_str()
            .unwrap_or(UNCHECKED_SECRETS)
            .into());
    }
    let names = placeholders(source);
    let JValue::Arr(used) = answer.get("used") else {
        return Err(UNCHECKED_SECRETS.into());
    };
    if used.len() != names.len()
        || used
            .iter()
            .zip(&names)
            .any(|(v, (_, _, name))| v.as_str() != Some(*name))
    {
        return Err(UNCHECKED_SECRETS.into());
    }
    let mut output = zeroize::Zeroizing::new(String::new());
    let mut offset = 0;
    let mut used_names = vec![];
    for (start, end, name) in names {
        let secret = secrets
            .iter()
            .find(|s| s.name == name)
            .ok_or(UNCHECKED_SECRETS)?;
        output.push_str(&source[offset..start]);
        output.push_str(&secret.value);
        offset = end;
        used_names.push(name.to_owned());
    }
    output.push_str(&source[offset..]);
    Ok(Typed {
        value: output,
        used: used_names,
    })
}
fn component(value: &str) -> String {
    let mut result = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) {
            result.push(char::from(byte));
        } else {
            result.push_str(&format!("%{byte:02X}"));
        }
    }
    result
}
/// Node's longest-first raw/URL/form masking, applied before evidence or approval publication.
pub fn mask(source: &str, secrets: &[Secret]) -> String {
    let mut entries = secrets
        .iter()
        .filter(|s| wtf16::len(&s.value) >= 4)
        .collect::<Vec<_>>();
    entries.sort_by_key(|s| std::cmp::Reverse(wtf16::len(&s.value)));
    let mut output = source.to_owned();
    for secret in entries {
        let encoded = component(&secret.value);
        let form = encoded.replace("%20", "+");
        for value in [secret.value.as_str(), encoded.as_str(), form.as_str()] {
            output = output.replace(value, &format!("{{{{secret:{}}}}}", secret.name));
        }
    }
    output
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
    #[test]
    fn node_secret_binding_and_masking() {
        let fixture =
            wtf16::parse(include_str!("../tests/fixtures/browser-secrets-node.json")).unwrap();
        let JValue::Arr(rows) = fixture else {
            panic!("fixture");
        };
        for row in rows {
            let JValue::Obj(items) = row.get("secrets") else {
                panic!("secrets");
            };
            let secrets = items
                .iter()
                .map(|(name, v)| {
                    Secret::new(
                        name.clone(),
                        match v.get("domains") {
                            JValue::Arr(ds) => ds
                                .iter()
                                .filter_map(|d| d.as_str().map(str::to_owned))
                                .collect(),
                            _ => vec![],
                        },
                        v.get("value").as_str().unwrap().into(),
                    )
                })
                .collect::<Vec<_>>();
            let source = row.get("source").as_str().unwrap();
            let actual = match substitute(source, &secrets, row.get("origin").as_str().unwrap()) {
                Ok(typed) => object(vec![
                    ("ok", JValue::Bool(true)),
                    ("value", string(typed.value())),
                    (
                        "used",
                        JValue::Arr(typed.used.iter().map(|n| string(n)).collect()),
                    ),
                ]),
                Err(reason) => object(vec![
                    ("ok", JValue::Bool(false)),
                    ("reason", string(&reason)),
                ]),
            };
            assert_eq!(&actual, row.get("result"));
            assert_eq!(
                mask(row.get("evidence").as_str().unwrap(), &secrets),
                row.get("masked").as_str().unwrap()
            );
        }
    }
}
