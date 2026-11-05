//! D5 write routes for a ready managed tenant: PUT /api/file, POST /api/directory,
//! POST /api/entries/append and POST /api/entries/edit, ported from app.py, workspace_files.py
//! and corpus_store.py. The caller holds the tenant's writer (`write::Writers`) for the whole
//! call. Every `Reply::Decline` below is returned before anything is written.
//!
//! Differences from Python, all invisible in the reply:
//! - the retrieval index is not refreshed here (no sqlite-vec in this process): every changed
//!   document is left in `dirty_documents`, which Python's `_reindex_dirty` picks up;
//! - corpus text always comes from the diary-corpus-write transforms, so these routes are
//!   served only when Python itself runs them (DIARY_CORPUS_WRITE_IMPL=rust).

use crate::corpus::{self, Layout};
use crate::managed::Managed;
use crate::py;
use crate::workspace::{safe_path, Reply, MAX_FILE};
use crate::write::{self, Journal, Mkdir, Put, PyVal, Store, WriteFailure};
use chrono::{DateTime, Datelike, FixedOffset, NaiveDate, NaiveDateTime, Timelike};
use diary_corpus_write as cw;
use serde::Serialize;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

pub const APPEND_MAX_CHARS: usize = 8000;
const PENDING_NATIVE: &str =
    "Diary exchange is pending; the save outcome is uncertain until journal replay";
const EDIT_QUEUED: &str =
    "Correction is queued but could not be saved yet. It will retry when storage is available.";
const MALFORMED: &str = "The entry cannot be edited safely: malformed or missing block";

/// What a write route needs of the tenant.
pub struct Ctx<'a> {
    pub reader: &'a Managed,
    pub store: Store,
    pub journal: Journal,
    pub layout: &'a Layout,
}

fn json_reply<T: Serialize>(status: u16, value: &T) -> Reply {
    match serde_json::to_vec(value) {
        Ok(body) => Reply::Json(status, body),
        Err(_) => Reply::Decline,
    }
}

fn detail(status: u16, text: &str) -> Reply {
    json_reply(status, &json!({ "detail": text }))
}

fn error(status: u16, text: &str) -> Reply {
    json_reply(status, &json!({ "error": text }))
}

/// An unhandled exception in Python: Starlette's plain 500.
pub fn internal() -> Reply {
    Reply::Text(500, "Internal Server Error".into())
}

fn sha256_hex(data: &[u8]) -> String {
    Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Python truthiness of a JSON value.
fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64() != Some(0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

// ------------------------------------------------------------------ PUT /api/file

#[derive(Serialize)]
struct Saved<'a> {
    path: &'a str,
    content: &'a str,
    version: String,
}

/// workspace_files.file_write (then app.py's `_reindex_dirty`, left to Python: see module doc).
pub fn file_write(c: &Ctx<'_>, body: &Map<String, Value>) -> Reply {
    let path = match body.get("path") {
        Some(Value::String(s)) => s.as_str(),
        _ => return detail(400, "Invalid path"),
    };
    if let Err(msg) = safe_path(path, false) {
        return detail(400, msg);
    }
    let content = match body.get("content") {
        Some(Value::String(s)) if s.len() <= MAX_FILE && body.contains_key("version") => s,
        _ => return detail(400, "Content and base version required; maximum 512 KiB"),
    };
    let full = corpus::join(&[path]);
    let (current, etag) = match c.store.get(&full) {
        Ok(Some((data, etag))) => match String::from_utf8(data) {
            Ok(text) => (Some(text), Some(etag)),
            // errors="replace" changes the text Python hashes: leave it to Python.
            Err(_) => return Reply::Decline,
        },
        Ok(None) => (None, None),
        Err(_) => return Reply::Decline,
    };
    let current_version = current.as_ref().map(|t| sha256_hex(t.as_bytes()));
    let matches = match (body.get("version"), &current_version) {
        (Some(Value::String(v)), Some(cur)) => v == cur,
        (Some(Value::Null), None) => true,
        _ => false,
    };
    if !matches {
        return detail(
            409,
            "File changed elsewhere. Reopen it before saving; your draft has been kept.",
        );
    }
    let saved = |text: &str| {
        json_reply(
            200,
            &Saved {
                path,
                content: text,
                version: sha256_hex(text.as_bytes()),
            },
        )
    };
    if current.as_deref() == Some(content.as_str()) {
        return saved(content);
    }
    let Ok(was_dirty) = c.journal.is_dirty(&full) else {
        return internal();
    };
    if c.journal.mark_dirty(&full).is_err() {
        return internal();
    }
    match c.store.put(&full, content.as_bytes(), etag.as_deref()) {
        Ok(Put::Ok(..)) => saved(content),
        Ok(Put::PathConflict(_)) => {
            if !was_dirty && c.journal.clear_dirty(&full).is_err() {
                return internal();
            }
            detail(
                409,
                "A directory or file already occupies this path; your draft has been kept",
            )
        }
        Ok(Put::Precondition) => detail(
            409,
            "Storage write failed or conflicted; your draft has been kept",
        ),
        Err(_) => internal(),
    }
}

// ------------------------------------------------------------------ POST /api/directory

#[derive(Serialize)]
struct Dir<'a> {
    path: &'a str,
    #[serde(rename = "isDir")]
    is_dir: bool,
}

/// workspace_files.directory_create over the managed backend.
pub fn directory_create(c: &Ctx<'_>, body: &Map<String, Value>) -> Reply {
    let path = match body.get("path") {
        None => "",
        Some(Value::String(s)) => s.as_str(),
        Some(_) => return detail(400, "Invalid path"),
    };
    if let Err(msg) = safe_path(path, true) {
        return detail(400, msg);
    }
    if path.is_empty() {
        return detail(405, "The diary root already exists");
    }
    match c.store.create_directory(&corpus::join(&[path])) {
        Ok(Mkdir::Ok) => json_reply(200, &Dir { path, is_dir: true }),
        Ok(Mkdir::Exists) => detail(405, "A file or directory already exists at this path"),
        Ok(Mkdir::NoParent) => detail(409, "Parent directory does not exist"),
        Err(_) => internal(),
    }
}

// ------------------------------------------------------------------ journal application

/// One journal entry this call enqueued, applied as CorpusStore.apply_pending would (the tenant
/// had nothing else pending, or the request was declined). Ok(true) when applied.
fn apply_entry(
    c: &Ctx<'_>,
    id: &str,
    apply: impl FnOnce() -> Result<(), WriteFailure>,
) -> Result<bool, write::Error> {
    match apply() {
        Ok(()) => {
            c.journal.mark_applied(id)?;
            Ok(true)
        }
        Err(WriteFailure::Permanent(msg)) => {
            // Quarantined (nothing later is pending for this tenant). journal.is_applied is true
            // for a quarantined row, so Python reports it as applied: kept for parity (filed as
            // a Python bug in the D5 design note).
            c.journal.quarantine(id, &format!("quarantined: {msg}"))?;
            Ok(true)
        }
        Err(WriteFailure::Infra(e)) => {
            c.journal.mark_failed(id, &e.0)?;
            Ok(false)
        }
    }
}

// ------------------------------------------------------------------ POST /api/entries/append

/// `MARKER_RE.search(text)`: `<!--\s*xid:[0-9a-fA-F-]+\s*-->` with Python's `\s`.
fn has_any_marker(text: &str) -> bool {
    let mut from = 0;
    while let Some(rel) = text.get(from..).and_then(|t| t.find("<!--")) {
        let start = from + rel + 4;
        from = from + rel + 1;
        let rest = text.get(start..).unwrap_or("");
        let rest = rest.trim_start_matches(py::is_space);
        let Some(rest) = rest.strip_prefix("xid:") else {
            continue;
        };
        let run = rest
            .bytes()
            .take_while(|b| b.is_ascii_hexdigit() || *b == b'-')
            .count();
        // The greedy run may have eaten the "--" of "-->": try every shorter split.
        for k in (1..=run).rev() {
            let tail = rest.get(k..).unwrap_or("").trim_start_matches(py::is_space);
            if tail.starts_with("-->") {
                return true;
            }
        }
    }
    false
}

/// `re.search(r"^\s*#", text, re.M)`.
fn has_heading(text: &str) -> bool {
    text.split('\n')
        .any(|line| line.trim_start_matches(py::is_space).starts_with('#'))
}

fn is_uuid4_lower(s: &str) -> bool {
    let b = s.as_bytes();
    let hex = |c: &u8| c.is_ascii_digit() || (b'a'..=b'f').contains(c);
    b.len() == 36
        && b.iter().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => *c == b'-',
            14 => *c == b'4',
            19 => matches!(c, b'8' | b'9' | b'a' | b'b'),
            _ => hex(c),
        })
}

pub enum EntryTime {
    At(DateTime<FixedOffset>),
    /// HTTPException(400, "Invalid diary date/time; include a timezone offset").
    Invalid,
    /// A form Python's datetime.fromisoformat may read differently: Python serves it.
    Unsure,
}

/// `datetime.fromisoformat(raw)` for the forms browsers send: `YYYY-MM-DDTHH:MM[:SS[.fff|
/// .ffffff]]` plus `Z` or `±HH:MM`. Anything else is Unsure.
pub fn parse_entry_time(raw: &str) -> EntryTime {
    let (main, offset) = if let Some(m) = raw.strip_suffix('Z') {
        (m, Some(0i32))
    } else {
        let tail = raw.len().saturating_sub(6);
        match raw.get(tail..) {
            Some(t) if (t.starts_with('+') || t.starts_with('-')) && t.get(3..4) == Some(":") => {
                let sign = if t.starts_with('-') { -1 } else { 1 };
                let hh = t.get(1..3).and_then(|x| x.parse::<i32>().ok());
                let mm = t.get(4..6).and_then(|x| x.parse::<i32>().ok());
                match (hh, mm) {
                    (Some(h), Some(m))
                        if t.get(1..3)
                            .is_some_and(|x| x.bytes().all(|b| b.is_ascii_digit()))
                            && t.get(4..6)
                                .is_some_and(|x| x.bytes().all(|b| b.is_ascii_digit())) =>
                    {
                        if h > 23 || m > 59 {
                            return EntryTime::Unsure;
                        }
                        (
                            raw.get(..tail).unwrap_or(""),
                            Some(sign * (h * 3600 + m * 60)),
                        )
                    }
                    _ => return EntryTime::Unsure,
                }
            }
            _ => (raw, None),
        }
    };
    let shape_ok = |s: &str| {
        let b = s.as_bytes();
        let digit = |i: usize| b.get(i).is_some_and(u8::is_ascii_digit);
        let base = b.len() >= 16
            && (0..4).all(digit)
            && b.get(4) == Some(&b'-')
            && (5..7).all(digit)
            && b.get(7) == Some(&b'-')
            && (8..10).all(digit)
            && b.get(10) == Some(&b'T')
            && (11..13).all(digit)
            && b.get(13) == Some(&b':')
            && (14..16).all(digit);
        base && match b.len() {
            16 => true,
            19 => b.get(16) == Some(&b':') && (17..19).all(digit),
            23 | 26 => {
                b.get(16) == Some(&b':')
                    && (17..19).all(digit)
                    && b.get(19) == Some(&b'.')
                    && (20..b.len()).all(digit)
            }
            _ => false,
        }
    };
    if !shape_ok(main) {
        return EntryTime::Unsure;
    }
    let fmt = match main.len() {
        16 => "%Y-%m-%dT%H:%M",
        19 => "%Y-%m-%dT%H:%M:%S",
        _ => "%Y-%m-%dT%H:%M:%S%.f",
    };
    let Ok(naive) = NaiveDateTime::parse_from_str(main, fmt) else {
        // Shape is right but the calendar value is not (month 13, Feb 30, hour 24 ...): Python's
        // fromisoformat raises ValueError too.
        return EntryTime::Invalid;
    };
    let Some(seconds) = offset else {
        // A naive time: "timezone offset required".
        return EntryTime::Invalid;
    };
    match FixedOffset::east_opt(seconds).and_then(|tz| naive.and_local_timezone(tz).single()) {
        Some(at) => EntryTime::At(at),
        None => EntryTime::Unsure,
    }
}

#[derive(Serialize)]
struct Appended<'a> {
    ok: bool,
    queued: bool,
    xid: &'a str,
    day: String,
    document: &'a str,
}

/// POST /api/entries/append, after the body parsed as a JSON object. `validate` runs before
/// tenant resolution in Python; the caller only resolves a ready tenant, so the order is kept.
pub fn append_validate(body: &Map<String, Value>) -> Result<AppendReq, Reply> {
    let text = match body.get("text") {
        Some(Value::String(t))
            if !py::strip(t).is_empty() && t.chars().count() <= APPEND_MAX_CHARS =>
        {
            t.clone()
        }
        _ => {
            return Err(error(
                400,
                &format!("text is required (at most {APPEND_MAX_CHARS} characters)"),
            ))
        }
    };
    let title = match body.get("title") {
        Some(v) if truthy(v) => match v {
            Value::String(t)
                if t.chars().count() <= 80
                    && !t.contains('\r')
                    && !t.contains('\n')
                    && !t.contains('#') =>
            {
                t.clone()
            }
            _ => {
                return Err(error(
                    400,
                    "title must be one line of at most 80 characters",
                ))
            }
        },
        _ => "Added from chat (approved)".to_string(),
    };
    let request_id = match body.get("requestId") {
        Some(Value::String(r)) if is_uuid4_lower(r) => r.clone(),
        _ => return Err(error(400, "requestId must be a UUIDv4")),
    };
    if body.contains_key("entryDay") {
        return Err(error(400, "appends always go to today's entry"));
    }
    if has_any_marker(&text) || has_heading(&text) {
        return Err(error(
            400,
            "text must not contain diary markers or Markdown headings",
        ));
    }
    let invalid = || detail(400, "Invalid diary date/time; include a timezone offset");
    let now = match body.get("entryTime") {
        Some(v) if truthy(v) => match v {
            Value::String(raw) => match parse_entry_time(raw) {
                EntryTime::At(at) => at,
                EntryTime::Invalid => return Err(invalid()),
                EntryTime::Unsure => return Err(Reply::Decline),
            },
            _ => return Err(invalid()),
        },
        _ => chrono::Local::now().fixed_offset(),
    };
    let day = now.date_naive();
    if !(1900..=2100).contains(&day.year()) {
        return Err(invalid());
    }
    let drift = (now.timestamp_millis() - chrono::Utc::now().timestamp_millis()).abs();
    if drift > 900_000 {
        return Err(error(400, "entryTime must be the current time"));
    }
    Ok(AppendReq {
        text,
        title,
        request_id,
        now,
        day,
    })
}

pub struct AppendReq {
    text: String,
    title: String,
    request_id: String,
    now: DateTime<FixedOffset>,
    day: NaiveDate,
}

/// CorpusStore.log_exchange (native transform) and the rest of api_entries_append.
pub fn append(c: &Ctx<'_>, req: &AppendReq) -> Reply {
    let l = c.layout;
    let xid = req.request_id.as_str();
    let document = corpus::document_path(l, req.day);
    let has = |doc: &str| -> Result<bool, ()> {
        match c.store.get(doc) {
            Ok(Some((data, _))) => Ok(cw::has_marker(&String::from_utf8_lossy(&data), xid)),
            Ok(None) => Ok(false),
            Err(_) => Err(()),
        }
    };
    let ok = |queued: bool| {
        json_reply(
            if queued { 202 } else { 200 },
            &Appended {
                ok: true,
                queued,
                xid,
                day: req.day.format("%Y-%m-%d").to_string(),
                document: &document,
            },
        )
    };
    // A repeated requestId returns the same xid without writing again.
    match has(&document) {
        Ok(true) => return ok(false),
        Ok(false) => {}
        Err(()) => return internal(),
    }
    match log_exchange(
        c,
        req.day,
        (req.now.hour(), req.now.minute()),
        &req.title,
        &req.text,
        xid,
    ) {
        Ok(true) => {}
        // NativeWriteUnavailable: 503 {"error": ...}.
        Ok(false) => return error(503, PENDING_NATIVE),
        Err(()) => return internal(),
    }
    match has(&document) {
        Ok(true) => ok(false),
        _ => ok(true),
    }
}

/// CorpusStore.log_exchange with the native transform: journal the exchange, then apply it.
/// Ok(true): applied; Ok(false): the journal holds it (storage refused it); Err: nothing usable.
#[allow(clippy::result_unit_err)]
pub fn log_exchange(
    c: &Ctx<'_>,
    day: NaiveDate,
    (hour, minute): (u32, u32),
    title: &str,
    text: &str,
    xid: &str,
) -> Result<bool, ()> {
    let l = c.layout;
    let document = corpus::document_path(l, day);
    let time = chrono::NaiveTime::from_hms_opt(hour, minute, 0).ok_or(())?;
    let rendered = cw::render_subsection_header(time, py::strip(title));
    let sub_header = rendered.strip_prefix("### ").map(str::to_owned).ok_or(())?;
    let body = cw::render_exchange(text, "", xid);
    let scaffold = match (c.journal.has_exchange(), corpus::list_months(c.reader, l)) {
        (Ok(has), Ok(months)) => !has && months.is_empty(),
        _ => return Err(()),
    };
    let month = corpus::month_filename(l, day);
    let label = format!("{} {}", corpus::month_name(day.month()), day.year());
    let day_iso = day.format("%Y-%m-%d").to_string();
    let payload = write::py_json(&[
        ("scaffold", PyVal::Bool(scaffold)),
        ("xid", PyVal::Str(xid)),
        ("day", PyVal::Str(&day_iso)),
        ("sub_header", PyVal::Str(&sub_header)),
        ("body", PyVal::Str(&body)),
        ("month", PyVal::Str(&month)),
        ("document", PyVal::Str(&document)),
        ("month_label", PyVal::Str(&label)),
    ]);
    // The index refresh Python runs in the background is left to Python's next _reindex_dirty.
    // The document is marked dirty BEFORE anything is written (as file_write and edit do), so a
    // process lost at any later point (after the corpus commit, or after the journal row is marked
    // applied) still leaves its retrieval invalidation behind. A failed write leaves one extra
    // reindex, which is harmless.
    c.journal.mark_dirty(&document).map_err(|_| ())?;
    let id = c.journal.enqueue("exchange", &payload).map_err(|_| ())?;
    apply_entry(c, &id, || {
        c.store.guarded_write(&document, |current| {
            if let Some(text) = current.filter(|t| cw::has_marker(t, xid)) {
                return Ok((text.to_string(), false));
            }
            // apply_append with the canonical day header.
            cw::append_to_month_text(current, day, &sub_header, &body)
                .map(|t| (t, true))
                .ok_or_else(|| "invalid day".to_string())
        })?;
        if scaffold {
            seed(c)?;
        }
        Ok(())
    })
    .map_err(|_| ())
}

/// CorpusStore._scaffold: create-only seed files.
fn seed(c: &Ctx<'_>) -> Result<(), WriteFailure> {
    let seeds = [
        (
            c.layout.entries_prefix.as_str(),
            "Diary entries are saved in the configured daily or monthly layout.\n",
        ),
        (
            "AI Memory",
            "Keep durable diary memory and context as Markdown files here.\n",
        ),
        (
            "Raw Sources",
            "Keep original reference material for your diary here.\n",
        ),
    ];
    for (folder, text) in seeds {
        let path = corpus::join(&[folder, "README.md"]);
        c.store.guarded_write(&path, |current| match current {
            None => Ok((text.to_string(), true)),
            Some(t) => Ok((t.to_string(), false)),
        })?;
    }
    Ok(())
}

// ------------------------------------------------------------------ POST /api/entries/edit

pub struct EditReq {
    xid: String,
    me: String,
    assistant: String,
    month: Option<String>,
    base_hash: Option<String>,
}

/// The EditEntryRequest model (pydantic): None when Python would answer 422 (or might read the
/// body differently): decline.
pub fn edit_model(body: &Map<String, Value>) -> Option<EditReq> {
    let s = |k: &str| match body.get(k) {
        Some(Value::String(v)) => Some(v.clone()),
        _ => None,
    };
    let opt = |k: &str| match body.get(k) {
        None | Some(Value::Null) => Some(None),
        Some(Value::String(v)) => Some(Some(v.clone())),
        _ => None,
    };
    Some(EditReq {
        xid: s("xid")?,
        me: s("me")?,
        assistant: match body.get("assistant") {
            None => String::new(),
            Some(Value::String(v)) => v.clone(),
            _ => return None,
        },
        month: opt("month")?,
        base_hash: opt("base_hash")?,
    })
}

/// MONTH_ID_RE `^\d{4}-(0[1-9]|1[0-2])$`: Some(true/false) when certain; None for a value
/// Python's Unicode `\d` or `$` could read differently.
fn month_id_ok(m: &str) -> Option<bool> {
    if !m.is_ascii() || m.ends_with('\n') {
        return None;
    }
    let b = m.as_bytes();
    Some(
        b.len() == 7
            && b.iter().take(4).all(u8::is_ascii_digit)
            && b.get(4) == Some(&b'-')
            && matches!(
                (b.get(5), b.get(6)),
                (Some(b'0'), Some(b'1'..=b'9')) | (Some(b'1'), Some(b'0'..=b'2'))
            ),
    )
}

/// api_entries_edit's checks before tenant resolution.
pub fn edit_validate(req: &EditReq) -> Result<(), Reply> {
    let xid = py::strip(&req.xid);
    let xid_ok =
        (8..=64).contains(&xid.len()) && xid.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-');
    if !xid_ok {
        return Err(error(400, "invalid xid"));
    }
    if py::strip(&req.me).is_empty() {
        return Err(error(400, "me is required"));
    }
    if let Some(m) = &req.month {
        match month_id_ok(m) {
            Some(true) => {}
            Some(false) => return Err(error(400, "month must be YYYY-MM")),
            None => return Err(Reply::Decline),
        }
    }
    if let Some(h) = &req.base_hash {
        let h = py::strip(h);
        if !(h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit())) {
            return Err(error(400, "invalid base_hash"));
        }
    }
    Ok(())
}

/// CorpusStore._documents_for_month. Err: a listing failed (Python logs and skips the month).
fn documents_for_month(c: &Ctx<'_>, year: i32, month: u32) -> Result<Vec<(String, NaiveDate)>, ()> {
    let Some(first) = NaiveDate::from_ymd_opt(year, month, 1) else {
        return Ok(Vec::new());
    };
    let l = c.layout;
    if !l.daily {
        return Ok(vec![(corpus::document_path(l, first), first)]);
    }
    let name = corpus::month_name(month);
    let dir = corpus::join(&[&l.entries_prefix, &year.to_string(), name]);
    let entries = c.reader.list_dir(&dir).map_err(|_| ())?;
    let mut docs = Vec::new();
    for e in entries {
        if e.is_dir {
            continue;
        }
        let Some(d) = corpus::daily_name(&e.name, name, year as u32) else {
            continue;
        };
        if (1..=corpus::days_in_month(year, month)).contains(&d) {
            if let Some(day) = NaiveDate::from_ymd_opt(year, month, d) {
                docs.push((corpus::daily_path(l, day), day));
            }
        }
    }
    docs.sort_by_key(|(_, d)| *d);
    Ok(docs)
}

/// CorpusStore._find_document_with_xid. `errors` is set when a listing or read failed.
fn find_document(
    c: &Ctx<'_>,
    xid: &str,
    month: Option<&str>,
    errors: &mut bool,
) -> Result<Option<(String, NaiveDate)>, ()> {
    let parse = |m: &str| -> Option<(i32, u32)> {
        Some((m.get(..4)?.parse().ok()?, m.get(5..7)?.parse().ok()?))
    };
    let mut candidates = Vec::new();
    match month.filter(|m| !m.is_empty()) {
        Some(m) => {
            let (y, mo) = parse(m).ok_or(())?;
            match documents_for_month(c, y, mo) {
                Ok(d) => candidates = d,
                Err(()) => *errors = true,
            }
        }
        None => {
            let months = corpus::list_months(c.reader, c.layout).map_err(|_| ())?;
            for item in months.iter().rev() {
                let (y, mo) = parse(&item.id).ok_or(())?;
                match documents_for_month(c, y, mo) {
                    Ok(d) => candidates.extend(d.into_iter().rev()),
                    Err(()) => *errors = true,
                }
            }
        }
    }
    for (path, day) in candidates {
        match c.store.get(&path) {
            Ok(Some((data, _))) => {
                let text = String::from_utf8_lossy(&data);
                if !text.is_empty() && cw::has_marker(&text, xid) {
                    return Ok(Some((path, day)));
                }
            }
            Ok(None) => {}
            Err(_) => *errors = true,
        }
    }
    Ok(None)
}

#[derive(Serialize)]
struct Edited<'a> {
    ok: bool,
    document: &'a str,
    day: String,
    hash: Option<String>,
}

#[derive(Serialize)]
struct Conflict {
    error: &'static str,
    conflict: bool,
    current_hash: Option<String>,
    current_text: Option<String>,
}

/// CorpusStore.edit_exchange (native transform) and the rest of api_entries_edit.
pub fn edit(c: &Ctx<'_>, req: &EditReq) -> Reply {
    let xid = py::strip(&req.xid).to_string();
    let month = req.month.as_deref();
    let mut scan_errors = false;
    let found = match find_document(c, &xid, month, &mut scan_errors) {
        Ok(Some(f)) => f,
        Ok(None) => {
            return error(404, &format!("no corpus document contains exchange {xid}"));
        }
        Err(()) => return error(502, "edit failed"),
    };
    let original = match c.store.get(&found.0) {
        Ok(d) => d.map(|(data, _)| String::from_utf8_lossy(&data).into_owned()),
        Err(_) => return error(502, "edit failed"),
    };
    let edited = original
        .as_deref()
        .and_then(|t| cw::replace_exchange_text(t, &xid, &req.me, &req.assistant));
    let Some(edited) = edited else {
        return error(404, MALFORMED);
    };
    let current_hash = cw::exchange_hash(original.as_deref(), &xid);
    let edited_hash = cw::exchange_hash(Some(&edited), &xid);
    if let Some(base) = &req.base_hash {
        if current_hash.as_deref() != Some(py::strip(base).to_lowercase().as_str()) {
            return json_reply(
                409,
                &Conflict {
                    error: "The entry changed since it was opened; reload and retry",
                    conflict: true,
                    current_hash,
                    current_text: cw::exchange_text(original.as_deref(), &xid),
                },
            );
        }
        // An earlier queued edit for this exchange cannot exist: the tenant had nothing pending.
    }
    let mut pairs = vec![
        ("xid", PyVal::Str(&xid)),
        ("new_me", PyVal::Str(&req.me)),
        ("new_claude", PyVal::Str(&req.assistant)),
    ];
    if let Some(m) = month.filter(|m| !m.is_empty()) {
        pairs.push(("month", PyVal::Str(m)));
    }
    let Ok(id) = c.journal.enqueue("exchange_edit", &write::py_json(&pairs)) else {
        return error(502, "edit failed");
    };
    let applied = apply_entry(c, &id, || {
        // _apply_exchange_edit: discover again (a replay cannot rely on the pre-check).
        let mut errors = false;
        let target = match find_document(c, &xid, month, &mut errors) {
            Ok(Some((path, _))) => path,
            Ok(None) if errors => {
                return Err(WriteFailure::Infra(write::Error(format!(
                    "no corpus document contains exchange {xid} (scan incomplete)"
                ))))
            }
            Ok(None) => {
                return Err(WriteFailure::Permanent(format!(
                    "no corpus document contains exchange {xid}"
                )))
            }
            Err(()) => return Err(WriteFailure::Infra(write::Error("scan failed".into()))),
        };
        c.journal.mark_dirty(&target).map_err(WriteFailure::Infra)?;
        c.store.guarded_write(&target, |current| {
            let Some(current) = current else {
                return Err("The entry no longer exists".into());
            };
            match cw::replace_exchange_text(current, &xid, &req.me, &req.assistant) {
                Some(text) => {
                    let changed = text != current;
                    Ok((text, changed))
                }
                None => Err(MALFORMED.into()),
            }
        })
    });
    match applied {
        Ok(true) => {}
        Ok(false) => return error(503, EDIT_QUEUED),
        Err(_) => return error(502, "edit failed"),
    }
    json_reply(
        200,
        &Edited {
            ok: true,
            document: &found.0,
            day: found.1.format("%Y-%m-%d").to_string(),
            hash: edited_hash,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_regex_matches_python() {
        assert!(has_any_marker("a <!-- xid:abc --> b"));
        assert!(has_any_marker("<!--xid:abc-->"));
        assert!(has_any_marker("<!--\u{1c}xid:a-b\u{3000}-->"));
        assert!(!has_any_marker("<!-- xid: -->"));
        assert!(!has_any_marker("<!-- xid:zz -->"));
        assert!(!has_any_marker("<!-- xid:abc ->"));
    }

    #[test]
    fn heading_regex_matches_python() {
        assert!(has_heading("# x"));
        assert!(has_heading("a\n  \u{a0}# x"));
        assert!(!has_heading("a # b"));
    }

    #[test]
    fn entry_time_forms() {
        assert!(matches!(
            parse_entry_time("2026-11-04T10:15:00+01:00"),
            EntryTime::At(_)
        ));
        assert!(matches!(
            parse_entry_time("2026-11-04T10:15:00.123Z"),
            EntryTime::At(_)
        ));
        assert!(matches!(
            parse_entry_time("2026-11-04T10:15"),
            EntryTime::Invalid
        ));
        assert!(matches!(
            parse_entry_time("2026-02-30T10:15:00Z"),
            EntryTime::Invalid
        ));
        assert!(matches!(
            parse_entry_time("2026-11-04 10:15:00Z"),
            EntryTime::Unsure
        ));
        assert!(matches!(parse_entry_time("tomorrow"), EntryTime::Unsure));
    }

    #[test]
    fn month_ids() {
        assert_eq!(month_id_ok("2026-11"), Some(true));
        assert_eq!(month_id_ok("2026-13"), Some(false));
        assert_eq!(month_id_ok("2026-11\n"), None);
        assert_eq!(month_id_ok("٢٠٢٦-11"), None);
    }
}
