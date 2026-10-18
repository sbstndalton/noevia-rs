//! The source-bound FastAPI path/int contract before reading a benchmark run.
const PREFIX: &str = "/api/v1/benchmark/runs";
#[derive(Debug, PartialEq, Eq)]
pub enum Route {
    NotOwned,
    NotFound,
    Redirect(String),
    Item(String),
}
fn namespace(raw: &str) -> bool {
    let mut bytes = raw.bytes();
    let decoded = |bytes: &mut std::str::Bytes<'_>| -> Option<u8> {
        let byte = bytes.next()?;
        if byte != b'%' {
            return Some(byte);
        }
        let a = char::from(bytes.next()?).to_digit(16)?;
        let b = char::from(bytes.next()?).to_digit(16)?;
        u8::try_from(a * 16 + b).ok()
    };
    for expected in PREFIX.bytes() {
        if decoded(&mut bytes) != Some(expected) {
            return false;
        }
    }
    bytes.len() == 0 || decoded(&mut bytes) == Some(b'/')
}
pub fn classify(raw: &str) -> Route {
    if !namespace(raw) {
        return Route::NotOwned;
    }
    let decoded = crate::percent_decode_lossy(raw);
    let canonical = decoded.trim_end_matches('/');
    let redirect = canonical != decoded;
    let Some(item) = canonical
        .strip_prefix(PREFIX)
        .and_then(|p| p.strip_prefix('/'))
    else {
        return Route::NotFound;
    };
    if item.is_empty() || item.contains('/') {
        return Route::NotFound;
    }
    if redirect {
        Route::Redirect(canonical.to_owned())
    } else {
        Route::Item(item.to_owned())
    }
}
#[derive(Debug, PartialEq, Eq)]
pub enum Id {
    Value(i64),
    Overflow,
    Invalid,
    TooLarge,
}
/// Pydantic first attempts a JSON integer, then its string cleanup grammar.
/// These phases have different size-error semantics; both are bound by the front URI cap.
/// Valid integers outside SQLite's i64 range reach its 500 without allocating a big integer.
pub fn parse(raw: &str) -> Id {
    match json_integer(raw) {
        Id::Invalid => {}
        result => return result,
    }
    let Some(cleaned) = clean_integer(raw) else {
        return Id::Invalid;
    };
    match json_integer(&cleaned) {
        Id::TooLarge => Id::Invalid,
        result => result,
    }
}
fn json_integer(raw: &str) -> Id {
    let (negative, digits) = raw.strip_prefix('-').map_or((false, raw), |s| (true, s));
    let Some(first) = digits.as_bytes().first() else {
        return Id::Invalid;
    };
    if *first == b'0' {
        return if digits.len() == 1 {
            Id::Value(0)
        } else {
            Id::Invalid
        };
    }
    if !matches!(first, b'1'..=b'9') {
        return Id::Invalid;
    }
    let count = digits.bytes().take_while(u8::is_ascii_digit).count();
    // Jiter's first pass encounters this limit before inspecting trailing junk.
    if count > 4300 {
        return Id::TooLarge;
    }
    if count != digits.len() {
        return Id::Invalid;
    }
    let mut number = Some(0u64);
    for byte in digits.bytes() {
        number = number
            .and_then(|n| n.checked_mul(10))
            .and_then(|n| n.checked_add(u64::from(byte - b'0')));
    }
    let Some(number) = number else {
        return Id::Overflow;
    };
    if negative && number == 1u64 << 63 {
        return Id::Value(i64::MIN);
    }
    match i64::try_from(number) {
        Ok(n) => Id::Value(if negative { -n } else { n }),
        Err(_) => Id::Overflow,
    }
}
fn clean_integer(raw: &str) -> Option<String> {
    let mut text = raw.trim();
    if let Some(s) = text.strip_prefix('+') {
        if s.starts_with('-') {
            return None;
        }
        text = s;
    }
    let negative = text.starts_with('-');
    if negative {
        text = text.strip_prefix('-')?;
        if text.starts_with(['-', '+']) {
            return None;
        }
    }
    let first = text.as_bytes().first()?;
    if *first == b'0' {
        let mut start = text.len().checked_sub(1)?;
        for (index, byte) in text.bytes().enumerate().skip(1) {
            match byte {
                b'0' | b'_' => {}
                b'1'..=b'9' | b'-' => {
                    start = index;
                    break;
                }
                b'.' => {
                    start = index.checked_sub(1)?;
                    break;
                }
                _ => return None,
            }
        }
        text = text.get(start..)?;
    } else if !matches!(first, b'1'..=b'9' | b'-') {
        return None;
    }
    if let Some((integer, fraction)) = text.split_once('.') {
        if !fraction.is_empty() && fraction.bytes().all(|b| b == b'0') {
            text = integer;
        }
    }
    let clean = if text.contains('_')
        && !text.starts_with('_')
        && !text.ends_with('_')
        && !text.contains("__")
    {
        text.replace('_', "")
    } else {
        text.to_owned()
    };
    Some(if negative { format!("-{clean}") } else { clean })
}
/// RedirectResponse quotes UTF-8, preserving Starlette's URL-safe set, including existing '%'.
pub fn location(host: &str, path: &str, query: Option<&str>) -> String {
    let raw = format!(
        "http://{host}{path}{}",
        query.map_or(String::new(), |q| format!("?{q}"))
    );
    let mut out = String::new();
    for byte in raw.bytes() {
        if byte.is_ascii_alphanumeric() || b":/%#?=@[]!$&'()*+,;-._~".contains(&byte) {
            out.push(char::from(byte));
        } else {
            use std::fmt::Write;
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}
