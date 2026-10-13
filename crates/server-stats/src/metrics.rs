//! core `server/llamacpp-metrics.cjs`: `parseMetrics` (one llama-server's Prometheus text) and
//! `summarize` (the loaded processes' counters, the engine statistics the llama.cpp kind reports).

use crate::js::{is_space, string_to_number};
use prompt_framing::json::Value;
use std::collections::HashMap;

/// `summarize`'s `scope`.
pub const SCOPE: &str = "loaded model processes; counters reset on unload";

/// `parseMetrics(text)` over `String(text || '')`: every line matching
/// `/^llamacpp:([a-z_]+)\s+([^\s]+)$/` whose value is a finite, non-negative `Number(...)`; a
/// later line with the same name wins.
pub fn parse_metrics(text: &[u16]) -> HashMap<String, f64> {
    let mut values = HashMap::new();
    for line in text.split(|&c| c == u16::from(b'\n')) {
        let Some(rest) = strip_ascii(line, "llamacpp:") else {
            continue;
        };
        let name_len = rest
            .iter()
            .position(|&c| {
                !(c == u16::from(b'_') || (u16::from(b'a')..=u16::from(b'z')).contains(&c))
            })
            .unwrap_or(rest.len());
        if name_len == 0 {
            continue;
        }
        let (name, rest) = rest.split_at(name_len);
        let spaces = rest
            .iter()
            .position(|&c| !is_space(c))
            .unwrap_or(rest.len());
        if spaces == 0 {
            continue;
        }
        let value = rest.get(spaces..).unwrap_or(&[]);
        // `([^\s]+)$`: the rest of the line, non-empty, without a space.
        if value.is_empty() || value.iter().any(|&c| is_space(c)) {
            continue;
        }
        let n = string_to_number(value);
        if n.is_finite() && n >= 0.0 {
            values.insert(name.iter().map(|&c| char::from(c as u8)).collect(), n);
        }
    }
    values
}

/// `s` without the ASCII `prefix`, if it starts with it.
pub(crate) fn strip_ascii<'a>(s: &'a [u16], prefix: &str) -> Option<&'a [u16]> {
    let n = prefix.len();
    let head = s.get(..n)?;
    head.iter()
        .copied()
        .eq(prefix.bytes().map(u16::from))
        .then(|| s.get(n..).unwrap_or(&[]))
}

/// One loaded model's counters: `{ model: m.id, values }` (`model` is `None` where `m.id` is
/// `undefined`).
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub model: Option<Value>,
    pub values: HashMap<String, f64>,
}

/// A `mtp` row of `summarize`.
#[derive(Debug, Clone, PartialEq)]
pub struct MtpRow {
    pub model: Option<Value>,
    pub drafted: f64,
    pub accepted: f64,
    pub rate: f64,
}

/// What `summarize` returns; `scope` is always [`SCOPE`].
#[derive(Debug, Clone, PartialEq)]
pub struct Summary {
    pub tokens_per_second: Option<f64>,
    pub input_tokens_total: Option<f64>,
    pub output_tokens_total: Option<f64>,
    pub mtp: Vec<MtpRow>,
}

/// `summarize(rows)`: a sum only when every row has the counter (none for no rows), added in row
/// order from 0; an `mtp` row where `drafted > 0` and `accepted <= drafted`.
pub fn summarize(rows: &[Row]) -> Summary {
    let sum = |key: &str| -> Option<f64> {
        if rows.is_empty() {
            return None;
        }
        let mut total = 0.0;
        for r in rows {
            let v = r.values.get(key).copied().filter(|v| v.is_finite())?;
            total += v;
        }
        Some(total)
    };
    let mtp = rows
        .iter()
        .filter_map(|r| {
            let drafted = r
                .values
                .get("spec_decode_num_draft_tokens_total")
                .copied()?;
            let accepted = r
                .values
                .get("spec_decode_num_accepted_tokens_total")
                .copied()?;
            (drafted > 0.0 && accepted.is_finite() && accepted <= drafted).then(|| MtpRow {
                model: r.model.clone(),
                drafted,
                accepted,
                rate: accepted / drafted,
            })
        })
        .collect();
    Summary {
        tokens_per_second: sum("predicted_tokens_seconds"),
        input_tokens_total: sum("prompt_tokens_total"),
        output_tokens_total: sum("tokens_predicted_total"),
        mtp,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn u(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    #[test]
    fn lines_match_exactly() {
        let text = u("# HELP x\nllamacpp:prompt_tokens_total 12\nllamacpp:tokens_predicted_total\t7\r\nllamacpp:Bad 1\nllamacpp:a 1 2\nllamacpp:b -1\nllamacpp:c 0x10\nllamacpp:d\u{a0}5\nllamacpp:e 1e999\nllamacpp:prompt_tokens_total 13\nllamacpp:f\u{2028}3\nllamacpp: 4\n");
        let v = parse_metrics(&text);
        assert_eq!(v.get("prompt_tokens_total"), Some(&13.0));
        assert_eq!(
            v.get("tokens_predicted_total"),
            None,
            "CRLF never matches $"
        );
        assert_eq!(v.get("c"), Some(&16.0));
        assert_eq!(v.get("d"), Some(&5.0));
        assert_eq!(v.get("f"), Some(&3.0));
        for k in ["Bad", "a", "b", "e", ""] {
            assert!(!v.contains_key(k), "{k}");
        }
    }

    #[test]
    fn sums_need_every_row() {
        let row = |pairs: &[(&str, f64)]| Row {
            model: Some(Value::Str(u("m"))),
            values: pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect(),
        };
        assert_eq!(summarize(&[]).tokens_per_second, None);
        let s = summarize(&[
            row(&[
                ("predicted_tokens_seconds", 1.5),
                ("prompt_tokens_total", 2.0),
            ]),
            row(&[("predicted_tokens_seconds", 2.25)]),
        ]);
        assert_eq!(s.tokens_per_second, Some(3.75));
        assert_eq!(s.input_tokens_total, None);
        let m = summarize(&[
            row(&[
                ("spec_decode_num_draft_tokens_total", 10.0),
                ("spec_decode_num_accepted_tokens_total", 4.0),
            ]),
            row(&[
                ("spec_decode_num_draft_tokens_total", 3.0),
                ("spec_decode_num_accepted_tokens_total", 4.0),
            ]),
            row(&[("spec_decode_num_draft_tokens_total", 3.0)]),
        ]);
        assert_eq!(m.mtp.len(), 1);
        assert_eq!(m.mtp[0].rate, 0.4);
    }
}
