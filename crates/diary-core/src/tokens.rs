//! Token estimate for context budgeting (port of `estimate_tokens` in `diary/agent/util.py`).

/// Characters per token used by the estimate (English prose).
const CHARS_PER_TOKEN: f64 = 3.6;

/// Estimate the tokens in `text`: `0` when empty, otherwise `max(1, trunc(chars / 3.6))`.
///
/// Counts Unicode code points, like Python `len` on `str`. It over-estimates a little and is only
/// used for budgeting, never to truncate text the user can see.
pub fn estimate_tokens(text: &str) -> u64 {
    if text.is_empty() {
        return 0;
    }
    let chars = text.chars().count() as f64;
    ((chars / CHARS_PER_TOKEN) as u64).max(1)
}

#[cfg(test)]
mod tests {
    use super::estimate_tokens;

    #[test]
    fn empty_text_is_zero() {
        assert_eq!(estimate_tokens(""), 0);
    }

    #[test]
    fn short_text_is_at_least_one() {
        assert_eq!(estimate_tokens("a"), 1);
        assert_eq!(estimate_tokens("abc"), 1);
    }

    #[test]
    fn truncates_chars_over_three_point_six() {
        // 36 chars / 3.6 = 10 exactly; 37 chars truncates to 10.
        assert_eq!(estimate_tokens(&"x".repeat(36)), 10);
        assert_eq!(estimate_tokens(&"x".repeat(37)), 10);
        assert_eq!(estimate_tokens(&"x".repeat(38)), 10);
        assert_eq!(estimate_tokens(&"x".repeat(39)), 10);
        assert_eq!(estimate_tokens(&"x".repeat(40)), 11);
    }

    #[test]
    fn counts_code_points_not_bytes() {
        // Four code points, each three bytes in UTF-8: one token, not two.
        assert_eq!(estimate_tokens("日本語文"), 1);
        assert_eq!(estimate_tokens(&"é".repeat(36)), 10);
    }

    #[test]
    fn long_input_does_not_overflow() {
        let long = "y".repeat(1_000_000);
        assert_eq!(estimate_tokens(&long), 277_777);
    }
}
