//! The message of the `SyntaxError` Node's `JSON.parse` (V8, the Node 22 line production runs)
//! throws for a text, as UTF-16 code units, or `None` when V8 accepts the text. The MTP artifact
//! check reports it as its reason when the source's revision reply is not JSON (core
//! mtp-artifact.cjs `readCappedJson`). tests/fixtures/json-parse-errors-node.json holds Node's own
//! messages for the grammar's error points.
//!
//! The scan is iterative (an explicit stack), so a deeply nested reply cannot exhaust the stack.

const WS: [u16; 4] = [0x20, 0x09, 0x0a, 0x0d];

enum Fault {
    /// Nothing left where a token was needed: "Unexpected end of JSON input".
    End,
    /// The generic token message, for the code unit at the position.
    Token(usize),
    /// "Unexpected number in JSON at position ...".
    Number(usize),
    /// A fixed message with a position.
    At(&'static str, usize),
}

enum Frame {
    Array,
    Object,
}

struct Scan<'a> {
    s: &'a [u16],
}

fn is_digit(c: Option<u16>) -> bool {
    c.is_some_and(|c| (0x30..=0x39).contains(&c))
}

fn is_hex(c: Option<u16>) -> bool {
    c.is_some_and(|c| {
        (0x30..=0x39).contains(&c) || (0x41..=0x46).contains(&c) || (0x61..=0x66).contains(&c)
    })
}

impl Scan<'_> {
    fn at(&self, i: usize) -> Option<u16> {
        self.s.get(i).copied()
    }
    fn ws(&self, mut i: usize) -> usize {
        while self.at(i).is_some_and(|c| WS.contains(&c)) {
            i += 1;
        }
        i
    }
    fn string(&self, i: usize) -> Result<usize, Fault> {
        let mut j = i + 1;
        loop {
            let Some(c) = self.at(j) else {
                return Err(Fault::At("Unterminated string", j));
            };
            match c {
                0x22 => return Ok(j + 1),
                0x5c => {
                    let Some(e) = self.at(j + 1) else {
                        return Err(Fault::End);
                    };
                    match e {
                        0x22 | 0x5c | 0x2f | 0x62 | 0x66 | 0x6e | 0x72 | 0x74 => j += 2,
                        0x75 => {
                            for k in 0..4 {
                                if !is_hex(self.at(j + 2 + k)) {
                                    return Err(Fault::At("Bad Unicode escape", j + 2 + k));
                                }
                            }
                            j += 6;
                        }
                        _ => return Err(Fault::At("Bad escaped character", j + 1)),
                    }
                }
                c if c < 0x20 => {
                    return Err(Fault::At("Bad control character in string literal", j))
                }
                _ => j += 1,
            }
        }
    }
    fn number(&self, i: usize) -> Result<usize, Fault> {
        let mut j = i;
        if self.at(j) == Some(0x2d) {
            j += 1;
            if !is_digit(self.at(j)) {
                return Err(Fault::At("No number after minus sign", j));
            }
        }
        if self.at(j) == Some(0x30) {
            j += 1;
            if is_digit(self.at(j)) {
                return Err(Fault::Number(j));
            }
        } else {
            while is_digit(self.at(j)) {
                j += 1;
            }
        }
        if self.at(j) == Some(0x2e) {
            j += 1;
            if !is_digit(self.at(j)) {
                return Err(Fault::At("Unterminated fractional number", j));
            }
            while is_digit(self.at(j)) {
                j += 1;
            }
        }
        if matches!(self.at(j), Some(0x65 | 0x45)) {
            j += 1;
            if matches!(self.at(j), Some(0x2b | 0x2d)) {
                j += 1;
            }
            if !is_digit(self.at(j)) {
                return Err(Fault::At("Exponent part is missing a number", j));
            }
            while is_digit(self.at(j)) {
                j += 1;
            }
        }
        Ok(j)
    }
    fn literal(&self, i: usize, word: &str) -> Result<usize, Fault> {
        for (k, w) in word.encode_utf16().enumerate() {
            match self.at(i + k) {
                None => return Err(Fault::End),
                Some(c) if c != w => return Err(Fault::Token(i + k)),
                Some(_) => {}
            }
        }
        Ok(i + word.len())
    }
    /// A scalar value at `i` (after whitespace), or the container it opens.
    fn value(&self, i: usize, stack: &mut Vec<Frame>) -> Result<Option<usize>, Fault> {
        match self.at(i) {
            None => Err(Fault::End),
            Some(0x22) => self.string(i).map(Some),
            Some(c) if c == 0x2d || (0x30..=0x39).contains(&c) => self.number(i).map(Some),
            Some(0x74) => self.literal(i, "true").map(Some),
            Some(0x66) => self.literal(i, "false").map(Some),
            Some(0x6e) => self.literal(i, "null").map(Some),
            Some(0x5b) => {
                stack.push(Frame::Array);
                Ok(None)
            }
            Some(0x7b) => {
                stack.push(Frame::Object);
                Ok(None)
            }
            Some(_) => Err(Fault::Token(i)),
        }
    }
    /// A property name and its colon at `j` (after whitespace); the position of the value.
    fn key(&self, j: usize, missing: &'static str) -> Result<usize, Fault> {
        if self.at(j) != Some(0x22) {
            return Err(Fault::At(missing, j));
        }
        let k = self.ws(self.string(j)?);
        if self.at(k) != Some(0x3a) {
            return Err(Fault::At("Expected ':' after property name", k));
        }
        Ok(self.ws(k + 1))
    }
    fn run(&self) -> Result<(), Fault> {
        let mut stack: Vec<Frame> = Vec::new();
        let mut i = self.ws(0);
        // `i` is where a value starts; after a scalar or a closed container, `end` is where it ended.
        loop {
            let mut end = match self.value(i, &mut stack)? {
                Some(e) => e,
                None => {
                    let j = self.ws(i + 1);
                    match stack.last() {
                        Some(Frame::Array) if self.at(j) == Some(0x5d) => {
                            stack.pop();
                            j + 1
                        }
                        Some(Frame::Object) if self.at(j) == Some(0x7d) => {
                            stack.pop();
                            j + 1
                        }
                        Some(Frame::Array) => {
                            i = j;
                            continue;
                        }
                        Some(Frame::Object) => {
                            i = self.key(j, "Expected property name or '}'")?;
                            continue;
                        }
                        None => j,
                    }
                }
            };
            // After a value: close containers, or move to the next element.
            loop {
                let j = self.ws(end);
                match stack.last() {
                    None => {
                        if j < self.s.len() {
                            return Err(Fault::At(
                                "Unexpected non-whitespace character after JSON",
                                j,
                            ));
                        }
                        return Ok(());
                    }
                    Some(Frame::Array) => match self.at(j) {
                        Some(0x2c) => {
                            i = self.ws(j + 1);
                            break;
                        }
                        Some(0x5d) => {
                            stack.pop();
                            end = j + 1;
                        }
                        _ => return Err(Fault::At("Expected ',' or ']' after array element", j)),
                    },
                    Some(Frame::Object) => match self.at(j) {
                        Some(0x2c) => {
                            i = self.key(self.ws(j + 1), "Expected double-quoted property name")?;
                            break;
                        }
                        Some(0x7d) => {
                            stack.pop();
                            end = j + 1;
                        }
                        _ => return Err(Fault::At("Expected ',' or '}' after property value", j)),
                    },
                }
            }
        }
    }
}

/// `(line N column M)` of a position: `\n`, a lone `\r`, U+2028 and U+2029 end a line.
fn line_column(s: &[u16], pos: usize) -> (usize, usize) {
    let (mut line, mut start) = (1, 0);
    for (k, &c) in s.iter().enumerate().take(pos) {
        let ends =
            c == 0x0a || (c == 0x0d && s.get(k + 1) != Some(&0x0a)) || c == 0x2028 || c == 0x2029;
        if ends {
            line += 1;
            start = k + 1;
        }
    }
    (line, pos - start + 1)
}

fn units(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

const CONTEXT: usize = 10;

/// The `SyntaxError` message of `JSON.parse(text)`, or `None` when it parses.
pub fn parse_error(s: &[u16]) -> Option<Vec<u16>> {
    let fault = Scan { s }.run().err()?;
    let positioned = |msg: &str, pos: usize| {
        let (l, c) = line_column(s, pos);
        // "after JSON" already names it: V8's template has no second "in JSON".
        let place = if msg.ends_with("after JSON") {
            ""
        } else {
            " in JSON"
        };
        units(&format!(
            "{msg}{place} at position {pos} (line {l} column {c})"
        ))
    };
    Some(match fault {
        Fault::End => units("Unexpected end of JSON input"),
        Fault::Number(pos) => positioned("Unexpected number", pos),
        Fault::At(msg, pos) => positioned(msg, pos),
        Fault::Token(pos) => {
            if ["NaN", "Infinity", "undefined", "[object Object]"]
                .iter()
                .any(|w| units(w) == s)
            {
                let mut out = units("\"");
                out.extend_from_slice(s);
                out.extend(units("\" is not valid JSON"));
                return Some(out);
            }
            let mut out = units("Unexpected token '");
            out.extend(s.get(pos).copied());
            out.extend(units("', "));
            let n = s.len();
            let (from, to, before, after) = if n <= 2 * CONTEXT {
                (0, n, "", "")
            } else if pos < CONTEXT {
                (0, pos + CONTEXT, "", "...")
            } else if pos < n - CONTEXT {
                (pos - CONTEXT, pos + CONTEXT, "...", "...")
            } else {
                (pos - CONTEXT, n, "...", "")
            };
            out.extend(units(before));
            out.extend(units("\""));
            out.extend_from_slice(s.get(from..to.min(n)).unwrap_or_default());
            out.extend(units("\""));
            out.extend(units(after));
            out.extend(units(" is not valid JSON"));
            out
        }
    })
}
