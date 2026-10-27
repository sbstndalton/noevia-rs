use crate::generated_rules as rules;
use crate::{Entity, Error, Kind, MAX_ENTITIES, MAX_QUERY_BYTES, MAX_WORDS, MAX_WORD_CHARS};
use unicode_normalization::UnicodeNormalization;

pub(crate) fn has(set: &[&str], key: &str) -> bool {
    set.binary_search(&key).is_ok()
}
fn in_ranges(cp: u32, ranges: &[(u32, u32)]) -> bool {
    let mut low = 0usize;
    let mut high = ranges.len();
    while low < high {
        let mid = low + (high - low) / 2;
        let Some(&(a, b)) = ranges.get(mid) else {
            return false;
        };
        if cp < a {
            high = mid;
        } else if cp > b {
            low = mid + 1;
        } else {
            return true;
        }
    }
    false
}
fn decimal(c: char) -> bool {
    in_ranges(c as u32, rules::DECIMAL_RANGES)
}
pub(crate) fn decimal_digit(c: char) -> Option<u8> {
    let cp = c as u32;
    rules::DECIMAL_RANGES
        .iter()
        .find(|(a, b)| *a <= cp && cp <= *b)
        .map(|(a, _)| ((cp - *a) % 10) as u8)
}
fn combining(c: char) -> bool {
    in_ranges(c as u32, rules::COMBINING_RANGES)
}
pub(crate) fn py_alpha(c: char) -> bool {
    in_ranges(c as u32, rules::ALPHA_RANGES)
}
fn py_alnum(c: char) -> bool {
    in_ranges(c as u32, rules::ALNUM_RANGES)
}
fn py_upper(c: char) -> bool {
    in_ranges(c as u32, rules::UPPER_RANGES)
}
pub(crate) fn py_isupper(s: &str) -> bool {
    let mut cased = false;
    for c in s.chars() {
        if in_ranges(c as u32, rules::CASED_RANGES) {
            cased = true;
            if !py_upper(c) {
                return false;
            }
        }
    }
    cased
}
fn word_letter(c: char) -> bool {
    py_alnum(c) && !decimal(c)
}
fn alnum_no_underscore(c: char) -> bool {
    py_alnum(c)
}
pub(crate) fn py_space(c: char) -> bool {
    matches!(c,'\u{0009}'..='\u{000d}'|'\u{001c}'..='\u{0020}'|'\u{0085}'|'\u{00a0}'|'\u{1680}'|'\u{2000}'..='\u{200a}'|'\u{2028}'|'\u{2029}'|'\u{202f}'|'\u{205f}'|'\u{3000}')
}

pub(crate) fn norm(s: &str) -> String {
    s.replace(['’', '‘'], "'")
}
pub(crate) fn key(s: &str) -> String {
    let mut folded = String::new();
    for c in s.to_lowercase().chars() {
        if let Some((_, to)) = rules::FOLD.iter().find(|(from, _)| *from == c) {
            folded.push_str(to);
        } else {
            folded.push(c);
        }
    }
    folded
        .nfkd()
        .filter(|c| !combining(*c) && *c != '\'')
        .collect()
}

#[derive(Clone, Debug)]
pub(crate) struct Word {
    pub text: String,
    pub start: usize,
    pub end: usize,
    pub possessive: bool,
}

pub(crate) fn words(text: &str) -> Result<Vec<Word>, Error> {
    if text.len() > MAX_QUERY_BYTES {
        return Err(Error::Capacity);
    }
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut result = Vec::new();
    let mut i = 0usize;
    while i < chars.len() {
        let Some(&(start, c)) = chars.get(i) else {
            break;
        };
        if !word_letter(c) {
            i += 1;
            continue;
        }
        let mut j = i + 1;
        while let Some(&(_, next)) = chars.get(j) {
            if word_letter(next) {
                j += 1;
            } else if matches!(next, '\'' | '-')
                && chars
                    .get(j + 1)
                    .is_some_and(|(_, after)| word_letter(*after))
            {
                j += 2;
            } else {
                break;
            }
        }
        let end = chars.get(j).map_or(text.len(), |(at, _)| *at);
        let raw = text.get(start..end).ok_or(Error::Capacity)?;
        if raw.chars().count() > MAX_WORD_CHARS {
            return Err(Error::Capacity);
        }
        let stripped = if raw.chars().count() > 2 {
            raw.strip_suffix("'s")
                .or_else(|| raw.strip_suffix("'S"))
                .or_else(|| raw.strip_suffix("'ſ"))
                .or_else(|| raw.strip_suffix('\''))
                .unwrap_or(raw)
        } else {
            raw
        };
        let possessive = !stripped.is_empty() && stripped != raw;
        result.push(Word {
            text: if stripped.is_empty() {
                raw.to_owned()
            } else {
                stripped.to_owned()
            },
            start,
            end,
            possessive,
        });
        if result.len() > MAX_WORDS {
            return Err(Error::Capacity);
        }
        i = j;
    }
    Ok(result)
}

pub(crate) fn sentence_start(text: &str, pos: usize) -> bool {
    let Some(before) = text.get(..pos) else {
        return false;
    };
    let mut chars = before.chars().rev();
    loop {
        match chars.next() {
            None => return true,
            Some(' ' | '\t' | '\r' | '*' | '_' | '-' | '>' | '\'') => continue,
            Some(c) => return ".!?:;\n\"()[]…—–•“”«»".contains(c),
        }
    }
}

fn short_forms(key: &str) -> Option<&'static [&'static str]> {
    rules::SHORT_FORMS
        .binary_search_by_key(&key, |(name, _)| name)
        .ok()
        .and_then(|i| rules::SHORT_FORMS.get(i).map(|(_, forms)| *forms))
}
fn ordinary(k: &str) -> bool {
    has(rules::VERB_FORMS, k) || (short_forms(k).is_some() && has(rules::NOT_NAMES, k))
}

fn trailing_prev_name_word(text: &str) -> Option<String> {
    let trimmed = text.trim_end_matches(|c: char| py_space(c) || c == ',');
    let mut rev = trimmed
        .chars()
        .rev()
        .take_while(|c| word_letter(*c))
        .collect::<Vec<_>>();
    if rev.is_empty() {
        None
    } else {
        rev.reverse();
        Some(rev.into_iter().collect())
    }
}
fn reads_as_name(text: &str, start: usize, end: usize, initial: bool) -> bool {
    let opens = sentence_start(text, start);
    if opens && !initial {
        return false;
    }
    let after = text.get(end..).unwrap_or_default();
    let next = after.trim_start_matches(|c: char| py_space(c) || c == ',');
    let nxt: String = next
        .chars()
        .take_while(|c| alnum_no_underscore(*c))
        .collect();
    if !nxt.is_empty()
        && (nxt.chars().next().is_some_and(decimal)
            || (opens && has(rules::AFTER_VERB, &key(&nxt))))
    {
        return false;
    }
    if opens && after.trim_start_matches(py_space).starts_with(['?', '!']) {
        return false;
    }
    let prev = text.get(..start).and_then(trailing_prev_name_word);
    let current = text.get(start..end).unwrap_or_default();
    let k = key(current
        .strip_suffix("'s")
        .or_else(|| current.strip_suffix('\''))
        .unwrap_or(current));
    let is_month = has(rules::MONTHS, &k)
        || rules::MONTHS.iter().any(|m| m.get(..3) == Some(k.as_str()))
        || k == "sept";
    if is_month
        && (prev.is_some_and(|p| has(rules::DATE_LEAD, &key(&p)))
            || next.chars().next().is_some_and(decimal))
    {
        return false;
    }
    true
}

pub(crate) type Run = Vec<(String, usize)>;
pub(crate) fn cap_runs(text: &str, stop: bool) -> Result<Vec<Run>, Error> {
    let tokens = words(text)?;
    let mut runs = Vec::new();
    let mut current = Vec::new();
    let mut prev_end = None;
    let mut prev_possessive = false;
    let mut prev_title = false;
    let mut prev_initial = false;
    for token in tokens {
        let gap = prev_end
            .and_then(|end| text.get(end..token.start))
            .map(|s| s.trim_matches([' ', '\t']));
        let dotted = prev_title || prev_initial;
        let joined =
            gap.is_some_and(|g| !prev_possessive && (g.is_empty() || (dotted && g == ".")));
        let chars = token.text.chars().collect::<Vec<_>>();
        let first = chars.first().copied();
        let initial = chars.len() == 1
            && first.is_some_and(py_upper)
            && token.text != "I"
            && text.get(token.end..).is_some_and(|s| s.starts_with('.'));
        let mut name_like = first.is_some_and(py_upper)
            && (chars.len() > 1 || !stop || initial)
            && !(stop && !initial && has(rules::NOT_NAMES, &token.text.to_lowercase()));
        if first.is_some_and(py_upper)
            && ordinary(&key(&token.text))
            && !(joined && !current.is_empty())
        {
            name_like = reads_as_name(text, token.start, token.end, !stop);
        }
        if name_like && joined && !current.is_empty() {
            current.push((token.text.clone(), token.start));
        } else {
            if !current.is_empty() {
                runs.push(std::mem::take(&mut current));
            }
            if name_like {
                current.push((token.text.clone(), token.start));
            }
        }
        prev_end = Some(token.end);
        prev_possessive = token.possessive;
        prev_title = has(rules::TITLES, &key(&token.text));
        prev_initial = chars.len() == 1 && first.is_some_and(py_upper);
    }
    if !current.is_empty() {
        runs.push(current);
    }
    Ok(runs)
}

pub(crate) fn keys(words: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for word in words {
        for part in word.split('-') {
            let k = key(part);
            if !k.is_empty() && !has(rules::TITLES, &k) {
                out.push(k);
            }
        }
    }
    out
}

pub fn asked_entities(message: &str) -> Result<Vec<Entity>, Error> {
    let text = norm(message);
    let mut out = Vec::new();
    let mut seen = Vec::<Vec<String>>::new();
    for run in cap_runs(&text, true)? {
        if run.len() == 1
            && run
                .first()
                .is_some_and(|(_, at)| sentence_start(&text, *at))
        {
            continue;
        }
        let k = keys(&run.iter().map(|(w, _)| w.clone()).collect::<Vec<_>>());
        if k.is_empty() || seen.contains(&k) {
            continue;
        }
        seen.push(k.clone());
        out.push(Entity {
            name: run
                .iter()
                .map(|(w, _)| w.as_str())
                .collect::<Vec<_>>()
                .join(" "),
            keys: k,
        });
        if out.len() >= MAX_ENTITIES {
            break;
        }
    }
    Ok(out)
}

fn near_distance(a: &str, b: &str, limit: usize) -> usize {
    let ac: Vec<char> = a.chars().collect();
    let bc: Vec<char> = b.chars().collect();
    if ac.len().abs_diff(bc.len()) > limit {
        return limit + 1;
    }
    let mut prev2: Option<Vec<usize>> = None;
    let mut prev = (0..=bc.len()).collect::<Vec<_>>();
    for i in 1..=ac.len() {
        let mut cur = vec![i; bc.len() + 1];
        for j in 1..=bc.len() {
            let cost = usize::from(ac.get(i - 1) != bc.get(j - 1));
            let a = prev.get(j).copied().unwrap_or(usize::MAX).saturating_add(1);
            let b = cur
                .get(j - 1)
                .copied()
                .unwrap_or(usize::MAX)
                .saturating_add(1);
            let c = prev
                .get(j - 1)
                .copied()
                .unwrap_or(usize::MAX)
                .saturating_add(cost);
            let mut value = a.min(b).min(c);
            if let Some(previous) = &prev2 {
                if i > 1
                    && j > 1
                    && ac.get(i - 1) == bc.get(j - 2)
                    && ac.get(i - 2) == bc.get(j - 1)
                {
                    value = value.min(
                        previous
                            .get(j - 2)
                            .copied()
                            .unwrap_or(usize::MAX)
                            .saturating_add(1),
                    );
                }
            }
            if let Some(slot) = cur.get_mut(j) {
                *slot = value;
            }
        }
        if cur.iter().copied().min().is_some_and(|v| v > limit) {
            return limit + 1;
        }
        prev2 = Some(prev);
        prev = cur;
    }
    prev.last().copied().unwrap_or(limit + 1)
}
pub fn near_word(a: &str, b: &str) -> bool {
    let a = key(a);
    let b = key(b);
    let min = a.chars().count().min(b.chars().count());
    if a == b || min < 3 || a.chars().count() > MAX_WORD_CHARS || b.chars().count() > MAX_WORD_CHARS
    {
        return false;
    }
    let limit = if min <= 5 { 1 } else { 2 };
    near_distance(&a, &b, limit) <= limit
}

fn relation(word: &str, want: &str) -> Option<&'static str> {
    if word == want {
        return Some("eq");
    }
    if (want.chars().count() == 1 && word.starts_with(want))
        || (word.chars().count() == 1 && want.starts_with(word))
    {
        return Some("variant");
    }
    if short_forms(word).is_some_and(|forms| forms.contains(&want))
        || short_forms(want).is_some_and(|forms| forms.contains(&word))
    {
        return Some("variant");
    }
    if has(rules::NOT_NAMES, word) || has(rules::ROLE_WORDS, word) {
        return None;
    }
    if near_word(word, want) {
        return Some("near");
    }
    if word.chars().count().min(want.chars().count()) >= 2
        && (want.starts_with(word) || word.starts_with(want))
    {
        return Some("variant");
    }
    None
}
fn subsequence(short: &[String], long: &[String]) -> bool {
    if short.is_empty() {
        return false;
    }
    let mut at = 0;
    for item in long {
        if short.get(at) == Some(item) {
            at += 1;
            if at == short.len() {
                return true;
            }
        }
    }
    false
}
fn names_in_full(window: &[String], e: &[String]) -> bool {
    window == e
        || (e.len() >= 2
            && window.len() == e.len()
            && window.get(..e.len() - 1) == e.get(..e.len() - 1)
            && window
                .last()
                .zip(e.last())
                .is_some_and(|(w, s)| s.chars().count() >= 2 && *w == format!("{s}s")))
}

pub fn classify(text: &str, entity: &Entity) -> Result<(Option<Kind>, Option<String>), Error> {
    let n = entity.keys.len();
    if n == 0 {
        return Ok((None, None));
    }
    let cleaned = crate::people::clean(text)?;
    let mut best: (Option<Kind>, Option<String>) = (None, None);
    for run in cap_runs(&cleaned, false)? {
        let ws = run.iter().map(|(w, _)| w.clone()).collect::<Vec<_>>();
        let k = keys(&ws);
        if k.is_empty() {
            continue;
        }
        let shown = ws.join(" ");
        if k.windows(n)
            .any(|window| names_in_full(window, &entity.keys))
        {
            return Ok((Some(Kind::Match), Some(shown)));
        }
        let mut kinds = Vec::new();
        if n >= 2
            && k.len() >= n
            && (subsequence(&entity.keys, &k) || entity.keys.iter().all(|x| k.contains(x)))
        {
            kinds.push(Kind::Extended);
        }
        if k.len() < n && subsequence(&k, &entity.keys) {
            kinds.push(Kind::Partial);
        }
        if n >= 2
            && k.len() == 1
            && k.first()
                .zip(entity.keys.last())
                .is_some_and(|(w, s)| s.chars().count() >= 2 && *w == format!("{s}s"))
        {
            kinds.push(Kind::Partial);
        } else if n >= 2 && k.len() == 1 && k.first().is_some_and(|word| word.chars().count() > 1) {
            let first = k.first().map(String::as_str).unwrap_or_default();
            let rel = [
                relation(
                    first,
                    entity.keys.first().map(String::as_str).unwrap_or_default(),
                ),
                relation(
                    first,
                    entity.keys.last().map(String::as_str).unwrap_or_default(),
                ),
            ];
            if rel.contains(&Some("near")) {
                kinds.push(Kind::Near);
            } else if rel.contains(&Some("variant")) {
                kinds.push(Kind::Variant);
            }
        }
        for window in k.windows(n) {
            let rel = window
                .iter()
                .zip(&entity.keys)
                .map(|(a, b)| relation(a, b))
                .collect::<Vec<_>>();
            if rel.iter().all(Option::is_some) {
                kinds.push(if rel.iter().all(|r| matches!(r, Some("eq" | "near"))) {
                    Kind::Near
                } else {
                    Kind::Variant
                });
            } else if n >= 2 && rel.contains(&Some("eq")) {
                kinds.push(Kind::Shared);
            }
        }
        for kind in kinds {
            if best.0.is_none_or(|old| kind > old) {
                best = (Some(kind), Some(shown.clone()));
            }
        }
    }
    Ok(best)
}

pub fn refers_back(message: &str) -> bool {
    let text = norm(message);
    let follow = regex::Regex::new(
        r"(?i)^\s*(?:and|also|plus|so|then|what about|how about)\b|\b(?:he|she|they|him|her|them|his|hers|their|theirs|that person)\b",
    );
    if follow.is_ok_and(|re| re.is_match(&text)) {
        return true;
    }
    let time_word = |s: &str| {
        let low = s.to_lowercase();
        let singular = low.strip_suffix('s').unwrap_or(&low);
        has(rules::MONTHS, singular)
            || has(rules::WEEKDAYS, singular)
            || matches!(
                singular,
                "week"
                    | "weekend"
                    | "morning"
                    | "afternoon"
                    | "evening"
                    | "night"
                    | "day"
                    | "month"
                    | "year"
                    | "time"
                    | "summer"
                    | "winter"
                    | "spring"
                    | "autumn"
                    | "fall"
                    | "season"
                    | "term"
                    | "quarter"
            )
    };
    let is_time_after = |end: usize| {
        let after = text.get(end..).unwrap_or_default();
        let stripped = after.trim_start_matches(py_space);
        if stripped.len() == after.len() {
            return false;
        }
        let word: String = stripped.chars().take_while(|c| py_alpha(*c)).collect();
        !word.is_empty() && time_word(&word)
    };
    if let Ok(strong) = regex::Regex::new(r"(?i)\b(?:that|those|these)\b") {
        if strong.find_iter(&text).any(|m| !is_time_after(m.end())) {
            return true;
        }
    }
    let weak = regex::Regex::new(r"(?i)\b(?:it|this)\b").ok();
    let meta=regex::Regex::new(r"(?i)\b(?:written|write|wrote|recorded|record|records|noted|logged|sure|true|correct|right|infer|inferred|inferring|guess|guessed|guessing|assume|assumed|assuming|source|sources|diary|entry|entries|made\s+up|make\s+up|making\s+up|come\s+from|came\s+from|get|got|know|knew|say|said|mention|mentioned|confirm|confirmed|certain)\b").ok();
    weak.is_some_and(|re| re.find_iter(&text).any(|m| !is_time_after(m.end())))
        && meta.is_some_and(|re| re.is_match(&text))
}

pub fn carried_entities(message: &str, earlier: &[String]) -> Result<Vec<Entity>, Error> {
    if !asked_entities(message)?.is_empty() || !refers_back(message) {
        return Ok(Vec::new());
    }
    for previous in earlier.iter().rev() {
        let found = asked_entities(previous)?;
        if !found.is_empty() {
            return Ok(found);
        }
    }
    Ok(Vec::new())
}

pub fn relational(message: &str, entities: &[Entity]) -> bool {
    let text = norm(message);
    for entity in entities {
        let name = entity
            .name
            .split_whitespace()
            .map(regex::escape)
            .collect::<Vec<_>>()
            .join(r"\.?\s+");
        if name.is_empty() {
            continue;
        }
        let possessive = format!(
            r"(?i){}(?:'s|')\s+((?:[\p{{L}}]+\s+)?[\p{{L}}]+(?:-[\p{{L}}]+)*)",
            name
        );
        if let Ok(re) = regex::Regex::new(&possessive) {
            for cap in re.captures_iter(&text) {
                let phrase = cap.get(1).map(|m| m.as_str()).unwrap_or_default();
                let parts = phrase
                    .split_whitespace()
                    .flat_map(|w| std::iter::once(w).chain(w.split('-')));
                if parts.into_iter().any(|w| {
                    let low = w.to_lowercase();
                    has(rules::RELATIONS, &low) || has(rules::RELATIONS, low.trim_end_matches('s'))
                }) {
                    return true;
                }
            }
        }
        let of = format!(
            r"(?i)\b([\p{{L}}]+(?:-[\p{{L}}]+)*)\s+of\s+(?:the\s+)?{}\b",
            name
        );
        if let Ok(re) = regex::Regex::new(&of) {
            for cap in re.captures_iter(&text) {
                let phrase = cap
                    .get(1)
                    .map(|m| m.as_str())
                    .unwrap_or_default()
                    .to_lowercase();
                if std::iter::once(phrase.as_str())
                    .chain(phrase.split('-'))
                    .any(|w| {
                        has(rules::RELATIONS, w) || has(rules::RELATIONS, w.trim_end_matches('s'))
                    })
                {
                    return true;
                }
            }
        }
    }
    false
}

#[cfg(test)]
mod frozen_parity_probes {
    use super::*;
    #[test]
    fn frozen_key_0() {
        assert_eq!(key("Ᲊ"), "Ᲊ");
    }
    #[test]
    fn frozen_key_1() {
        assert_eq!(key("𜳖"), "𜳖");
    }
    #[test]
    fn frozen_key_2() {
        assert_eq!(key("ΟΣ"), "ος");
    }
    #[test]
    fn frozen_key_3() {
        assert_eq!(key("Alderͅ"), "alder");
    }
    #[test]
    fn frozen_words_0() {
        assert_eq!(
            words("Alderͅ Brannock")
                .map(|tokens| tokens.into_iter().map(|w| w.text).collect::<Vec<_>>()),
            Ok(vec![String::from("Alder"), String::from("Brannock")])
        );
    }
    #[test]
    fn frozen_words_1() {
        assert_eq!(
            words("Alder Brannock'ſ")
                .map(|tokens| tokens.into_iter().map(|w| w.text).collect::<Vec<_>>()),
            Ok(vec![String::from("Alder"), String::from("Brannock")])
        );
    }
    #[test]
    fn frozen_words_2() {
        assert_eq!(
            words("AlderᲉ Brannock")
                .map(|tokens| tokens.into_iter().map(|w| w.text).collect::<Vec<_>>()),
            Ok(vec![String::from("Alder"), String::from("Brannock")])
        );
    }
}
