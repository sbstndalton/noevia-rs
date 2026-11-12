//! CPython JSON input grammar, including its nonfinite extension and integer precision.
use crate::value::{decode_error, decode_utf8, integer, Kind, Result, Value};
struct Parser<'a> {
    text: &'a str,
    at: usize,
}
impl Parser<'_> {
    fn rest(&self) -> &str {
        self.text.get(self.at..).unwrap_or("")
    }
    fn byte(&self) -> Option<u8> {
        self.text.as_bytes().get(self.at).copied()
    }
    fn space(&mut self) {
        while self.byte().is_some_and(|c| b" \t\n\r".contains(&c)) {
            self.at += 1;
        }
    }
    fn error(&self, message: &str, at: usize) -> String {
        let prefix = self.text.get(..at).unwrap_or("");
        let line = prefix.chars().filter(|c| *c == '\n').count() + 1;
        let col = prefix.rsplit('\n').next().unwrap_or("").chars().count() + 1;
        let at = prefix.chars().count();
        format!("json.decoder.JSONDecodeError: {message}: line {line} column {col} (char {at})")
    }
    fn string(&mut self) -> Result<Value> {
        self.at += 1;
        let start = self.at.saturating_sub(1);
        let mut units = Vec::new();
        loop {
            let Some(c) = self.rest().chars().next() else {
                return Err(self.error("Unterminated string starting at", start));
            };
            self.at += c.len_utf8();
            match c {
                '"' => {
                    return Ok(match String::from_utf16(&units) {
                        Ok(s) => Value::string(s),
                        Err(_) => Value::new(Kind::Surrogate(units)),
                    })
                }
                '\\' => {
                    let slash = self.at - 1;
                    let Some(c) = self.byte() else {
                        return Err(self.error("Unterminated string starting at", start));
                    };
                    self.at += 1;
                    match c {
                        b'"' => units.push(34),
                        b'\\' => units.push(92),
                        b'/' => units.push(47),
                        b'b' => units.push(8),
                        b'f' => units.push(12),
                        b'n' => units.push(10),
                        b'r' => units.push(13),
                        b't' => units.push(9),
                        b'u' => {
                            let pos = self.at;
                            let raw = self
                                .text
                                .get(pos..pos + 4)
                                .ok_or_else(|| self.error("Invalid \\uXXXX escape", pos - 1))?;
                            let n = u16::from_str_radix(raw, 16)
                                .map_err(|_| self.error("Invalid \\uXXXX escape", pos - 1))?;
                            units.push(n);
                            self.at += 4;
                        }
                        _ => return Err(self.error("Invalid \\escape", slash)),
                    }
                }
                c if c < ' ' => {
                    return Err(self.error("Invalid control character at", self.at - c.len_utf8()))
                }
                c => units.extend_from_slice(c.encode_utf16(&mut [0; 2])),
            }
        }
    }
    fn value(&mut self, depth: usize) -> Result<Value> {
        if depth > 500 {
            return Err("RecursionError: maximum recursion depth exceeded while decoding a JSON object from a unicode string".into());
        }
        self.space();
        for (token, kind) in [
            ("null", Kind::Null),
            ("true", Kind::Bool(true)),
            ("false", Kind::Bool(false)),
            ("NaN", Kind::Float(f64::NAN)),
            ("Infinity", Kind::Float(f64::INFINITY)),
            ("-Infinity", Kind::Float(f64::NEG_INFINITY)),
        ] {
            if self.rest().starts_with(token) {
                self.at += token.len();
                return Ok(Value::new(kind));
            }
        }
        match self.byte() {
            Some(b'"') => self.string(),
            Some(b'{') => {
                self.at += 1;
                let map = Value::map();
                self.space();
                if self.byte() == Some(b'}') {
                    self.at += 1;
                    return Ok(map);
                }
                loop {
                    self.space();
                    if self.byte() != Some(b'"') {
                        return Err(self
                            .error("Expecting property name enclosed in double quotes", self.at));
                    }
                    let k = self.string()?;
                    self.space();
                    if self.byte() != Some(b':') {
                        return Err(self.error("Expecting ':' delimiter", self.at));
                    }
                    self.at += 1;
                    let v = self.value(depth + 1)?;
                    map.insert(k, v)?;
                    self.space();
                    match self.byte() {
                        Some(b',') => {
                            self.at += 1;
                        }
                        Some(b'}') => {
                            self.at += 1;
                            return Ok(map);
                        }
                        _ => return Err(self.error("Expecting ',' delimiter", self.at)),
                    }
                }
            }
            Some(b'[') => {
                self.at += 1;
                let mut values = Vec::new();
                self.space();
                if self.byte() == Some(b']') {
                    self.at += 1;
                    return Ok(Value::new(Kind::Seq(values)));
                }
                loop {
                    values.push(self.value(depth + 1)?);
                    self.space();
                    match self.byte() {
                        Some(b',') => {
                            self.at += 1;
                        }
                        Some(b']') => {
                            self.at += 1;
                            return Ok(Value::new(Kind::Seq(values)));
                        }
                        _ => return Err(self.error("Expecting ',' delimiter", self.at)),
                    }
                }
            }
            Some(b'-' | b'0'..=b'9') => {
                let start = self.at;
                if self.byte() == Some(b'-') {
                    self.at += 1;
                }
                match self.byte() {
                    Some(b'0') => {
                        self.at += 1;
                    }
                    Some(b'1'..=b'9') => {
                        while self.byte().is_some_and(|c| c.is_ascii_digit()) {
                            self.at += 1;
                        }
                    }
                    _ => return Err(self.error("Expecting value", start)),
                }
                if self.byte() == Some(b'.')
                    && self
                        .text
                        .as_bytes()
                        .get(self.at + 1)
                        .is_some_and(u8::is_ascii_digit)
                {
                    self.at += 1;
                    while self.byte().is_some_and(|c| c.is_ascii_digit()) {
                        self.at += 1;
                    }
                }
                if matches!(self.byte(), Some(b'e' | b'E')) {
                    let saved = self.at;
                    self.at += 1;
                    if matches!(self.byte(), Some(b'+' | b'-')) {
                        self.at += 1;
                    }
                    if self.byte().is_some_and(|c| c.is_ascii_digit()) {
                        while self.byte().is_some_and(|c| c.is_ascii_digit()) {
                            self.at += 1;
                        }
                    } else {
                        self.at = saved;
                    }
                }
                let token = self.text.get(start..self.at).unwrap_or("");
                if token.contains(['.', 'e', 'E']) {
                    Ok(Value::new(Kind::Float(
                        token
                            .parse()
                            .map_err(|_| self.error("Expecting value", start))?,
                    )))
                } else {
                    integer(token, 10)
                }
            }
            _ => Err(self.error("Expecting value", self.at)),
        }
    }
}
fn decode(bytes: &[u8]) -> Result<String> {
    let first = |n| bytes.get(n).copied().unwrap_or(1);
    let (width, little, skip) = if bytes.starts_with(&[0xff, 0xfe, 0, 0]) {
        (4, true, 4)
    } else if bytes.starts_with(&[0, 0, 0xfe, 0xff]) {
        (4, false, 4)
    } else if bytes.starts_with(&[0xff, 0xfe]) {
        (2, true, 2)
    } else if bytes.starts_with(&[0xfe, 0xff]) {
        (2, false, 2)
    } else if bytes.len() >= 4 && first(0) == 0 {
        if first(1) == 0 {
            (4, false, 0)
        } else {
            (2, false, 0)
        }
    } else if bytes.len() >= 4 && first(1) == 0 {
        if first(2) == 0 && first(3) == 0 {
            (4, true, 0)
        } else {
            (2, true, 0)
        }
    } else if bytes.len() == 2 && first(0) == 0 {
        (2, false, 0)
    } else if bytes.len() == 2 && first(1) == 0 {
        (2, true, 0)
    } else {
        (1, false, 0)
    };
    let body = bytes.get(skip..).unwrap_or(&[]);
    if width > 1 && (body.len() % width) != 0 {
        let at = bytes.len() - body.len() % width;
        let codec = if width == 4 {
            if little {
                "utf-32-le"
            } else {
                "utf-32-be"
            }
        } else if little {
            "utf-16-le"
        } else {
            "utf-16-be"
        };
        return Err(decode_error(
            codec,
            bytes,
            at,
            bytes.len() - at,
            "truncated data",
        ));
    }
    match width {
        4 => {
            let mut text = String::new();
            for chunk in body.as_chunks::<4>().0 {
                let n = if little {
                    u32::from_le_bytes(*chunk)
                } else {
                    u32::from_be_bytes(*chunk)
                };
                text.push(
                    char::from_u32(n).ok_or("UnicodeDecodeError: invalid UTF-32 code point")?,
                );
            }
            Ok(text)
        }
        2 => {
            let units = body
                .as_chunks::<2>()
                .0
                .iter()
                .map(|p| {
                    if little {
                        u16::from_le_bytes(*p)
                    } else {
                        u16::from_be_bytes(*p)
                    }
                })
                .collect::<Vec<_>>();
            String::from_utf16(&units).map_err(|_| "UnicodeDecodeError: invalid UTF-16".into())
        }
        _ => {
            if let Some(body) = body.strip_prefix(&[0xef, 0xbb, 0xbf]) {
                decode_utf8(body)
                    .map_err(|e| format!("{e}\ndecoding with 'utf-8-sig' codec failed"))
            } else {
                decode_utf8(body)
            }
        }
    }
}
pub fn parse(bytes: &[u8]) -> Result<Value> {
    let text = decode(bytes)?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    let mut parser = Parser { text, at: 0 };
    let value = parser.value(0)?;
    parser.space();
    if parser.at != text.len() {
        return Err(parser.error("Extra data", parser.at));
    }
    Ok(value)
}
