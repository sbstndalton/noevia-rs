//! Which recovery copies of `models.ini` a write makes and which old ones it removes
//! (sbstndalton/noevia#1021). noevia model-manager's `ini.py` keeps two kinds beside the file:
//!
//! - rotating copies `<file>.bak-<timestamp>[-n]`: the newest [`KEEP_ROTATING`] by name stay
//!   (the rule `_prune_backups` has always used: names sorted descending, the rest removed);
//! - revision copies `<file>.noevia-backup-<baseRevision>` (64 lowercase hex): the file as it was
//!   before a compare-and-swap write. These were never removed, and auto-tune writes many; now the
//!   newest `keepRevisions` by modification time stay (name ascending breaks ties), and the copy
//!   this write makes is always among them, so the newest pre-tune copy is never removed.
//!
//! A write may carry noevia-core's hint `backup: false` (#1003: auto-tune already kept one copy
//! for its run): then it makes neither copy. Old copies are still pruned on every write.
//!
//! [`plan_json`] is the whole contract (the `model-files backups` CLI): it only decides; the
//! caller creates and removes files. Names outside the two patterns, and anything not listed in
//! the input, are never in the reply. Input is untrusted and bounded; nothing here panics.

use crate::json::{self, Value};

/// Rotating copies kept (`BACKUPS_TO_KEEP` in ini.py).
pub const KEEP_ROTATING: usize = 10;
/// Revision copies kept when the input does not say.
pub const KEEP_REVISIONS: usize = 10;
/// Largest `keepRevisions` accepted.
pub const MAX_KEEP: usize = 1000;
/// Most listed files accepted.
pub const MAX_EXISTING: usize = 10_000;
/// Longest file name accepted (bytes).
pub const MAX_NAME_BYTES: usize = 255;
/// Largest request accepted (bytes).
pub const MAX_INPUT_BYTES: usize = 4 * 1024 * 1024;

/// Why a request was refused. Codes are fixed and never carry input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackupError {
    TooLarge,
    Input,
    /// `baseRevision` is present but not a sha256 hex digest (ini.py raises ValueError).
    BaseRevision,
}

impl BackupError {
    pub fn code(self) -> &'static str {
        match self {
            BackupError::TooLarge => "too_large",
            BackupError::Input => "input",
            BackupError::BaseRevision => "base_revision",
        }
    }
}

/// One listed file: its name in the directory and its modification time in nanoseconds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Listed {
    pub name: String,
    pub mtime_ns: u128,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    /// The preset file's own name (e.g. `models.ini`).
    pub file: String,
    pub base_revision: Option<String>,
    /// The write's hint; `false` makes no copies.
    pub backup: bool,
    /// The name the rotating copy would get (ini.py picks it from the clock).
    pub rotating_name: Option<String>,
    pub keep_revisions: usize,
    /// Regular files in the directory before this write.
    pub existing: Vec<Listed>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    /// Make the rotating copy (under `rotating_name`).
    pub rotating: Option<String>,
    /// Make this revision copy (exclusive create: an existing one is left as it is).
    pub revision: Option<String>,
    /// Remove these, after the copies are made. Sorted by name.
    pub prune: Vec<String>,
}

fn is_hex64(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_NAME_BYTES
        && s != "."
        && s != ".."
        && !s.contains(['/', '\\', '\0'])
}

fn rotating_prefix(file: &str) -> String {
    format!("{file}.bak-")
}

fn revision_prefix(file: &str) -> String {
    format!("{file}.noevia-backup-")
}

/// Whether `name` is one of `file`'s revision copies.
pub fn is_revision_copy(file: &str, name: &str) -> bool {
    name.strip_prefix(&revision_prefix(file))
        .is_some_and(is_hex64)
}

/// Whether `name` is one of `file`'s rotating copies (ini.py's glob `<file>.bak-*`).
pub fn is_rotating_copy(file: &str, name: &str) -> bool {
    name.len() > rotating_prefix(file).len() && name.starts_with(&rotating_prefix(file))
}

/// The decision for one write.
pub fn plan(r: &Request) -> Plan {
    let rotating = r.rotating_name.clone().filter(|_| r.backup);
    let revision = match (&r.base_revision, r.backup) {
        (Some(base), true) => Some(format!("{}{}", revision_prefix(&r.file), base)),
        _ => None,
    };
    let mut prune: Vec<String> = Vec::new();

    // Rotating: names descending, the newest KEEP_ROTATING stay (the new copy counts).
    let mut rot: Vec<&str> = r
        .existing
        .iter()
        .map(|l| l.name.as_str())
        .filter(|n| is_rotating_copy(&r.file, n))
        .collect();
    if let Some(new) = rotating.as_deref() {
        if !rot.contains(&new) {
            rot.push(new);
        }
    }
    rot.sort_unstable_by(|a, b| b.cmp(a));
    rot.dedup();
    prune.extend(
        rot.iter()
            .skip(KEEP_ROTATING)
            .filter(|n| Some(**n) != rotating.as_deref())
            .map(|n| (*n).to_owned()),
    );

    // Revision: newest by mtime (then name) stay; this write's copy counts as the newest.
    let keep = r.keep_revisions.clamp(1, MAX_KEEP);
    let mut revs: Vec<(u128, &str)> = r
        .existing
        .iter()
        .filter(|l| is_revision_copy(&r.file, &l.name))
        .map(|l| (l.mtime_ns, l.name.as_str()))
        .collect();
    if let Some(new) = revision.as_deref() {
        match revs.iter_mut().find(|(_, n)| *n == new) {
            // Exclusive create leaves an existing copy as it is, but it is this write's copy.
            Some(entry) => entry.0 = u128::MAX,
            None => revs.push((u128::MAX, new)),
        }
    }
    revs.sort_unstable_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
    revs.dedup_by(|a, b| a.1 == b.1);
    prune.extend(
        revs.iter()
            .skip(keep)
            .filter(|(_, n)| Some(*n) != revision.as_deref())
            .map(|(_, n)| (*n).to_owned()),
    );
    // Only names that exist.
    prune.retain(|n| r.existing.iter().any(|l| &l.name == n));
    prune.sort_unstable();
    prune.dedup();
    Plan {
        rotating,
        revision,
        prune,
    }
}

fn str_of<'a>(v: &'a Value, key: &str) -> Result<Option<&'a str>, BackupError> {
    match v.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Str(s)) => Ok(Some(s)),
        Some(_) => Err(BackupError::Input),
    }
}

fn uint_of(v: &Value) -> Option<u128> {
    match v {
        Value::Int(i) => i.to_string().parse::<u128>().ok(),
        _ => None,
    }
}

/// Parse a request:
///
/// ```json
/// {"file":"models.ini","baseRevision":"<64 hex>"|null,"backup":true,
///  "rotatingName":"models.ini.bak-20261007-101500","keepRevisions":10,
///  "existing":[{"name":"models.ini.noevia-backup-…","mtimeNs":1759831000000000000}]}
/// ```
pub fn parse(input: &[u8]) -> Result<Request, BackupError> {
    if input.len() > MAX_INPUT_BYTES {
        return Err(BackupError::TooLarge);
    }
    let text = std::str::from_utf8(input).map_err(|_| BackupError::Input)?;
    let v = json::parse(text).map_err(|_| BackupError::Input)?;
    if !matches!(v, Value::Obj(_)) {
        return Err(BackupError::Input);
    }
    let file = str_of(&v, "file")?
        .filter(|f| valid_name(f))
        .ok_or(BackupError::Input)?
        .to_owned();
    let base_revision = match str_of(&v, "baseRevision")? {
        None => None,
        Some(b) if is_hex64(b) => Some(b.to_owned()),
        Some(_) => return Err(BackupError::BaseRevision),
    };
    let backup = match v.get("backup") {
        None | Some(Value::Null) => true,
        Some(Value::Bool(b)) => *b,
        Some(_) => return Err(BackupError::Input),
    };
    let rotating_name = match str_of(&v, "rotatingName")? {
        None => None,
        Some(n) if valid_name(n) && is_rotating_copy(&file, n) => Some(n.to_owned()),
        Some(_) => return Err(BackupError::Input),
    };
    let keep_revisions = match v.get("keepRevisions") {
        None | Some(Value::Null) => KEEP_REVISIONS,
        Some(k) => match uint_of(k) {
            Some(n) if (1..=MAX_KEEP as u128).contains(&n) => n as usize,
            _ => return Err(BackupError::Input),
        },
    };
    let Some(Value::Arr(list)) = v.get("existing") else {
        return Err(BackupError::Input);
    };
    if list.len() > MAX_EXISTING {
        return Err(BackupError::Input);
    }
    let mut existing = Vec::with_capacity(list.len());
    for e in list {
        let name = str_of(e, "name")?
            .filter(|n| valid_name(n))
            .ok_or(BackupError::Input)?;
        let mtime_ns = e
            .get("mtimeNs")
            .and_then(uint_of)
            .ok_or(BackupError::Input)?;
        existing.push(Listed {
            name: name.to_owned(),
            mtime_ns,
        });
    }
    // A directory lists each name once; a repeated one is not a listing.
    let mut names: Vec<&str> = existing.iter().map(|l| l.name.as_str()).collect();
    names.sort_unstable();
    if names.windows(2).any(|w| matches!(w, [a, b] if a == b)) {
        return Err(BackupError::Input);
    }
    Ok(Request {
        file,
        base_revision,
        backup,
        rotating_name,
        keep_revisions,
        existing,
    })
}

fn push_str(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 || !c.is_ascii() => {
                let mut buf = [0u16; 2];
                for u in c.encode_utf16(&mut buf) {
                    out.push_str(&format!("\\u{u:04x}"));
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// `{"rotating":…|null,"revision":…|null,"prune":[…]}` (ASCII only).
pub fn plan_to_json(p: &Plan) -> String {
    let mut out = String::from("{\"rotating\":");
    match &p.rotating {
        Some(n) => push_str(&mut out, n),
        None => out.push_str("null"),
    }
    out.push_str(",\"revision\":");
    match &p.revision {
        Some(n) => push_str(&mut out, n),
        None => out.push_str("null"),
    }
    out.push_str(",\"prune\":[");
    for (i, n) in p.prune.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        push_str(&mut out, n);
    }
    out.push_str("]}");
    out
}

/// The CLI contract: request JSON bytes in, plan JSON out, or a fixed error code.
pub fn plan_json(input: &[u8]) -> Result<String, BackupError> {
    Ok(plan_to_json(&plan(&parse(input)?)))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn rev(n: u32) -> String {
        format!("models.ini.noevia-backup-{n:064x}")
    }

    fn req(existing: Vec<Listed>, backup: bool) -> Request {
        Request {
            file: "models.ini".into(),
            base_revision: Some(A.into()),
            backup,
            rotating_name: Some("models.ini.bak-20261007-120000".into()),
            keep_revisions: 3,
            existing,
        }
    }

    #[test]
    fn a_hinted_write_makes_no_copies_but_still_prunes() {
        let existing = (0..5)
            .map(|i| Listed {
                name: rev(i),
                mtime_ns: u128::from(i),
            })
            .collect();
        let p = plan(&req(existing, false));
        assert_eq!(p.rotating, None);
        assert_eq!(p.revision, None);
        assert_eq!(p.prune, vec![rev(0), rev(1)]);
    }

    #[test]
    fn this_writes_copy_is_newest_and_kept() {
        let existing: Vec<Listed> = (0..5)
            .map(|i| Listed {
                name: rev(i),
                mtime_ns: u128::from(i),
            })
            .collect();
        let p = plan(&req(existing, true));
        assert_eq!(
            p.revision.as_deref(),
            Some(&*format!("models.ini.noevia-backup-{A}"))
        );
        assert_eq!(p.prune, vec![rev(0), rev(1), rev(2)]);
    }

    #[test]
    fn rotating_rule_matches_ini_py_and_other_names_are_never_touched() {
        let mut existing: Vec<Listed> = (0..12)
            .map(|i| Listed {
                name: format!("models.ini.bak-20260101-0000{i:02}"),
                mtime_ns: 0,
            })
            .collect();
        for n in [
            "models.ini",
            "models.ini.noevia-backup-notes",
            "other.ini.bak-1",
            "models.ini.bak-before-d3",
        ] {
            existing.push(Listed {
                name: n.into(),
                mtime_ns: 0,
            });
        }
        let p = plan(&req(existing, true));
        // 12 timestamped + before-d3 + the new one: the 4 oldest timestamps go.
        assert_eq!(
            p.prune,
            (0..4)
                .map(|i| format!("models.ini.bak-20260101-0000{i:02}"))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn json_contract() {
        let body = format!(
            r#"{{"file":"models.ini","baseRevision":"{A}","rotatingName":"models.ini.bak-1","existing":[{{"name":"{}","mtimeNs":5}}]}}"#,
            rev(1)
        );
        assert_eq!(
            plan_json(body.as_bytes()).unwrap(),
            format!(
                r#"{{"rotating":"models.ini.bak-1","revision":"models.ini.noevia-backup-{A}","prune":[]}}"#
            )
        );
        assert_eq!(
            plan_json(br#"{"file":"models.ini","baseRevision":"x","existing":[]}"#),
            Err(BackupError::BaseRevision)
        );
        for bad in [
            &br#"{"file":"a/b","existing":[]}"#[..],
            br#"{"file":"models.ini"}"#,
            br#"{"file":"models.ini","existing":[{"name":"x","mtimeNs":-1}]}"#,
            br#"{"file":"models.ini","existing":[],"backup":"no"}"#,
            br#"{"file":"models.ini","existing":[],"keepRevisions":0}"#,
            br#"{"file":"models.ini","existing":[],"rotatingName":"evil"}"#,
            br#"[]"#,
            br#"{"file":"models.ini","existing":[{"name":"x","mtimeNs":1},{"name":"x","mtimeNs":2}]}"#,
        ] {
            assert_eq!(
                plan_json(bad),
                Err(BackupError::Input),
                "{}",
                String::from_utf8_lossy(bad)
            );
        }
    }
}
