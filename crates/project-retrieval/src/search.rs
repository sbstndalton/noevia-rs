//! The vector half of `searchProject` (`rag.cjs:878-922`) over a project index that Node or the
//! native embedding worker wrote: the active model's slot (`vector-slots.cjs`), the cosine SQL
//! with its score floor and `LIMIT pool × 4`, the version filter, `slice(0, pool)` and the
//! rounding. The query vector comes from the caller (it embeds the question with `kind: query` on
//! the active target); the reranker is not here.
//!
//! Reads only. Node's `openIndex` also writes on every search: it creates the schema when it is
//! missing, adds the four later `chunks` columns to an older index, and records an unclaimed
//! legacy `vec_items` table as the slot of a `nomic-v1` target. Here a missing column reads as
//! NULL (what Node's added column holds) and the unclaimed legacy table is used as that slot
//! without writing its record, so the hits are the same and the file is left as it was.
//!
//! Known difference, kept on purpose for parity (finding filed): the version filter runs after the
//! SQL `LIMIT`, so a file being re-indexed whose stale chunks fill the whole `pool × 4` window
//! leaves fewer than `pool` hits, or none, although other files have matching chunks.

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::hits::Hit;

/// `MIN_SCORE` (`rag.cjs:104`).
pub const MIN_SCORE: f64 = 0.3;
/// `TOP_K` (`rag.cjs:99`): the pool without the reranker.
pub const TOP_K: usize = 6;
/// The legacy vector table every index built before #1636 holds (`vector-slots.cjs:22`).
pub const LEGACY_TABLE: &str = "vec_items";

/// The embedding identity a slot belongs to (`embedding-config.cjs` target, without its address:
/// the slot id is `profile \0 model`, `vector-slots.cjs:34`).
#[derive(Clone, Copy, Debug)]
pub struct Target<'a> {
    pub profile: &'a str,
    pub model: &'a str,
}

/// A slot record (`vec_meta 'slot:<id>'`), checked as `vector-slots.cjs` `safe` checks it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Slot {
    pub table: String,
    pub map: Option<String>,
    pub dim: usize,
}

/// Why a search could not run. Node logs these and answers `[]`; the caller decides.
#[derive(Debug, PartialEq, Eq)]
pub enum Fault {
    /// The index file could not be opened or read.
    Index,
    /// The SQL failed (a damaged slot table, a missing vec0 module).
    Query,
}

/// The wanted version per file name (`rag.cjs` `fileVersions`): `Some(v)` while a re-index to the
/// text with sha256 `v` is requested and not finished, `None` while a delete is not finished.
/// A file without an entry has nothing requested, so every chunk of it is current.
pub type Versions = HashMap<String, Option<String>>;

/// `isCurrentVersion` (`rag.cjs:866-871`).
pub fn is_current(versions: &Versions, file: &str, version: Option<&str>) -> bool {
    match versions.get(file) {
        None => true,
        Some(None) => false,
        Some(Some(want)) => version == Some(want.as_str()),
    }
}

/// `slotId` (`vector-slots.cjs:34`): sha256 of `profile \0 model`, first 16 hex digits.
pub fn slot_id(target: Target) -> String {
    let digest = Sha256::digest(format!("{}\0{}", target.profile, target.model).as_bytes());
    let mut hex = String::with_capacity(16);
    for b in digest.iter().take(8) {
        hex.push_str(&format!("{b:02x}"));
    }
    hex
}

fn hex16(name: &str, prefix: &str) -> bool {
    name.strip_prefix(prefix).is_some_and(|s| {
        s.len() == 16
            && s.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

/// `safe(rec)` (`vector-slots.cjs:40-45`).
fn safe(value: &Value) -> Option<Slot> {
    let table = value.get("table")?.as_str()?;
    if !(table == LEGACY_TABLE || hex16(table, "vec_")) {
        return None;
    }
    let map = match value.get("map") {
        None | Some(Value::Null) => None,
        Some(Value::String(m)) if hex16(m, "vecmap_") => Some(m.clone()),
        Some(_) => return None,
    };
    // `Number.isInteger(dim) && dim > 0 && dim <= 65536`: 8.0 is an integer in JS too.
    let dim = value.get("dim")?.as_f64()?;
    if !(dim.fract() == 0.0 && dim > 0.0 && dim <= 65536.0) {
        return None;
    }
    Some(Slot {
        table: table.to_owned(),
        map,
        // In range 1..=65536 and integral, checked above.
        dim: dim as usize,
    })
}

fn meta(conn: &Connection, key: &str) -> Result<Option<String>, Fault> {
    conn.query_row("SELECT value FROM vec_meta WHERE key = ?", [key], |r| {
        r.get::<_, Option<String>>(0)
    })
    .optional()
    .map(Option::flatten)
    .map_err(|_| Fault::Query)
}

fn table_exists(conn: &Connection, name: &str) -> Result<bool, Fault> {
    conn.query_row("SELECT 1 FROM sqlite_master WHERE name = ?", [name], |_| {
        Ok(())
    })
    .optional()
    .map(|r| r.is_some())
    .map_err(|_| Fault::Query)
}

/// The slot records in this file (`listSlots`), for the legacy-claimed check.
fn legacy_claimed(conn: &Connection) -> Result<bool, Fault> {
    let mut stmt = conn
        .prepare("SELECT value FROM vec_meta WHERE key LIKE 'slot:%'")
        .map_err(|_| Fault::Query)?;
    let rows = stmt
        .query_map([], |r| r.get::<_, Option<String>>(0))
        .map_err(|_| Fault::Query)?;
    for raw in rows {
        let raw = raw.map_err(|_| Fault::Query)?.unwrap_or_default();
        let slot = serde_json::from_str::<Value>(&raw)
            .ok()
            .and_then(|v| safe(&v));
        if slot.is_some_and(|s| s.table == LEGACY_TABLE) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// `Number(meta(db, 'dim'))` as `adoptLegacy` reads it, when it is a usable width.
fn legacy_dim(raw: Option<&str>) -> Option<usize> {
    // `Number(null)` is 0 and `Number('')` is 0, both refused by `dim > 0`. Node then stores the
    // value as its JSON number, which `safe` must accept as an integer in 1..=65536.
    let n: f64 = raw?.trim().parse().ok()?;
    (n.fract() == 0.0 && n > 0.0 && n <= 65536.0).then_some(n as usize)
}

/// `adoptLegacy` then `findSlot` (`vector-slots.cjs:59-75`), without the write: the slot of
/// `target`, or `None` when it has none.
pub fn find_slot(conn: &Connection, target: Target) -> Result<Option<Slot>, Fault> {
    let id = slot_id(target);
    if let Some(raw) = meta(conn, &format!("slot:{id}"))? {
        return Ok(serde_json::from_str::<Value>(&raw)
            .ok()
            .and_then(|v| safe(&v)));
    }
    if target.profile != "nomic-v1" {
        return Ok(None);
    }
    let Some(dim) = legacy_dim(meta(conn, "dim")?.as_deref()) else {
        return Ok(None);
    };
    if !table_exists(conn, LEGACY_TABLE)? || legacy_claimed(conn)? {
        return Ok(None);
    }
    Ok(Some(Slot {
        table: LEGACY_TABLE.to_owned(),
        map: None,
        dim,
    }))
}

/// `joinSql(slot)` (`vector-slots.cjs:147-151`). Table names are checked by [`safe`].
fn join_sql(slot: &Slot) -> String {
    match &slot.map {
        Some(map) => format!(
            "{} v JOIN chunks c ON c.id = v.chunk_id JOIN {map} m ON m.chunk_id = c.id AND m.content_hash = c.content_hash",
            slot.table
        ),
        None => format!("{} v JOIN chunks c ON c.id = v.chunk_id", slot.table),
    }
}

fn columns(conn: &Connection) -> Result<Vec<String>, Fault> {
    let mut stmt = conn
        .prepare("PRAGMA table_info(chunks)")
        .map_err(|_| Fault::Query)?;
    let rows = stmt
        .query_map([], |r| r.get::<_, String>(1))
        .map_err(|_| Fault::Query)?;
    rows.collect::<Result<_, _>>().map_err(|_| Fault::Query)
}

/// Little-endian `float32` bytes, as `serializeF32` (`Buffer.from(new Float32Array(vec).buffer)`).
fn f32_blob(vector: &[f32]) -> Vec<u8> {
    vector.iter().flat_map(|x| x.to_le_bytes()).collect()
}

/// `Math.round(score * 1000) / 1000`. Scores here are at or above [`MIN_SCORE`], so rounding half
/// away from zero is rounding half up, as `Math.round` does.
fn round3(score: f64) -> f64 {
    (score * 1000.0).round() / 1000.0
}

/// `textHitsConfigured` without the reranker (`rag.cjs:909-922`): at most `pool` text hits.
pub fn text_hits(
    conn: &Connection,
    slot: &Slot,
    query: &[f32],
    pool: usize,
    versions: &Versions,
) -> Result<Vec<Hit>, Fault> {
    let cols = columns(conn)?;
    let has = |c: &str| cols.iter().any(|n| n == c);
    // Node adds these columns (NULL) before it reads; a missing one reads as NULL here.
    let version = if has("version") { "c.version" } else { "NULL" };
    let kind = if has("kind") { "c.kind" } else { "NULL" };
    let sql = format!(
        "SELECT c.file, c.body, {version}, 1.0 - vec_distance_cosine(v.embedding, ?1) AS score
         FROM {}
         WHERE 1.0 - vec_distance_cosine(v.embedding, ?1) >= ?2 AND COALESCE({kind}, 'text') = 'text'
         ORDER BY score DESC, c.id ASC
         LIMIT ?3",
        join_sql(slot)
    );
    let limit = i64::try_from(pool.saturating_mul(4)).unwrap_or(i64::MAX);
    let mut stmt = conn.prepare(&sql).map_err(|_| Fault::Query)?;
    let rows = stmt
        .query_map(rusqlite::params![f32_blob(query), MIN_SCORE, limit], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, f64>(3)?,
            ))
        })
        .map_err(|_| Fault::Query)?;
    let mut hits = Vec::new();
    for row in rows {
        let (file, body, version, score) = row.map_err(|_| Fault::Query)?;
        if hits.len() < pool && is_current(versions, &file, version.as_deref()) {
            hits.push(Hit {
                file,
                body,
                score: round3(score),
            });
        }
    }
    Ok(hits)
}

/// `searchProjectNow` after the query is embedded: no slot for the active target, or a query
/// vector of another width, gives no hits (never another model's vectors).
pub fn search(
    conn: &Connection,
    target: Target,
    query: &[f32],
    pool: usize,
    versions: &Versions,
) -> Result<Vec<Hit>, Fault> {
    let Some(slot) = find_slot(conn, target)? else {
        return Ok(Vec::new());
    };
    if query.len() != slot.dim {
        return Ok(Vec::new());
    }
    text_hits(conn, &slot, query, pool, versions)
}

/// Opens an existing index for reading; `Ok(None)` when the file does not exist (#1776: a read
/// never creates an index). sqlite-vec must already be registered for the process
/// (`sqlite-vec-loader`). The connection refuses writes (`query_only`).
pub fn open_index(path: &Path) -> Result<Option<Connection>, Fault> {
    if !path.is_file() {
        return Ok(None);
    }
    // Read-write without create: a read-only handle cannot open a WAL index whose -shm is gone.
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|_| Fault::Index)?;
    conn.busy_timeout(Duration::from_secs(5))
        .map_err(|_| Fault::Index)?;
    conn.pragma_update(None, "query_only", true)
        .map_err(|_| Fault::Index)?;
    Ok(Some(conn))
}

#[cfg(test)]
mod tests {
    use super::{is_current, legacy_dim, round3, safe, slot_id, Target, Versions};
    use serde_json::json;

    #[test]
    fn slot_id_is_node_slot_id() {
        // vector-slots.cjs slotId({profile:'nomic-v1', model:'qa-embed-legacy'}), see F2-F6 S4.
        assert_eq!(
            slot_id(Target {
                profile: "nomic-v1",
                model: "qa-embed-legacy"
            }),
            "3c52bf61bb4b407a"
        );
    }

    #[test]
    fn version_rule() {
        let mut v = Versions::new();
        assert!(is_current(&v, "a", None));
        v.insert("a".into(), None);
        assert!(!is_current(&v, "a", Some("x")));
        v.insert("b".into(), Some("x".into()));
        assert!(is_current(&v, "b", Some("x")));
        assert!(!is_current(&v, "b", Some("y")));
        assert!(!is_current(&v, "b", None));
    }

    #[test]
    fn slot_records_are_checked_like_safe() {
        assert!(safe(&json!({"table":"vec_items","map":null,"dim":8})).is_some());
        assert!(safe(
            &json!({"table":"vec_0123456789abcdef","map":"vecmap_0123456789abcdef","dim":8.0})
        )
        .is_some());
        assert!(safe(&json!({"table":"vec_items; DROP","dim":8})).is_none());
        assert!(safe(&json!({"table":"vec_0123456789ABCDEF","dim":8})).is_none());
        assert!(safe(&json!({"table":"vec_items","map":"chunks","dim":8})).is_none());
        assert!(safe(&json!({"table":"vec_items","map":3,"dim":8})).is_none());
        assert!(safe(&json!({"table":"vec_items","dim":8.5})).is_none());
        assert!(safe(&json!({"table":"vec_items","dim":0})).is_none());
        assert!(safe(&json!({"table":"vec_items","dim":65537})).is_none());
        assert!(safe(&json!({"table":"vec_items","dim":"8"})).is_none());
    }

    #[test]
    fn legacy_dim_is_number_of_the_text() {
        assert_eq!(legacy_dim(Some("768")), Some(768));
        assert_eq!(legacy_dim(Some(" 8 ")), Some(8));
        assert_eq!(legacy_dim(Some("0")), None);
        assert_eq!(legacy_dim(Some("")), None);
        assert_eq!(legacy_dim(Some("8.5")), None);
        assert_eq!(legacy_dim(None), None);
    }

    #[test]
    fn rounding_is_math_round() {
        assert_eq!(round3(0.4445), (0.4445f64 * 1000.0).round() / 1000.0);
        assert_eq!(round3(1.0), 1.0);
    }
}
