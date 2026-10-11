//! Complete prompt writer, default off. DATA_DIR must remain trusted and stable (#1357).
//! SQLite canonicalizes names: identity checks detect substitution, not hostile rename races.
mod encoding;
mod literals;
mod store;
mod text;
pub use store::Prompts;
pub const NAME_CAP: usize = 4096;
pub const BODY_CAP: usize = 262_144;
pub const ROW_CAP: usize = 4096;
pub const RESPONSE_CAP: usize = 16_777_216;
pub const SEED_KEY: &str = "bench_prompts_seeded";
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Input,
    Body,
    Object,
    Required,
    Text,
    Limit,
    Store,
    Busy,
    NotFound,
    Id,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Input => "invalid prompt input",
            Self::Body => "There was an error parsing the body",
            Self::Object => "request body must be a dictionary",
            Self::Required => "A name and prompt text are required",
            Self::Text => "prompt text exceeded limit",
            Self::Limit => "prompt store exceeded limit",
            Self::Store => "prompt store unavailable",
            Self::Busy => "service is busy",
            Self::NotFound => "prompt not found",
            Self::Id => "invalid prompt ID",
        })
    }
}
impl std::error::Error for Error {}
#[derive(Debug)]
pub enum Operation {
    List,
    Add(String, String),
    Delete(i64),
}

pub fn parse_fields(bytes: &[u8]) -> Result<(String, String), Error> {
    parse_fields_json(&encoding::decode(bytes)?)
}
pub fn decode_body(bytes: &[u8]) -> Result<std::borrow::Cow<'_, str>, Error> {
    encoding::decode(bytes)
}
pub fn parse_fields_json(text: &str) -> Result<(String, String), Error> {
    text::fields(&text::parse(text.as_bytes())?)
}
pub fn json_content_type(value: Option<&str>) -> bool {
    value.is_none_or(|value| {
        let media = value.split(';').next().unwrap_or("").trim();
        media.split_once('/').is_some_and(|(kind, sub)| {
            kind.eq_ignore_ascii_case("application")
                && (sub.eq_ignore_ascii_case("json")
                    || sub
                        .get(sub.len().saturating_sub(5)..)
                        .is_some_and(|suffix| suffix.eq_ignore_ascii_case("+json")))
        })
    })
}
pub fn non_json_validation(bytes: &[u8]) -> Result<serde_json::Value, Error> {
    let input = std::str::from_utf8(bytes).map_err(|_| Error::Body)?;
    Ok(
        serde_json::json!({"detail":[{"type":"dict_type","loc":["body"],"msg":"Input should be a valid dictionary","input":input}]}),
    )
}
pub fn validation(bytes: &[u8]) -> Option<serde_json::Value> {
    validation_with_empty(bytes, bytes.is_empty())
}
pub fn validation_with_empty(bytes: &[u8], empty_body: bool) -> Option<serde_json::Value> {
    use serde_json::json;
    if (bytes.is_empty() && empty_body) || bytes == b"null" {
        return Some(
            json!({"detail":[{"type":"missing","loc":["body"],"msg":"Field required","input":null}]}),
        );
    }
    let raw = std::str::from_utf8(bytes).ok()?;
    match model_files::json::parse_diagnostics(raw,text::LIMITS) {
        Ok(model_files::json::Value::Null)=>Some(json!({"detail":[{"type":"missing","loc":["body"],"msg":"Field required","input":null}]})),
        Ok(model_files::json::Value::Obj(_))=>None,
        Ok(_)=>serde_json::from_slice::<serde_json::Value>(bytes).ok().map(|input|json!({"detail":[{"type":"dict_type","loc":["body"],"msg":"Input should be a valid dictionary","input":input}]})),
        Err((model_files::Error::Json(message),position))=>{
            let error=match message {
                "expected a string key"=>"Expecting property name enclosed in double quotes",
                "expected ':'"=>"Expecting ':' delimiter",
                "expected ',' or '}'"|"expected ',' or ']'"=>"Expecting ',' delimiter",
                "extra data after the value"=>"Extra data",
                "unterminated string"=>"Unterminated string starting at",
                "control character in string"=>"Invalid control character at",
                "bad escape"=>"Invalid \\escape",
                "short \\u escape"|"bad \\u escape"=>"Invalid \\uXXXX escape",
                _=>"Expecting value",
            };
            Some(json!({"detail":[{"type":"json_invalid","loc":["body",position],"msg":"JSON decode error","input":{},"ctx":{"error":error}}]}))
        },
        Err(_)=>None,
    }
}
#[derive(Debug, PartialEq, Eq)]
pub enum Route {
    Collection,
    Item(String),
    Redirect(String),
    NotOwned,
    Invalid,
}
pub fn classify(raw: &str) -> Route {
    // Establish ownership using only the fixed ASCII namespace. Malformed bytes in
    // another owner's path must retain that owner's dispatch and timeout policy.
    if !prompt_namespace(raw) {
        return Route::NotOwned;
    }
    if raw.len() > 8192 {
        return Route::Invalid;
    }
    let path = match crate::decode_segment(raw) {
        Ok(p) => p,
        Err(_) => return Route::Invalid,
    };
    let canonical = path.strip_suffix('/').unwrap_or(&path);
    let item = canonical
        .strip_prefix("/api/v1/prompts/")
        .filter(|id| !id.is_empty() && !id.contains('/'));
    if raw
        .as_bytes()
        .windows(3)
        .any(|b| b.eq_ignore_ascii_case(b"%2f"))
        && (path == "/api/v1/prompts" || path.starts_with("/api/v1/prompts/"))
    {
        return Route::Invalid;
    }
    if canonical != "/api/v1/prompts" && item.is_none() {
        return Route::NotOwned;
    }
    if canonical != path {
        return Route::Redirect(canonical.to_owned());
    }
    item.map_or(Route::Collection, |id| Route::Item(id.to_owned()))
}
fn prompt_namespace(raw: &str) -> bool {
    let mut bytes = raw.bytes();
    fn decoded(bytes: &mut impl Iterator<Item = u8>) -> Option<u8> {
        let byte = bytes.next()?;
        if byte != b'%' {
            return Some(byte);
        }
        let high = char::from(bytes.next()?).to_digit(16)?;
        let low = char::from(bytes.next()?).to_digit(16)?;
        u8::try_from(high * 16 + low).ok()
    }
    for expected in b"/api/v1/prompts" {
        if decoded(&mut bytes) != Some(*expected) {
            return false;
        }
    }
    match bytes.next() {
        None | Some(b'/') => true,
        Some(b'%') => bytes.next() == Some(b'2') && matches!(bytes.next(), Some(b'f' | b'F')),
        _ => false,
    }
}
pub fn parse_id(id: &str) -> Result<i64, Error> {
    if id.len() > 64 {
        return Err(Error::Id);
    }
    // Pydantic's integer string parser uses Unicode whitespace (not Python str.strip).
    let id = id.trim();
    let (negative, raw) = if let Some(s) = id.strip_prefix('-') {
        (true, s)
    } else {
        (false, id.strip_prefix('+').unwrap_or(id))
    };
    let (integer, fraction) = raw
        .split_once('.')
        .map_or((raw, None), |(a, b)| (a, Some(b)));
    if fraction.is_some_and(|s| s.is_empty() || !s.bytes().all(|b| b == b'0')) {
        return Err(Error::Id);
    }
    if integer.is_empty() {
        return Err(Error::Id);
    }
    let mut n = 0u64;
    let mut previous_digit = false;
    for (at, b) in integer.bytes().enumerate() {
        match b {
            b'0'..=b'9' => {
                n = n
                    .checked_mul(10)
                    .and_then(|n| n.checked_add(u64::from(b - b'0')))
                    .ok_or(Error::Id)?;
                previous_digit = true;
            }
            b'_' if previous_digit && at + 1 < integer.len() => previous_digit = false,
            _ => return Err(Error::Id),
        }
    }
    if !previous_digit {
        return Err(Error::Id);
    }
    if negative {
        if n == 1u64 << 63 {
            Ok(i64::MIN)
        } else {
            i64::try_from(n).map(|n| -n).map_err(|_| Error::Id)
        }
    } else {
        i64::try_from(n).map_err(|_| Error::Id)
    }
}

#[cfg(test)]
mod tests;
