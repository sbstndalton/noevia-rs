//! Node's `path` module on POSIX, for the paths code-workspace.cjs builds and compares: lexical
//! `resolve`, `relative`, `dirname`, and `fs.realpathSync`. Paths are strings, as in JS; a path
//! that is not UTF-8 on disk is refused where it would have to be read back.

use std::path::Path;

/// `path.resolve(base, p)`: `p` if absolute, else `base/p`, with `.` and `..` folded lexically and
/// no trailing slash (the root stays `/`). `base` is expected to be absolute already.
pub fn resolve(base: &str, p: &str) -> String {
    let start = if p.starts_with('/') {
        String::new()
    } else {
        base.to_string()
    };
    let mut parts: Vec<&str> = Vec::new();
    for part in start.split('/').chain(p.split('/')) {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    format!("/{}", parts.join("/"))
}

/// `path.join(a, b)` for a relative `b`: `resolve` without leaving `a`'s root.
pub fn join(a: &str, b: &str) -> String {
    resolve(a, b)
}

/// `path.dirname(p)` for an absolute, resolved `p`.
pub fn dirname(p: &str) -> String {
    match p.rfind('/') {
        Some(0) | None => "/".to_string(),
        Some(i) => p.get(..i).unwrap_or("/").to_string(),
    }
}

/// `path.relative(from, to)` for two absolute paths.
pub fn relative(from: &str, to: &str) -> String {
    let from = resolve("/", from);
    let to = resolve("/", to);
    if from == to {
        return String::new();
    }
    let a: Vec<&str> = from.split('/').filter(|s| !s.is_empty()).collect();
    let b: Vec<&str> = to.split('/').filter(|s| !s.is_empty()).collect();
    let common = a.iter().zip(b.iter()).take_while(|(x, y)| x == y).count();
    let mut out: Vec<&str> = vec![".."; a.len().saturating_sub(common)];
    out.extend(b.iter().skip(common));
    out.join("/")
}

/// `fs.realpathSync(p)`, or `None` when it throws (missing, a loop, not UTF-8).
pub fn realpath(p: &str) -> Option<String> {
    if p.is_empty() {
        return None;
    }
    std::fs::canonicalize(Path::new(p))
        .ok()
        .and_then(|r| r.into_os_string().into_string().ok())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn node_path_semantics() {
        assert_eq!(resolve("/a/b", "c/../d"), "/a/b/d");
        assert_eq!(resolve("/a/b", "/x/./y/"), "/x/y");
        assert_eq!(resolve("/a/b", "../../.."), "/");
        assert_eq!(resolve("/a/b", ""), "/a/b");
        assert_eq!(dirname("/a/b"), "/a");
        assert_eq!(dirname("/a"), "/");
        assert_eq!(relative("/a/b", "/a/b/c/d"), "c/d");
        assert_eq!(relative("/a/b/c", "/a/x"), "../../x");
        assert_eq!(relative("/a", "/a"), "");
        assert_eq!(relative("/", "/a"), "a");
    }
}
