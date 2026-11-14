//! noevia-core's `server/tool-router.cjs` in Rust: task-conditional toolbox selection before the first
//! model call (`route`) and the cosine it ranks by (`cosine`). Pure: the caller gives the toolbox rows
//! with their precomputed embeddings, the task's embedding, the deployment ceiling and the cap. Unused
//! until a native chat preparation calls it (core's caller is `chat-tool-routing.cjs`).
//!
//! The JS is followed rule by rule, including its edges:
//! - `cosine` runs to the shorter vector and is `0` when either norm is `0` or NaN (`na && nb`);
//! - the user's selection and the fallback keep first-seen order and drop duplicates (a `Set`);
//!   `byId` is a `Map`, so of two rows with one id the later one is looked up, while ranking still
//!   walks every row;
//! - ranking keeps `score >= threshold` (a NaN threshold keeps nothing), sorts by score descending
//!   then id by `localeCompare`, and takes `slice(0, topK)` (a negative `topK` counts from the end,
//!   a fraction is truncated, NaN is `0`);
//! - a box's `requires` closure is depth-first, all-or-nothing, refused when any member is missing or
//!   not eligible; a closure that brings a tool name already loaded, or passes the cap, loads nothing
//!   (`current.size + adding.length > maxTools`: an undefined or NaN cap never refuses);
//! - `reasons` is a JS object: a key set again keeps its place, and array-index keys ("0", "17") come
//!   first in ascending order when it is read ([`Routed::reasons`]);
//! - `score.toFixed(2)` rounds the exact value half away from zero.
//!
//! Named difference: `localeCompare` (ICU root collation) is reproduced for ASCII ids (punctuation,
//! then digits, then letters case-insensitively with lower case first on a tie); ids with other
//! characters fall back to code-point order after the ASCII rule. Ties need two boxes with exactly
//! the same score, so this only orders equal scores.
use js_json::JValue;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

/// Node's default `topK`.
pub const DEFAULT_TOP_K: f64 = 3.0;
/// Node's default `threshold`.
pub const DEFAULT_THRESHOLD: f64 = 0.35;

/// One manifest row.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ToolBox {
    pub id: String,
    /// `tools` (`|| []`).
    pub tools: Vec<String>,
    /// `autoLoad === 'never'`.
    pub never_auto_load: bool,
    /// `requires` (`|| []`).
    pub requires: Vec<String>,
    /// `embedding` when it is an array.
    pub embedding: Option<Vec<f64>>,
}

/// `route`'s input.
pub struct Input<'a> {
    pub boxes: &'a [ToolBox],
    /// `taskEmbedding`: `None` when embeddings are unavailable.
    pub task_embedding: Option<&'a [f64]>,
    /// `offered(id)`: the deployment ceiling.
    pub offered: &'a dyn Fn(&str) -> bool,
    pub user_selection: &'a [String],
    pub fallback: &'a [String],
    /// `maxTools` (`f64::NAN` for an undefined cap).
    pub max_tools: f64,
    pub top_k: f64,
    pub threshold: f64,
}

/// One `skipped` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    pub id: String,
    pub reason: &'static str,
}

/// `route`'s answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Routed {
    /// `[...chosen]`, in insertion order.
    pub boxes: Vec<String>,
    /// `reasons` in insertion order (a key set again keeps its first place).
    reasons: Vec<(String, String)>,
    pub skipped: Vec<Skipped>,
    pub routed: bool,
}

impl Routed {
    /// `reasons` as JS reads the object back: array-index keys first (ascending), then the rest in
    /// insertion order.
    pub fn reasons(&self) -> Vec<(&str, &str)> {
        let mut index: Vec<(u32, &str, &str)> = vec![];
        let mut rest: Vec<(&str, &str)> = vec![];
        for (k, v) in &self.reasons {
            match array_index(k) {
                Some(i) => index.push((i, k, v)),
                None => rest.push((k, v)),
            }
        }
        index.sort_by_key(|(i, _, _)| *i);
        index
            .into_iter()
            .map(|(_, k, v)| (k, v))
            .chain(rest)
            .collect()
    }

    /// `reasons[id]`.
    pub fn reason(&self, id: &str) -> Option<&str> {
        self.reasons
            .iter()
            .find(|(k, _)| k == id)
            .map(|(_, v)| v.as_str())
    }

    /// The object `route` returns, as `JSON.stringify` writes it.
    pub fn to_json(&self) -> JValue {
        JValue::obj([
            (
                "boxes",
                JValue::Arr(
                    self.boxes
                        .iter()
                        .map(|b| JValue::from(b.as_str()))
                        .collect(),
                ),
            ),
            (
                "reasons",
                JValue::Obj(
                    self.reasons()
                        .into_iter()
                        .map(|(k, v)| (k.to_owned(), JValue::from(v)))
                        .collect(),
                ),
            ),
            (
                "skipped",
                JValue::Arr(
                    self.skipped
                        .iter()
                        .map(|s| {
                            JValue::obj([
                                ("id", JValue::from(s.id.as_str())),
                                ("reason", JValue::from(s.reason)),
                            ])
                        })
                        .collect(),
                ),
            ),
            ("routed", JValue::Bool(self.routed)),
        ])
    }

    fn set_reason(&mut self, id: &str, text: String) {
        match self.reasons.iter_mut().find(|(k, _)| k == id) {
            Some((_, v)) => *v = text,
            None => self.reasons.push((id.to_owned(), text)),
        }
    }
}

/// A canonical array index ("0".."4294967294"), as JS orders object keys.
fn array_index(key: &str) -> Option<u32> {
    if key.is_empty() || (key.len() > 1 && key.starts_with('0')) {
        return None;
    }
    if !key.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    key.parse::<u32>().ok().filter(|i| *i != u32::MAX)
}

/// `cosine(a, b)`.
pub fn cosine(a: &[f64], b: &[f64]) -> f64 {
    let (mut dot, mut na, mut nb) = (0.0, 0.0, 0.0);
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    // `na && nb`: 0 and NaN are falsy.
    let truthy = |v: f64| v != 0.0 && !v.is_nan();
    if truthy(na) && truthy(nb) {
        dot / (na * nb).sqrt()
    } else {
        0.0
    }
}

/// An insertion-ordered set of ids.
#[derive(Default)]
struct Ordered {
    items: Vec<String>,
    seen: HashSet<String>,
}

impl Ordered {
    fn has(&self, id: &str) -> bool {
        self.seen.contains(id)
    }
    fn add(&mut self, id: &str) {
        if self.seen.insert(id.to_owned()) {
            self.items.push(id.to_owned());
        }
    }
}

/// `closure(id, byId, eligible, seen)`: `false` when a dependency is missing or not eligible.
fn closure(
    id: &str,
    by_id: &HashMap<&str, &ToolBox>,
    eligible: &dyn Fn(&ToolBox) -> bool,
    seen: &mut Ordered,
) -> bool {
    if seen.has(id) {
        return true;
    }
    let Some(b) = by_id.get(id) else {
        return false;
    };
    if !eligible(b) {
        return false;
    }
    seen.add(id);
    b.requires
        .iter()
        .all(|dep| closure(dep, by_id, eligible, seen))
}

/// `slice(0, k)`'s end over `len` items.
fn slice_end(len: usize, k: f64) -> usize {
    if k.is_nan() {
        return 0;
    }
    let k = k.trunc();
    if k < 0.0 {
        let back = (-k).min(len as f64) as usize;
        len - back
    } else {
        k.min(len as f64) as usize
    }
}

/// `x.toFixed(2)` for `|x| < 1e21`: the exact value rounded half away from zero.
pub fn to_fixed_2(x: f64) -> String {
    if !x.is_finite() || x.abs() >= 1e21 {
        return js_json::number_to_string(x);
    }
    // Rust prints the exact decimal expansion for a large enough precision (1074 places covers
    // every f64 fraction).
    let exact = format!("{:.1100}", x.abs());
    let (int, frac) = exact.split_once('.').unwrap_or((exact.as_str(), ""));
    let digits: Vec<u8> = frac.bytes().map(|b| b - b'0').collect();
    let mut n: u128 = int.parse::<u128>().unwrap_or(0) * 100
        + u128::from(digits.first().copied().unwrap_or(0)) * 10
        + u128::from(digits.get(1).copied().unwrap_or(0));
    if digits.get(2).copied().unwrap_or(0) >= 5 {
        n += 1;
    }
    let sign = if x < 0.0 { "-" } else { "" };
    format!("{sign}{}.{:02}", n / 100, n % 100)
}

/// ICU root collation weights for ASCII: (primary class, primary, tertiary).
fn weight(c: char) -> (u8, u32, u8) {
    // CLDR root order of the ASCII punctuation and symbols (non-ignorable).
    const PUNCT: &str = "\t\n\u{b}\u{c}\r _-,;:!?.'\"()[]{}@*/\\&#%`^+<=>|~$";
    if let Some(i) = PUNCT.find(c) {
        return (0, i as u32, 0);
    }
    if c.is_ascii_digit() {
        return (1, c as u32, 0);
    }
    if c.is_ascii_alphabetic() {
        let tertiary = u8::from(c.is_ascii_uppercase());
        return (2, c.to_ascii_lowercase() as u32, tertiary);
    }
    (3, c as u32, 0)
}

/// `a.localeCompare(b)` (see the module doc for its reach).
pub fn locale_compare(a: &str, b: &str) -> Ordering {
    let primary = |s: &str| -> Vec<(u8, u32)> {
        s.chars()
            .filter(|c| !c.is_control() || c.is_ascii_whitespace())
            .map(|c| {
                let (class, w, _) = weight(c);
                (class, w)
            })
            .collect()
    };
    let tertiary = |s: &str| -> Vec<u8> { s.chars().map(|c| weight(c).2).collect() };
    primary(a)
        .cmp(&primary(b))
        .then_with(|| tertiary(a).cmp(&tertiary(b)))
        .then_with(|| a.cmp(b))
}

/// `route(input)`.
pub fn route(input: &Input<'_>) -> Routed {
    let offered = input.offered;
    let mut by_id: HashMap<&str, &ToolBox> = HashMap::new();
    for b in input.boxes {
        by_id.insert(b.id.as_str(), b);
    }
    let tools_of = |ids: &[String]| -> Vec<&str> {
        ids.iter()
            .flat_map(|id| {
                by_id
                    .get(id.as_str())
                    .map(|b| b.tools.iter().map(String::as_str).collect::<Vec<_>>())
                    .unwrap_or_default()
            })
            .collect()
    };
    let mut chosen = Ordered::default();
    for id in input.user_selection {
        if by_id.contains_key(id.as_str()) && offered(id) {
            chosen.add(id);
        }
    }
    let mut out = Routed {
        boxes: vec![],
        reasons: vec![],
        skipped: vec![],
        routed: true,
    };
    for id in &chosen.items {
        out.set_reason(id, "selected by you".to_owned());
    }
    let task = match input.task_embedding {
        Some(t) if !t.is_empty() => t,
        _ => {
            for id in input.fallback {
                if by_id.contains_key(id.as_str()) && offered(id) && !chosen.has(id) {
                    chosen.add(id);
                    out.set_reason(id, "project selection (router unavailable)".to_owned());
                }
            }
            out.boxes = chosen.items;
            out.routed = false;
            return out;
        }
    };
    let eligible = |b: &ToolBox| offered(&b.id) && !b.never_auto_load;
    let mut ranked: Vec<(&str, f64)> = input
        .boxes
        .iter()
        .filter(|b| eligible(*b) && !chosen.has(&b.id))
        .filter_map(|b| {
            b.embedding
                .as_deref()
                .map(|e| (b.id.as_str(), cosine(task, e)))
        })
        .filter(|(_, score)| *score >= input.threshold)
        .collect();
    // `b.score - a.score || a.id.localeCompare(b.id)`: the scores are never NaN here.
    ranked.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(Ordering::Equal)
            .then_with(|| locale_compare(a.0, b.0))
    });
    ranked.truncate(slice_end(ranked.len(), input.top_k));
    for (id, score) in ranked {
        let mut group = Ordered::default();
        if !closure(id, &by_id, &eligible, &mut group) {
            out.skipped.push(Skipped {
                id: id.to_owned(),
                reason: "a required toolbox is unavailable",
            });
            continue;
        }
        let adding: Vec<String> = group
            .items
            .iter()
            .filter(|x| !chosen.has(x))
            .cloned()
            .collect();
        let current: HashSet<&str> = tools_of(&chosen.items).into_iter().collect();
        let added = tools_of(&adding);
        // Never load two boxes exposing the same tool name; the higher-scoring one is already in.
        if added.iter().any(|name| current.contains(name)) {
            out.skipped.push(Skipped {
                id: id.to_owned(),
                reason: "duplicates a loaded tool name",
            });
            continue;
        }
        // Whole boxes only: if the closure would break the cap, load nothing extra.
        if (current.len() + added.len()) as f64 > input.max_tools {
            out.skipped.push(Skipped {
                id: id.to_owned(),
                reason: "would exceed the tool cap",
            });
            continue;
        }
        for x in &adding {
            chosen.add(x);
            let text = if x == id {
                format!("matched this task ({})", to_fixed_2(score))
            } else {
                format!("required by {id}")
            };
            out.set_reason(x, text);
        }
    }
    out.boxes = chosen.items;
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_fixed_rounds_the_exact_value_half_away_from_zero() {
        // 0.125 and 0.375 are exact: JS picks the larger n.
        assert_eq!(to_fixed_2(0.125), "0.13");
        assert_eq!(to_fixed_2(0.375), "0.38");
        assert_eq!(to_fixed_2(-0.125), "-0.13");
        // 0.345 is 0.34499999999999997335...: down.
        assert_eq!(to_fixed_2(0.345), "0.34");
        // 1.005 is 1.00499999999999989...: down, as JS (1.005).toFixed(2) === '1.00'.
        assert_eq!(to_fixed_2(1.005), "1.00");
        assert_eq!(to_fixed_2(0.999), "1.00");
        assert_eq!(to_fixed_2(1.0), "1.00");
        assert_eq!(to_fixed_2(0.0), "0.00");
        assert_eq!(to_fixed_2(-0.0), "0.00");
        assert_eq!(to_fixed_2(-0.004), "-0.00");
        assert_eq!(to_fixed_2(0.35), "0.35");
        assert_eq!(to_fixed_2(0.7071067811865475), "0.71");
    }

    #[test]
    fn cosine_edges() {
        assert_eq!(cosine(&[1.0, 0.0], &[1.0, 0.0]), 1.0);
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 0.0]), 0.0);
        assert_eq!(cosine(&[f64::NAN], &[1.0]), 0.0);
        assert_eq!(cosine(&[], &[1.0]), 0.0);
        // The shorter length wins: [1, 5] vs [1] is 1.
        assert_eq!(cosine(&[1.0, 5.0], &[1.0]), 1.0);
        assert_eq!(cosine(&[1.0, 0.0], &[-1.0, 0.0]), -1.0);
    }

    #[test]
    fn slice_end_follows_js() {
        assert_eq!(slice_end(5, 3.0), 3);
        assert_eq!(slice_end(2, 3.0), 2);
        assert_eq!(slice_end(5, -1.0), 4);
        assert_eq!(slice_end(5, -9.0), 0);
        assert_eq!(slice_end(5, 2.9), 2);
        assert_eq!(slice_end(5, -0.5), 5);
        assert_eq!(slice_end(5, f64::NAN), 0);
        assert_eq!(slice_end(5, f64::INFINITY), 5);
    }

    #[test]
    fn array_index_keys() {
        assert_eq!(array_index("0"), Some(0));
        assert_eq!(array_index("17"), Some(17));
        assert_eq!(array_index("017"), None);
        assert_eq!(array_index("4294967294"), Some(4_294_967_294));
        assert_eq!(array_index("4294967295"), None);
        assert_eq!(array_index("-1"), None);
        assert_eq!(array_index("web"), None);
        assert_eq!(array_index(""), None);
    }

    #[test]
    fn locale_compare_ascii() {
        assert_eq!(locale_compare("a", "B"), Ordering::Less);
        assert_eq!(locale_compare("a", "A"), Ordering::Less);
        assert_eq!(locale_compare("web-crawl", "web-search"), Ordering::Less);
        assert_eq!(locale_compare("web_x", "web-x"), Ordering::Less);
        assert_eq!(locale_compare("web-x", "webx"), Ordering::Less);
        assert_eq!(locale_compare("box2", "box10"), Ordering::Greater);
        assert_eq!(locale_compare("9", "a"), Ordering::Less);
        assert_eq!(locale_compare("same", "same"), Ordering::Equal);
    }
}
