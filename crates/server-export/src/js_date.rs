//! `new Date(value).toISOString()` as Node (V8) computes it, for the single-chat download's
//! `_Last updated …_` line (core chat-export.cjs `chatMarkdown`, #1471).
//!
//! - A number is TimeClip'd: not finite or beyond ±8.64e15 ms is an invalid Date, else truncated.
//! - `true` is 1. An array is its `join(',')` string; an object is `"[object Object]"` (an own
//!   `toString`, never callable in JSON, throws TypeError).
//! - A string is `Date.parse`: a port of V8's src/date/dateparser (the ES5 ISO form, then the legacy
//!   Safari-compatible parser: words, month names, AM/PM, zone names and offsets, parentheses), then
//!   MakeDay / MakeTime / MakeDate and, when no zone was given, the local time of this process (as
//!   Node's, whose zone is the same container's), then TimeClip.
//! - The ISO text uses V8's extended years: `±YYYYYY` outside 0000–9999.
//!
//! An invalid Date is Node's RangeError (`toISOString`), here `Err(Invalid)`.
use chrono::{Offset, TimeZone};
use js_json::JValue;

/// Why there is no ISO text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// Node's RangeError: Invalid time value.
    Invalid,
    /// Node's TypeError (an object whose own `toString` is not callable).
    Type,
}

const MAX_TIME_MS: f64 = 8.64e15;
const NONE: i64 = i32::MAX as i64; // V8 kNone (kMaxInt)
const MAX_SIGNIFICANT_DIGITS: usize = 9;
const SMI_MAX: i64 = i32::MAX as i64;

fn time_clip(t: f64) -> Option<f64> {
    if !t.is_finite() || t.abs() > MAX_TIME_MS {
        return None;
    }
    Some(t.trunc() + 0.0)
}

// ── Input reader and tokenizer (V8 dateparser.h InputReader, DateStringTokenizer) ─────────────

/// One UTF-16-like unit: the character, or a lone surrogate's own value.
fn unit(c: char) -> u32 {
    js_json::wtf16::lone_unit(c).map_or(u32::from(c), u32::from)
}

/// V8 `IsWhiteSpace`: ES WhiteSpace (not the line terminators).
fn is_white_space(c: u32) -> bool {
    (0x2000..=0x200a).contains(&c)
        || matches!(
            c,
            0x09 | 0x0b | 0x0c | 0x20 | 0xa0 | 0x1680 | 0x202f | 0x205f | 0x3000 | 0xfeff
        )
}

/// V8 `IsWhiteSpaceOrLineTerminator`.
fn is_white_space_or_lt(c: u32) -> bool {
    is_white_space(c) || matches!(c, 0x0a | 0x0d | 0x2028 | 0x2029)
}

struct Reader {
    units: Vec<u32>,
    pos: usize,
}

impl Reader {
    /// The current unit; 0 past the end (V8's `ch_`).
    fn ch(&self) -> u32 {
        self.units.get(self.pos).copied().unwrap_or(0)
    }
    fn is_end(&self) -> bool {
        self.pos >= self.units.len()
    }
    fn next(&mut self) {
        if self.pos < self.units.len() {
            self.pos += 1;
        }
    }
    fn is_digit(&self) -> bool {
        (u32::from(b'0')..=u32::from(b'9')).contains(&self.ch())
    }
    fn skip(&mut self, c: u8) -> bool {
        if self.ch() == u32::from(c) {
            self.next();
            true
        } else {
            false
        }
    }
    fn read_unsigned_numeral(&mut self) -> i64 {
        let mut n: i64 = 0;
        let mut i = 0usize;
        while self.ch() == u32::from(b'0') {
            self.next();
        }
        while self.is_digit() {
            if i < MAX_SIGNIFICANT_DIGITS {
                n = n * 10 + i64::from(self.ch() - u32::from(b'0'));
            }
            i += 1;
            self.next();
        }
        n
    }
    fn is_alpha_or_above(&self) -> bool {
        self.ch() >= u32::from(b'A')
    }
    fn read_word(&mut self) -> ([u32; 3], usize) {
        let mut prefix = [0u32; 3];
        let mut len = 0usize;
        while self.is_alpha_or_above() && !is_white_space(self.ch()) {
            if let Some(slot) = prefix.get_mut(len) {
                *slot = self.ch() | 0x20;
            }
            len += 1;
            self.next();
        }
        (prefix, len)
    }
    fn skip_white_space(&mut self) -> bool {
        if is_white_space_or_lt(self.ch()) {
            self.next();
            true
        } else {
            false
        }
    }
    fn skip_parentheses(&mut self) -> bool {
        if self.ch() != u32::from(b'(') {
            return false;
        }
        let mut balance = 0i64;
        loop {
            if self.ch() == u32::from(b')') {
                balance -= 1;
            } else if self.ch() == u32::from(b'(') {
                balance += 1;
            }
            self.next();
            if !(balance > 0 && self.ch() != 0) {
                break;
            }
        }
        true
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kw {
    MonthName,
    AmPm,
    TimeZoneName,
    TimeSeparator,
    Invalid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Token {
    Invalid,
    Unknown,
    Number { value: i64, length: usize },
    Symbol(u8),
    WhiteSpace,
    Keyword { kind: Kw, value: i64, length: usize },
    End,
}

impl Token {
    fn is_number(self) -> bool {
        matches!(self, Token::Number { .. })
    }
    fn number(self) -> i64 {
        match self {
            Token::Number { value, .. } => value,
            _ => 0,
        }
    }
    fn is_fixed_length_number(self, n: usize) -> bool {
        matches!(self, Token::Number { length, .. } if length == n)
    }
    fn is_symbol(self, c: u8) -> bool {
        self == Token::Symbol(c)
    }
    fn is_ascii_sign(self) -> bool {
        self.is_symbol(b'+') || self.is_symbol(b'-')
    }
    fn ascii_sign(self) -> i64 {
        if self.is_symbol(b'-') {
            -1
        } else {
            1
        }
    }
    fn is_keyword_type(self, k: Kw) -> bool {
        matches!(self, Token::Keyword { kind, .. } if kind == k)
    }
    fn is_keyword_z(self) -> bool {
        matches!(
            self,
            Token::Keyword {
                kind: Kw::TimeZoneName,
                length: 1,
                value: 0
            }
        )
    }
    fn is_end(self) -> bool {
        self == Token::End
    }
}

/// V8 KeywordTable: three-letter prefixes; a longer word matches only a month name.
const KEYWORDS: &[([u8; 3], Kw, i64)] = &[
    (*b"jan", Kw::MonthName, 1),
    (*b"feb", Kw::MonthName, 2),
    (*b"mar", Kw::MonthName, 3),
    (*b"apr", Kw::MonthName, 4),
    (*b"may", Kw::MonthName, 5),
    (*b"jun", Kw::MonthName, 6),
    (*b"jul", Kw::MonthName, 7),
    (*b"aug", Kw::MonthName, 8),
    (*b"sep", Kw::MonthName, 9),
    (*b"oct", Kw::MonthName, 10),
    (*b"nov", Kw::MonthName, 11),
    (*b"dec", Kw::MonthName, 12),
    (*b"am\0", Kw::AmPm, 0),
    (*b"pm\0", Kw::AmPm, 12),
    (*b"ut\0", Kw::TimeZoneName, 0),
    (*b"utc", Kw::TimeZoneName, 0),
    (*b"z\0\0", Kw::TimeZoneName, 0),
    (*b"gmt", Kw::TimeZoneName, 0),
    (*b"cdt", Kw::TimeZoneName, -5),
    (*b"cst", Kw::TimeZoneName, -6),
    (*b"edt", Kw::TimeZoneName, -4),
    (*b"est", Kw::TimeZoneName, -5),
    (*b"mdt", Kw::TimeZoneName, -6),
    (*b"mst", Kw::TimeZoneName, -7),
    (*b"pdt", Kw::TimeZoneName, -7),
    (*b"pst", Kw::TimeZoneName, -8),
    (*b"t\0\0", Kw::TimeSeparator, 0),
];

fn keyword(prefix: [u32; 3], len: usize) -> (Kw, i64) {
    for (word, kind, value) in KEYWORDS {
        if word
            .iter()
            .zip(prefix.iter())
            .all(|(a, b)| u32::from(*a) == *b)
            && (len <= 3 || *kind == Kw::MonthName)
        {
            return (*kind, *value);
        }
    }
    (Kw::Invalid, 0)
}

struct Scanner {
    reader: Reader,
    next: Token,
}

impl Scanner {
    fn new(s: &str) -> Self {
        let mut reader = Reader {
            units: s.chars().map(unit).collect(),
            pos: 0,
        };
        let next = scan(&mut reader);
        Scanner { reader, next }
    }
    fn next(&mut self) -> Token {
        let t = self.next;
        self.next = scan(&mut self.reader);
        t
    }
    fn peek(&self) -> Token {
        self.next
    }
    fn skip_symbol(&mut self, c: u8) -> bool {
        if self.next.is_symbol(c) {
            self.next();
            true
        } else {
            false
        }
    }
}

fn scan(r: &mut Reader) -> Token {
    let pre = r.pos;
    if r.is_end() {
        return Token::End;
    }
    if r.is_digit() {
        let value = r.read_unsigned_numeral();
        return Token::Number {
            value,
            length: r.pos - pre,
        };
    }
    for &c in b":-+.)" {
        if r.skip(c) {
            return Token::Symbol(c);
        }
    }
    if r.is_alpha_or_above() && !is_white_space(r.ch()) {
        let (prefix, length) = r.read_word();
        let (kind, value) = keyword(prefix, length);
        return Token::Keyword {
            kind,
            value,
            length,
        };
    }
    if r.skip_white_space() {
        return Token::WhiteSpace;
    }
    if r.skip_parentheses() {
        return Token::Unknown;
    }
    r.next();
    Token::Unknown
}

// ── Composers (V8 dateparser.cc) ─────────────────────────────────────────────────────────────

fn between(x: i64, lo: i64, hi: i64) -> bool {
    lo <= x && x <= hi
}
fn is_month(x: i64) -> bool {
    between(x, 1, 12)
}
fn is_day(x: i64) -> bool {
    between(x, 1, 31)
}
fn is_minute(x: i64) -> bool {
    between(x, 0, 59)
}
fn is_hour(x: i64) -> bool {
    between(x, 0, 23)
}
fn is_second(x: i64) -> bool {
    between(x, 0, 59)
}
fn is_hour12(x: i64) -> bool {
    between(x, 0, 12)
}
fn is_millisecond(x: i64) -> bool {
    between(x, 0, 999)
}

#[derive(Default)]
struct Day {
    comp: [i64; 3],
    index: usize,
    named_month: Option<i64>,
    iso: bool,
}

impl Day {
    fn is_empty(&self) -> bool {
        self.index == 0
    }
    fn add(&mut self, n: i64) -> bool {
        match self.comp.get_mut(self.index) {
            Some(slot) => {
                *slot = n;
                self.index += 1;
                true
            }
            None => false,
        }
    }
    /// (year, 0-based month, day).
    fn write(&mut self) -> Option<(i64, i64, i64)> {
        if self.index < 1 {
            return None;
        }
        while self.index < 3 {
            self.add(1);
        }
        let [c0, c1, c2] = self.comp;
        let (mut year, month, day);
        match self.named_month {
            None => {
                if self.iso || !is_day(c0) {
                    (year, month, day) = (c0, c1, c2);
                } else {
                    (month, day, year) = (c0, c1, c2);
                }
            }
            Some(m) => {
                month = m;
                // The composer always holds three components here, so V8's `index_ == 1` (MD/DM)
                // never applies: "Jan 5" is day 5 of year 1 (2001).
                if !is_day(c0) {
                    (year, day) = (c0, c1);
                } else {
                    (day, year) = (c0, c1);
                }
            }
        }
        if !self.iso {
            if between(year, 0, 49) {
                year += 2000;
            } else if between(year, 50, 99) {
                year += 1900;
            }
        }
        if year.abs() > SMI_MAX || !is_month(month) || !is_day(day) {
            return None;
        }
        Some((year, month - 1, day))
    }
}

struct Time {
    comp: [i64; 4],
    index: usize,
    hour_offset: Option<i64>,
}

impl Time {
    fn new() -> Self {
        Time {
            comp: [0; 4],
            index: 0,
            hour_offset: None,
        }
    }
    fn is_empty(&self) -> bool {
        self.index == 0
    }
    fn is_expecting(&self, n: i64) -> bool {
        (self.index == 1 && is_minute(n))
            || (self.index == 2 && is_second(n))
            || (self.index == 3 && is_millisecond(n))
    }
    fn add(&mut self, n: i64) -> bool {
        match self.comp.get_mut(self.index) {
            Some(slot) => {
                *slot = n;
                self.index += 1;
                true
            }
            None => false,
        }
    }
    fn add_final(&mut self, n: i64) -> bool {
        if !self.add(n) {
            return false;
        }
        while self.index < 4 {
            self.add(0);
        }
        true
    }
    fn write(&mut self) -> Option<[i64; 4]> {
        while self.index < 4 {
            self.add(0);
        }
        let [mut hour, minute, second, ms] = self.comp;
        if let Some(offset) = self.hour_offset {
            if !is_hour12(hour) {
                return None;
            }
            hour %= 12;
            hour += offset;
        }
        if !(is_hour(hour) && is_minute(minute) && is_second(second) && is_millisecond(ms))
            && (hour != 24 || minute != 0 || second != 0 || ms != 0)
        {
            return None;
        }
        Some([hour, minute, second, ms])
    }
}

struct Zone {
    sign: i64,
    hour: i64,
    minute: i64,
}

impl Zone {
    fn new() -> Self {
        Zone {
            sign: NONE,
            hour: NONE,
            minute: NONE,
        }
    }
    fn set(&mut self, offset_in_hours: i64) {
        self.sign = if offset_in_hours < 0 { -1 } else { 1 };
        self.hour = offset_in_hours * self.sign;
        self.minute = 0;
    }
    fn set_sign(&mut self, sign: i64) {
        self.sign = if sign < 0 { -1 } else { 1 };
    }
    fn is_expecting(&self, n: i64) -> bool {
        self.hour != NONE && self.minute == NONE && is_minute(n)
    }
    fn is_utc(&self) -> bool {
        self.hour == 0 && self.minute == 0
    }
    fn is_empty(&self) -> bool {
        self.hour == NONE
    }
    /// The UTC offset in seconds, `Some(None)` for local time, `None` when invalid.
    fn write(&mut self) -> Option<Option<i64>> {
        if self.sign == NONE {
            return Some(None);
        }
        if self.hour == NONE {
            self.hour = 0;
        }
        if self.minute == NONE {
            self.minute = 0;
        }
        // V8 computes in unsigned 32-bit arithmetic and refuses past Smi::kMaxValue.
        let total = (self.hour as u32)
            .wrapping_mul(3600)
            .wrapping_add((self.minute as u32).wrapping_mul(60));
        if i64::from(total) > SMI_MAX {
            return None;
        }
        let total = i64::from(total);
        Some(Some(if self.sign < 0 { -total } else { total }))
    }
}

fn read_milliseconds(token: Token) -> i64 {
    let (mut number, mut length) = match token {
        Token::Number { value, length } => (value, length),
        _ => (0, 0),
    };
    if length < 3 {
        if length == 1 {
            number *= 100;
        } else if length == 2 {
            number *= 10;
        }
    } else if length > 3 {
        if length > MAX_SIGNIFICANT_DIGITS {
            length = MAX_SIGNIFICANT_DIGITS;
        }
        let mut factor = 1;
        loop {
            factor *= 10;
            length -= 1;
            if length <= 3 {
                break;
            }
        }
        number /= factor;
    }
    number
}

/// V8 `ParseES5DateTime`: the next unhandled token (End when the whole string was ES5).
fn parse_es5(s: &mut Scanner, day: &mut Day, time: &mut Time, tz: &mut Zone) -> Token {
    if s.peek().is_ascii_sign() {
        let sign_token = s.next();
        if !s.peek().is_fixed_length_number(6) {
            return sign_token;
        }
        let sign = sign_token.ascii_sign();
        let year = s.next().number();
        if sign < 0 && year == 0 {
            return sign_token;
        }
        day.add(sign * year);
    } else if s.peek().is_fixed_length_number(4) {
        let year = s.next().number();
        day.add(year);
    } else {
        return s.next();
    }
    if s.skip_symbol(b'-') {
        if !s.peek().is_fixed_length_number(2) || !is_month(s.peek().number()) {
            return s.next();
        }
        let month = s.next().number();
        day.add(month);
        if s.skip_symbol(b'-') {
            if !s.peek().is_fixed_length_number(2) || !is_day(s.peek().number()) {
                return s.next();
            }
            let d = s.next().number();
            day.add(d);
        }
    }
    if !s.peek().is_keyword_type(Kw::TimeSeparator) {
        if !s.peek().is_end() {
            return s.next();
        }
    } else {
        s.next();
        if !s.peek().is_fixed_length_number(2) || !between(s.peek().number(), 0, 24) {
            return Token::Invalid;
        }
        let hour_is_24 = s.peek().number() == 24;
        let hour = s.next().number();
        time.add(hour);
        if !s.skip_symbol(b':') {
            return Token::Invalid;
        }
        if !s.peek().is_fixed_length_number(2)
            || !is_minute(s.peek().number())
            || (hour_is_24 && s.peek().number() > 0)
        {
            return Token::Invalid;
        }
        let minute = s.next().number();
        time.add(minute);
        if s.skip_symbol(b':') {
            if !s.peek().is_fixed_length_number(2)
                || !is_second(s.peek().number())
                || (hour_is_24 && s.peek().number() > 0)
            {
                return Token::Invalid;
            }
            let second = s.next().number();
            time.add(second);
            if s.skip_symbol(b'.') {
                if !s.peek().is_number() || (hour_is_24 && s.peek().number() > 0) {
                    return Token::Invalid;
                }
                let ms = read_milliseconds(s.next());
                time.add(ms);
            }
        }
        if s.peek().is_keyword_z() {
            s.next();
            tz.set(0);
        } else if s.peek().is_symbol(b'+') || s.peek().is_symbol(b'-') {
            let sign = if s.next().is_symbol(b'+') { 1 } else { -1 };
            tz.set_sign(sign);
            if s.peek().is_fixed_length_number(4) {
                let hourmin = s.next().number();
                let (hour, min) = (hourmin / 100, hourmin % 100);
                if !is_hour(hour) || !is_minute(min) {
                    return Token::Invalid;
                }
                tz.hour = hour;
                tz.minute = min;
            } else {
                if !s.peek().is_fixed_length_number(2) || !is_hour(s.peek().number()) {
                    return Token::Invalid;
                }
                tz.hour = s.next().number();
                if !s.skip_symbol(b':') {
                    return Token::Invalid;
                }
                if !s.peek().is_fixed_length_number(2) || !is_minute(s.peek().number()) {
                    return Token::Invalid;
                }
                tz.minute = s.next().number();
            }
        }
        if !s.peek().is_end() {
            return Token::Invalid;
        }
    }
    if tz.is_empty() && time.is_empty() {
        tz.set(0);
    }
    day.iso = true;
    Token::End
}

/// The parsed fields: (year, month0, day), (h, m, s, ms), UTC offset in seconds or local.
type Fields = ((i64, i64, i64), [i64; 4], Option<i64>);

/// V8 `DateParser::Parse`.
fn parse_fields(text: &str) -> Option<Fields> {
    let mut s = Scanner::new(text);
    let mut day = Day::default();
    let mut time = Time::new();
    let mut tz = Zone::new();
    let first = parse_es5(&mut s, &mut day, &mut time, &mut tz);
    if first == Token::Invalid {
        return None;
    }
    let mut has_read_number = !day.is_empty();
    let mut token = first;
    while !token.is_end() {
        match token {
            Token::Number { value: n, .. } => {
                has_read_number = true;
                if s.skip_symbol(b':') {
                    if s.skip_symbol(b':') {
                        if !time.is_empty() {
                            return None;
                        }
                        time.add(n);
                        time.add(0);
                    } else {
                        if !time.add(n) {
                            return None;
                        }
                        if s.peek().is_symbol(b'.') {
                            s.next();
                        }
                    }
                } else if s.peek().is_symbol(b'.') && time.is_expecting(n) {
                    s.next();
                    time.add(n);
                    if !s.peek().is_number() {
                        return None;
                    }
                    let ms = read_milliseconds(s.next());
                    if ms < 0 {
                        return None;
                    }
                    time.add_final(ms);
                } else if tz.is_expecting(n) {
                    tz.minute = n;
                } else if time.is_expecting(n) {
                    time.add_final(n);
                    let peek = s.peek();
                    if !peek.is_end()
                        && peek != Token::WhiteSpace
                        && !peek.is_keyword_z()
                        && !peek.is_ascii_sign()
                    {
                        return None;
                    }
                } else {
                    if !day.add(n) {
                        return None;
                    }
                    s.skip_symbol(b'-');
                }
            }
            Token::Keyword { kind, value, .. } => {
                if kind == Kw::AmPm && !time.is_empty() {
                    time.hour_offset = Some(value);
                } else if kind == Kw::MonthName {
                    day.named_month = Some(value);
                    s.skip_symbol(b'-');
                } else if kind == Kw::TimeZoneName && has_read_number {
                    tz.set(value);
                } else {
                    if has_read_number {
                        return None;
                    }
                    if s.peek().is_number() {
                        return None;
                    }
                }
            }
            t if t.is_ascii_sign() && (tz.is_utc() || !time.is_empty()) => {
                tz.set_sign(t.ascii_sign());
                let (mut n, mut length) = (0, 0);
                if s.peek().is_number() {
                    if let Token::Number { value, length: l } = s.next() {
                        n = value;
                        length = l;
                    }
                }
                has_read_number = true;
                if s.peek().is_symbol(b':') {
                    tz.hour = n;
                    tz.minute = NONE;
                } else if length == 2 || length == 1 {
                    tz.hour = n;
                    tz.minute = 0;
                } else if length == 4 || length == 3 {
                    tz.hour = n / 100;
                    tz.minute = n % 100;
                } else {
                    return None;
                }
            }
            t if (t.is_ascii_sign() || t.is_symbol(b')')) && has_read_number => return None,
            _ => {}
        }
        token = s.next();
    }
    let d = day.write()?;
    let t = time.write()?;
    let z = tz.write()?;
    Some((d, t, z))
}

/// ES MakeDay, as V8 computes it (years ±1,000,000).
fn make_day(year: i64, month: i64, date: i64) -> Option<f64> {
    if !(-1_000_000..=1_000_000).contains(&year) || !(-10_000_000..=10_000_000).contains(&month) {
        return None;
    }
    let mut y = year + month / 12;
    let mut m = month % 12;
    if m < 0 {
        m += 12;
        y -= 1;
    }
    const YEAR_DELTA: i64 = 399_999;
    let days_before = |y: i64| {
        let v = y + YEAR_DELTA;
        365 * v + v / 4 - v / 100 + v / 400
    };
    let base = days_before(1970);
    let mut day_from_year = days_before(y) - base;
    let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    let table: [i64; 12] = if leap {
        [0, 31, 60, 91, 121, 152, 182, 213, 244, 274, 305, 335]
    } else {
        [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334]
    };
    day_from_year += table.get(usize::try_from(m).ok()?).copied()?;
    Some((day_from_year - 1) as f64 + date as f64)
}

/// The local wall-clock milliseconds `local` as UTC milliseconds, in this process's zone. A time in
/// a gap reads with the offset before it and an ambiguous one takes the earlier instant, as V8's
/// DateCache::ToUTC does.
fn local_to_utc(local: f64) -> f64 {
    let ms = local as i64;
    let guess = offset_at(ms);
    // Two passes, as V8 does: the offset at the guessed instant, then at the corrected one.
    let first = ms - guess * 1000;
    let second = offset_at(first);
    // Prefer the earlier instant when both offsets describe the same wall time.
    let mut best = None;
    for c in [ms - second * 1000, first] {
        if c + offset_at(c) * 1000 == ms {
            best = Some(best.map_or(c, |b: i64| b.min(c)));
        }
    }
    best.unwrap_or(first) as f64
}

/// This process's UTC offset in seconds at the instant `ms`. chrono reaches about ±262,000 years,
/// Date ±275,760: beyond it the instant moves by whole 400-year Gregorian cycles (the calendar and
/// its weekdays repeat), so the zone's first offset or its standing rule reads as it would there.
fn offset_at(ms: i64) -> i64 {
    const CYCLE_MS: i64 = 146_097 * 86_400_000;
    let mut t = ms;
    while t > 8_000_000_000_000_000 {
        t -= CYCLE_MS;
    }
    while t < -8_000_000_000_000_000 {
        t += CYCLE_MS;
    }
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(t).map_or(0, |d| {
        i64::from(
            chrono::Local
                .offset_from_utc_datetime(&d.naive_utc())
                .fix()
                .local_minus_utc(),
        )
    })
}

/// `Date.parse(text)`: the time value, `None` for NaN.
pub fn parse(text: &str) -> Option<f64> {
    let ((year, month, day), [h, mi, s, ms], offset) = parse_fields(text)?;
    let day = make_day(year, month, day)?;
    let time = (h * 3_600_000 + mi * 60_000 + s * 1000 + ms) as f64;
    let mut date = day * 86_400_000.0 + time;
    if !date.is_finite() {
        return None;
    }
    match offset {
        None => {
            if date.abs() > MAX_TIME_MS + 864_000_000.0 {
                return None;
            }
            date = local_to_utc(date);
        }
        Some(seconds) => {
            date -= seconds as f64 * 1000.0;
            if date.abs() > MAX_TIME_MS {
                return None;
            }
        }
    }
    time_clip(date)
}

/// `new Date(value)`'s time value for a truthy JSON value.
pub fn time_value(v: &JValue) -> Result<f64, Error> {
    let clipped = match v {
        JValue::Num(n) => time_clip(*n),
        JValue::Bool(b) => time_clip(if *b { 1.0 } else { 0.0 }),
        JValue::Null => time_clip(0.0),
        JValue::Str(s) => parse(s),
        JValue::Arr(_) => {
            let text = js_json::to_js_string(v).map_err(|_| Error::Type)?;
            parse(&text)
        }
        JValue::Obj(items) => {
            if items.iter().any(|(k, _)| k == "toString") {
                return Err(Error::Type);
            }
            None // "[object Object]"
        }
        JValue::Undefined => None,
    };
    clipped.ok_or(Error::Invalid)
}

/// `Date.prototype.toISOString` for a valid time value.
pub fn iso(t: f64) -> String {
    let ms = t as i64;
    let days = ms.div_euclid(86_400_000);
    let in_day = ms.rem_euclid(86_400_000);
    // Civil from days (Howard Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    let year = if (0..=9999).contains(&y) {
        format!("{y:04}")
    } else if y < 0 {
        format!("-{:06}", -y)
    } else {
        format!("+{y:06}")
    };
    format!(
        "{year}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z",
        in_day / 3_600_000,
        in_day / 60_000 % 60,
        in_day / 1000 % 60,
        in_day % 1000
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc(text: &str) -> Option<String> {
        parse(text).map(iso)
    }

    #[test]
    fn numbers_and_extended_years() {
        assert_eq!(iso(253_402_300_800_000.0), "+010000-01-01T00:00:00.000Z");
        assert_eq!(iso(8.64e15), "+275760-09-13T00:00:00.000Z");
        assert_eq!(iso(-8.64e15), "-271821-04-20T00:00:00.000Z");
        assert_eq!(iso(-62_198_755_200_000.0), "-000001-01-01T00:00:00.000Z");
        assert_eq!(iso(-1.0), "1969-12-31T23:59:59.999Z");
        assert_eq!(time_value(&JValue::Num(8.64e15 + 1.0)), Err(Error::Invalid));
        assert_eq!(time_value(&JValue::Num(1.9)), Ok(1.0));
        assert_eq!(time_value(&JValue::Num(-1.9)), Ok(-1.0));
    }

    #[test]
    fn iso_and_utc_legacy_forms() {
        // Date-only ISO forms are UTC; these all carry their zone, so the host zone does not matter.
        assert_eq!(
            utc("2026-01-01").as_deref(),
            Some("2026-01-01T00:00:00.000Z")
        );
        assert_eq!(utc("2026").as_deref(), Some("2026-01-01T00:00:00.000Z"));
        assert_eq!(
            utc("+010000-01-01").as_deref(),
            Some("+010000-01-01T00:00:00.000Z")
        );
        assert_eq!(
            utc("2026-02-31").as_deref(),
            Some("2026-03-03T00:00:00.000Z")
        );
        assert_eq!(
            utc("2026-01-01T24:00Z").as_deref(),
            Some("2026-01-02T00:00:00.000Z")
        );
        assert_eq!(
            utc("2026-01-01T10:00:00.1234+0130").as_deref(),
            Some("2026-01-01T08:30:00.123Z")
        );
        assert_eq!(
            utc("Thu, 01 Jan 2026 10:00:00 GMT").as_deref(),
            Some("2026-01-01T10:00:00.000Z")
        );
        assert_eq!(
            utc("Jan 1 2026 10:00 PM UTC").as_deref(),
            Some("2026-01-01T22:00:00.000Z")
        );
        assert_eq!(
            utc("1 January 2026 10:00 EST").as_deref(),
            Some("2026-01-01T15:00:00.000Z")
        );
        assert_eq!(utc("2026-01-01T10:00:00Zx"), None);
        assert_eq!(utc("not a date"), None);
        assert_eq!(utc(""), None);
    }
}
