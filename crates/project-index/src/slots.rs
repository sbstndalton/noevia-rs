//! The write side of `server/vector-slots.cjs`: per-model vector tables in one index file.
//!
//! A slot is a `vec_meta` row `slot:<id>` (id: the first 16 hex of sha256 of `profile NUL model`)
//! holding `{ model, profile, dim, table, map }`. The pre-slot table `vec_items` belongs to the
//! legacy raw-text profile and has no hash map; every other slot has `vec_<id>` and `vecmap_<id>`.

use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};

/// The pre-slot vector table.
pub const LEGACY_TABLE: &str = "vec_items";
/// The raw-text framing profile, the only one that existed before slots; only it may own the legacy table.
pub const LEGACY_PROFILE: &str = "nomic-v1";

/// The model a slot holds vectors of.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub profile: String,
    pub model: String,
}

/// One recorded slot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Slot {
    pub id: String,
    pub model: String,
    pub profile: String,
    pub dim: usize,
    pub table: String,
    pub map: Option<String>,
}

type Res<T> = Result<T, String>;
fn err(e: rusqlite::Error) -> String {
    format!("index: {e}")
}

/// `slotId`.
pub fn slot_id(t: &Target) -> String {
    let mut text = t.profile.clone();
    text.push('\0');
    text.push_str(&t.model);
    crate::chunk::hash(&text).chars().take(16).collect()
}

fn hex16(s: &str) -> bool {
    s.len() == 16
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn table_ok(t: &str) -> bool {
    t == LEGACY_TABLE || t.strip_prefix("vec_").is_some_and(hex16)
}

fn map_ok(m: &str) -> bool {
    m.strip_prefix("vecmap_").is_some_and(hex16)
}

/// `safe`: a recorded slot only when its names and width are ones this code would have written.
fn safe(id: &str, raw: &str) -> Option<Slot> {
    let v: Value = serde_json::from_str(raw).ok()?;
    let table = v.get("table")?.as_str()?.to_string();
    let map = match v.get("map") {
        None | Some(Value::Null) => None,
        Some(Value::String(m)) if map_ok(m) => Some(m.clone()),
        Some(_) => return None,
    };
    if !table_ok(&table) {
        return None;
    }
    // Number.isInteger(dim) && 0 < dim <= 65536 (a JSON 768.0 is an integer to JavaScript).
    let dim = v.get("dim")?.as_f64()?;
    if !(dim.fract() == 0.0 && dim > 0.0 && dim <= 65536.0) {
        return None;
    }
    let text = |k: &str| {
        v.get(k)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    Some(Slot {
        id: id.to_string(),
        model: text("model"),
        profile: text("profile"),
        dim: dim as usize,
        table,
        map,
    })
}

fn meta(db: &Connection, key: &str) -> Res<Option<String>> {
    db.query_row("SELECT value FROM vec_meta WHERE key = ?", [key], |r| {
        r.get::<_, Option<String>>(0)
    })
    .optional()
    .map(Option::flatten)
    .map_err(err)
}

fn set_meta(db: &Connection, key: &str, value: &str) -> Res<()> {
    db.execute(
        "INSERT OR REPLACE INTO vec_meta (key, value) VALUES (?, ?)",
        params![key, value],
    )
    .map(|_| ())
    .map_err(err)
}

/// Whether a table (or any schema object) of this name exists.
pub fn table_exists(db: &Connection, name: &str) -> Res<bool> {
    db.query_row("SELECT 1 FROM sqlite_master WHERE name = ?", [name], |_| {
        Ok(())
    })
    .optional()
    .map(|r| r.is_some())
    .map_err(err)
}

/// `listSlots`: every well-formed slot recorded in this file.
pub fn list_slots(db: &Connection) -> Res<Vec<Slot>> {
    let mut st = db
        .prepare("SELECT key, value FROM vec_meta WHERE key LIKE 'slot:%'")
        .map_err(err)?;
    let rows = st
        .query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
        })
        .map_err(err)?;
    let mut out = Vec::new();
    for row in rows {
        let (key, value) = row.map_err(err)?;
        if let Some(slot) = safe(
            key.get(5..).unwrap_or_default(),
            value.as_deref().unwrap_or_default(),
        ) {
            out.push(slot);
        }
    }
    Ok(out)
}

fn legacy_claimed(db: &Connection) -> Res<bool> {
    Ok(list_slots(db)?.iter().any(|s| s.table == LEGACY_TABLE))
}

/// `findSlot`.
pub fn find_slot(db: &Connection, t: &Target) -> Res<Option<Slot>> {
    let id = slot_id(t);
    Ok(meta(db, &format!("slot:{id}"))?.and_then(|raw| safe(&id, &raw)))
}

/// JSON.stringify of a slot record, in Node's key order (`model, profile, dim, table, map`).
fn record(model: &str, profile: &str, dim: &Value, table: &str, map: Option<&str>) -> String {
    let s = |v: &str| Value::from(v).to_string();
    let map = map.map_or_else(|| "null".to_string(), s);
    format!(
        "{{\"model\":{},\"profile\":{},\"dim\":{dim},\"table\":{},\"map\":{map}}}",
        s(model),
        s(profile),
        s(table)
    )
}

/// `adoptLegacy`: records the pre-slot table as the active model's slot when that model uses the
/// legacy profile and nothing has claimed the table yet.
pub fn adopt_legacy(db: &Connection, active: &Target) -> Res<()> {
    if active.profile != LEGACY_PROFILE {
        return Ok(());
    }
    // Number(meta('dim')): a missing or unreadable width leaves the table unclaimed.
    let dim = meta(db, "dim")?
        .and_then(|d| js_number(&d))
        .unwrap_or(f64::NAN);
    if dim.is_nan() || dim <= 0.0 || !table_exists(db, LEGACY_TABLE)? || legacy_claimed(db)? {
        return Ok(());
    }
    let id = slot_id(active);
    if meta(db, &format!("slot:{id}"))?.is_some_and(|v| !v.is_empty()) {
        return Ok(());
    }
    // JSON.stringify of Number(dim): an integer prints without a fraction.
    let dim_json = if dim.fract() == 0.0 && dim.abs() < 9.0e15 {
        json!(dim as i64)
    } else {
        json!(dim)
    };
    set_meta(
        db,
        &format!("slot:{id}"),
        &record(
            &active.model,
            &active.profile,
            &dim_json,
            LEGACY_TABLE,
            None,
        ),
    )
}

/// `Number(text)` for the plain decimal forms a width is written in (empty is 0, as in JavaScript).
fn js_number(text: &str) -> Option<f64> {
    let t = text.trim();
    if t.is_empty() {
        return Some(0.0);
    }
    t.parse::<f64>().ok().filter(|n| n.is_finite())
}

/// `ensureSlot`: the target's slot, created for `dim`-wide vectors when it does not exist.
pub fn ensure_slot(db: &Connection, t: &Target, dim: usize) -> Res<Slot> {
    if let Some(found) = find_slot(db, t)? {
        return Ok(found);
    }
    if dim == 0 || dim > 65536 {
        return Err(format!("bad vector width {dim}"));
    }
    let id = slot_id(t);
    let slot = if t.profile == LEGACY_PROFILE
        && !legacy_claimed(db)?
        && !table_exists(db, LEGACY_TABLE)?
    {
        db.execute_batch(&format!(
            "CREATE VIRTUAL TABLE IF NOT EXISTS {LEGACY_TABLE} USING vec0(chunk_id INTEGER PRIMARY KEY, embedding float[{dim}])"
        ))
        .map_err(err)?;
        set_meta(db, "dim", &dim.to_string())?;
        Slot {
            id: id.clone(),
            model: t.model.clone(),
            profile: t.profile.clone(),
            dim,
            table: LEGACY_TABLE.into(),
            map: None,
        }
    } else {
        let (table, map) = (format!("vec_{id}"), format!("vecmap_{id}"));
        db.execute_batch(&format!(
            "CREATE VIRTUAL TABLE IF NOT EXISTS {table} USING vec0(chunk_id INTEGER PRIMARY KEY, embedding float[{dim}])"
        ))
        .map_err(err)?;
        db.execute_batch(&format!("CREATE TABLE IF NOT EXISTS {map} (chunk_id INTEGER PRIMARY KEY, content_hash TEXT NOT NULL)"))
            .map_err(err)?;
        Slot {
            id: id.clone(),
            model: t.model.clone(),
            profile: t.profile.clone(),
            dim,
            table,
            map: Some(map),
        }
    };
    set_meta(
        db,
        &format!("slot:{id}"),
        &record(
            &slot.model,
            &slot.profile,
            &json!(dim),
            &slot.table,
            slot.map.as_deref(),
        ),
    )?;
    Ok(slot)
}

/// `serializeF32`: little-endian f32 bytes.
pub fn serialize_f32(v: &[f64]) -> Vec<u8> {
    v.iter().flat_map(|x| (*x as f32).to_le_bytes()).collect()
}

/// One vector to store.
#[derive(Clone, Debug)]
pub struct VectorRow {
    pub id: i64,
    pub hash: String,
    pub vector: Vec<f64>,
}

/// `writeVectors`: one vector per chunk; a vector of the wrong width is skipped. Caller holds a transaction.
pub fn write_vectors(db: &Connection, slot: &Slot, rows: &[VectorRow]) -> Res<()> {
    for r in rows {
        if r.vector.len() != slot.dim {
            continue;
        }
        db.execute(
            &format!("DELETE FROM {} WHERE chunk_id = ?", slot.table),
            [r.id],
        )
        .map_err(err)?;
        db.execute(
            &format!(
                "INSERT INTO {} (chunk_id, embedding) VALUES ({}, ?)",
                slot.table, r.id
            ),
            [serialize_f32(&r.vector)],
        )
        .map_err(err)?;
        if let Some(map) = &slot.map {
            db.execute(&format!("DELETE FROM {map} WHERE chunk_id = ?"), [r.id])
                .map_err(err)?;
            db.execute(
                &format!("INSERT INTO {map} (chunk_id, content_hash) VALUES (?, ?)"),
                params![r.id, r.hash],
            )
            .map_err(err)?;
        }
    }
    Ok(())
}

/// `dropChunks`: drops these chunk ids' vectors from every slot (and an unclaimed legacy table).
pub fn drop_chunks(db: &Connection, ids: &[i64]) -> Res<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let mut tables: Vec<(String, Option<String>)> = list_slots(db)?
        .into_iter()
        .map(|s| (s.table, s.map))
        .collect();
    if !tables.iter().any(|(t, _)| t == LEGACY_TABLE) && table_exists(db, LEGACY_TABLE)? {
        tables.push((LEGACY_TABLE.into(), None));
    }
    for (table, map) in tables {
        if !table_exists(db, &table)? {
            continue;
        }
        let map = match map {
            Some(m) if table_exists(db, &m)? => Some(m),
            _ => None,
        };
        for id in ids {
            db.execute(&format!("DELETE FROM {table} WHERE chunk_id = ?"), [id])
                .map_err(err)?;
            if let Some(m) = &map {
                db.execute(&format!("DELETE FROM {m} WHERE chunk_id = ?"), [id])
                    .map_err(err)?;
            }
        }
    }
    Ok(())
}

/// `joinSql`: a slot's current vectors joined to chunks, aliased v and c.
pub fn join_sql(slot: &Slot) -> String {
    match &slot.map {
        Some(m) => format!(
            "{} v JOIN chunks c ON c.id = v.chunk_id JOIN {m} m ON m.chunk_id = c.id AND m.content_hash = c.content_hash",
            slot.table
        ),
        None => format!("{} v JOIN chunks c ON c.id = v.chunk_id", slot.table),
    }
}
