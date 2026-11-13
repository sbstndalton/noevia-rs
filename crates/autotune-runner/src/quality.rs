//! The quality gate (core llamacpp-full-autotune.cjs `QUALITY`, `qualityCheck`, `qualityFailure`,
//! the baseline helpers, #328): three deterministic probes, judged on the final answer only, and
//! relative to a baseline measured once per model at its reference settings.

use crate::js;
use js_json::{wtf16, JValue};
use regex::Regex;
use std::sync::LazyLock;

/// One probe: the prompt sent as the only user turn and the one answer accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Probe {
    pub id: &'static str,
    pub prompt: &'static str,
    pub expected: &'static str,
}

pub const QUALITY: [Probe; 3] = [
    Probe {
        id: "arithmetic",
        prompt: "Compute (17 * 4) - 9. Reply with only the integer.",
        expected: "59",
    },
    Probe {
        id: "extraction",
        prompt: "Record: name=Juniper; code=AX-417; colour=blue. Return only the code, without quotes.",
        expected: "AX-417",
    },
    Probe {
        id: "reasoning",
        prompt: "All daxes are blue. No blue things are round. Can a dax be round? Reply with only yes or no.",
        expected: "no",
    },
];

/// The probe a prompt belongs to.
pub fn probe_of(prompt: &str) -> Option<&'static Probe> {
    QUALITY.iter().find(|q| q.prompt == prompt)
}

pub const NO_PROBE_PASSED: &str = "The model failed every quality probe at its reference settings, so auto-tune cannot tell whether a setting harms it.";

/// `REFERENCE_LABEL[reference]` (`f16`, `bf16`, `current`).
pub fn reference_label(reference: &str) -> &'static str {
    match reference {
        "f16" => "f16 KV cache, drafting off",
        "bf16" => "bf16 KV cache, drafting off",
        "current" => "the current settings, drafting off",
        _ => "undefined",
    }
}

/// The JS `\s` class, spelled out (Rust's `\s` is Unicode White_Space: it has U+0085 and lacks
/// U+FEFF).
const S: &str = r"\t\n\x0B\x0C\r \x{a0}\x{1680}\x{2000}-\x{200a}\x{2028}\x{2029}\x{202f}\x{205f}\x{3000}\x{feff}";

static WORKED_SUM: LazyLock<Regex> = LazyLock::new(|| {
    let class = format!("[0-9{S}+\\-\u{2212}*x\u{d7}\u{f7}/().]");
    #[allow(clippy::expect_used)] // a constant pattern
    Regex::new(&format!("^{class}+(?:=[{S}]*{class}+)*=[{S}]*([0-9]+)$"))
        .expect("worked-sum pattern")
});
static CONTROLS: LazyLock<Regex> = LazyLock::new(|| {
    #[allow(clippy::expect_used)] // a constant pattern
    Regex::new(r"[\p{Cc}\p{Zl}\p{Zp}]+").expect("control pattern")
});
static SPACES: LazyLock<Regex> = LazyLock::new(|| {
    #[allow(clippy::expect_used)] // a constant pattern
    Regex::new(&format!("[{S}]+")).expect("space pattern")
});

/// `normalizedAnswer(text)`: trimmed, lower-cased, one layer of `**`/`__`/`` ` `` formatting
/// around the entire answer removed (in that order, each at most once), then one trailing `.` or
/// `!`.
pub fn normalized_answer(text: &str) -> String {
    let mut answer = js::trim(text).to_lowercase();
    for mark in ["**", "__", "`"] {
        if answer.starts_with(mark) && answer.ends_with(mark) && answer.len() > mark.len() * 2 {
            answer = js::trim(&answer[mark.len()..answer.len() - mark.len()]).to_string();
        }
    }
    if answer.ends_with('.') || answer.ends_with('!') {
        answer.pop();
    }
    js::trim(&answer).to_string()
}

/// `acceptsAnswer(q, answer)`: the expected answer exactly, or for arithmetic a worked line whose
/// value after the last `=` is exactly the expected integer.
pub fn accepts_answer(q: &Probe, answer: &str) -> bool {
    answer == q.expected.to_lowercase()
        || (q.id == "arithmetic"
            && WORKED_SUM
                .captures(answer)
                .and_then(|c| c.get(1))
                .is_some_and(|m| m.as_str() == q.expected))
}

/// `snippetOf(text)`: a short printable excerpt (at most 24 UTF-16 units) of a wrong answer.
pub fn snippet_of(text: &str) -> String {
    let flat = CONTROLS.replace_all(text, " ");
    let flat = SPACES.replace_all(&flat, " ");
    let flat = js::trim(&flat);
    if wtf16::len(flat) > 24 {
        wtf16::slice(flat, 23) + "\u{2026}"
    } else {
        flat.to_string()
    }
}

/// One probe's verdict over the reply `r` the runner's `chat` gave (`{ text, finishReason }` or
/// `{ failure }`): `{ id, passed, reason?, answer? }`.
pub fn judge(q: &Probe, r: &JValue) -> JValue {
    let answer = r
        .get("text")
        .as_str()
        .map(normalized_answer)
        .unwrap_or_default();
    let failure = r.get("failure");
    let reason: Option<JValue> = if failure.truthy() {
        Some(failure.clone())
    } else if r.get("finishReason").as_str() == Some("length") {
        Some("truncated".into())
    } else if answer.is_empty() {
        Some("no response".into())
    } else if !accepts_answer(q, &answer) {
        Some("mismatch".into())
    } else {
        None
    };
    let mut check = JValue::obj([
        ("id", JValue::from(q.id)),
        ("passed", reason.is_none().into()),
    ]);
    if let Some(reason) = reason {
        let mismatch = reason.as_str() == Some("mismatch");
        js::set(&mut check, "reason", reason);
        if mismatch {
            let text = r.get("text").as_str().unwrap_or_default();
            js::set(&mut check, "answer", snippet_of(text));
        }
    }
    check
}

/// The probes a baseline lets through (`QUALITY` in order, only those its `probes` lists), or
/// all three without one.
pub fn probes_for(baseline: Option<&JValue>) -> Vec<&'static Probe> {
    match baseline {
        None => QUALITY.iter().collect(),
        Some(b) => {
            let ids = js::items(b.get("probes"));
            QUALITY
                .iter()
                .filter(|q| ids.iter().any(|id| id.as_str() == Some(q.id)))
                .collect()
        }
    }
}

/// `qualityCheck`'s result from the checks it made: `{ passed, checks, skipped? }`.
pub fn summary(checks: Vec<JValue>, baseline: Option<&JValue>) -> JValue {
    let passed = checks
        .iter()
        .all(|c| c.get("passed") == &JValue::Bool(true));
    let skipped: Vec<JValue> = baseline
        .map(|b| {
            js::items(b.get("skipped"))
                .iter()
                .map(|s| s.get("id").clone())
                .collect()
        })
        .unwrap_or_default();
    let mut out = JValue::obj([("passed", passed.into()), ("checks", JValue::Arr(checks))]);
    if !skipped.is_empty() {
        js::set(&mut out, "skipped", JValue::Arr(skipped));
    }
    out
}

/// `qualityCheck(model, chat, baseline)`: asks each probe the baseline lets through, one after the
/// other, with `budget` (the family's quality budget) as `max_tokens`. A `chat` error stops it.
pub async fn quality_check<E>(
    budget: u32,
    baseline: Option<&JValue>,
    mut chat: impl AsyncFnMut(&'static str, u32) -> Result<JValue, E>,
) -> Result<JValue, E> {
    let mut checks = Vec::new();
    for q in probes_for(baseline) {
        let r = chat(q.prompt, budget).await?;
        checks.push(judge(q, &r));
    }
    Ok(summary(checks, baseline))
}

/// `qualityFailure(quality)`: the failed probes and what the model answered.
pub fn quality_failure(quality: &JValue) -> String {
    let failed: Vec<&JValue> = js::items(quality.get("checks"))
        .iter()
        .filter(|c| c.get("passed") != &JValue::Bool(true))
        .collect();
    let mut out = String::from("Quality checks failed: ");
    out.push_str(
        &failed
            .iter()
            .map(|c| {
                format!(
                    "{} ({})",
                    string_of(c.get("id")),
                    string_of(c.get("reason"))
                )
            })
            .collect::<Vec<_>>()
            .join(", "),
    );
    out.push('.');
    for c in failed.iter().filter(|c| c.get("answer").truthy()) {
        out.push_str(&format!(
            " Answered {}: \"{}\".",
            string_of(c.get("id")),
            string_of(c.get("answer"))
        ));
    }
    out
}

/// `String(v)` for the strings and absent values these records hold.
fn string_of(v: &JValue) -> String {
    js_json::to_js_string(v).unwrap_or_default()
}

/// `engineFailed(quality)`: an HTTP failure is the engine failing, not the model answering.
pub fn engine_failed(quality: &JValue) -> bool {
    js::items(quality.get("checks")).iter().any(|c| {
        let reason = c.get("reason").as_str().unwrap_or("");
        reason.strip_prefix("HTTP").is_some_and(|rest| {
            !rest
                .chars()
                .next()
                .is_some_and(|ch| ch.is_ascii_alphanumeric() || ch == '_')
        })
    })
}

/// `baselineOf(reference, quality)`.
pub fn baseline_of(reference: &str, quality: &JValue) -> JValue {
    let checks = js::items(quality.get("checks"));
    let passed = |c: &&JValue| c.get("passed") == &JValue::Bool(true);
    let probes = checks
        .iter()
        .filter(passed)
        .map(|c| c.get("id").clone())
        .collect();
    let skipped = checks
        .iter()
        .filter(|c| !passed(c))
        .map(|c| {
            let mut s = JValue::obj([
                ("id", c.get("id").clone()),
                ("reason", c.get("reason").clone()),
            ]);
            if c.get("answer").truthy() {
                js::set(&mut s, "answer", c.get("answer").clone());
            }
            s
        })
        .collect();
    JValue::obj([
        ("reference", JValue::from(reference)),
        ("probes", JValue::Arr(probes)),
        ("skipped", JValue::Arr(skipped)),
    ])
}

/// `baselineSummary(b)`: the job log line.
pub fn baseline_summary(b: &JValue) -> String {
    let mut parts: Vec<String> = js::items(b.get("probes"))
        .iter()
        .map(|id| format!("{} passed", string_of(id)))
        .collect();
    for s in js::items(b.get("skipped")) {
        let answer = if s.get("answer").truthy() {
            format!("; answered \"{}\"", string_of(s.get("answer")))
        } else {
            String::new()
        };
        parts.push(format!(
            "{} skipped (the model gets this wrong at its reference settings{answer})",
            string_of(s.get("id"))
        ));
    }
    format!(
        "Quality baseline at {}: {}.",
        reference_label(b.get("reference").as_str().unwrap_or("")),
        parts.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formatting_and_worked_sums() {
        // Formatting is stripped before the trailing dot, so a dot after it keeps the marks (as in Node).
        assert_eq!(normalized_answer("  **59**. "), "**59**");
        assert_eq!(normalized_answer("  **59.** "), "59");
        assert_eq!(normalized_answer("`AX-417`"), "ax-417");
        assert_eq!(normalized_answer("No!"), "no");
        let a = &QUALITY[0];
        assert!(accepts_answer(a, "68 - 9 = 59"));
        assert!(accepts_answer(a, "(17 × 4) − 9 = 68 − 9 = 59"));
        assert!(!accepts_answer(a, "not 59, it's 61"));
        assert!(!accepts_answer(a, "68 - 9 = 59 or 61"));
        assert!(!accepts_answer(&QUALITY[1], "68 - 9 = 59"));
    }

    #[test]
    fn engine_failure_reads_the_http_prefix() {
        let q = |r: &str| {
            JValue::obj([(
                "checks",
                JValue::Arr(vec![JValue::obj([("reason", JValue::from(r))])]),
            )])
        };
        assert!(engine_failed(&q("HTTP 500")));
        assert!(engine_failed(&q("HTTP error")));
        assert!(engine_failed(&q("HTTP")));
        assert!(!engine_failed(&q("HTTPS 500")));
        assert!(!engine_failed(&q("mismatch")));
    }

    #[test]
    fn snippet_cuts_at_23_units() {
        assert_eq!(snippet_of("a\nb\tc"), "a b c");
        let long = "x".repeat(30);
        assert_eq!(snippet_of(&long), "x".repeat(23) + "\u{2026}");
    }
}
