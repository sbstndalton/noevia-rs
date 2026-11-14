//! The runtime limit of a provider-row turn (chat-context.cjs `resolveRuntimeLimit` without the model manager):
//! chat.cjs passes `manager: null` and `hosted: provider` for every row but the default, so the limit is the
//! row's stated `contextTokens` when providers.cjs `validContextTokens` accepts it, else the hosted default.
//! The native turn's frame has no such limit yet (a turn without the manager falls back to the local 8192):
//! a provider-row frame must carry this one (see the crate's CONTINUE).

use js_json::JValue;

/// chat-context.cjs `HOSTED_DEFAULT_LIMIT`.
pub const HOSTED_DEFAULT_LIMIT: u64 = 32_768;
/// providers.cjs `CONTEXT_TOKENS_MIN` / `CONTEXT_TOKENS_MAX`.
pub const CONTEXT_TOKENS_MIN: u64 = 2_048;
pub const CONTEXT_TOKENS_MAX: u64 = 2_000_000;
pub const PROVIDER_SETTING: &str = "Provider setting";
pub const HOSTED_DEFAULT: &str = "Default for hosted providers";
/// chat-context.cjs `runtimeLimit(null, model)`: the local turn without a manager.
pub const LOCAL_FALLBACK: (u64, &str) = (8_192, "Conservative fallback; backend limit unavailable");

/// providers.cjs `validContextTokens`: a JSON number that is a whole number in [MIN, MAX], else `None` (text,
/// fractions, booleans and anything else included).
pub fn valid_context_tokens(v: &JValue) -> Option<u64> {
    let n = v.as_num()?;
    #[allow(clippy::cast_precision_loss)]
    let (min, max) = (CONTEXT_TOKENS_MIN as f64, CONTEXT_TOKENS_MAX as f64);
    if n.is_finite() && n.fract() == 0.0 && (min..=max).contains(&n) {
        // In [2048, 2e6] and whole: exact as an integer.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let whole = n as u64;
        Some(whole)
    } else {
        None
    }
}

/// chat-context.cjs `hostedLimit(provider)`: the limit and its source text.
pub fn hosted_limit(row: &JValue) -> (u64, &'static str) {
    match valid_context_tokens(row.get("contextTokens")) {
        Some(n) => (n, PROVIDER_SETTING),
        None => (HOSTED_DEFAULT_LIMIT, HOSTED_DEFAULT),
    }
}

/// `resolveRuntimeLimit` with no manager (or one turned off): the hosted row's limit, else the local fallback.
pub fn without_manager(hosted: Option<&JValue>) -> (u64, &'static str) {
    hosted.map_or(LOCAL_FALLBACK, hosted_limit)
}
