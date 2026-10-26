//! Diary temporal anchor parser and ranked candidate cutoff.
#![forbid(unsafe_code)]
use chrono::{Datelike, NaiveDate, Weekday};
use regex::{Captures, Regex};
use std::sync::OnceLock;
use unicode_normalization::{char::canonical_combining_class, UnicodeNormalization};
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeAnchor {
    pub day: NaiveDate,
    pub inclusive: bool,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate<T> {
    pub id: T,
    pub day: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cutoff<T> {
    pub kept: Vec<Candidate<T>>,
    pub removed: Vec<(Candidate<T>, &'static str)>,
}
fn re() -> Option<&'static Regex> {
    static R: OnceLock<Option<Regex>> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(concat!(
            r"(?i)\b(?P<kw>as\s+of|as\s+at|up\s+to|until|till|before)\s+(?:the\s+)?(?:",
            r"(?P<iso>\d{4}-\d{2}-\d{2})|",
            r"(?P<m2>[A-Za-z]{3,9})\.?\s+(?P<d2>\d{1,2})(?:st|nd|rd|th)?,?\s+(?P<y2>\d{4})|",
            r"(?P<d1>\d{1,2})(?:st|nd|rd|th)?\s+(?P<m1>[A-Za-z]{3,9})\.?[,]?\s+(?P<y1>\d{4})",
            r")\b"
        ))
        .ok()
    })
    .as_ref()
}
const M: [&str; 12] = [
    "january",
    "february",
    "march",
    "april",
    "may",
    "june",
    "july",
    "august",
    "september",
    "october",
    "november",
    "december",
];
const T: [&str; 23] = [
    "dr",
    "mr",
    "mrs",
    "ms",
    "mx",
    "miss",
    "prof",
    "professor",
    "sir",
    "dame",
    "rev",
    "revd",
    "fr",
    "lord",
    "lady",
    "aunt",
    "auntie",
    "uncle",
    "cousin",
    "nan",
    "nana",
    "grandma",
    "grandpa",
];
const DECIMAL_ZEROES: [u32; 68] = [
    0x30, 0x660, 0x6F0, 0x7C0, 0x966, 0x9E6, 0xA66, 0xAE6, 0xB66, 0xBE6, 0xC66, 0xCE6, 0xD66,
    0xDE6, 0xE50, 0xED0, 0xF20, 0x1040, 0x1090, 0x17E0, 0x1810, 0x1946, 0x19D0, 0x1A80, 0x1A90,
    0x1B50, 0x1BB0, 0x1C40, 0x1C50, 0xA620, 0xA8D0, 0xA900, 0xA9D0, 0xA9F0, 0xAA50, 0xABF0, 0xFF10,
    0x104A0, 0x10D30, 0x11066, 0x110F0, 0x11136, 0x111D0, 0x112F0, 0x11450, 0x114D0, 0x11650,
    0x116C0, 0x11730, 0x118E0, 0x11950, 0x11C50, 0x11D50, 0x11DA0, 0x11F50, 0x16A60, 0x16AC0,
    0x16B50, 0x1D7CE, 0x1D7D8, 0x1D7E2, 0x1D7EC, 0x1D7F6, 0x1E140, 0x1E2F0, 0x1E4F0, 0x1E950,
    0x1FBF0,
];

fn decimal_value(c: char) -> Option<u32> {
    let cp = u32::from(c);
    DECIMAL_ZEROES
        .iter()
        .find_map(|zero| (cp >= *zero && cp < *zero + 10).then(|| cp - *zero))
}
fn decimal_number(s: &str) -> Option<u32> {
    s.chars().try_fold(0u32, |value, c| {
        value.checked_mul(10)?.checked_add(decimal_value(c)?)
    })
}
fn decimal_year(s: &str) -> Option<i32> {
    i32::try_from(decimal_number(s)?).ok()
}
fn month(s: &str) -> Option<u32> {
    let l = s.to_ascii_lowercase();
    M.iter()
        .position(|m| *m == l)
        .map(|i| i as u32 + 1)
        .or_else(|| {
            if l == "sept" {
                Some(9)
            } else {
                M.iter()
                    .position(|m| m.get(..3) == Some(l.as_str()))
                    .map(|i| i as u32 + 1)
            }
        })
}
fn parsed(m: &Captures<'_>) -> Option<NaiveDate> {
    if let Some(s) = m.name("iso") {
        return NaiveDate::parse_from_str(s.as_str(), "%Y-%m-%d")
            .ok()
            .filter(|day| day.year() > 0);
    }
    let (a, b, c) = if let Some(x) = m.name("m2") {
        (
            month(x.as_str())?,
            decimal_number(m.name("d2")?.as_str())?,
            decimal_year(m.name("y2")?.as_str())?,
        )
    } else {
        (
            month(m.name("m1")?.as_str())?,
            decimal_number(m.name("d1")?.as_str())?,
            decimal_year(m.name("y1")?.as_str())?,
        )
    };
    if c < 1 {
        None
    } else {
        NaiveDate::from_ymd_opt(c, a, b)
    }
}
fn boundary(s: &str, i: usize) -> bool {
    let Some(c) = s.get(i..).and_then(|x| x.chars().next()) else {
        return false;
    };
    if matches!(c, '!' | '?' | '\n') {
        return true;
    }
    if c != '.' {
        return false;
    }
    let prefix = s.get(..i).unwrap_or("");
    // Python [^\W\d_]+ uses general-category letters and nondecimal
    // numbers, not the broader derived Alphabetic property (which includes
    // combining marks such as U+0345).
    static TOKEN: OnceLock<Option<Regex>> = OnceLock::new();
    let token = TOKEN
        .get_or_init(|| Regex::new(r"[\p{L}\p{Nl}\p{No}]+$").ok())
        .as_ref()
        .and_then(|pattern| pattern.find(prefix))
        .map_or("", |m| m.as_str());
    let mut folded = String::new();
    for c in token.to_lowercase().chars() {
        folded.push_str(match c {
            '\u{00f8}' => "o",
            '\u{0142}' => "l",
            '\u{0111}' | '\u{00f0}' => "d",
            '\u{00fe}' => "th",
            '\u{00e6}' => "ae",
            '\u{0153}' => "oe",
            '\u{00df}' => "ss",
            '\u{0131}' => "i",
            _ => {
                folded.push(c);
                continue;
            }
        });
    }
    let key: String = folded
        .nfkd()
        .filter(|c| canonical_combining_class(*c) == 0)
        .collect();
    !(T.contains(&key.as_str())
        || M.iter().any(|m| m.get(..3) == Some(key.as_str()))
        || key == "sept")
}
fn question(s: &str, a: usize, b: usize) -> (bool, bool) {
    let mut previous = None;
    for (i, _) in s.char_indices().take_while(|(i, _)| *i < a) {
        if boundary(s, i) {
            previous = Some(i);
        }
    }
    let start = previous.map_or(0, |i| {
        i + s
            .get(i..)
            .and_then(|x| x.chars().next())
            .map_or(1, char::len_utf8)
    });
    let opens = s
        .get(start..a)
        .unwrap_or("")
        .trim_matches(|c| matches!(c, ' ' | '\t' | '"' | '\'' | '(' | '*' | '_'))
        .is_empty();
    let mut ending = None;
    for (i, _) in s.get(b..).unwrap_or("").char_indices() {
        let absolute = b + i;
        if boundary(s, absolute) {
            ending = Some(absolute);
            break;
        }
    }
    (
        ending
            .and_then(|i| s.get(i..))
            .is_some_and(|x| x.starts_with('?')),
        opens,
    )
}
/// Parses the first valid qualifying anchor in left-to-right order.
pub fn time_anchor(message: &str) -> Option<TimeAnchor> {
    let s = message.replace(['\u{2019}', '\u{2018}'], "'");
    for c in re()?.captures_iter(&s) {
        let m = c.get(0)?;
        let kw = c
            .name("kw")?
            .as_str()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_ascii_lowercase();
        let (q, o) = question(&s, m.start(), m.end());
        if !q || (matches!(kw.as_str(), "before" | "until" | "till" | "up to") && !o) {
            continue;
        }
        if let Some(day) = parsed(&c) {
            return Some(TimeAnchor {
                day,
                inclusive: kw != "before",
            });
        }
    }
    None
}
fn candidate_date(s: &str) -> Option<NaiveDate> {
    // Chrono also accepts unpadded/signed/whitespace forms that Python's
    // date.fromisoformat rejects. Validate the exact supported ASCII layouts
    // before asking Chrono to validate the calendar date.
    let bytes = s.as_bytes();
    let extended = bytes.len() == 10
        && bytes.get(4) == Some(&b'-')
        && bytes.get(7) == Some(&b'-')
        && bytes
            .iter()
            .enumerate()
            .all(|(i, b)| i == 4 || i == 7 || b.is_ascii_digit());
    let basic = bytes.len() == 8 && bytes.iter().all(u8::is_ascii_digit);
    if extended || basic {
        return NaiveDate::parse_from_str(s, if extended { "%Y-%m-%d" } else { "%Y%m%d" })
            .ok()
            .filter(|day| (1..=9999).contains(&day.year()));
    }
    let year_text = s.get(..4)?;
    if !year_text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let year = year_text.parse::<i32>().ok()?;
    if year < 1 {
        return None;
    }
    let suffix = s.get(4..)?;
    let (week_text, weekday_text) = if let Some(rest) = suffix.strip_prefix("-W") {
        match rest.split_once('-') {
            Some((week, weekday)) => (week, Some(weekday)),
            None => (rest, None),
        }
    } else {
        let rest = suffix.strip_prefix('W')?;
        match rest.len() {
            2 => (rest, None),
            3 => (rest.get(..2)?, rest.get(2..)),
            _ => return None,
        }
    };
    if week_text.len() != 2 || !week_text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let week = week_text.parse::<u32>().ok()?;
    let weekday = match weekday_text {
        Some("1") => Weekday::Mon,
        Some("2") => Weekday::Tue,
        Some("3") => Weekday::Wed,
        Some("4") => Weekday::Thu,
        Some("5") => Weekday::Fri,
        Some("6") => Weekday::Sat,
        Some("7") => Weekday::Sun,
        None => Weekday::Mon,
        _ => return None,
    };
    NaiveDate::from_isoywd_opt(year, week, weekday).filter(|day| (1..=9999).contains(&day.year()))
}
/// Stable cutoff. Missing/invalid candidate dates survive; before is exclusive and other anchors inclusive.
pub fn apply_cutoff<U>(a: Option<TimeAnchor>, v: Vec<Candidate<U>>) -> Cutoff<U> {
    let (mut kept, mut removed) = (Vec::new(), Vec::new());
    for c in v {
        let d = c.day.as_deref().and_then(candidate_date);
        if a.is_some_and(|a| d.is_some_and(|d| d > a.day || (d == a.day && !a.inclusive))) {
            removed.push((c, "after_time_anchor"))
        } else {
            kept.push(c)
        }
    }
    Cutoff { kept, removed }
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn matches_python312_query_fixture() {
        let parsed: Result<Vec<Value>, _> =
            serde_json::from_str(include_str!("../tests/python312-anchor-fixtures.json"));
        assert!(parsed.is_ok(), "reference fixture must be valid JSON");
        let Ok(cases) = parsed else { return };
        for case in cases {
            let query = case.get("query").and_then(Value::as_str).unwrap_or("");
            let actual = time_anchor(query);
            let expected = case.get("anchor").unwrap_or(&Value::Null);
            if expected.is_null() {
                assert_eq!(actual, None, "{query}");
            } else {
                let day = expected.get("day").and_then(Value::as_str).unwrap_or("");
                let inclusive = expected
                    .get("inclusive")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                assert_eq!(
                    actual.map(|a| (a.day.to_string(), a.inclusive)),
                    Some((day.to_owned(), inclusive)),
                    "{query}"
                );
            }
        }
    }

    #[test]
    fn matches_python312_cutoff_fixture() {
        let parsed: Result<Value, _> =
            serde_json::from_str(include_str!("../tests/python312-cutoff-fixture.json"));
        assert!(parsed.is_ok(), "reference fixture must be valid JSON");
        let Ok(fixture) = parsed else { return };
        let rows = fixture.get("rows").and_then(Value::as_array);
        assert!(rows.is_some(), "reference rows must be an array");
        let Some(rows) = rows else { return };
        let candidates = rows
            .iter()
            .filter_map(|row| {
                let id = row.get("id").and_then(Value::as_str)?.to_owned();
                let day = row.get("day").and_then(Value::as_str).map(str::to_owned);
                Some(Candidate { id, day })
            })
            .collect();
        let query = fixture.get("query").and_then(Value::as_str).unwrap_or("");
        let out = apply_cutoff(time_anchor(query), candidates);
        let kept: Vec<_> = out.kept.iter().map(|c| c.id.as_str()).collect();
        let expected_kept: Vec<_> = fixture
            .get("kept_ids")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        assert_eq!(kept, expected_kept);
        let removed: Vec<_> = out
            .removed
            .iter()
            .map(|(c, reason)| (c.id.as_str(), *reason))
            .collect();
        let expected_removed: Vec<_> = fixture
            .get("removed")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|x| Some((x.get("id")?.as_str()?, x.get("reason")?.as_str()?)))
            .collect();
        assert_eq!(removed, expected_removed);
    }

    #[test]
    fn unicode_decimal_anchor_digits_match_python_int() {
        assert_eq!(
            time_anchor(
                "As of January \u{0662}\u{0660}, \u{0662}\u{0660}\u{0663}\u{0660}, what happened?"
            )
            .map(|a| (a.day.to_string(), a.inclusive)),
            Some(("2030-01-20".to_owned(), true))
        );
        assert_eq!(
            time_anchor(
                "As of \u{0662}\u{0660} Jan \u{0662}\u{0660}\u{0663}\u{0660}, what happened?"
            )
            .map(|a| (a.day.to_string(), a.inclusive)),
            Some(("2030-01-20".to_owned(), true))
        );
    }

    #[test]
    fn unsupported_decimal_and_nondigit_inputs_do_not_panic() {
        // CPython 3.12 / Unicode 15 does not recognize Garay digits. The
        // newer regex Unicode tables can match them, but conversion must reject
        // them without evaluating subtraction outside an accepted digit block.
        for c in ['x', ' ', '\u{0000}', '\u{10d40}', '\u{10d42}'] {
            assert_eq!(decimal_value(c), None);
        }
        for zero in DECIMAL_ZEROES {
            for digit in 0..10 {
                let c = char::from_u32(zero + digit);
                assert!(c.is_some(), "decimal table contains an invalid code point");
                if let Some(c) = c {
                    assert_eq!(decimal_value(c), Some(digit));
                }
            }
        }
        assert_eq!(
            time_anchor("As of January \u{10d42}\u{10d40}, 2030, what happened?"),
            None
        );
    }

    #[test]
    fn python_date_rejects_year_zero_for_anchors_and_candidates() {
        assert_eq!(time_anchor("As of 0000-01-01, what?"), None);
        assert!(time_anchor("As of 0001-01-01, what?").is_some());
        assert_eq!(candidate_date("0000-01-01"), None);
        assert_eq!(candidate_date("00000101"), None);
        assert_eq!(candidate_date("0000-W01-1"), None);
        assert_eq!(
            candidate_date("2030W011").map(|d| d.to_string()).as_deref(),
            Some("2029-12-31")
        );
        assert_eq!(
            candidate_date("2030W01").map(|d| d.to_string()).as_deref(),
            Some("2029-12-31")
        );
        assert_eq!(candidate_date("2030W01-1"), None);
        assert_eq!(candidate_date("2030-W011"), None);
    }

    #[test]
    fn title_boundaries_match_pinned_python312_edges() {
        let fixture: Value =
            serde_json::from_str(include_str!("../tests/python312-title-boundary-edges.json"))
                .unwrap_or(Value::Null);
        let cases = fixture.get("cases").and_then(Value::as_array);
        assert!(cases.is_some(), "boundary oracle must contain cases");
        let Some(cases) = cases else { return };
        for case in cases {
            let query = case.get("query").and_then(Value::as_str).unwrap_or("");
            let actual = time_anchor(query)
                .map(|a| {
                    serde_json::json!({
                        "day": a.day.to_string(), "inclusive": a.inclusive
                    })
                })
                .unwrap_or(Value::Null);
            assert_eq!(Some(&actual), case.get("anchor"), "{query:?}");
        }
    }

    #[test]
    fn candidate_dates_match_pinned_python312_edges() {
        let fixture: Value =
            serde_json::from_str(include_str!("../tests/python312-candidate-date-edges.json"))
                .unwrap_or(Value::Null);
        let cases = fixture.get("cases").and_then(Value::as_array);
        assert!(cases.is_some(), "candidate-date oracle must contain cases");
        let Some(cases) = cases else { return };
        for case in cases {
            let input = case.get("input").and_then(Value::as_str).unwrap_or("");
            let expected = case.get("day").and_then(Value::as_str);
            let actual = candidate_date(input).map(|d| d.to_string());
            assert_eq!(actual.as_deref(), expected, "{input:?}");
        }
        let Some(anchor) = NaiveDate::from_ymd_opt(2020, 1, 1) else {
            return;
        };
        let out = apply_cutoff(
            Some(TimeAnchor {
                day: anchor,
                inclusive: true,
            }),
            vec![Candidate {
                id: 1,
                day: Some("2030-1-20".to_owned()),
            }],
        );
        assert_eq!(out.kept.len(), 1);
        assert!(out.removed.is_empty());
    }

    #[test]
    fn cutoff_keeps_invalid_missing_days_and_stable_order() {
        let Some(day) = NaiveDate::from_ymd_opt(2030, 1, 20) else {
            return;
        };
        let anchor = TimeAnchor {
            day,
            inclusive: false,
        };
        let input = [
            Some("2030-01-19"),
            Some("2030-01-20"),
            Some("2030-01-21"),
            None,
            Some("bad"),
        ]
        .into_iter()
        .enumerate()
        .map(|(id, day)| Candidate {
            id,
            day: day.map(str::to_owned),
        })
        .collect();
        let out = apply_cutoff(Some(anchor), input);
        assert_eq!(out.kept.iter().map(|c| c.id).collect::<Vec<_>>(), [0, 3, 4]);
        assert_eq!(
            out.removed
                .iter()
                .map(|(c, reason)| (c.id, *reason))
                .collect::<Vec<_>>(),
            [(1, "after_time_anchor"), (2, "after_time_anchor")]
        );
    }
}
