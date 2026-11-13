//! Admission for `POST /api/models/autotune` (sbstndalton/noevia R6, head start).
//!
//! Oracle: noevia-core `server/llamacpp-full-autotune.cjs`, `start`, lines 1604-1622 (the checks
//! that need only the request and two facts from state). The steps after them in Node (the flush,
//! the scan, the ladder read, the long-profile dry run, the job record) are not here.
//! `tests/fixtures/autotune-start.node.json` is Node's own answer for every case, recorded by
//! `tests/fixtures/record-node.cjs` (source sha256 in the file).
//!
//! Pure: nothing is started, held or written. The caller reads the body fields as raw JSON values
//! (so a hostile type reaches the same refusal Node gives) and supplies `run_active` and
//! `system_model`. The checks run in Node's order and the first failure wins.

use js_json::JValue;

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
    pub confirm_pause: Option<&'a JValue>,
    /// `mode`: absent (`None` or `undefined`) means Fast (Node's destructuring default). Present
    /// but not one of the two strings (including `null`) is refused.
    pub mode: Option<&'a JValue>,
    /// `promptBudgetSeconds`: absent or `null` means the mode's default (Node `??`).
    pub prompt_budget_seconds: Option<&'a JValue>,
    /// `untuned`: the bulk run over every configured chat model. The route passes
    /// `body.untuned === true`, so only the JSON value `true` makes a bulk run.
    pub bulk: bool,
    /// Node `starting` or the job's status is `running`.
    pub run_active: bool,
    /// `isSystemModel(model)`. Only checked for a single-model run (Node `!bulk && ...`).
    pub system_model: bool,
}

/// Admit or refuse a start. Order: confirm, mode, Long with bulk, time limit, active run, system
/// model. The first failure is returned.
pub fn admit(start: &Start<'_>) -> Result<Plan, Refusal> {
    if start.confirm_pause != Some(&JValue::Bool(true)) {
        return Err(refuse(400, CONFIRM_PAUSE));
    }
    let mode = match start.mode {
        // Node's destructuring default applies to `undefined` only; `null` is refused below.
        None | Some(JValue::Undefined) => Mode::Fast,
        Some(JValue::Str(s)) if s == "fast" => Mode::Fast,
        Some(JValue::Str(s)) if s == "long" => Mode::Long,
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
        None | Some(JValue::Null | JValue::Undefined) => f64::from(default),
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

/// JavaScript `Number(value)` for a JSON value: `ToPrimitive` first (an array becomes its
/// `join(',')`, so `[120]` and `["0x78"]` are 120; an object is `"[object Object]"`), then
/// `StringToNumber` with JS whitespace (`\u{feff}` trims, `\u{85}` does not). A radix literal past
/// 2^53 (which `llamacpp_presets::to_number` does not decide exactly) is far above 1800, so NaN
/// refuses it the same way.
fn js_number(v: &JValue) -> f64 {
    match v {
        JValue::Bool(b) => f64::from(u8::from(*b)),
        JValue::Num(n) => *n,
        JValue::Null => 0.0,
        JValue::Undefined => f64::NAN,
        JValue::Str(_) | JValue::Arr(_) | JValue::Obj(_) => js_json::to_js_string(v)
            .ok()
            .and_then(|s| llamacpp_presets::to_number(&s.encode_utf16().collect::<Vec<_>>()))
            .unwrap_or(f64::NAN),
    }
}
