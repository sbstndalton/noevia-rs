//! What a saved transcript may hold: core chat-sources.cjs `sanitizeHistory` with chat-variants.cjs
//! `capEntry`, value for value (sbstndalton/noevia#1071 slice 4: the transcript store writes every
//! `history-<id>.json`, so the bounds the client cannot be trusted with are applied here).
//!
//! - Each entry's stored alternatives (`answers`/`answer` from Regenerate, `branches`/`branch` from
//!   Edit & re-run, each version's `tail`) keep at most [`MAX_VARIANTS`] per turn, never dropping
//!   the one shown; past [`MAX_DEPTH`] nested versions their alternatives are dropped.
//! - Every item, alternative and tail entry has its client-supplied `sources` (assistant only, at
//!   most [`MAX_SOURCES`], re-shaped), `skill` (user only, the exact pin form) and `failure`
//!   (assistant only, trimmed, at most [`STORED_FAILURE_MAX`] UTF-16 units) bounded by its role.
//!
//! Node returns the same object when nothing needed changing and a rebuilt one otherwise, and that
//! difference shows: a rebuilt item has `sources`/`skill`/`failure` moved to its end, and a rebuilt
//! version list has its `branch` normalised. So every step here reports whether Node would have
//! rebuilt, and acts on it exactly where Node does.
//!
//! `Err` where Node throws: `String(x)` of an object with its own `toString` (a snippet or a file
//! name), which the save route answers as invalid JSON.

use js_json::wtf16;
use js_json::JValue;

/// chat-sources.cjs `MAX_SOURCES`.
pub const MAX_SOURCES: usize = 12;
/// chat-sources.cjs `SNIPPET_MAX`.
pub const SNIPPET_MAX: usize = 200;
const NAME_MAX: usize = 300;
const PAGE_MAX: f64 = 100_000.0;
/// chat-sources.cjs `STORED_FAILURE_MAX`.
pub const STORED_FAILURE_MAX: usize = 1000;
/// chat-variants.cjs `MAX_VARIANTS`.
pub const MAX_VARIANTS: usize = 10;
/// chat-variants.cjs `MAX_DEPTH`.
pub const MAX_DEPTH: usize = 16;

/// Node threw (see the module).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Thrown;

/// JS `WhiteSpace` and `LineTerminator`: what `\s`, `trim` and `trimEnd` take.
fn js_space(c: char) -> bool {
    matches!(
        c,
        '\u{9}'..='\u{d}'
            | ' '
            | '\u{a0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200a}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202f}'
            | '\u{205f}'
            | '\u{3000}'
            | '\u{feff}'
    )
}

fn trim(s: &str) -> &str {
    s.trim_matches(js_space)
}

fn is_object(v: &JValue) -> bool {
    matches!(v, JValue::Obj(_) | JValue::Arr(_))
}

/// `key in v` for a JSON value (none of the keys asked about is inherited).
fn has(v: &JValue, key: &str) -> bool {
    match v {
        JValue::Obj(items) => items.iter().any(|(k, _)| k == key),
        _ => false,
    }
}

/// `{ ...v }` of a value already known to be an object, as its own (key, value) list.
fn spread(v: &JValue) -> Vec<(String, JValue)> {
    match v {
        JValue::Obj(items) => items.clone(),
        // `{ ...[a, b] }` is `{ "0": a, "1": b }` (Node only spreads an array it changed, which
        // never happens to an array entry; kept for completeness).
        JValue::Arr(items) => items
            .iter()
            .enumerate()
            .map(|(i, x)| (i.to_string(), x.clone()))
            .collect(),
        _ => Vec::new(),
    }
}

/// `{ ...obj, key: value }` (an existing key keeps its place). A new key set to `undefined` is
/// left out, as `JSON.stringify` would leave it.
fn with(mut items: Vec<(String, JValue)>, key: &str, value: JValue) -> Vec<(String, JValue)> {
    if matches!(value, JValue::Undefined) && !items.iter().any(|(k, _)| k == key) {
        return items;
    }
    wtf16::set(&mut items, key, value);
    items
}

fn without(v: &JValue, keys: &[&str]) -> Vec<(String, JValue)> {
    spread(v)
        .into_iter()
        .filter(|(k, _)| !keys.contains(&k.as_str()))
        .collect()
}

/// `String(v)` of a value Node coerces.
fn string_of(v: &JValue) -> Result<String, Thrown> {
    js_json::to_js_string(v).map_err(|_| Thrown)
}

/// `String(text || '')`, whitespace runs to one space, trimmed, cut to [`SNIPPET_MAX`] with `…`.
fn snippet_of(text: &JValue) -> Result<String, Thrown> {
    let raw = if text.truthy() {
        string_of(text)?
    } else {
        String::new()
    };
    let mut flat = String::with_capacity(raw.len());
    let mut in_space = false;
    for c in raw.chars() {
        if js_space(c) {
            if !in_space {
                flat.push(' ');
            }
            in_space = true;
        } else {
            flat.push(c);
            in_space = false;
        }
    }
    let flat = trim(&flat);
    Ok(if wtf16::len(flat) > SNIPPET_MAX {
        let mut cut = wtf16::slice(flat, SNIPPET_MAX - 1)
            .trim_end_matches(js_space)
            .to_string();
        cut.push('…');
        cut
    } else {
        flat.to_string()
    })
}

fn valid_page(v: &JValue) -> bool {
    matches!(v, JValue::Num(n) if n.fract() == 0.0 && *n >= 1.0 && *n <= PAGE_MAX)
}

/// `Math.round(x)`: the nearest integer, a half rounding up (towards +∞); `-0` for (-0.5, -0].
fn js_round(x: f64) -> f64 {
    if !x.is_finite() || x.fract() == 0.0 {
        return x;
    }
    let floor = x.floor();
    let r = if x - floor >= 0.5 { floor + 1.0 } else { floor };
    if r == 0.0 && x < 0.0 {
        -0.0
    } else {
        r
    }
}

/// chat-sources.cjs `shape(id, file, body, kind, score, page)`.
fn shape(
    id: &str,
    file: &str,
    body: &JValue,
    kind: &JValue,
    score: &JValue,
    page: &JValue,
) -> Result<JValue, Thrown> {
    let excerpt = kind.as_str() == Some("excerpt");
    let id = if id.is_empty() { file } else { id };
    let mut item = vec![
        ("id".to_string(), JValue::Str(wtf16::slice(id, NAME_MAX))),
        (
            "file".to_string(),
            JValue::Str(wtf16::slice(file, NAME_MAX)),
        ),
        ("snippet".to_string(), JValue::Str(snippet_of(body)?)),
        (
            "kind".to_string(),
            JValue::Str(if excerpt { "excerpt" } else { "file" }.to_string()),
        ),
    ];
    if excerpt && valid_page(page) {
        item.push(("page".to_string(), page.clone()));
    }
    if let JValue::Num(n) = score {
        if n.is_finite() {
            item.push((
                "score".to_string(),
                JValue::Num(js_round(n * 1000.0) / 1000.0),
            ));
        }
    }
    Ok(JValue::Obj(item))
}

/// chat-sources.cjs `sanitizeStored(list)`; `None` for `undefined`.
fn sanitize_stored(list: &JValue) -> Result<Option<JValue>, Thrown> {
    let JValue::Arr(list) = list else {
        return Ok(None);
    };
    let mut out = Vec::new();
    for s in list {
        if out.len() >= MAX_SOURCES {
            break;
        }
        let Some(file) = s.get("file").as_str().filter(|f| !f.is_empty()) else {
            continue;
        };
        if !matches!(s, JValue::Obj(_)) {
            continue;
        }
        let id = s.get("id").as_str().unwrap_or("");
        out.push(shape(
            id,
            file,
            s.get("snippet"),
            s.get("kind"),
            s.get("score"),
            s.get("page"),
        )?);
    }
    Ok((!out.is_empty()).then_some(JValue::Arr(out)))
}

/// chat-sources.cjs `STORED_PIN`: `^skill_[a-f0-9]{32}@[a-f0-9]{64}$`.
fn valid_pin(v: &JValue) -> bool {
    let Some(s) = v.as_str() else { return false };
    let hex = |t: &str, n: usize| {
        t.len() == n
            && t.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    };
    s.strip_prefix("skill_")
        .and_then(|rest| rest.split_once('@'))
        .is_some_and(|(id, sha)| hex(id, 32) && hex(sha, 64))
}

/// chat-sources.cjs `sanitizeFailure`.
fn sanitize_failure(v: &JValue) -> Option<String> {
    let s = v.as_str()?;
    let text = wtf16::slice(trim(s), STORED_FAILURE_MAX);
    (!text.is_empty()).then_some(text)
}

/// chat-sources.cjs `cleanItem(m, role)`: the item, and whether Node rebuilt it.
fn clean_item(m: &JValue, role: &JValue) -> Result<(JValue, bool), Thrown> {
    if !m.truthy() || !is_object(m) || !(has(m, "sources") || has(m, "skill") || has(m, "failure"))
    {
        return Ok((m.clone(), false));
    }
    let role = role.as_str();
    if !has(m, "sources") && !has(m, "failure") && role == Some("user") && valid_pin(m.get("skill"))
    {
        return Ok((m.clone(), false));
    }
    let mut rest = without(m, &["sources", "skill", "failure"]);
    if role == Some("assistant") {
        if let Some(sources) = sanitize_stored(m.get("sources"))? {
            rest.push(("sources".to_string(), sources));
        }
    }
    if role == Some("user") && valid_pin(m.get("skill")) {
        rest.push(("skill".to_string(), m.get("skill").clone()));
    }
    if role == Some("assistant") {
        if let Some(failure) = sanitize_failure(m.get("failure")) {
            rest.push(("failure".to_string(), JValue::Str(failure)));
        }
    }
    Ok((JValue::Obj(rest), true))
}

/// chat-variants.cjs `capVariants(list, chosen)`: the kept list (`None`: unchanged) and the
/// chosen index.
fn cap_variants(list: &[JValue], chosen: &JValue) -> (Option<Vec<JValue>>, f64) {
    let shown = match chosen {
        JValue::Num(n) if n.fract() == 0.0 && *n >= 0.0 => *n,
        _ => 0.0,
    };
    // f64 throughout, as in JS: `chosen` may be any integer a JSON number holds.
    let mut drop = (list.len() as f64).max(shown + 1.0) - MAX_VARIANTS as f64;
    if drop <= 0.0 {
        return (None, shown);
    }
    let mut keep = Vec::new();
    let mut before = 0.0;
    for (i, item) in list.iter().enumerate() {
        let i = i as f64;
        if drop > 0.0 && i != shown {
            drop -= 1.0;
            if i < shown {
                before += 1.0;
            }
            continue;
        }
        keep.push(item.clone());
    }
    (Some(keep), shown - before)
}

/// A value and whether Node made a new one for it.
type Step = (JValue, bool);

/// chat-variants.cjs `capAnswers(owner)`.
fn cap_answers(owner: Step) -> Step {
    let (v, changed) = owner;
    let JValue::Arr(answers) = v.get("answers") else {
        return (v, changed);
    };
    match cap_variants(answers, v.get("answer")) {
        (None, _) => (v, changed),
        (Some(list), chosen) => {
            let items = with(spread(&v), "answers", JValue::Arr(list));
            (
                JValue::Obj(with(items, "answer", JValue::Num(chosen))),
                true,
            )
        }
    }
}

/// chat-variants.cjs `cleanAnswers(owner, clean)`.
fn clean_answers(owner: Step) -> Result<Step, Thrown> {
    let (v, changed) = owner;
    let JValue::Arr(answers) = v.get("answers") else {
        return Ok((v, changed));
    };
    let mut list = Vec::with_capacity(answers.len());
    let mut any = false;
    for a in answers {
        if a.truthy() && is_object(a) {
            let (cleaned, rebuilt) = clean_item(a, &JValue::Str("assistant".into()))?;
            any |= rebuilt;
            list.push(cleaned);
        } else {
            list.push(a.clone());
        }
    }
    Ok(if any {
        (
            JValue::Obj(with(spread(&v), "answers", JValue::Arr(list))),
            true,
        )
    } else {
        (v, changed)
    })
}

/// chat-variants.cjs `capEntry(entry, depth, cleanItem)`.
fn cap_entry(entry: &JValue, depth: usize) -> Result<Step, Thrown> {
    if !entry.truthy() || !is_object(entry) || (!has(entry, "answers") && !has(entry, "branches")) {
        return Ok((entry.clone(), false));
    }
    if depth > MAX_DEPTH {
        return Ok((
            JValue::Obj(without(entry, &["answers", "answer", "branches", "branch"])),
            true,
        ));
    }
    let mut next = clean_answers(cap_answers((entry.clone(), false)))?;
    let JValue::Arr(branches) = next.0.get("branches").clone() else {
        return Ok(next);
    };
    let role = entry.get("role");
    let mut versions = Vec::with_capacity(branches.len());
    let mut any_version = false;
    for version in &branches {
        if !version.truthy() || !is_object(version) {
            versions.push(version.clone());
            continue;
        }
        let with_answers = clean_answers(cap_answers(clean_item(version, role)?))?;
        let (tail, tail_changed) = match with_answers.0.get("tail") {
            JValue::Arr(items) => {
                let mut out = Vec::with_capacity(items.len());
                let mut changed = false;
                for h in items {
                    let (capped, c1) = cap_entry(h, depth + 1)?;
                    let (cleaned, c2) = if capped.truthy() && is_object(&capped) {
                        let role = capped.get("role").clone();
                        clean_item(&capped, &role)?
                    } else {
                        (capped, false)
                    };
                    changed |= c1 || c2;
                    out.push(cleaned);
                }
                (JValue::Arr(out), changed)
            }
            other => (other.clone(), false),
        };
        if !with_answers.1 && !tail_changed {
            versions.push(version.clone());
        } else {
            any_version = true;
            versions.push(JValue::Obj(with(spread(&with_answers.0), "tail", tail)));
        }
    }
    let (capped, chosen) = cap_variants(&versions, next.0.get("branch"));
    if capped.is_some() || any_version {
        let list = capped.unwrap_or(versions);
        let items = with(spread(&next.0), "branches", JValue::Arr(list));
        next = (
            JValue::Obj(with(items, "branch", JValue::Num(chosen))),
            true,
        );
    }
    Ok(next)
}

/// chat-sources.cjs `sanitizeHistory(history)`.
pub fn sanitize_history(history: &[JValue]) -> Result<Vec<JValue>, Thrown> {
    history
        .iter()
        .map(|entry| {
            let (m, _) = cap_entry(entry, 0)?;
            if m.truthy() && is_object(&m) {
                let role = m.get("role").clone();
                Ok(clean_item(&m, &role)?.0)
            } else {
                Ok(m)
            }
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    fn run(text: &str) -> String {
        let v = wtf16::parse(text).unwrap();
        let JValue::Arr(items) = v else { panic!() };
        wtf16::stringify(&JValue::Arr(sanitize_history(&items).unwrap())).unwrap()
    }

    #[test]
    fn sources_are_reshaped_and_moved_to_the_end_on_assistant_turns_only() {
        assert_eq!(
            run(
                r#"[{"role":"assistant","sources":[{"file":"qa.md","snippet":"  a\n\tb  ","score":0.12345,"kind":"excerpt","page":3}],"content":"x"}]"#
            ),
            r#"[{"role":"assistant","content":"x","sources":[{"id":"qa.md","file":"qa.md","snippet":"a b","kind":"excerpt","page":3,"score":0.123}]}]"#
        );
        assert_eq!(
            run(r#"[{"role":"user","sources":[{"file":"qa.md"}],"content":"x"}]"#),
            r#"[{"role":"user","content":"x"}]"#
        );
    }

    #[test]
    fn rounding_is_math_round() {
        assert_eq!(js_round(2.5), 3.0);
        assert_eq!(js_round(-2.5), -2.0);
        assert!(js_round(-0.4).is_sign_negative());
        assert_eq!(js_round(0.499_999_999_999_999_94), 0.0);
    }

    #[test]
    fn variants_keep_the_shown_one() {
        let answers: Vec<String> = (0..12).map(|i| i.to_string()).collect();
        let text = format!(
            r#"[{{"role":"user","content":"q","answers":[{}],"answer":0}}]"#,
            answers.join(",")
        );
        assert_eq!(
            run(&text),
            r#"[{"role":"user","content":"q","answers":[0,3,4,5,6,7,8,9,10,11],"answer":0}]"#
        );
    }

    #[test]
    fn a_rebuilt_version_list_normalises_its_branch() {
        assert_eq!(
            run(r#"[{"role":"user","branches":[{"content":"v","skill":"bad"}]}]"#),
            r#"[{"role":"user","branches":[{"content":"v"}],"branch":0}]"#
        );
        // Nothing to change: untouched, no `branch` added.
        assert_eq!(
            run(r#"[{"role":"user","branches":[{"content":"v"}]}]"#),
            r#"[{"role":"user","branches":[{"content":"v"}]}]"#
        );
    }

    #[test]
    fn an_own_to_string_throws_like_string_does() {
        let v = wtf16::parse(
            r#"[{"role":"assistant","sources":[{"file":"f","snippet":{"toString":1}}]}]"#,
        )
        .unwrap();
        let JValue::Arr(items) = v else { panic!() };
        assert_eq!(sanitize_history(&items), Err(Thrown));
    }
}
