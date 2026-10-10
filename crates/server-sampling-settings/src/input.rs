//! Linear bounded JSON validation, retaining only the last decoded `enabled` field. RawValue
//! discards extra values without maps, numeric coercion or quadratic duplicate-key lookups.
use crate::BODY_LIMIT;
use serde_json::value::RawValue;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Json,
    Enabled,
}
fn raw(s: &str) -> Result<(&str, usize), Error> {
    let mut stream = serde_json::Deserializer::from_str(s).into_iter::<&RawValue>();
    let value = stream.next().ok_or(Error::Json)?.map_err(|_| Error::Json)?;
    Ok((value.get(), stream.byte_offset()))
}
fn ws(s: &str) -> &str {
    s.trim_start_matches([' ', '\n', '\r', '\t'])
}
fn enabled_key(raw: &str) -> bool {
    let Some(s) = raw.strip_prefix('"').and_then(|s| s.strip_suffix('"')) else {
        return false;
    };
    let mut chars = s.chars();
    for expected in "enabled".chars() {
        let c = match chars.next() {
            Some('\\') => {
                if chars.next() != Some('u') {
                    return false;
                }
                let mut code = 0u32;
                for _ in 0..4 {
                    let Some(n) = chars.next().and_then(|c| c.to_digit(16)) else {
                        return false;
                    };
                    code = code * 16 + n;
                }
                char::from_u32(code)
            }
            c => c,
        };
        if c != Some(expected) {
            return false;
        }
    }
    chars.next().is_none()
}
pub fn parse(bytes: &[u8]) -> Result<bool, Error> {
    if bytes.len() > BODY_LIMIT {
        return Err(Error::Json);
    }
    // Node readBody Buffer.toString replaces invalid UTF-8 before JSON.parse.
    let text = String::from_utf8_lossy(bytes);
    // Explicit depth128 bounds work/stack; quote/escape state avoids counting string braces.
    let (mut depth, mut quoted, mut escape) = (0usize, false, false);
    for b in text.bytes() {
        if quoted {
            if escape {
                escape = false;
            } else if b == b'\\' {
                escape = true;
            } else if b == b'"' {
                quoted = false;
            }
        } else if b == b'"' {
            quoted = true;
        } else if b == b'{' || b == b'[' {
            depth += 1;
            if depth > 128 {
                return Err(Error::Json);
            }
        } else if b == b'}' || b == b']' {
            depth = depth.saturating_sub(1);
        }
    }
    let (root, used) = raw(&text)?;
    if !ws(text.get(used..).ok_or(Error::Json)?).is_empty() {
        return Err(Error::Json);
    }
    let Some(mut rest) = root.strip_prefix('{').and_then(|s| s.strip_suffix('}')) else {
        return Err(Error::Enabled);
    };
    let mut enabled = None;
    rest = ws(rest);
    while !rest.is_empty() {
        let (key, used) = raw(rest)?;
        if !key.starts_with('"') {
            return Err(Error::Json);
        }
        rest = ws(rest.get(used..).ok_or(Error::Json)?);
        rest = ws(rest.strip_prefix(':').ok_or(Error::Json)?);
        let (value, used) = raw(rest)?;
        if enabled_key(key) {
            enabled = match value {
                "true" => Some(true),
                "false" => Some(false),
                _ => None,
            };
        }
        rest = ws(rest.get(used..).ok_or(Error::Json)?);
        if rest.is_empty() {
            break;
        }
        rest = ws(rest.strip_prefix(',').ok_or(Error::Json)?);
    }
    enabled.ok_or(Error::Enabled)
}
