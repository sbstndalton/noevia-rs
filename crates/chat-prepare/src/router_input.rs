//! What the router role reads of a turn (#779 F1, #1640, slice P5d, part 3): noevia-core
//! `server/routing-modes.cjs` `routerInput({ message, system, history, attachments }, { chunkChars,
//! maxChunks })`, with `fairShares` and `chunkCharsFor`. The hybrid route's sensitivity check
//! (`routingModes.sensitivity`, a decision seam) reads these chunks; `complete` is false when
//! anything a cloud route would send was dropped or cut, and then the check may not clear the turn.
//!
//! Lengths, cuts and chunks are in UTF-16 code units (a cut may split a surrogate pair: the halves
//! are lone surrogates, held as `wtf16` text). Recorded in `tests/fixtures/node-prerules.v1.json`
//! (`routerInput`, `chunkCharsFor`), written by `tests/record_prerules.cjs`; no named differences.

use js_json::{to_js_string, JValue};

use crate::js;
use crate::Thrown;

/// routing-modes.cjs ROUTER_CHUNK_CHARS.
pub const ROUTER_CHUNK_CHARS: usize = 1000;
/// ROUTER_MAX_CHUNKS.
pub const ROUTER_MAX_CHUNKS: usize = 1;
/// MAX_CHUNKS_CAP.
pub const MAX_CHUNKS_CAP: usize = 8;
/// The history entries read, the newest first.
const HISTORY_READ: usize = 40;
const SEP: &str = "\n\n";

/// `routerInput`'s answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouterInput {
    pub chunks: Vec<String>,
    pub complete: bool,
}

/// `Math.floor(Number(x) || fallback)` clamped to `[lo, hi]`, as `Math.max(lo, Math.min(hi, ..))`.
fn bounded(x: Option<f64>, fallback: usize, lo: usize, hi: usize) -> usize {
    let n = x
        .filter(|n| !n.is_nan() && *n != 0.0)
        .unwrap_or(fallback as f64);
    let n = n.floor();
    if n <= lo as f64 {
        lo
    } else if n >= hi as f64 {
        hi
    } else {
        n as usize
    }
}

/// `textOf(content)`: a string as is, an array's parts' `text` joined with a space, else `''`.
fn text_of(content: &JValue) -> String {
    match content {
        JValue::Str(s) => s.clone(),
        JValue::Arr(parts) => parts
            .iter()
            .map(|p| match (p.truthy(), p.get("text")) {
                (true, JValue::Str(t)) => t.as_str(),
                _ => "",
            })
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    }
}

/// `fairShares(needs, budget)`: max-min fair shares of `budget` for sections needing `needs` units.
pub fn fair_shares(needs: &[usize], budget: i64) -> Vec<usize> {
    let mut shares = vec![0usize; needs.len()];
    let mut left = budget.max(0) as usize;
    let mut open: Vec<usize> = (0..needs.len())
        .filter(|&i| needs.get(i).is_some_and(|&n| n > 0))
        .collect();
    while !open.is_empty() && left > 0 {
        let each = left / open.len();
        if each == 0 {
            break;
        }
        let mut next = Vec::new();
        for &i in &open {
            let (Some(&need), Some(share)) = (needs.get(i), shares.get_mut(i)) else {
                continue;
            };
            let give = each.min(need - *share);
            *share += give;
            left -= give;
            if *share < need {
                next.push(i);
            }
        }
        open = next;
    }
    shares
}

/// `routerInput(...)`. `history` is `mappedHistory` (anything but an array reads as none);
/// `attachments` the attachment names (the non-text ones are left out, as Node's filter does);
/// `chunk_chars` and `max_chunks` the options (`None`: `undefined`). `Err` where Node throws:
/// a history entry whose `role` `String()` refuses.
pub fn router_input(
    message: &str,
    system: &str,
    history: &JValue,
    attachments: &[JValue],
    chunk_chars: Option<f64>,
    max_chunks: Option<f64>,
) -> Result<RouterInput, Thrown> {
    let size = bounded(chunk_chars, ROUTER_CHUNK_CHARS, 200, ROUTER_CHUNK_CHARS * 4);
    let count = bounded(max_chunks, ROUTER_MAX_CHUNKS, 1, MAX_CHUNKS_CAP);
    let cap = size * count;
    let mut complete = true;
    let entries: Vec<&JValue> = match history {
        JValue::Arr(items) => items.iter().filter(|h| h.truthy()).collect(),
        _ => Vec::new(),
    };
    if entries.len() > HISTORY_READ {
        complete = false;
    }
    let mut lines = Vec::new();
    for h in entries.iter().rev().take(HISTORY_READ) {
        // An applied write's target and arguments are sent with it (chat.cjs appliedToolNote).
        let field = |key: &str, label: &str| match h.get(key) {
            JValue::Str(s) if !s.is_empty() => Some(format!("{label}: {s}")),
            _ => None,
        };
        let extra: Vec<String> = [field("target", "target"), field("args", "arguments")]
            .into_iter()
            .flatten()
            .collect();
        let content = text_of(h.get("content"));
        let text = if extra.is_empty() {
            content
        } else {
            format!("{content}\n{}", extra.join("\n"))
        };
        if js::len(&text) > cap {
            complete = false;
        }
        let role = to_js_string(h.get("role")).map_err(|_| Thrown)?;
        let line = format!("{role}: {}", js::slice(&text, cap));
        if js::len(&line) > 2 {
            lines.push(line);
        }
    }
    let hist = lines.join("\n");
    let names = attachments
        .iter()
        .filter_map(|n| match n {
            JValue::Str(s) if !s.is_empty() => Some(s.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(", ");
    let sections: Vec<(&str, String)> = [
        ("Message", message.to_string()),
        ("Recent history (most recent first)", hist),
        ("Context", system.to_string()),
        ("Attachments", names),
    ]
    .into_iter()
    .filter(|(_, text)| !js::trim(text).is_empty())
    .collect();
    if sections.is_empty() {
        return Ok(RouterInput {
            chunks: Vec::new(),
            complete,
        });
    }
    // Labels, the separators between sections and a little slack are paid for before the shares.
    let overhead: usize = sections
        .iter()
        .map(|(label, _)| label.len() + 2)
        .sum::<usize>()
        + SEP.len() * (sections.len() - 1);
    let needs: Vec<usize> = sections.iter().map(|(_, text)| js::len(text)).collect();
    let shares = fair_shares(&needs, cap as i64 - overhead as i64);
    if needs.iter().zip(&shares).any(|(need, share)| share < need) {
        complete = false;
    }
    let whole = sections
        .iter()
        .zip(&shares)
        .map(|((label, text), &share)| format!("{label}:\n{}", js::slice(text, share)))
        .collect::<Vec<_>>()
        .join(SEP);
    let units = js::to_units(&whole);
    let chunks: Vec<String> = units.chunks(size).take(count).map(js::from_units).collect();
    if chunks.len() * size < units.len() {
        complete = false;
    }
    Ok(RouterInput { chunks, complete })
}

/// `chunkCharsFor(backend)`: the backend's `limits.maxStateChars` when it is a finite number of
/// at least 200 (floored, at most [`ROUTER_CHUNK_CHARS`]), else [`ROUTER_CHUNK_CHARS`]. `limit` is
/// `Number(backend?.limits?.maxStateChars)`, the caller's (`None`: not a number).
pub fn chunk_chars_for(limit: Option<f64>) -> usize {
    match limit {
        Some(n) if n.is_finite() && n >= 200.0 => (n.floor() as usize).min(ROUTER_CHUNK_CHARS),
        _ => ROUTER_CHUNK_CHARS,
    }
}
