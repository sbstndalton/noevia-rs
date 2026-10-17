//! noevia#1318: native `PUT` and `DELETE /api/v1/badges` (a model's 1-5 rating per category).
//!
//! The Python routes this replaces (services `api.set_badge`, `api.clear_badge`, `db.badge_set`,
//! `db.badge_clear`, `db.badges_for`):
//!
//! ```python
//! @router.put("/badges")
//! def set_badge(body: dict = Body(...)) -> dict:
//!     alias = str(body.get("alias") or "")
//!     ok, err = db.badge_set(alias, str(body.get("category") or ""), int(body.get("rating") or 0),
//!                            str(body.get("note") or ""), int(body.get("runId") or 0) or None)
//!     if not ok:
//!         raise HTTPException(400, err)
//!     return {"badges": [_row(b) for b in db.badges_for(alias)]}
//!
//! @router.delete("/badges")
//! def clear_badge(alias: str = Query(...), category: str = Query(...)) -> dict:
//!     db.badge_clear(alias, category)
//!     return {"badges": [_row(b) for b in db.badges_for(alias)]}
//! ```
//!
//! Fixed on the way (noevia#1905, also fixed in the Python routes): a `rating` or `runId` that
//! is not an integer answers 400 instead of 500 (and a fractional or boolean rating is no longer
//! truncated or read as 1), and the alias is stripped once and used for the write, the delete and
//! the answer, so a padded alias is answered and can be cleared.
//!
//! Store: `DATA_DIR/model_loader.db`, opened read-write but never created, never converted to
//! another journal mode, with a busy wait and a progress deadline inside the native budget. Any
//! store failure answers FastAPI's `500 text/plain Internal Server Error`, as Python's would.
use crate::prompts::{py_repr_str, py_str, JsonValue};
use rusqlite::{types::ValueRef, Connection, OpenFlags};
use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

/// The closed category vocabulary (`db.BADGE_CATEGORIES`).
pub const CATEGORIES: [&str; 5] = ["coding", "writing", "reasoning", "tools", "vision"];
/// Python keeps the first 500 code points of the stripped note.
pub const NOTE_CHARS: usize = 500;
/// Largest number of badge rows answered (one alias has at most one per category in practice).
pub const ROW_CAP: usize = 4096;
/// Largest stored text or blob value read back.
pub const VALUE_CAP: i32 = 1024 * 1024;
const BUSY_WAIT: Duration = Duration::from_millis(4000);
const DEADLINE: Duration = Duration::from_millis(4500);

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Status and exact JSON body.
    Json(u16, Vec<u8>),
    /// Python would raise (store missing, locked or corrupt), or a native bound was passed.
    Failed,
}

pub type Clock = std::sync::Arc<dyn Fn() -> f64 + Send + Sync>;

fn now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

pub struct Badges {
    database: PathBuf,
    clock: Clock,
    /// Python serialises its own writes with a process lock; so does this side.
    /// Owned guards travel into the blocking job (noevia#1909), so a write whose request was
    /// dropped (the front's 504) still finishes before the next one starts.
    lock: std::sync::Arc<tokio::sync::Mutex<()>>,
}

/// A validated `PUT` body.
#[derive(Debug, PartialEq)]
pub struct Rating {
    pub alias: String,
    pub category: String,
    pub rating: i64,
    pub note: String,
    pub run_id: Option<i64>,
}

impl Badges {
    pub fn new(root: &Path) -> Self {
        Self::with_clock(root, std::sync::Arc::new(now))
    }

    /// Tests pin `created_at`.
    pub fn with_clock(root: &Path, clock: Clock) -> Self {
        Self {
            database: root.join("model_loader.db"),
            clock,
            lock: std::sync::Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    pub async fn put(&self, body: &JsonValue) -> Outcome {
        let deadline = Instant::now() + DEADLINE;
        let rating = match validate(body) {
            Ok(rating) => rating,
            Err(message) => return detail(400, &message),
        };
        let Some(guard) = self.acquire(deadline).await else {
            return busy();
        };
        let database = self.database.clone();
        let created = (self.clock)();
        run(move || {
            let _guard = guard;
            let db = open(&database, deadline)?;
            db.execute(
                "INSERT INTO model_badge(alias, category, rating, note, run_id, created_at) \
                 VALUES(?, ?, ?, ?, ?, ?) \
                 ON CONFLICT(alias, category) DO UPDATE SET \
                   rating = excluded.rating, note = excluded.note, \
                   run_id = excluded.run_id, created_at = excluded.created_at",
                rusqlite::params![
                    rating.alias,
                    rating.category,
                    rating.rating,
                    rating.note,
                    rating.run_id,
                    created
                ],
            )
            .map_err(|_| ())?;
            badges_for(&db, &rating.alias)
        })
        .await
    }

    /// `query` is the raw query string.
    pub async fn delete(&self, query: &str) -> Outcome {
        let deadline = Instant::now() + DEADLINE;
        let (alias, category) = match delete_query(query) {
            Ok(fields) => fields,
            Err(body) => return Outcome::Json(422, body),
        };
        let alias = py_strip(&alias).to_owned();
        let Some(guard) = self.acquire(deadline).await else {
            return busy();
        };
        let database = self.database.clone();
        run(move || {
            let _guard = guard;
            let db = open(&database, deadline)?;
            db.execute(
                "DELETE FROM model_badge WHERE alias = ? AND category = ?",
                [&alias, &category],
            )
            .map_err(|_| ())?;
            badges_for(&db, &alias)
        })
        .await
    }
}

impl Badges {
    /// The write lock, waited for only until the request's own deadline (noevia#1909).
    async fn acquire(&self, deadline: Instant) -> Option<tokio::sync::OwnedMutexGuard<()>> {
        tokio::time::timeout_at(deadline.into(), self.lock.clone().lock_owned())
            .await
            .ok()
    }
}

/// The prompt writer's answer when its write lock is not free in time.
fn busy() -> Outcome {
    detail(503, "service is busy")
}

/// noevia#2006: admitted: a dropped request keeps its permit (as the job keeps the write lock)
/// until the write returns.
async fn run(job: impl FnOnce() -> Result<Vec<u8>, ()> + Send + 'static) -> Outcome {
    match crate::docker::spawn_blocking_admitted(job).await {
        Ok(Ok(bytes)) => Outcome::Json(200, bytes),
        _ => Outcome::Failed,
    }
}

fn py_strip(text: &str) -> &str {
    model_autoconfig::pyval::py_strip(text)
}

fn detail(status: u16, message: &str) -> Outcome {
    let body = serde_json::to_vec(&serde_json::json!({ "detail": message }));
    body.map_or(Outcome::Failed, |body| Outcome::Json(status, body))
}

/// Why a body value is not a usable integer.
#[derive(Debug, PartialEq, Eq)]
pub enum NotInt {
    /// An integer Python accepts, outside i64 (a rating then fails the 1-5 check).
    Big,
    /// Not an integer (noevia#1905: Python answered 500, or truncated a fraction or a bool).
    Other,
}

/// The route's `int(body.get(key) or 0)`, fixed (noevia#1905): a falsy value is 0; an integer,
/// an integral finite float or an integer string (`int()` syntax, ASCII digits) is its value;
/// `true`, a fraction, a list, an object or other text is [`NotInt::Other`].
pub fn route_int(value: Option<&JsonValue>) -> Result<i64, NotInt> {
    let Some(value) = value.filter(|v| v.truthy()) else {
        return Ok(0);
    };
    match value {
        JsonValue::Int(n) => n.to_string().parse::<i64>().map_err(|_| NotInt::Big),
        JsonValue::Float(f) if f.is_finite() && f.fract() == 0.0 => {
            // i64's range exactly, as Python's `-2**63 <= int(value) < 2**63` (noevia#1910).
            if (-9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0).contains(f) {
                // Exact: an integral float inside i64 converts without rounding.
                #[allow(clippy::cast_possible_truncation)]
                Ok(*f as i64)
            } else {
                Err(NotInt::Big)
            }
        }
        JsonValue::Str(s) => int_text(py_strip(s)),
        _ => Err(NotInt::Other),
    }
}

/// CPython `int(text)` for base 10 with ASCII digits: optional sign, digits, single underscores
/// between digits.
fn int_text(text: &str) -> Result<i64, NotInt> {
    let (negative, digits) = match text.as_bytes().first() {
        Some(b'-') => (true, text.get(1..).ok_or(NotInt::Other)?),
        Some(b'+') => (false, text.get(1..).ok_or(NotInt::Other)?),
        _ => (false, text),
    };
    if digits.is_empty()
        || digits.starts_with('_')
        || digits.ends_with('_')
        || digits.contains("__")
        || !digits.bytes().all(|b| b.is_ascii_digit() || b == b'_')
    {
        return Err(NotInt::Other);
    }
    // Accumulated with the sign applied, so i64::MIN itself fits (noevia#1910).
    let mut value: i64 = 0;
    for b in digits.bytes().filter(u8::is_ascii_digit) {
        let digit = i64::from(b - b'0');
        value = value
            .checked_mul(10)
            .and_then(|v| {
                if negative {
                    v.checked_sub(digit)
                } else {
                    v.checked_add(digit)
                }
            })
            .ok_or(NotInt::Big)?;
    }
    Ok(value)
}

/// The whole `PUT` validation, in Python's order (the route's conversions, then `badge_set`).
pub fn validate(body: &JsonValue) -> Result<Rating, String> {
    let text = |key: &str| py_str(body.get(key)).map_err(|_| "invalid badge input".to_owned());
    // A huge integer rating is a number, just not 1-5 (checked below, after alias and category).
    let rating = match route_int(body.get("rating")) {
        Ok(n) => Some(n),
        Err(NotInt::Big) => None,
        Err(NotInt::Other) => return Err("rating must be a number".into()),
    };
    let run_id = route_int(body.get("runId")).map_err(|_| "runId must be a number".to_owned())?;
    let alias = py_strip(&text("alias")?).to_owned();
    if alias.is_empty() {
        return Err("no model given".into());
    }
    let category = text("category")?;
    if !CATEGORIES.contains(&category.as_str()) {
        let shown = py_repr_str(&category).map_err(|_| "invalid badge input".to_owned())?;
        return Err(format!("unknown category {shown}"));
    }
    let Some(rating) = rating.filter(|r| (1..=5).contains(r)) else {
        return Err("rating must be 1-5".into());
    };
    let note: String = py_strip(&text("note")?).chars().take(NOTE_CHARS).collect();
    Ok(Rating {
        alias,
        category,
        rating,
        note,
        run_id: (run_id != 0).then_some(run_id),
    })
}

/// FastAPI's required `alias` and `category` query parameters (the last value of each), or its
/// exact 422 body listing the missing ones.
pub fn delete_query(query: &str) -> Result<(String, String), Vec<u8>> {
    let last = |key: &str| {
        form_urlencoded::parse(query.as_bytes())
            .filter(|(name, _)| name == key)
            .last()
            .map(|(_, value)| value.into_owned())
    };
    let (alias, category) = (last("alias"), last("category"));
    let mut missing = Vec::new();
    for (key, value) in [("alias", &alias), ("category", &category)] {
        if value.is_none() {
            // FastAPI's key order, written by hand (serde_json maps sort their keys).
            missing.push(format!(
                "{{\"type\":\"missing\",\"loc\":[\"query\",\"{key}\"],\"msg\":\"Field required\",\"input\":null}}"
            ));
        }
    }
    match (alias, category) {
        (Some(alias), Some(category)) => Ok((alias, category)),
        _ => Err(format!("{{\"detail\":[{}]}}", missing.join(",")).into_bytes()),
    }
}

fn open(database: &Path, deadline: Instant) -> Result<Connection, ()> {
    let db = Connection::open_with_flags(
        database,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|_| ())?;
    db.set_limit(rusqlite::limits::Limit::SQLITE_LIMIT_LENGTH, VALUE_CAP)
        .map_err(|_| ())?;
    db.busy_timeout(BUSY_WAIT.min(deadline.saturating_duration_since(Instant::now())))
        .map_err(|_| ())?;
    db.progress_handler(1000, Some(move || Instant::now() >= deadline))
        .map_err(|_| ())?;
    Ok(db)
}

/// `{"badges": [dict(row) ...]}` for `SELECT * ... WHERE alias = ? ORDER BY category`, rendered
/// as FastAPI renders the sqlite3 rows (columns in table order, floats as Python's repr).
fn badges_for(db: &Connection, alias: &str) -> Result<Vec<u8>, ()> {
    let mut statement = db
        .prepare("SELECT * FROM model_badge WHERE alias = ? ORDER BY category")
        .map_err(|_| ())?;
    let names: Vec<String> = statement
        .column_names()
        .into_iter()
        .map(str::to_owned)
        .collect();
    let mut rows = statement.query([alias]).map_err(|_| ())?;
    let mut out = String::from("{\"badges\":[");
    let mut count = 0;
    while let Some(row) = rows.next().map_err(|_| ())? {
        count += 1;
        if count > ROW_CAP {
            return Err(());
        }
        if count > 1 {
            out.push(',');
        }
        out.push('{');
        // dict(row): a repeated column name keeps its last value at its first position.
        let mut seen: Vec<&str> = Vec::new();
        for name in &names {
            if seen.contains(&name.as_str()) {
                continue;
            }
            seen.push(name);
            let index = names.iter().rposition(|n| n == name).ok_or(())?;
            if seen.len() > 1 {
                out.push(',');
            }
            out.push_str(&json_string(name));
            out.push(':');
            out.push_str(&value(row.get_ref(index).map_err(|_| ())?)?);
        }
        out.push('}');
    }
    out.push_str("]}");
    Ok(out.into_bytes())
}

fn json_string(text: &str) -> String {
    serde_json::to_string(text).unwrap_or_default()
}

fn value(value: ValueRef<'_>) -> Result<String, ()> {
    Ok(match value {
        ValueRef::Null => "null".to_owned(),
        ValueRef::Integer(n) => n.to_string(),
        // Python would write `Infinity`; a stored infinity is refused here instead.
        ValueRef::Real(f) if f.is_finite() => gguf::py_float_repr(f),
        ValueRef::Real(_) => return Err(()),
        // sqlite3 decodes TEXT as UTF-8 (raising otherwise); FastAPI decodes bytes as UTF-8.
        ValueRef::Text(bytes) | ValueRef::Blob(bytes) => {
            json_string(std::str::from_utf8(bytes).map_err(|_| ())?)
        }
    })
}

#[cfg(test)]
mod tests;
