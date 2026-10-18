//! The query of `GET /api/v1/sections/{name}/autoconfig` as FastAPI reads and validates it
//! (pydantic 2 lax mode on strings), with its 422 answer byte for byte.
//!
//! `parse_qsl(keep_blank_values=True)` gives every key's last value. A `str` parameter takes any
//! text. For `int`, `float` and `bool` a value is judged three ways: one whose reading is certain
//! gives its value; one with a character no such value can have gives pydantic's error (the input
//! echoed, the errors in declaration order, all together); anything else (surrounding whitespace,
//! underscores, a fraction on an integer, more than 18 digits) is [`Parsed::Unknown`] and the route
//! answers its native refusal instead of guessing.
use crate::validation;

/// The route's parameters after validation, before the handler's own clamping.
#[derive(Debug, Clone, PartialEq)]
pub struct Params {
    pub preset: String,
    pub sessions: i64,
    pub spec: String,
    pub vision: bool,
    pub verified_ctx: i64,
    pub prompt_budget_s: f64,
    pub mode: String,
    pub budget_gib: f64,
}

/// Why a query is not answered by the route itself.
#[derive(Debug, PartialEq)]
pub enum Rejection {
    /// FastAPI's 422 body.
    Invalid(String),
    /// A value pydantic reads in a way this side does not reproduce.
    Unknown,
}

#[derive(Debug, PartialEq)]
pub(crate) enum Parsed<T> {
    Value(T),
    Invalid,
    Unknown,
}

/// The value a key last had, as `parse_qsl` decodes it (`+` is a space, bad UTF-8 replaced).
fn last(pairs: &[(String, String)], name: &str) -> Option<String> {
    pairs
        .iter()
        .rev()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.clone())
}

fn plain_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

/// pydantic's lax `int` from a str, where certain.
pub(crate) fn parse_int(raw: &str) -> Parsed<i64> {
    if raw.chars().any(char::is_whitespace) {
        return Parsed::Unknown;
    }
    // No integer pydantic reads has another character, or none of these digits.
    if raw
        .chars()
        .any(|c| !matches!(c, '0'..='9' | '+' | '-' | '_' | '.'))
        || !raw.bytes().any(|b| b.is_ascii_digit())
    {
        return Parsed::Invalid;
    }
    let negative = raw.starts_with('-');
    let digits = raw
        .strip_prefix('-')
        .or_else(|| raw.strip_prefix('+'))
        .unwrap_or(raw);
    // A fraction that is not all zeros, or a point with nothing after it.
    if let Some((whole, fraction)) = digits.split_once('.') {
        if plain_digits(whole)
            && fraction.bytes().all(|b| b.is_ascii_digit())
            && (fraction.is_empty() || fraction.bytes().any(|b| b != b'0'))
        {
            return Parsed::Invalid;
        }
    }
    if !plain_digits(digits) {
        return Parsed::Unknown;
    }
    let significant = digits.trim_start_matches('0');
    if significant.len() > 18 {
        return Parsed::Unknown;
    }
    let magnitude: i64 = if significant.is_empty() {
        0
    } else {
        match significant.parse() {
            Ok(n) => n,
            Err(_) => return Parsed::Unknown,
        }
    };
    Parsed::Value(if negative { -magnitude } else { magnitude })
}

/// pydantic's lax `float` from a str, where certain: a plain decimal with an optional sign,
/// fraction and exponent, or `inf`, `infinity` and `nan` in any letter case.
pub(crate) fn parse_float(raw: &str) -> Parsed<f64> {
    if raw.chars().any(char::is_whitespace) {
        return Parsed::Unknown;
    }
    let (sign, body) = match raw.strip_prefix('-') {
        Some(rest) => (-1.0, rest),
        None => (1.0, raw.strip_prefix('+').unwrap_or(raw)),
    };
    match body.to_ascii_lowercase().as_str() {
        "inf" | "infinity" => return Parsed::Value(sign * f64::INFINITY),
        "nan" => return Parsed::Value(f64::NAN),
        _ => {}
    }
    let alphabet = |c: char| {
        c.is_ascii_digit()
            || matches!(c, '+' | '-' | '_' | '.' | 'e' | 'E')
            || matches!(c.to_ascii_lowercase(), 'i' | 'n' | 'f' | 't' | 'y' | 'a')
    };
    // Not a number's character, or neither a word nor with a digit, or an exponent mark with
    // nothing after it.
    if raw.is_empty()
        || !raw.chars().all(alphabet)
        || !raw.bytes().any(|b| b.is_ascii_digit())
        || (!raw.contains('_') && body.trim_end_matches(['+', '-']).ends_with(['e', 'E']))
    {
        return Parsed::Invalid;
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
        && exponent.is_none_or(|e| plain_digits(e.strip_prefix(['+', '-']).unwrap_or(e)));
    if !clean || body.len() > 64 {
        return Parsed::Unknown;
    }
    match body.parse::<f64>() {
        Ok(v) => Parsed::Value(sign * v),
        Err(_) => Parsed::Unknown,
    }
}

/// pydantic's lax `bool` from a str: these words in any ASCII letter case, nothing stripped.
pub(crate) fn parse_bool(raw: &str) -> Parsed<bool> {
    match raw.to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" | "t" | "y" => Parsed::Value(true),
        "false" | "0" | "no" | "off" | "f" | "n" => Parsed::Value(false),
        _ if raw.is_ascii() => Parsed::Invalid,
        _ => Parsed::Unknown,
    }
}

fn error(name: &str, raw: &str, kind: &str, message: &str) -> String {
    validation::error(
        kind,
        &[validation::key("query"), validation::key(name)],
        message,
        &validation::string(raw),
        None,
    )
}

const INT: (&str, &str) = (
    "int_parsing",
    "Input should be a valid integer, unable to parse string as an integer",
);
const FLOAT: (&str, &str) = (
    "float_parsing",
    "Input should be a valid number, unable to parse string as a number",
);
const BOOL: (&str, &str) = (
    "bool_parsing",
    "Input should be a valid boolean, unable to interpret input",
);

/// The parameters of `query` (the raw query string), or why the route does not answer it.
pub fn parse(query: &str) -> Result<Params, Rejection> {
    let pairs: Vec<(String, String)> = form_urlencoded::parse(query.as_bytes())
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    let mut errors: Vec<String> = Vec::new();
    let mut unknown = false;
    let mut judge = |name: &str, raw: Option<String>, kind: (&str, &str), ok: bool, bad: bool| {
        if let Some(raw) = raw {
            if bad {
                errors.push(error(name, &raw, kind.0, kind.1));
            } else if !ok {
                unknown = true;
            }
        }
    };
    let text = |name: &str| last(&pairs, name).unwrap_or_default();
    let mut sessions = 1;
    let raw = last(&pairs, "sessions");
    let got = raw.as_deref().map(parse_int);
    if let Some(Parsed::Value(n)) = got {
        sessions = n;
    }
    judge(
        "sessions",
        raw,
        INT,
        matches!(got, Some(Parsed::Value(_))),
        got == Some(Parsed::Invalid),
    );
    let mut vision = true;
    let raw = last(&pairs, "vision");
    let got = raw.as_deref().map(parse_bool);
    if let Some(Parsed::Value(b)) = got {
        vision = b;
    }
    judge(
        "vision",
        raw,
        BOOL,
        matches!(got, Some(Parsed::Value(_))),
        got == Some(Parsed::Invalid),
    );
    let mut verified_ctx = 0;
    let raw = last(&pairs, "verified_ctx");
    let got = raw.as_deref().map(parse_int);
    if let Some(Parsed::Value(n)) = got {
        verified_ctx = n;
    }
    judge(
        "verified_ctx",
        raw,
        INT,
        matches!(got, Some(Parsed::Value(_))),
        got == Some(Parsed::Invalid),
    );
    let mut prompt_budget_s = 120.0;
    let raw = last(&pairs, "prompt_budget_s");
    let got = raw.as_deref().map(parse_float);
    if let Some(Parsed::Value(x)) = got {
        prompt_budget_s = x;
    }
    let valid = matches!(got, Some(Parsed::Value(_)));
    judge(
        "prompt_budget_s",
        raw,
        FLOAT,
        valid,
        matches!(got, Some(Parsed::Invalid)),
    );
    let mut budget_gib = 0.0;
    let raw = last(&pairs, "budget_gib");
    let got = raw.as_deref().map(parse_float);
    if let Some(Parsed::Value(x)) = got {
        budget_gib = x;
    }
    let valid = matches!(got, Some(Parsed::Value(_)));
    judge(
        "budget_gib",
        raw,
        FLOAT,
        valid,
        matches!(got, Some(Parsed::Invalid)),
    );
    if unknown {
        return Err(Rejection::Unknown);
    }
    if !errors.is_empty() {
        return Err(Rejection::Invalid(format!(
            "{{\"detail\":[{}]}}",
            errors.join(",")
        )));
    }
    Ok(Params {
        preset: text("preset"),
        sessions,
        spec: text("spec"),
        vision,
        verified_ctx,
        prompt_budget_s,
        mode: text("mode"),
        budget_gib,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;

    #[test]
    fn readings() {
        assert_eq!(parse_int("007"), Parsed::Value(7));
        assert_eq!(parse_int("-3"), Parsed::Value(-3));
        assert_eq!(parse_int("+0"), Parsed::Value(0));
        assert_eq!(parse_int("x"), Parsed::Invalid);
        assert_eq!(parse_int(""), Parsed::Invalid);
        assert_eq!(parse_int("1.5"), Parsed::Invalid);
        assert_eq!(parse_int("2."), Parsed::Invalid);
        assert_eq!(parse_int("1.0"), Parsed::Unknown);
        assert_eq!(parse_int(" 1"), Parsed::Unknown);
        assert_eq!(parse_int("1_0"), Parsed::Unknown);
        assert_eq!(parse_int(&"9".repeat(19)), Parsed::Unknown);
        assert_eq!(parse_float("1e3"), Parsed::Value(1000.0));
        assert_eq!(parse_float("-.5"), Parsed::Value(-0.5));
        assert_eq!(parse_float("INF"), Parsed::Value(f64::INFINITY));
        assert!(matches!(parse_float("nan"), Parsed::Value(x) if x.is_nan()));
        assert_eq!(parse_float("abc"), Parsed::Invalid);
        assert_eq!(parse_float("1e"), Parsed::Invalid);
        assert_eq!(parse_float(""), Parsed::Invalid);
        assert_eq!(parse_float("1_0"), Parsed::Unknown);
        assert_eq!(parse_float("\u{0661}"), Parsed::Invalid);
        assert_eq!(parse_bool("Yes"), Parsed::Value(true));
        assert_eq!(parse_bool("off"), Parsed::Value(false));
        assert_eq!(parse_bool(" true"), Parsed::Invalid);
        assert_eq!(parse_bool("\u{0442}"), Parsed::Unknown);
    }

    #[test]
    fn errors_come_together_in_declaration_order() {
        let Err(Rejection::Invalid(body)) =
            parse("budget_gib=x&sessions=y&vision=maybe&verified_ctx=2.5&prompt_budget_s=")
        else {
            panic!("not refused");
        };
        let order: Vec<&str> = [
            "sessions",
            "vision",
            "verified_ctx",
            "prompt_budget_s",
            "budget_gib",
        ]
        .into_iter()
        .collect();
        let mut at = 0;
        for name in order {
            let found = body[at..].find(&format!("\"{name}\"")).unwrap();
            at += found;
        }
        assert_eq!(parse("sessions=%201"), Err(Rejection::Unknown));
        let params = parse("sessions=3&sessions=5&preset=fast&mode=+Code+").unwrap();
        assert_eq!(
            (
                params.sessions,
                params.preset.as_str(),
                params.mode.as_str()
            ),
            (5, "fast", " Code ")
        );
    }
}
