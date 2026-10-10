//! Node's `atomicJson` (core server/workspace.cjs) and the inline write-then-rename copies in
//! account-preferences.cjs, chat-lists.cjs and evidence.cjs:
//!
//! ```js
//! fs.mkdirSync(path.dirname(file), { recursive: true, mode: 0o700 });
//! const tmp = `${file}.${process.pid}.tmp`;
//! fs.writeFileSync(tmp, JSON.stringify(value, null, 2), { mode: 0o600 });
//! fs.renameSync(tmp, file);
//! ```
//!
//! Same directory mode, temp name, file mode (on create), truncation and rename, and the same
//! bytes as `JSON.stringify` ([`Format::Pretty`] = `(value, null, 2)`, [`Format::Compact`] =
//! `(value)`). Differences, all stricter: the temp file is fsynced before the rename, writers in
//! this process are serialised (Node is single-threaded, threads here would share the temp
//! name), and numbers `JSON.stringify` would print in exponent form (|x| < 1e-6 or >= 1e21) are
//! refused rather than formatted. Object keys are written in the `serde_json::Map` order.

use serde_json::Value;
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::Path;
use std::sync::Mutex;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// `JSON.stringify(value, null, 2)` (atomicJson).
    Pretty,
    /// `JSON.stringify(value)` (account-preferences.cjs, chat-lists.cjs).
    Compact,
}

#[derive(Debug)]
pub enum JsonError {
    /// The file is not in [`crate::OWNED_JSON_FILES`] or is not a plain file name.
    NotOwned(String),
    /// A number `JSON.stringify` would write in exponent form.
    Unsupported,
    Io(std::io::Error),
}

impl std::fmt::Display for JsonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JsonError::NotOwned(n) => write!(f, "Rust does not own {n}"),
            JsonError::Unsupported => write!(f, "number out of the supported range"),
            JsonError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for JsonError {}

impl From<std::io::Error> for JsonError {
    fn from(e: std::io::Error) -> Self {
        JsonError::Io(e)
    }
}

static WRITE_LOCK: Mutex<()> = Mutex::new(());

/// `JSON.stringify` of `value`, see the module docs.
pub fn stringify(value: &Value, format: Format) -> Result<String, JsonError> {
    let mut out = String::new();
    emit(&mut out, value, format, 0)?;
    Ok(out)
}

fn number(out: &mut String, n: &serde_json::Number) -> Result<(), JsonError> {
    if let Some(i) = n.as_i64() {
        let _ = write!(out, "{i}");
        return Ok(());
    }
    if let Some(u) = n.as_u64() {
        let _ = write!(out, "{u}");
        return Ok(());
    }
    let f = n.as_f64().ok_or(JsonError::Unsupported)?;
    if !f.is_finite() {
        // JSON.stringify writes null; serde_json cannot hold these anyway.
        out.push_str("null");
        return Ok(());
    }
    if f == 0.0 {
        out.push('0'); // -0 too, as JS
        return Ok(());
    }
    let a = f.abs();
    if !(1e-6..1e21).contains(&a) {
        return Err(JsonError::Unsupported);
    }
    // Rust's Display is the shortest round-trip decimal, which is what JS prints in this range.
    let _ = write!(out, "{f}");
    Ok(())
}

fn indent(out: &mut String, depth: usize) {
    out.push('\n');
    for _ in 0..depth {
        out.push_str("  ");
    }
}

fn emit(out: &mut String, value: &Value, format: Format, depth: usize) -> Result<(), JsonError> {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => number(out, n)?,
        Value::String(s) => out.push_str(&quote(s)),
        Value::Array(items) => {
            if items.is_empty() {
                out.push_str("[]");
                return Ok(());
            }
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                if format == Format::Pretty {
                    indent(out, depth + 1);
                }
                emit(out, item, format, depth + 1)?;
            }
            if format == Format::Pretty {
                indent(out, depth);
            }
            out.push(']');
        }
        Value::Object(map) => {
            if map.is_empty() {
                out.push_str("{}");
                return Ok(());
            }
            out.push('{');
            for (i, (k, v)) in map.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                if format == Format::Pretty {
                    indent(out, depth + 1);
                }
                out.push_str(&quote(k));
                out.push(':');
                if format == Format::Pretty {
                    out.push(' ');
                }
                emit(out, v, format, depth + 1)?;
            }
            if format == Format::Pretty {
                indent(out, depth);
            }
            out.push('}');
        }
    }
    Ok(())
}

/// JSON.stringify's string quoting: `"` `\` and the C0 controls (`\b \t \n \f \r`, else
/// `\u00xx` lowercase); everything else, including U+2028/9 and DEL, as is.
fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\u{c}' => out.push_str("\\f"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The write itself; callers outside this crate go through [`write_owned`].
pub(crate) fn write_atomic(file: &Path, value: &Value, format: Format) -> Result<(), JsonError> {
    let text = stringify(value, format)?;
    let _guard = WRITE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(dir) = file.parent().filter(|d| !d.as_os_str().is_empty()) {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(dir)?;
    }
    let mut tmp = file.as_os_str().to_owned();
    tmp.push(format!(".{}.tmp", std::process::id()));
    let tmp = std::path::PathBuf::from(tmp);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| -> std::io::Result<()> {
        let mut f = options.open(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
        std::fs::rename(&tmp, file)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    Ok(result?)
}

/// Writes `dir/name` atomically, only when `name` is in [`crate::OWNED_JSON_FILES`] (empty in
/// M2, so this always refuses) and is a plain file name.
pub fn write_owned(dir: &Path, name: &str, value: &Value, format: Format) -> Result<(), JsonError> {
    write_owned_from(crate::OWNED_JSON_FILES, dir, name, value, format)
}

fn write_owned_from(
    owned: &[&str],
    dir: &Path,
    name: &str,
    value: &Value,
    format: Format,
) -> Result<(), JsonError> {
    let plain =
        !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\', '\0']);
    if !plain || !owned.contains(&name) {
        return Err(JsonError::NotOwned(name.chars().take(64).collect()));
    }
    write_atomic(&dir.join(name), value, format)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn stringify_matches_js_shapes() {
        let v = json!({"a": 1, "b": [true, null, "x\u{1}\n\"\\\u{2028}\u{7f}é"], "c": {}, "d": [], "e": {"f": -0.0, "g": 0.5, "h": 1234.5}});
        assert_eq!(
            stringify(&v, Format::Compact).unwrap(),
            "{\"a\":1,\"b\":[true,null,\"x\\u0001\\n\\\"\\\\\u{2028}\u{7f}é\"],\"c\":{},\"d\":[],\"e\":{\"f\":0,\"g\":0.5,\"h\":1234.5}}"
        );
        assert_eq!(
            stringify(&json!({"a": [1, {"b": 2}], "c": {}}), Format::Pretty).unwrap(),
            "{\n  \"a\": [\n    1,\n    {\n      \"b\": 2\n    }\n  ],\n  \"c\": {}\n}"
        );
        assert!(matches!(
            stringify(&json!(1e-7), Format::Compact),
            Err(JsonError::Unsupported)
        ));
        assert!(matches!(
            stringify(&json!(1e21), Format::Compact),
            Err(JsonError::Unsupported)
        ));
        assert_eq!(
            stringify(&json!(1e20), Format::Compact).unwrap(),
            "100000000000000000000"
        );
    }

    #[test]
    fn writes_atomically_with_node_modes() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("nested/deeper/prefs.json");
        write_atomic(&file, &json!({"k": "v"}), Format::Pretty).unwrap();
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "{\n  \"k\": \"v\"\n}"
        );
        // Overwrite replaces whole; no temp file is left behind.
        write_atomic(&file, &json!([1]), Format::Compact).unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "[1]");
        let names: Vec<_> = std::fs::read_dir(file.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names, vec!["prefs.json".to_string()]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&file), 0o600);
            assert_eq!(mode(&dir.path().join("nested")), 0o700);
            assert_eq!(mode(&dir.path().join("nested/deeper")), 0o700);
        }
    }

    #[test]
    fn a_failed_write_keeps_the_old_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.json");
        write_atomic(&file, &json!({"old": true}), Format::Compact).unwrap();
        assert!(write_atomic(&file, &json!(1e-9), Format::Compact).is_err());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "{\"old\":true}");
        // The rename target is a directory: the write fails and no temp file is left.
        let target = dir.path().join("d.json");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("x"), "x").unwrap();
        assert!(write_atomic(&target, &json!(1), Format::Compact).is_err());
        let left: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(left.is_empty(), "{left:?}");
    }

    #[test]
    fn only_owned_plain_names_are_written() {
        let dir = tempfile::tempdir().unwrap();
        // M2: nothing is owned.
        assert!(matches!(
            write_owned(dir.path(), "preferences.json", &json!({}), Format::Pretty),
            Err(JsonError::NotOwned(_))
        ));
        let owned = ["mine.json"];
        for bad in ["../mine.json", "a/mine.json", "", ".", "..", "theirs.json"] {
            assert!(
                write_owned_from(&owned, dir.path(), bad, &json!(1), Format::Compact).is_err(),
                "{bad}"
            );
        }
        write_owned_from(&owned, dir.path(), "mine.json", &json!(1), Format::Compact).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("mine.json")).unwrap(),
            "1"
        );
    }
}
