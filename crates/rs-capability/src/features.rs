//! `deriveFeatures(dockerfile, cargo)`: the feature set of the shipped binary, or `None` when the build or the
//! manifest is outside the supported shapes. Cargo defaults and transitive local features are expanded only when
//! declared in the snapshot; no feature is ever invented.

use std::collections::BTreeSet;

use crate::{dockerfile, js};

/// Byte offsets where JavaScript's multiline `^` matches: the start and after every line terminator.
fn line_starts(text: &str) -> impl Iterator<Item = usize> + '_ {
    std::iter::once(0).chain(
        text.char_indices()
            .filter(|(_, c)| js::is_line_terminator(*c))
            .map(|(i, c)| i + c.len_utf8()),
    )
}

/// End offsets of the matches of `/^\[features\]\s*$/gm` (greedy `\s*`, so a match ends at the last line end inside
/// the whitespace run that follows the header).
fn feature_headers(cargo: &str) -> Vec<usize> {
    const HEADER: &str = "[features]";
    let mut ends = Vec::new();
    for start in line_starts(cargo) {
        if !cargo.get(start..).is_some_and(|r| r.starts_with(HEADER)) {
            continue;
        }
        let after = start + HEADER.len();
        let rest = cargo.get(after..).unwrap_or("");
        let run = rest
            .char_indices()
            .find(|(_, c)| !js::is_space(*c))
            .map_or(rest.len(), |(i, _)| i);
        let end = if run == rest.len() {
            Some(cargo.len())
        } else {
            rest.get(..run)
                .and_then(|ws| {
                    ws.char_indices()
                        .filter(|(_, c)| js::is_line_terminator(*c))
                        .map(|(i, _)| i)
                        .next_back()
                })
                .map(|i| after + i)
        };
        ends.extend(end);
    }
    ends
}

/// TOML comments outside basic strings removed (`/"(?:\\.|[^"\\])*"|#[^\n]*/g`).
fn strip_comments(table: &str) -> String {
    let chars: Vec<char> = table.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while let Some(&c) = chars.get(i) {
        if c == '"' {
            let mut j = i + 1;
            let closed = loop {
                match chars.get(j) {
                    None => break None,
                    Some('"') => break Some(j),
                    Some('\\')
                        if chars
                            .get(j + 1)
                            .is_some_and(|n| !js::is_line_terminator(*n)) =>
                    {
                        j += 2
                    }
                    Some('\\') => break None,
                    Some(_) => j += 1,
                }
            };
            if let Some(close) = closed {
                out.extend(chars.get(i..=close).unwrap_or_default());
                i = close + 1;
                continue;
            }
        } else if c == '#' {
            while chars.get(i).is_some_and(|&x| x != '\n') {
                i += 1;
            }
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

/// The `[features]` table as an ordered graph, or `None` for any manifest shape outside the supported subset.
fn feature_graph(cargo: &str) -> Option<Vec<(String, Vec<String>)>> {
    let headers = feature_headers(cargo);
    let [end] = headers.as_slice() else {
        return None;
    };
    let tail = cargo.get(*end..)?;
    let table_end = line_starts(tail)
        .find(|&s| tail.get(s..).is_some_and(|r| r.starts_with('[')))
        .unwrap_or(tail.len());
    let clean = strip_comments(tail.get(..table_end)?);
    let entry = js_re!(r"^\s*([A-Za-z0-9_-]+)\s*=\s*\[([^\]]*)\]\s*").ok()?;
    let mut graph: Vec<(String, Vec<String>)> = Vec::new();
    let mut rest = clean.as_str();
    while !js::trim(rest).is_empty() {
        let m = entry.captures(rest)?;
        let name = m.get(1)?.as_str();
        if graph.iter().any(|(n, _)| n == name) {
            return None;
        }
        let list = m.get(2)?.as_str();
        let list = match js::trim_end(list).strip_suffix(',') {
            Some(without) => without,
            None => list,
        };
        let values: Vec<serde_json::Value> = serde_json::from_str(&format!("[{list}]")).ok()?;
        let values = values
            .into_iter()
            .map(|v| v.as_str().map(str::to_string))
            .collect::<Option<Vec<_>>>()?;
        graph.push((name.to_string(), values));
        rest = rest.get(m.get(0)?.end()..)?;
    }
    Some(graph)
}

/// Recognize the production build's restricted feature-selection program, without executing shell code. Any changed
/// or unsupported build or manifest syntax is unknown (`None`).
pub fn derive_features(build: &str, cargo: &str) -> Option<BTreeSet<String>> {
    let graph = feature_graph(cargo)?;
    let proof = dockerfile::prove_features(build, cargo).ok()?;
    let has = |name: &str| graph.iter().any(|(n, _)| n == name);
    let mut enabled: BTreeSet<String> = proof.features.iter().cloned().collect();
    if !proof.no_default && has("default") {
        enabled.insert("default".to_string());
    }
    let mut visited = BTreeSet::new();
    let mut stack: Vec<String> = enabled.iter().cloned().collect();
    while let Some(name) = stack.pop() {
        if !visited.insert(name.clone()) {
            continue;
        }
        let (_, deps) = graph.iter().find(|(n, _)| *n == name)?;
        enabled.insert(name);
        stack.extend(
            deps.iter()
                .filter(|d| !d.starts_with("dep:") && !d.contains('/'))
                .cloned(),
        );
    }
    Some(enabled)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_and_table_follow_multiline_regex_semantics() {
        assert_eq!(feature_headers("[features]  \n\n\na = []").len(), 1);
        assert!(feature_headers("[features] x\n").is_empty());
        assert_eq!(feature_headers("x\n[features]").first(), Some(&12));
        let graph = feature_graph("[features]\n# c\na = [\"b\", ] # tail\nb = []\n[deps]\nz = 1\n");
        assert_eq!(graph.map(|g| g.len()), Some(2));
        assert!(feature_graph("[features]\na = [broken]").is_none());
        assert!(feature_graph("[features]\na = []\na = []").is_none());
        assert_eq!(strip_comments("a = [\"x#y\"] # z\n"), "a = [\"x#y\"] \n");
    }
}
