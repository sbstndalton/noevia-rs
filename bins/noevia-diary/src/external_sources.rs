//! GET /api/external-sources, ported from services diary/agent/external_sources.py (`scan_source`,
//! `infer_date`) and app.py `api_external_sources`: a read-only listing of diary-like files in the
//! operator-configured folders (`DIARY_EXTERNAL_SOURCES`). Python resolves the configured paths
//! (its normalisation, `~` expansion and de-duplication) and sends them at start-up, so this side
//! lists exactly what Python would.
//!
//! Anything this side cannot reproduce byte for byte is DECLINED (Python then serves it): a listing
//! longer than the 500-file cap (which files survive depends on directory order), a name that is
//! not UTF-8 or that contains a non-ASCII digit (`\d`/`\D` in Python's `re` are Unicode-aware),
//! a directory whose iteration fails part-way (`os.walk` stops everything there), a path whose
//! `stat` fails with an error Python raises, or a timestamp that cannot be mapped to a local date.
//!
//! `datetime.fromtimestamp(mtime)` rounds a float to microseconds; here the whole second is used.
//! The two differ only for a modification time within a microsecond of local midnight.

use crate::workspace::Reply;
use chrono::{Datelike, NaiveDate, TimeZone};
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

pub const MAX_FILES_PER_SOURCE: usize = 500;

#[derive(Serialize)]
struct FileItem {
    name: String,
    rel_path: String,
    size: u64,
    date: Option<String>,
    date_source: &'static str,
}

#[derive(Serialize)]
struct Source {
    path: String,
    exists: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<&'static str>,
    files: Vec<FileItem>,
    total: usize,
    truncated: bool,
}

#[derive(Serialize)]
struct Listing {
    configured: bool,
    sources: Vec<Source>,
    total: usize,
}

/// `Path(name).suffix.lower()` is one of .txt/.md/.markdown (PurePath.suffix: the part from the
/// last dot, unless the dot starts the name or ends it).
fn allowed_suffix(name: &str) -> bool {
    let Some(i) = name.rfind('.') else {
        return false;
    };
    if i == 0 || i + 1 >= name.len() {
        return false;
    }
    let suffix = name.get(i..).unwrap_or("").to_lowercase();
    matches!(suffix.as_str(), ".txt" | ".md" | ".markdown")
}

/// How many ASCII digits start at `from`, at most `max`.
fn digit_run(c: &[char], from: usize, max: usize) -> usize {
    c.iter()
        .skip(from)
        .take(max)
        .take_while(|ch| ch.is_ascii_digit())
        .count()
}

fn num(c: &[char], from: usize, len: usize) -> Option<u32> {
    let s: String = c.get(from..from.checked_add(len)?)?.iter().collect();
    s.parse().ok()
}

/// `(?:^|\D)(\d{4})-(\d{1,2})-(\d{1,2})(?:\D|$)`, first match. Candidates are tried by increasing
/// start of the year; a digit before it rules the candidate out. After the month's first digits the
/// next character must be `-`, and after the day's the next must not be a digit, so greedy
/// matching never needs to backtrack into a shorter run.
fn search_dashed(c: &[char]) -> Option<(u32, u32, u32)> {
    for d in 0..c.len() {
        if d > 0 && c.get(d - 1).is_some_and(char::is_ascii_digit) {
            continue;
        }
        if digit_run(c, d, 4) != 4 || c.get(d + 4) != Some(&'-') {
            continue;
        }
        let m = d + 5;
        let mn = digit_run(c, m, 2);
        if mn == 0 || c.get(m + mn) != Some(&'-') {
            continue;
        }
        let dd = m + mn + 1;
        let dn = digit_run(c, dd, 2);
        if dn == 0 || c.get(dd + dn).is_some_and(char::is_ascii_digit) {
            continue;
        }
        return Some((num(c, d, 4)?, num(c, m, mn)?, num(c, dd, dn)?));
    }
    None
}

/// `(?:^|\D)(\d{4})(\d{2})(\d{2})(?:\D|$)`, first match.
fn search_compact(c: &[char]) -> Option<(u32, u32, u32)> {
    for d in 0..c.len() {
        if d > 0 && c.get(d - 1).is_some_and(char::is_ascii_digit) {
            continue;
        }
        if digit_run(c, d, 8) != 8 || c.get(d + 8).is_some_and(char::is_ascii_digit) {
            continue;
        }
        return Some((num(c, d, 4)?, num(c, d + 4, 2)?, num(c, d + 6, 2)?));
    }
    None
}

/// `infer_date` for the file name alone: a date from the filename patterns in order (a match that
/// is not a real calendar date falls through to the next pattern), else None.
fn filename_date(name: &str) -> Option<NaiveDate> {
    let c: Vec<char> = name.chars().collect();
    for (y, m, d) in [search_dashed(&c), search_compact(&c)]
        .into_iter()
        .flatten()
    {
        // datetime(year, month, day): year 1..=9999.
        let date = i32::try_from(y)
            .ok()
            .and_then(|y| NaiveDate::from_ymd_opt(y, m, d))
            .filter(|date| date.year() >= 1);
        if date.is_some() {
            return date;
        }
    }
    None
}

/// OSError numbers `Path.exists`/`is_dir` treat as "no such path": ENOENT, ENOTDIR, EBADF, ELOOP.
fn ignored(e: &std::io::Error) -> bool {
    matches!(e.raw_os_error(), Some(2 | 9 | 20 | 40))
}

/// None: decline.
fn mtime_date(meta: &fs::Metadata) -> Option<(Option<NaiveDate>, &'static str)> {
    let Ok(modified) = meta.modified() else {
        return Some((None, "unavailable"));
    };
    let secs = match modified.duration_since(UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_secs()).ok()?,
        Err(_) => return None,
    };
    let local = chrono::Local.timestamp_opt(secs, 0).single()?;
    Some((Some(local.date_naive()), "mtime"))
}

enum Walk {
    Done(Vec<FileItem>),
    Decline,
}

fn walk(root: &Path) -> Walk {
    let mut files: Vec<FileItem> = Vec::new();
    // (directory, its path relative to the root, "/"-joined)
    let mut stack: Vec<(PathBuf, String)> = vec![(root.to_path_buf(), String::new())];
    while let Some((dir, rel)) = stack.pop() {
        // os.walk ignores a directory it cannot list.
        let Ok(read) = fs::read_dir(&dir) else {
            continue;
        };
        let mut subdirs: Vec<(PathBuf, String)> = Vec::new();
        for entry in read {
            let Ok(entry) = entry else {
                return Walk::Decline;
            };
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                return Walk::Decline;
            };
            if name.chars().any(|c| !c.is_ascii() && c.is_numeric()) {
                return Walk::Decline;
            }
            let path = entry.path();
            let Ok(kind) = entry.file_type() else {
                return Walk::Decline;
            };
            // DirEntry.is_dir() follows symlinks; an error counts as "not a directory".
            let is_dir = if kind.is_symlink() {
                fs::metadata(&path).map(|m| m.is_dir()).unwrap_or(false)
            } else {
                kind.is_dir()
            };
            if is_dir {
                // followlinks=False: a symlink to a directory is neither listed nor entered.
                if !kind.is_symlink() {
                    let child = if rel.is_empty() {
                        name
                    } else {
                        format!("{rel}/{name}")
                    };
                    subdirs.push((path, child));
                }
                continue;
            }
            if !allowed_suffix(&name) {
                continue;
            }
            if files.len() >= MAX_FILES_PER_SOURCE {
                // Python truncates here, keeping whichever files its directory order met first.
                return Walk::Decline;
            }
            // A file that vanished mid-scan is skipped.
            let Ok(meta) = fs::metadata(&path) else {
                continue;
            };
            let (date, date_source) = match filename_date(&name) {
                Some(d) => (Some(d), "filename"),
                None => match mtime_date(&meta) {
                    Some(x) => x,
                    None => return Walk::Decline,
                },
            };
            files.push(FileItem {
                rel_path: if rel.is_empty() {
                    name.clone()
                } else {
                    format!("{rel}/{name}")
                },
                name,
                size: meta.len(),
                date: date.map(|d| d.format("%Y-%m-%d").to_string()),
                date_source,
            });
        }
        stack.extend(subdirs);
    }
    files.sort_by(|a, b| {
        (
            a.date.is_none(),
            a.date.as_deref().unwrap_or(""),
            &a.rel_path,
        )
            .cmp(&(
                b.date.is_none(),
                b.date.as_deref().unwrap_or(""),
                &b.rel_path,
            ))
    });
    Walk::Done(files)
}

/// `scan_source`; None: decline.
fn scan(source: &str) -> Option<Source> {
    let root = Path::new(source);
    let meta = match fs::metadata(root) {
        Ok(m) => m,
        Err(e) if ignored(&e) => {
            return Some(Source {
                path: source.to_string(),
                exists: false,
                error: None,
                files: Vec::new(),
                total: 0,
                truncated: false,
            })
        }
        Err(_) => return None,
    };
    if !meta.is_dir() {
        return Some(Source {
            path: source.to_string(),
            exists: true,
            error: Some("not a directory"),
            files: Vec::new(),
            total: 0,
            truncated: false,
        });
    }
    match walk(root) {
        Walk::Decline => None,
        Walk::Done(files) => Some(Source {
            path: source.to_string(),
            exists: true,
            error: None,
            total: files.len(),
            files,
            truncated: false,
        }),
    }
}

/// The reply of `api_external_sources` once auth and the tenant are resolved. `paths` are the
/// configured sources exactly as Python's `external_source_paths()` returns them.
pub fn list(paths: &[String]) -> Reply {
    if paths.is_empty() {
        return json(&Listing {
            configured: false,
            sources: Vec::new(),
            total: 0,
        });
    }
    let mut sources = Vec::new();
    let mut total = 0usize;
    for p in paths {
        let Some(s) = scan(p) else {
            return Reply::Decline;
        };
        total += s.total;
        sources.push(s);
    }
    json(&Listing {
        configured: true,
        sources,
        total,
    })
}

fn json(v: &Listing) -> Reply {
    match serde_json::to_vec(v) {
        Ok(body) => Reply::Json(200, body),
        Err(_) => Reply::Decline,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
    use super::*;

    fn d(s: &str) -> Option<String> {
        filename_date(s).map(|d| d.format("%Y-%m-%d").to_string())
    }

    #[test]
    fn filename_patterns_match_the_python_regexes() {
        assert_eq!(d("2026-09-06 notes.md").as_deref(), Some("2026-09-06"));
        assert_eq!(d("x2026-9-6y.txt").as_deref(), Some("2026-09-06"));
        assert_eq!(d("journal_20260906.md").as_deref(), Some("2026-09-06"));
        // a digit on either side rules the candidate out
        assert_eq!(d("12026-09-06.md"), None);
        assert_eq!(d("2026-09-061.md"), None);
        assert_eq!(d("120260906.md"), None);
        assert_eq!(d("202609061.md"), None);
        // dashed pattern first; an impossible date falls through to the compact one
        assert_eq!(d("2026-13-45 20260906.md").as_deref(), Some("2026-09-06"));
        assert_eq!(d("2026-02-30.md"), None);
        // year 0000 is not a datetime
        assert_eq!(d("0000-01-01.md"), None);
        // first match only: a bad first dashed date does not try a later dashed one
        assert_eq!(d("2026-02-30 2026-03-04.md"), None);
        assert_eq!(d("2026-02-30 notes.md"), None);
    }

    #[test]
    fn suffix_rule_is_pathlib_suffix() {
        assert!(allowed_suffix("a.md"));
        assert!(allowed_suffix("a.MD"));
        assert!(allowed_suffix("a.b.markdown"));
        assert!(allowed_suffix("a.marKdown")); // KELVIN SIGN lower-cases to k
        assert!(!allowed_suffix(".md"));
        assert!(!allowed_suffix("a.md."));
        assert!(!allowed_suffix("md"));
        assert!(!allowed_suffix("a.mdx"));
    }

    #[test]
    fn missing_file_and_empty_configuration() {
        let Reply::Json(200, body) = list(&[]) else {
            panic!("expected json");
        };
        assert_eq!(
            String::from_utf8(body).unwrap(),
            r#"{"configured":false,"sources":[],"total":0}"#
        );
        let Reply::Json(200, body) = list(&["/definitely/not/here".to_string()]) else {
            panic!("expected json");
        };
        assert_eq!(
            String::from_utf8(body).unwrap(),
            r#"{"configured":true,"sources":[{"path":"/definitely/not/here","exists":false,"files":[],"total":0,"truncated":false}],"total":0}"#
        );
    }
}
