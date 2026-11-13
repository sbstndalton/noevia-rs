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
//! quantifiers `* + ?` on a single unit or class, and `?` on a group that cannot match empty. Only
//! single units and classes repeat without bound, and they are matched by a loop, so recursion
//! depth is bounded by the pattern, never by the text (`tests/node_helpers.rs` runs megabyte
//! inputs on a 256 KiB stack).
//!
//! Cost: a pattern that starts with `^` is tried at position 0 only. A pattern compiled with
//! [`Regex::memoized`] records, per repetition and text position, that the rest of the pattern
//! failed there. That is exact because every point of the pattern has one fixed continuation (no
//! captures or backreferences are read, and an optional group never matches empty), and a scan
//! stops at its first success; it bounds the work of a pattern such as `CITE_TAIL` to
//! O(text x pattern) plus the repetition loops, where V8 backtracks cubically.

use std::cell::RefCell;

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
    /// A class repeated `min..=max` times; the last field numbers it for the memo.
    Rep(Set, usize, usize, usize),
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
    reps: usize,
    memo: bool,
}

/// Why a pattern did not compile (only constant patterns are compiled; a test compiles each).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PatternError;

struct Parser {
    src: Vec<char>,
    at: usize,
    reps: usize,
}

/// Whether a sequence can match the empty string.
fn nullable(seq: &[Node]) -> bool {
    seq.iter().all(|n| match n {
        Node::Unit(_) | Node::Set(_) => false,
        Node::Rep(_, min, _, _) => *min == 0,
        Node::Alt(alts) => alts.iter().any(|a| nullable(a)),
        Node::Opt(_) | Node::Start | Node::End | Node::WordBoundary => true,
    })
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
            self.reps += 1;
            let id = self.reps - 1;
            seq.push(match atom {
                Node::Unit(u) => Node::Rep(set_of(vec![(u, u)], false, false), min, max, id),
                Node::Set(s) => Node::Rep(s, min, max, id),
                // JS fails an empty iteration of `?`; a group that can match empty is refused so
                // that every point of the pattern keeps one continuation (see the memo).
                Node::Alt(_) if (min, max) == (0, 1) && !nullable(std::slice::from_ref(&atom)) => {
                    Node::Opt(vec![atom])
                }
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
            reps: 0,
        };
        let seq = p.alternation()?;
        if p.at != p.src.len() {
            return Err(PatternError);
        }
        Ok(Regex {
            seq,
            icase,
            reps: p.reps,
            memo: false,
        })
    }

    /// The same pattern, matched with the failure memo (see the module notes).
    pub fn memoized(mut self) -> Regex {
        self.memo = true;
        self
    }

    /// The first match at or after `from`: its start and end, as `exec` with `lastIndex = from`.
    pub fn find_from(&self, text: &[u16], from: usize) -> Option<(usize, usize)> {
        let anchored = matches!(self.seq.first(), Some(Node::Start));
        let last = if anchored { 0 } else { text.len() };
        let m = Matcher {
            re: self,
            text,
            width: text.len() + 1,
            failed: RefCell::new(if self.memo {
                vec![0u64; (self.reps * (text.len() + 1)).div_ceil(64)]
            } else {
                Vec::new()
            }),
        };
        (from..=last).find_map(|start| {
            let mut end = None;
            m.run(&self.seq, start, &mut |e| {
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

    /// `text.replace(re, '')` for a non-global `re` that starts with `^`: what follows the match
    /// (the text itself when there is none), without a copy.
    pub fn strip_prefix<'t>(&self, text: &'t [u16]) -> &'t [u16] {
        match self.find_from(text, 0) {
            Some((0, e)) => text.get(e..).unwrap_or_default(),
            _ => text,
        }
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

    /// The spans `text.matchAll(re)` yields for a global `re`, lazily (an empty match steps one
    /// unit on).
    pub fn find_iter<'r, 't>(
        &'r self,
        text: &'t [u16],
    ) -> impl Iterator<Item = (usize, usize)> + use<'r, 't> {
        let mut from = Some(0usize);
        std::iter::from_fn(move || {
            let (s, e) = self.find_from(text, from?)?;
            from = Some(if e == s { e + 1 } else { e }).filter(|&f| f <= text.len());
            Some((s, e))
        })
    }
}

/// One scan of one text.
struct Matcher<'r, 't> {
    re: &'r Regex,
    text: &'t [u16],
    width: usize,
    /// Bit `rep * width + pos`: the pattern after repetition `rep` failed from `pos`.
    failed: RefCell<Vec<u64>>,
}

impl Matcher<'_, '_> {
    fn known_failure(&self, rep: usize, pos: usize) -> bool {
        let bit = rep * self.width + pos;
        self.failed
            .try_borrow()
            .is_ok_and(|f| f.get(bit / 64).is_some_and(|w| w & (1 << (bit % 64)) != 0))
    }

    fn note_failure(&self, rep: usize, pos: usize) {
        let bit = rep * self.width + pos;
        if let Ok(mut f) = self.failed.try_borrow_mut() {
            if let Some(w) = f.get_mut(bit / 64) {
                *w |= 1 << (bit % 64);
            }
        }
    }

    fn run(&self, seq: &[Node], pos: usize, k: &mut dyn FnMut(usize) -> bool) -> bool {
        let Some((first, rest)) = seq.split_first() else {
            return k(pos);
        };
        let icase = self.re.icase;
        let at = |i: usize| self.text.get(i).copied();
        match first {
            Node::Unit(u) => {
                at(pos).is_some_and(|c| c == *u || (icase && fold(c) == fold(*u)))
                    && self.run(rest, pos + 1, k)
            }
            Node::Set(s) => {
                at(pos).is_some_and(|c| s.matches(c, icase)) && self.run(rest, pos + 1, k)
            }
            Node::Rep(s, min, max, id) => {
                let mut n = 0usize;
                while n < *max && at(pos + n).is_some_and(|c| s.matches(c, icase)) {
                    n += 1;
                }
                if n < *min {
                    return false;
                }
                (*min..=n).rev().any(|len| {
                    let p = pos + len;
                    if self.re.memo && self.known_failure(*id, p) {
                        return false;
                    }
                    let ok = self.run(rest, p, k);
                    if !ok && self.re.memo {
                        self.note_failure(*id, p);
                    }
                    ok
                })
            }
            Node::Alt(alts) => alts
                .iter()
                .any(|alt| self.run(alt, pos, &mut |end| self.run(rest, end, k))),
            Node::Opt(inner) => {
                self.run(inner, pos, &mut |end| self.run(rest, end, k)) || self.run(rest, pos, k)
            }
            Node::Start => pos == 0 && self.run(rest, pos, k),
            Node::End => pos == self.text.len() && self.run(rest, pos, k),
            Node::WordBoundary => {
                let before = pos.checked_sub(1).and_then(at).is_some_and(is_word);
                let after = at(pos).is_some_and(is_word);
                before != after && self.run(rest, pos, k)
            }
        }
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
