//! core instruction-skills.cjs `reconcile(project)`: which of a project's files are tracked as
//! instruction skills (`project.instructionSkills`, keyed by file name). Node runs it on the
//! project `getProject` returns (saving when it changed) and on every project when it saves the
//! list (`saveProjects`), so a Rust write of projects.json does the same to leave Node's bytes.
//!
//! Stricter than Node, as a refusal ([`Unsupported`]): a project, file or `instructionSkills`
//! value of a shape Node never writes (a non-object file, a non-string file name, files that are
//! a string). Where Node itself throws (a `null` file, a non-iterable `files`) the refusal stands
//! for that throw.

use crate::js::is_space;
use js_json::JValue;

/// Node would do something this port does not reproduce, or would throw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Unsupported;

/// `/(?:^|\/)SKILL\.md$/i.test(name)`: case-insensitive for ASCII only (no `u` flag).
fn skill_md_name(name: &str) -> bool {
    let suffix = "skill.md";
    let Some(start) = name.len().checked_sub(suffix.len()) else {
        return false;
    };
    let Some(tail) = name.get(start..) else {
        return false;
    };
    tail.eq_ignore_ascii_case(suffix)
        && (start == 0 || name.as_bytes().get(start - 1) == Some(&b'/'))
}

/// `/^---\r?\n([\s\S]*?)\r?\n---(?:\r?\n|$)/.exec(content)?.[1]`, in one pass.
pub fn front_matter(content: &str) -> Option<&str> {
    let start = if content.starts_with("---\n") {
        4
    } else if content.starts_with("---\r\n") {
        5
    } else {
        return None;
    };
    let b = content.as_bytes();
    let closes = |p: usize| -> bool {
        let at = |i: usize| b.get(i).copied();
        let after_dashes = |q: usize| -> bool {
            at(q) == Some(b'-')
                && at(q + 1) == Some(b'-')
                && at(q + 2) == Some(b'-')
                && match at(q + 3) {
                    None => true,
                    Some(b'\n') => true,
                    Some(b'\r') => at(q + 4) == Some(b'\n'),
                    _ => false,
                }
        };
        match at(p) {
            Some(b'\r') => at(p + 1) == Some(b'\n') && after_dashes(p + 2),
            Some(b'\n') => after_dashes(p + 1),
            _ => false,
        }
    };
    // The lazy group ends at the first position the closing line matches.
    (start..=b.len())
        .find(|&p| closes(p))
        .and_then(|p| content.get(start..p))
}

/// `/^(?:name|description)\s*:/im.test(header)`: at the start of any line (after `\n`, `\r`,
/// U+2028 or U+2029), ASCII case-insensitive.
fn names_a_skill(header: &str) -> bool {
    let mut line_start = true;
    let mut rest = header;
    while !rest.is_empty() {
        if line_start {
            for word in ["name", "description"] {
                let hit = rest
                    .get(..word.len())
                    .is_some_and(|w| w.eq_ignore_ascii_case(word));
                if hit {
                    let after = rest.get(word.len()..).unwrap_or("");
                    if after.trim_start_matches(is_space).starts_with(':') {
                        return true;
                    }
                }
            }
        }
        let Some(c) = rest.chars().next() else { break };
        line_start = matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}');
        rest = rest.get(c.len_utf8()..).unwrap_or("");
    }
    false
}

/// `isFrontMatterSkill(file, header)`.
pub fn is_front_matter_skill(name: &str, content: &str) -> bool {
    skill_md_name(name) || front_matter(content).is_some_and(names_a_skill)
}

/// What `reconcile(project)` does, worked out without changing the project.
#[derive(Debug, Clone, PartialEq)]
pub enum Plan {
    /// An array: Node sets a property JSON.stringify never writes.
    Nothing,
    /// `project.instructionSkills = value`; `changed`: its JSON differs from before.
    Set { value: JValue, changed: bool },
}

/// `reconcile(project)`: updates `project.instructionSkills` and says whether its JSON changed.
pub fn reconcile(project: &mut JValue) -> Result<bool, Unsupported> {
    match plan(project)? {
        Plan::Nothing => Ok(false),
        Plan::Set { value, changed } => {
            crate::store::set(project, "instructionSkills", value);
            Ok(changed)
        }
    }
}

/// [`reconcile`] without the assignment, so a caller can check every project before changing any.
pub fn plan(project: &JValue) -> Result<Plan, Unsupported> {
    let JValue::Obj(fields) = project else {
        // Anything but an array: a TypeError (strict mode) or a value Node never stores.
        return match project {
            JValue::Arr(_) => Ok(Plan::Nothing),
            _ => Err(Unsupported),
        };
    };
    let get = |k: &str| {
        fields
            .iter()
            .find(|(key, _)| key == k)
            .map_or(&JValue::Undefined, |(_, v)| v)
    };
    let current = get("instructionSkills");
    let empty = Vec::new();
    let skills: &Vec<(String, JValue)> = match current {
        JValue::Obj(items) => items,
        v if !v.truthy() => &empty,
        _ => return Err(Unsupported),
    };
    let files: &[JValue] = match get("files") {
        JValue::Arr(items) => items,
        v if !v.truthy() => &[],
        _ => return Err(Unsupported),
    };
    // Hash lookups: a project with many files and skills stays linear.
    let by_name: std::collections::HashMap<&str, &JValue> = skills
        .iter()
        .rev() // a repeated key (none in JSON Node wrote) resolves to its first entry
        .map(|(k, v)| (k.as_str(), v))
        .collect();
    let mut next: Vec<(String, JValue)> = Vec::new();
    let mut placed: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for file in files {
        if !file.is_object() {
            return Err(Unsupported);
        }
        let Some(name) = file.get("name").as_str() else {
            return Err(Unsupported);
        };
        let content = match file.get("content") {
            v if v.truthy() => js_json::to_js_string(v).map_err(|_| Unsupported)?,
            _ => String::new(),
        };
        let existing = by_name.get(name).copied();
        let reviewed = existing.is_some_and(|e| e.get("reviewedHash").truthy());
        if is_front_matter_skill(name, &content) || reviewed {
            let value = match existing {
                Some(e) if e.truthy() => e.clone(),
                _ => JValue::obj([
                    ("enabled", JValue::Bool(false)),
                    ("reviewedHash", JValue::Null),
                ]),
            };
            // next[file.name] = ...: a repeated name keeps its first place.
            match placed.get(name).and_then(|&i| next.get_mut(i)) {
                Some(slot) => slot.1 = value,
                None => {
                    placed.insert(name.to_string(), next.len());
                    next.push((name.to_string(), value));
                }
            }
        }
    }
    js_json::wtf16::order_keys(&mut next);
    // JSON.stringify(project.instructionSkills || {}) against JSON.stringify(next).
    let before = match current {
        JValue::Obj(_) => js_json::wtf16::stringify(current),
        _ => Some("{}".to_string()),
    };
    let value = JValue::Obj(next);
    let changed = before != js_json::wtf16::stringify(&value);
    Ok(Plan::Set { value, changed })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn front_matter_is_the_lazy_regex() {
        assert_eq!(front_matter("---\nname: x\n---\nbody"), Some("name: x"));
        assert_eq!(front_matter("---\r\nname: x\r\n---"), Some("name: x"));
        assert_eq!(front_matter("---\n\n---\n"), Some(""));
        assert_eq!(front_matter("---\n---\n"), None);
        assert_eq!(front_matter("---\na\n----\nb\n---\n"), Some("a\n----\nb"));
        assert_eq!(front_matter("---\na\n---x\n---"), Some("a\n---x"));
        assert_eq!(front_matter("x---\na\n---"), None);
        assert_eq!(front_matter("---\na\r\n---\r"), None);
        assert_eq!(front_matter("---\na\r\n---\rb\n---"), Some("a\r\n---\rb"));
    }

    #[test]
    fn skill_detection() {
        assert!(is_front_matter_skill("docs/Skill.MD", ""));
        assert!(is_front_matter_skill("SKILL.md", ""));
        assert!(!is_front_matter_skill("MYSKILL.md", ""));
        assert!(!is_front_matter_skill("\u{17f}KILL.md", ""));
        assert!(is_front_matter_skill(
            "a.md",
            "---\nx: 1\nDESCRIPTION \u{3000}\n : y\n---\n"
        ));
        assert!(!is_front_matter_skill("a.md", "---\n name: y\n---\n"));
        assert!(is_front_matter_skill("a.md", "---\nx\u{2028}name:\n---"));
        assert!(!is_front_matter_skill("a.md", "name: y"));
    }

    #[test]
    fn reconcile_tracks_skills_like_node() {
        let mut p = js_json::wtf16::parse(
            r#"{"id":"p","files":[{"name":"b/SKILL.md","content":""},{"name":"10","content":"---\nname: n\n---"},{"name":"plain.md","content":"x"},{"name":"kept.md","content":"x"}],"instructionSkills":{"kept.md":{"enabled":true,"reviewedHash":"h"},"gone.md":{"enabled":true,"reviewedHash":"h"}}}"#,
        )
        .unwrap();
        assert_eq!(reconcile(&mut p), Ok(true));
        assert_eq!(
            js_json::wtf16::stringify(p.get("instructionSkills")).unwrap(),
            r#"{"10":{"enabled":false,"reviewedHash":null},"b/SKILL.md":{"enabled":false,"reviewedHash":null},"kept.md":{"enabled":true,"reviewedHash":"h"}}"#
        );
        assert_eq!(reconcile(&mut p), Ok(false));
        let mut bare = js_json::wtf16::parse(r#"{"id":"q"}"#).unwrap();
        assert_eq!(reconcile(&mut bare), Ok(false));
        assert_eq!(
            js_json::wtf16::stringify(&bare).unwrap(),
            r#"{"id":"q","instructionSkills":{}}"#
        );
        for bad in [
            r#"null"#,
            r#"5"#,
            r#"{"files":"abc"}"#,
            r#"{"files":[null]}"#,
            r#"{"files":[{"name":1}]}"#,
            r#"{"instructionSkills":[]}"#,
        ] {
            assert_eq!(
                reconcile(&mut js_json::wtf16::parse(bad).unwrap()),
                Err(Unsupported),
                "{bad}"
            );
        }
        assert_eq!(
            reconcile(&mut js_json::wtf16::parse("[]").unwrap()),
            Ok(false)
        );
    }
}
