//! noevia#1318: native, read-only `GET /api/v1/benchmark/runs/{run_id}`.
//!
//! Python (`api.benchmark_run`, `helpers._bench_charts`, `db.bench_*`, `db.badges_for`):
//!
//! ```python
//! run = db.bench_run(run_id)
//! if run is None: raise HTTPException(404, "run not found")
//! results, sweeps, variants = db.bench_results(run_id), db.bench_sweeps(run_id), db.bench_variants(run_id)
//! return {"run": _row(run), "variants": [...], "results": [...], "sweeps": [...],
//!         "charts": _bench_charts(run_id, results, sweeps),
//!         "badges": {v["alias"]: [_row(b) for b in db.badges_for(v["alias"])] for v in variants}}
//! ```
//!
//! Everything comes from `DATA_DIR/model_loader.db` (read-only, never created) except two live
//! values in the charts: `capacity_gb` (the GPU sampler, through `helpers._backend_list`) and
//! `vram_predicted` (autoconfig on the model files against those backends). Those are asked of
//! the Python child through its private `GET /api/v1/internal/bench-run-runtime` call and placed
//! verbatim, never re-implemented, as model detail does for `loadedOn` and `fit`.
//!
//! Path and integer spellings follow the actual FastAPI contract (`paths`), including validation,
//! redirects and SQLite overflow. Live chart compatibility selection remains independent.
use model_files::json::Value as Json;
use rusqlite::{types::ValueRef, Connection};
use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

/// The child's private call for the live chart values.
pub const RUNTIME_PATH: &str = "/api/v1/internal/bench-run-runtime";
/// Largest runtime answer read from the child.
pub const RUNTIME_CAP: usize = 1024 * 1024;
/// Answer when the child cannot give the live values.
pub const REFUSAL: &str = "Native benchmark run is unavailable";
/// Most rows read from any one table for one run.
pub const ROW_CAP: usize = 100_000;
/// Largest answer assembled.
pub const RESPONSE_CAP: usize = 64 * 1024 * 1024;
/// Most distinct aliases sent to the child.
pub const ALIAS_CAP: usize = 1024;
const VALUE_CAP: i32 = 16 * 1024 * 1024;
const BUSY_WAIT: Duration = Duration::from_millis(3000);
const DEADLINE: Duration = Duration::from_millis(3500);

/// A JSON string literal as `json.dumps(ensure_ascii=False)` writes it.
pub(crate) fn json_string(text: &str) -> String {
    serde_json::to_string(text).unwrap_or_else(|_| "\"\"".to_owned())
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Json(u16, Vec<u8>),
    /// Python would raise, or a native bound was passed: FastAPI's 500.
    Failed,
    /// The child's live values could not be had: 503 [`REFUSAL`].
    Unavailable,
}

pub mod paths;

/// A number as Python holds it: `int` stays `int` through `round`, `float` is a float.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Num {
    Int(i64),
    Float(f64),
}

impl Num {
    fn value(self) -> f64 {
        match self {
            #[allow(clippy::cast_precision_loss)]
            Num::Int(n) => n as f64,
            Num::Float(f) => f,
        }
    }
    fn truthy(self) -> bool {
        self.value() != 0.0
    }
    /// JSON text as FastAPI writes a `-> dict` answer: pydantic's JSON mode writes non-finite
    /// floats as `null` (noevia#1935).
    pub fn render(self) -> Option<String> {
        match self {
            Num::Int(n) => Some(n.to_string()),
            Num::Float(f) if f.is_finite() => Some(gguf::py_float_repr(f)),
            Num::Float(_) => Some("null".to_owned()),
        }
    }
}

/// Python's `round(x, digits)`: an int is unchanged, a float is correctly rounded, ties to even
/// on the exact binary value (Rust's fixed-precision formatting does the same).
pub fn py_round(x: Num, digits: usize) -> Option<Num> {
    match x {
        Num::Int(_) => Some(x),
        Num::Float(f) if f.is_finite() => format!("{f:.digits$}").parse().ok().map(Num::Float),
        // `round(inf, n)` and `round(nan, n)` return the value itself.
        Num::Float(_) => Some(x),
    }
}

/// `helpers._median` on values already known truthy: sorted (stable, numeric), middle or the
/// float mean of the two middle values.
pub fn median(values: &[Num]) -> Option<Num> {
    let mut xs: Vec<Num> = values.iter().copied().filter(|x| x.truthy()).collect();
    if xs.is_empty() {
        return Some(Num::Float(0.0));
    }
    if xs.iter().any(|x| x.value().is_nan()) {
        return None;
    }
    xs.sort_by(|a, b| a.value().total_cmp(&b.value()));
    let n = xs.len();
    Some(if n % 2 == 1 {
        *xs.get(n / 2)?
    } else {
        Num::Float((xs.get(n / 2 - 1)?.value() + xs.get(n / 2)?.value()) / 2.0)
    })
}

/// One stored value, owned.
#[derive(Debug, Clone, PartialEq)]
pub enum Val {
    Null,
    Int(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

impl Val {
    fn truthy(&self) -> bool {
        match self {
            Val::Null => false,
            Val::Int(n) => *n != 0,
            Val::Real(f) => *f != 0.0,
            Val::Text(s) => !s.is_empty(),
            Val::Blob(b) => !b.is_empty(),
        }
    }
    fn num(&self) -> Option<Num> {
        match self {
            Val::Int(n) => Some(Num::Int(*n)),
            Val::Real(f) => Some(Num::Float(*f)),
            _ => None,
        }
    }
    fn text(&self) -> Option<&str> {
        match self {
            Val::Text(s) => Some(s),
            _ => None,
        }
    }
    /// `json.dumps` of what sqlite3 handed Python; `None` where FastAPI would raise.
    fn render(&self) -> Option<String> {
        match self {
            Val::Null => Some("null".into()),
            Val::Int(n) => Some(n.to_string()),
            Val::Real(f) => Num::Float(*f).render(),
            Val::Text(s) => Some(json_string(s)),
            Val::Blob(b) => std::str::from_utf8(b).ok().map(json_string),
        }
    }
}

/// `dict(row)`: names in column order; a repeated name keeps its last value at its first place.
#[derive(Debug, Clone, Default)]
pub struct Row(pub(crate) Vec<(String, Val)>);

impl Row {
    fn get(&self, name: &str) -> Option<&Val> {
        self.0.iter().rev().find(|(n, _)| n == name).map(|(_, v)| v)
    }
    fn truthy(&self, name: &str) -> bool {
        self.get(name).is_some_and(Val::truthy)
    }
    pub(crate) fn render(&self) -> Option<String> {
        let mut seen: Vec<&str> = Vec::new();
        let mut out = String::from("{");
        for (name, _) in &self.0 {
            if seen.contains(&name.as_str()) {
                continue;
            }
            if !seen.is_empty() {
                out.push(',');
            }
            seen.push(name);
            out.push_str(&json_string(name));
            out.push(':');
            out.push_str(&self.get(name)?.render()?);
        }
        out.push('}');
        Some(out)
    }
}

/// Everything the store gives for one run.
#[derive(Debug, Default)]
pub struct Stored {
    pub run: Row,
    pub variants: Vec<Row>,
    pub results: Vec<Row>,
    pub sweeps: Vec<Row>,
    /// `badges_for(alias)` per variant, in variant order.
    pub badges: Vec<(String, Vec<Row>)>,
}

/// The native part of the answer, waiting for the child's live values.
#[derive(Debug)]
pub struct Located {
    /// Everything before `"capacity_gb":` …
    head: String,
    /// … and the charts between `capacity_gb` and `vram_predicted`.
    middle: String,
    /// `vram_labels`, in order: the aliases whose prediction the child gives.
    pub vram_aliases: Vec<String>,
    /// Everything after `vram_predicted`.
    tail: String,
}

pub enum Step {
    Done(Outcome),
    Located(Located),
}

pub struct BenchRuns {
    database: PathBuf,
}

impl BenchRuns {
    pub fn new(root: &Path) -> Self {
        Self {
            database: root.join("model_loader.db"),
        }
    }

    /// The store part, on a blocking thread with a busy wait and progress deadline.
    pub async fn locate(&self, run_id: i64) -> Step {
        let database = self.database.clone();
        let deadline = Instant::now() + DEADLINE;
        // noevia#2006: admitted: a dropped request keeps its permit until the read returns.
        match crate::docker::spawn_blocking_admitted(move || read(&database, run_id, deadline))
            .await
        {
            Ok(Ok(None)) => Step::Done(Outcome::Json(
                404,
                br#"{"detail":"run not found"}"#.to_vec(),
            )),
            Ok(Ok(Some(stored))) => {
                build(&stored).map_or(Step::Done(Outcome::Failed), Step::Located)
            }
            _ => Step::Done(Outcome::Failed),
        }
    }
}

fn val(value: ValueRef<'_>) -> Val {
    match value {
        ValueRef::Null => Val::Null,
        ValueRef::Integer(n) => Val::Int(n),
        ValueRef::Real(f) => Val::Real(f),
        // sqlite3 raises on TEXT that is not UTF-8; render() refuses the blob form the same way.
        ValueRef::Text(t) => std::str::from_utf8(t)
            .map_or_else(|_| Val::Blob(vec![0xff]), |s| Val::Text(s.to_owned())),
        ValueRef::Blob(b) => Val::Blob(b.to_vec()),
    }
}

fn rows(db: &Connection, sql: &str, param: rusqlite::types::Value) -> Result<Vec<Row>, ()> {
    rows_with(db, sql, &[param])
}

/// `dict(r) for r in cursor` for any parameter list (one row cap for every query).
pub(crate) fn rows_with(
    db: &Connection,
    sql: &str,
    params: &[rusqlite::types::Value],
) -> Result<Vec<Row>, ()> {
    let mut statement = db.prepare(sql).map_err(|_| ())?;
    let names: Vec<String> = statement
        .column_names()
        .into_iter()
        .map(str::to_owned)
        .collect();
    let mut cursor = statement
        .query(rusqlite::params_from_iter(params.iter()))
        .map_err(|_| ())?;
    let mut out = Vec::new();
    while let Some(row) = cursor.next().map_err(|_| ())? {
        if out.len() >= ROW_CAP {
            return Err(());
        }
        let mut fields = Vec::with_capacity(names.len());
        for (i, name) in names.iter().enumerate() {
            fields.push((name.clone(), val(row.get_ref(i).map_err(|_| ())?)));
        }
        out.push(Row(fields));
    }
    Ok(out)
}

/// The store reads, in Python's order. `Ok(None)` is "run not found".
pub(crate) fn read(database: &Path, run_id: i64, deadline: Instant) -> Result<Option<Stored>, ()> {
    // noevia#1922: opened as Python opens it (read-write without CREATE, query_only); the open
    // and every later store error are logged once.
    let logged = |e: rusqlite::Error| crate::sqlite_read::log_once("bench_runs", &e);
    let db = crate::sqlite_read::open(database, "bench_runs").map_err(|_| ())?;
    db.set_limit(rusqlite::limits::Limit::SQLITE_LIMIT_LENGTH, VALUE_CAP)
        .map_err(logged)?;
    db.busy_timeout(BUSY_WAIT).map_err(|_| ())?;
    db.progress_handler(1000, Some(move || Instant::now() >= deadline))
        .map_err(|_| ())?;
    let id = rusqlite::types::Value::Integer(run_id);
    let run = rows(&db, "SELECT * FROM bench_run WHERE id = ?", id.clone())?;
    let Some(run) = run.into_iter().next() else {
        return Ok(None);
    };
    let results = rows(
        &db,
        "SELECT x.*, v.alias FROM bench_result x JOIN bench_variant v ON x.variant_id = v.id \
         WHERE v.run_id = ? ORDER BY v.id, x.prompt_name, x.rep",
        id.clone(),
    )?;
    let sweeps = rows(
        &db,
        "SELECT * FROM bench_sweep WHERE run_id = ? ORDER BY alias, n_depth, id",
        id.clone(),
    )?;
    let variants = rows(
        &db,
        "SELECT * FROM bench_variant WHERE run_id = ? ORDER BY id",
        id,
    )?;
    let mut badges = Vec::new();
    for v in &variants {
        // `v["alias"]` is a dict key: sqlite3 gives TEXT as str; anything else would still key
        // the dict, but such a row cannot come from the bench writer, so it is refused here.
        let alias = v.get("alias").and_then(Val::text).ok_or(())?.to_owned();
        let list = rows(
            &db,
            "SELECT * FROM model_badge WHERE alias = ? ORDER BY category",
            rusqlite::types::Value::Text(alias.clone()),
        )?;
        badges.push((alias, list));
    }
    Ok(Some(Stored {
        run,
        variants,
        results,
        sweeps,
        badges,
    }))
}

fn list(rows: &[Row]) -> Option<String> {
    let mut parts = Vec::with_capacity(rows.len());
    for row in rows {
        parts.push(row.render()?);
    }
    Some(format!("[{}]", parts.join(",")))
}

fn opt(x: Option<Num>) -> Option<String> {
    x.map_or(Some("null".into()), Num::render)
}

fn strings(xs: &[String]) -> String {
    format!(
        "[{}]",
        xs.iter()
            .map(|s| json_string(s))
            .collect::<Vec<_>>()
            .join(",")
    )
}

/// `sum(json.loads(peak_vram_json or "[]"))`: `Ok(None)` is the ValueError Python skips,
/// `Err` the TypeError it raises.
pub(crate) fn peak_sum(value: Option<&Val>) -> Result<Option<Num>, ()> {
    let text = match value {
        None | Some(Val::Null) => "[]".to_owned(),
        Some(v) if !v.truthy() => "[]".to_owned(),
        Some(Val::Text(s)) => s.clone(),
        Some(Val::Blob(b)) => match std::str::from_utf8(b) {
            Ok(s) => s.to_owned(),
            Err(_) => return Ok(None),
        },
        Some(_) => return Err(()),
    };
    let Ok(parsed) = model_files::json::parse(&text) else {
        return Ok(None);
    };
    let items: Vec<&Json> = match &parsed {
        Json::Arr(items) => items.iter().collect(),
        Json::Obj(pairs) if pairs.is_empty() => Vec::new(),
        Json::Str(s) if s.is_empty() => Vec::new(),
        _ => return Err(()),
    };
    let mut total = Num::Int(0);
    for item in items {
        let x = match item {
            Json::Bool(b) => Num::Int(i64::from(*b)),
            Json::Int(n) => Num::Int(n.to_string().parse().map_err(|_| ())?),
            Json::Float(f) => Num::Float(*f),
            _ => return Err(()),
        };
        total = match (total, x) {
            (Num::Int(a), Num::Int(b)) => Num::Int(a.checked_add(b).ok_or(())?),
            (a, b) => Num::Float(a.value() + b.value()),
        };
    }
    Ok(Some(total))
}

fn text_of<'a>(row: &'a Row, name: &str) -> Result<&'a str, ()> {
    row.get(name).and_then(Val::text).ok_or(())
}

fn depth_of(row: &Row) -> Result<i64, ()> {
    match row.get("n_depth") {
        None | Some(Val::Null) => Ok(0),
        Some(Val::Int(n)) => Ok(*n),
        Some(Val::Real(f)) if *f == 0.0 => Ok(0),
        _ => Err(()),
    }
}

/// Everything native, in Python's key order; `None` where Python would raise.
pub fn build(stored: &Stored) -> Option<Located> {
    let clean: Vec<&Row> = stored
        .results
        .iter()
        .filter(|r| !r.truthy("cold") && !r.truthy("contended") && !r.truthy("err"))
        .collect();
    let mut aliases: Vec<String> = Vec::new();
    let mut prompts: Vec<String> = Vec::new();
    for r in &clean {
        let a = text_of(r, "alias").ok()?.to_owned();
        let p = text_of(r, "prompt_name").ok()?.to_owned();
        if !aliases.contains(&a) {
            aliases.push(a);
        }
        if !prompts.contains(&p) {
            prompts.push(p);
        }
    }
    aliases.sort();
    prompts.sort();
    let pick = |alias: &str, prompt: &str, field: &str, digits: usize| -> Option<String> {
        let mut vals = Vec::new();
        for r in &clean {
            if text_of(r, "alias").ok()? == alias && text_of(r, "prompt_name").ok()? == prompt {
                let v = r.get(field).cloned().unwrap_or(Val::Null);
                if v.truthy() {
                    vals.push(v.num()?);
                }
            }
        }
        if vals.is_empty() {
            return Some("null".into());
        }
        opt(Some(py_round(median(&vals)?, digits)?))
    };
    let series = |field: &str, digits: usize| -> Option<String> {
        let mut out = Vec::new();
        for p in &prompts {
            let mut data = Vec::new();
            for a in &aliases {
                data.push(pick(a, p, field, digits)?);
            }
            out.push(format!(
                "{{\"label\":{},\"data\":[{}]}}",
                json_string(p),
                data.join(",")
            ));
        }
        Some(format!("[{}]", out.join(",")))
    };
    let gen = series("gen_tps", 1)?;
    let ttft = series("ttft_ms", 0)?;

    let mut depths: Vec<i64> = Vec::new();
    for w in &stored.sweeps {
        let d = depth_of(w).ok()?;
        if !depths.contains(&d) {
            depths.push(d);
        }
    }
    depths.sort_unstable();
    let mut sweep_aliases: Vec<String> = Vec::new();
    for w in &stored.sweeps {
        let a = text_of(w, "alias").ok()?.to_owned();
        if !sweep_aliases.contains(&a) {
            sweep_aliases.push(a);
        }
    }
    sweep_aliases.sort();
    let mut decay = Vec::new();
    for a in &sweep_aliases {
        for (kind, want_gen) in [("tg", true), ("pp", false)] {
            let mut pts = Vec::new();
            for d in &depths {
                let mut first = None;
                for w in &stored.sweeps {
                    if text_of(w, "alias").ok()? == a
                        && depth_of(w).ok()? == *d
                        && w.truthy("n_gen") == want_gen
                        && w.truthy("avg_ts")
                    {
                        first = Some(w.get("avg_ts")?.num()?);
                        break;
                    }
                }
                pts.push(match first {
                    Some(x) => Some(py_round(x, 1)?),
                    None => None,
                });
            }
            if pts.iter().any(Option::is_some) {
                let mut data = Vec::new();
                for p in &pts {
                    data.push(opt(*p)?);
                }
                decay.push(format!(
                    "{{\"label\":{},\"data\":[{}],\"kind\":{}}}",
                    json_string(&format!("{a} {kind}")),
                    data.join(","),
                    json_string(kind)
                ));
            }
        }
    }

    let mut vram_aliases = Vec::new();
    let mut vram_measured = Vec::new();
    for a in &aliases {
        let mut peaks = Vec::new();
        for r in &clean {
            if text_of(r, "alias").ok()? != a {
                continue;
            }
            if let Some(sum) = peak_sum(r.get("peak_vram_json")).ok()? {
                peaks.push(sum);
            }
        }
        if peaks.is_empty() {
            continue;
        }
        // max(): the first of equal maxima.
        let mut best = *peaks.first()?;
        for p in &peaks {
            if p.value().is_nan() || best.value().is_nan() {
                return None;
            }
            if p.value() > best.value() {
                best = *p;
            }
        }
        vram_aliases.push(a.clone());
        vram_measured.push(py_round(best, 2)?.render()?);
    }
    if aliases.len() > ALIAS_CAP || vram_aliases.len() > ALIAS_CAP {
        return None;
    }

    let mut badges = Vec::new();
    for (alias, rows) in &stored.badges {
        badges.push((alias.clone(), list(rows)?));
    }
    // A dict: a repeated alias keeps its first place and its last list.
    let mut keys: Vec<&str> = Vec::new();
    for (a, _) in &badges {
        if !keys.contains(&a.as_str()) {
            keys.push(a);
        }
    }
    let badge_fields: Vec<String> = keys
        .iter()
        .map(|k| {
            let rows = badges
                .iter()
                .rev()
                .find(|(a, _)| a == k)
                .map_or("[]", |(_, r)| r.as_str());
            format!("{}:{}", json_string(k), rows)
        })
        .collect();

    let head = format!(
        "{{\"run\":{},\"variants\":{},\"results\":{},\"sweeps\":{},\"charts\":{{\"capacity_gb\":",
        stored.run.render()?,
        list(&stored.variants)?,
        list(&stored.results)?,
        list(&stored.sweeps)?
    );
    let middle = format!(
        ",\"aliases\":{},\"gen\":{},\"ttft\":{},\"depths\":[{}],\"decay\":[{}],\"vram_labels\":{},\"vram_measured\":[{}],\"vram_predicted\":",
        strings(&aliases),
        gen,
        ttft,
        depths.iter().map(i64::to_string).collect::<Vec<_>>().join(","),
        decay.join(","),
        strings(&vram_aliases),
        vram_measured.join(",")
    );
    let tail = format!("}},\"badges\":{{{}}}}}", badge_fields.join(","));
    if head.len() + middle.len() + tail.len() > RESPONSE_CAP {
        return None;
    }
    Some(Located {
        head,
        middle,
        vram_aliases,
        tail,
    })
}

/// The runtime call's query: every alias whose prediction is needed, in order.
pub fn runtime_query(located: &Located) -> String {
    let mut query = form_urlencoded::Serializer::new(String::new());
    for a in &located.vram_aliases {
        query.append_pair("alias", a);
    }
    query.finish()
}

/// Place the child's `{"capacity_gb": x, "predicted": {alias: y|null}}` into the answer.
pub fn splice(located: &Located, body: &[u8]) -> Outcome {
    use serde_json::value::RawValue;
    let Ok(fields) =
        serde_json::from_slice::<std::collections::HashMap<String, Box<RawValue>>>(body)
    else {
        return Outcome::Unavailable;
    };
    let (Some(capacity), Some(predicted)) = (fields.get("capacity_gb"), fields.get("predicted"))
    else {
        return Outcome::Unavailable;
    };
    let Ok(predicted) =
        serde_json::from_str::<std::collections::HashMap<String, Box<RawValue>>>(predicted.get())
    else {
        return Outcome::Unavailable;
    };
    let mut values = Vec::with_capacity(located.vram_aliases.len());
    for a in &located.vram_aliases {
        let Some(v) = predicted.get(a) else {
            return Outcome::Unavailable;
        };
        values.push(v.get());
    }
    let out = format!(
        "{}{}{}[{}]{}",
        located.head,
        capacity.get(),
        located.middle,
        values.join(","),
        located.tail
    );
    Outcome::Json(200, out.into_bytes())
}

#[cfg(test)]
mod tests;
