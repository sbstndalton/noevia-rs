//! Which routes in `contracts/http/routes.toml` a corpus exercises.

/// Reads the `path = "..."` values of a routes.toml (the only key this tool needs; the file is
/// generated, one `[[route]]` table per path).
pub fn paths(toml: &str) -> Vec<String> {
    toml.lines()
        .filter_map(|l| {
            let l = l.trim();
            let rest = l
                .strip_prefix("path")?
                .trim_start()
                .strip_prefix('=')?
                .trim();
            let inner = rest.strip_prefix('"')?.strip_suffix('"')?;
            Some(inner.to_string())
        })
        .collect()
}

fn route_segments(route: &str) -> Vec<String> {
    // `{*?}` is an optional suffix or query on the last segment, not a segment of its own.
    let route = route.replace("{*?}", "");
    route
        .split('/')
        .map(|s| {
            if s.starts_with('{') && s.ends_with('}') {
                "{}".to_string()
            } else {
                s.to_string()
            }
        })
        .collect()
}

/// True when a recorded path (placeholders, literal ids) can be an instance of the route.
pub fn covers(route: &str, recorded: &str) -> bool {
    let r = route_segments(route);
    let p: Vec<&str> = recorded.trim_end_matches('/').split('/').collect();
    let r_len = r.len();
    let trimmed_r: Vec<&String> = if r.last().is_some_and(|s| s.is_empty()) && r_len > 1 {
        r.iter().take(r_len - 1).collect()
    } else {
        r.iter().collect()
    };
    trimmed_r.len() == p.len()
        && trimmed_r
            .iter()
            .zip(p.iter())
            .all(|(rs, ps)| rs.as_str() == "{}" && !ps.is_empty() || rs.as_str() == *ps)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_paths_and_matches_shapes() {
        let t = "# c\n[[route]]\npath = \"/api/projects/{id}/files\"\nowner = \"node\"\n\n[[route]]\npath = \"/api/providers/{providerId}/models{*?}\"\n";
        assert_eq!(
            paths(t),
            vec![
                "/api/projects/{id}/files",
                "/api/providers/{providerId}/models{*?}"
            ]
        );
        assert!(covers(
            "/api/projects/{id}/files",
            "/api/projects/<id:3>/files"
        ));
        assert!(covers(
            "/api/providers/{providerId}/models{*?}",
            "/api/providers/default/models"
        ));
        assert!(!covers("/api/projects/{id}/files", "/api/projects/<id:3>"));
        assert!(!covers("/api/projects", "/api/projects/<id:3>"));
        assert!(covers("/api/projects", "/api/projects"));
    }
}
