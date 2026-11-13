//! The subset of JavaScript's (non-unicode) regular expressions the four helpers use, matched over
//! UTF-16 code units with JavaScript's backtracking order, so a `replace` removes exactly the span
//! V8 removes.
//!
//! - `\s` is JavaScript's WhiteSpace and LineTerminator set ([`is_js_space`]), U+FEFF included.
//! - `\w` is `[A-Za-z0-9_]` and `\b` the boundary between `\w` and anything else (non-unicode).
//! - The `i` flag folds ASCII letters only: in a non-unicode JS regex a character at or above
//!   U+0080 never canonicalizes to an ASCII one (`ſ` is not `s`, the Kelvin sign is not `k`), and
//!   none of the patterns here carries a non-ASCII letter.
//! - `^` and `$` are the ends of the text (no `m` flag); `.` is not used.
//!
//! Supported syntax: literals, `\` escapes (`\s \w \b` and an escaped punctuation unit), classes
//! with ranges and negation, `(?:...)` and `(...)` groups (no captures are read), `|`, the greedy
//! quantifiers `* + ?` on a single unit or class, and `?` on a group. Only single units and classes
//! repeat without bound, and they are matched by a loop, so recursion depth is bounded by the
//! pattern, never by the text.

/// JavaScript's `\s` and the set `String.prototype.trim` removes (WhiteSpace and LineTerminator).
pub fn is_js_space(u: u16) -> bool {
    matches!(
        u,
        0x09..=0x0D
            | 0x20
            | 0xA0
            | 0x1680
            | 0x2000..=0x200A
            | 0x2028
            | 0x2029
            | 0x202F
            | 0x205F
            | 0x3000
            | 0xFEFF
    )
}

fn is_word(u: u16) -> bool {
    u8::try_from(u).is_ok_and(|b| b.is_ascii_alphanumeric() || b == b'_')
}

fn fold(u: u16) -> u16 {
    if (u16::from(b'a')..=u16::from(b'z')).contains(&u) {
        u - 0x20
    } else {
        u
    }
}

#[derive(Debug, Clone)]
struct Set {
    negated: bool,
    ranges: Vec<(u16, u16)>,
    space: bool,
    word: bool,
}

impl Set {
    fn contains_raw(&self, u: u16) -> bool {
        (self.space && is_js_space(u))
            || (self.word && is_word(u))
            || self.ranges.iter().any(|&(a, b)| a <= u && u <= b)
    }

    /// Non-unicode `/i`: a member matches when it equals the unit's ASCII upper or lower case.
    fn matches(&self, u: u16, icase: bool) -> bool {
        let lower = if (0x41..=0x5A).contains(&u) {
            u + 0x20
        } else {
            u
        };
        let hit = self.contains_raw(u)
            || (icase && (self.contains_raw(fold(u)) || self.contains_raw(lower)));
        hit != self.negated
    }
}

#[derive(Debug, Clone)]
enum Node {
    Unit(u16),
    Set(Set),
    Rep(Set, usize, usize),
    Alt(Vec<Vec<Node>>),
    Opt(Vec<Node>),
    Start,
    End,
    WordBoundary,
}

/// A compiled pattern.
#[derive(Debug, Clone)]
pub struct Regex {
    seq: Vec<Node>,
    icase: bool,
}

/// Why a pattern did not compile (only constant patterns are compiled; a test compiles each).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PatternError;

struct Parser {
    src: Vec<char>,
    at: usize,
}

fn unit(c: char) -> Result<u16, PatternError> {
    u16::try_from(u32::from(c)).map_err(|_| PatternError)
}

fn set_of(ranges: Vec<(u16, u16)>, space: bool, word: bool) -> Set {
    Set {
        negated: false,
        ranges,
        space,
        word,
    }
}

impl Parser {
    fn peek(&self) -> Option<char> {
        self.src.get(self.at).copied()
    }

    fn next(&mut self) -> Result<char, PatternError> {
        let c = self.peek().ok_or(PatternError)?;
        self.at += 1;
        Ok(c)
    }

    fn eat(&mut self, c: char) -> bool {
        let hit = self.peek() == Some(c);
        self.at += usize::from(hit);
        hit
    }

    fn alternation(&mut self) -> Result<Vec<Node>, PatternError> {
        let mut alts = vec![self.sequence()?];
        while self.eat('|') {
            alts.push(self.sequence()?);
        }
        Ok(if alts.len() == 1 {
            alts.pop().unwrap_or_default()
        } else {
            vec![Node::Alt(alts)]
        })
    }

    fn sequence(&mut self) -> Result<Vec<Node>, PatternError> {
        let mut seq = Vec::new();
        while !matches!(self.peek(), None | Some('|' | ')')) {
            let atom = self.atom()?;
            let (min, max) = match self.peek() {
                Some('*') => (0, usize::MAX),
                Some('+') => (1, usize::MAX),
                Some('?') => (0, 1),
                Some('{') => return Err(PatternError),
                _ => {
                    seq.push(atom);
                    continue;
                }
            };
            self.at += 1;
            if self.peek() == Some('?') {
                return Err(PatternError); // lazy quantifiers are not used
            }
            seq.push(match atom {
                Node::Unit(u) => Node::Rep(set_of(vec![(u, u)], false, false), min, max),
                Node::Set(s) => Node::Rep(s, min, max),
                Node::Alt(_) if (min, max) == (0, 1) => Node::Opt(vec![atom]),
                _ => return Err(PatternError),
            });
        }
        Ok(seq)
    }

    fn atom(&mut self) -> Result<Node, PatternError> {
        match self.next()? {
            '^' => Ok(Node::Start),
            '$' => Ok(Node::End),
            '.' | '*' | '+' | '?' | '{' | ']' => Err(PatternError),
            '(' => {
                if self.eat('?') && !self.eat(':') {
                    return Err(PatternError);
                }
                let inner = self.alternation()?;
                if !self.eat(')') {
                    return Err(PatternError);
                }
                // A group is an alternation of sequences, so it can carry a quantifier as one node.
                Ok(match inner.as_slice() {
                    [Node::Alt(_)] => inner.into_iter().next().ok_or(PatternError)?,
                    _ => Node::Alt(vec![inner]),
                })
            }
            '[' => self.class().map(Node::Set),
            '\\' => match self.next()? {
                'b' => Ok(Node::WordBoundary),
                e => escape(e).map(|e| e.map_or_else(Node::Set, Node::Unit)),
            },
            c => unit(c).map(Node::Unit),
        }
    }

    /// One class member: a unit, or the set an escape such as `\s` stands for.
    fn member(&mut self) -> Result<Result<u16, Set>, PatternError> {
        match self.next()? {
            '\\' => escape(self.next()?),
            c => unit(c).map(Ok),
        }
    }

    fn class(&mut self) -> Result<Set, PatternError> {
        let mut set = set_of(Vec::new(), false, false);
        set.negated = self.eat('^');
        while !self.eat(']') {
            match self.member()? {
                Err(inner) => {
                    set.space |= inner.space;
                    set.word |= inner.word;
                }
                Ok(lo) if self.peek() == Some('-') && self.src.get(self.at + 1) != Some(&']') => {
                    self.at += 1;
                    let hi = self.member()?.map_err(|_| PatternError)?;
                    if hi < lo {
                        return Err(PatternError);
                    }
                    set.ranges.push((lo, hi));
                }
                Ok(lo) => set.ranges.push((lo, lo)),
            }
        }
        Ok(set)
    }
}

/// `\s` and `\w` as sets, an escaped punctuation unit as itself; other escapes are not used.
fn escape(e: char) -> Result<Result<u16, Set>, PatternError> {
    match e {
        's' => Ok(Err(set_of(Vec::new(), true, false))),
        'w' => Ok(Err(set_of(Vec::new(), false, true))),
        c if c.is_ascii_alphanumeric() => Err(PatternError),
        c => unit(c).map(Ok),
    }
}

impl Regex {
    /// Compiles `source` as `new RegExp(source, icase ? 'i' : '')` would, within the subset.
    pub fn new(source: &str, icase: bool) -> Result<Regex, PatternError> {
        let mut p = Parser {
            src: source.chars().collect(),
            at: 0,
        };
        let seq = p.alternation()?;
        if p.at != p.src.len() {
            return Err(PatternError);
        }
        Ok(Regex { seq, icase })
    }

    fn run(
        &self,
        text: &[u16],
        seq: &[Node],
        pos: usize,
        k: &mut dyn FnMut(usize) -> bool,
    ) -> bool {
        let Some((first, rest)) = seq.split_first() else {
            return k(pos);
        };
        let at = |i: usize| text.get(i).copied();
        match first {
            Node::Unit(u) => {
                at(pos).is_some_and(|c| c == *u || (self.icase && fold(c) == fold(*u)))
                    && self.run(text, rest, pos + 1, k)
            }
            Node::Set(s) => {
                at(pos).is_some_and(|c| s.matches(c, self.icase))
                    && self.run(text, rest, pos + 1, k)
            }
            Node::Rep(s, min, max) => {
                let mut n = 0usize;
                while n < *max && at(pos + n).is_some_and(|c| s.matches(c, self.icase)) {
                    n += 1;
                }
                if n < *min {
                    return false;
                }
                (*min..=n)
                    .rev()
                    .any(|len| self.run(text, rest, pos + len, k))
            }
            Node::Alt(alts) => alts
                .iter()
                .any(|alt| self.run(text, alt, pos, &mut |end| self.run(text, rest, end, k))),
            Node::Opt(inner) => {
                self.run(text, inner, pos, &mut |end| {
                    end != pos && self.run(text, rest, end, k)
                }) || self.run(text, rest, pos, k)
            }
            Node::Start => pos == 0 && self.run(text, rest, pos, k),
            Node::End => pos == text.len() && self.run(text, rest, pos, k),
            Node::WordBoundary => {
                let before = pos.checked_sub(1).and_then(at).is_some_and(is_word);
                let after = at(pos).is_some_and(is_word);
                before != after && self.run(text, rest, pos, k)
            }
        }
    }

    /// The first match at or after `from`: its start and end, as `exec` with `lastIndex = from`.
    pub fn find_from(&self, text: &[u16], from: usize) -> Option<(usize, usize)> {
        (from..=text.len()).find_map(|start| {
            let mut end = None;
            self.run(text, &self.seq, start, &mut |e| {
                end = Some(e);
                true
            });
            end.map(|e| (start, e))
        })
    }

    /// `re.test(text)` for a non-global `re`.
    pub fn test(&self, text: &[u16]) -> bool {
        self.find_from(text, 0).is_some()
    }

    /// `text.replace(re, '')` for a non-global `re`: the first match removed.
    pub fn remove_first(&self, text: &[u16]) -> Vec<u16> {
        match self.find_from(text, 0) {
            Some((s, e)) => {
                let mut out = text.get(..s).unwrap_or_default().to_vec();
                out.extend_from_slice(text.get(e..).unwrap_or_default());
                out
            }
            None => text.to_vec(),
        }
    }

    /// The spans `text.matchAll(re)` yields for a global `re` (an empty match steps one unit on).
    pub fn find_all(&self, text: &[u16]) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        let mut from = 0usize;
        while from <= text.len() {
            let Some((s, e)) = self.find_from(text, from) else {
                break;
            };
            out.push((s, e));
            from = if e == s { e + 1 } else { e };
        }
        out
    }
}

/// `String.prototype.trim` over UTF-16 units.
pub fn trim(text: &[u16]) -> &[u16] {
    let start = text
        .iter()
        .position(|&u| !is_js_space(u))
        .unwrap_or(text.len());
    let end = text
        .iter()
        .rposition(|&u| !is_js_space(u))
        .map_or(start, |i| i + 1);
    text.get(start..end).unwrap_or_default()
}

/// `text.replace(/[set]+$/, '')`: the trailing run of units in the set removed. The same result as
/// the regex (its leftmost match is the whole maximal run), without its quadratic backtracking.
pub fn strip_trailing(text: &[u16], in_set: impl Fn(u16) -> bool) -> &[u16] {
    let end = text.iter().rposition(|&u| !in_set(u)).map_or(0, |i| i + 1);
    text.get(..end).unwrap_or_default()
}
