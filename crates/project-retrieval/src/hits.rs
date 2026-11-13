//! The post-SQL half of `searchProjectNow` (`rag.cjs:912-919`) and the version rule
//! `isCurrentVersion` (`rag.cjs:866-871`). The cosine SQL itself is not here (R4-2).

/// `MIN_SCORE` (`rag.cjs:104`).
pub const MIN_SCORE: f64 = 0.3;
/// `TOP_K` (`rag.cjs:99`).
pub const TOP_K: usize = 6;

/// One row as the SQL returns it, before filtering.
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub file: String,
    pub body: String,
    /// `1.0 - vec_distance_cosine(...)`, unrounded.
    pub score: f64,
    /// `c.kind`; `None` is SQL NULL.
    pub kind: Option<String>,
    /// `c.version`; `None` is SQL NULL.
    pub version: Option<String>,
}

/// A kept hit. `score` is rounded to three decimals.
#[derive(Clone, Debug, PartialEq)]
pub struct Hit {
    pub file: String,
    pub body: String,
    pub score: f64,
}

/// `Math.round(x * 1000) / 1000` for finite values.
pub fn round3(score: f64) -> f64 {
    (score * 1000.0 + 0.5).floor() / 1000.0
}

/// `isCurrentVersion`: `entry` is the file's `fileVersions` slot. No slot means every
/// version is current; a slot holding null means none is; otherwise the version must match.
pub fn is_current_version(entry: Option<&Option<String>>, version: Option<&str>) -> bool {
    match entry {
        None => true,
        Some(None) => false,
        Some(Some(want)) => version == Some(want.as_str()),
    }
}

/// The text side of a search with no reranker. Keeps rows of kind text (or NULL) with
/// `score >= min_score`, orders them by score (descending), keeps `limit * 4` of them as the
/// SQL `LIMIT` does, then applies `is_current` and keeps `limit`.
///
/// Known difference: SQLite leaves the order of equal scores undefined; this keeps input order.
pub fn select_hits(
    rows: &[Row],
    is_current: impl Fn(&str, Option<&str>) -> bool,
    min_score: f64,
    limit: usize,
) -> Vec<Hit> {
    let mut eligible: Vec<&Row> = rows
        .iter()
        .filter(|r| r.score >= min_score && r.kind.as_deref().is_none_or(|k| k == "text"))
        .collect();
    eligible.sort_by(|a, b| b.score.total_cmp(&a.score));
    eligible
        .into_iter()
        .take(limit.saturating_mul(4))
        .filter(|r| is_current(&r.file, r.version.as_deref()))
        .take(limit)
        .map(|r| Hit {
            file: r.file.clone(),
            body: r.body.clone(),
            score: round3(r.score),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{is_current_version, round3, select_hits, Row};

    fn row(file: &str, score: f64, kind: Option<&str>, version: Option<&str>) -> Row {
        Row {
            file: file.to_owned(),
            body: format!("body of {file}"),
            score,
            kind: kind.map(str::to_owned),
            version: version.map(str::to_owned),
        }
    }

    #[test]
    fn rounding_is_js_math_round() {
        assert_eq!(round3(0.12345), 0.123);
        assert_eq!(round3(0.2), 0.2);
        assert_eq!(round3(0.5), 0.5);
        assert_eq!(round3(0.3), 0.3);
    }

    #[test]
    fn version_rule() {
        assert!(is_current_version(None, Some("v1")));
        assert!(is_current_version(None, None));
        assert!(!is_current_version(Some(&None), Some("v1")));
        assert!(is_current_version(Some(&Some("v2".into())), Some("v2")));
        assert!(!is_current_version(Some(&Some("v2".into())), Some("v1")));
        assert!(!is_current_version(Some(&Some("v2".into())), None));
    }

    #[test]
    fn filters_orders_limits_and_rounds() {
        let rows = vec![
            row("a.md", 0.2999, None, Some("v1")),
            row("b.md", 0.9, Some("text"), Some("v1")),
            row("c.md", 0.95, Some("image"), Some("v1")),
            row("d.md", 0.61234, None, Some("v1")),
            row("e.md", 0.7, Some("text"), Some("old")),
            row("f.md", 0.5, None, Some("v1")),
        ];
        let current = |_: &str, v: Option<&str>| v == Some("v1");
        let hits = select_hits(&rows, current, 0.3, 2);
        let names: Vec<_> = hits.iter().map(|h| (h.file.as_str(), h.score)).collect();
        assert_eq!(names, vec![("b.md", 0.9), ("d.md", 0.612)]);
    }

    #[test]
    fn limit_applies_before_version_filter() {
        // limit 1 keeps 4 candidates before the version filter, so the stale top row
        // leaves no hit even though a current row sits at position 5.
        let mut rows = vec![row("stale.md", 0.9, None, Some("old"))];
        for i in 0..4 {
            rows.push(row(
                &format!("x{i}.md"),
                0.8 - f64::from(i) * 0.01,
                None,
                Some("old"),
            ));
        }
        rows.push(row("now.md", 0.5, None, Some("v1")));
        let hits = select_hits(&rows, |_, v| v == Some("v1"), 0.3, 1);
        assert!(hits.is_empty());
    }
}
