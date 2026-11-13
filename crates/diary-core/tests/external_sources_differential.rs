//! Differential test: `external-sources-fixtures.json` (recorded from the frozen Python module
//! `diary/agent/external_sources.py` by `generate_external_sources_fixtures.py`) replayed on this
//! port: `infer_date`, the mtime rounding, the Unicode digit table, `scan_source` and
//! `resolve_import_target` on trees rebuilt in a temporary directory from the recorded spec, and
//! `external_source_paths` with the recorded HOME and passwd answers.
//!
//! Every recorded mtime is noon UTC of a fixed day, so the dates agree in any timezone from UTC-11 to
//! UTC+11 (run with `TZ=Etc/GMT+11` or `TZ=Etc/GMT-11` to check); beyond that the local date moves.
#![cfg(unix)]
#![allow(clippy::indexing_slicing)] // serde_json `Value[...]` on fixture data

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, UNIX_EPOCH};

use diary_core::external_sources::{
    decimal_digit_value, external_source_paths, external_source_paths_with,
    fromtimestamp_epoch_secs, infer_date, resolve_import_target, scan_source, ExternalSourcesError,
    HomeOf,
};
use serde_json::Value;

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn fixtures() -> Result<Value, Box<dyn std::error::Error>> {
    Ok(serde_json::from_str(include_str!(
        "external-sources-fixtures.json"
    ))?)
}

/// A fresh canonical temporary directory, removed on drop.
struct TempBase(PathBuf);

impl TempBase {
    fn new(tag: &str) -> Result<Self, Box<dyn std::error::Error>> {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "r9-ext-{tag}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        fs::create_dir_all(&dir)?;
        Ok(Self(dir.canonicalize()?))
    }
    fn text(&self) -> String {
        self.0.to_string_lossy().into_owned()
    }
}

impl Drop for TempBase {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn set_mtime(path: &Path, secs: i64) -> std::io::Result<()> {
    let delta = Duration::from_secs(secs.unsigned_abs());
    let time = if secs >= 0 {
        UNIX_EPOCH + delta
    } else {
        UNIX_EPOCH - delta
    };
    fs::File::options()
        .write(true)
        .open(path)?
        .set_modified(time)
}

/// The mtime the filesystem actually stored, in whole seconds since the epoch (floor).
fn stored_mtime(path: &Path) -> std::io::Result<i64> {
    use std::os::unix::fs::MetadataExt;
    fs::metadata(path).map(|m| m.mtime())
}

/// Some filesystems cannot store every recorded mtime: ext4 (the CI runners' temporary directory)
/// clamps anything before 1901-12-13T20:45:52Z (`i32::MIN` seconds) to that instant. A case whose
/// mtime lies outside the signed 32-bit range and did not survive the round trip says nothing
/// about the port, so it is skipped; any other mismatch is a real failure.
fn mtime_unstorable(path: &Path, secs: i64) -> Result<bool, Box<dyn std::error::Error>> {
    let stored = stored_mtime(path)?;
    if stored == secs {
        return Ok(false);
    }
    let outside_i32 = secs < i64::from(i32::MIN) || secs > i64::from(i32::MAX);
    assert!(
        outside_i32,
        "{}: mtime {secs} stored as {stored}",
        path.display()
    );
    Ok(true)
}

fn str_at(v: &Value, i: usize) -> Result<&str, Box<dyn std::error::Error>> {
    Ok(v[i].as_str().ok_or("string")?)
}

fn hex_bytes(hex: &str) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    (0..hex.len())
        .step_by(2)
        .map(|i| Ok(u8::from_str_radix(hex.get(i..i + 2).ok_or("hex")?, 16)?))
        .collect()
}

/// The recorded spec with its `gen` / `genlink` shorthands expanded.
fn expand(nodes: &Value) -> Result<Vec<Vec<Value>>, Box<dyn std::error::Error>> {
    let mut out = Vec::new();
    for n in nodes.as_array().ok_or("nodes")? {
        let kind = str_at(n, 0)?;
        let join = |dir: &str, name: String| {
            if dir.is_empty() {
                name
            } else {
                format!("{dir}/{name}")
            }
        };
        match kind {
            "gen" => {
                let dir = str_at(n, 1)?;
                let (prefix, suffix) = (str_at(n, 2)?, str_at(n, 3)?);
                let count = n[4].as_u64().ok_or("count")?;
                let width = usize::try_from(n[5].as_u64().ok_or("width")?)?;
                let (size, mt) = (n[6].clone(), n[7].as_i64().ok_or("mtime")?);
                for i in 0..count {
                    let name = format!("{prefix}{i:0width$}{suffix}");
                    let day = i64::try_from(i % 9)? * 86_400;
                    out.push(vec![
                        Value::from("file"),
                        Value::from(join(dir, name)),
                        size.clone(),
                        Value::from(mt + day),
                    ]);
                }
            }
            "genlink" => {
                let dir = str_at(n, 1)?;
                let (prefix, suffix) = (str_at(n, 2)?, str_at(n, 3)?);
                let count = n[4].as_u64().ok_or("count")?;
                let width = usize::try_from(n[5].as_u64().ok_or("width")?)?;
                for i in 0..count {
                    let name = format!("{prefix}{i:0width$}{suffix}");
                    out.push(vec![
                        Value::from("symlink"),
                        Value::from(join(dir, name)),
                        n[6].clone(),
                    ]);
                }
            }
            _ => out.push(n.as_array().ok_or("node")?.clone()),
        }
    }
    Ok(out)
}

fn build_tree(base: &TempBase, nodes: &Value) -> TestResult {
    let base_text = base.text();
    for n in expand(nodes)? {
        let kind = n[0].as_str().ok_or("kind")?;
        let raw = n[1].as_str().ok_or("path")?;
        let rel: Vec<u8> = if kind.ends_with("_b") {
            hex_bytes(raw)?
        } else {
            raw.as_bytes().to_vec()
        };
        let path = base.0.join(OsStr::from_bytes(&rel));
        match kind {
            "dir" | "dir_b" => fs::create_dir_all(&path)?,
            "file" | "file_b" => {
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(
                    &path,
                    vec![b'x'; usize::try_from(n[2].as_u64().ok_or("size")?)?],
                )?;
                set_mtime(&path, n[3].as_i64().ok_or("mtime")?)?;
            }
            "symlink" => {
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent)?;
                }
                let target = n[2].as_str().ok_or("target")?.replace("{base}", &base_text);
                symlink(target, &path)?;
            }
            other => return Err(format!("unknown node {other}").into()),
        }
    }
    Ok(())
}

/// The Python exception a recorded `{"error": "<Class>"}` stands for (a scan result of a path that is
/// not a directory also has an `error` key, but alongside `exists` and `files`).
fn error_name(expected: &Value) -> Option<&str> {
    let object = expected.as_object()?;
    if object.len() == 1 {
        object.get("error").and_then(Value::as_str)
    } else {
        None
    }
}

#[test]
fn infer_date_matches_python() -> TestResult {
    let fx = fixtures()?;
    let cases = fx["infer_date"].as_array().ok_or("infer_date")?;
    assert!(cases.len() >= 150, "{} infer_date cases", cases.len());
    let base = TempBase::new("infer")?;
    let mut unstorable = 0usize;
    for (i, case) in cases.iter().enumerate() {
        let name = case["name"].as_str().ok_or("name")?;
        let label = format!("{name:?}");
        let path = if case["create"].as_bool().ok_or("create")? {
            let dir = match case["dir"].as_str() {
                Some(d) => base.0.join(d),
                None => base.0.join(format!("d{i}")),
            };
            fs::create_dir_all(&dir)?;
            let file = dir.join(name);
            fs::write(&file, b"x")?;
            let mtime = case["mtime"].as_i64().ok_or("mtime")?;
            set_mtime(&file, mtime)?;
            if mtime_unstorable(&file, mtime)? {
                unstorable += 1;
                continue;
            }
            file
        } else {
            PathBuf::from(OsStr::new(&format!(
                "{}/nonexistent-dir/{name}",
                base.text()
            )))
        };
        let (date, source) = infer_date(&path);
        let got = (date.map(|d| d.to_string()), source);
        let expected = &case["expected"];
        if error_name(expected).is_some() {
            // Known difference: Python raises (NUL byte, local year outside 1..9999); the port
            // reports "unavailable".
            assert_eq!(got, (None, "unavailable"), "{label}: raising case");
        } else {
            let want = (
                expected[0].as_str().map(str::to_owned),
                expected[1].as_str().ok_or("source")?,
            );
            assert_eq!(got, want, "{label}");
        }
    }
    if unstorable > 0 {
        eprintln!(
            "infer_date: skipped {unstorable} cases whose mtime this filesystem cannot store"
        );
    }
    // At most the cases recorded before 1901-12-13 (the ext4 floor) may be skipped.
    assert!(
        unstorable <= cases.len() / 4,
        "{unstorable} of {} cases skipped",
        cases.len()
    );
    Ok(())
}

#[test]
fn mtime_rounding_matches_python() -> TestResult {
    let fx = fixtures()?;
    let cases = fx["carry"].as_array().ok_or("carry")?;
    assert!(cases.len() >= 60);
    let mut carried = 0;
    for c in cases {
        let (sec, nsec) = (
            c["sec"].as_i64().ok_or("sec")?,
            c["nsec"].as_i64().ok_or("nsec")?,
        );
        let got = fromtimestamp_epoch_secs(sec, nsec);
        assert_eq!(got, c["epoch"].as_i64(), "sec {sec} nsec {nsec}");
        carried += usize::from(got != Some(sec));
    }
    assert!(
        carried >= 5,
        "the fixtures must exercise the carry into the next second"
    );
    Ok(())
}

#[test]
fn decimal_digit_table_matches_python() -> TestResult {
    let fx = fixtures()?;
    let mut want: HashMap<u32, u32> = HashMap::new();
    for row in fx["digits"].as_array().ok_or("digits")? {
        want.insert(
            u32::try_from(row[0].as_u64().ok_or("cp")?)?,
            u32::try_from(row[1].as_u64().ok_or("value")?)?,
        );
    }
    assert_eq!(want.len(), 680);
    for cp in 0..0x11_0000u32 {
        if let Some(c) = char::from_u32(cp) {
            assert_eq!(decimal_digit_value(c), want.get(&cp).copied(), "{cp:#x}");
        }
    }
    Ok(())
}

#[test]
fn scan_and_resolve_match_python() -> TestResult {
    let fx = fixtures()?;
    let trees = fx["trees"].as_array().ok_or("trees")?;
    assert!(trees.len() >= 10);
    let mut truncated_trees = 0;
    for tree in trees {
        let name = tree["name"].as_str().ok_or("tree name")?;
        let base = TempBase::new("tree")?;
        let base_text = base.text();
        build_tree(&base, &tree["nodes"])?;
        for scan in tree["scans"].as_array().ok_or("scans")? {
            let source = scan["source"]
                .as_str()
                .ok_or("source")?
                .replace("{base}", &base_text);
            let got = scan_source(&source);
            let mut expected = scan["expected"].clone();
            if let Some(exc) = error_name(&expected) {
                assert_eq!(
                    got.err().map(|e| e.python_exception()),
                    Some(exc),
                    "{name}: scan {source:?}"
                );
                continue;
            }
            if let Some(p) = expected["path"].as_str() {
                expected["path"] = Value::from(p.replace("{base}", &base_text));
            }
            let got = got.map_err(|e| format!("{name}: scan {source:?}: {e}"))?;
            truncated_trees += usize::from(got["truncated"] == Value::Bool(true));
            assert_eq!(got, expected, "{name}: scan {source:?}");
        }
        for r in tree["resolves"].as_array().ok_or("resolves")? {
            let source = r["source"]
                .as_str()
                .ok_or("source")?
                .replace("{base}", &base_text);
            let rel = r["rel"]
                .as_str()
                .ok_or("rel")?
                .replace("{base}", &base_text);
            let got = resolve_import_target(&source, &rel);
            let expected = &r["expected"];
            let what = format!("{name}: resolve {source:?} {rel:?}");
            if let Some(exc) = error_name(expected) {
                assert_eq!(got.err().map(|e| e.python_exception()), Some(exc), "{what}");
            } else if expected.is_null() {
                assert_eq!(got, Ok(None), "{what}");
            } else {
                let found = got
                    .map_err(|e| format!("{what}: {e}"))?
                    .ok_or(what.clone())?;
                let rel_found = found.strip_prefix(&base.0)?.to_string_lossy().into_owned();
                assert_eq!(
                    Some(rel_found.as_str()),
                    expected["path"].as_str(),
                    "{what}"
                );
            }
        }
    }
    assert!(truncated_trees >= 6, "{truncated_trees} truncated scans");
    Ok(())
}

#[test]
fn external_source_paths_match_python() -> TestResult {
    let fx = fixtures()?;
    let sp = &fx["source_paths"];
    let mut users: HashMap<String, String> = HashMap::new();
    for (k, v) in sp["users"].as_object().ok_or("users")? {
        users.insert(k.clone(), v.as_str().ok_or("home")?.to_owned());
    }
    let current = sp["current_home"].as_str().map(str::to_owned);
    let cases = sp["cases"].as_array().ok_or("cases")?;
    assert!(cases.len() >= 40);
    for c in cases {
        let raw = c["raw"].as_str().ok_or("raw")?;
        let home = c["home"].as_str();
        let got = external_source_paths_with(raw, &|who| match who {
            HomeOf::Current => home.map(str::to_owned).or_else(|| current.clone()),
            HomeOf::User(name) => users.get(name).cloned(),
        });
        let expected = &c["expected"];
        let what = format!("{raw:?} HOME={home:?}");
        if let Some(exc) = error_name(expected) {
            assert_eq!(got.err().map(|e| e.python_exception()), Some(exc), "{what}");
        } else {
            let want: Vec<String> = expected
                .as_array()
                .ok_or("list")?
                .iter()
                .map(|v| v.as_str().unwrap_or_default().to_owned())
                .collect();
            assert_eq!(got, Ok(want), "{what}");
        }
    }
    Ok(())
}

#[test]
fn external_source_paths_with_a_plain_home() {
    assert_eq!(
        external_source_paths(" ~/a , /b ,~/a/", Some("/home/qa/")),
        Ok(vec!["/home/qa/a".to_owned(), "/b".to_owned()])
    );
    assert_eq!(
        external_source_paths("~", None),
        Err(ExternalSourcesError::NoHomeDirectory)
    );
}
