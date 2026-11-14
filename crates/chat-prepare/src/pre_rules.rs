//! Routing modes' deterministic pre-rules (#779, slice P5d, part 3): noevia-core
//! `server/routing-modes.cjs` `preRule(text, { health })` (the secret, IBAN, card and health rules
//! after `withoutNoeviaData`) and chat.cjs's `holdForCloud` verdict on a tool result (#779 F2,
//! ~997-1008: a Diary tool's result, or the pre-rules with the health rule only for a tool that is
//! not positively public, #1592).
//!
//! The rules are JS regular expressions without the `u` flag, so they see UTF-16 code units: `\s`
//! is JS's white space (with U+FEFF, without U+0085), `\b`, `\w` and `\d` are ASCII, `.` stops at
//! the four line terminators, `i` folds ASCII letters only (no `ſ` for `s`, no Kelvin sign for `k`),
//! and every count (`{4,200}`, `length >= 6`, the 60-unit health window, the 400,000-unit scan
//! limit) is in code units. So the text is scanned as one byte per code unit ([`units`]): an ASCII
//! unit as itself, a line terminator as `\n`, any other JS white space as `\v`, `…` as `0x81` (the
//! one non-ASCII character a rule names) and every other unit, each half of a surrogate pair
//! included, as `0x80`. Every rule reads only those classes, so the byte rules (`regex::bytes`
//! without Unicode) find what Node's find, at the same unit offsets. The regex crate reports the
//! leftmost-first match a backtracking engine reports; the one lookahead (`(?:=(?!=)|:(?!:))` in
//! the assignment rule) is checked after the match, and a match it refuses is searched again one
//! unit further on, as `matchAll` does (the match before the separator has a single path, so a
//! refused separator refuses the start).
//!
//! Recorded in `tests/fixtures/node-prerules.v1.json`, written by `tests/record_prerules.cjs`; no
//! named differences.

use std::sync::OnceLock;

use js_json::JValue;
use regex::bytes::Regex;

use crate::js;

/// routing-modes.cjs SCAN_LIMIT: the code units read.
pub const SCAN_LIMIT: usize = 400_000;

const LINE: u8 = b'\n';
const SPACE: u8 = 0x0B;
const ELLIPSIS: u8 = 0x81;
const OTHER: u8 = 0x80;

/// One byte per UTF-16 code unit of `text`, each in the classes the rules read (see the module doc).
pub fn units(text: &str) -> Vec<u8> {
    js::to_units(text)
        .into_iter()
        .map(|u| match u {
            0..=0x7F => u as u8,
            0x2028 | 0x2029 => LINE,
            0xA0 | 0x1680 | 0x2000..=0x200A | 0x202F | 0x205F | 0x3000 | 0xFEFF => SPACE,
            0x2026 => ELLIPSIS,
            _ => OTHER,
        })
        .collect()
}

struct Rules {
    private_key: Regex,
    bearer: Regex,
    assign: Regex,
    not_a_value: Regex,
    pin: Regex,
    placeholder: Regex,
    member_access: Regex,
    call: Regex,
    code_name: Regex,
    digit: Regex,
    letter: Regex,
    jwt: Regex,
    key_shape: Regex,
    iban: Regex,
    card: Regex,
    diagnosis: Regex,
    condition: Regex,
    record: Regex,
    chat_context: Regex,
    data_uri: Regex,
}

fn rules() -> Option<&'static Rules> {
    static R: OnceLock<Option<Rules>> = OnceLock::new();
    R.get_or_init(|| {
        // JS `.` without the s flag: anything but a line terminator (all of them are `\n` or `\r` here).
        let re = |src: &str| Regex::new(&format!("(?-u){}", src.replace("<dot>", r"[^\n\r]"))).ok();
        Some(Rules {
            private_key: re(r"-----BEGIN [A-Z ]{0,40}PRIVATE KEY-----")?,
            bearer: re(r"\bBearer\s{1,8}[A-Za-z0-9._~+/-]{16}")?,
            // SECRET_ASSIGN_RE without its lookahead (checked in `secret_assignment`).
            assign: re(
                r#"(?i)\b(password|passwort|passwd|passcode|pin|api[ _-]?key|secret|access[ _-]?token|auth[ _-]?token|token)\s{0,8}([=:])\s{0,8}(["'`]?)([^\s"'`;,)]{4,200})"#,
            )?,
            not_a_value: re(r"(?i)^(?:null|none|nil|true|false|undefined)$")?,
            pin: re(r"^\d{4,12}$")?,
            placeholder: re(
                r"(?i)^(?:x+|\*+|\.{3,}|\x81|<<dot>*>|\$\{<dot>*\}|your<dot>*|changeme|example<dot>*)$",
            )?,
            member_access: re(r"^[A-Za-z_$][\w$]*(?:\.[A-Za-z_$][\w$]*)+$")?,
            call: re(r"^[A-Za-z_$][\w$.]*\(")?,
            code_name: re(r"^[A-Za-z_$][A-Za-z_$]*\d{0,2}$")?,
            digit: re(r"\d")?,
            letter: re(r"[A-Za-z]")?,
            jwt: re(r"\beyJ[A-Za-z0-9_-]*(?:\.[A-Za-z0-9_-]*)*")?,
            key_shape: re(r"\b[a-z]{2,8}[-_][A-Za-z0-9_-]{24,}\b")?,
            iban: re(r"\b[A-Z]{2}\d{2}(?:[ ]?[A-Z0-9]{4}){2,7}(?:[ ]?[A-Z0-9]{1,4})?\b")?,
            card: re(r"\b\d(?:[ -]?\d){12,18}\b")?,
            diagnosis: re(r"(?i)\bdiagnos(?:is|es|ed|e)\b")?,
            condition: re(
                r"(?i)\b(?:hypertension|hypertensive|diabet\w*|cancer|asthma|depressi\w*|bipolar|schizophreni\w*|epilep\w*|dementia|alzheimer\w*|arthritis|pneumonia|hepatitis|hiv|covid\w*|migraine|tuberculosis|malaria|lupus|psoriasis|glaucoma|adhd|autism|ptsd|copd|an(?:a)?emia|thyroid|stroke|heart attack)\b",
            )?,
            record: re(
                r"(?i)\b(?:my|his|her|their)\s{1,3}(?:medical\s{1,3}(?:records?|history)|diagnosis)\b",
            )?,
            chat_context: re(r"\bcowork-chat-context-c-\d{10,16}-[a-z0-9]{1,8}/")?,
            data_uri: re(r"(?i)\bdata:image/(?:png|jpeg|webp|gif);base64,[A-Za-z0-9+/]+={0,2}")?,
        })
    })
    .as_ref()
}

const PASSWORD_WORDS: [&[u8]; 3] = [b"password", b"passwort", b"passwd"];

/// `secretAssignment(s)`.
fn secret_assignment(r: &Rules, s: &[u8]) -> bool {
    let mut at = 0;
    while let Some(m) = r.assign.captures_at(s, at) {
        let (Some(whole), Some(keyword), Some(sep), Some(quote), Some(value)) =
            (m.get(0), m.get(1), m.get(2), m.get(3), m.get(4))
        else {
            return false;
        };
        // `(?:=(?!=)|:(?!:))`: the unit after the separator is not the separator again.
        if s.get(sep.end()) == sep.as_bytes().first() {
            at = whole.start() + 1;
            continue;
        }
        at = whole.end();
        let keyword = keyword.as_bytes().to_ascii_lowercase();
        let (quoted, value) = (!quote.is_empty(), value.as_bytes());
        if r.not_a_value.is_match(value) {
            continue;
        }
        // A PIN is short and all digits; anything else follows the general rule.
        if (keyword == b"pin" || keyword == b"passcode") && r.pin.is_match(value) {
            return true;
        }
        if quoted {
            if value.len() >= 6 && !r.placeholder.is_match(value) {
                return true;
            }
            continue;
        }
        let code = |v: &[u8]| r.call.is_match(v) || r.member_access.is_match(v);
        if PASSWORD_WORDS.contains(&keyword.as_slice()) {
            if value.len() >= 6 && !r.placeholder.is_match(value) && !code(value) {
                return true;
            }
            continue;
        }
        if value.len() >= 8
            && r.digit.is_match(value)
            && !r.placeholder.is_match(value)
            && !code(value)
            && !r.code_name.is_match(value)
        {
            return true;
        }
    }
    false
}

/// `jwtShaped(s)`: the first three dotted segments at least 11, 8 and 8 units long.
fn jwt_shaped(r: &Rules, s: &[u8]) -> bool {
    r.jwt.find_iter(s).any(|m| {
        let mut parts = m.as_bytes().split(|&b| b == b'.');
        let (a, b, c) = (parts.next(), parts.next(), parts.next());
        a.is_some_and(|a| a.len() >= 11)
            && b.is_some_and(|b| b.len() >= 8)
            && c.is_some_and(|c| c.len() >= 8)
    })
}

/// `keyShaped(s)`: a digit anywhere, and a letter after the first `-` or `_`.
fn key_shaped(r: &Rules, s: &[u8]) -> bool {
    r.key_shape.find_iter(s).any(|m| {
        let t = m.as_bytes();
        let after = t
            .iter()
            .position(|&b| b == b'-' || b == b'_')
            .map_or(t, |i| t.get(i + 1..).unwrap_or_default());
        r.digit.is_match(t) && r.letter.is_match(after)
    })
}

/// `ibanValid(raw)`: 15 to 34 characters once the white space is out, and the ISO 7064 check.
fn iban_valid(raw: &[u8]) -> bool {
    let s: Vec<u8> = raw
        .iter()
        .filter(|&&b| !matches!(b, b'\t' | b'\n' | SPACE | 0x0C | b'\r' | b' '))
        .map(u8::to_ascii_uppercase)
        .collect();
    if s.len() < 15 || s.len() > 34 {
        return false;
    }
    let (head, tail) = s.split_at(4);
    let mut rem: u32 = 0;
    for &ch in tail.iter().chain(head) {
        let code = if ch.is_ascii_uppercase() {
            u32::from(ch - b'A') + 10
        } else {
            // Only digits are left: the match is [A-Z0-9] and spaces.
            u32::from(ch.wrapping_sub(b'0'))
        };
        for d in if code >= 10 {
            vec![code / 10, code % 10]
        } else {
            vec![code]
        } {
            rem = (rem * 10 + d) % 97;
        }
    }
    rem == 1
}

/// `luhn(digits)`.
fn luhn(digits: &[u8]) -> bool {
    let mut sum = 0u32;
    let mut dbl = false;
    for &c in digits.iter().rev() {
        let mut d = u32::from(c - b'0');
        if dbl {
            d *= 2;
            if d > 9 {
                d -= 9;
            }
        }
        sum += d;
        dbl = !dbl;
    }
    sum.is_multiple_of(10)
}

/// `healthShaped(s)`: a diagnosis word with a named condition within 60 units either side, or a
/// personal health-record phrase.
fn health_shaped(r: &Rules, s: &[u8]) -> bool {
    for m in r.diagnosis.find_iter(s) {
        let from = m.start().saturating_sub(60);
        let to = (m.end() + 60).min(s.len());
        if s.get(from..to).is_some_and(|w| r.condition.is_match(w)) {
            return true;
        }
    }
    r.record.is_match(s)
}

/// `preRule(text, { health })`: the first rule `text` trips (`secret`, `iban`, `card`, `health`),
/// or `None`. `health: false` skips the health rule (#1592).
///
/// `text` is the string Node is given (`String(text || '')` is the caller's).
pub fn pre_rule(text: &str, health: bool) -> Option<&'static str> {
    let Some(r) = rules() else {
        // The patterns are constants; were one ever refused, nothing may go to the cloud unchecked.
        return Some("secret");
    };
    let mut s = units(text);
    s.truncate(SCAN_LIMIT);
    // withoutNoeviaData: both patterns match ASCII only, so the replacements are exact here.
    let s = r.chat_context.replace_all(&s, &b"chat/"[..]);
    let s = r.data_uri.replace_all(&s, &b"[image]"[..]);
    if r.private_key.is_match(&s)
        || r.bearer.is_match(&s)
        || secret_assignment(r, &s)
        || jwt_shaped(r, &s)
        || key_shaped(r, &s)
    {
        return Some("secret");
    }
    if r.iban.find_iter(&s).any(|m| iban_valid(m.as_bytes())) {
        return Some("iban");
    }
    if r.card.find_iter(&s).any(|m| {
        let digits: Vec<u8> = m
            .as_bytes()
            .iter()
            .copied()
            .filter(u8::is_ascii_digit)
            .collect();
        (13..=19).contains(&digits.len()) && luhn(&digits)
    }) {
        return Some("card");
    }
    if health && health_shaped(r, &s) {
        return Some("health");
    }
    None
}

/// chat.cjs `holdForCloud`'s verdict on one tool result (#779 F2): `diary` for a Diary tool
/// (`/^diary[_-]/i` on its name), else [`pre_rule`] of the result, the health rule only when the
/// tool is not positively public (`isPublicTool(name)`, [`crate::tools_mode::is_public_tool_name`]).
/// [`CloudHold`] asks it once per result of a cloud route until one is held.
pub fn tool_result_flag(name: &str, text: &str, public: bool) -> Option<&'static str> {
    let n = name.as_bytes();
    let diary = n.len() >= 6
        && n.get(..5).is_some_and(|p| p.eq_ignore_ascii_case(b"diary"))
        && matches!(n.get(5), Some(b'_' | b'-'));
    if diary {
        return Some("diary");
    }
    pre_rule(text, !public)
}

/// chat.cjs `cloudHold` and `holdForCloud(name, text)` for one turn: on a cloud route chosen by
/// routing modes (`routeTarget?.route === 'cloud'`), the first tool result a rule flags is held
/// and ends the exchange; nothing after it is checked or held again.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CloudHold {
    /// The held result's flag (`cloudHold`).
    pub held: Option<&'static str>,
}

/// A result just held: what Node logs (`routingModes.log(entry)`, its failure ignored) and the
/// console line, in that order.
#[derive(Debug, Clone, PartialEq)]
pub struct Held {
    pub flag: &'static str,
    pub log: JValue,
    pub line: String,
}

impl CloudHold {
    /// `holdForCloud(name, text)`: `Some` when this result is held. `cloud_route`: the turn's route
    /// target is `cloud`; `public`: `isPublicTool(name)`.
    pub fn hold(
        &mut self,
        cloud_route: bool,
        name: &str,
        text: &str,
        public: bool,
    ) -> Option<Held> {
        if self.held.is_some() || !cloud_route {
            return None;
        }
        let flag = tool_result_flag(name, text, public)?;
        self.held = Some(flag);
        Some(Held {
            flag,
            log: JValue::Obj(vec![
                ("mode".into(), JValue::from("tool-result")),
                ("route".into(), JValue::from("held")),
                ("reason".into(), JValue::from("sensitive-rule")),
                ("flag".into(), JValue::from(flag)),
            ]),
            line: format!("[routing] tool result held from cloud flag={flag}"),
        })
    }
}
