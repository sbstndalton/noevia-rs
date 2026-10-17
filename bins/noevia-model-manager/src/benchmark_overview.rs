//! noevia#1318: native `GET /api/v1/benchmark` (the benchmark page's overview).
//!
//! Everything but `job` is read here exactly as `api.benchmark_overview` reads it: the
//! models.ini section names and their llama-bench flags, the prompt list, the effective backend
//! names, the two token limits, the last 25 runs and the badge categories. `job` is the live state
//! of the benchmark runner, which lives in the Python process with the runner thread; it is asked
//! of the child (`GET /api/v1/internal/bench-job`) and spliced in byte for byte, so the job
//! controls (start, sweep, progress, cancel) and the runner stay with Python until the runner is
//! ported as one piece.

use crate::bench_runs::{json_string, rows_with, Row};
use crate::section_writes::cfg;
use rusqlite::types::Value;
use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

pub const JOB_PATH: &str = "/api/v1/internal/bench-job";
/// The job's log tail is capped at 200 lines by the runner; this bounds the whole answer.
pub const JOB_CAP: usize = 4 * 1024 * 1024;
/// models.ini is read whole, as Python reads it.
pub const INI_CAP: u64 = 16 * 1024 * 1024;
/// `bench.DEFAULT_MAX_TOKENS` and `bench.MAX_TOKENS_CEILING`.
pub const MAX_TOKENS_DEFAULT: u32 = 3072;
pub const MAX_TOKENS_CEILING: u32 = 8192;
/// `db.BADGE_CATEGORIES`, in order.
pub const CATEGORIES: [(&str, &str); 5] = [
    ("coding", "Coding"),
    ("writing", "Creative writing"),
    ("reasoning", "Reasoning"),
    ("tools", "Tool use"),
    ("vision", "Vision"),
];
/// `bench._SWEEP_FROM_INI`: models.ini keys carried into a sweep, with their llama-bench flag.
const SWEEP_FROM_INI: [(&str, &str); 11] = [
    ("cache-type-k", "-ctk"),
    ("cache-type-v", "-ctv"),
    ("flash-attn", "-fa"),
    ("ngl", "-ngl"),
    ("n-gpu-layers", "-ngl"),
    ("n-cpu-moe", "-ncmoe"),
    ("split-mode", "-sm"),
    ("tensor-split", "-ts"),
    ("main-gpu", "-mg"),
    ("ubatch-size", "-ub"),
    ("batch-size", "-b"),
];
const VALUE_CAP: i32 = 16 * 1024 * 1024;
const BUSY_WAIT: Duration = Duration::from_millis(3000);
const DEADLINE: Duration = Duration::from_millis(3500);

pub struct BenchmarkOverview {
    database: PathBuf,
    ini: PathBuf,
}

/// What the native side reads before it asks the child for `job`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Local {
    pub sections: Vec<String>,
    pub sweep: Vec<(String, String)>,
    pub prompts: String,
    pub runs: String,
}

impl BenchmarkOverview {
    pub fn new(data: &Path, ini: &Path) -> Self {
        Self {
            database: data.join("model_loader.db"),
            ini: ini.to_owned(),
        }
    }

    /// The models.ini and store reads, on a blocking thread. `Err` is FastAPI's 500.
    pub(crate) async fn local(&self) -> Result<Local, ()> {
        let database = self.database.clone();
        let ini = self.ini.clone();
        // noevia#2006: admitted: a dropped request keeps its permit until the reads return.
        crate::docker::spawn_blocking_admitted(move || {
            let (sections, sweep) = sections(&ini)?;
            let (prompts, runs) = store(&database, Instant::now() + DEADLINE)?;
            Ok(Local {
                sections,
                sweep,
                prompts,
                runs,
            })
        })
        .await
        .map_err(|_| ())?
    }
}

/// `ini.section_names()` and `bench.sweep_args_for_section` for each, from one read.
/// Section names and each one's `sweepArgs` value.
type Sections = (Vec<String>, Vec<(String, String)>);

pub(crate) fn sections(ini: &Path) -> Result<Sections, ()> {
    let text = match std::fs::metadata(ini) {
        // `settings.models_ini_path.exists()` is false: an empty parser.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((Vec::new(), Vec::new())),
        Err(_) => return Err(()),
        Ok(m) if m.len() > INI_CAP => return Err(()),
        Ok(_) => std::fs::read(ini).map_err(|_| ())?,
    };
    let text = String::from_utf8(text).map_err(|_| ())?;
    let cp = cfg::parse_allowing(&text).map_err(|_| ())?;
    let names: Vec<String> = cp.names().into_iter().map(str::to_owned).collect();
    let mut sweep = Vec::with_capacity(names.len());
    for name in &names {
        let args = sweep_args(&cp, name).join(" ");
        match sweep
            .iter_mut()
            .find(|(n, _): &&mut (String, String)| n == name)
        {
            Some(slot) => slot.1 = args,
            None => sweep.push((name.clone(), args)),
        }
    }
    Ok((names, sweep))
}

/// `bench.sweep_args_for_section(name)` on an already-parsed file.
pub fn sweep_args(cp: &cfg::Parser, name: &str) -> Vec<String> {
    let Some(vals) = cp.items(name) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut seen: Vec<&str> = Vec::new();
    for (key, flag) in SWEEP_FROM_INI {
        let v = cfg::strip(vals.get(key).unwrap_or_default());
        if v.is_empty() || seen.contains(&flag) {
            continue;
        }
        let v = if flag == "-ts" {
            v.replace(',', "/")
        } else {
            v.to_owned()
        };
        out.push(flag.to_owned());
        out.push(v);
        seen.push(flag);
    }
    out
}

/// `db.list_prompts()` and `db.bench_runs(limit=25)`, rendered as FastAPI renders them.
pub(crate) fn store(database: &Path, deadline: Instant) -> Result<(String, String), ()> {
    let logged = |e: rusqlite::Error| crate::sqlite_read::log_once("benchmark_overview", &e);
    let db = crate::sqlite_read::open(database, "benchmark_overview").map_err(|_| ())?;
    db.set_limit(rusqlite::limits::Limit::SQLITE_LIMIT_LENGTH, VALUE_CAP)
        .map_err(logged)?;
    db.busy_timeout(BUSY_WAIT).map_err(|_| ())?;
    db.progress_handler(1000, Some(move || Instant::now() >= deadline))
        .map_err(|_| ())?;
    let prompts = rows_with(
        &db,
        "SELECT id, name, body, created_at FROM prompts ORDER BY name COLLATE NOCASE",
        &[],
    )?;
    let runs = rows_with(
        &db,
        "SELECT r.*, (SELECT COUNT(*) FROM bench_variant v WHERE v.run_id = r.id) AS n_variants, \
                (SELECT COUNT(*) FROM bench_result x JOIN bench_variant v2 ON x.variant_id = v2.id \
                 WHERE v2.run_id = r.id) AS n_results \
         FROM bench_run r ORDER BY r.id DESC LIMIT ?",
        &[Value::Integer(25)],
    )?;
    Ok((list(&prompts).ok_or(())?, list(&runs).ok_or(())?))
}

fn list(rows: &[Row]) -> Option<String> {
    let mut parts = Vec::with_capacity(rows.len());
    for row in rows {
        parts.push(row.render()?);
    }
    Some(format!("[{}]", parts.join(",")))
}

/// The child's `_bench_job()` must be one JSON object; anything else is not spliced.
pub fn job_object(bytes: &[u8]) -> bool {
    matches!(
        serde_json::from_slice::<serde_json::Value>(bytes),
        Ok(serde_json::Value::Object(_))
    )
}

/// `api.benchmark_overview()`'s body, in its key order.
pub fn render(local: &Local, backends: &[String], job: &[u8]) -> Option<Vec<u8>> {
    let strings = |xs: &[String]| {
        format!(
            "[{}]",
            xs.iter()
                .map(|s| json_string(s))
                .collect::<Vec<_>>()
                .join(",")
        )
    };
    let sweep = local
        .sweep
        .iter()
        .map(|(n, a)| format!("{}:{}", json_string(n), json_string(a)))
        .collect::<Vec<_>>()
        .join(",");
    let categories = CATEGORIES
        .iter()
        .map(|(k, v)| {
            format!(
                "{{\"key\":{},\"label\":{}}}",
                json_string(k),
                json_string(v)
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    let job = std::str::from_utf8(job).ok()?.trim();
    Some(
        format!(
            "{{\"sections\":{},\"sweepArgs\":{{{sweep}}},\"prompts\":{},\"backends\":{},\
             \"maxTokensDefault\":{MAX_TOKENS_DEFAULT},\"maxTokensCeiling\":{MAX_TOKENS_CEILING},\
             \"job\":{job},\"runs\":{},\"categories\":[{categories}]}}",
            strings(&local.sections),
            local.prompts,
            strings(backends),
            local.runs,
        )
        .into_bytes(),
    )
}
