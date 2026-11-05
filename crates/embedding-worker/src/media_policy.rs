//! Legacy document progress formats and bounded page windows. Chunking is supplied
//! by the native RAG index owner so head and tail hashes always share boundaries.
use serde_json::{json, Value};
fn hex(value: &Value) -> bool {
    value.as_str().is_some_and(|s| {
        s.len() == 64
            && s.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(v) => *v,
        Value::Number(v) => v.as_f64().is_some_and(|n| n != 0.0),
        Value::String(v) => !v.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}
fn image(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    [".png", ".jpg", ".jpeg", ".webp", ".gif"]
        .iter()
        .any(|ext| name.ends_with(ext))
}
pub fn scanned_page(page: &Value) -> bool {
    page["ocr"] == true
        || page["status"] == "ocr"
        || page["method"].as_str().is_some_and(|s| {
            s.split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                .any(|s| s == "ocr")
        })
}
pub fn needs_figures(file: &Value, record: &Value) -> bool {
    let d = &file["document"];
    let Some(name) = file["name"].as_str() else {
        return false;
    };
    let Some(extractor) = d["extractor"].as_str() else {
        return false;
    };
    if !d.is_object()
        || image(name)
        || extractor.contains(":figures-v1")
        || !hex(&d["version"])
        || !hex(&d["byteHash"])
        || d["figureSweep"] == d["version"]
        || !d["availableVersion"]
            .as_str()
            .is_some_and(|s| !s.is_empty())
        || matches!(d["state"].as_str(), Some("failed" | "queued"))
    {
        return false;
    }
    let pages: Vec<_> = record["pageTexts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|p| !p.is_null() && p["status"] != "blank")
        .collect();
    pages.is_empty() || !pages.iter().all(|p| scanned_page(p))
}
pub fn upload_candidate(file: &Value, extractor: &str) -> bool {
    let d = &file["document"];
    d.is_object()
        && !truthy(&file["source"])
        && file["name"]
            .as_str()
            .is_some_and(|s| s.to_ascii_lowercase().ends_with(".pdf"))
        && hex(&d["version"])
        && hex(&d["byteHash"])
        && d["extractor"] == extractor
        && d["completionSweep"] != d["version"]
}
pub fn tail_candidate(file: &Value, head_cap: usize, text_length: usize) -> bool {
    let d = &file["document"];
    d.is_object()
        && hex(&d["availableVersion"])
        && matches!(d["indexing"].as_str(), Some("ready" | "partial"))
        && text_length >= head_cap
        && !(d.pointer("/searchable/version") == d.get("availableVersion")
            && d.pointer("/searchable/complete") == Some(&Value::Bool(true)))
}
fn number(value: &Value) -> Option<i64> {
    value.as_i64().or_else(|| value.as_str()?.parse().ok())
}
fn segment(page: &Value) -> String {
    format!(
        "[Page {}]\n{}",
        number(&page["number"]).unwrap_or(0),
        page["text"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| format!("({})", page["status"].as_str().unwrap_or("undefined")))
    )
}
fn page_markers(text: &str) -> Vec<i64> {
    let mut out = vec![];
    for part in std::iter::once(text).chain(text.match_indices("\n\n").map(|(i, _)| &text[i + 2..]))
    {
        if let Some(after) = part.strip_prefix("[Page ") {
            if let Some((n, _)) = after.split_once("]\n") {
                if !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) {
                    if let Ok(n) = n.parse() {
                        out.push(n);
                    }
                }
            }
        }
    }
    out
}
pub fn tail_window(
    content: &str,
    record: &Value,
    options: &Value,
    chunk: impl Fn(&str) -> Vec<String>,
) -> Value {
    if content.is_empty() {
        return Value::Null;
    }
    let mut pages: Vec<_> = record["pageTexts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|p| p.is_object() && number(&p["number"]).is_some())
        .collect();
    pages.sort_by_key(|p| number(&p["number"]).unwrap_or(0));
    let Some(last) = page_markers(content).last().copied().filter(|n| *n != 0) else {
        return Value::Null;
    };
    let head = pages
        .iter()
        .filter(|p| number(&p["number"]).unwrap_or(0) <= last)
        .map(|p| segment(p))
        .collect::<Vec<_>>()
        .join("\n\n");
    if !head.starts_with(content) {
        return Value::Null;
    }
    let start = last.max(options["fromPage"].as_i64().unwrap_or(0));
    let cap = options["textLimit"]
        .as_u64()
        .filter(|n| *n > 0 && *n <= 9_007_199_254_740_991)
        .unwrap_or(u64::MAX);
    let max_chars = options["maxChars"].as_u64().unwrap_or(24_000);
    let (mut offset, mut chars, mut to_page, mut limited) = (0u64, 0u64, start - 1, false);
    let mut parts = vec![];
    for (i, page) in pages.iter().enumerate() {
        let seg = segment(page);
        let from = offset.saturating_add(if i == 0 { 0 } else { 2 });
        offset = from.saturating_add(seg.encode_utf16().count() as u64);
        let n = number(&page["number"]).unwrap_or(0);
        if n < start {
            continue;
        }
        if from >= cap {
            limited = true;
            break;
        }
        if !parts.is_empty() && chars >= max_chars {
            break;
        }
        let kept = if offset > cap {
            limited = true;
            js_json::wtf16::slice(&seg, (cap - from) as usize)
        } else {
            seg
        };
        chars += kept.encode_utf16().count() as u64;
        parts.push(kept);
        to_page = n;
        if limited {
            break;
        }
    }
    let final_window = limited
        || pages
            .last()
            .is_none_or(|p| to_page >= number(&p["number"]).unwrap_or(0));
    let chunks: Vec<_> = if parts.is_empty() {
        vec![]
    } else {
        chunk(&parts.join("\n\n"))
            .into_iter()
            .map(|body| {
                let page = page_markers(&body).first().copied().unwrap_or(0);
                json!({"body": body, "page": page})
            })
            .collect()
    };
    json!({"chunks": chunks, "fromPage": start, "toPage": to_page, "final": final_window, "limited": limited, "headPage": last})
}
/// Number(string) for worker environment settings, including the radix forms
/// accepted by the existing Node settings. Nonfinite values remain unavailable.
pub fn env_number(raw: &str) -> Option<f64> {
    let raw = server_projects::js::trim(raw);
    if raw.is_empty() {
        return Some(0.0);
    }
    for (prefix, base) in [
        ("0x", 16),
        ("0X", 16),
        ("0o", 8),
        ("0O", 8),
        ("0b", 2),
        ("0B", 2),
    ] {
        if let Some(digits) = raw.strip_prefix(prefix) {
            if digits.is_empty() {
                return None;
            }
            let mut result = 0.0;
            for digit in digits.chars() {
                result = result * (base as f64) + (digit.to_digit(base)? as f64);
            }
            return result.is_finite().then_some(result);
        }
    }
    raw.parse::<f64>().ok().filter(|n| n.is_finite())
}

/// Environment coercion and clamping match document-backfill.cjs settings().
pub fn settings(env: &Value) -> Value {
    let n = |key: &str, default: f64, lo: f64, hi: f64, round: bool| {
        let value = env
            .get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .and_then(env_number)
            .filter(|v| v.is_finite())
            .unwrap_or(default)
            .clamp(lo, hi);
        if round {
            value.round()
        } else {
            value
        }
    };
    json!({
        "perPass": n("PROJECT_TAIL_EMBED_PER_PASS", 48.0, 1.0, 10000.0, true),
        "perHour": n("PROJECT_TAIL_EMBED_PER_HOUR", 480.0, 1.0, 100000.0, true),
        "perDocument": n("PROJECT_TAIL_EMBED_PER_DOCUMENT", 24.0, 1.0, 10000.0, true),
        "batch": n("PROJECT_TAIL_EMBED_BATCH", 8.0, 1.0, 64.0, true),
        "uploads": n("PROJECT_UPLOAD_COMPLETION_PER_PASS", 2.0, 0.0, 50.0, true),
        "duty": n("PROJECT_BACKFILL_DUTY", 0.25, 0.05, 1.0, false),
        "minPauseMs": n("PROJECT_BACKFILL_MIN_PAUSE_MS", 250.0, 0.0, 600000.0, true),
        "delayMs": n("PROJECT_BACKFILL_DELAY_MS", 90000.0, 0.0, 3600000.0, true),
        "intervalMs": n("PROJECT_BACKFILL_INTERVAL_MS", 300000.0, 10000.0, 86400000.0, true)
    })
}

/// rag.cjs clampInt uses parseInt(value, 10), including signs and trailing text.
pub fn media_timeout_ms(raw: Option<&str>) -> u64 {
    let Some(raw) = raw else {
        return 120000;
    };
    let raw = server_projects::js::trim(raw);
    let (negative, digits) = if let Some(rest) = raw.strip_prefix('-') {
        (true, rest)
    } else {
        (false, raw.strip_prefix('+').unwrap_or(raw))
    };
    let mut number = 0.0_f64;
    let mut count = 0;
    for digit in digits.bytes().take_while(u8::is_ascii_digit) {
        number = number * 10.0 + f64::from(digit - b'0');
        count += 1;
    }
    if count == 0 || !number.is_finite() {
        return 120000;
    }
    if negative {
        number = -number;
    }
    number.clamp(5000.0, 600000.0) as u64
}
