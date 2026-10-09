//! The fixed regular expressions of tool-gate.cjs, matched as V8 matches them without the `u` flag:
//! over UTF-16 code units, `/i` folding ASCII letters only (in non-Unicode mode `Canonicalize` never
//! maps a non-ASCII unit to an ASCII one, and every literal here is ASCII), `\b` on the ASCII word
//! characters `[A-Za-z0-9_]`, `\s` the ECMAScript WhiteSpace and LineTerminator set, `\d` ASCII.
//!
//! A pattern is built from [`P`] and compiled to a small backtracking program with leftmost-first,
//! greedy semantics (the order ECMAScript's backtracking explores). None of the patterns nests a
//! quantifier inside another, so one start position costs at most a constant times the whitespace
//! run its literal prefix reaches; every step is charged to the request's [`Work`] budget anyway.
//! Start positions whose first unit cannot begin a match are skipped (one unit of work each).

use crate::{Work, R};
use prompt_framing::js::is_js_space;

/// A pattern.
pub enum P {
    /// ASCII text, matched case-insensitively (the patterns are all `/i` or letter-free).
    Lit(&'static str),
    /// One unit of a class.
    Class(fn(u16) -> bool),
    /// `\b`.
    WordB,
    /// `^` (no `m` flag).
    Start,
    /// `$` (no `m` flag).
    End,
    /// Concatenation.
    Cat(Vec<P>),
    /// Alternation, in order.
    Alt(Vec<P>),
    /// Greedy `?`.
    Opt(Box<P>),
    /// Greedy `+`.
    Plus(Box<P>),
    /// Greedy `*`.
    Star(Box<P>),
    /// Capture group `n` (1-based).
    Group(usize, Box<P>),
}

#[derive(Clone, Copy)]
enum Inst {
    Unit(u16),
    Class(fn(u16) -> bool),
    WordB,
    Start,
    End,
    Split(usize, usize),
    Jmp(usize),
    Save(usize),
    Match,
}

/// A compiled pattern.
pub struct Re {
    prog: Vec<Inst>,
    groups: usize,
    first: fn(u16) -> bool,
}

/// `[A-Za-z0-9_]`.
pub const fn is_word(c: u16) -> bool {
    matches!(c, 0x30..=0x39 | 0x41..=0x5a | 0x5f | 0x61..=0x7a)
}

/// `\s`.
pub fn space(c: u16) -> bool {
    is_js_space(c)
}

/// `\d`.
pub fn digit(c: u16) -> bool {
    (0x30..=0x39).contains(&c)
}

const fn lower(c: u16) -> u16 {
    if c >= 0x41 && c <= 0x5a {
        c | 0x20
    } else {
        c
    }
}

fn compile(p: &P, out: &mut Vec<Inst>, groups: &mut usize) {
    match p {
        P::Lit(s) => out.extend(s.bytes().map(|b| Inst::Unit(lower(u16::from(b))))),
        P::Class(f) => out.push(Inst::Class(*f)),
        P::WordB => out.push(Inst::WordB),
        P::Start => out.push(Inst::Start),
        P::End => out.push(Inst::End),
        P::Cat(ps) => ps.iter().for_each(|q| compile(q, out, groups)),
        P::Alt(ps) => {
            let mut jumps = Vec::new();
            for (k, q) in ps.iter().enumerate() {
                if k + 1 < ps.len() {
                    let split = out.len();
                    out.push(Inst::Split(split + 1, 0));
                    compile(q, out, groups);
                    jumps.push(out.len());
                    out.push(Inst::Jmp(0));
                    let next = out.len();
                    if let Some(Inst::Split(_, b)) = out.get_mut(split) {
                        *b = next;
                    }
                } else {
                    compile(q, out, groups);
                }
            }
            let end = out.len();
            for j in jumps {
                if let Some(Inst::Jmp(t)) = out.get_mut(j) {
                    *t = end;
                }
            }
        }
        P::Opt(q) => {
            let split = out.len();
            out.push(Inst::Split(split + 1, 0));
            compile(q, out, groups);
            let end = out.len();
            if let Some(Inst::Split(_, b)) = out.get_mut(split) {
                *b = end;
            }
        }
        P::Star(q) => {
            let split = out.len();
            out.push(Inst::Split(split + 1, 0));
            compile(q, out, groups);
            out.push(Inst::Jmp(split));
            let end = out.len();
            if let Some(Inst::Split(_, b)) = out.get_mut(split) {
                *b = end;
            }
        }
        P::Plus(q) => {
            let start = out.len();
            compile(q, out, groups);
            let split = out.len();
            out.push(Inst::Split(start, split + 1));
        }
        P::Group(n, q) => {
            *groups = (*groups).max(*n);
            out.push(Inst::Save(2 * n));
            compile(q, out, groups);
            out.push(Inst::Save(2 * n + 1));
        }
    }
}

/// One match: `slots[2k..2k+2]` the bounds of group `k` (group 0 the whole match).
pub type Caps = Vec<Option<usize>>;

/// The bounds of group `k` of a match.
pub fn group(caps: &Caps, k: usize) -> Option<(usize, usize)> {
    match (
        caps.get(2 * k).copied().flatten(),
        caps.get(2 * k + 1).copied().flatten(),
    ) {
        (Some(a), Some(b)) if a <= b => Some((a, b)),
        _ => None,
    }
}

enum Step {
    Try(usize, usize),
    Restore(usize, Option<usize>),
}

impl Re {
    /// Compile `p`; `first` holds for every unit a match can start with (after a leading `\b`).
    pub fn new(p: P, first: fn(u16) -> bool) -> Re {
        let mut prog = vec![Inst::Save(0)];
        let mut groups = 0;
        compile(&p, &mut prog, &mut groups);
        prog.push(Inst::Save(1));
        prog.push(Inst::Match);
        Re {
            prog,
            groups,
            first,
        }
    }

    fn run(&self, s: &[u16], start: usize, work: &mut Work) -> R<Option<Caps>> {
        let mut caps: Caps = vec![None; 2 * (self.groups + 1)];
        let mut stack = vec![Step::Try(0, start)];
        while let Some(step) = stack.pop() {
            let (mut pc, mut at) = match step {
                Step::Restore(slot, old) => {
                    if let Some(c) = caps.get_mut(slot) {
                        *c = old;
                    }
                    continue;
                }
                Step::Try(pc, at) => (pc, at),
            };
            loop {
                work.charge(0)?;
                let Some(&inst) = self.prog.get(pc) else {
                    break;
                };
                match inst {
                    Inst::Unit(u) => match s.get(at) {
                        Some(&c) if (c < 0x80 && lower(c) == u) => {
                            pc += 1;
                            at += 1;
                        }
                        _ => break,
                    },
                    Inst::Class(f) => match s.get(at) {
                        Some(&c) if f(c) => {
                            pc += 1;
                            at += 1;
                        }
                        _ => break,
                    },
                    Inst::WordB => {
                        let before = at > 0 && s.get(at - 1).is_some_and(|&c| is_word(c));
                        let after = s.get(at).is_some_and(|&c| is_word(c));
                        if before == after {
                            break;
                        }
                        pc += 1;
                    }
                    Inst::Start => {
                        if at != 0 {
                            break;
                        }
                        pc += 1;
                    }
                    Inst::End => {
                        if at != s.len() {
                            break;
                        }
                        pc += 1;
                    }
                    Inst::Split(a, b) => {
                        stack.push(Step::Try(b, at));
                        pc = a;
                    }
                    Inst::Jmp(t) => pc = t,
                    Inst::Save(slot) => {
                        let old = caps.get(slot).copied().flatten();
                        stack.push(Step::Restore(slot, old));
                        if let Some(c) = caps.get_mut(slot) {
                            *c = Some(at);
                        }
                        pc += 1;
                    }
                    Inst::Match => return Ok(Some(caps)),
                }
            }
        }
        Ok(None)
    }

    /// The leftmost match at or after `from` (`exec` with `lastIndex = from`).
    pub fn find(&self, s: &[u16], from: usize, work: &mut Work) -> R<Option<Caps>> {
        let mut start = from;
        while start <= s.len() {
            work.charge(0)?;
            let can = match s.get(start) {
                Some(&c) => (self.first)(c),
                None => false,
            };
            if can {
                if let Some(c) = self.run(s, start, work)? {
                    return Ok(Some(c));
                }
            }
            start += 1;
        }
        Ok(None)
    }

    /// `re.test(s)`.
    pub fn test(&self, s: &[u16], work: &mut Work) -> R<bool> {
        Ok(self.find(s, 0, work)?.is_some())
    }

    /// `s.replace(/re/g, with)` (no `$` patterns in `with`).
    pub fn replace_all(&self, s: &[u16], with: &[u16], work: &mut Work) -> R<Vec<u16>> {
        let mut out = Vec::with_capacity(s.len());
        let mut from = 0;
        let mut copied = 0;
        while from <= s.len() {
            let Some(caps) = self.find(s, from, work)? else {
                break;
            };
            let Some((a, b)) = group(&caps, 0) else {
                break;
            };
            work.charge(b.saturating_sub(copied))?;
            out.extend_from_slice(s.get(copied..a).unwrap_or(&[]));
            out.extend_from_slice(with);
            copied = b;
            // An empty match advances by one unit, as String.prototype.replace does.
            from = if b > a { b } else { b + 1 };
        }
        out.extend_from_slice(s.get(copied..).unwrap_or(&[]));
        Ok(out)
    }
}

/// `x{n}`.
pub fn rep(n: usize, f: fn(u16) -> bool) -> P {
    P::Cat((0..n).map(|_| P::Class(f)).collect())
}

/// `\s+`.
pub fn ws1() -> P {
    P::Plus(Box::new(P::Class(space)))
}

/// `\s*`.
pub fn ws0() -> P {
    P::Star(Box::new(P::Class(space)))
}

/// Words joined by `\s+` (`can\s+you`).
pub fn words(text: &'static str) -> P {
    let mut parts = Vec::new();
    for (k, w) in text.split(' ').enumerate() {
        if k > 0 {
            parts.push(ws1());
        }
        parts.push(P::Lit(w));
    }
    P::Cat(parts)
}

/// `\b(alternatives)\b` with the alternatives in order, as group 1.
pub fn bounded(alts: Vec<P>) -> P {
    P::Cat(vec![
        P::WordB,
        P::Group(1, Box::new(P::Alt(alts))),
        P::WordB,
    ])
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use prompt_framing::js::units;

    fn any(_: u16) -> bool {
        true
    }

    #[test]
    fn leftmost_first_and_greedy() {
        let mut w = Work::new(1 << 20);
        // /\b(folder|folders)\b/: the first alternative fails at its \b, so the second is used.
        let re = Re::new(bounded(vec![P::Lit("folder"), P::Lit("folders")]), any);
        let c = re.find(&units("my Folders."), 0, &mut w).unwrap().unwrap();
        assert_eq!(group(&c, 1), Some((3, 10)));
        // /search(?:\s+for)?\b/ backs out of "for" when "forward" follows.
        let re = Re::new(
            P::Cat(vec![
                P::Lit("search"),
                P::Opt(Box::new(P::Cat(vec![ws1(), P::Lit("for")]))),
                P::WordB,
            ]),
            any,
        );
        let c = re
            .find(&units("search forward"), 0, &mut w)
            .unwrap()
            .unwrap();
        assert_eq!(group(&c, 0), Some((0, 6)));
        let c = re
            .find(&units("search  for x"), 0, &mut w)
            .unwrap()
            .unwrap();
        assert_eq!(group(&c, 0), Some((0, 11)));
        let out = re
            .replace_all(&units("Search for a, search"), &units("_"), &mut w)
            .unwrap();
        assert_eq!(String::from_utf16(&out).unwrap(), "_ a, _");
    }

    #[test]
    fn ascii_only_folding_and_boundaries() {
        let mut w = Work::new(1 << 20);
        let re = Re::new(bounded(vec![P::Lit("news")]), any);
        assert!(re.test(&units("NEWS!"), &mut w).unwrap());
        // U+017F LATIN SMALL LETTER LONG S does not fold to s without the u flag.
        assert!(!re.test(&units("new\u{17f}"), &mut w).unwrap());
        // é is not a word character for \b.
        assert!(re.test(&units("énewsé"), &mut w).unwrap());
        assert!(!re.test(&units("_news"), &mut w).unwrap());
    }

    #[test]
    fn budget_stops() {
        let re = Re::new(P::Cat(vec![P::Lit("a"), ws0(), P::Lit("b")]), any);
        let s = units(&format!("a{}c", " ".repeat(1000)));
        assert!(re.test(&s, &mut Work::new(100)).is_err());
        assert!(!re.test(&s, &mut Work::new(1 << 20)).unwrap());
    }
}
