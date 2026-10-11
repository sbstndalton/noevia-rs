//! Validate all JSON syntax, then retain only relevant last decoded fields. Unknown values
//! stay raw, so Node-accepted huge numbers and lone surrogate strings do not fail decoding.
use crate::BODY_LIMIT;
use serde_json::value::RawValue;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidJson;
fn whitespace(s: &str) -> &str {
    s.trim_start_matches([' ', '\t', '\r', '\n'])
}
fn raw(s: &str) -> Result<(&str, usize), InvalidJson> {
    let mut it = serde_json::Deserializer::from_str(s).into_iter::<&RawValue>();
    let v = it.next().ok_or(InvalidJson)?.map_err(|_| InvalidJson)?;
    Ok((v.get(), it.byte_offset()))
}
#[derive(Debug, Clone)]
pub struct Json(pub String);
impl Json {
    pub fn parse(bytes: &[u8]) -> Result<Self, InvalidJson> {
        if bytes.len() > BODY_LIMIT {
            return Err(InvalidJson);
        }
        let text = String::from_utf8_lossy(bytes);
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
                    return Err(InvalidJson);
                }
            } else if b == b'}' || b == b']' {
                depth = depth.saturating_sub(1);
            }
        }
        let (v, n) = raw(&text)?;
        if !whitespace(text.get(n..).ok_or(InvalidJson)?).is_empty() {
            return Err(InvalidJson);
        }
        Ok(Self(v.to_owned()))
    }
    pub fn object(&self) -> bool {
        self.0.starts_with('{')
    }
    pub fn null(&self) -> bool {
        self.0 == "null"
    }
    pub fn get(&self, wanted: &str) -> Option<Self> {
        let mut s = self.0.strip_prefix('{')?.strip_suffix('}')?;
        let mut out = None;
        s = whitespace(s);
        while !s.is_empty() {
            let (key, n) = raw(s).ok()?;
            s = whitespace(s.get(n..)?).strip_prefix(':')?;
            s = whitespace(s);
            let (value, n) = raw(s).ok()?;
            if serde_json::from_str::<String>(key).ok().as_deref() == Some(wanted) {
                out = Some(Self(value.into()));
            }
            s = whitespace(s.get(n..)?);
            if s.is_empty() {
                break;
            }
            s = whitespace(s.strip_prefix(',')?);
        }
        out
    }
    pub fn string(&self) -> Option<String> {
        serde_json::from_str(&self.0).ok()
    }
    pub fn array(&self) -> Option<Vec<Self>> {
        let mut s = whitespace(self.0.strip_prefix('[')?.strip_suffix(']')?);
        let mut out = Vec::new();
        while !s.is_empty() {
            let (v, n) = raw(s).ok()?;
            out.push(Self(v.into()));
            s = whitespace(s.get(n..)?);
            if s.is_empty() {
                break;
            }
            s = whitespace(s.strip_prefix(',')?);
        }
        Some(out)
    }
}
