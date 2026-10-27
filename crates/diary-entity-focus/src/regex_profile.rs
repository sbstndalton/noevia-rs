//! Primitives for the finite entity-policy patterns, not a general Python regex engine.
//! Explicit Unicode15 classes and case equivalents preserve original UTF-8 offsets.
use crate::{generated_rules as rules, lexical, unicode15, Error};
use regex::Regex;
use std::sync::OnceLock;

pub(crate) const SPACE: &str = r"[\x{9}-\x{d}\x{1c}-\x{20}\x{85}\x{a0}\x{1680}\x{2000}-\x{200a}\x{2028}\x{2029}\x{202f}\x{205f}\x{3000}]";
fn range_content(ranges: &[(u32, u32)]) -> String {
    ranges
        .iter()
        .map(|(a, b)| format!(r"\x{{{a:x}}}-\x{{{b:x}}}"))
        .collect()
}
pub(crate) fn digits() -> &'static str {
    static VALUE: OnceLock<String> = OnceLock::new();
    VALUE.get_or_init(|| format!("[{}]", range_content(rules::DECIMAL_RANGES)))
}
pub(crate) fn letters() -> &'static str {
    static VALUE: OnceLock<String> = OnceLock::new();
    VALUE.get_or_init(|| {
        format!(
            "[{}&&[^{}]]",
            range_content(rules::ALNUM_RANGES),
            range_content(rules::DECIMAL_RANGES)
        )
    })
}
pub(crate) fn literal(value: &str) -> String {
    let mut out = String::new();
    for c in value.chars() {
        let members = unicode15::RE_CASES
            .binary_search_by_key(&(c as u32), |(cp, _)| *cp)
            .ok()
            .and_then(|i| unicode15::RE_CASES.get(i).map(|(_, members)| *members));
        if let Some(members) = members {
            out.push('[');
            for cp in members {
                out.push_str(&format!(r"\x{{{cp:x}}}"));
            }
            out.push(']');
        } else {
            out.push_str(&regex::escape(&c.to_string()));
        }
    }
    out
}
pub(crate) fn alternatives(values: &[&str]) -> String {
    values
        .iter()
        .map(|s| literal(s))
        .collect::<Vec<_>>()
        .join("|")
}
fn word(c: char) -> bool {
    c == '_' || lexical::py_alnum(c)
}
fn boundary(text: &str, at: usize) -> bool {
    let before = text
        .get(..at)
        .and_then(|s| s.chars().next_back())
        .is_some_and(word);
    let after = text
        .get(at..)
        .and_then(|s| s.chars().next())
        .is_some_and(word);
    before != after
}
#[derive(Clone)]
pub(crate) struct Pattern {
    re: Regex,
    left: bool,
    right: bool,
}
pub(crate) struct Hit<'a> {
    pub end: usize,
    pub capture: Option<&'a str>,
}
impl Pattern {
    pub fn new(body: &str, left: bool, right: bool) -> Result<Self, Error> {
        let re = Regex::new(&format!(r"\A(?:{body})")).map_err(|_| Error::Internal)?;
        if re.is_match("") {
            return Err(Error::Internal);
        }
        Ok(Self { re, left, right })
    }
    pub fn at<'a>(&self, text: &'a str, at: usize) -> Option<Hit<'a>> {
        if self.left && !boundary(text, at) {
            return None;
        }
        let captures = self.re.captures(text.get(at..)?)?;
        let matched = captures.get(0)?;
        let end = at + matched.end();
        if self.right && !boundary(text, end) {
            return None;
        }
        Some(Hit {
            end,
            capture: captures.get(1).map(|m| m.as_str()),
        })
    }
    fn from<'a>(&self, text: &'a str, start: usize) -> Option<Hit<'a>> {
        for (offset, _) in text.get(start..)?.char_indices() {
            if let Some(hit) = self.at(text, start + offset) {
                return Some(hit);
            }
        }
        None
    }
    pub fn is_match(&self, text: &str) -> bool {
        self.from(text, 0).is_some()
    }
    pub fn find_iter<'p, 'a>(&'p self, text: &'a str) -> Matches<'p, 'a> {
        Matches {
            pattern: self,
            text,
            next: 0,
        }
    }
}
pub(crate) struct Matches<'p, 'a> {
    pattern: &'p Pattern,
    text: &'a str,
    next: usize,
}
impl<'a> Iterator for Matches<'_, 'a> {
    type Item = Hit<'a>;
    fn next(&mut self) -> Option<Self::Item> {
        let hit = self.pattern.from(self.text, self.next)?;
        self.next = hit.end;
        Some(hit)
    }
}
pub(crate) fn keywords(values: &[&str]) -> Result<Vec<Pattern>, Error> {
    values
        .iter()
        .map(|s| Pattern::new(&literal(s), true, true))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::literal;
    use regex::Regex;
    use serde::Deserialize;
    #[derive(Deserialize)]
    struct Oracle {
        regex_pairs: Vec<(u32, u32, bool)>,
    }
    #[test]
    fn actual_python_ignore_case_classes() -> Result<(), Box<dyn std::error::Error>> {
        let oracle: Oracle = serde_json::from_str(include_str!("../tests/unicode15-oracle.json"))?;
        let mut current = None;
        for (a, b, expected) in oracle.regex_pairs {
            if current.as_ref().is_none_or(|(cp, _)| *cp != a) {
                let c = char::from_u32(a).ok_or("bad scalar")?;
                current = Some((
                    a,
                    Regex::new(&format!(r"\A(?:{})\z", literal(&c.to_string())))?,
                ));
            }
            let actual = current.as_ref().is_some_and(|(_, re)| {
                char::from_u32(b).is_some_and(|c| re.is_match(&c.to_string()))
            });
            assert_eq!(actual, expected, "re.I {a:x}/{b:x}");
        }
        Ok(())
    }
}

#[cfg(test)]
mod property_proof {
    use super::{digits, letters, word, SPACE};
    use regex::Regex;
    use serde::Deserialize;
    use sha2::{Digest, Sha256};
    #[derive(Deserialize)]
    struct Oracle {
        regex_properties_sha256: String,
    }
    #[test]
    fn all_scalar_property_classes_match_actual_python_regex(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let oracle: Oracle = serde_json::from_str(include_str!("../tests/unicode15-oracle.json"))?;
        let space = Regex::new(&format!(r"\A(?:{SPACE})\z"))?;
        let digit = Regex::new(&format!(r"\A(?:{})\z", digits()))?;
        let letter = Regex::new(&format!(r"\A(?:{})\z", letters()))?;
        let mut hash = Sha256::new();
        for cp in 0..=0x10ffff_u32 {
            if let Some(c) = char::from_u32(cp) {
                let s = c.to_string();
                let bits = u8::from(space.is_match(&s))
                    | (u8::from(digit.is_match(&s)) << 1)
                    | (u8::from(letter.is_match(&s)) << 2)
                    | (u8::from(word(c)) << 3);
                hash.update(cp.to_be_bytes());
                hash.update([bits]);
            }
        }
        assert_eq!(
            format!("{:x}", hash.finalize()),
            oracle.regex_properties_sha256
        );
        Ok(())
    }
}
