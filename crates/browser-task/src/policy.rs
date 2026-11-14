//! core browser-policy.cjs, the host half: the projections it sends crate browser-policy (over the
//! same JSON call dav-parse.wasm exports), the strict answer on an unknown, a fault or a bad reply
//! (a navigation is refused, any other action asks), the secret values put in only for the
//! placeholders the crate allowed, and `maskSecrets`.

use js_json::{to_js_string, JValue};
use std::collections::BTreeSet;
use std::sync::Mutex;

pub const UNCHECKED_ACTION: &str = "This action could not be double-checked, so it asks first.";
pub const UNCHECKED_ADDRESS: &str = "This address could not be double-checked, so it is not opened.";
pub const UNCHECKED_SECRETS: &str =
    "The secrets in this value could not be double-checked, so nothing was typed.";

/// One secret the executor may type: its name, value and the domains it is bound to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Secret {
    pub name: String,
    pub value: String,
    pub domains: Vec<String>,
}

static WARNED: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());

/// `portWarn`: once per event and reason, with no input.
fn warn(event: &str, reason: &str) {
    let key = format!("{event}:{reason}");
    let mut warned = WARNED.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if warned.insert(key) {
        eprintln!("[browser-policy] {event} ({reason}); the stricter answer was used");
    }
}

fn s(text: &str) -> JValue {
    JValue::Str(text.to_string())
}

fn strings(list: &[String]) -> JValue {
    JValue::Arr(list.iter().map(|d| s(d)).collect())
}

/// The crate's answer to one call, `None` (after a warning) on a refusal or an unreadable reply.
fn ask(op: u8, args: JValue) -> Option<JValue> {
    let Some(body) = js_json::stringify(&args) else {
        warn("browser_policy.wasm_fault", "input");
        return None;
    };
    let mut input = Vec::with_capacity(body.len() + 1);
    input.push(op);
    input.extend_from_slice(body.as_bytes());
    let (status, reply) = browser_policy::call(&input);
    let parsed = std::str::from_utf8(&reply)
        .ok()
        .and_then(|t| js_json::parse(t).ok());
    if status != 0 {
        let reason = parsed
            .as_ref()
            .and_then(|r| r.get("error").as_str().map(str::to_string))
            .unwrap_or_else(|| "unexpected".into());
        warn("browser_policy.wasm_fault", &reason);
        return None;
    }
    if parsed.is_none() {
        warn("browser_policy.wasm_fault", "reply");
    }
    parsed
}

fn keys_are(v: &JValue, keys: &[&str]) -> bool {
    match v {
        JValue::Obj(items) => items.len() == keys.len() && keys.iter().all(|k| items.iter().any(|(n, _)| n == k)),
        _ => false,
    }
}

fn non_empty(v: &JValue) -> bool {
    v.as_str().is_some_and(|t| !t.is_empty())
}

/// `v ? String(v) : ''`.
fn text_of(v: &JValue) -> Option<String> {
    if v.truthy() {
        to_js_string(v).ok()
    } else {
        Some(String::new())
    }
}

/// classifyAction's verdict: `(status, reason)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    pub status: String,
    pub reason: String,
}

/// `classifyAction(facts, { origin, allowedDomains })`: `facts` is the executor's
/// `{ type, url, method, key, element }`.
pub fn classify(kind: &str, action: &JValue, element: &JValue, origin: &str, allowed: &[String]) -> Verdict {
    let projected = (|| {
        let field = |when: bool, v: Option<String>| if when { v.map(JValue::Str) } else { Some(JValue::Null) };
        let url = field(kind == "navigate", to_js_string(action.get("url")).ok())?;
        let method_value = action.get("method");
        let method = field(
            kind == "navigate",
            if method_value.truthy() { to_js_string(method_value).ok() } else { Some("GET".into()) },
        )?;
        let key_value = action.get("key");
        let key = field(
            kind == "press",
            if key_value.is_nullish() { Some(String::new()) } else { to_js_string(key_value).ok() },
        )?;
        let action = JValue::obj([("type", s(kind)), ("url", url), ("method", method), ("key", key)]);
        let mut el = Vec::new();
        for (name, value) in [("tag", "tag"), ("type", "type"), ("role", "role"), ("name", "name"), ("text", "text"), ("value", "value")] {
            el.push((name.to_string(), JValue::Str(text_of(element.get(value))?)));
        }
        el.push(("inForm".to_string(), JValue::Bool(element.get("inForm").truthy())));
        let page = JValue::obj([
            ("origin", s(origin)),
            ("allowedDomains", if kind == "navigate" { strings(allowed) } else { JValue::Arr(Vec::new()) }),
        ]);
        Some(JValue::Arr(vec![action, JValue::Obj(el), page]))
    })();
    let reply = match projected {
        Some(args) => ask(1, args),
        None => {
            warn("browser_policy.wasm_fault", "unexpected");
            None
        }
    };
    let shaped = reply.filter(|r| {
        keys_are(r, &["status", "reason"])
            && r.get("reason").as_str().is_some()
            && match r.get("status") {
                JValue::Null => r.get("reason").as_str() == Some(""),
                JValue::Str(t) => matches!(t.as_str(), "allow" | "needs_approval" | "blocked"),
                _ => false,
            }
    });
    if let Some(r) = &shaped {
        if let Some(status) = r.get("status").as_str() {
            return Verdict {
                status: status.to_string(),
                reason: r.get("reason").as_str().unwrap_or("").to_string(),
            };
        }
        warn("browser_policy.unknown", "classify");
    }
    if kind == "navigate" {
        Verdict { status: "blocked".into(), reason: UNCHECKED_ADDRESS.into() }
    } else {
        Verdict { status: "needs_approval".into(), reason: UNCHECKED_ACTION.into() }
    }
}

/// `checkNavigation(url, allowedDomains)`: `Ok(origin)` or `Err(reason)`.
pub fn navigation(url: &str, allowed: &[String]) -> Result<String, String> {
    let reply = ask(2, JValue::Arr(vec![s(url), strings(allowed)]));
    match reply {
        Some(r) if keys_are(&r, &["ok", "origin"]) && r.get("ok").as_bool() == Some(true) && non_empty(r.get("origin")) => {
            Ok(r.get("origin").as_str().unwrap_or("").to_string())
        }
        Some(r) if keys_are(&r, &["ok", "reason"]) && r.get("ok").as_bool() == Some(false) && non_empty(r.get("reason")) => {
            Err(r.get("reason").as_str().unwrap_or("").to_string())
        }
        Some(_) => {
            warn("browser_policy.wasm_fault", "reply");
            Err(UNCHECKED_ADDRESS.into())
        }
        None => Err(UNCHECKED_ADDRESS.into()),
    }
}

/// `/\{\{secret:([A-Za-z0-9_.-]{1,64})\}\}/g` over `text`: each match's byte range and name.
fn placeholders(text: &str) -> Vec<(usize, usize, String)> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while let Some(at) = text.get(i..).and_then(|rest| rest.find("{{secret:")) {
        let start = i + at;
        let name_start = start + "{{secret:".len();
        let mut end = name_start;
        while end < bytes.len()
            && end - name_start < 65
            && bytes.get(end).is_some_and(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
        {
            end += 1;
        }
        let len = end - name_start;
        if (1..=64).contains(&len) && text.get(end..end + 2) == Some("}}") {
            out.push((start, end + 2, text.get(name_start..end).unwrap_or("").to_string()));
            i = end + 2;
        } else {
            i = start + 1;
        }
    }
    out
}

/// A substitution's outcome: the value to type and the names used, or why nothing is typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Substituted {
    Ok { value: String, used: Vec<String> },
    Refused(String),
}

/// `substituteSecrets(text, secrets, origin)`.
pub fn substitute(text: &str, secrets: &[Secret], origin: &str) -> Substituted {
    let valid = |n: &str| (1..=64).contains(&n.len()) && n.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'));
    let projection: Vec<JValue> = secrets
        .iter()
        .filter(|sec| valid(&sec.name))
        .map(|sec| JValue::Arr(vec![s(&sec.name), strings(&sec.domains)]))
        .collect();
    let reply = ask(3, JValue::Arr(vec![s(text), JValue::Arr(projection), s(origin)]));
    let Some(r) = reply else {
        return Substituted::Refused(UNCHECKED_SECRETS.into());
    };
    let used_ok = match r.get("used") {
        JValue::Arr(items) => items.iter().all(non_empty),
        _ => false,
    };
    if keys_are(&r, &["ok", "reason"]) && r.get("ok").as_bool() == Some(false) && non_empty(r.get("reason")) {
        return Substituted::Refused(r.get("reason").as_str().unwrap_or("").to_string());
    }
    if !(keys_are(&r, &["ok", "used"]) && r.get("ok").as_bool() == Some(true) && used_ok) {
        warn("browser_policy.wasm_fault", "reply");
        return Substituted::Refused(UNCHECKED_SECRETS.into());
    }
    let used: Vec<String> = match r.get("used") {
        JValue::Arr(items) => items.iter().filter_map(|u| u.as_str().map(str::to_string)).collect(),
        _ => Vec::new(),
    };
    let found = placeholders(text);
    let value_of = |name: &str| secrets.iter().find(|sec| sec.name == name).map(|sec| sec.value.as_str());
    let known = found.iter().all(|(_, _, n)| value_of(n).is_some());
    if !known || found.len() != used.len() || found.iter().zip(&used).any(|((_, _, n), u)| n != u) {
        warn("browser_policy.impl_mismatch", "used");
        return Substituted::Refused(UNCHECKED_SECRETS.into());
    }
    let mut value = String::new();
    let mut at = 0;
    for (start, end, name) in &found {
        value.push_str(text.get(at..*start).unwrap_or(""));
        value.push_str(value_of(name).unwrap_or(""));
        at = *end;
    }
    value.push_str(text.get(at..).unwrap_or(""));
    Substituted::Ok { value, used }
}

/// `encodeURIComponent`.
fn encode_uri_component(text: &str) -> String {
    let mut out = String::new();
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// `maskSecrets(text, secrets)`: every value of four or more UTF-16 units, longest first, as typed
/// and as it travels in a URL or a form body, replaced by its placeholder.
pub fn mask(text: &str, secrets: &[Secret]) -> String {
    let mut entries: Vec<&Secret> = secrets.iter().filter(|sec| js_json::js_len(&sec.value) >= 4).collect();
    // A stable sort, longest first, as Array.prototype.sort is.
    entries.sort_by_key(|sec| std::cmp::Reverse(js_json::js_len(&sec.value)));
    let mut out = text.to_string();
    for sec in entries {
        let encoded = encode_uri_component(&sec.value);
        let plus = encoded.replace("%20", "+");
        let mut forms: Vec<String> = Vec::new();
        for f in [sec.value.clone(), encoded, plus] {
            if !forms.contains(&f) {
                forms.push(f);
            }
        }
        for form in forms {
            out = out.replace(&form, &format!("{{{{secret:{}}}}}", sec.name));
        }
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn placeholder_scan() {
        let found = placeholders("a{{secret:x}}b{{secret:}}{{secret:y.z-1}}{{secret:bad name}}");
        let names: Vec<&str> = found.iter().map(|(_, _, n)| n.as_str()).collect();
        assert_eq!(names, ["x", "y.z-1"]);
    }

    #[test]
    fn masking() {
        let secrets = [Secret { name: "qa".into(), value: "qa secret".into(), domains: vec![] }];
        assert_eq!(mask("x qa secret y qa%20secret z qa+secret", &secrets), "x {{secret:qa}} y {{secret:qa}} z {{secret:qa}}");
        assert_eq!(mask("abc", &[Secret { name: "s".into(), value: "abc".into(), domains: vec![] }]), "abc");
    }

    #[test]
    fn nothing_typed_without_secrets() {
        assert_eq!(
            substitute("plain text", &[], "https://shop.example.test"),
            Substituted::Ok { value: "plain text".into(), used: vec![] }
        );
        assert!(matches!(substitute("{{secret:qa}}", &[], "https://shop.example.test"), Substituted::Refused(_)));
    }
}
