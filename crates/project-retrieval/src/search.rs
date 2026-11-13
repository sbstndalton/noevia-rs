//! The vector half of `searchProject` (`rag.cjs:878-922`) over a project index that Node or the
//! native embedding worker wrote: the active model's slot (`vector-slots.cjs`), the cosine SQL
//! with its score floor and `LIMIT pool × 4`, the version filter, `slice(0, pool)` and the
//! rounding. The query vector comes from the caller (it embeds the question with `kind: query` on
//! the active target); the reranker is not here.
//!
//! Reads only. Node's `openIndex` also writes on every search: it creates the schema when it is
//! missing, adds the four later `chunks` columns to an older index, and records an unclaimed
//! legacy `vec_items` table as the slot of a `nomic-v1` target. Here a missing column reads as
//! NULL (what Node's added column holds), a missing table is a [`Fault`], and nothing is claimed.
//!
//! Named differences from Node (recorded in `tests/fixtures/F2-F6-search.json`, asserted):
//! - **unclaimed-legacy**: an index from before #1636 (`vec_items` with no slot record) has no slot
//!   here until Node or the native worker (`embedding-worker` `sqlite.rs`, on its next open)
//!   records the claim; Node claims it for the active `nomic-v1` model on its first search. Taking
//!   the claim in a read would let a later model inherit vectors another model made.
//! - **tie-order**: exact score ties are ordered by chunk id; Node's SQL has no tie-breaker and
//!   returns them in vec0 scan order, which can differ after vectors are rewritten. A tie across
//!   the `pool × 4` or `pool` cut can therefore also change which of the tied chunks is returned.
//!
//! Copied on purpose for parity (finding sbstndalton/noevia#2518): the version filter runs after
//! the SQL `LIMIT`, so a file being re-indexed whose stale chunks fill the whole `pool × 4` window
//! leaves fewer than `pool` hits, or none, although other files have matching chunks.

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use rusqlite::config::DbConfig;
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::hits::Hit;

/// `MIN_SCORE` (`rag.cjs:104`).
pub const MIN_SCORE: f64 = 0.3;
/// `TOP_K` (`rag.cjs:99`): the pool without the reranker.
pub const TOP_K: usize = 6;
/// The legacy vector table every index built before #1636 holds (`vector-slots.cjs:22`).
const LEGACY_TABLE: &str = "vec_items";

/// The embedding identity a slot belongs to (`embedding-config.cjs` target, without its address:
/// the slot id is `profile \0 model`, `vector-slots.cjs:34`).
#[derive(Clone, Copy, Debug)]
pub struct Target<'a> {
    pub profile: &'a str,
    pub model: &'a str,
}

/// A slot record (`vec_meta 'slot:<id>'`), checked as `vector-slots.cjs` `safe` checks it. Only
/// [`find_slot`] makes one, so its table names are always checked before they reach SQL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Slot {
    table: String,
    map: Option<String>,
    dim: usize,
}

impl Slot {
    /// The vector width of this slot.
    pub fn dim(&self) -> usize {
        self.dim
    }
}

/// Why a search could not run (an index without `chunks` or `vec_meta`, a damaged slot table, a
/// missing vec0 module). Node logs these and answers `[]`; the caller answers `[]` too.
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

/// `findSlot` (`vector-slots.cjs:69-75`): the slot recorded for `target`, or `None`. An empty or
/// unreadable record is no slot, as in Node. Node runs `adoptLegacy` first; see the module notes.
pub fn find_slot(conn: &Connection, target: Target) -> Result<Option<Slot>, Fault> {
    let Some(raw) = meta(conn, &format!("slot:{}", slot_id(target)))? else {
        return Ok(None);
    };
    Ok(serde_json::from_str::<Value>(&raw)
        .ok()
        .and_then(|v| safe(&v)))
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
fn text_hits(
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
/// (`sqlite-vec-loader`). The connection refuses writes (`query_only`) and does not checkpoint
/// when it closes, so the main file is never written; SQLite may create the `-wal` and `-shm`
/// side files of a WAL index that has none, as any reader does.
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
    conn.set_db_config(DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE, true)
        .map_err(|_| Fault::Index)?;
    conn.pragma_update(None, "query_only", true)
        .map_err(|_| Fault::Index)?;
    Ok(Some(conn))
}

/// `IMAGE_TOP_K` and `IMAGE_MIN_SCORE` (`rag.cjs:971-972`), read from `RAG_IMAGE_TOP_K` and
/// `RAG_IMAGE_MIN_SCORE` as Node reads them.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ImageSettings {
    pub top_k: usize,
    pub min_score: f64,
}

/// `parseInt(String(v), 10)`: optional leading JS whitespace and sign, then decimal digits up to the
/// first other character; `None` (NaN) without a digit.
fn js_parse_int(v: Option<&str>) -> Option<f64> {
    let u = prompt_framing::js::units(v.unwrap_or("undefined"));
    let s = String::from_utf16_lossy(prompt_framing::js::trim(&u));
    let (neg, rest) = match s.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, s.strip_prefix('+').unwrap_or(&s)),
    };
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    let n: f64 = digits.parse().ok()?;
    Some(if neg { -n } else { n })
}

/// `Number(v)` of a string: JS whitespace trimmed, empty is 0, decimal (with exponent), `0x`/`0o`/`0b`
/// integers and `Infinity`; anything else is NaN. `undefined` is NaN.
fn js_number(v: Option<&str>) -> f64 {
    let Some(v) = v else { return f64::NAN };
    let u = prompt_framing::js::units(v);
    let s = String::from_utf16_lossy(prompt_framing::js::trim(&u));
    if s.is_empty() {
        return 0.0;
    }
    for (prefix, radix) in [
        ("0x", 16),
        ("0X", 16),
        ("0o", 8),
        ("0O", 8),
        ("0b", 2),
        ("0B", 2),
    ] {
        if let Some(d) = s.strip_prefix(prefix) {
            return if !d.is_empty() && d.chars().all(|c| c.is_digit(radix)) {
                d.chars().fold(0.0, |n, c| {
                    n * f64::from(radix) + f64::from(c.to_digit(radix).unwrap_or(0))
                })
            } else {
                f64::NAN
            };
        }
    }
    let body = s.strip_prefix(['+', '-']).unwrap_or(&s);
    if body == "Infinity" {
        return if s.starts_with('-') {
            f64::NEG_INFINITY
        } else {
            f64::INFINITY
        };
    }
    // Decimal literal only: Rust's parser also takes "inf", "nan" and other spellings JS does not.
    let decimal = {
        let (mantissa, exponent) = match body.find(['e', 'E']) {
            Some(i) => (&body[..i], Some(&body[i + 1..])),
            None => (body, None),
        };
        let digits = mantissa.chars().filter(char::is_ascii_digit).count();
        digits > 0
            && mantissa.chars().filter(|c| *c == '.').count() <= 1
            && mantissa.chars().all(|c| c.is_ascii_digit() || c == '.')
            && exponent.is_none_or(|e| {
                let e = e.strip_prefix(['+', '-']).unwrap_or(e);
                !e.is_empty() && e.chars().all(|c| c.is_ascii_digit())
            })
    };
    if decimal {
        s.parse().unwrap_or(f64::NAN)
    } else {
        f64::NAN
    }
}

impl ImageSettings {
    /// `clampInt(RAG_IMAGE_TOP_K, 2, 0, 8)` and `RAG_IMAGE_MIN_SCORE` when it is a number in [-1, 1],
    /// else [`MIN_SCORE`].
    pub fn from_env(top_k: Option<&str>, min_score: Option<&str>) -> ImageSettings {
        let top_k = js_parse_int(top_k).map_or(2.0, |n| n.clamp(0.0, 8.0));
        let n = js_number(min_score);
        ImageSettings {
            // In 0..=8, an integer.
            top_k: top_k as usize,
            min_score: if n.is_finite() && (-1.0..=1.0).contains(&n) {
                n
            } else {
                MIN_SCORE
            },
        }
    }
}

/// An image row as a search result carries it (`mediaRow`): `{file, kind: 'image', meta}`.
#[derive(Clone, Debug, PartialEq)]
pub struct ImageRow {
    pub file: String,
    pub meta: Value,
}

/// An image hit: the row, its label (`body`), its `img:` hash and its rounded cosine score.
#[derive(Clone, Debug, PartialEq)]
pub struct ImageHit {
    pub row: ImageRow,
    pub label: String,
    pub hash: String,
    pub score: f64,
}

/// `searchProjectWithImages`' answer.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct WithImages {
    pub text: Vec<Hit>,
    pub images: Vec<ImageHit>,
    /// Every image row with a current vector, readable meta only.
    pub indexed: Vec<ImageRow>,
}

/// `mediaRow(row)`: `JSON.parse(row.meta || 'null')` must be an object (an array is one in JS).
/// Named difference: a meta whose JSON holds a lone surrogate escape (`"\ud800"`), which
/// `JSON.parse` takes, is unreadable here.
fn media_row(file: String, meta: Option<String>) -> Option<ImageRow> {
    let meta: Value =
        serde_json::from_str(meta.as_deref().filter(|m| !m.is_empty()).unwrap_or("null")).ok()?;
    matches!(meta, Value::Object(_) | Value::Array(_)).then_some(ImageRow { file, meta })
}

/// The image half of `searchProjectWithImagesNow` (`rag.cjs:957-967`): `indexed` (every image row
/// joined to a current vector, by chunk id: the tie-order rule of this module) and `images` (cosine
/// at or above `min_score`, best first, at most `top_k`; no version filter, no reranker).
pub fn image_hits(
    conn: &Connection,
    slot: &Slot,
    query: &[f32],
    settings: ImageSettings,
) -> Result<(Vec<ImageHit>, Vec<ImageRow>), Fault> {
    let cols = columns(conn)?;
    if !(cols.iter().any(|c| c == "kind") && cols.iter().any(|c| c == "meta")) {
        // An index from before #1726 holds no image rows.
        return Ok((Vec::new(), Vec::new()));
    }
    let join = join_sql(slot);
    let mut stmt = conn
        .prepare(&format!(
            "SELECT c.file, c.meta FROM {join} WHERE c.kind = 'image' ORDER BY c.id"
        ))
        .map_err(|_| Fault::Query)?;
    let indexed = stmt
        .query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
        })
        .map_err(|_| Fault::Query)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| Fault::Query)?
        .into_iter()
        .filter_map(|(file, meta)| media_row(file, meta))
        .collect();
    let mut stmt = conn
        .prepare(&format!(
            "SELECT c.file, c.body, c.content_hash, c.meta, 1.0 - vec_distance_cosine(v.embedding, ?1) AS score
             FROM {join}
             WHERE c.kind = 'image' AND 1.0 - vec_distance_cosine(v.embedding, ?1) >= ?2
             ORDER BY score DESC, c.id ASC
             LIMIT ?3"
        ))
        .map_err(|_| Fault::Query)?;
    let limit = i64::try_from(settings.top_k).unwrap_or(0);
    let images = stmt
        .query_map(
            rusqlite::params![f32_blob(query), settings.min_score, limit],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, f64>(4)?,
                ))
            },
        )
        .map_err(|_| Fault::Query)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| Fault::Query)?
        .into_iter()
        .filter_map(|(file, label, hash, meta, score)| {
            Some(ImageHit {
                row: media_row(file, meta)?,
                label,
                hash,
                score: round3(score),
            })
        })
        .collect();
    Ok((images, indexed))
}

/// `searchProjectWithImagesNow` after the query is embedded: the text hits exactly as [`search`]
/// gives them, and the image half beside them. No slot, or a query of another width, is empty.
pub fn search_with_images(
    conn: &Connection,
    target: Target,
    query: &[f32],
    pool: usize,
    versions: &Versions,
    settings: ImageSettings,
) -> Result<WithImages, Fault> {
    let Some(slot) = find_slot(conn, target)? else {
        return Ok(WithImages::default());
    };
    if query.len() != slot.dim {
        return Ok(WithImages::default());
    }
    let text = text_hits(conn, &slot, query, pool, versions)?;
    let (images, indexed) = image_hits(conn, &slot, query, settings)?;
    Ok(WithImages {
        text,
        images,
        indexed,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        is_current, js_number, js_parse_int, round3, safe, slot_id, ImageSettings, Target, Versions,
    };
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
    fn node_number_and_parse_int() {
        assert_eq!(js_parse_int(Some(" 2.9")), Some(2.0));
        assert_eq!(js_parse_int(Some("-3x")), Some(-3.0));
        assert_eq!(js_parse_int(Some("many")), None);
        assert_eq!(js_parse_int(None), None);
        assert_eq!(js_number(Some(" 0.25 ")), 0.25);
        assert_eq!(js_number(Some("")), 0.0);
        assert_eq!(js_number(Some("0x10")), 16.0);
        assert_eq!(js_number(Some("-1e0")), -1.0);
        assert_eq!(js_number(Some(".5")), 0.5);
        assert_eq!(js_number(Some("5.")), 5.0);
        assert!(js_number(Some("inf")).is_nan());
        assert!(js_number(Some("NaN")).is_nan());
        assert!(js_number(Some("1e")).is_nan());
        assert!(js_number(Some("0x")).is_nan());
        assert!(js_number(Some("-0x10")).is_nan());
        assert_eq!(js_number(Some("-Infinity")), f64::NEG_INFINITY);
        assert!(js_number(None).is_nan());
        // RAG_IMAGE_MIN_SCORE set but empty is Number('') = 0, a valid floor.
        assert_eq!(ImageSettings::from_env(None, Some("")).min_score, 0.0);
        assert_eq!(ImageSettings::from_env(Some("-4"), None).top_k, 0);
    }

    #[test]
    fn rounding_is_math_round() {
        assert_eq!(round3(0.4445), (0.4445f64 * 1000.0).round() / 1000.0);
        assert_eq!(round3(1.0), 1.0);
    }
}
