//! The schema reader (JSON text, as `JSON.stringify` writes it, to a tree whose strings are
//! UTF-16 code units, so a lone surrogate in an enum value or a property name survives exactly)
//! and the reply writer (ASCII-only JSON: every unit outside printable ASCII as `\uXXXX`, which
//! `JSON.parse` turns back into the same units, lone surrogates included).

/// A JSON value. Object members keep their order; a repeated name keeps the last value, as
/// `JSON.parse` does.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum J {
    Null,
    Bool(bool),
    Num(f64),
    Str(Vec<u16>),
    Arr(Vec<J>),
    Obj(Vec<(Vec<u16>, J)>),
}

impl J {
    pub(crate) fn get(&self, name: &str) -> Option<&J> {
        let J::Obj(members) = self else { return None };
        let key: Vec<u16> = name.encode_utf16().collect();
        members.iter().find(|(k, _)| *k == key).map(|(_, v)| v)
    }

    /// JavaScript truthiness of the value `JSON.parse` would give.
    pub(crate) fn truthy(&self) -> bool {
        match self {
            J::Null => false,
            J::Bool(b) => *b,
            J::Num(n) => *n != 0.0 && !n.is_nan(),
            J::Str(s) => !s.is_empty(),
            J::Arr(_) | J::Obj(_) => true,
        }
    }
}

struct Reader<'a> {
    b: &'a [u8],
    i: usize,
}

/// Parse `text` as one JSON document, nesting at most `max_depth` arrays/objects deep.
pub(crate) fn parse(text: &[u8], max_depth: usize) -> Option<J> {
    let mut r = Reader { b: text, i: 0 };
    r.ws();
    let v = r.value(max_depth)?;
    r.ws();
    (r.i == r.b.len()).then_some(v)
}

impl Reader<'_> {
    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }

    fn ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.i += 1;
        }
    }

    fn lit(&mut self, word: &[u8], v: J) -> Option<J> {
        let end = self.i.checked_add(word.len())?;
        (self.b.get(self.i..end)? == word).then(|| {
            self.i = end;
            v
        })
    }

    fn value(&mut self, depth: usize) -> Option<J> {
        match self.peek()? {
            b'n' => self.lit(b"null", J::Null),
            b't' => self.lit(b"true", J::Bool(true)),
            b'f' => self.lit(b"false", J::Bool(false)),
            b'"' => self.string().map(J::Str),
            b'[' => {
                let depth = depth.checked_sub(1)?;
                self.i += 1;
                let mut out = Vec::new();
                self.ws();
                if self.peek()? == b']' {
                    self.i += 1;
                    return Some(J::Arr(out));
                }
                loop {
                    self.ws();
                    out.push(self.value(depth)?);
                    self.ws();
                    match self.peek()? {
                        b',' => self.i += 1,
                        b']' => {
                            self.i += 1;
                            return Some(J::Arr(out));
                        }
                        _ => return None,
                    }
                }
            }
            b'{' => {
                let depth = depth.checked_sub(1)?;
                self.i += 1;
                let mut out: Vec<(Vec<u16>, J)> = Vec::new();
                self.ws();
                if self.peek()? == b'}' {
                    self.i += 1;
                    return Some(J::Obj(out));
                }
                loop {
                    self.ws();
                    if self.peek()? != b'"' {
                        return None;
                    }
                    let k = self.string()?;
                    self.ws();
                    if self.peek()? != b':' {
                        return None;
                    }
                    self.i += 1;
                    self.ws();
                    let v = self.value(depth)?;
                    match out.iter_mut().find(|(ok, _)| *ok == k) {
                        Some(slot) => slot.1 = v,
                        None => out.push((k, v)),
                    }
                    self.ws();
                    match self.peek()? {
                        b',' => self.i += 1,
                        b'}' => {
                            self.i += 1;
                            return Some(J::Obj(out));
                        }
                        _ => return None,
                    }
                }
            }
            b'-' | b'0'..=b'9' => self.number(),
            _ => None,
        }
    }

    fn number(&mut self) -> Option<J> {
        let start = self.i;
        let digits = |r: &mut Self| {
            let s = r.i;
            while matches!(r.peek(), Some(b'0'..=b'9')) {
                r.i += 1;
            }
            r.i > s
        };
        if self.peek() == Some(b'-') {
            self.i += 1;
        }
        if self.peek() == Some(b'0') {
            self.i += 1;
        } else if !digits(self) {
            return None;
        }
        if self.peek() == Some(b'.') {
            self.i += 1;
            if !digits(self) {
                return None;
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.i += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.i += 1;
            }
            if !digits(self) {
                return None;
            }
        }
        let text = std::str::from_utf8(self.b.get(start..self.i)?).ok()?;
        text.parse::<f64>().ok().map(J::Num)
    }

    fn hex4(&mut self) -> Option<u16> {
        let mut v: u16 = 0;
        for _ in 0..4 {
            let d = char::from(self.peek()?).to_digit(16)?;
            v = v.checked_mul(16)?.checked_add(u16::try_from(d).ok()?)?;
            self.i += 1;
        }
        Some(v)
    }

    fn string(&mut self) -> Option<Vec<u16>> {
        self.i += 1; // the opening quote
        let mut out = Vec::new();
        loop {
            let c = self.peek()?;
            match c {
                b'"' => {
                    self.i += 1;
                    return Some(out);
                }
                b'\\' => {
                    self.i += 1;
                    let e = self.peek()?;
                    self.i += 1;
                    let unit = match e {
                        b'"' => 0x22,
                        b'\\' => 0x5c,
                        b'/' => 0x2f,
                        b'b' => 0x08,
                        b'f' => 0x0c,
                        b'n' => 0x0a,
                        b'r' => 0x0d,
                        b't' => 0x09,
                        b'u' => self.hex4()?,
                        _ => return None,
                    };
                    out.push(unit);
                }
                0x00..=0x1f => return None,
                _ => {
                    // One UTF-8 scalar; the reader only ever sees TextEncoder output.
                    let len = match c {
                        0x00..=0x7f => 1,
                        0xc2..=0xdf => 2,
                        0xe0..=0xef => 3,
                        0xf0..=0xf4 => 4,
                        _ => return None,
                    };
                    let end = self.i.checked_add(len)?;
                    let s = std::str::from_utf8(self.b.get(self.i..end)?).ok()?;
                    let ch = s.chars().next()?;
                    let mut buf = [0u16; 2];
                    out.extend_from_slice(ch.encode_utf16(&mut buf));
                    self.i = end;
                }
            }
        }
    }
}

/// Append `units` as a JSON string literal using only printable ASCII.
pub(crate) fn push_str(out: &mut Vec<u8>, units: &[u16]) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    out.push(b'"');
    for &u in units {
        match u {
            0x22 => out.extend_from_slice(b"\\\""),
            0x5c => out.extend_from_slice(b"\\\\"),
            0x20..=0x7e => out.push(u as u8),
            _ => {
                out.extend_from_slice(b"\\u");
                for shift in [12u16, 8, 4, 0] {
                    out.push(
                        HEX.get(usize::from((u >> shift) & 0xf))
                            .copied()
                            .unwrap_or(b'0'),
                    );
                }
            }
        }
    }
    out.push(b'"');
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;

    fn u(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    #[test]
    fn reads_what_stringify_writes() {
        let v = parse(
            br#"{"a":[1,-0,2.5e3,"x\ud800y",true,null],"a":{"\u00e9":"\u00e9"}}"#,
            8,
        )
        .unwrap();
        assert_eq!(
            v,
            J::Obj(vec![(
                u("a"),
                J::Obj(vec![(u("\u{e9}"), J::Str(u("\u{e9}")))])
            )])
        );
        let v = parse(br#"["x\ud800y", 1e400, -0]"#, 2).unwrap();
        let J::Arr(a) = v else { panic!() };
        assert_eq!(a[0], J::Str(vec![0x78, 0xd800, 0x79]));
        assert_eq!(a[1], J::Num(f64::INFINITY));
        assert!(matches!(a[2], J::Num(n) if n == 0.0 && n.is_sign_negative()));
        assert_eq!(
            parse("\"\u{1f600}\"".as_bytes(), 1),
            Some(J::Str(u("\u{1f600}")))
        );
    }

    #[test]
    fn refuses_what_json_parse_refuses() {
        for bad in [
            &b""[..],
            b"01",
            b"1.",
            b".5",
            b"+1",
            b"[1,]",
            b"{\"a\" 1}",
            b"\"\x01\"",
            b"\"\\x\"",
            b"\"\\u12\"",
            b"tru",
            b"[] []",
            b"{'a':1}",
            b"\"\xff\"",
            b"NaN",
        ] {
            assert_eq!(parse(bad, 8), None, "{bad:?}");
        }
        assert!(parse(b"[[[]]]", 3).is_some());
        assert_eq!(parse(b"[[[[]]]]", 3), None);
    }

    #[test]
    fn writes_ascii_json() {
        let mut out = Vec::new();
        push_str(&mut out, &[0x22, 0x5c, 0x0a, 0x41, 0xe9, 0xd800, 0x7f]);
        assert_eq!(out, br#""\"\\\u000aA\u00e9\ud800\u007f""#);
    }
}
