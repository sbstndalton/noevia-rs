//! Shipped CPython 3.12 email._parseaddr date grammar, with supplied handling
//! for naive/local dates. Adapted under the PSF license (LICENSE-PYTHON-3.12.txt).
//! Changes: typed, bounded Rust arithmetic and no host timezone/clock reads.
use chrono::{NaiveDate, NaiveDateTime};
use std::collections::VecDeque;

use super::{py_space, unicode};
mod tables;

#[derive(Clone, Copy)]
enum Integer {
    Invalid,
    Value(i64),
    Huge,
}

// Python int accepts Unicode Nd and underscores between digits, but limits
// decimal conversions to 4300 digits. Distinguish syntax errors from integers
// too large for downstream datetime/timedelta, which must not become local TZ.
fn integer(text: &str) -> Integer {
    let mut chars = text.chars().peekable();
    let negative = chars.peek() == Some(&'-');
    if matches!(chars.peek(), Some('+' | '-')) {
        chars.next();
    }
    let mut value = Some(0i128);
    let mut digits = 0usize;
    let mut previous_digit = false;
    while let Some(c) = chars.next() {
        if let Some(digit) = unicode::decimal(c) {
            digits += 1;
            previous_digit = true;
            value = value.and_then(|n| n.checked_mul(10)?.checked_add(i128::from(digit)));
        } else if c == '_'
            && previous_digit
            && chars.peek().is_some_and(|c| unicode::decimal(*c).is_some())
        {
            previous_digit = false;
        } else {
            return Integer::Invalid;
        }
    }
    if digits == 0 || digits > 4300 || !previous_digit {
        return Integer::Invalid;
    }
    let value = value.and_then(|n| if negative { n.checked_neg() } else { Some(n) });
    match value.and_then(|n| i64::try_from(n).ok()) {
        Some(n) => Integer::Value(n),
        None => Integer::Huge,
    }
}

fn number(text: &str) -> Option<i64> {
    match integer(text) {
        Integer::Value(n) => Some(n),
        Integer::Invalid | Integer::Huge => None,
    }
}

fn upper_ascii(text: &str) -> Option<String> {
    let mut result = String::with_capacity(text.len());
    for c in text.chars() {
        if c.is_ascii() {
            result.push(c.to_ascii_uppercase());
        } else {
            let entry = tables::UPPER_ASCII
                .iter()
                .find(|(cp, _)| *cp == u32::from(c))?;
            result.push_str(entry.1);
        }
    }
    Some(result)
}

fn is_digit(c: char) -> bool {
    let cp = u32::from(c);
    tables::DIGITS
        .iter()
        .any(|(start, end)| *start <= cp && cp <= *end)
}

pub(super) fn timestamp(
    text: &str,
    resolve_local: &mut impl FnMut(NaiveDateTime) -> Option<f64>,
) -> Option<f64> {
    let mut fields: VecDeque<String> = text
        .split(py_space)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect();
    let first = fields.front()?;
    if first.ends_with(',')
        || ["mon", "tue", "wed", "thu", "fri", "sat", "sun"]
            .contains(&first.to_ascii_lowercase().as_str())
    {
        fields.pop_front();
    } else if let Some(at) = first.rfind(',') {
        let tail = first.get(at + 1..)?.to_owned();
        *fields.front_mut()? = tail;
    }
    if fields.len() == 3 {
        let components: Vec<String> = fields.front()?.split('-').map(str::to_owned).collect();
        if components.len() == 3 {
            fields.pop_front();
            for component in components.into_iter().rev() {
                fields.push_front(component);
            }
        }
    }
    if fields.len() == 4 {
        let last = fields.back()?;
        let split = last
            .find('+')
            .or_else(|| last.find('-'))
            .filter(|at| *at > 0);
        if let Some(at) = split {
            let clock = last.get(..at)?.to_owned();
            let zone = last.get(at..)?.to_owned();
            fields.pop_back();
            fields.push_back(clock);
            fields.push_back(zone);
        } else {
            fields.push_back(String::new());
        }
    }
    if fields.len() < 5 {
        return None;
    }
    let mut fields = fields.into_iter();
    let mut day = fields.next()?;
    let mut month = fields.next()?.to_ascii_lowercase();
    let mut year = fields.next()?;
    let mut clock = fields.next()?;
    let mut zone = fields.next()?;
    if day.is_empty() || month.is_empty() || year.is_empty() {
        return None;
    }
    let names = [
        "jan",
        "feb",
        "mar",
        "apr",
        "may",
        "jun",
        "jul",
        "aug",
        "sep",
        "oct",
        "nov",
        "dec",
        "january",
        "february",
        "march",
        "april",
        "may",
        "june",
        "july",
        "august",
        "september",
        "october",
        "november",
        "december",
    ];
    if !names.contains(&month.as_str()) {
        let next_month = day.to_ascii_lowercase();
        day = month;
        month = next_month;
    }
    let month = u32::try_from(names.iter().position(|s| *s == month)? % 12 + 1).ok()?;
    if let Some(trimmed) = day.strip_suffix(',') {
        day = trimmed.to_owned();
    }
    if year.find(':').is_some_and(|at| at > 0) {
        std::mem::swap(&mut year, &mut clock);
    }
    if let Some(trimmed) = year.strip_suffix(',') {
        year = trimmed.to_owned();
    }
    if !is_digit(year.chars().next()?) {
        std::mem::swap(&mut year, &mut zone);
    }
    if let Some(trimmed) = clock.strip_suffix(',') {
        clock = trimmed.to_owned();
    }
    let components: Vec<&str> = clock.split(':').collect();
    let components = if components.len() == 1 && clock.contains('.') {
        clock.split('.').collect()
    } else {
        components
    };
    let mut components = components.into_iter();
    let hours = number(components.next()?)?;
    let minutes = number(components.next()?)?;
    let seconds = match components.next() {
        Some(s) => number(s)?,
        None => 0,
    };
    if components.next().is_some() {
        return None;
    }
    let mut year = number(&year)?;
    if year < 100 {
        year = year.checked_add(if year > 68 { 1900 } else { 2000 })?;
    }
    // Python datetime accepts only years 1..9999 and seconds 0..59.
    if !(1..=9999).contains(&year) {
        return None;
    }
    let date = NaiveDate::from_ymd_opt(
        i32::try_from(year).ok()?,
        month,
        u32::try_from(number(&day)?).ok()?,
    )?;
    let date = date.and_hms_opt(
        u32::try_from(hours).ok()?,
        u32::try_from(minutes).ok()?,
        u32::try_from(seconds).ok()?,
    )?;
    let known = upper_ascii(&zone).and_then(|s| match s.as_str() {
        "UT" | "UTC" | "GMT" | "Z" => Some(0),
        "AST" | "EDT" => Some(-400),
        "ADT" => Some(-300),
        "EST" | "CDT" => Some(-500),
        "CST" | "MDT" => Some(-600),
        "MST" | "PDT" => Some(-700),
        "PST" => Some(-800),
        _ => None,
    });
    let offset = match known {
        Some(value) => Some(value),
        None => match integer(&zone) {
            Integer::Value(0) if zone.starts_with('-') => None,
            Integer::Value(n) => Some(n),
            Integer::Invalid => None,
            Integer::Huge => return None,
        },
    };
    match offset {
        None => resolve_local(date).filter(|n| n.is_finite()),
        Some(offset) => {
            let magnitude = offset.unsigned_abs();
            let seconds = (magnitude / 100)
                .checked_mul(3600)?
                .checked_add((magnitude % 100) * 60)?;
            if seconds >= 86400 {
                return None;
            }
            let seconds = i64::try_from(seconds).ok()? * if offset < 0 { -1 } else { 1 };
            Some((date.and_utc().timestamp() - seconds) as f64)
        }
    }
}
