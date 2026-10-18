//! The query of `GET /api/v1/search` and `GET /api/v1/search/repo` as FastAPI reads and validates
//! it (pydantic 2 lax mode on strings), with its 422 answer byte for byte.
//!
//! `parse_qsl(keep_blank_values=True)` gives every key's last value. A `str` parameter takes any
//! text. An `int`, `float` or `bool` parameter takes what pydantic parses and answers its
//! `int_parsing`, `float_parsing` or `bool_parsing` error (the input echoed) for the rest; the
//! errors come in declaration order, all together.
//!
//! pydantic reads some numbers in ways no grammar written from its documentation predicts
//! (underscores in odd places, `0-1` as -1). A value is therefore judged in three ways: one whose
//! reading is certain gives its number; one with a character no number can have (a letter that is
//! not `e` or part of `inf`/`infinity`/`nan`, a non-ASCII digit, a comma) gives pydantic's error;
//! anything else is [`Parsed::Unknown`] and the route answers its native refusal.
//! `tests.rs` checks all three against pydantic itself over a corpus of 17,000 values.
use crate::validation;

/// `search(...)`'s parameters, after validation.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchParams {
    pub q: String,
    pub sort: String,
    /// `limit`, as `int(limit or 30)` clamped by the route: 1 to 60.
    pub limit: i64,
    pub trusted_only: bool,
    pub show_unsuitable: bool,
    pub min_gb: Option<f64>,
    pub max_gb: Option<f64>,
    pub quants: String,
    pub min_params: Option<f64>,
    pub max_params: Option<f64>,
    pub moe: String,
    pub vision: bool,
    pub license: String,
    pub owner: String,
}

/// Why a query is not answered by the route itself.
#[derive(Debug, PartialEq)]
pub enum Rejection {
    /// FastAPI's 422 body.
    Invalid(String),
    /// A value pydantic reads in a way this side does not reproduce.
    Unsupported,
}

/// The values of a query, last one winning.
pub struct Query(Vec<(String, String)>);

impl Query {
    pub fn parse(query: &str) -> Self {
        Self(
            form_urlencoded::parse(query.as_bytes())
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect(),
        )
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .rev()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

/// How pydantic fails a value.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Failure {
    /// The text is not an integer (`int_parsing`).
    Int,
    /// More than 4300 characters (`int_parsing_size`).
    IntSize,
    Float,
    Bool,
}

/// A value judged: its reading, pydantic's error, or neither known.
#[derive(Debug, PartialEq)]
pub(super) enum Parsed<T> {
    Value(T),
    Invalid(Failure),
    Unknown,
}

/// pydantic's lax `int` from a str: surrounding whitespace is trimmed; an optional sign and
/// digits are an integer (past 18 digits only the sign matters to the route: it clamps to 1..=60).
pub(super) fn parse_int(raw: &str) -> Parsed<i64> {
    let text = raw.trim();
    if text.len() > 4300 {
        return Parsed::Invalid(Failure::IntSize);
    }
    if text.is_empty()
        || text
            .chars()
            .any(|c| !matches!(c, '0'..='9' | '+' | '-' | '_' | '.'))
    {
        return Parsed::Invalid(Failure::Int);
    }
    if !text.bytes().any(|b| b.is_ascii_digit()) {
        return Parsed::Invalid(Failure::Int);
    }
    let (negative, digits) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    // A fraction that is not all zeros, or a point with nothing after it.
    if let Some((whole, fraction)) = digits.split_once('.') {
        let plain = |s: &str| s.bytes().all(|b| b.is_ascii_digit());
        if !whole.is_empty()
            && plain(whole)
            && plain(fraction)
            && (fraction.is_empty() || fraction.bytes().any(|b| b != b'0'))
        {
            return Parsed::Invalid(Failure::Int);
        }
    }
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Parsed::Unknown;
    }
    let significant = digits.trim_start_matches('0');
    let magnitude = if significant.is_empty() {
        0
    } else if significant.len() > 18 {
        i64::MAX
    } else {
        match significant.parse::<i64>() {
            Ok(n) => n,
            Err(_) => return Parsed::Unknown,
        }
    };
    Parsed::Value(if negative { -magnitude } else { magnitude })
}

/// pydantic's lax `float` from a str: surrounding whitespace is trimmed; a decimal number with an
/// optional sign, fraction and exponent, or `inf`, `infinity` and `nan` in any case.
pub(super) fn parse_float(raw: &str) -> Parsed<f64> {
    let text = raw.trim();
    let alphabet = |c: char| {
        c.is_ascii_digit()
            || matches!(c, '+' | '-' | '_' | '.' | 'e' | 'E')
            || matches!(c.to_ascii_lowercase(), 'i' | 'n' | 'f' | 't' | 'y' | 'a')
    };
    if text.is_empty() || !text.chars().all(alphabet) {
        return Parsed::Invalid(Failure::Float);
    }
    let (sign, body) = match text.strip_prefix('-') {
        Some(rest) => (-1.0, rest),
        None => (1.0, text.strip_prefix('+').unwrap_or(text)),
    };
    match body.to_ascii_lowercase().as_str() {
        "inf" | "infinity" => return Parsed::Value(sign * f64::INFINITY),
        "nan" => return Parsed::Value(f64::NAN),
        _ => {}
    }
    // Neither a word nor with a digit; or an exponent mark with nothing after it.
    if !text.bytes().any(|b| b.is_ascii_digit()) {
        return Parsed::Invalid(Failure::Float);
    }
    if !text.contains('_') && body.trim_end_matches(['+', '-']).ends_with(['e', 'E']) {
        return Parsed::Invalid(Failure::Float);
    }
    let (mantissa, exponent) = match body.split_once(['e', 'E']) {
        Some((m, e)) => (m, Some(e)),
        None => (body, None),
    };
    let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let digits = |s: &str| s.bytes().all(|b| b.is_ascii_digit());
    let clean = digits(whole)
        && digits(fraction)
        && !(whole.is_empty() && fraction.is_empty())
        && exponent.is_none_or(|e| {
            let e = e.strip_prefix(['+', '-']).unwrap_or(e);
            !e.is_empty() && digits(e)
        });
    if !clean {
        return Parsed::Unknown;
    }
    match body.parse::<f64>() {
        Ok(v) => Parsed::Value(sign * v),
        Err(_) => Parsed::Unknown,
    }
}

/// pydantic's lax `bool` from a str: `true false 1 0 yes no on off t f y n`, any case, no
/// surrounding space.
pub(super) fn parse_bool(raw: &str) -> Parsed<bool> {
    match raw.to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" | "t" | "y" => Parsed::Value(true),
        "false" | "0" | "no" | "off" | "f" | "n" => Parsed::Value(false),
        _ => Parsed::Invalid(Failure::Bool),
    }
}

/// One validation error of a query parameter.
fn error(name: &str, raw: &str, failure: &Failure) -> String {
    let (kind, message) = match failure {
        Failure::Int => (
            "int_parsing",
            "Input should be a valid integer, unable to parse string as an integer",
        ),
        Failure::IntSize => (
            "int_parsing_size",
            "Unable to parse input string as an integer, exceeded maximum size",
        ),
        Failure::Float => (
            "float_parsing",
            "Input should be a valid number, unable to parse string as a number",
        ),
        Failure::Bool => (
            "bool_parsing",
            "Input should be a valid boolean, unable to interpret input",
        ),
    };
    validation::error(
        kind,
        &[validation::key("query"), validation::key(name)],
        message,
        &validation::string(raw),
        None,
    )
}

/// Moves a parser's value into its slot; what is left is the verdict.
fn keep<T>(parsed: Parsed<T>, slot: &mut T) -> Parsed<()> {
    match parsed {
        Parsed::Value(v) => {
            *slot = v;
            Parsed::Value(())
        }
        Parsed::Invalid(f) => Parsed::Invalid(f),
        Parsed::Unknown => Parsed::Unknown,
    }
}

/// `search(...)`'s parameters, or why not.
pub fn search(query: &str) -> Result<SearchParams, Rejection> {
    let q = Query::parse(query);
    let text = |name: &str| q.get(name).unwrap_or_default().to_owned();
    // The validated parameters, in declaration order (the order of the errors).
    let mut errors: Vec<String> = Vec::new();
    let mut unknown = false;
    let mut take = |name: &str, raw: &str, verdict: Parsed<()>| match verdict {
        Parsed::Value(()) => {}
        Parsed::Invalid(failure) => errors.push(error(name, raw, &failure)),
        Parsed::Unknown => unknown = true,
    };
    let mut limit = 30;
    let mut trusted_only = true;
    let mut show_unsuitable = false;
    let mut vision = false;
    let mut floats: [Option<f64>; 4] = [None; 4];
    if let Some(raw) = q.get("limit") {
        take("limit", raw, keep(parse_int(raw), &mut limit));
    }
    if let Some(raw) = q.get("trustedOnly") {
        take("trustedOnly", raw, keep(parse_bool(raw), &mut trusted_only));
    }
    if let Some(raw) = q.get("showUnsuitable") {
        take(
            "showUnsuitable",
            raw,
            keep(parse_bool(raw), &mut show_unsuitable),
        );
    }
    for (slot, name) in ["minGb", "maxGb", "minParams", "maxParams"]
        .iter()
        .enumerate()
    {
        if let Some(raw) = q.get(name) {
            let mut value = 0.0;
            let verdict = keep(parse_float(raw), &mut value);
            if matches!(verdict, Parsed::Value(())) {
                if let Some(cell) = floats.get_mut(slot) {
                    *cell = Some(value);
                }
            }
            take(name, raw, verdict);
        }
    }
    if let Some(raw) = q.get("vision") {
        take("vision", raw, keep(parse_bool(raw), &mut vision));
    }
    if unknown {
        return Err(Rejection::Unsupported);
    }
    if !errors.is_empty() {
        return Err(Rejection::Invalid(format!(
            "{{\"detail\":[{}]}}",
            errors.join(",")
        )));
    }
    let [min_gb, max_gb, min_params, max_params] = floats;
    Ok(SearchParams {
        q: text("q"),
        sort: q.get("sort").unwrap_or("fit").to_owned(),
        // `max(1, min(int(limit or 30), 60))`.
        limit: if limit == 0 { 30 } else { limit }.clamp(1, 60),
        trusted_only,
        show_unsuitable,
        min_gb,
        max_gb,
        quants: text("quants"),
        min_params,
        max_params,
        moe: q.get("moe").unwrap_or("any").to_owned(),
        vision,
        license: text("license"),
        owner: text("owner"),
    })
}

/// `search_repo(repo: str = Query(...))`: the value, or FastAPI's 422 body for a missing one.
pub fn repo(query: &str) -> Result<String, String> {
    Query::parse(query)
        .get("repo")
        .map(str::to_owned)
        .ok_or_else(|| {
            format!(
                "{{\"detail\":[{}]}}",
                validation::missing(&[validation::key("query"), validation::key("repo")])
            )
        })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
    use super::*;

    fn detail(query: &str) -> String {
        match search(query) {
            Err(Rejection::Invalid(body)) => body,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn defaults() {
        let p = search("").unwrap();
        assert_eq!(p.q, "");
        assert_eq!(p.sort, "fit");
        assert_eq!(p.limit, 30);
        assert!(p.trusted_only && !p.show_unsuitable && !p.vision);
        assert_eq!(
            (p.min_gb, p.max_gb, p.min_params, p.max_params),
            (None, None, None, None)
        );
        assert_eq!(p.moe, "any");
    }

    #[test]
    fn last_value_wins_and_plus_is_a_space() {
        let p = search("q=a+b&q=c%20d&limit=1&limit=2").unwrap();
        assert_eq!(p.q, "c d");
        assert_eq!(p.limit, 2);
        assert_eq!(search("q=a%FFb").unwrap().q, "a\u{fffd}b");
    }

    #[test]
    fn limit_is_clamped_like_the_route() {
        for (raw, expect) in [
            ("0", 30),
            ("-5", 1),
            ("1", 1),
            ("60", 60),
            ("61", 60),
            ("99999999999999999999999", 60),
            ("-99999999999999999999999", 1),
            ("%2B30", 30),
            ("%2030%20", 30),
            ("-0", 30),
        ] {
            assert_eq!(
                search(&format!("limit={raw}")).unwrap().limit,
                expect,
                "{raw}"
            );
        }
    }

    #[test]
    fn readings_pydantic_makes_that_this_side_does_not_are_refused() {
        for raw in ["3_0", "30.0", "0-1", "1__0"] {
            assert_eq!(
                search(&format!("limit={raw}")),
                Err(Rejection::Unsupported),
                "{raw}"
            );
        }
        for raw in ["1_0", "1.5.5", "--1", "1e_5", "1_e", "+_1"] {
            assert_eq!(
                search(&format!("minGb={raw}")),
                Err(Rejection::Unsupported),
                "{raw}"
            );
        }
        // An unknown reading hides the other errors: the answer would list them all.
        assert_eq!(search("limit=3_0&vision=zz"), Err(Rejection::Unsupported));
    }

    #[test]
    fn errors_come_together_in_declaration_order() {
        let body = detail("vision=zz&minGb=q&limit=abc");
        assert_eq!(
            body,
            concat!(
                r#"{"detail":["#,
                r#"{"type":"int_parsing","loc":["query","limit"],"msg":"Input should be a valid integer, unable to parse string as an integer","input":"abc"},"#,
                r#"{"type":"float_parsing","loc":["query","minGb"],"msg":"Input should be a valid number, unable to parse string as a number","input":"q"},"#,
                r#"{"type":"bool_parsing","loc":["query","vision"],"msg":"Input should be a valid boolean, unable to interpret input","input":"zz"}]}"#
            )
        );
        assert!(detail(&format!("limit={}", "9".repeat(4301))).contains("int_parsing_size"));
    }

    #[test]
    fn numbers() {
        let p = search("maxGb=-INFINITY&minParams=.5&maxParams=5.&minGb=1e3").unwrap();
        assert_eq!(
            (p.min_gb, p.max_gb, p.min_params, p.max_params),
            (Some(1000.0), Some(f64::NEG_INFINITY), Some(0.5), Some(5.0))
        );
        assert!(search("minGb=nan").unwrap().min_gb.unwrap().is_nan());
        assert_eq!(search("minGb=1E400").unwrap().min_gb, Some(f64::INFINITY));
        assert_eq!(
            search("minGb=%20-0%20").unwrap().min_gb.unwrap().to_bits(),
            (-0.0f64).to_bits()
        );
    }

    #[test]
    fn repo_is_required() {
        assert_eq!(repo("repo=a%2Fb").unwrap(), "a/b");
        assert_eq!(repo("repo=").unwrap(), "");
        assert_eq!(
            repo("").unwrap_err(),
            r#"{"detail":[{"type":"missing","loc":["query","repo"],"msg":"Field required","input":null}]}"#
        );
    }
}
