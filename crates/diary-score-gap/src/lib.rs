//! Pure score-gap leaf from noevia-services diary/agent/score_gap.py.
//!
//! This foundation has no production consumer yet. The host retains config parsing,
//! Python score coercion, entity/time focus, row ownership, and safe trace assembly.
//! Pass scores after focus filtering; named-entity suppression is caller policy.

/// Python uses this tolerance so a score exactly at the configured gap survives.
pub const EPSILON: f64 = 1e-9;

/// Stable zero-based positions into the input, never vector ranks or row contents.
#[derive(Debug, PartialEq)]
pub struct Cut {
    pub kept: Vec<usize>,
    pub removed: Vec<usize>,
    pub best: Option<f64>,
}

/// Partition already-normalized scores without changing their order.
///
/// Missing and nonfinite scores survive. `best` is the greatest finite score,
/// even when `gap` is `None`. The host supplies a parsed gap in `(0, 1]` or
/// `None`, matching Python `parse_gap`; this leaf does not validate configuration.
/// Arithmetic intentionally mirrors Python for any supplied `f64` gap.
/// Storage and time are linear in the number of candidates; no text is needed.
pub fn cut(scores: &[Option<f64>], gap: Option<f64>) -> Cut {
    let best = scores
        .iter()
        .flatten()
        .copied()
        .filter(|s| s.is_finite())
        .reduce(|a, b| if b > a { b } else { a });
    let mut result = Cut {
        kept: Vec::new(),
        removed: Vec::new(),
        best,
    };
    for (position, score) in scores.iter().enumerate() {
        let remove = match (best, gap, score) {
            (Some(best), Some(gap), Some(score)) if score.is_finite() => {
                best - score > gap + EPSILON
            }
            _ => false,
        };
        if remove {
            result.removed.push(position);
        } else {
            result.kept.push(position);
        }
    }
    result
}
