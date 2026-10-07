//! The pure part of `insideRoot` in noevia-core `code-sandbox/supervisor.cjs`.
//!
//! The JS resolves both paths with `fs.realpathSync` first; symlinks are a property of the
//! filesystem, so that step stays in JS. What is ported is the decision on the two real paths:
//!
//! ```js
//! const rel = path.relative(resolvedRoot, resolved);
//! if (rel !== '' && (rel.startsWith('..') || path.isAbsolute(rel))) return null;
//! ```
//!
//! with Node's POSIX `path.resolve`/`path.relative` ported literally (byte-wise; `/` and `.` are
//! ASCII, so positions agree with JS's UTF-16 ones on well-formed text). Inputs outside the domain
//! `realpathSync` can return (not absolute, or holding NUL) are refused: not contained.
//! As in JS, a child whose name begins with `..` (`root/..x`) is refused too (fail closed).

/// Node's `normalizeString(path, allowAboveRoot = false, '/')`.
fn normalize(path: &[u8]) -> Vec<u8> {
    let mut res: Vec<u8> = Vec::new();
    let mut last_segment_length: usize = 0;
    let mut last_slash: isize = -1;
    let mut dots: isize = 0;
    let mut code: u8 = 0;
    let len = path.len();
    let mut i = 0usize;
    while i <= len {
        if let Some(&c) = path.get(i) {
            code = c;
        } else if code == b'/' {
            break;
        } else {
            code = b'/';
        }
        if code == b'/' {
            if last_slash == i as isize - 1 || dots == 1 {
                // nothing
            } else if dots == 2 {
                let n = res.len();
                if n < 2
                    || last_segment_length != 2
                    || res.get(n - 1) != Some(&b'.')
                    || res.get(n - 2) != Some(&b'.')
                {
                    if n > 2 {
                        match res.iter().rposition(|&b| b == b'/') {
                            None => {
                                res.clear();
                                last_segment_length = 0;
                            }
                            Some(idx) => {
                                res.truncate(idx);
                                // Node: res.length - 1 - res.lastIndexOf('/')
                                let li = res
                                    .iter()
                                    .rposition(|&b| b == b'/')
                                    .map_or(-1, |p| p as isize);
                                last_segment_length = (res.len() as isize - 1 - li) as usize;
                            }
                        }
                        last_slash = i as isize;
                        dots = 0;
                        i += 1;
                        continue;
                    } else if n != 0 {
                        res.clear();
                        last_segment_length = 0;
                        last_slash = i as isize;
                        dots = 0;
                        i += 1;
                        continue;
                    }
                }
                // allowAboveRoot is false for absolute paths: nothing is appended.
            } else {
                let from = (last_slash + 1) as usize;
                let seg = path.get(from..i).unwrap_or(&[]);
                if !res.is_empty() {
                    res.push(b'/');
                }
                res.extend_from_slice(seg);
                last_segment_length = i - from;
            }
            last_slash = i as isize;
            dots = 0;
        } else if code == b'.' && dots != -1 {
            dots += 1;
        } else {
            dots = -1;
        }
        i += 1;
    }
    res
}

/// Node's `path.posix.resolve(p)` for an absolute `p`.
fn resolve(p: &[u8]) -> Vec<u8> {
    let mut out = vec![b'/'];
    out.extend(normalize(p));
    out
}

/// Node's `path.posix.relative(from, to)` for absolute `from` and `to`.
pub fn relative(from: &str, to: &str) -> String {
    if from == to {
        return String::new();
    }
    let from = resolve(from.as_bytes());
    let to = resolve(to.as_bytes());
    if from == to {
        return String::new();
    }
    let from_start = 1usize;
    let from_end = from.len();
    let from_len = from_end - from_start;
    let to_start = 1usize;
    let to_len = to.len() - to_start;
    let length = from_len.min(to_len);
    let mut last_common_sep: isize = -1;
    let mut i = 0usize;
    while i < length {
        let fc = from.get(from_start + i);
        if fc != to.get(to_start + i) {
            break;
        } else if fc == Some(&b'/') {
            last_common_sep = i as isize;
        }
        i += 1;
    }
    let lossy = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
    if i == length {
        if to_len > length {
            if to.get(to_start + i) == Some(&b'/') {
                return lossy(to.get(to_start + i + 1..).unwrap_or(&[]));
            }
            if i == 0 {
                return lossy(to.get(to_start + i..).unwrap_or(&[]));
            }
        } else if from_len > length {
            if from.get(from_start + i) == Some(&b'/') {
                last_common_sep = i as isize;
            } else if i == 0 {
                last_common_sep = 0;
            }
        }
    }
    let mut out = String::new();
    let mut j = (from_start as isize + last_common_sep + 1) as usize;
    while j <= from_end {
        if j == from_end || from.get(j) == Some(&b'/') {
            out.push_str(if out.is_empty() { ".." } else { "/.." });
        }
        j += 1;
    }
    let tail_from = (to_start as isize + last_common_sep) as usize;
    out.push_str(&lossy(to.get(tail_from..).unwrap_or(&[])));
    out
}

/// Whether `resolved` (a real path) is `resolved_root` or inside it, by supervisor.cjs's rule.
/// Returns the decision and Node's `path.relative` result (for differential tests).
pub fn contained(resolved_root: &str, resolved: &str) -> (bool, Option<String>) {
    let in_domain = |p: &str| p.starts_with('/') && !p.contains('\0');
    if !in_domain(resolved_root) || !in_domain(resolved) {
        return (false, None);
    }
    let rel = relative(resolved_root, resolved);
    let ok = rel.is_empty() || !(rel.starts_with("..") || rel.starts_with('/'));
    (ok, Some(rel))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_relative_examples() {
        assert_eq!(
            relative("/data/orandea/test/aaa", "/data/orandea/impl/bbb"),
            "../../impl/bbb"
        );
        assert_eq!(relative("/w", "/w/a"), "a");
        assert_eq!(relative("/", "/w"), "w");
        assert_eq!(relative("/w/", "/w"), "");
        assert_eq!(relative("/w", "/w/../x"), "../x");
        assert_eq!(relative("/w/a", "/"), "../..");
        assert!(contained("/w", "/w/a/b").0);
        assert!(contained("/w", "/w").0);
        assert!(!contained("/w", "/wx").0);
        assert!(!contained("/w", "/w/..x").0);
        assert!(!contained("/w", "w/a").0);
        assert!(!contained("/w", "/w/a\0").0);
    }
}
