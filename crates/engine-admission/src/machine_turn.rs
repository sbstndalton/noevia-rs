//! core chat-machines.cjs (#1840, #1849, #1851, #1854, #1880) and machine-health.cjs (#1638, #1861): a chat
//! role served by another machine's OpenAI-compatible server, as the turn sees it. Read-only and inert: nothing
//! in the front calls this yet (pool item 1, p8-3 CONTINUE 4 "machines"); a turn on a machine still runs in Node.
//!
//! What it carries, in Node's words and order:
//! - [`provider_row`] / [`MachineTurns::measured_provider`]: the provider-shaped row a turn uses for the role on
//!   its machine (`machine:<id>`, no key, `<base>/v1`, the measured context and thinking switch);
//! - [`MachineTurns::ready`]: whether the machine answers its health check AND serves the assigned model, with
//!   the "unreachable" / "serves …, not …" texts ([`unreachable_text`], [`wrong_model_text`]);
//! - the facts a machine's server says about itself ([`props_facts_of`] over `/props`, [`model_ids`] over
//!   `/v1/models`), kept beside its health result in one ~30 s cache ([`HealthCache`]);
//! - a Manual pick of a model a machine serves ([`manual_target`]), the Manual list ([`machine_models`]) and the
//!   Automatic roles as a turn sees them ([`effective_roles`], [`local_roles`]);
//! - the turns in flight per machine ([`Turns`]: `beginTurn`, `busy`, `onTurn`) a measurement reads.
//!
//! Every request goes through [`Fetch`]; [`NetFetch`] is the on-this-network client (model_stream::net: the
//! address checked at connect time, no redirect, no proxy) with a deadline and a body cap.
use crate::machines::Target;
use js_json::{wtf16, JValue};
use std::time::Duration;

/// `ROLE_LABEL`.
pub const CHAT_ROLES: [&str; 5] = ["fast", "smart", "code", "creative", "vision"];
pub const MODELS_TIMEOUT: Duration = Duration::from_millis(5000);
pub const PROPS_TIMEOUT: Duration = Duration::from_millis(2500);
pub const MODELS_BODY_CAP: usize = 256 * 1024;
/// A /props answer carries the whole chat template (#1850 reads it for the tools gate).
pub const PROPS_BODY_CAP: usize = 1024 * 1024;
pub const MAX_MODELS: usize = 200;
pub const CONTEXT_MIN: f64 = 2048.0;
pub const CONTEXT_MAX: f64 = 2_000_000.0;
/// A llama-server that does not say its context gets the local engine's conservative floor.
pub const CONTEXT_FALLBACK: u32 = 8192;
/// #1854: the measurements that belong to this machine's engine and are never taken on another machine.
pub const NOT_MEASURED_ON_MACHINES: [&str; 4] =
    ["autotune", "speculativeDecoding", "longContextProfile", "presets"];

/// `ROLE_LABEL[role] || role`.
pub fn label(role: &str) -> &str {
    match role {
        "fast" => "Fast",
        "smart" => "Smart",
        "code" => "Code",
        "creative" => "Creative",
        "vision" => "Vision",
        other => other,
    }
}

fn is_chat_role(role: &str) -> bool {
    CHAT_ROLES.contains(&role)
}

/// JS white space and line terminators (`String.prototype.trim`).
fn js_space(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n'
            | '\u{b}'
            | '\u{c}'
            | '\r'
            | ' '
            | '\u{a0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200a}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202f}'
            | '\u{205f}'
            | '\u{3000}'
            | '\u{feff}'
    )
}

fn js_trim(s: &str) -> &str {
    s.trim_matches(js_space)
}

/// `StringToNumber`.
fn string_to_number(text: &str) -> f64 {
    let t = js_trim(text);
    if t.is_empty() {
        return 0.0;
    }
    match t {
        "Infinity" | "+Infinity" => return f64::INFINITY,
        "-Infinity" => return f64::NEG_INFINITY,
        _ => {}
    }
    let radix = |digits: &str, base: u32| {
        if digits.is_empty() || !digits.chars().all(|c| c.is_digit(base)) {
            return f64::NAN;
        }
        digits
            .chars()
            .filter_map(|c| c.to_digit(base))
            .fold(0.0, |acc, d| acc * f64::from(base) + f64::from(d))
    };
    let bytes = t.as_bytes();
    if bytes.len() > 2 && bytes.first() == Some(&b'0') {
        match bytes.get(1) {
            Some(b'x' | b'X') => return radix(t.get(2..).unwrap_or(""), 16),
            Some(b'o' | b'O') => return radix(t.get(2..).unwrap_or(""), 8),
            Some(b'b' | b'B') => return radix(t.get(2..).unwrap_or(""), 2),
            _ => {}
        }
    }
    let unsigned = t.strip_prefix(['+', '-']).unwrap_or(t);
    let (mantissa, exponent) = match unsigned.find(['e', 'E']) {
        Some(at) => (unsigned.get(..at).unwrap_or(""), unsigned.get(at + 1..)),
        None => (unsigned, None),
    };
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let mantissa_ok = match mantissa.split_once('.') {
        Some((int, frac)) => {
            (digits(int) || int.is_empty())
                && (digits(frac) || frac.is_empty())
                && !(int.is_empty() && frac.is_empty())
        }
        None => digits(mantissa),
    };
    let exponent_ok = exponent.is_none_or(|e| digits(e.strip_prefix(['+', '-']).unwrap_or(e)));
    if !mantissa_ok || !exponent_ok {
        return f64::NAN;
    }
    t.parse::<f64>().unwrap_or(f64::NAN)
}

/// `Number(v)`.
fn to_number(v: &JValue) -> f64 {
    match v {
        JValue::Num(x) => *x,
        JValue::Bool(b) => f64::from(u8::from(*b)),
        JValue::Null => 0.0,
        JValue::Str(text) => string_to_number(text),
        // `Number([])` is 0 and `Number([x])` is `Number(String(x))`; objects are NaN.
        JValue::Arr(items) => match items.as_slice() {
            [] => 0.0,
            [JValue::Null | JValue::Undefined] => 0.0,
            [one @ (JValue::Num(_) | JValue::Str(_))] => to_number(one),
            [JValue::Arr(inner)] => to_number(&JValue::Arr(inner.clone())),
            _ => f64::NAN,
        },
        JValue::Undefined | JValue::Obj(_) => f64::NAN,
    }
}

/// `/\benable_thinking\b/`: ASCII word boundaries on both sides.
fn names_thinking_switch(template: &str) -> bool {
    const WORD: &str = "enable_thinking";
    let word = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let bytes = template.as_bytes();
    let mut from = 0;
    while let Some(at) = template.get(from..).and_then(|rest| rest.find(WORD)) {
        let start = from + at;
        let end = start + WORD.len();
        let before = start == 0 || bytes.get(start - 1).is_none_or(|b| !word(*b));
        let after = bytes.get(end).is_none_or(|b| !word(*b));
        if before && after {
            return true;
        }
        from = start + 1;
    }
    false
}

/// What a llama-server `/props` body says (each `None` when it does not say).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PropsFacts {
    pub context_tokens: Option<u32>,
    pub thinking: Option<bool>,
    pub vision: Option<bool>,
}

/// core `propsFactsOf(body)`.
pub fn props_facts_of(body: &JValue) -> PropsFacts {
    let n = to_number(body.get("default_generation_settings").get("n_ctx"));
    let context_tokens = (n.fract() == 0.0 && (CONTEXT_MIN..=CONTEXT_MAX).contains(&n))
        .then_some(n as u32);
    let thinking = match body.get("chat_template") {
        JValue::Str(t) if !t.is_empty() => Some(names_thinking_switch(t)),
        _ => None,
    };
    let modalities = body.get("modalities");
    let vision = if modalities.truthy() {
        modalities.get("vision").as_bool()
    } else {
        None
    };
    PropsFacts {
        context_tokens,
        thinking,
        vision,
    }
}

/// core `modelIds(body)`: llama-server `data[].id`, older servers `models[].name|model`; at most
/// [`MAX_MODELS`], each 1..=200 UTF-16 units of non-control text, no repeats.
pub fn model_ids(body: &JValue) -> Vec<String> {
    let rows: &[JValue] = match (body.get("data"), body.get("models")) {
        (JValue::Arr(rows), _) => rows,
        (_, JValue::Arr(rows)) => rows,
        _ => &[],
    };
    let mut out: Vec<String> = Vec::new();
    for r in rows {
        // `typeof r === 'string' ? r : r?.id ?? r?.model ?? r?.name`: `??` passes over null and undefined only.
        let id = match r {
            JValue::Str(_) => r,
            _ => [r.get("id"), r.get("model"), r.get("name")]
                .into_iter()
                .find(|v| !v.is_nullish())
                .unwrap_or(&JValue::Undefined),
        };
        if let JValue::Str(id) = id {
            let clean = !js_trim(id).is_empty()
                && wtf16::len(id) <= 200
                && !id.chars().any(|c| (c as u32) < 0x20 || c as u32 == 0x7f);
            if clean && !out.iter().any(|o| o == id) {
                out.push(id.clone());
            }
        }
        if out.len() >= MAX_MODELS {
            break;
        }
    }
    out
}

/// core `unreachableText(t, detail)`.
pub fn unreachable_text(t: &Target, detail: &str) -> String {
    let role = label(&t.role);
    let detail = if detail.is_empty() {
        String::new()
    } else {
        format!(": {detail}")
    };
    format!(
        "{role} model\u{2019}s machine ({}) is unreachable{detail}. Check that it is on and serving {} on port {}, or move the {role} role back to this machine in Settings \u{2192} Inference machines.",
        t.machine_name, t.model, t.port
    )
}

/// core `wrongModelText(t, served)`.
pub fn wrong_model_text(t: &Target, served: &[String]) -> String {
    let role = label(&t.role);
    let first = served.iter().take(3).cloned().collect::<Vec<_>>().join(", ");
    let first = if first.is_empty() {
        "no model".to_owned()
    } else {
        first
    };
    let more = if served.len() > 3 { " and more" } else { "" };
    format!(
        "The {role} machine ({}) serves {first}{more}, not {}. Start {} on port {}, or choose the model it serves for the {role} role in Settings \u{2192} Inference machines.",
        t.machine_name, t.model, t.model, t.port
    )
}

/// Why a request to a machine failed at the socket (core `connectError`'s three cases, else none).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectWhy {
    /// `EPUBLICADDR`: the on-this-network rule refused the address.
    NotOnThisNetwork,
    /// The machine answered with a redirect.
    Redirect,
    /// `TimeoutError`.
    Timeout,
    Other,
}

/// core `connectError(t, err)`. (`machines::connect_error` has no timeout case; this one has Node's three.)
pub fn connect_error(t: &Target, why: ConnectWhy) -> String {
    unreachable_text(
        t,
        match why {
            ConnectWhy::NotOnThisNetwork => "its address is not on this network",
            ConnectWhy::Redirect => "it answered with a redirect, which is never followed",
            ConnectWhy::Timeout => "no answer in time",
            ConnectWhy::Other => "",
        },
    )
}

/// core `provider(t, { contextTokens, thinking })`: the provider row a turn uses for the role on its machine,
/// keys in Node's order.
pub fn provider_row(t: &Target, context_tokens: Option<u32>, thinking: Option<bool>) -> JValue {
    let mut row = vec![
        ("id".to_owned(), JValue::from(format!("machine:{}", t.machine_id))),
        (
            "label".to_owned(),
            JValue::from(format!(
                "{} model\u{2019}s machine ({})",
                label(&t.role),
                t.machine_name
            )),
        ),
        ("baseUrl".to_owned(), JValue::from(format!("{}/v1", t.base_url))),
        ("apiKey".to_owned(), JValue::from("")),
        ("shared".to_owned(), JValue::Bool(true)),
    ];
    if let Some(ctx) = context_tokens.filter(|c| *c != 0) {
        row.push(("contextTokens".to_owned(), JValue::Num(f64::from(ctx))));
    }
    let mut machine = vec![
        ("id".to_owned(), JValue::from(t.machine_id.as_str())),
        ("name".to_owned(), JValue::from(t.machine_name.as_str())),
        ("role".to_owned(), JValue::from(t.role.as_str())),
        ("port".to_owned(), JValue::Num(f64::from(t.port))),
        ("model".to_owned(), JValue::from(t.model.as_str())),
    ];
    if thinking == Some(true) {
        machine.push(("thinkingSwitch".to_owned(), JValue::Bool(true)));
    }
    row.push(("machine".to_owned(), JValue::Obj(machine)));
    JValue::Obj(row)
}
