//! core chat-framing.cjs `normalizeFrame(frame)`: a chat's frame (#737) in its canonical shape,
//! or `None` when the value is not a frame at all. Strings are JS strings ([`js_json::wtf16`]):
//! lengths and cuts are UTF-16 units.

use crate::js::{is_space, trim};
use js_json::wtf16;
use js_json::JValue;
use std::collections::HashSet;

/// KINDS.
pub const KINDS: &[&str] = &["search", "action", "idea", "question", "code"];
const MAX_TAGS: usize = 20;
const MAX_LINKS: usize = 20;
const MAX_TAG_CHARS: usize = 64;
const MAX_ID_CHARS: usize = 128;

/// `cleanTag(t)`: trimmed, leading `#`s dropped, each run of whitespace one `-`, 64 units.
fn clean_tag(t: &JValue) -> String {
    let Some(s) = t.as_str() else {
        return String::new();
    };
    let s = trim(s).trim_start_matches('#');
    let mut out = String::with_capacity(s.len());
    let mut in_space = false;
    for c in s.chars() {
        if is_space(c) {
            if !in_space {
                out.push('-');
            }
            in_space = true;
        } else {
            out.push(c);
            in_space = false;
        }
    }
    wtf16::slice(&out, MAX_TAG_CHARS)
}

/// `cleanId(id)`: a non-empty string of at most 128 units, else null.
fn clean_id(id: &JValue) -> Option<String> {
    id.as_str()
        .filter(|s| !s.is_empty() && wtf16::len(s) <= MAX_ID_CHARS)
        .map(str::to_string)
}

/// `[...new Set(items)].slice(0, cap)` over the non-empty values.
fn unique(items: impl Iterator<Item = String>, cap: usize) -> Vec<JValue> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for s in items {
        if out.len() == cap {
            break;
        }
        if !s.is_empty() && seen.insert(s.clone()) {
            out.push(JValue::Str(s));
        }
    }
    out
}

/// `normalizeFrame(frame)`.
pub fn normalize(frame: &JValue) -> Option<JValue> {
    if !frame.is_object() {
        return None;
    }
    let kind = frame.get("kind").as_str().filter(|k| KINDS.contains(k))?;
    let list = |key: &str| match frame.get(key) {
        JValue::Arr(items) => items.as_slice(),
        _ => &[],
    };
    let tags = unique(list("tags").iter().map(clean_tag), MAX_TAGS);
    let links = unique(list("links").iter().filter_map(clean_id), MAX_LINKS);
    let source = frame
        .get("source")
        .as_str()
        .filter(|s| matches!(*s, "suggested" | "user"))
        .unwrap_or("suggested");
    Some(JValue::obj([
        (
            "projectId",
            clean_id(frame.get("projectId")).map_or(JValue::Null, JValue::Str),
        ),
        ("kind", JValue::Str(kind.to_string())),
        ("tags", JValue::Arr(tags)),
        ("links", JValue::Arr(links)),
        (
            "confirmed",
            JValue::Bool(frame.get("confirmed") == &JValue::Bool(true)),
        ),
        ("source", JValue::Str(source.to_string())),
    ]))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn norm(text: &str) -> Option<String> {
        normalize(&wtf16::parse(text).unwrap()).map(|v| wtf16::stringify(&v).unwrap())
    }

    #[test]
    fn frames_like_node() {
        assert_eq!(norm("null"), None);
        assert_eq!(norm("[]"), None);
        assert_eq!(norm(r#"{"kind":"nope"}"#), None);
        assert_eq!(
            norm(r##"{"kind":"idea","tags":[" ##a  b　c ","a-b-c",7,"",null,"#"],"links":["x","x","",5],"projectId":"","confirmed":1,"source":"user"}"##).unwrap(),
            r#"{"projectId":null,"kind":"idea","tags":["a-b-c"],"links":["x"],"confirmed":false,"source":"user"}"#
        );
        let long = "x".repeat(129);
        assert_eq!(
            norm(&format!(
                r#"{{"kind":"code","projectId":"{long}","links":["{long}"],"source":"other"}}"#
            ))
            .unwrap(),
            r#"{"projectId":null,"kind":"code","tags":[],"links":[],"confirmed":false,"source":"suggested"}"#
        );
        // 64 units: an emoji across the cut keeps its high surrogate, as in JS.
        let tag = format!("{}😀", "t".repeat(63));
        assert_eq!(
            norm(&format!(r#"{{"kind":"search","tags":["{tag}"]}}"#)).unwrap(),
            format!(
                r#"{{"projectId":null,"kind":"search","tags":["{}\ud83d"],"links":[],"confirmed":false,"source":"suggested"}}"#,
                "t".repeat(63)
            )
        );
        let many: Vec<String> = (0..30).map(|i| format!("\"t{i}\"")).collect();
        let v = wtf16::parse(
            &norm(&format!(
                r#"{{"kind":"action","tags":[{}]}}"#,
                many.join(",")
            ))
            .unwrap(),
        )
        .unwrap();
        let JValue::Arr(tags) = v.get("tags") else {
            unreachable!()
        };
        assert_eq!(tags.len(), 20);
    }
}
