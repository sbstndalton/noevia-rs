//! Per-model vector slots inside one Diary index file (Python `vector_slots.py`, #1636).
//!
//! ```text
//! vec_meta 'slot:<id>'  JSON {model, profile, dim, table, map}   id = sha256(profile \0 model)[:16]
//! <table>               vec0(chunk_id INTEGER PRIMARY KEY, embedding float[dim])
//! <map>                 (chunk_id INTEGER PRIMARY KEY, content_hash TEXT): the text each vector was
//!                       made from; a vector counts only while its chunk still has that hash.
//! ```
//!
//! The legacy `vec_items` table (+ `vec_meta 'dim'`) is what an index built before #1636 holds
//! (raw text, the nomic-v1 profile). It becomes the slot of the first nomic-v1 identity that opens
//! the file, and a new nomic-v1 slot in a fresh file uses it too, so an older image rolled back to
//! still finds its vectors. It has no map.
//!
//! Table names only ever come from records that pass [`safe`], so the SQL built from them here
//! cannot carry anything but `vec_items`, `vec_<16 hex>` and `vecmap_<16 hex>`.

use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;

use crate::{pyjson, Error, Result, Target};

pub const LEGACY_TABLE: &str = "vec_items";
const MAX_DIM: i64 = 65536;

#[derive(Debug, Clone, PartialEq)]
pub struct Slot {
    pub id: String,
    pub dim: usize,
    pub table: String,
    pub map: Option<String>,
    /// The rest of the stored record (model, profile), kept as read.
    pub model: Value,
    pub profile: Value,
}

/// Python `re.match(r"^(...)$", name)`: `$` also matches before one final newline.
fn strip_final_newline(name: &str) -> &str {
    name.strip_suffix('\n').unwrap_or(name)
}

fn hex16(s: &str) -> bool {
    s.len() == 16 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn table_name_ok(name: &str) -> bool {
    let name = strip_final_newline(name);
    name == LEGACY_TABLE || name.strip_prefix("vec_").is_some_and(hex16)
}

fn map_name_ok(name: &str) -> bool {
    strip_final_newline(name)
        .strip_prefix("vecmap_")
        .is_some_and(hex16)
}

/// Python `str(value)` as far as the name checks can tell: only a JSON string can ever match.
fn as_name(value: Option<&Value>) -> Option<&str> {
    match value {
        Some(Value::String(s)) => Some(s.as_str()),
        _ => None,
    }
}

/// Python `isinstance(dim, int) and 0 < dim <= 65536` (a JSON `true` is the int 1 there).
fn dim_of(value: Option<&Value>) -> Option<usize> {
    let dim = match value? {
        Value::Bool(true) => 1,
        Value::Number(n) => n.as_i64()?,
        _ => return None,
    };
    if 0 < dim && dim <= MAX_DIM {
        usize::try_from(dim).ok()
    } else {
        None
    }
}

/// Python `_safe({"id": id, **record})`. A record carrying its own string `"id"` overrides the
/// key's id, as the dict merge does. Known difference: a non-string `"id"` in a record (never
/// written by any release) is refused here; Python would carry the raw value.
pub fn safe(id: &str, record: &Value) -> Option<Slot> {
    let obj = record.as_object()?;
    let id = match obj.get("id") {
        None => id.to_owned(),
        Some(Value::String(s)) => s.clone(),
        Some(_) => return None,
    };
    let table = as_name(obj.get("table"))?;
    if !table_name_ok(table) {
        return None;
    }
    let map = match obj.get("map") {
        None | Some(Value::Null) => None,
        Some(Value::String(m)) if map_name_ok(m) => Some(m.clone()),
        Some(_) => return None,
    };
    let dim = dim_of(obj.get("dim"))?;
    Some(Slot {
        id,
        dim,
        table: table.to_owned(),
        map,
        model: obj.get("model").cloned().unwrap_or(Value::Null),
        profile: obj.get("profile").cloned().unwrap_or(Value::Null),
    })
}

/// Python `json.loads` of a `vec_meta` value: text or a UTF-8 blob; anything else fails.
///
/// Known differences (no release writes such records): Python's `json.loads` also accepts the
/// `NaN`, `Infinity` and `-Infinity` tokens and UTF-16/32 encoded blobs; such a record is refused
/// here, so a slot only Python could read is not found (and a re-index would create a new one).
fn parse_record(value: rusqlite::types::Value) -> Option<Value> {
    match value {
        rusqlite::types::Value::Text(s) => serde_json::from_str(&s).ok(),
        rusqlite::types::Value::Blob(b) => serde_json::from_slice(&b).ok(),
        _ => None,
    }
}

fn meta(conn: &Connection, key: &str) -> Result<Option<rusqlite::types::Value>> {
    Ok(conn
        .query_row("SELECT value FROM vec_meta WHERE key = ?", [key], |r| {
            r.get(0)
        })
        .optional()?)
}

fn set_meta(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO vec_meta (key, value) VALUES (?, ?)",
        params![key, value],
    )?;
    Ok(())
}

pub fn exists(conn: &Connection, name: &str) -> Result<bool> {
    Ok(conn
        .query_row("SELECT 1 FROM sqlite_master WHERE name = ?", [name], |_| {
            Ok(())
        })
        .optional()?
        .is_some())
}

pub fn list_slots(conn: &Connection) -> Result<Vec<Slot>> {
    let mut stmt = conn.prepare("SELECT key, value FROM vec_meta WHERE key LIKE 'slot:%'")?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, rusqlite::types::Value>(0)?,
            r.get::<_, rusqlite::types::Value>(1)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (key, value) = row?;
        // LIKE matched, so the key is text (or a blob that reads as text) of at least 5 chars.
        let key = match key {
            rusqlite::types::Value::Text(s) => s,
            rusqlite::types::Value::Blob(b) => String::from_utf8_lossy(&b).into_owned(),
            _ => continue,
        };
        let id: String = key.chars().skip(5).collect();
        if let Some(slot) = parse_record(value).and_then(|rec| safe(&id, &rec)) {
            out.push(slot);
        }
    }
    Ok(out)
}

fn legacy_claimed(conn: &Connection) -> Result<bool> {
    Ok(list_slots(conn)?.iter().any(|s| s.table == LEGACY_TABLE))
}

/// The legacy width as Python `int(_meta(conn, "dim") or 0)` reads it (None when that raises).
fn legacy_dim(conn: &Connection) -> Result<Option<(bool, String)>> {
    Ok(match meta(conn, "dim")? {
        None | Some(rusqlite::types::Value::Null) => Some((false, "0".to_owned())),
        Some(rusqlite::types::Value::Integer(i)) => Some((i < 0, i.unsigned_abs().to_string())),
        Some(rusqlite::types::Value::Text(s)) if s.is_empty() => Some((false, "0".to_owned())),
        Some(rusqlite::types::Value::Text(s)) => pyjson::py_int(&s),
        // int(b"16") reads ASCII bytes the same way.
        Some(rusqlite::types::Value::Blob(b)) if b.is_empty() => Some((false, "0".to_owned())),
        Some(rusqlite::types::Value::Blob(b)) => std::str::from_utf8(&b)
            .ok()
            .filter(|s| s.is_ascii())
            .and_then(pyjson::py_int),
        // int(float) truncates; a stored REAL width is not something any release writes.
        Some(rusqlite::types::Value::Real(f)) if f.is_finite() => {
            let t = f.trunc();
            Some((t < 0.0, format!("{:.0}", t.abs())))
        }
        // int(nan) is the ValueError the Python catches; int(inf) is an OverflowError it does not.
        Some(rusqlite::types::Value::Real(f)) if f.is_nan() => None,
        Some(rusqlite::types::Value::Real(_)) => {
            return Err(Error::Value("legacy width is infinite"))
        }
    })
}

fn positive(dim: &(bool, String)) -> bool {
    !dim.0 && dim.1 != "0"
}

/// Read-only readiness probe; any actual adoption needs the caller's publication guard.
pub fn needs_legacy_adoption(conn: &Connection, active: Option<&Target>) -> Result<bool> {
    let Some(active) = active else {
        return Ok(false);
    };
    if active.profile != "nomic-v1" {
        return Ok(false);
    }
    let Some(dim) = legacy_dim(conn)? else {
        return Ok(false);
    };
    Ok(positive(&dim)
        && exists(conn, LEGACY_TABLE)?
        && !legacy_claimed(conn)?
        && !meta_truthy(conn, &format!("slot:{}", active.slot_id()))?)
}

/// Python `not _meta(conn, key)`: a missing row, NULL, empty text, 0 or an empty blob is falsy.
fn meta_truthy(conn: &Connection, key: &str) -> Result<bool> {
    Ok(match meta(conn, key)? {
        None | Some(rusqlite::types::Value::Null) => false,
        Some(rusqlite::types::Value::Integer(i)) => i != 0,
        Some(rusqlite::types::Value::Real(f)) => f != 0.0,
        Some(rusqlite::types::Value::Text(s)) => !s.is_empty(),
        Some(rusqlite::types::Value::Blob(b)) => !b.is_empty(),
    })
}

/// Records the legacy table as `active`'s slot, under an already-covered write transaction.
pub fn adopt_legacy(conn: &Connection, active: Option<&Target>) -> Result<()> {
    let Some(target) = active else {
        return Ok(());
    };
    if !needs_legacy_adoption(conn, active)? {
        return Ok(());
    }
    let Some((_, dim)) = legacy_dim(conn)? else {
        return Ok(());
    };
    let record = pyjson::slot_record(&target.model, &target.profile, &dim, LEGACY_TABLE, None);
    set_meta(conn, &format!("slot:{}", target.slot_id()), &record)
}

pub fn find_slot(conn: &Connection, target: Option<&Target>) -> Result<Option<Slot>> {
    let Some(target) = target else {
        return Ok(None);
    };
    let id = target.slot_id();
    let raw = meta(conn, &format!("slot:{id}"))?;
    let truthy = match &raw {
        None | Some(rusqlite::types::Value::Null) => false,
        Some(rusqlite::types::Value::Text(s)) => !s.is_empty(),
        Some(rusqlite::types::Value::Blob(b)) => !b.is_empty(),
        Some(rusqlite::types::Value::Integer(i)) => *i != 0,
        Some(rusqlite::types::Value::Real(f)) => *f != 0.0,
    };
    if !truthy {
        return Ok(None);
    }
    Ok(raw.and_then(parse_record).and_then(|rec| safe(&id, &rec)))
}

/// `target`'s slot, created for `dim`-wide vectors if needed. Call inside a write transaction.
pub fn ensure_slot(conn: &Connection, target: &Target, dim: usize) -> Result<Slot> {
    if let Some(found) = find_slot(conn, Some(target))? {
        return Ok(found);
    }
    if dim == 0 || dim > 65536 {
        return Err(Error::BadWidth(dim));
    }
    let id = target.slot_id();
    let (table, map) = if target.profile == "nomic-v1"
        && !legacy_claimed(conn)?
        && !exists(conn, LEGACY_TABLE)?
    {
        conn.execute_batch(&format!(
            "CREATE VIRTUAL TABLE IF NOT EXISTS {LEGACY_TABLE} USING vec0(chunk_id INTEGER PRIMARY KEY, embedding float[{dim}])"
        ))?;
        set_meta(conn, "dim", &dim.to_string())?;
        (LEGACY_TABLE.to_owned(), None)
    } else {
        let table = format!("vec_{id}");
        let map = format!("vecmap_{id}");
        conn.execute_batch(&format!(
            "CREATE VIRTUAL TABLE IF NOT EXISTS {table} USING vec0(chunk_id INTEGER PRIMARY KEY, embedding float[{dim}])"
        ))?;
        conn.execute_batch(&format!(
            "CREATE TABLE IF NOT EXISTS {map} (chunk_id INTEGER PRIMARY KEY, content_hash TEXT NOT NULL)"
        ))?;
        (table, Some(map))
    };
    let record = pyjson::slot_record(
        &target.model,
        &target.profile,
        &dim.to_string(),
        &table,
        map.as_deref(),
    );
    set_meta(conn, &format!("slot:{id}"), &record)?;
    Ok(Slot {
        id,
        dim,
        table,
        map,
        model: Value::String(target.model.clone()),
        profile: Value::String(target.profile.clone()),
    })
}

/// Python `struct.pack(f"{n}f", *vec)` as CPython 3.12 (the Diary image) does it: each value
/// rounded to the nearest f32, a finite value beyond the f32 range packed as infinity. (CPython
/// 3.14 raises `OverflowError` there instead; the recorded fixtures pin the 3.12 behaviour.)
pub fn serialize_f32(vec: &[f64]) -> Vec<u8> {
    let mut out = Vec::with_capacity(vec.len().saturating_mul(4));
    for &x in vec {
        #[allow(clippy::cast_possible_truncation)]
        let y = x as f32;
        out.extend_from_slice(&y.to_le_bytes());
    }
    out
}

/// rows: (chunk id, content hash, vector). Call inside a write transaction. Returns rows written;
/// a vector of the wrong width is skipped.
pub fn write_vectors(
    conn: &Connection,
    slot: &Slot,
    rows: &[(i64, &str, &[f64])],
) -> Result<usize> {
    let mut n = 0;
    for (chunk_id, content_hash, vector) in rows {
        if vector.len() != slot.dim {
            continue;
        }
        let blob = serialize_f32(vector);
        conn.execute(
            &format!("DELETE FROM {} WHERE chunk_id = ?", slot.table),
            [chunk_id],
        )?;
        conn.execute(
            &format!(
                "INSERT INTO {} (chunk_id, embedding) VALUES (?, ?)",
                slot.table
            ),
            params![chunk_id, blob],
        )?;
        if let Some(map) = &slot.map {
            conn.execute(
                &format!("INSERT OR REPLACE INTO {map} (chunk_id, content_hash) VALUES (?, ?)"),
                params![chunk_id, content_hash],
            )?;
        }
        n += 1;
    }
    Ok(n)
}

/// Deletes these chunks' vectors from every slot (and the legacy table). Inside a transaction.
pub fn drop_chunks(conn: &Connection, ids: &[i64]) -> Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let mut tables: Vec<(String, Option<String>)> = list_slots(conn)?
        .into_iter()
        .map(|s| (s.table, s.map))
        .collect();
    if !tables.iter().any(|(t, _)| t == LEGACY_TABLE) && exists(conn, LEGACY_TABLE)? {
        tables.push((LEGACY_TABLE.to_owned(), None));
    }
    for (table, map) in tables {
        if !exists(conn, &table)? {
            continue;
        }
        let map = match map {
            Some(m) if exists(conn, &m)? => Some(m),
            _ => None,
        };
        for chunk_id in ids {
            conn.execute(
                &format!("DELETE FROM {table} WHERE chunk_id = ?"),
                [chunk_id],
            )?;
            if let Some(m) = &map {
                conn.execute(&format!("DELETE FROM {m} WHERE chunk_id = ?"), [chunk_id])?;
            }
        }
    }
    Ok(())
}

fn missing_sql(slot: Option<&Slot>) -> String {
    match slot {
        None => "SELECT c.id FROM chunks c".to_owned(),
        Some(Slot { map: Some(map), .. }) => format!(
            "SELECT c.id FROM chunks c LEFT JOIN {map} m ON m.chunk_id = c.id AND m.content_hash = c.content_hash WHERE m.chunk_id IS NULL"
        ),
        Some(slot) => format!(
            "SELECT c.id FROM chunks c WHERE c.id NOT IN (SELECT chunk_id FROM {})",
            slot.table
        ),
    }
}

pub fn has_vector(
    conn: &Connection,
    slot: Option<&Slot>,
    chunk_id: i64,
    content_hash: &str,
) -> Result<bool> {
    let Some(slot) = slot else {
        return Ok(false);
    };
    let found = match &slot.map {
        Some(map) => conn
            .query_row(
                &format!("SELECT 1 FROM {map} WHERE chunk_id = ? AND content_hash = ?"),
                params![chunk_id, content_hash],
                |_| Ok(()),
            )
            .optional()?,
        None => conn
            .query_row(
                &format!("SELECT 1 FROM {} WHERE chunk_id = ?", slot.table),
                [chunk_id],
                |_| Ok(()),
            )
            .optional()?,
    };
    Ok(found.is_some())
}

/// A chunk with no current vector in a slot: (id, body, content hash).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Missing {
    pub id: i64,
    pub body: String,
    pub content_hash: String,
}

/// Chunks with no current vector in `slot`, minus `exclude`, in id order.
pub fn missing_chunks(
    conn: &Connection,
    slot: Option<&Slot>,
    limit: i64,
    exclude: &[i64],
) -> Result<Vec<Missing>> {
    let skip: Vec<String> = exclude.iter().map(i64::to_string).collect();
    let extra = if skip.is_empty() {
        String::new()
    } else {
        format!(" AND c.id NOT IN ({})", skip.join(","))
    };
    let sql = format!(
        "SELECT c.id, c.body, c.content_hash FROM chunks c WHERE c.id IN ({}){extra} ORDER BY c.id LIMIT ?",
        missing_sql(slot)
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([limit], |r| {
        Ok(Missing {
            id: r.get(0)?,
            body: r.get(1)?,
            content_hash: r.get(2)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Coverage {
    pub chunks: i64,
    pub vectors: i64,
}

pub fn coverage(conn: &Connection, slot: Option<&Slot>) -> Result<Coverage> {
    let chunks: i64 = conn.query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0))?;
    let missing: i64 = conn.query_row(
        &format!("SELECT COUNT(*) FROM ({})", missing_sql(slot)),
        [],
        |r| r.get(0),
    )?;
    Ok(Coverage {
        chunks,
        vectors: chunks - missing,
    })
}

/// FROM clause joining a slot's current vectors (v) to chunks (c).
pub fn join_sql(slot: &Slot) -> String {
    match &slot.map {
        Some(map) => format!(
            "{} v JOIN chunks c ON c.id = v.chunk_id JOIN {map} m ON m.chunk_id = c.id AND m.content_hash = c.content_hash",
            slot.table
        ),
        None => format!("{} v JOIN chunks c ON c.id = v.chunk_id", slot.table),
    }
}

/// Removes `target`'s slot. The legacy table stays (an older image opens it) but is emptied.
pub fn drop_slot(conn: &Connection, target: &Target) -> Result<bool> {
    let Some(slot) = find_slot(conn, Some(target))? else {
        return Ok(false);
    };
    if slot.table == LEGACY_TABLE {
        if exists(conn, LEGACY_TABLE)? {
            conn.execute_batch(&format!("DELETE FROM {LEGACY_TABLE}"))?;
        }
    } else {
        conn.execute_batch(&format!("DROP TABLE IF EXISTS {}", slot.table))?;
        if let Some(map) = &slot.map {
            conn.execute_batch(&format!("DROP TABLE IF EXISTS {map}"))?;
        }
    }
    conn.execute(
        "DELETE FROM vec_meta WHERE key = ?",
        [format!("slot:{}", slot.id)],
    )?;
    Ok(true)
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn safe_follows_python_name_and_width_rules() {
        let ok = json!({"model": "m", "profile": "nomic-v1", "dim": 16, "table": "vec_items", "map": null});
        assert!(safe("x", &ok).is_some());
        for (field, value) in [
            ("table", json!("vec_items; DROP TABLE chunks")),
            ("table", json!("vec_0123456789ABCDEF")),
            ("table", json!(17)),
            ("map", json!("vecmap_short")),
            ("map", json!("")),
            ("dim", json!(0)),
            ("dim", json!(65537)),
            ("dim", json!(16.0)),
            ("dim", json!("16")),
            ("dim", json!(false)),
        ] {
            let mut rec = ok.clone();
            rec[field] = value;
            assert!(safe("x", &rec).is_none(), "{field} {rec}");
        }
        let mut rec = ok.clone();
        rec["dim"] = json!(true);
        assert_eq!(safe("x", &rec).map(|s| s.dim), Some(1));
        rec["table"] = json!("vec_items\n");
        assert!(
            safe("x", &rec).is_some(),
            "Python $ allows one final newline"
        );
        rec["table"] = json!("vec_items\n\n");
        assert!(safe("x", &rec).is_none());
    }

    #[test]
    fn serialize_packs_like_cpython_312() {
        // Recorded with CPython 3.12.15: struct.pack('1f', 1e39).hex() == '0000807f'.
        assert_eq!(serialize_f32(&[1e39]), f32::INFINITY.to_le_bytes().to_vec());
        assert_eq!(serialize_f32(&[1.0]), 1.0f32.to_le_bytes().to_vec());
    }
}
