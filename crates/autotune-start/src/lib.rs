//! Admission for `POST /api/models/autotune` (sbstndalton/noevia R6, head start).
//!
//! Oracle: noevia-core `server/llamacpp-full-autotune.cjs`, `start`, lines 1604-1622 (the checks
//! that need only the request and two facts from state). The steps after them in Node (the flush,
//! the scan, the ladder read, the long-profile dry run, the job record) are not here.
//!
//! Pure: nothing is started, held or written. The caller reads the body fields as raw JSON values
//! (so a hostile type reaches the same refusal Node gives) and supplies `run_active` and
//! `system_model`. The checks run in Node's order and the first failure wins.

use serde_json::Value;

/// Fast tunes the model's own section with today's prompt time limit (Node `MODES.fast`).
pub const FAST_DEFAULT_SECONDS: u32 = 120;
/// Long tunes the `<model>-long` section with a limit up to 1800 s (Node `MODES.long`).
pub const LONG_DEFAULT_SECONDS: u32 = 1800;
/// Lower bound of an accepted prompt time limit, in seconds, inclusive.
pub const MIN_SECONDS: u32 = 15;
/// Upper bound of an accepted prompt time limit, in seconds, inclusive.
pub const MAX_SECONDS: u32 = 1800;
/// Node `model-system.cjs` `SYSTEM_MODEL_REASON`, shown as the refusal text.
pub const SYSTEM_MODEL_REASON: &str = "System routing model — not tuned";

const CONFIRM_PAUSE: &str =
    "Confirm that chat can pause and other model-server clients are stopped.";
const CHOOSE_MODE: &str = "Choose Fast or Long.";
const LONG_ONE_MODEL: &str = "A Long tune runs for one model at a time. Choose a model.";
const CHOOSE_SECONDS: &str = "Choose a prompt time limit between 15 and 1800 seconds.";
const ALREADY_RUNNING: &str = "Auto-tune is already running.";

/// The two modes Node accepts (`MODES`). Any other value is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Fast,
    Long,
}

/// An accepted start: the mode and the integer prompt time limit to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Plan {
    pub mode: Mode,
    pub prompt_budget_seconds: u32,
}

/// A refused start: the HTTP status and the message Node sends in `{ error }`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Refusal {
    pub status: u16,
    pub message: &'static str,
}

/// The request fields as the route received them. `None` means the key was absent from the body.
#[derive(Debug, Clone, Copy)]
pub struct Start<'a> {
    /// `confirmPause`: only the JSON value `true` passes (Node `!== true`).
    pub confirm_pause: Option<&'a Value>,
    /// `mode`: absent means Fast (Node's destructuring default). Present but not one of the two
    /// strings (including `null`) is refused.
    pub mode: Option<&'a Value>,
    /// `promptBudgetSeconds`: absent or `null` means the mode's default (Node `??`).
    pub prompt_budget_seconds: Option<&'a Value>,
    /// `untuned`: the bulk run over every configured chat model. Node's truthiness is applied by
    /// the caller.
    pub bulk: bool,
    /// Node `starting` or the job's status is `running`.
    pub run_active: bool,
    /// `isSystemModel(model)`. Only checked for a single-model run (Node `!bulk && ...`).
    pub system_model: bool,
}

/// Admit or refuse a start. Order: confirm, mode, Long with bulk, time limit, active run, system
/// model. The first failure is returned.
pub fn admit(start: &Start<'_>) -> Result<Plan, Refusal> {
    if start.confirm_pause != Some(&Value::Bool(true)) {
        return Err(refuse(400, CONFIRM_PAUSE));
    }
    let mode = match start.mode {
        None => Mode::Fast,
        Some(Value::String(s)) if s == "fast" => Mode::Fast,
        Some(Value::String(s)) if s == "long" => Mode::Long,
        Some(_) => return Err(refuse(400, CHOOSE_MODE)),
    };
    if mode == Mode::Long && start.bulk {
        return Err(refuse(400, LONG_ONE_MODEL));
    }
    let default = match mode {
        Mode::Fast => FAST_DEFAULT_SECONDS,
        Mode::Long => LONG_DEFAULT_SECONDS,
    };
    let seconds = match start.prompt_budget_seconds {
        None | Some(Value::Null) => f64::from(default),
        Some(v) => js_number(v),
    };
    let prompt_budget_seconds = whole_seconds(seconds).ok_or(refuse(400, CHOOSE_SECONDS))?;
    if start.run_active {
        return Err(refuse(409, ALREADY_RUNNING));
    }
    if !start.bulk && start.system_model {
        return Err(refuse(400, SYSTEM_MODEL_REASON));
    }
    Ok(Plan {
        mode,
        prompt_budget_seconds,
    })
}

fn refuse(status: u16, message: &'static str) -> Refusal {
    Refusal { status, message }
}

/// Node `Number.isInteger(x) && 15 <= x <= 1800`.
fn whole_seconds(x: f64) -> Option<u32> {
    if !x.is_finite() || x.fract() != 0.0 {
        return None;
    }
    let min = f64::from(MIN_SECONDS);
    let max = f64::from(MAX_SECONDS);
    if (min..=max).contains(&x) {
        // Range-checked above, so the cast is exact; `as` on an in-range whole f64 cannot wrap.
        Some(x as u32)
    } else {
        None
    }
}

/// The subset of JavaScript `Number(value)` that a JSON body can carry: booleans, numbers,
/// strings (decimal, `0x`/`0o`/`0b` integers, empty and whitespace-only as 0). Anything else is
/// NaN and so refused. Not covered: one-element arrays (JS coerces them to their element).
fn js_number(v: &Value) -> f64 {
    match v {
        Value::Bool(b) => f64::from(u8::from(*b)),
        Value::Number(n) => n.as_f64().unwrap_or(f64::NAN),
        Value::String(s) => js_string_number(s),
        Value::Null => 0.0,
        Value::Array(_) | Value::Object(_) => f64::NAN,
    }
}

fn js_string_number(s: &str) -> f64 {
    let t = s.trim();
    if t.is_empty() {
        return 0.0;
    }
    let radix = match t.get(..2) {
        Some("0x") | Some("0X") => Some(16),
        Some("0o") | Some("0O") => Some(8),
        Some("0b") | Some("0B") => Some(2),
        _ => None,
    };
    if let Some(radix) = radix {
        return t
            .get(2..)
            .and_then(|digits| u64::from_str_radix(digits, radix).ok())
            .map_or(f64::NAN, |n| n as f64);
    }
    // Rust accepts `inf` and `nan`, which JS Number does not; both end as non-finite, so both refuse.
    t.parse::<f64>().unwrap_or(f64::NAN)
}
