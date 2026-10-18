//! The telemetry calls of `db.py`, with the same SQL: `get_setting`/`set_setting` on `kv`,
//! `record_timings`, `record_server_configs`, `server_instances_on_port`, `recent_timings`,
//! `timings_by_instance` and `server_configs_for`. Each call is its own transaction, as each
//! Python call is its own `with _conn()` block. The connection is Python's: the store is never
//! created here, `journal_mode=WAL` is set as `_conn()` sets it, and a busy store is waited on
//! for sqlite3's default five seconds.
use super::Stop;
use backend_telemetry as bt;
use rusqlite::{params, types::ValueRef, Connection, OpenFlags};
use std::path::Path;

/// sqlite3's default `timeout`.
const BUSY_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

/// A read that Python answers differently from a value: it raised (`Raised`), or a stored value
/// is of a type this side does not follow (`Refused`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Read {
    Raised,
    Refused,
}

pub(super) struct Store {
    db: Connection,
}

fn raised<E>(_: E) -> Stop {
    Stop::Raised
}

/// `float(text or 0)`, with `except ValueError: 0.0`; `Err(Refused)` for a form Python reads
/// that this side does not (non-ASCII digits, underscores).
pub(super) fn py_float(text: &str) -> Result<f64, Stop> {
    let t = text.trim_matches(|c: char| c.is_whitespace() || ('\x1c'..='\x1f').contains(&c));
    if text.is_empty() {
        return Ok(0.0);
    }
    if !t.is_ascii() || t.contains('_') {
        return Err(Stop::Refused);
    }
    // Rust's grammar is Python's for ASCII without underscores (inf, infinity and nan in any
    // case, with a sign; digits with an optional fraction and exponent).
    Ok(t.parse::<f64>().unwrap_or(0.0))
}

impl Store {
    pub(super) fn open(path: &Path) -> Result<Store, Stop> {
        let db = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(raised)?;
        db.set_limit(
            rusqlite::limits::Limit::SQLITE_LIMIT_SQL_LENGTH,
            crate::sqlite_read::SQL_CAP,
        )
        .map_err(raised)?;
        db.busy_timeout(BUSY_WAIT).map_err(raised)?;
        let _: String = db
            .query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))
            .map_err(raised)?;
        Ok(Store { db })
    }

    /// `float(db.get_setting(key, "0") or 0)` with `except ValueError: 0.0`; a store error is
    /// Python's (unhandled) raise.
    pub(super) fn float_setting(&self, key: &str) -> Result<f64, Stop> {
        let value: Option<rusqlite::types::Value> =
            match self
                .db
                .query_row("SELECT value FROM kv WHERE key = ?", [key], |r| r.get(0))
            {
                Ok(v) => Some(v),
                Err(rusqlite::Error::QueryReturnedNoRows) => None,
                Err(_) => return Err(Stop::Raised),
            };
        match value {
            None => Ok(0.0),
            Some(rusqlite::types::Value::Text(t)) => py_float(&t),
            // `float(int)` and `float(float)`; a blob raises TypeError.
            Some(rusqlite::types::Value::Integer(i)) => Ok(if i == 0 { 0.0 } else { i as f64 }),
            Some(rusqlite::types::Value::Real(r)) => Ok(if r == 0.0 { 0.0 } else { r }),
            Some(_) => Err(Stop::Refused),
        }
    }

    pub(super) fn set_setting(&self, key: &str, value: &str) -> rusqlite::Result<()> {
        self.db.execute(
            "INSERT INTO kv(key, value) VALUES(?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    /// `db.record_timings(backend, samples)`.
    pub(super) fn record_timings(
        &self,
        backend: &str,
        samples: &[&bt::Sample],
    ) -> rusqlite::Result<()> {
        if samples.is_empty() {
            return Ok(());
        }
        let tx = self.db.unchecked_transaction()?;
        {
            let mut insert = tx.prepare(
                "INSERT OR IGNORE INTO req_timing(\
                 ts, backend, instance, task, model_path, alias, spec_type, \
                 prompt_tokens, prompt_tps, gen_tokens, gen_tps, draft_acc, draft_len) \
                 SELECT ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ? \
                 WHERE (? IS NULL OR NOT EXISTS (\
                   SELECT 1 FROM req_timing WHERE backend = ? AND instance = ? \
                   AND task = ? AND ts = ?)) \
                 AND (? IS NULL OR NOT EXISTS (\
                   SELECT 1 FROM req_timing WHERE backend = ? AND task = ? AND ts = ? \
                   AND substr(instance, 1, length(?)) = ?))",
            )?;
            for s in samples {
                let (legacy, prefix) = match s.instance.split_once('@') {
                    Some((port, _)) => (Some(port.to_owned()), None),
                    None => (None, Some(format!("{}@", s.instance))),
                };
                insert.execute(params![
                    s.ts,
                    backend,
                    s.instance,
                    s.task,
                    s.model_path,
                    s.alias,
                    s.spec_type,
                    s.prompt_tokens,
                    s.prompt_tps,
                    s.gen_tokens,
                    s.gen_tps,
                    s.draft_acc,
                    s.draft_len,
                    legacy,
                    backend,
                    legacy,
                    s.task,
                    s.ts,
                    prefix,
                    backend,
                    s.task,
                    s.ts,
                    prefix,
                    prefix,
                ])?;
            }
        }
        tx.commit()
    }

    /// `db.record_server_configs(backend, configs)`.
    pub(super) fn record_server_configs(
        &self,
        backend: &str,
        configs: &[bt::ServerConfig],
    ) -> rusqlite::Result<()> {
        if configs.is_empty() {
            return Ok(());
        }
        let tx = self.db.unchecked_transaction()?;
        {
            let mut upsert = tx.prepare(
                "INSERT INTO server_config(backend, instance, alias, model_path, argv_json, first_seen) \
                 VALUES(?, ?, ?, ?, ?, ?) \
                 ON CONFLICT(backend, instance) DO UPDATE SET \
                   alias      = CASE WHEN excluded.alias      != '' THEN excluded.alias      ELSE server_config.alias END, \
                   model_path = CASE WHEN excluded.model_path != '' THEN excluded.model_path ELSE server_config.model_path END, \
                   argv_json  = CASE WHEN excluded.argv_json  != '{}' THEN excluded.argv_json ELSE server_config.argv_json END, \
                   first_seen = CASE WHEN server_config.first_seen = 0 THEN excluded.first_seen ELSE server_config.first_seen END",
            )?;
            for c in configs {
                upsert.execute(params![
                    backend,
                    c.instance,
                    c.alias,
                    c.model_path,
                    dumps_sorted(&c.argv),
                    c.first_seen
                ])?;
            }
        }
        tx.commit()
    }

    /// `db.server_instances_on_port(backend, port)`.
    pub(super) fn server_instances_on_port(
        &self,
        backend: &str,
        port: &str,
    ) -> Result<Vec<bt::Stored>, Read> {
        let prefix = format!("{port}@");
        let mut q = self
            .db
            .prepare(
                "SELECT instance, alias, model_path, argv_json, first_seen FROM server_config \
                 WHERE backend = ? AND substr(instance, 1, length(?)) = ? AND first_seen > 0 \
                 ORDER BY first_seen DESC",
            )
            .map_err(|_| Read::Raised)?;
        let mut rows = q
            .query(params![backend, prefix, prefix])
            .map_err(|_| Read::Raised)?;
        let mut out = Vec::new();
        while let Some(r) = rows.next().map_err(|_| Read::Raised)? {
            out.push(bt::Stored {
                instance: text(r.get_ref(0))?,
                alias: text(r.get_ref(1))?,
                model_path: text(r.get_ref(2))?,
                argv: argv(r.get_ref(3))?,
                first_seen: real(r.get_ref(4))?,
            });
        }
        Ok(out)
    }

    fn timing_rows(&self, sql: &str, args: &[&dyn rusqlite::ToSql]) -> Result<Vec<bt::Row>, Read> {
        let mut q = self.db.prepare(sql).map_err(|_| Read::Raised)?;
        let names: Vec<String> = q.column_names().iter().map(|s| (*s).to_owned()).collect();
        let col = |n: &str| names.iter().position(|c| c == n).ok_or(Read::Raised);
        let (inst, ts, gen, prompt, acc, dlen, spec) = (
            col("instance")?,
            col("ts")?,
            col("gen_tps")?,
            col("prompt_tps")?,
            col("draft_acc")?,
            col("draft_len")?,
            col("spec_type")?,
        );
        let mut rows = q.query(args).map_err(|_| Read::Raised)?;
        let mut out = Vec::new();
        while let Some(r) = rows.next().map_err(|_| Read::Raised)? {
            out.push(bt::Row {
                instance: text(r.get_ref(inst))?,
                ts: real(r.get_ref(ts))?,
                gen_tps: real(r.get_ref(gen))?,
                prompt_tps: real(r.get_ref(prompt))?,
                draft_acc: opt_real(r.get_ref(acc))?,
                draft_len: opt_real(r.get_ref(dlen))?,
                spec_type: match r.get_ref(spec).map_err(|_| Read::Raised)? {
                    ValueRef::Null => None,
                    v => Some(text(Ok(v))?),
                },
            });
        }
        Ok(out)
    }

    /// `db.recent_timings(model_path=, alias=, min_gen_tokens=16, limit=)`.
    pub(super) fn recent_timings(
        &self,
        model_path: &str,
        alias: &str,
        limit: i64,
    ) -> Result<Vec<bt::Row>, Read> {
        let min = bt::MIN_GEN_TOKENS;
        if !alias.is_empty() {
            self.timing_rows(
                "SELECT * FROM req_timing WHERE gen_tokens >= ? AND alias = ? ORDER BY ts DESC LIMIT ?",
                &[&min, &alias, &limit],
            )
        } else if !model_path.is_empty() {
            self.timing_rows(
                "SELECT * FROM req_timing WHERE gen_tokens >= ? AND model_path = ? ORDER BY ts DESC LIMIT ?",
                &[&min, &model_path, &limit],
            )
        } else {
            Ok(Vec::new())
        }
    }

    /// `db.timings_by_instance(model_path=, alias=, min_gen_tokens=16)`.
    pub(super) fn timings_by_instance(
        &self,
        model_path: &str,
        alias: &str,
    ) -> Result<Vec<bt::Row>, Read> {
        let min = bt::MIN_GEN_TOKENS;
        let select = "SELECT t.instance, t.gen_tps, t.prompt_tps, t.draft_acc, t.draft_len, t.ts, \
                      t.spec_type, t.backend FROM req_timing t WHERE ";
        if !alias.is_empty() {
            self.timing_rows(
                &format!("{select}gen_tokens >= ? AND alias = ? ORDER BY t.ts DESC"),
                &[&min, &alias],
            )
        } else if !model_path.is_empty() {
            self.timing_rows(
                &format!("{select}gen_tokens >= ? AND model_path = ? ORDER BY t.ts DESC"),
                &[&min, &model_path],
            )
        } else {
            Ok(Vec::new())
        }
    }

    /// `db.server_configs_for(instances)`: `{instance: argv}`, a later row for the same instance
    /// replacing an earlier one.
    pub(super) fn server_configs_for(
        &self,
        instances: &[String],
    ) -> Result<Vec<(String, bt::Dict)>, Read> {
        if instances.is_empty() {
            return Ok(Vec::new());
        }
        let marks = vec!["?"; instances.len()].join(",");
        let mut q = self
            .db
            .prepare(&format!(
                "SELECT instance, argv_json FROM server_config WHERE instance IN ({marks})"
            ))
            .map_err(|_| Read::Raised)?;
        let mut rows = q
            .query(rusqlite::params_from_iter(instances.iter()))
            .map_err(|_| Read::Raised)?;
        let mut out: Vec<(String, bt::Dict)> = Vec::new();
        while let Some(r) = rows.next().map_err(|_| Read::Raised)? {
            let inst = text(r.get_ref(0))?;
            let a = argv(r.get_ref(1))?;
            match out.iter_mut().find(|(k, _)| *k == inst) {
                Some((_, slot)) => *slot = a,
                None => out.push((inst, a)),
            }
        }
        Ok(out)
    }
}

fn text(v: rusqlite::Result<ValueRef<'_>>) -> Result<String, Read> {
    match v.map_err(|_| Read::Raised)? {
        // sqlite3 raises on TEXT that is not UTF-8.
        ValueRef::Text(t) => std::str::from_utf8(t)
            .map(str::to_owned)
            .map_err(|_| Read::Raised),
        _ => Err(Read::Refused),
    }
}

fn real(v: rusqlite::Result<ValueRef<'_>>) -> Result<f64, Read> {
    match v.map_err(|_| Read::Raised)? {
        ValueRef::Real(r) => Ok(r),
        _ => Err(Read::Refused),
    }
}

fn opt_real(v: rusqlite::Result<ValueRef<'_>>) -> Result<Option<f64>, Read> {
    match v.map_err(|_| Read::Raised)? {
        ValueRef::Null => Ok(None),
        ValueRef::Real(r) => Ok(Some(r)),
        _ => Err(Read::Refused),
    }
}

/// `json.loads(argv_json or "{}")` with `except ValueError: {}`; anything but an object of
/// strings is refused (Python would carry a non-string value on, or trip over a non-dict).
fn argv(v: rusqlite::Result<ValueRef<'_>>) -> Result<bt::Dict, Read> {
    let raw = text(v)?;
    let raw = if raw.is_empty() { "{}".to_owned() } else { raw };
    let limits = model_files::json::Limits {
        string_chars: 1 << 24,
        bytes: 1 << 26,
        ..model_files::json::Limits::default()
    };
    if raw.len() > limits.bytes {
        return Err(Read::Refused);
    }
    let parsed = match model_files::json::parse_with_limits(&raw, limits) {
        Ok(v) => v,
        Err(_) => return Ok(bt::Dict::new()),
    };
    let model_files::json::Value::Obj(pairs) = parsed else {
        return Err(Read::Refused);
    };
    let mut out = bt::Dict::new();
    for (k, v) in pairs {
        let model_files::json::Value::Str(v) = v else {
            return Err(Read::Refused);
        };
        out.insert(k, v);
    }
    Ok(out)
}

/// `json.dumps(argv or {}, sort_keys=True)`: `", "` and `": "`, every non-ASCII character escaped.
pub(super) fn dumps_sorted(argv: &bt::Dict) -> String {
    let mut items: Vec<(&String, &String)> = argv.iter().collect();
    items.sort_by(|a, b| a.0.cmp(b.0));
    let mut out = String::from("{");
    for (i, (k, v)) in items.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        ascii_string(&mut out, k);
        out.push_str(": ");
        ascii_string(&mut out, v);
    }
    out.push('}');
    out
}

fn ascii_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 || (c as u32) > 0x7e => {
                let mut buf = [0u16; 2];
                for unit in c.encode_utf16(&mut buf) {
                    out.push_str(&format!("\\u{unit:04x}"));
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
}
