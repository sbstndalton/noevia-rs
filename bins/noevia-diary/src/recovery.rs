//! Managed journal recovery. The caller holds both the forwarding writer token and
//! the native tenant lock for this entire synchronous operation. Corpus commits precede
//! journal completion; xid transforms make a restart in between idempotent.
use crate::write::{self, Entry, WriteFailure};
use crate::writes::Ctx;
use chrono::{Datelike, NaiveDate};
use diary_corpus_write as cw;
use diary_index_transform as ix;
use serde_json::Value;
use std::collections::HashSet;
use std::time::{Duration, Instant};

#[derive(Default)]
pub struct State {
    pub recovered: bool,
    transient_until: Option<Instant>,
}

fn permanent(s: impl Into<String>) -> WriteFailure {
    WriteFailure::Permanent(s.into())
}
fn field<'a>(p: &'a Value, key: &str) -> Result<&'a str, WriteFailure> {
    match p.get(key) {
        None => Err(permanent(format!("'{key}'"))),
        Some(Value::String(s)) => Ok(s),
        _ => Err(permanent(format!("invalid {key}"))),
    }
}

fn edits(p: &Value, key: &str) -> Result<Vec<ix::EditOp>, WriteFailure> {
    let Some(v) = p.get(key).filter(|v| truthy(v)) else {
        return Ok(vec![]);
    };
    let a = v
        .as_array()
        .ok_or_else(|| permanent("invalid index operations"))?;
    a.iter()
        .map(|v| {
            Ok(ix::EditOp {
                action: v.get("action").and_then(Value::as_str).unwrap_or("").into(),
                text: field(v, "text")?.into(),
                replacement: match v.get("replacement") {
                    None => None,
                    Some(Value::String(s)) => Some(Some(s.clone())),
                    _ => return Err(permanent("invalid replacement")),
                },
                date: v.get("date").and_then(Value::as_str).map(str::to_owned),
            })
        })
        .collect()
}

fn apply(c: &Ctx<'_>, e: &Entry, p: &Value) -> Result<(), WriteFailure> {
    match e.kind.as_str() {
        "exchange" => {
            let raw = field(p, "day")?;
            let day = NaiveDate::parse_from_str(raw, "%Y-%m-%d")
                .map_err(|_| permanent(format!("Invalid isoformat string: '{raw}'")))?;
            let xid = field(p, "xid")?;
            let header = field(p, "sub_header")?;
            let body = field(p, "body")?;
            let path = crate::corpus::document_path(c.layout, day);
            // S6b (named difference: Python's replay marks nothing and relies on its sweep, which
            // runs once here): the replayed document is marked before it is written, as every
            // native write marks its own.
            c.journal.mark_dirty(&path).map_err(WriteFailure::Infra)?;
            c.store.guarded_write(&path, |current| {
                if let Some(t) = current.filter(|t| cw::has_marker(t, xid)) {
                    return Ok((t.into(), false));
                }
                cw::append_to_month_text(current, day, header, body)
                    .map(|t| (t, true))
                    .ok_or_else(|| "invalid day".into())
            })?;
            if p.get("scaffold").is_some_and(truthy) {
                crate::writes::seed(c)?;
            }
            Ok(())
        }
        "exchange_edit" => {
            let xid = field(p, "xid")?;
            let mut errors = false;
            let found = crate::writes::find_document(
                c,
                xid,
                p.get("month").and_then(Value::as_str),
                &mut errors,
            )
            .map_err(|_| WriteFailure::Infra(write::Error::other("scan failed")))?;
            let path = match found {
                Some((path, _)) => path,
                None if errors => {
                    return Err(WriteFailure::Infra(write::Error::other(format!(
                        "no corpus document contains exchange {xid} (scan incomplete)"
                    ))))
                }
                None => {
                    return Err(permanent(format!(
                        "no corpus document contains exchange {xid}"
                    )))
                }
            };
            c.journal.mark_dirty(&path).map_err(WriteFailure::Infra)?;
            let me = field(p, "new_me")?;
            let assistant = field(p, "new_claude")?;
            c.store.guarded_write(&path, |current| {
                let current = current.ok_or("The entry no longer exists")?;
                let text = cw::replace_exchange_text(current, xid, me, assistant)
                    .ok_or("The entry cannot be edited safely: malformed or missing block")?;
                let changed = text != current;
                Ok((text, changed))
            })
        }
        "index_month" | "index_update" => {
            let path = crate::corpus::join(&[&c.layout.monthly_prefix, &c.layout.index_file]);
            c.journal.mark_dirty(&path).map_err(WriteFailure::Infra)?;
            c.store.guarded_write(&path, |current| {
                let mut idx = ix::parse_index(current).map_err(|e| e.to_string())?;
                let changed = if e.kind == "index_month" {
                    let label = field(p, "label").map_err(message)?;
                    let month = field(p, "month").map_err(message)?;
                    ix::register_month(&mut idx, label, month)
                } else {
                    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
                    ix::apply_index_edits(
                        &mut idx,
                        &edits(p, "open_questions").map_err(message)?,
                        &edits(p, "timeline").map_err(message)?,
                        p.get("today").and_then(Value::as_str).unwrap_or(&today),
                    )
                }
                .map_err(|e| e.to_string())?;
                Ok((
                    if changed {
                        idx.render().map_err(|e| e.to_string())?
                    } else {
                        current.unwrap_or("").into()
                    },
                    changed,
                ))
            })
        }
        _ => Err(permanent(format!("unknown journal kind: {}", e.kind))),
    }
}
fn message(e: WriteFailure) -> String {
    match e {
        WriteFailure::Permanent(s) => s,
        WriteFailure::Infra(e) | WriteFailure::Transient(e) => e.into_message(),
    }
}
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

/// Python decodes the complete selected batch before applying anything. Invalid JSON
/// therefore aborts recovery without changing any row (it is not a quarantine case).
pub fn apply_pending(c: &Ctx<'_>, limit: i64) -> Result<usize, write::Error> {
    let mut state = c.recovery.lock().unwrap_or_else(|e| e.into_inner());
    if state
        .transient_until
        .is_some_and(|until| Instant::now() < until)
    {
        return Ok(0);
    }
    let entries = c.journal.unapplied(limit)?;
    let decoded: Vec<Value> = entries
        .iter()
        .map(|e| serde_json::from_str(&e.payload))
        .collect::<Result<_, _>>()?;
    let mut retired = HashSet::new();
    let mut applied = 0;
    for (e, p) in entries.iter().zip(&decoded) {
        if retired.contains(&e.id) {
            continue;
        }
        match apply(c, e, p) {
            Ok(()) => {
                c.journal.mark_applied(&e.id)?;
                applied += 1;
            }
            Err(failure) => {
                let permanent = matches!(&failure, WriteFailure::Permanent(_))
                    || (!matches!(&failure, WriteFailure::Transient(_)) && e.attempts >= 19);
                if matches!(&failure, WriteFailure::Transient(_)) {
                    state.transient_until = Some(Instant::now() + Duration::from_secs(30));
                }
                let msg = message(failure);
                if !permanent {
                    c.journal.mark_failed(&e.id, &msg)?;
                    break;
                }
                c.journal
                    .quarantine(&e.id, &format!("quarantined: {msg}"))?;
                if let Some(xid) = p.get("xid").filter(|v| truthy(v)) {
                    // Dependents beyond this pass's limit must also be retired, in rowid order.
                    for dependent in c.journal.unapplied(-1)? {
                        let dp: Value = serde_json::from_str(&dependent.payload)?;
                        if dependent.rowid > e.rowid && dp.get("xid") == Some(xid) {
                            c.journal.quarantine(
                                &dependent.id,
                                &format!("quarantined: depends on quarantined {}", e.id),
                            )?;
                            retired.insert(dependent.id);
                        }
                    }
                }
            }
        }
    }
    Ok(applied)
}

/// Transfer each outbox path only after its dirty mark is durable. A crash before
/// the managed transaction commits leaves the row for a harmless repeat transfer.
/// The whole drain runs inside ONE Journal-owned primary reservation (Python's
/// primary_scope borrow in tenant_writer.index_publication): the backup-intent check runs
/// inside it before anything, and every mark re-checks that the reserved primary is still
/// the file it holds.
pub fn drain_index_outbox(c: &Ctx<'_>) -> Result<(), write::Error> {
    crate::reindex::drain_outbox(&c.journal)
}

/// Native replay recovery; retrieval remains Python-owned. Dirty marks are retained
/// until that owner's reindex pass.
///
/// Every Markdown document is marked dirty once per process and tenant, as Python's
/// `_recover_state` does once per tenant state (`dirty_on_recover`): what an earlier process
/// may have written without re-indexing is unknown. Later native writes need no such sweep,
/// because each marks the documents it may write before it writes anything (writes.rs, and
/// the replay in `apply`: Python's replay always runs before a sweep), so a write lost at any
/// point still leaves its invalidation behind. Replay and the outbox drain run every time.
/// As in Python, the READMEs a scaffold seeds are left to the next sweep.
pub fn recover(c: &Ctx<'_>) -> Result<(), write::Error> {
    let swept = c
        .recovery
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .recovered;
    if !swept {
        mark_all_documents(c)?;
    }
    apply_pending(c, 100)?;
    drain_index_outbox(c)?;
    c.recovery
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .recovered = true;
    Ok(())
}

fn mark_all_documents(c: &Ctx<'_>) -> Result<(), write::Error> {
    // Read first and release the primary, as _recover_state does: each mark then takes
    // its own primary reservation, never while this connection still reads it.
    let documents = {
        let db = write::open_rw(c.reader.db_path())?;
        let mut stmt = db.prepare("SELECT path FROM files WHERE lower(path) LIKE '%.md' AND path NOT IN (SELECT path FROM workspace_index_outbox)")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.collect::<Result<Vec<_>, _>>()?
    };
    for path in documents {
        c.journal.mark_dirty(&path)?;
    }
    Ok(())
}

/// Noncanonical legacy payload shapes stay with the reference applier, before any
/// native effect. In particular, do not replace its type/date exception text with
/// parser-specific diagnostics. Missing fields and unknown kinds are handled natively.
pub fn supported(c: &Ctx<'_>) -> bool {
    let mut index = None;
    let Ok(entries) = c.journal.unapplied(-1) else {
        return false;
    };
    for e in entries {
        let Ok(p) = serde_json::from_str::<Value>(&e.payload) else {
            return false;
        };
        let fields: &[&str] = match e.kind.as_str() {
            "exchange" => &["day", "xid", "sub_header", "body"],
            "exchange_edit" => &["xid", "new_me", "new_claude", "month"],
            "index_month" => &["label", "month"],
            "index_update" => &["today"],
            _ => continue,
        };
        if !p.is_object()
            || fields
                .iter()
                .any(|k| p.get(*k).is_some_and(|v| !v.is_string()))
        {
            return false;
        }
        if matches!(e.kind.as_str(), "index_month" | "index_update") {
            if index.is_none() {
                let path = crate::corpus::join(&[&c.layout.monthly_prefix, &c.layout.index_file]);
                let Ok(current) = c.store.get(&path) else {
                    return false;
                };
                let text = current
                    .as_ref()
                    .map(|(data, _)| String::from_utf8_lossy(data));
                let Ok(parsed) = ix::parse_index(text.as_deref()) else {
                    return false;
                };
                index = Some(parsed);
            }
            if let Some(idx) = index.as_mut() {
                let result = if e.kind == "index_month" {
                    match (
                        p.get("label").and_then(Value::as_str),
                        p.get("month").and_then(Value::as_str),
                    ) {
                        (Some(label), Some(month)) => ix::register_month(idx, label, month),
                        _ => continue, // missing field: native quarantine retains the reference error
                    }
                } else {
                    let (Ok(q), Ok(t)) = (edits(&p, "open_questions"), edits(&p, "timeline"))
                    else {
                        return false;
                    };
                    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
                    ix::apply_index_edits(
                        idx,
                        &q,
                        &t,
                        p.get("today").and_then(Value::as_str).unwrap_or(&today),
                    )
                };
                // Capacity bounds belong to the leaf, not to the persisted Python format.
                // Leave an out-of-range document with the reference path, never quarantine it.
                if result.is_err() || idx.render().is_err() {
                    return false;
                }
            }
        }
        if e.kind == "exchange_edit" {
            if let Some(month) = p.get("month").and_then(Value::as_str) {
                // Empty means no hint in the reference applier. Other forms must
                // preserve its integer/calendar exceptions and retry classification.
                if !month.is_empty() {
                    let first = format!("{month}-01");
                    if NaiveDate::parse_from_str(&first, "%Y-%m-%d")
                        .ok()
                        // Year 0 formats back unchanged, but the reference date() raises.
                        .is_none_or(|d| d.year() < 1 || d.format("%Y-%m").to_string() != month)
                    {
                        return false;
                    }
                }
            }
        }
        if e.kind == "exchange" {
            if let Some(day) = p.get("day").and_then(Value::as_str) {
                if NaiveDate::parse_from_str(day, "%Y-%m-%d")
                    .ok()
                    .is_none_or(|d| d.year() < 1 || d.format("%Y-%m-%d").to_string() != day)
                {
                    return false;
                }
            }
        }
        if e.kind == "index_update" {
            for key in ["open_questions", "timeline"] {
                if let Some(v) = p.get(key).filter(|v| truthy(v)) {
                    let Some(ops) = v.as_array() else {
                        return false;
                    };
                    if ops.len() > 1000
                        || ops.iter().any(|op| {
                            !op.is_object()
                                || !op.get("text").is_some_and(Value::is_string)
                                || ["replacement", "date", "action"]
                                    .iter()
                                    .any(|k| op.get(*k).is_some_and(|v| !v.is_string()))
                        })
                    {
                        return false;
                    }
                }
            }
        }
    }
    true
}
