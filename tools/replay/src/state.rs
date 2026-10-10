//! A comparable snapshot of a server's data directory after a replay: the file tree (paths only)
//! and, when `sqlite3` is on PATH, every table of `cowork.db`, with bound values rewritten to their
//! placeholders and run-specific values (timestamps, hashes, random tokens) masked.

use crate::at;
use crate::bind::Bindings;
use serde_json::{json, Map, Value};
use std::path::Path;
use std::process::Command;

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
    json!({ "files": files, "db": db })
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
        if ignore.iter().any(|i| {
            rel == *i
                || rel.starts_with(&format!("{i}/"))
                || (i.starts_with('*') && rel.ends_with(i.trim_start_matches('*')))
        }) {
            continue;
        }
        let is_dir = path.is_dir();
        out.push(format!(
            "{}{}",
            b.unbind(&rel),
            if is_dir { "/" } else { "" }
        ));
        if is_dir {
            walk(root, &path, ignore, b, out);
        }
    }
}

fn sqlite_json(db: &Path, sql: &str) -> Result<Value, String> {
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
}
