//! `PUT /api/models/inference-budget` (`routes/models.cjs`, `inference-budget.cjs`): an
//! administrator saves the inference memory budget, from 2 GiB to the host's RAM less 4 GiB, in
//! steps of 0.1. The caller writes the settings row and the audit record when [`Put::save`] says
//! so. Core enforces the saved figure only if it reads the row on every use (noevia-core
//! `inference-budget.cjs` from track R11 on); an older core reads it once at its start and keeps
//! enforcing the figure it read, so the caller's switch must be paired with such a core.

use crate::body;
use crate::round1;
use chat_turn::js;
use js_json::JValue;

/// The settings row's key.
pub const KEY: &str = "inference:memory-budget";
/// The audit action.
pub const AUDIT: &str = "inference.budget";
const DEFAULT_BUDGET_GIB: f64 = 16.0;
const MIN_BUDGET_GIB: f64 = 2.0;
const HOST_HEADROOM_GIB: f64 = 4.0;
const FALLBACK_MAX_GIB: f64 = 1024.0;
const KIB_PER_GIB: f64 = 1_048_576.0;

/// JavaScript's `\s`.
fn js_space(c: char) -> bool {
    js::is_js_space(c)
}

/// `hostRamGib(readMeminfo)`: `/^MemTotal:\s+(\d+)\s*kB/m` on the text, in GiB to one decimal;
/// `None` when the text cannot be read (`Err`), has no such line, or names no memory.
pub fn host_ram_gib(meminfo: Result<&str, ()>) -> Option<f64> {
    let text = meminfo.ok()?;
    let mut starts = vec![0usize];
    starts.extend(
        text.char_indices()
            .filter(|(_, c)| matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}'))
            .map(|(i, c)| i + c.len_utf8()),
    );
    for at in starts {
        let Some(rest) = text.get(at..).and_then(|r| r.strip_prefix("MemTotal:")) else {
            continue;
        };
        let after_space = rest.trim_start_matches(js_space);
        if after_space.len() == rest.len() {
            continue;
        }
        let digits_end = after_space
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(after_space.len());
        let Some(digits) = after_space.get(..digits_end).filter(|d| !d.is_empty()) else {
            continue;
        };
        let tail = after_space.get(digits_end..).unwrap_or("");
        if !tail.trim_start_matches(js_space).starts_with("kB") {
            continue;
        }
        let kib: f64 = digits.parse().unwrap_or(f64::NAN);
        return (kib > 0.0).then(|| round1(kib / KIB_PER_GIB));
    }
    None
}

/// `range()`: what may be saved on this host.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Range {
    pub min_gib: f64,
    pub max_gib: f64,
    pub host_ram_gib: Option<f64>,
}

/// `range()` for the host's RAM (`hostRamGib`): a host that names no memory (or 0 GiB) gets the
/// wide fallback bound.
pub fn range(host_ram_gib: Option<f64>) -> Range {
    let max_gib = match host_ram_gib {
        Some(h) if h != 0.0 && !h.is_nan() => MIN_BUDGET_GIB.max((h - HOST_HEADROOM_GIB).floor()),
        _ => FALLBACK_MAX_GIB,
    };
    Range {
        min_gib: MIN_BUDGET_GIB,
        max_gib,
        host_ram_gib,
    }
}

/// `positive(env.INFERENCE_MEMORY_BUDGET_GIB, DEFAULT_BUDGET_GIB)`.
pub fn default_gib(env: Option<&str>) -> f64 {
    let n = env.map_or(f64::NAN, js::string_to_number);
    if n.is_finite() && n > 0.0 {
        n
    } else {
        DEFAULT_BUDGET_GIB
    }
}

/// `save(value)`'s check: the figure, or the 400's body.
pub fn validate(value: &JValue, r: &Range) -> Result<f64, JValue> {
    let n = value.get("budgetGib").as_num().unwrap_or(f64::NAN);
    if !n.is_finite() || round1(n) != n || n < r.min_gib || n > r.max_gib {
        return Err(JValue::obj([
            (
                "error",
                JValue::Str(format!(
                    "Enter a budget from {} to {} GiB, in steps of 0.1.",
                    js_json::number_to_string(r.min_gib),
                    js_json::number_to_string(r.max_gib)
                )),
            ),
            ("errorId", JValue::from("invalidBudget")),
            ("minGib", JValue::Num(r.min_gib)),
            ("maxGib", JValue::Num(r.max_gib)),
        ]));
    }
    Ok(n)
}

/// `get()` right after a save of `n` (in range, so not limited).
pub fn saved(n: f64, default_gib: f64, r: &Range) -> JValue {
    JValue::obj([
        ("budgetGib", JValue::Num(n)),
        ("source", JValue::from("admin")),
        ("defaultGib", JValue::Num(default_gib)),
        ("minGib", JValue::Num(r.min_gib)),
        ("maxGib", JValue::Num(r.max_gib)),
        (
            "hostRamGib",
            r.host_ram_gib.map_or(JValue::Null, JValue::Num),
        ),
        ("limited", JValue::Bool(false)),
    ])
}

/// The settings row's text for `n` (`JSON.stringify({ budgetGib: n })`).
pub fn row(n: f64) -> String {
    format!(r#"{{"budgetGib":{}}}"#, js_json::number_to_string(n))
}

/// The audit record's detail.
pub fn audit_detail(n: f64) -> JValue {
    JValue::obj([("budgetGib", JValue::Num(n))])
}

/// What one PUT answers.
#[derive(Debug, Clone, PartialEq)]
pub struct Put {
    pub status: u16,
    pub body: JValue,
    /// `Cache-Control: no-store` (every answer past the administrator check).
    pub no_store: bool,
    /// The figure to write (the row and the audit record) before answering.
    pub save: Option<f64>,
}

/// One `PUT /api/models/inference-budget`: `body` is the request body (`None`: over the limit),
/// `default_gib` the deployment's figure, `host_ram_gib` this host's RAM. `None`: a body the port
/// cannot hold ([`body::Read::Unsupported`]), which goes to core unchanged.
pub fn put(
    admin: bool,
    body: Option<&[u8]>,
    default_gib: f64,
    host_ram_gib: Option<f64>,
) -> Option<Put> {
    if !admin {
        return Some(Put {
            status: 403,
            body: body::administrator_required(),
            no_store: false,
            save: None,
        });
    }
    let answer = |status, body, save| Put {
        status,
        body,
        no_store: true,
        save,
    };
    let value = match body::read_json(body) {
        body::Read::Value(v) => v,
        body::Read::Invalid => return Some(answer(400, body::invalid_json(), None)),
        body::Read::Unsupported => return None,
    };
    let r = range(host_ram_gib);
    Some(match validate(&value, &r) {
        Ok(n) => answer(200, saved(n, default_gib, &r), Some(n)),
        Err(refusal) => answer(400, refusal, None),
    })
}
