use crate::Config;
use std::{collections::HashMap, time::Duration};
// Unicode15.0 Numeric_Type=Decimal, matching production CPython3.12. Source:
// https://www.unicode.org/Public/15.0.0/ucd/extracted/DerivedNumericType.txt
// Mathematical digits comprise five consecutive ten-digit alphabets.
const BASES: &[u32] = &[
    0x30, 0x660, 0x6f0, 0x7c0, 0x966, 0x9e6, 0xa66, 0xae6, 0xb66, 0xbe6, 0xc66, 0xce6, 0xd66,
    0xde6, 0xe50, 0xed0, 0xf20, 0x1040, 0x1090, 0x17e0, 0x1810, 0x1946, 0x19d0, 0x1a80, 0x1a90,
    0x1b50, 0x1bb0, 0x1c40, 0x1c50, 0xa620, 0xa8d0, 0xa900, 0xa9d0, 0xa9f0, 0xaa50, 0xabf0, 0xff10,
    0x104a0, 0x10d30, 0x11066, 0x110f0, 0x11136, 0x111d0, 0x112f0, 0x11450, 0x114d0, 0x11650,
    0x116c0, 0x11730, 0x118e0, 0x11950, 0x11c50, 0x11d50, 0x11da0, 0x11f50, 0x16a60, 0x16ac0,
    0x16b50, 0x1d7ce, 0x1d7d8, 0x1d7e2, 0x1d7ec, 0x1d7f6, 0x1e140, 0x1e2f0, 0x1e4f0, 0x1e950,
    0x1fbf0,
];
fn space(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}
fn normalize(value: &str) -> Option<String> {
    if value.len() > 16384 {
        return None;
    }
    let value = value.trim_matches(space);
    let mut output = String::new();
    output.try_reserve_exact(value.len()).ok()?;
    for c in value.chars() {
        let code = u32::from(c);
        let digit = BASES
            .iter()
            .find_map(|base| code.checked_sub(*base).filter(|n| *n < 10));
        if let Some(d) = digit {
            output.push(char::from(b'0' + u8::try_from(d).ok()?));
        } else {
            output.push(c);
        }
    }
    let bytes = output.as_bytes();
    for (at, c) in bytes.iter().enumerate() {
        if *c == b'_'
            && !(at
                .checked_sub(1)
                .and_then(|n| bytes.get(n))
                .is_some_and(u8::is_ascii_digit)
                && bytes.get(at + 1).is_some_and(u8::is_ascii_digit))
        {
            return None;
        }
    }
    Some(output.replace('_', ""))
}
fn int(value: &str) -> Option<u64> {
    let value = normalize(value)?;
    let negative = value.starts_with('-');
    let value = value.strip_prefix(['+', '-']).unwrap_or(&value);
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let number = value.bytes().try_fold(0u64, |n, b| {
        n.checked_mul(10)?.checked_add(u64::from(b - b'0'))
    })?;
    if negative && number != 0 {
        None
    } else {
        Some(number)
    }
}
fn float(value: &str) -> Option<f64> {
    normalize(value)?.parse().ok()
}
impl Config {
    pub fn from_env(env: &HashMap<String, String>) -> Self {
        let size = env
            .get("LAYA_MAX_CONCURRENCY")
            .and_then(|s| int(s))
            .filter(|n| (1..=4).contains(n))
            .and_then(|n| usize::try_from(n).ok())
            .unwrap_or(1);
        let decision = env
            .get("LAYA_DECISION_TIMEOUT_S")
            .and_then(|s| float(s))
            .filter(|n| n.is_finite() && (0.5..=2.0).contains(n))
            .unwrap_or(1.3);
        let queue = env
            .get("LAYA_QUEUE_TIMEOUT_MS")
            .and_then(|s| int(s))
            .filter(|n| *n <= 10000)
            .unwrap_or(2000);
        Self {
            python: "python".into(),
            script: "/service/server.py".into(),
            size,
            startup: Duration::from_secs(90),
            decision: Duration::from_secs_f64(decision),
            queue: Duration::from_millis(queue),
            late_grace: Duration::from_secs(15),
            retries: [
                Duration::ZERO,
                Duration::from_secs(2),
                Duration::from_secs(8),
            ],
        }
    }
    pub fn production() -> Self {
        let env = [
            "LAYA_MAX_CONCURRENCY",
            "LAYA_DECISION_TIMEOUT_S",
            "LAYA_QUEUE_TIMEOUT_MS",
        ]
        .into_iter()
        .filter_map(|key| std::env::var(key).ok().map(|value| (key.to_owned(), value)))
        .collect();
        Self::from_env(&env)
    }
}
