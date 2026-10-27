use crate::generated_rules as rules;
use crate::lexical::{
    asked_entities, cap_runs, carried_entities, classify, has, key, keys, near_word, norm,
    relational,
};
use crate::people::{body, identified_people, is_person};
use crate::{
    Analysis, Anchor, Entity, Error, FocusResult, Kind, NumericValue, Reason, Row, Similar,
    TemporalPartition, DEFAULT_MARGIN, EPSILON, MAX_CANDIDATES, MAX_EARLIER, MAX_EARLIER_BYTES,
    MAX_ENTITIES, MAX_NEAR_SPELLINGS, MAX_QUERY_BYTES, MAX_ROW_BYTES, MAX_TOTAL_ROW_BYTES,
    MAX_WORDS,
};
use chrono::{Datelike, NaiveDate};
use regex::Regex;
use std::collections::{BTreeMap, BTreeSet};

fn validate_anchor(anchor: &Anchor) -> bool {
    if anchor.day.len() != 10 || !anchor.day.is_ascii() {
        return false;
    }
    let Ok(day) = NaiveDate::parse_from_str(&anchor.day, "%Y-%m-%d") else {
        return false;
    };
    (1..=9999).contains(&day.year()) && day.format("%Y-%m-%d").to_string() == anchor.day
}
pub fn analyse(
    message: &str,
    earlier: &[String],
    anchor: Option<Anchor>,
) -> Result<Analysis, Error> {
    if message.len() > MAX_QUERY_BYTES
        || earlier.len() > MAX_EARLIER
        || earlier.iter().any(|s| s.len() > MAX_QUERY_BYTES)
        || earlier.iter().map(String::len).sum::<usize>() > MAX_EARLIER_BYTES
    {
        return Err(Error::Capacity);
    }
    if anchor.as_ref().is_some_and(|a| !validate_anchor(a)) {
        return Err(Error::InvalidAnchor);
    }
    let entities = asked_entities(message)?;
    let carried = if entities.is_empty() {
        carried_entities(message, earlier)?
    } else {
        Vec::new()
    };
    let found = if entities.is_empty() {
        carried.clone()
    } else {
        entities.clone()
    };
    let relation_message = if entities.is_empty() {
        earlier.last().map(String::as_str).unwrap_or("")
    } else {
        message
    };
    Ok(Analysis {
        entities: found,
        carried: !carried.is_empty(),
        anchor,
        relational: relational(
            relation_message,
            &(if entities.is_empty() {
                carried
            } else {
                entities
            }),
        ),
    })
}

fn python_float_text(text: &str) -> Option<f64> {
    if text.len() > 128 {
        return None;
    }
    let text = text.trim_matches(crate::lexical::py_space);
    let mut normalized = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '_' {
            let before = normalized
                .chars()
                .last()
                .is_some_and(|c| c.is_ascii_digit());
            let after = chars
                .peek()
                .is_some_and(|c| crate::lexical::decimal_digit(*c).is_some());
            if !before || !after {
                return None;
            }
            continue;
        }
        if let Some(d) = crate::lexical::decimal_digit(c) {
            normalized.push(char::from(b'0' + d));
        } else if c.is_ascii() {
            normalized.push(c);
        } else {
            return None;
        }
    }
    let unsigned = normalized.trim_start_matches(['+', '-']);
    if matches!(
        unsigned.to_ascii_lowercase().as_str(),
        "inf" | "infinity" | "nan"
    ) {
        return normalized.parse::<f64>().ok();
    }
    let valid = Regex::new(r"^(?:[0-9]+(?:\.[0-9]*)?|\.[0-9]+)(?:[eE][+-]?[0-9]+)?$").ok()?;
    if !valid.is_match(unsigned) {
        return None;
    }
    normalized.parse::<f64>().ok()
}
fn numeric_value(value: &NumericValue) -> Option<f64> {
    match value {
        NumericValue::Null => None,
        NumericValue::Bool(value) => Some(if *value { 1.0 } else { 0.0 }),
        NumericValue::Number(value) => Some(*value),
        NumericValue::Text(value) => python_float_text(value),
    }
}
pub fn parse_margin(value: &NumericValue) -> f64 {
    if matches!(value, NumericValue::Bool(_)) {
        return DEFAULT_MARGIN;
    }
    match numeric_value(value) {
        Some(value) if value.is_finite() && (0.0..=1.0).contains(&value) => value,
        _ => DEFAULT_MARGIN,
    }
}
fn row_text(row: &Row) -> String {
    format!("{}\n{}", row.header, row.text)
}
fn score(row: &Row) -> Option<f64> {
    numeric_value(&row.score)
}
fn about(kind: Option<Kind>) -> bool {
    kind.is_some_and(Kind::about)
}
fn plural_of(word: &str, surname: &str) -> bool {
    surname.chars().count() >= 2 && word == format!("{surname}s")
}
fn ascii_words(value: &str, letters_only: bool) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    for c in value.chars() {
        if c.is_ascii_lowercase() || (!letters_only && c.is_ascii_digit()) {
            current.push(c);
        } else if !current.is_empty() {
            out.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}
fn plain(value: &str) -> String {
    format!(" {} ", ascii_words(&key(&norm(value)), false).join(" "))
}
fn could_refer(
    found: &[(Option<Kind>, Option<String>)],
    entities: &[Entity],
    matched: &[Entity],
    text: &str,
) -> bool {
    let p = plain(text);
    for ((kind, _), entity) in found.iter().zip(entities) {
        if about(*kind) || matches!(kind, Some(Kind::Near | Kind::Variant)) {
            return true;
        }
        if *kind == Some(Kind::Shared) && !matched.contains(entity) {
            return true;
        }
        if (!entity.keys.is_empty()
            && (entity.keys.len() >= 2
                || entity.keys.first().is_some_and(|k| k.chars().count() >= 3)))
            && p.contains(&format!(" {} ", entity.keys.join(" ")))
        {
            return true;
        }
    }
    false
}
fn same_given_near_surname(text: &str, entity: &Entity) -> Result<Option<String>, Error> {
    if entity.keys.len() != 2 {
        return Ok(None);
    }
    let cleaned = crate::people::clean(text)?;
    for run in cap_runs(&cleaned, false)? {
        let ws = run
            .iter()
            .map(|(w, _)| w.clone())
            .filter(|w| !has(rules::TITLES, &key(w)))
            .collect::<Vec<_>>();
        let ks = keys(&ws);
        if ks.len() != ws.len() {
            continue;
        }
        for (i, pair) in ks.windows(2).enumerate() {
            if let (Some(first), Some(last), Some(given), Some(surname)) = (
                pair.first(),
                pair.get(1),
                entity.keys.first(),
                entity.keys.get(1),
            ) {
                if first != given || !near_word(last, surname) || plural_of(last, surname) {
                    continue;
                }
                let shown = ws
                    .get(i)
                    .zip(ws.get(i + 1))
                    .map(|(a, b)| format!("{a} {b}"));
                if shown.is_some() {
                    return Ok(shown);
                }
            }
        }
    }
    Ok(None)
}
fn order(row: &Row) -> (String, String) {
    let day = row
        .day
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or("")
        .to_owned();
    let mut chars = row
        .header
        .trim_start_matches(crate::lexical::py_space)
        .chars()
        .peekable();
    let mut time = String::new();
    for _ in 0..2 {
        if chars
            .peek()
            .is_some_and(|c| crate::lexical::decimal_digit(*c).is_some())
        {
            if let Some(c) = chars.next() {
                time.push(c);
            }
        } else {
            break;
        }
    }
    if time.is_empty() || chars.next() != Some(':') {
        return (day, String::new());
    }
    time.push(':');
    for _ in 0..2 {
        match chars.next() {
            Some(c) if crate::lexical::decimal_digit(c).is_some() => time.push(c),
            _ => return (day, String::new()),
        }
    }
    let width = time.chars().count();
    let padded = if width < 5 {
        format!("{}{}", "0".repeat(5 - width), time)
    } else {
        time
    };
    (day, padded)
}
fn entity_sentences(text: &str, entity: &Entity) -> Result<Vec<String>, Error> {
    let re = Regex::new(r"[^.!?\n]+").map_err(|_| Error::Internal)?;
    let mut out = Vec::new();
    for found in re.find_iter(&norm(text)) {
        if about(classify(found.as_str(), entity)?.0) {
            out.push(found.as_str().to_owned());
        }
    }
    Ok(out)
}
fn content_words(sentences: &[String], entity: &Entity) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for sentence in sentences {
        for word in ascii_words(&key(&norm(sentence)), true) {
            if word.len() >= 3
                && !has(rules::NOT_NAMES, &word)
                && !has(rules::CUE_WORDS, &word)
                && !entity.keys.contains(&word)
            {
                out.insert(word);
            }
        }
    }
    out
}
fn superseded(
    ranked: &[usize],
    rows: &[Row],
    matched: &[Entity],
) -> Result<Vec<(usize, String)>, Error> {
    use crate::regex_profile::{self as rx, Pattern};
    use std::sync::OnceLock;
    static CUE: OnceLock<Result<Vec<Pattern>, Error>> = OnceLock::new();
    static OLD: OnceLock<Result<Pattern, Error>> = OnceLock::new();
    let cue = CUE
        .get_or_init(|| {
            let sp = format!("{}+", rx::SPACE);
            let i = rx::literal("I");
            [
                rx::literal("correction"),
                format!(
                    "{i}{sp}(?:{}{sp})?(?:{})",
                    rx::literal("had"),
                    rx::alternatives(&["misheard", "misremembered", "misspoke", "misread"])
                ),
                format!(
                    "{i}{sp}(?:{}){sp}(?:{}){sp}{}",
                    rx::alternatives(&["got", "had"]),
                    rx::alternatives(&["it", "that", "this"]),
                    rx::literal("wrong")
                ),
                format!(
                    "{}{sp}{}{sp}(?:{}){sp}(?:(?:{}){sp})?(?:{})",
                    rx::literal("to"),
                    rx::literal("correct"),
                    rx::alternatives(&["my", "an", "the", "that"]),
                    rx::alternatives(&["earlier", "previous"]),
                    rx::alternatives(&["note", "entry"])
                ),
            ]
            .iter()
            .map(|body| Pattern::new(body, true, true))
            .collect()
        })
        .as_ref()
        .map_err(|e| *e)?;
    let old_value = OLD
        .get_or_init(|| {
            let sp = format!("{}+", rx::SPACE);
            let phrase = format!(
                "(?:{}|{}{sp}{}|{}{sp}{})",
                rx::literal("not"),
                rx::literal("instead"),
                rx::literal("of"),
                rx::literal("rather"),
                rx::literal("than")
            );
            Pattern::new(
                &format!(
                    r"{phrase}{sp}(?:(?:{}){sp})?([^.,;:!?\n]{{2,60}})",
                    rx::alternatives(&["the", "a", "an"])
                ),
                true,
                false,
            )
        })
        .as_ref()
        .map_err(|e| *e)?;
    let mut out = BTreeMap::<usize, ((String, String), String)>::new();
    for fix_rank in ranked {
        let Some(fix) = fix_rank.checked_sub(1).and_then(|i| rows.get(i)) else {
            return Err(Error::InvalidPartition);
        };
        for entity in matched {
            let said = entity_sentences(&fix.text, entity)?;
            if said.is_empty()
                || !(cue.iter().any(|re| re.is_match(&fix.header))
                    || said.iter().any(|s| cue.iter().any(|re| re.is_match(s))))
            {
                continue;
            }
            let old = said
                .iter()
                .flat_map(|s| old_value.find_iter(s))
                .filter_map(|c| c.capture.map(plain))
                .filter(|s| !s.trim_matches(crate::lexical::py_space).is_empty())
                .collect::<Vec<_>>();
            let attribute = content_words(&said, entity);
            for rank in ranked {
                if rank == fix_rank {
                    continue;
                }
                let Some(row) = rank.checked_sub(1).and_then(|i| rows.get(i)) else {
                    return Err(Error::InvalidPartition);
                };
                if order(row) >= order(fix) {
                    continue;
                }
                let earlier = row_text(row);
                if !about(classify(&earlier, entity)?.0) {
                    continue;
                }
                let hit = if old.is_empty() {
                    attribute
                        .intersection(&content_words(&[earlier], entity))
                        .take(2)
                        .count()
                        >= 2
                } else {
                    old.iter().any(|value| plain(&earlier).contains(value))
                };
                if hit && out.get(rank).is_none_or(|(before, _)| order(fix) > *before) {
                    out.insert(
                        *rank,
                        (
                            order(fix),
                            fix.day.clone().unwrap_or_else(|| "None".to_owned()),
                        ),
                    );
                }
            }
        }
    }
    Ok(out
        .into_iter()
        .map(|(rank, (_, day))| (rank, day))
        .collect())
}

pub fn focus_candidates(
    analysis: &Analysis,
    rows: &[Row],
    partition: &TemporalPartition,
    limit: Option<usize>,
    margin: &NumericValue,
) -> Result<FocusResult, Error> {
    if rows.len() > MAX_CANDIDATES
        || rows.iter().any(|r| {
            r.header.len().saturating_add(r.text.len()) > MAX_ROW_BYTES
                || r.day.as_ref().is_some_and(|d| d.len() > 64)
                || matches!(&r.score,NumericValue::Text(s) if s.len()>128)
        })
    {
        return Err(Error::Capacity);
    }
    if rows
        .iter()
        .map(|r| r.header.len().saturating_add(r.text.len()))
        .sum::<usize>()
        > MAX_TOTAL_ROW_BYTES
        || analysis.entities.len() > MAX_ENTITIES
        || analysis.entities.iter().any(|e| {
            e.name.len() > MAX_QUERY_BYTES
                || e.keys.len() > MAX_WORDS
                || e.keys.iter().any(|k| k.len() > MAX_QUERY_BYTES)
        })
        || limit.is_some_and(|v| v > MAX_CANDIDATES)
        || matches!(margin,NumericValue::Text(s) if s.len()>128)
    {
        return Err(Error::Capacity);
    }
    if analysis
        .anchor
        .as_ref()
        .is_some_and(|a| !validate_anchor(a))
    {
        return Err(Error::InvalidAnchor);
    }
    partition.validate(rows.len())?;
    if analysis.anchor.is_none() && !partition.removed.is_empty() {
        return Err(Error::InvalidPartition);
    }
    let margin = parse_margin(margin);
    let mut result = FocusResult {
        analysis: analysis.clone(),
        margin,
        matched: Vec::new(),
        kept_ranks: Vec::new(),
        removed: partition
            .removed
            .iter()
            .map(|rank| (*rank, Reason::AfterTimeAnchor))
            .collect(),
        similar: Vec::new(),
        corrected_by: Vec::new(),
        absent: Vec::new(),
        exact: Vec::new(),
    };
    let mut ranked = partition.survivors.clone();
    if !analysis.entities.is_empty() {
        let mut kinds = BTreeMap::<usize, Vec<(Option<Kind>, Option<String>)>>::new();
        for rank in &ranked {
            let Some(row) = rank.checked_sub(1).and_then(|i| rows.get(i)) else {
                return Err(Error::InvalidPartition);
            };
            let text = row_text(row);
            let found = analysis
                .entities
                .iter()
                .map(|e| classify(&text, e))
                .collect::<Result<Vec<_>, _>>()?;
            kinds.insert(*rank, found);
        }
        result.matched = analysis
            .entities
            .iter()
            .enumerate()
            .filter_map(|(i, e)| {
                kinds
                    .values()
                    .any(|found| {
                        found
                            .get(i)
                            .is_some_and(|(kind, _)| *kind == Some(Kind::Match))
                    })
                    .then_some(e.clone())
            })
            .collect();
        let arming = result
            .matched
            .iter()
            .filter(|e| {
                e.keys.len() >= 2
                    || e.name
                        .split(crate::lexical::py_space)
                        .find(|s| !s.is_empty())
                        .is_some_and(|name| has(rules::TITLES, &key(name)))
            })
            .collect::<Vec<_>>();
        let mut best = None::<f64>;
        if result.matched.len() == analysis.entities.len() {
            for rank in &ranked {
                let Some(row) = rank.checked_sub(1).and_then(|i| rows.get(i)) else {
                    return Err(Error::InvalidPartition);
                };
                if kinds.get(rank).is_some_and(|found| {
                    found.iter().enumerate().any(|(i, (kind, _))| {
                        *kind == Some(Kind::Match)
                            && analysis
                                .entities
                                .get(i)
                                .is_some_and(|e| arming.contains(&e))
                    })
                }) {
                    if let Some(next) = score(row) {
                        if best.is_none_or(|current| next > current) {
                            best = Some(next);
                        }
                    }
                }
            }
        }
        let people_asked = if analysis.relational {
            Vec::new()
        } else {
            result
                .matched
                .iter()
                .filter(|e| is_person(e))
                .cloned()
                .collect::<Vec<_>>()
        };
        let absent = if analysis.relational {
            Vec::new()
        } else {
            analysis
                .entities
                .iter()
                .filter(|e| !result.matched.contains(e) && e.keys.len() == 2 && is_person(e))
                .cloned()
                .collect::<Vec<_>>()
        };
        let mut survivors = Vec::new();
        for rank in ranked {
            let Some(row) = rank.checked_sub(1).and_then(|i| rows.get(i)) else {
                return Err(Error::InvalidPartition);
            };
            let found = kinds.get(&rank).ok_or(Error::Internal)?;
            if found.iter().any(|(kind, _)| about(*kind)) {
                survivors.push(rank);
                let named = found
                    .iter()
                    .enumerate()
                    .filter_map(|(i, (kind, _))| {
                        (*kind == Some(Kind::Match))
                            .then(|| analysis.entities.get(i).map(|e| e.name.clone()))
                            .flatten()
                    })
                    .collect::<Vec<_>>();
                if !named.is_empty() {
                    result.exact.push((rank, named));
                }
                continue;
            }
            if found.iter().enumerate().any(|(i, (kind, _))| {
                *kind == Some(Kind::Near)
                    && analysis
                        .entities
                        .get(i)
                        .is_some_and(|e| result.matched.contains(e))
            }) {
                result.removed.push((rank, Reason::NearName));
                continue;
            }
            let mut near = Vec::new();
            for (i, (kind, _)) in found.iter().enumerate() {
                if *kind == Some(Kind::Near) {
                    if let Some(entity) = analysis.entities.get(i).filter(|e| absent.contains(e)) {
                        if let Some(shown) = same_given_near_surname(&row_text(row), entity)? {
                            near.push((entity.clone(), shown));
                        }
                    }
                }
            }
            if !near.is_empty() {
                result.removed.push((rank, Reason::NearNameUnmatched));
                for (entity, shown) in near {
                    if let Some((_, spellings)) =
                        result.absent.iter_mut().find(|(e, _)| *e == entity)
                    {
                        if !spellings.contains(&shown) && spellings.len() < MAX_NEAR_SPELLINGS {
                            spellings.push(shown);
                        }
                    } else {
                        result.absent.push((entity, vec![shown]));
                    }
                }
                continue;
            }
            if !could_refer(found, &analysis.entities, &result.matched, &row_text(row))
                && !analysis.relational
            {
                if !people_asked.is_empty()
                    && !identified_people(&body(&row.text)?, Some(&analysis.entities))?.is_empty()
                {
                    result.removed.push((rank, Reason::OtherPerson));
                    continue;
                }
                if let (Some(best), Some(score)) = (best, score(row)) {
                    if best - score > margin + EPSILON {
                        result.removed.push((rank, Reason::BelowMargin));
                        continue;
                    }
                }
            }
            survivors.push(rank);
            let similar = found
                .iter()
                .enumerate()
                .filter_map(|(i, (kind, shown))| {
                    kind.zip(shown.as_ref()).and_then(|(k, s)| {
                        analysis
                            .entities
                            .get(i)
                            .map(|e| (k, s.clone(), e.name.clone()))
                    })
                })
                .max_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
            if let Some((kind, shown, asked)) = similar {
                result.similar.push((rank, Similar { kind, shown, asked }));
            }
        }
        ranked = survivors;
    }
    if let Some(limit) = limit {
        if ranked.len() > limit {
            result.removed.extend(
                ranked
                    .iter()
                    .skip(limit)
                    .map(|rank| (*rank, Reason::CandidateLimit)),
            );
            ranked.truncate(limit);
            result.similar.retain(|(rank, _)| ranked.contains(rank));
            result.exact.retain(|(rank, _)| ranked.contains(rank));
        }
    }
    result.corrected_by = superseded(&ranked, rows, &result.matched)?;
    result.kept_ranks = ranked;
    result.removed.sort_by_key(|(rank, _)| *rank);
    Ok(result)
}

#[cfg(test)]
mod frozen_order_probes {
    use super::*;
    #[test]
    fn frozen_order_0() {
        let row = Row {
            day: Some("2030-01-20".into()),
            header: "١:00".into(),
            text: String::new(),
            score: NumericValue::Null,
        };
        assert_eq!(order(&row), ("2030-01-20".into(), "0١:00".into()));
    }
    #[test]
    fn frozen_order_1() {
        let row = Row {
            day: Some("2030-01-20".into()),
            header: "".into(),
            text: String::new(),
            score: NumericValue::Null,
        };
        assert_eq!(order(&row), ("2030-01-20".into(), "".into()));
    }
    #[test]
    fn frozen_order_2() {
        let row = Row {
            day: Some("2030-01-20".into()),
            header: "١:00".into(),
            text: String::new(),
            score: NumericValue::Null,
        };
        assert_eq!(order(&row), ("2030-01-20".into(), "0١:00".into()));
    }
    #[test]
    fn frozen_order_3() {
        let row = Row {
            day: Some("2030-01-20".into()),
            header: "𞗱:00".into(),
            text: String::new(),
            score: NumericValue::Null,
        };
        assert_eq!(order(&row), ("2030-01-20".into(), "".into()));
    }
}
