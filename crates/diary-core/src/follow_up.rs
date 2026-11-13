//! Elliptical follow-ups about another person (port of `diary/agent/follow_up.py`, services#173).
//!
//! "And what about Tamsin Fenimore?" right after "Which map projection does Tamsin Fennimore prefer?"
//! is a new question about a different name. [`resolve`] turns such a message into a standalone
//! question by putting the new name into the previous question. Conservative by design: the message
//! is left as is (`Ok(None)`) when it is not purely "And what about <Name>?" (also "What about",
//! "How about", "And"), when the name is not capitalised, is the same person as the earlier one, or
//! the previous question names nobody, several people, the same name twice, or is itself an
//! elliptical follow-up.
//!
//! Differences from the Python module, all by construction of the Rust types:
//! - Turns are `serde_json::Value` objects, the shape `context::completed_exchanges` returns. A turn
//!   that is not an object is skipped (Python would raise on it); only `{"role": "user", "content":
//!   <string>}` turns count, as in Python.
//! - Known difference, capacity: `resolve` returns [`Error::Capacity`] where Python would simply
//!   run longer, in these named cases, all raised by entity focus and not by this module:
//!   (1) any text it reads (the name phrase, the previous question) over 1 MiB (`MAX_QUERY_BYTES`);
//!   (2) a name phrase whose whitespace-separated words total over 1 MiB;
//!   (3) more than 8,192 words, or a word over 256 characters, in text entity focus reads;
//!   (4) in the near-spelling check, a name key over 256 characters after NFKD expansion (`near_word`),
//!   e.g. a key of 90 "\u{fb03}" ligatures expands to 270 characters: Python resolves with
//!   `near = True`, Rust returns the error (unit test `a_key_over_the_near_word_cap_is_a_capacity_error`).
//!   A message that does not match the elliptical pattern is never an error, however large.
//!   `mentions` and `safe_name` have no cap.
//! - Python strings may hold lone surrogates; a Rust `&str` cannot, so those are not representable.
//! - Unicode data (case folding, general category) is pinned to Unicode 15.0.0, the version of the
//!   Python 3.12 service, not the toolchain's.

use crate::context::is_python_space;
use crate::unicode15;
use diary_entity_focus::{asked_entities, name_keys, near_word, Entity, Error};
use serde_json::Value;
use std::collections::HashSet;

/// The standalone form of an elliptical follow-up about a different name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolution {
    /// The previous question with the new name in place of the earlier one.
    pub standalone: String,
    /// The name as the previous question wrote it.
    pub earlier_name: String,
    /// The name as the follow-up wrote it.
    pub asked_name: String,
    /// A near spelling (every word equal or a close spelling) rather than another name.
    pub near: bool,
}

/// Characters that could end or fake the quoting of a name inside a note: double quotes of any
/// style, backticks and square brackets (the note's own delimiters).
const QUOTE_CHARS: [char; 11] = [
    '"', '\u{201c}', '\u{201d}', '\u{201e}', '\u{201f}', '\u{ab}', '\u{bb}', '\u{2033}', '`', '[',
    ']',
];

/// Characters the elliptical pattern's name group may not contain: `[^?!.,;:\n]`.
fn forbidden_in_name(c: char) -> bool {
    matches!(c, '?' | '!' | '.' | ',' | ';' | ':' | '\n')
}

/// Python's `re.I` match of one of the pattern's literal letters. The only characters that match
/// any of a, b, d, h, n, o, t, u, w case-insensitively are the ASCII letters themselves (checked
/// over every code point by the fixture generator).
fn literal_at(chars: &[char], at: usize, literal: &str) -> Option<usize> {
    let mut i = at;
    for want in literal.chars() {
        if chars.get(i)?.to_ascii_lowercase() != want {
            return None;
        }
        i += 1;
    }
    Some(i)
}

/// End of the run of whitespace starting at `at`, when it holds at least one character (`\s+`).
fn spaces(chars: &[char], at: usize) -> Option<usize> {
    let mut i = at;
    while chars.get(i).copied().is_some_and(is_python_space) {
        i += 1;
    }
    (i > at).then_some(i)
}

/// The name group of Python's
///
/// ```text
/// ^\s*(?:(?:and\s+)?(?:what|how)\s+about|and)\s+(?P<name>[^?!.,;:\n]+?)\s*[?!.]*\s*$   (re.I)
/// ```
///
/// matched at the start of `text`, or `None`. Backtracking is reproduced: of the possible prefixes
/// the first that lets the rest match wins, the whitespace before the name gives back characters
/// (so the name may start with whitespace) when it must, and the lazy name is the shortest one the
/// tail accepts. `$` also matches before a final newline, but that newline is whitespace and the
/// tail's last `\s*` takes it anyway, so the end of the tail is simply the end of the text.
pub fn elliptical_name(text: &str) -> Option<&str> {
    // Every alternative opens with and/what/how and a whitespace character. Test that on the &str
    // first so a large non-matching text allocates nothing.
    let rest = text.trim_start_matches(is_python_space);
    let word_len = rest
        .find(|c: char| !c.is_ascii_alphabetic())
        .unwrap_or(rest.len());
    let opener = rest.get(..word_len).unwrap_or_default();
    let followed_by_space = rest
        .get(word_len..)
        .and_then(|after| after.chars().next())
        .is_some_and(is_python_space);
    let opens = ["and", "what", "how"]
        .iter()
        .any(|word| opener.eq_ignore_ascii_case(word));
    if !opens || !followed_by_space {
        return None;
    }
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    // Byte offset of every character index, plus the end.
    let mut offsets: Vec<usize> = text.char_indices().map(|(at, _)| at).collect();
    offsets.push(text.len());

    // The tail `\s*[?!.]*\s*$` matches from `from` exactly when `from >= tail_start`: the valid
    // starts are the longest suffix of the shape whitespace, punctuation, whitespace.
    let mut tail_start = n;
    for step in 0..3 {
        while tail_start > 0
            && chars.get(tail_start - 1).is_some_and(|&c| {
                if step == 1 {
                    matches!(c, '?' | '!' | '.')
                } else {
                    is_python_space(c)
                }
            })
        {
            tail_start -= 1;
        }
    }
    // next_forbidden[i]: first index >= i holding a character the name may not contain, or n.
    let mut next_forbidden = vec![n; n + 1];
    for i in (0..n).rev() {
        let next = if chars.get(i).copied().is_some_and(forbidden_in_name) {
            i
        } else {
            next_forbidden.get(i + 1).copied().unwrap_or(n)
        };
        if let Some(slot) = next_forbidden.get_mut(i) {
            *slot = next;
        }
    }

    // `^\s*`: the next character is a letter, so no backtracking into the whitespace is useful.
    let mut start = 0usize;
    while chars.get(start).copied().is_some_and(is_python_space) {
        start += 1;
    }

    // `what|how` then `\s+about`, ending at the character after "about".
    let what_how_about = |at: usize| -> Option<usize> {
        let after = literal_at(&chars, at, "what").or_else(|| literal_at(&chars, at, "how"))?;
        literal_at(&chars, spaces(&chars, after)?, "about")
    };
    // Where the whitespace before the name starts, for each alternative in match order.
    let mut run_starts: Vec<usize> = Vec::new();
    if let Some(after_and) = literal_at(&chars, start, "and") {
        if let Some(from) = spaces(&chars, after_and) {
            if let Some(end) = what_how_about(from) {
                run_starts.push(end);
            }
        }
    }
    if let Some(end) = what_how_about(start) {
        run_starts.push(end);
    }
    if let Some(after_and) = literal_at(&chars, start, "and") {
        run_starts.push(after_and);
    }

    for run_start in run_starts {
        let Some(run_end) = spaces(&chars, run_start) else {
            continue;
        };
        // `\s+` is greedy and gives back characters one at a time to the name.
        for name_start in (run_start + 1..=run_end).rev() {
            if name_start >= n {
                continue;
            }
            let name_end = (name_start + 1).max(tail_start);
            let clean = next_forbidden
                .get(name_start)
                .is_some_and(|&f| f >= name_end);
            if clean && name_end <= n {
                let from = offsets.get(name_start).copied()?;
                let to = offsets.get(name_end).copied()?;
                return text.get(from..to);
            }
        }
    }
    None
}

/// The single entity that `text` consists of, or `None` when it holds anything else.
fn only_name(text: &str) -> Result<Option<Entity>, Error> {
    let mut found = asked_entities(text)?;
    if found.len() != 1 {
        return Ok(None);
    }
    let Some(entity) = found.pop() else {
        return Ok(None);
    };
    let words: Vec<String> = text
        .split(is_python_space)
        .filter(|word| !word.is_empty())
        .map(str::to_owned)
        .collect();
    if name_keys(&words)? != entity.keys {
        return Ok(None);
    }
    Ok(Some(entity))
}

/// Python `set(a.keys) <= set(b.keys) or set(b.keys) <= set(a.keys)`.
fn same_person(a: &Entity, b: &Entity) -> bool {
    let left: HashSet<&str> = a.keys.iter().map(String::as_str).collect();
    let right: HashSet<&str> = b.keys.iter().map(String::as_str).collect();
    left.is_subset(&right) || right.is_subset(&left)
}

fn near(a: &Entity, b: &Entity) -> Result<bool, Error> {
    if a.keys.len() != b.keys.len() {
        return Ok(false);
    }
    for (x, y) in a.keys.iter().zip(&b.keys) {
        if x != y && !near_word(x, y)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// The standalone form of an elliptical follow-up about a different name, or `None` (leave the
/// message as is).
///
/// `session_turns` are the replayable turns (`context::completed_exchanges` output); the previous
/// question is the latest user turn with string content. Pass an empty slice for no conversation.
pub fn resolve(message: &str, session_turns: &[Value]) -> Result<Option<Resolution>, Error> {
    let Some(name) = elliptical_name(message) else {
        return Ok(None);
    };
    let Some(asked) = only_name(name)? else {
        return Ok(None);
    };
    let earlier_question = session_turns
        .iter()
        .rev()
        .filter(|turn| turn.get("role").and_then(Value::as_str) == Some("user"))
        .find_map(|turn| turn.get("content").and_then(Value::as_str));
    let Some(earlier_question) = earlier_question else {
        return Ok(None);
    };
    if elliptical_name(earlier_question).is_some() {
        return Ok(None); // no chains: the previous question must be a full question
    }
    let mut earlier = asked_entities(earlier_question)?;
    if earlier.len() != 1 {
        return Ok(None);
    }
    let Some(earlier) = earlier.pop() else {
        return Ok(None);
    };
    if same_person(&earlier, &asked) {
        return Ok(None);
    }
    if earlier_question.matches(earlier.name.as_str()).count() != 1 {
        return Ok(None); // written twice (or not as read): a partial replacement would mix two people
    }
    let standalone = earlier_question.replacen(earlier.name.as_str(), &asked.name, 1);
    let is_near = near(&earlier, &asked)?;
    Ok(Some(Resolution {
        standalone,
        earlier_name: earlier.name,
        asked_name: asked.name,
        near: is_near,
    }))
}

/// True when `c` is in general category C* (Cc, Cf, Cs, Co or unassigned) in Unicode 15.0.0.
pub fn is_category_c(c: char) -> bool {
    let cp = u32::from(c);
    unicode15::OTHER
        .binary_search_by(|&(low, high)| {
            if high < cp {
                std::cmp::Ordering::Less
            } else if low > cp {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .is_ok()
}

/// A name as quoted inside a note: no quote or bracket characters, no control characters (every
/// category C* character becomes a space), whitespace collapsed to single spaces.
pub fn safe_name(name: &str) -> String {
    let kept: String = name
        .chars()
        .filter(|c| !QUOTE_CHARS.contains(c))
        .map(|c| if is_category_c(c) { ' ' } else { c })
        .collect();
    kept.split(is_python_space)
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Labels the replayed earlier exchange; it is kept, never dropped. Reference data, not an
/// instruction.
///
/// `exact_loaded` says whether a retrieved entry naming the asked name exactly is shown (services#179).
/// Then the note says to answer from those entries; only without one does a near spelling get the
/// sentence about saying the diary records nothing. The note never holds both.
pub fn conversation_note(res: &Resolution, exact_loaded: bool) -> String {
    let earlier = safe_name(&res.earlier_name);
    let asked = safe_name(&res.asked_name);
    let kind = if res.near {
        "a near spelling of that name"
    } else {
        "a different name"
    };
    let mut note = format!(
        "[Reference note on this conversation: the earlier exchange above was about \"{earlier}\". \
         The new question asks about \"{asked}\", {kind}. The diary names people exactly, so they \
         are different people: the earlier answer's details are not details of \"{asked}\". The \
         new question is the earlier question asked about \"{asked}\"."
    );
    if exact_loaded {
        note.push_str(&format!(
            " Retrieved entries that name \"{asked}\" exactly are about \"{asked}\": answer from \
             them, following the latest-record and withdrawn rules."
        ));
    } else if res.near {
        note.push_str(&format!(
            " If the diary records nothing about \"{asked}\", say so and mention that it does \
             record \"{earlier}\"."
        ));
    }
    note.push(']');
    note
}

/// Labels today's log when it holds the earlier exchange (logged before this question).
pub fn today_note(res: &Resolution) -> String {
    let earlier = safe_name(&res.earlier_name);
    let asked = safe_name(&res.asked_name);
    format!(
        "[Reference note: today's log may include the earlier exchange about \"{earlier}\". It is \
         about \"{earlier}\", not \"{asked}\".]"
    )
}

/// Python `str.casefold()`: full Unicode case folding of each character on its own (ß becomes "ss",
/// ς becomes σ), Unicode 15.0.0 data.
pub fn casefold(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        let folded = unicode15::CASEFOLD
            .binary_search_by_key(&u32::from(c), |&(cp, _)| cp)
            .ok()
            .and_then(|index| unicode15::CASEFOLD.get(index));
        match folded {
            Some((_, replacement)) => out.push_str(replacement),
            None => out.push(c),
        }
    }
    out
}

/// True when `text` is non-empty and holds `name`, compared case-insensitively by full case folding
/// (`bool(text) and name.casefold() in text.casefold()`).
pub fn mentions(text: &str, name: &str) -> bool {
    !text.is_empty() && casefold(text).contains(&casefold(name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    #[test]
    fn oversized_input_is_a_capacity_error_not_a_silent_none() {
        let name = format!("And {}", "Tamsin ".repeat(200_000));
        assert_eq!(resolve(&name, &[]), Err(Error::Capacity));
        let turns = [json!({"role": "user", "content": "Tamsin ".repeat(200_000)})];
        assert_eq!(
            resolve("And Orla Brennock?", &turns),
            Err(Error::Capacity),
            "the earlier question is read by entity focus too"
        );
    }

    #[test]
    fn a_message_that_is_not_elliptical_never_reaches_entity_focus() -> TestResult {
        // Python never raises here (the regex fails first), so there is no capacity error either.
        let huge = "x".repeat(2_000_000);
        assert_eq!(resolve(&huge, &[])?, None);
        Ok(())
    }

    #[test]
    fn turns_that_are_not_objects_are_skipped() -> TestResult {
        let turns = [
            json!({"role": "user", "content": "Which map projection does Tamsin Fennimore prefer?"}),
            json!("not an object"),
            json!(null),
            json!(7),
        ];
        let res = resolve("And what about Tamsin Fenimore?", &turns)?;
        assert_eq!(
            res.map(|r| r.standalone),
            Some("Which map projection does Tamsin Fenimore prefer?".to_owned())
        );
        Ok(())
    }

    #[test]
    fn long_runs_of_whitespace_and_name_less_text_stay_linear() -> TestResult {
        let started = std::time::Instant::now();
        let spaces = format!("And what about{}", " ".repeat(900_000));
        assert_eq!(elliptical_name(&spaces).map(str::len), Some(1));
        let lone = format!("And what about{}x?", " ".repeat(900_000));
        assert_eq!(elliptical_name(&lone).map(str::len), Some(1));
        let no_end = format!("And Tamsin{}Fenimore, later", " Fenimore".repeat(100_000));
        assert_eq!(elliptical_name(&no_end), None);
        assert!(started.elapsed() < std::time::Duration::from_secs(20));
        Ok(())
    }

    #[test]
    fn a_large_non_elliptical_text_is_rejected_before_any_table_is_built() {
        let started = std::time::Instant::now();
        for lead in ["", "   ", "Andy ", "wh at ", "Which "] {
            let text = format!("{lead}{}", "x ".repeat(2_000_000));
            assert_eq!(elliptical_name(&text), None);
        }
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }

    #[test]
    fn same_person_is_linear_on_hostile_key_sets() -> TestResult {
        // 4000 words of 128 hyphen-joined keys each, per side: over half a million keys. The values
        // in the assertions were recorded from the Python module on this same input.
        let word = |part: &str| format!("Ab{}", format!("-{part}").repeat(127));
        let asked = format!("And what about {}?", format!("{} ", word("b")).repeat(4000));
        let earlier = format!("{} ?", format!("{} ", word("c")).repeat(4000));
        assert_eq!((asked.len(), earlier.len()), (1_028_016, 1_028_002));
        let turns = [json!({"role": "user", "content": earlier})];
        let started = std::time::Instant::now();
        let res = resolve(&asked, &turns)?.ok_or("resolves")?;
        // Entity focus reads both 1 MiB texts (about a second each in a debug build); the bound only
        // guards against the quadratic key-set comparison, which would take minutes.
        assert!(
            started.elapsed() < std::time::Duration::from_secs(15),
            "{:?}",
            started.elapsed()
        );
        assert!(!res.near);
        assert_eq!(res.standalone.len(), 1_028_002);
        assert_eq!(res.earlier_name.len(), 1_027_999);
        assert_eq!(res.asked_name.len(), 1_027_999);
        assert!(res.standalone.starts_with(&res.asked_name));
        let entity = |keys: &[&str]| Entity {
            name: "N".to_owned(),
            keys: keys.iter().map(|k| (*k).to_owned()).collect(),
        };
        let many: Vec<String> = (0..60_000).map(|i| format!("k{i}")).collect();
        let big = Entity {
            name: "N".to_owned(),
            keys: many.clone(),
        };
        let other = Entity {
            name: "N".to_owned(),
            keys: many.iter().rev().cloned().collect(),
        };
        let started = std::time::Instant::now();
        assert!(same_person(&big, &other));
        assert!(same_person(
            &entity(&["a", "a", "b"]),
            &entity(&["b", "a", "c"])
        ));
        assert!(same_person(&entity(&["a"]), &entity(&["a", "b"])));
        assert!(!same_person(&entity(&["a", "c"]), &entity(&["a", "b"])));
        assert!(same_person(&entity(&[]), &entity(&["a"])));
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
        Ok(())
    }

    #[test]
    fn a_key_over_the_near_word_cap_is_a_capacity_error() {
        // Python resolves this with near = True; the 90 ligatures NFKD-expand to 270 characters.
        let ligatures = "\u{fb03}".repeat(90);
        let turns = [json!({
            "role": "user",
            "content": format!("Which map does Qa C{ligatures} prefer?")
        })];
        let message = format!("And what about Qa B{ligatures}?");
        assert_eq!(resolve(&message, &turns), Err(Error::Capacity));
    }

    #[test]
    fn is_python_space_matches_the_recorded_re_s_and_isspace_sets() -> TestResult {
        let fx: Value = serde_json::from_str(include_str!("../tests/follow-up-fixtures.json"))?;
        let list = |key: &str| -> Result<Vec<u32>, String> {
            fx.get("whitespace")
                .and_then(|w| w.get(key))
                .and_then(Value::as_array)
                .ok_or_else(|| format!("missing {key}"))?
                .iter()
                .map(|v| {
                    v.as_u64()
                        .and_then(|n| u32::try_from(n).ok())
                        .ok_or("code point".to_owned())
                })
                .collect()
        };
        let re_s = list("re_s")?;
        assert_eq!(re_s, list("isspace")?);
        let ours: Vec<u32> = (0..=0x10_ffff_u32)
            .filter(|&cp| char::from_u32(cp).is_some_and(is_python_space))
            .collect();
        assert_eq!(ours, re_s);
        // Whitespace-looking characters that Python 3.12 does not treat as whitespace.
        for cp in [0x200b_u32, 0x180e, 0xfeff, 0x2060] {
            assert!(!re_s.contains(&cp));
        }
        // Category C whitespace.
        for cp in [0x85_u32, 0x1c, 0x1d, 0x1e, 0x1f] {
            assert!(re_s.contains(&cp));
        }
        Ok(())
    }

    #[test]
    fn mentions_is_casefold_not_lowercase() {
        assert!(mentions("Die Stra\u{df}e", "STRASSE"));
        assert!("Stra\u{df}e".to_lowercase() != "strasse");
    }
}
