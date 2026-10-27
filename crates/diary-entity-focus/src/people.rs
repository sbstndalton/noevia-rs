use crate::generated_rules as rules;
use crate::lexical::{cap_runs, has, key, keys, norm, sentence_start};
use crate::{Entity, Error};
use regex::Regex;

pub(crate) fn clean(text: &str) -> Result<String, Error> {
    use crate::regex_profile as rx;
    use std::sync::OnceLock;
    static ROLE: OnceLock<Result<Regex, Error>> = OnceLock::new();
    let re = ROLE
        .get_or_init(|| {
            let space = rx::SPACE;
            Regex::new(&format!(
                r"\*\*{space}*(?:{}){space}*:{space}*\*\*",
                rx::alternatives(&["Me", "Assistant", "Claude", "Companion", "User"])
            ))
            .map_err(|_| Error::Internal)
        })
        .as_ref()
        .map_err(|e| *e)?;
    Ok(re.replace_all(&norm(text), "\n").into_owned())
}
pub(crate) fn body(text: &str) -> Result<String, Error> {
    use crate::regex_profile as rx;
    use std::sync::OnceLock;
    static HEADER: OnceLock<Result<Regex, Error>> = OnceLock::new();
    let re = HEADER
        .get_or_init(|| {
            let space = rx::SPACE;
            let digit = rx::digits();
            Regex::new(&format!(
                r"(?m)^{space}*(?:\[{digit}{{4}}-{digit}{{2}}-{digit}{{2}}\]{space}*)?#+{space}.*$"
            ))
            .map_err(|_| Error::Internal)
        })
        .as_ref()
        .map_err(|e| *e)?;
    clean(&re.replace_all(text, "\n"))
}

fn short_form(k: &str) -> bool {
    rules::SHORT_FORMS
        .binary_search_by_key(&k, |(name, _)| name)
        .is_ok()
}
fn given_name(k: &str) -> bool {
    has(rules::GIVEN_NAMES, k)
        || short_form(k)
        || rules::SHORT_FORMS
            .iter()
            .any(|(_, fulls)| fulls.contains(&k))
}
fn institutional(words: &[String], k: &[String], lead: &str) -> bool {
    let Some(last) = k.last() else {
        return false;
    };
    if !has(rules::NOT_PERSON_LAST, last) {
        return false;
    }
    if !has(rules::SURNAME_LIKE_LAST, last) {
        return true;
    }
    let named = words
        .iter()
        .filter(|w| !has(rules::TITLES, &key(w)))
        .count();
    if named >= 3 {
        return true;
    }
    if named < words.len() {
        return false;
    }
    if has(rules::PLACE_CUES, lead) {
        return true;
    }
    !(k.len() >= 2 && k.first().is_some_and(|first| given_name(first)))
}
fn person_keys(words: &[String], lead: &str) -> Option<Vec<String>> {
    let first = words.first()?;
    let titled = has(rules::TITLES, &key(first));
    let k = keys(words);
    if k.is_empty()
        || k.iter()
            .any(|word| has(rules::NOT_NAMES, word) || word.chars().count() < 2)
    {
        return None;
    }
    if words
        .iter()
        .any(|word| word.chars().count() > 1 && crate::lexical::py_isupper(word))
    {
        return None;
    }
    if titled {
        return (!institutional(words, &k, "")).then_some(k);
    }
    if rules::PLACE_PHRASES.iter().any(|ph| {
        k.windows(ph.len())
            .any(|window| window.iter().map(String::as_str).eq(ph.iter().copied()))
    }) {
        return None;
    }
    if (2..=4).contains(&k.len())
        && !k
            .first()
            .is_some_and(|first| has(rules::NOT_PERSON_FIRST, first))
        && !institutional(words, &k, lead)
    {
        Some(k)
    } else {
        None
    }
}
pub(crate) fn is_person(entity: &Entity) -> bool {
    person_keys(
        &entity
            .name
            .split(crate::lexical::py_space)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect::<Vec<_>>(),
        "",
    )
    .is_some()
}
fn person_word(k: &[String], asked: &[Entity]) -> bool {
    if k.len() < 2 {
        return false;
    }
    if k.first().is_some_and(|first| given_name(first)) {
        return true;
    }
    for entity in asked {
        if entity.keys.len() < 2 {
            continue;
        }
        if k.first() == entity.keys.first() {
            return true;
        }
        if k.len() == 2
            && k.last()
                .zip(entity.keys.last())
                .is_some_and(|(last, surname)| last == surname || *last == format!("{surname}s"))
        {
            return true;
        }
    }
    false
}
fn trailing_word_same_line(text: &str, pos: usize) -> String {
    let before = text.get(..pos).unwrap_or_default();
    let trimmed = before.trim_end_matches([' ', '\t']);
    if trimmed.len() == before.len() {
        return String::new();
    }
    let word: String = trimmed
        .chars()
        .rev()
        .take_while(|c| crate::lexical::word_letter(*c))
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    key(&word)
}

pub fn identified_people(text: &str, asked: Option<&[Entity]>) -> Result<Vec<Vec<String>>, Error> {
    let text = clean(text)?;
    let mut out = Vec::new();
    let label_after = Regex::new(&format!(r"^[*_]*{}*:", crate::regex_profile::SPACE))
        .map_err(|_| Error::Internal)?;
    for run in cap_runs(&text, false)? {
        let mut words = run.iter().map(|(w, _)| w.clone()).collect::<Vec<_>>();
        let Some((_, first_pos)) = run.first() else {
            continue;
        };
        if sentence_start(&text, *first_pos) {
            while words
                .first()
                .is_some_and(|w| has(rules::NOT_NAMES, &key(w)))
            {
                words.remove(0);
            }
            if words.is_empty() {
                continue;
            }
            if !words.first().is_some_and(|w| has(rules::TITLES, &key(w))) {
                let Some((last, last_pos)) = run.last() else {
                    continue;
                };
                let after = text.get(last_pos + last.len()..).unwrap_or_default();
                if !asked.is_some_and(|entities| person_word(&keys(&words), entities))
                    || label_after.is_match(after)
                {
                    continue;
                }
            }
        }
        if words.iter().any(|w| {
            let k = key(w);
            has(rules::RELATIONS, &k) && !has(rules::TITLES, &k)
        }) {
            continue;
        }
        let lead = trailing_word_same_line(&text, *first_pos);
        if let Some(k) = person_keys(&words, &lead) {
            if !out.contains(&k) {
                out.push(k);
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod frozen_regex_probes {
    use super::{body, clean};
    use serde_json::Value;
    #[test]
    fn frozen_role_and_header_regex_probes() -> Result<(), Box<dyn std::error::Error>> {
        let mut n = 0;
        for line in include_str!("../tests/regex-probes.jsonl").lines() {
            let value: Value = serde_json::from_str(line)?;
            let kind = value.get("kind").and_then(Value::as_str);
            if !matches!(kind, Some("clean" | "body")) {
                continue;
            }
            let text = value
                .get("text")
                .and_then(Value::as_str)
                .ok_or("missing text")?;
            let expected = value
                .get("expected")
                .and_then(Value::as_str)
                .ok_or("missing expected")?;
            let actual = if kind == Some("clean") {
                clean(text)?
            } else {
                body(text)?
            };
            assert_eq!(actual, expected, "role/header case {n}: {text:?}");
            n += 1;
        }
        assert_eq!(n, 62);
        Ok(())
    }
}
