//! Placeholders in a recorded exchange and the values a replay binds them to.
//!
//! The recorder (noevia-core `server/contract-record.cjs`) writes `<secret:N>` and `<id:N>` for a
//! value it may not or should not keep, numbered by first sight across the whole recording, so the
//! same value carries the same placeholder in every exchange. `<ts>` stands for any timestamp and
//! `<origin>` for the server's own origin. Replaying, the first time an expected response holds an
//! unbound placeholder it is bound to what the server under test answered there; later requests
//! that carry the placeholder send that value, and later responses must repeat it.

use std::collections::BTreeMap;

/// One piece of a template string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tok {
    /// Text that must match exactly.
    Lit(String),
    /// `<secret:N>` or `<id:N>`: bound on first match, compared after.
    Var(String),
    /// `<ts>`: any non-empty run, never bound.
    Any,
    /// `<origin>`: the base URL's origin.
    Origin,
}

/// Splits a recorded string into literal text and placeholders.
pub fn tokens(s: &str) -> Vec<Tok> {
    let mut out = Vec::new();
    let mut lit = String::new();
    let mut rest = s;
    while !rest.is_empty() {
        if let Some((tok, len)) = placeholder_at(rest) {
            if !lit.is_empty() {
                out.push(Tok::Lit(std::mem::take(&mut lit)));
            }
            out.push(tok);
            rest = rest.get(len..).unwrap_or("");
            continue;
        }
        let mut chars = rest.chars();
        if let Some(c) = chars.next() {
            lit.push(c);
        }
        rest = chars.as_str();
    }
    if !lit.is_empty() {
        out.push(Tok::Lit(lit));
    }
    out
}

fn placeholder_at(s: &str) -> Option<(Tok, usize)> {
    if !s.starts_with('<') {
        return None;
    }
    let end = s.find('>')?;
    let inner = s.get(1..end)?;
    let tok = match inner {
        "ts" => Tok::Any,
        "origin" => Tok::Origin,
        _ => {
            let (kind, n) = inner.split_once(':')?;
            if !matches!(kind, "secret" | "id")
                || n.is_empty()
                || !n.bytes().all(|b| b.is_ascii_digit())
            {
                return None;
            }
            Tok::Var(format!("<{inner}>"))
        }
    };
    Some((tok, end + 1))
}

/// True when the string holds any placeholder.
pub fn has_placeholder(s: &str) -> bool {
    tokens(s).iter().any(|t| !matches!(t, Tok::Lit(_)))
}

/// Values bound during one replay.
#[derive(Debug, Clone, Default)]
pub struct Bindings {
    vars: BTreeMap<String, String>,
    origin: String,
}

impl Bindings {
    pub fn new(origin: &str) -> Self {
        Self {
            vars: BTreeMap::new(),
            origin: origin.trim_end_matches('/').to_string(),
        }
    }

    pub fn origin(&self) -> &str {
        &self.origin
    }

    pub fn get(&self, var: &str) -> Option<&str> {
        self.vars.get(var).map(String::as_str)
    }

    /// Binds `var` unless it is already bound; returns false on a conflicting value.
    pub fn bind(&mut self, var: &str, value: &str) -> bool {
        match self.vars.get(var) {
            Some(v) => v == value,
            None => {
                self.vars.insert(var.to_string(), value.to_string());
                true
            }
        }
    }

    pub fn len(&self) -> usize {
        self.vars.len()
    }

    pub fn is_empty(&self) -> bool {
        self.vars.is_empty()
    }

    /// Fills a template for sending. `<ts>` becomes `now`; an unbound variable is an error naming it.
    pub fn fill(&self, template: &str, now: &str) -> Result<String, String> {
        let mut out = String::new();
        for tok in tokens(template) {
            match tok {
                Tok::Lit(s) => out.push_str(&s),
                Tok::Any => out.push_str(now),
                Tok::Origin => out.push_str(&self.origin),
                Tok::Var(v) => match self.vars.get(&v) {
                    Some(val) => out.push_str(val),
                    None => return Err(v),
                },
            }
        }
        Ok(out)
    }

    /// Matches `actual` against a recorded template, binding new variables on success only.
    pub fn matches(&mut self, template: &str, actual: &str) -> bool {
        let toks = tokens(template);
        let mut trial = self.vars.clone();
        if match_toks(&toks, actual, &mut trial, &self.origin) {
            self.vars = trial;
            true
        } else {
            false
        }
    }

    /// Rewrites bound values in `s` back to their placeholders (longest value first), so a report
    /// or a state snapshot taken on one server compares with one taken on another.
    pub fn unbind(&self, s: &str) -> String {
        let mut pairs: Vec<(&String, &String)> =
            self.vars.iter().filter(|(_, v)| v.len() >= 4).collect();
        pairs.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then_with(|| a.0.cmp(b.0)));
        let mut out = s.to_string();
        for (var, val) in pairs {
            if out.contains(val.as_str()) {
                out = out.replace(val.as_str(), var);
            }
        }
        if !self.origin.is_empty() && out.contains(&self.origin) {
            out = out.replace(&self.origin, "<origin>");
        }
        out
    }
}

fn match_toks(toks: &[Tok], s: &str, vars: &mut BTreeMap<String, String>, origin: &str) -> bool {
    let Some((first, rest)) = toks.split_first() else {
        return s.is_empty();
    };
    match first {
        Tok::Lit(lit) => s
            .strip_prefix(lit.as_str())
            .is_some_and(|tail| match_toks(rest, tail, vars, origin)),
        Tok::Origin => s
            .strip_prefix(origin)
            .is_some_and(|tail| match_toks(rest, tail, vars, origin)),
        Tok::Var(v) => {
            if let Some(bound) = vars.get(v).cloned() {
                return s
                    .strip_prefix(bound.as_str())
                    .is_some_and(|tail| match_toks(rest, tail, vars, origin));
            }
            // Shortest non-empty capture first, then longer, so `a=<secret:1>; b=<secret:2>` splits
            // at the literal between them.
            for (end, _) in s
                .char_indices()
                .skip(1)
                .chain(std::iter::once((s.len(), ' ')))
                .filter(|(end, _)| *end > 0)
            {
                let (Some(head), Some(tail)) = (s.get(..end), s.get(end..)) else {
                    continue;
                };
                let mut trial = vars.clone();
                trial.insert(v.clone(), head.to_string());
                if match_toks(rest, tail, &mut trial, origin) {
                    *vars = trial;
                    return true;
                }
            }
            false
        }
        Tok::Any => {
            for (end, _) in s
                .char_indices()
                .skip(1)
                .chain(std::iter::once((s.len(), ' ')))
                .filter(|(end, _)| *end > 0)
            {
                let Some(tail) = s.get(end..) else { continue };
                if match_toks(rest, tail, vars, origin) {
                    return true;
                }
            }
            false
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn tokenises_placeholders_and_leaves_lookalikes_literal() {
        assert_eq!(
            tokens("a<id:1>b<ts><origin><secret:22>"),
            vec![
                Tok::Lit("a".into()),
                Tok::Var("<id:1>".into()),
                Tok::Lit("b".into()),
                Tok::Any,
                Tok::Origin,
                Tok::Var("<secret:22>".into())
            ]
        );
        assert_eq!(
            tokens("<id:x> <b> <secret:>"),
            vec![Tok::Lit("<id:x> <b> <secret:>".into())]
        );
        assert!(!has_placeholder("<html>"));
    }

    #[test]
    fn binds_on_first_match_and_holds_the_value_after() {
        let mut b = Bindings::new("http://127.0.0.1:1");
        assert!(b.matches(
            "cowork_session=<secret:1>; Path=/; HttpOnly",
            "cowork_session=abc; Path=/; HttpOnly"
        ));
        assert_eq!(b.get("<secret:1>"), Some("abc"));
        assert!(b.matches("<secret:1>", "abc"));
        assert!(!b.matches("<secret:1>", "abd"));
        assert!(
            !b.matches("x=<id:2>; y", "x=1; z"),
            "a failed match binds nothing"
        );
        assert_eq!(b.get("<id:2>"), None);
    }

    #[test]
    fn splits_adjacent_variables_at_the_literal_between_them() {
        let mut b = Bindings::new("");
        assert!(b.matches("a=<secret:1>; b=<secret:2>", "a=xx; b=yy"));
        assert_eq!(
            (b.get("<secret:1>"), b.get("<secret:2>")),
            (Some("xx"), Some("yy"))
        );
    }

    #[test]
    fn timestamps_match_anything_non_empty_and_origin_is_the_base() {
        let mut b = Bindings::new("http://h:9/");
        assert!(b.matches("updated <ts> ok", "updated 2026-10-09T00:00:00Z ok"));
        assert!(!b.matches("<ts>", ""));
        assert!(b.matches("<origin>/x", "http://h:9/x"));
        assert!(!b.matches("<origin>/x", "http://evil/x"));
    }

    #[test]
    fn fill_reports_the_unbound_variable() {
        let mut b = Bindings::new("http://h");
        assert_eq!(
            b.fill("/api/projects/<id:3>", "T"),
            Err("<id:3>".to_string())
        );
        assert!(b.bind("<id:3>", "p1"));
        assert!(!b.bind("<id:3>", "p2"));
        assert_eq!(
            b.fill("<origin>/api/projects/<id:3>?at=<ts>", "T").unwrap(),
            "http://h/api/projects/p1?at=T"
        );
    }

    #[test]
    fn unbind_rewrites_longest_values_first() {
        let mut b = Bindings::new("http://h");
        b.bind("<id:1>", "abcd");
        b.bind("<id:2>", "abcdef");
        assert_eq!(
            b.unbind("x abcdef y abcd http://h/z"),
            "x <id:2> y <id:1> <origin>/z"
        );
    }
}
