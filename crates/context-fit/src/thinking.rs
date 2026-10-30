//! core thinking-budget.cjs's numbers (#1475, #1477). The stream wrapper there (`capReasoning`) cuts a
//! reply's reasoning and asks again; it belongs with the chat turn's retry and is not here.

/// core `THINKING_CAP_TOKENS`.
pub const LOW: u64 = 512;
pub const DEFAULT: u64 = 2048;
pub const HIGH: u64 = 8192;
/// core `MIN_ANSWER_TOKENS`.
pub const MIN_ANSWER_TOKENS: u64 = 1024;
/// core `OUTPUT_CAP_MAX`.
pub const OUTPUT_CAP_MAX: u64 = 8192;

/// core `outputBudget(limit)`: a quarter of the context, at most 8,192.
pub fn output_budget(limit: u64) -> u64 {
    OUTPUT_CAP_MAX.min((limit as f64 * 0.25).floor() as u64)
}

/// core `thinkingCap(effort, budget)`: an unknown effort is the default.
pub fn thinking_cap(effort: &str, budget: u64) -> u64 {
    let cap = match effort {
        "low" => LOW,
        "high" => HIGH,
        _ => DEFAULT,
    };
    cap.min(budget.saturating_sub(MIN_ANSWER_TOKENS))
}
