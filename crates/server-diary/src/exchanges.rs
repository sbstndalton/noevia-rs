//! The Diary tab's recovery records: core `diary-jobs.cjs` `list` and `acknowledge`
//! (sbstndalton/noevia#2206), behind `GET` and `POST /api/diary/exchanges`.
//!
//! A chat turn for the Diary writes one JSON record per exchange, `diary-conversations/<day>/<id>.json`
//! in the person's workspace, before it dispatches and as the stream goes. Node's chat turn is still
//! the only writer of a running record; this module reads them and writes the one field
//! `acknowledge` sets, on a record nothing is running for.
//!
//! "Is something running for it" was a Set in Node's memory (`active`). Node now publishes it beside
//! the records, `diary-conversations/active.json` = `{ "start": <container start id>, "files":
//! ["<day>/<id>.json", ...] }`, written before a record is first written and updated after its last
//! write (core `diary-jobs.cjs`). Because of that order a record that reads `running` while its file is
//! missing from a set read after it is one nothing runs for, with one re-read to rule out the record
//! having ended in between ([`list`]). A set written by another container start (a Node that is
//! gone) names nothing.

use js_json::{wtf16, JValue};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// The directory of a workspace that holds the records, and the published set inside it.
pub const DIR: &str = "diary-conversations";
pub const ACTIVE_FILE: &str = "active.json";
/// `.slice(-100)`: the newest records a day returns.
pub const LIST_CAP: usize = 100;

/// An answer that is an error: Node's `Object.assign(Error(message), { status })`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fault {
    pub status: u16,
    pub message: &'static str,
}

fn fault(status: u16, message: &'static str) -> Fault {
    Fault { status, message }
}

/// diary-jobs.cjs `validDay`: `YYYY-MM-DD` that is a real calendar date (`new Date(day +
/// 'T12:00:00Z').toISOString().slice(0, 10) === day`), years 0000 to 9999.
pub fn valid_day(day: &str) -> bool {
    let mut parts = day.split('-');
    let (Some(y), Some(m), Some(d), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    let digits = |part: &str, n: usize| part.len() == n && part.bytes().all(|c| c.is_ascii_digit());
    if !digits(y, 4) || !digits(m, 2) || !digits(d, 2) {
        return false;
    }
    let (Ok(year), Ok(month), Ok(date)) = (y.parse::<u32>(), m.parse::<u32>(), d.parse::<u32>())
    else {
        return false;
    };
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return false,
    };
    (1..=days).contains(&date)
}

/// `/^[a-zA-Z0-9-]{16,80}$/`
fn valid_id(id: &str) -> bool {
    (16..=80).contains(&id.len()) && id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
}

/// `/^[a-zA-Z0-9-]{16,80}\.json$/`
fn record_name(name: &str) -> bool {
    name.strip_suffix(".json").is_some_and(valid_id)
}

/// The set Node published, as `<day>/<id>.json` names. Empty when none was published, when the file
/// is unreadable, or when it was written by another container start.
pub fn active_set(user_dir: &Path, start_id: Option<&str>) -> HashSet<String> {
    let Ok(bytes) = std::fs::read(user_dir.join(DIR).join(ACTIVE_FILE)) else {
        return HashSet::new();
    };
    let Ok(doc) = wtf16::parse(&String::from_utf8_lossy(&bytes)) else {
        return HashSet::new();
    };
    match (doc.get("start").as_str(), start_id) {
        (Some(written), Some(own)) if written == own => {}
        _ => return HashSet::new(),
    }
    match doc.get("files") {
        JValue::Arr(files) => files
            .iter()
            .filter_map(|f| f.as_str().map(str::to_owned))
            .collect(),
        _ => HashSet::new(),
    }
}

fn read_row(file: &Path) -> Result<Option<JValue>, Fault> {
    let bytes = match std::fs::read(file) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        // A record that cannot be read is left out, as `list` did (`catch { return null }`).
        Err(_) => return Ok(None),
    };
    match wtf16::parse(&String::from_utf8_lossy(&bytes)) {
        Ok(v) => Ok(Some(v)),
        Err(wtf16::Refusal::Invalid) => Ok(None),
        // JSON.parse would accept it and this port cannot hold it: not listed as less than it is.
        Err(wtf16::Refusal::Unsupported) => Err(fault(500, "Could not read recovery records")),
    }
}

fn is_running(row: &JValue) -> bool {
    row.get("state").as_str() == Some("running")
}

fn mark_uncertain(row: &mut JValue) {
    if let JValue::Obj(fields) = row {
        wtf16::set(fields, "state", JValue::Str("uncertain".to_owned()));
    }
}

/// `(a, b) => a.startedAt - b.startedAt`, as a total order. Core's comparator answers NaN for a
/// record with no numeric `startedAt`, which V8 reads as "equal" to everything, so such a record is
/// neither before nor after its neighbours and the sorted list depends on the order the sort visits
/// them in; a comparator that is not a total order may also panic Rust's sort. Here a record with no
/// numeric `startedAt` is older than every record that has one (`-Infinity`), and records equal on it
/// keep their file-name order (the sort is stable). Records the Diary writes always carry the field.
fn by_started(a: &JValue, b: &JValue) -> std::cmp::Ordering {
    let at = |v: &JValue| {
        v.get("startedAt")
            .as_num()
            .filter(|n| !n.is_nan())
            .unwrap_or(f64::NEG_INFINITY)
    };
    at(a).partial_cmp(&at(b)).unwrap_or(std::cmp::Ordering::Equal)
}

/// `list(workspace, day)`: the day's records, oldest first, the newest [`LIST_CAP`]; a record that
/// reads `running` with nothing running for it reads `uncertain`.
pub fn list(
    user_dir: &Path,
    day: Option<&str>,
    start_id: Option<&str>,
) -> Result<Vec<JValue>, Fault> {
    let Some(day) = day.filter(|d| valid_day(d)) else {
        return Err(fault(400, "Valid diary day required."));
    };
    let dir = user_dir.join(DIR).join(day);
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .map_err(|_| fault(500, "Could not read recovery records"))?
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|n| record_name(n))
        .collect();
    // libuv's scandir sorts by name.
    names.sort();
    let mut rows: Vec<JValue> = Vec::new();
    for name in names {
        let file = dir.join(&name);
        let Some(row) = read_row(&file)? else {
            continue;
        };
        if !row.truthy() {
            continue;
        }
        let row = if is_running(&row) {
            match settle_running(user_dir, day, &name, &file, start_id, row)? {
                Some(row) => row,
                None => continue,
            }
        } else {
            row
        };
        rows.push(row);
    }
    rows.sort_by(by_started);
    let skip = rows.len().saturating_sub(LIST_CAP);
    Ok(rows.split_off(skip))
}

/// What a record that reads `running` is. The set is read after the record; a record in it is
/// running. For one that is not, the record is read again: one that changed is the answer (it ended
/// in between), and only one that still reads `running` is abandoned, because it cannot then have
/// ended before the set was read, and a job that is alive was in the set from before its record
/// existed until after its last write. `None`: the record is gone.
fn settle_running(
    user_dir: &Path,
    day: &str,
    name: &str,
    file: &Path,
    start_id: Option<&str>,
    row: JValue,
) -> Result<Option<JValue>, Fault> {
    let key = format!("{day}/{name}");
    if active_set(user_dir, start_id).contains(&key) {
        return Ok(Some(row));
    }
    match read_row(file)? {
        Some(mut again) if again.truthy() => {
            if is_running(&again) {
                mark_uncertain(&mut again);
            }
            Ok(Some(again))
        }
        _ => Ok(None),
    }
}

fn location(user_dir: &Path, day: &JValue, id: &JValue) -> Result<(PathBuf, String), Fault> {
    let (Some(day), Some(id)) = (day.as_str(), id.as_str()) else {
        return Err(fault(400, "Valid diary day and exchange ID required."));
    };
    if !valid_day(day) || !valid_id(id) {
        return Err(fault(400, "Valid diary day and exchange ID required."));
    }
    Ok((
        user_dir.join(DIR).join(day).join(format!("{id}.json")),
        format!("{day}/{id}.json"),
    ))
}

/// Node's `atomicJson(file, row)`: write beside, then rename. `tmp` is `<file>.<pid>.tmp`.
fn write_row(file: &Path, row: &JValue) -> std::io::Result<()> {
    use std::io::Write as _;
    let text = wtf16::stringify_pretty(row)
        .ok_or_else(|| std::io::Error::other("the record cannot be written"))?;
    let mut tmp = file.as_os_str().to_owned();
    tmp.push(format!(".{}.tmp", std::process::id()));
    let tmp = PathBuf::from(tmp);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut out = options.open(&tmp)?;
    let result = out
        .write_all(text.as_bytes())
        .and_then(|()| out.sync_all())
        .and_then(|()| std::fs::rename(&tmp, file));
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// `acknowledge(workspace, day, id)`: marks an uncertain or failed exchange as reviewed. The record's
/// outcome is never rewritten; only `acknowledgedAt` and `updatedAt` are set, once.
pub fn acknowledge(
    user_dir: &Path,
    day: &JValue,
    id: &JValue,
    start_id: Option<&str>,
    now_ms: i64,
) -> Result<JValue, Fault> {
    let (file, key) = location(user_dir, day, id)?;
    if !file.exists() {
        return Err(fault(404, "No such exchange."));
    }
    if active_set(user_dir, start_id).contains(&key) {
        return Err(fault(409, "This exchange is still running."));
    }
    let unreadable = || fault(500, "This exchange record cannot be read.");
    let bytes = std::fs::read(&file).map_err(|_| unreadable())?;
    let mut row = wtf16::parse(&String::from_utf8_lossy(&bytes)).map_err(|_| unreadable())?;
    // JS would throw reading `row.state` of `null`.
    if row.is_nullish() {
        return Err(unreadable());
    }
    // An orphaned record, as `list` reports it.
    if is_running(&row) {
        mark_uncertain(&mut row);
    }
    if !matches!(row.get("state").as_str(), Some("uncertain" | "failed")) {
        return Err(fault(409, "This exchange has nothing to resolve."));
    }
    if !row.get("acknowledgedAt").truthy() {
        if let JValue::Obj(fields) = &mut row {
            let at = JValue::Num(now_ms as f64);
            wtf16::set(fields, "acknowledgedAt", at.clone());
            wtf16::set(fields, "updatedAt", at);
        }
        write_row(&file, &row).map_err(|_| fault(500, "Could not update the recovery record"))?;
    }
    Ok(row)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn days_are_real_calendar_dates_only() {
        for ok in [
            "2026-10-30",
            "2024-02-29",
            "2000-02-29",
            "0000-02-29",
            "9999-12-31",
            "0001-01-01",
        ] {
            assert!(valid_day(ok), "{ok}");
        }
        for bad in [
            "2026-02-29",
            "1900-02-29",
            "2026-13-01",
            "2026-00-10",
            "2026-04-31",
            "2026-4-3",
            "26-10-30",
            "2026-10-30 ",
            "2026-10-300",
            "２０２６-10-30",
            "2026/10/30",
            "",
            "2026-10-3x",
        ] {
            assert!(!valid_day(bad), "{bad}");
        }
    }

    const A: &str = "exchange-aaaaaaaaaaaaaaaa";
    const START: &str = "0123456789abcdef0123456789abcdef";

    fn rig(on_disk: Option<&str>, published: &[&str]) -> (tempfile::TempDir, PathBuf, String) {
        let root = tempfile::tempdir().unwrap();
        let day = root.path().join(DIR).join("2026-10-30");
        std::fs::create_dir_all(&day).unwrap();
        let file = day.join(format!("{A}.json"));
        if let Some(text) = on_disk {
            std::fs::write(&file, text).unwrap();
        }
        let names: Vec<String> = published.iter().map(|n| format!("{n:?}")).collect();
        std::fs::write(
            root.path().join(DIR).join(ACTIVE_FILE),
            format!(r#"{{"start":"{START}","files":[{}]}}"#, names.join(",")),
        )
        .unwrap();
        let key = format!("2026-10-30/{A}.json");
        (root, file, key)
    }

    /// The race the order of the two reads has to survive: the record read `running`, and by the time
    /// the set was read the job had ended (its record rewritten, its name taken out).
    #[test]
    fn a_record_that_ended_between_the_two_reads_is_what_it_became_not_uncertain() {
        let stale: JValue = wtf16::parse(&format!(
            r#"{{"id":"{A}","state":"running","startedAt":1}}"#
        ))
        .unwrap();
        let name = format!("{A}.json");
        // It completed: the answer is the new record.
        let (root, file, _) = rig(
            Some(&format!(
                r#"{{"id":"{A}","state":"complete","startedAt":1}}"#
            )),
            &[],
        );
        let got = settle_running(
            root.path(),
            "2026-10-30",
            &name,
            &file,
            Some(START),
            stale.clone(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(got.get("state").as_str(), Some("complete"));
        // It is still `running` after the set was read without it: nothing runs for it.
        let (root, file, _) = rig(
            Some(&format!(
                r#"{{"id":"{A}","state":"running","startedAt":1}}"#
            )),
            &[],
        );
        let got = settle_running(
            root.path(),
            "2026-10-30",
            &name,
            &file,
            Some(START),
            stale.clone(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(got.get("state").as_str(), Some("uncertain"));
        // It is in the set: left running, and the file is not even read again.
        let (root, file, key) = rig(None, &["2026-10-30/exchange-aaaaaaaaaaaaaaaa.json"]);
        assert!(active_set(root.path(), Some(START)).contains(&key));
        let got = settle_running(
            root.path(),
            "2026-10-30",
            &name,
            &file,
            Some(START),
            stale.clone(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(got.get("state").as_str(), Some("running"));
        // The record is gone: nothing to list.
        let (root, file, _) = rig(None, &[]);
        assert!(
            settle_running(root.path(), "2026-10-30", &name, &file, Some(START), stale)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn ids_and_record_names_follow_core_patterns() {
        assert!(valid_id("abcdefghijklmnop"));
        assert!(!valid_id("abcdefghijklmno"));
        assert!(valid_id(&"a".repeat(80)));
        assert!(!valid_id(&"a".repeat(81)));
        assert!(!valid_id("abcdefghijklmno_p"));
        assert!(record_name("abcdefghijklmnop.json"));
        assert!(!record_name("abcdefghijklmnop.json.tmp"));
        assert!(!record_name("active.json"));
        assert!(!record_name("../x.json"));
    }
}
