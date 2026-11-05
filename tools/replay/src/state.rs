//! A comparable snapshot of a server's data directory after a replay: the file tree (paths only)
//! and, when `sqlite3` is on PATH, every table of `cowork.db`, with bound values rewritten to their
//! placeholders and run-specific values (timestamps, hashes, random tokens) masked.

use crate::at;
use crate::bind::Bindings;
use serde_json::{json, Map, Value};
use std::path::Path;
use std::process::Command;

/// Diffs two snapshots. Both are already masked, so `<origin>` stands for itself (it was bound
/// to each run's own origin when the snapshot was taken).
pub fn compare(expected: &Value, actual: &Value) -> Vec<crate::diff::Diff> {
    let mut diffs = Vec::new();
    crate::diff::json(
        expected,
        actual,
        "state",
        &mut Bindings::new("<origin>"),
        &mut diffs,
    );
    // A length mismatch in the file tree says nothing about which path differs; name them. Only a
    // comparison that already failed gets this note, so it never changes the verdict (placeholders
    // such as `<id:1>` match by binding, not literally).
    if diffs.is_empty() {
        return diffs;
    }
    if let (Some(Value::Array(want)), Some(Value::Array(got))) =
        (expected.get("files"), actual.get("files"))
    {
        let names = |v: &[Value]| -> std::collections::BTreeSet<String> {
            v.iter()
                .filter_map(|f| f.as_str().map(str::to_owned))
                .collect()
        };
        let (want, got) = (names(want), names(got));
        let missing: Vec<&String> = want.difference(&got).collect();
        let extra: Vec<&String> = got.difference(&want).collect();
        if !missing.is_empty() || !extra.is_empty() {
            diffs.push(crate::diff::Diff {
                at: "state/files (paths)".to_owned(),
                expected: format!("missing {missing:?}"),
                actual: format!("extra {extra:?}"),
            });
        }
    }
    diffs
}

/// Whether an entry-only ignore (a pattern ending in `/`) matches a raw directory path written with
/// its trailing `/`: exactly, or as a `*suffix`. walk() and drop_dir_entries() share this predicate.
fn entry_only_matches(dir: &str, pattern: &str) -> bool {
    dir == pattern || (pattern.starts_with('*') && dir.ends_with(pattern.trim_start_matches('*')))
}

/// A normalized snapshot path written back with this replay's bound values, or None when it holds a
/// placeholder that cannot be reconstructed (unbound, or a masked `<ts>`-like value).
fn raw_path(path: &str, b: &Bindings) -> Option<String> {
    let mut out = String::new();
    for tok in crate::bind::tokens(path) {
        match tok {
            crate::bind::Tok::Lit(s) => out.push_str(&s),
            crate::bind::Tok::Var(v) => out.push_str(b.get(&v)?),
            crate::bind::Tok::Origin => out.push_str(b.origin()),
            crate::bind::Tok::Any => return None,
        }
    }
    Some(out)
}

/// Applies the directory-entry-only ignores (patterns ending in `/`) to a snapshot read from a file,
/// so an `--expect-state` reference recorded without them is projected the same way as the live
/// snapshot. Each reference directory entry is written back in raw form with this replay's bindings
/// and tested with walk()'s own predicate, so both sides match raw paths; an entry that cannot be
/// reconstructed is kept. Only directory entries are dropped; every path inside them stays.
pub fn drop_dir_entries(snapshot: &mut Value, ignore: &[String], b: &Bindings) {
    let entry_only: Vec<&String> = ignore.iter().filter(|i| i.ends_with('/')).collect();
    if entry_only.is_empty() {
        return;
    }
    if let Some(Value::Array(files)) = snapshot.get_mut("files") {
        files.retain(|f| {
            let Some(path) = f.as_str() else { return true };
            if !path.ends_with('/') {
                return true;
            }
            let Some(raw) = raw_path(path, b) else {
                return true;
            };
            !entry_only.iter().any(|i| entry_only_matches(&raw, i))
        });
    }
}

pub fn snapshot(dir: &Path, db_name: &str, ignore: &[String], b: &Bindings) -> Value {
    let mut files = Vec::new();
    walk(dir, dir, ignore, b, &mut files);
    files.sort();
    let db_path = dir.join(db_name);
    let db = if db_path.is_file() {
        database(&db_path, b)
    } else {
        Value::Null
    };
    // Routing configuration requires content equality, not merely a matching filename. Capture
    // only this fixed normalized account file; never read provider/account private stores.
    let mut routing = Map::new();
    if let Ok(users) = std::fs::read_dir(dir.join("users")) {
        for user in users.filter_map(Result::ok) {
            let path = user.path().join("routing-mode.json");
            if std::fs::symlink_metadata(&path).is_ok() {
                use std::io::Read;
                let value = (|| -> Result<String, ()> {
                    let fd = rustix::fs::open(
                        &path,
                        rustix::fs::OFlags::RDONLY
                            | rustix::fs::OFlags::NOFOLLOW
                            | rustix::fs::OFlags::NONBLOCK,
                        rustix::fs::Mode::empty(),
                    )
                    .map_err(|_| ())?;
                    let stat = rustix::fs::fstat(&fd).map_err(|_| ())?;
                    if rustix::fs::FileType::from_raw_mode(stat.st_mode)
                        != rustix::fs::FileType::RegularFile
                        || stat.st_nlink != 1
                    {
                        return Err(());
                    }
                    let mut bytes = Vec::new();
                    std::fs::File::from(fd)
                        .take(1025)
                        .read_to_end(&mut bytes)
                        .map_err(|_| ())?;
                    if bytes.len() > 1024 {
                        return Err(());
                    }
                    String::from_utf8(bytes).map_err(|_| ())
                })();
                if let Ok(rel) = path.strip_prefix(dir) {
                    routing.insert(
                        b.unbind(&rel.to_string_lossy()),
                        match value {
                            Ok(text) => Value::String(text),
                            Err(()) => json!({"error":"routing configuration unavailable"}),
                        },
                    );
                }
            }
        }
    }
    json!({ "files": files, "db": db, "routingModeFiles": routing })
}

fn walk(root: &Path, dir: &Path, ignore: &[String], b: &Bindings, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.filter_map(Result::ok) {
        let path = e.path();
        let Ok(rel) = path.strip_prefix(root) else {
            continue;
        };
        let rel = rel.to_string_lossy().replace('\\', "/");
        if ignore.iter().filter(|i| !i.ends_with('/')).any(|i| {
            rel == *i
                || rel.starts_with(&format!("{i}/"))
                || (i.starts_with('*') && rel.ends_with(i.trim_start_matches('*')))
        }) {
            continue;
        }
        let is_dir = path.is_dir();
        // A pattern ending in `/` drops only a directory's own entry: whether that directory
        // exists is not compared, but everything inside it still is.
        let entry_only = is_dir && {
            let dir = format!("{rel}/");
            ignore
                .iter()
                .filter(|i| i.ends_with('/'))
                .any(|i| entry_only_matches(&dir, i))
        };
        if !entry_only {
            out.push(format!(
                "{}{}",
                b.unbind(&rel),
                if is_dir { "/" } else { "" }
            ));
        }
        if is_dir {
            walk(root, &path, ignore, b, out);
        }
    }
}

pub(crate) fn sqlite_json(db: &Path, sql: &str) -> Result<Value, String> {
    let out = Command::new("sqlite3")
        .arg("-readonly")
        .arg("-json")
        .arg(db)
        .arg(sql)
        .output()
        .map_err(|e| format!("sqlite3: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "sqlite3: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    if text.trim().is_empty() {
        return Ok(Value::Array(Vec::new()));
    }
    serde_json::from_str(&text).map_err(|e| format!("sqlite3 output: {e}"))
}

fn database(db: &Path, b: &Bindings) -> Value {
    let tables = match sqlite_json(db, "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name") {
        Ok(Value::Array(rows)) => rows,
        Ok(_) => Vec::new(),
        Err(e) => return json!({ "error": e }),
    };
    let mut out = Map::new();
    for t in tables {
        let Some(name) = at(&t, "name").as_str() else {
            continue;
        };
        let quoted = name.replace('"', "\"\"");
        let rows = match sqlite_json(db, &format!("SELECT * FROM \"{quoted}\"")) {
            Ok(Value::Array(rows)) => rows,
            Ok(_) => Vec::new(),
            Err(e) => vec![json!({ "error": e })],
        };
        let mut rows: Vec<Value> = rows.into_iter().map(|r| mask(r, "", b)).collect();
        rows.sort_by_key(|r| r.to_string());
        out.insert(name.to_string(), Value::Array(rows));
    }
    Value::Object(out)
}

fn time_column(col: &str) -> bool {
    let c = col.to_ascii_lowercase();
    c.ends_with("_at")
        || col.ends_with("At")
        || c.ends_with("_ms")
        || c.ends_with("_time")
        || matches!(c.as_str(), "expires" | "ts")
}

fn opaque(s: &str) -> bool {
    if s.starts_with("$argon2") || s.starts_with("$scrypt") {
        return true;
    }
    s.len() >= 16
        && !s.contains(char::is_whitespace)
        && s.chars().any(|c| c.is_ascii_digit())
        && s.chars().any(|c| c.is_ascii_alphabetic())
        && !s.contains('<')
        && !s.starts_with('{')
        && !s.starts_with('[')
}

/// Masks one value from a table cell (JSON text cells are parsed and masked recursively).
pub fn mask(v: Value, col: &str, b: &Bindings) -> Value {
    match v {
        Value::Number(n) if time_column(col) && n.as_f64().is_some_and(|f| f > 1e9) => {
            Value::String("<ts>".into())
        }
        Value::String(s) => {
            if let Ok(inner @ (Value::Object(_) | Value::Array(_))) =
                serde_json::from_str::<Value>(&s)
            {
                return mask(inner, col, b);
            }
            let u = b.unbind(&s);
            if time_column(col) && !u.contains('<') {
                return Value::String("<ts>".into());
            }
            if opaque(&u) {
                Value::String("<opaque>".into())
            } else {
                Value::String(u)
            }
        }
        Value::Object(m) => Value::Object(
            m.into_iter()
                .map(|(k, v)| {
                    let masked = mask(v, &k, b);
                    (b.unbind(&k), masked)
                })
                .collect(),
        ),
        Value::Array(a) => Value::Array(a.into_iter().map(|v| mask(v, col, b)).collect()),
        other => other,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn snapshots_compare_placeholders_literally() {
        let a = json!({"settings": [{"key": "public_origin", "value": "<origin>"}], "u": "<id:1>"});
        assert!(compare(&a, &a.clone()).is_empty());
        let b = json!({"settings": [{"key": "public_origin", "value": "http://x"}], "u": "<id:1>"});
        assert_eq!(compare(&a, &b).len(), 1);
    }

    #[test]
    fn masks_times_hashes_and_bound_values() {
        let mut b = Bindings::new("");
        b.bind("<id:1>", "3f2c1a9e-8b7d-4c6e-9f10-1234567890ab");
        let row = json!({"id": "3f2c1a9e-8b7d-4c6e-9f10-1234567890ab", "created_at": 1760000000000_u64, "password_hash": "$argon2id$v=19$m=1,t=2,p=1$abc", "token_hash": "a1b2c3d4e5f6a7b8c9d0", "name": "Synthetic", "count": 3, "meta": "{\"updatedAt\":1760000000001,\"x\":\"y\"}"});
        assert_eq!(
            mask(row, "", &b),
            json!({"id": "<id:1>", "created_at": "<ts>", "password_hash": "<opaque>", "token_hash": "<opaque>", "name": "Synthetic", "count": 3, "meta": {"updatedAt": "<ts>", "x": "y"}})
        );
    }

    #[test]
    fn walks_the_tree_with_ignores_and_placeholders() {
        let dir = std::env::temp_dir().join(format!("replay-state-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("projects/p-77777777")).unwrap();
        std::fs::write(dir.join("projects/p-77777777/a.md"), "x").unwrap();
        std::fs::write(dir.join("cowork.db-wal"), "x").unwrap();
        std::fs::write(dir.join("keep.json"), "{}").unwrap();
        let mut b = Bindings::new("");
        b.bind("<id:2>", "p-77777777");
        let s = snapshot(&dir, "cowork.db", &["*-wal".to_string()], &b);
        assert_eq!(
            at(&s, "files"),
            &json!([
                "keep.json",
                "projects/",
                "projects/<id:2>/",
                "projects/<id:2>/a.md"
            ])
        );
        assert_eq!(at(&s, "db"), &Value::Null);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_path_note_never_fails_a_matching_comparison() {
        let expected = json!({"files": ["users/<id:1>/"]});
        assert!(compare(&expected, &json!({"files": ["users/alice/"]})).is_empty());
        let diffs = compare(&expected, &json!({"files": ["users/alice/", "extra.json"]}));
        assert!(diffs
            .iter()
            .any(|d| d.at == "state/files (paths)" && d.actual.contains("extra.json")));
    }

    #[test]
    fn a_reference_without_the_directory_ignore_is_projected_the_same_way() {
        let ignore = vec!["*/rag/".to_string()];
        let mut expected = json!({"files": ["users/", "users/<id:1>/", "users/<id:1>/rag/"]});
        // The replay bound <id:1> (the live snapshot's own placeholder comes from the same bindings).
        let mut none = Bindings::new("");
        none.bind("<id:1>", "alice");
        drop_dir_entries(&mut expected, &ignore, &none);
        assert!(compare(&expected, &json!({"files": ["users/", "users/alice/"]})).is_empty());
        // A database inside the directory is still compared: missing it on either side fails.
        let mut with_db = json!({"files": ["users/", "users/<id:1>/", "users/<id:1>/rag/", "users/<id:1>/rag/p.db"]});
        drop_dir_entries(&mut with_db, &ignore, &none);
        assert_eq!(
            with_db,
            json!({"files": ["users/", "users/<id:1>/", "users/<id:1>/rag/p.db"]})
        );
        assert!(!compare(&with_db, &json!({"files": ["users/", "users/alice/"]})).is_empty());
        // Exact, short and overlapping bound values: the reference is matched in raw form.
        let mut bound = Bindings::new("");
        bound.bind("<id:1>", "bob");
        let mut reference =
            json!({"files": ["users/<id:1>/rag/", "users/<id:1>/rag/p.db", "users/<id:2>/rag/"]});
        drop_dir_entries(&mut reference, &["users/bob/rag/".to_string()], &bound);
        assert_eq!(
            reference,
            json!({"files": ["users/<id:1>/rag/p.db", "users/<id:2>/rag/"]})
        );
        let mut overlap = Bindings::new("");
        overlap.bind("<id:1>", "alice");
        overlap.bind("<id:2>", "alice/rag");
        let mut reference =
            json!({"files": ["users/<id:1>/", "users/<id:2>/", "users/<id:2>/p.db"]});
        drop_dir_entries(&mut reference, &ignore, &overlap);
        assert_eq!(
            reference,
            json!({"files": ["users/<id:1>/", "users/<id:2>/p.db"]})
        );
        // A placeholder written in a pattern is literal: it matches no raw path on either side.
        let mut alice = Bindings::new("");
        alice.bind("<id:1>", "alice");
        let mut reference = json!({"files": ["users/<id:1>/rag/"]});
        // An unbound placeholder cannot be written back, so its entry is kept (fail closed).
        let mut unbound = json!({"files": ["users/<id:9>/rag/"]});
        drop_dir_entries(&mut unbound, &ignore, &alice);
        assert_eq!(unbound, json!({"files": ["users/<id:9>/rag/"]}));
        drop_dir_entries(&mut reference, &["users/<id:1>/rag/".to_string()], &alice);
        assert_eq!(reference, json!({"files": ["users/<id:1>/rag/"]}));
    }

    #[test]
    fn a_trailing_slash_ignore_drops_only_the_directory_entry() {
        let dir = std::env::temp_dir().join(format!("replay-state-dir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("users/u/rag")).unwrap();
        std::fs::create_dir_all(dir.join("users/v/rag")).unwrap();
        std::fs::write(dir.join("users/v/rag/p.db"), "x").unwrap();
        let b = Bindings::new("");
        let s = snapshot(&dir, "cowork.db", &["*/rag/".to_string()], &b);
        // Both rag directories' own entries are dropped, but a file inside one is still listed.
        assert_eq!(
            at(&s, "files"),
            &json!(["users/", "users/u/", "users/v/", "users/v/rag/p.db"])
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_placeholder_pattern_keeps_the_live_entry_too() {
        let dir = std::env::temp_dir().join(format!("replay-state-ph-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("users/alice/rag")).unwrap();
        let mut b = Bindings::new("");
        b.bind("<id:1>", "alice");
        let s = snapshot(&dir, "cowork.db", &["users/<id:1>/rag/".to_string()], &b);
        assert_eq!(
            at(&s, "files"),
            &json!(["users/", "users/<id:1>/", "users/<id:1>/rag/"])
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
