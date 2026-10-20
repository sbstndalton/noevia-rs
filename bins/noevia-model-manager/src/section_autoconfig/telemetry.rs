//! noevia#1318 step 3d: the section autoconfig's telemetry natively (`telemetry.ingest`,
//! `stats_for` and `config_history` as `api.section_autoconfig` calls them).
//!
//! This process is the only writer of the telemetry
//! tables (`req_timing`, `server_config`) and of the telemetry keys in `kv`
//! (`telemetry_last_ingest`, `telemetry_log_watermark:<backend>`,
//! `telemetry_continuous_since:<backend>`). The shipping process starts no child (noevia#2055),
//! so there is no child writer to hand the tables over from and no child telemetry guard to ask
//! (the former `GET /api/v1/internal/telemetry-guard` probe, #2041, went with the child); the
//! lifetime writer lease on `DATA_DIR` keeps a second manager process off the store.
//!
//! The ingest follows Python step by step: the 20-second interval on `telemetry_last_ingest`;
//! for each effective backend the log tail (`tail=20000`, timestamps, docker-py's frame
//! handling and `errors="replace"` decoding), the watermark and continuous-span keys, the
//! gap-free test, `parse_log` (the `backend-telemetry` crate) with the stored-instance lookup of
//! `db.server_instances_on_port`, `record_server_configs` and `record_timings` (the same SQL,
//! each its own transaction as each Python call is), then the two keys, an error there skipping
//! only that backend; and the interval key last. A step on which Python raises gives the route's
//! empty measurements, as its `except` does.
//!
//! Bounds: a backend whose log answer is past 64 MiB, whose read takes longer than 60 s, whose
//! tail has more than 40,000 lines or names more than 4,096 ports is skipped for that pass, as
//! Python skips a backend whose read fails: nothing of it is written and its keys stay. It is
//! logged once per backend and reason and not read again for [`BACKOFF_S`], so a huge log is
//! not re-read on every request. Only where rows could be written wrong (a stored value of a
//! type Python would treat differently, a log shape the parser refuses) is the answer the
//! native 503, with nothing more written.
//!
//! The store is created by the mandatory bootstrap, which completes before the process serves
//! requests.
mod store;

use crate::backend_stats::{BackendStats, Clock, Logs};
use backend_telemetry as bt;
use model_files::json::Value;
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::Arc,
};

/// `_INGEST_INTERVAL_S`.
const INGEST_INTERVAL_S: f64 = 20.0;
/// `_LAST_INGEST_KEY`, `_WATERMARK_KEY`, `_CONTINUOUS_KEY`.
const LAST_INGEST_KEY: &str = "telemetry_last_ingest";
const WATERMARK_KEY: &str = "telemetry_log_watermark:";
const CONTINUOUS_KEY: &str = "telemetry_continuous_since:";
/// `stats_for(limit=400)`, `config_history(min_runs=2, min_samples=3)`.
const STATS_LIMIT: i64 = 400;

/// Why a step stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stop {
    /// Python raised: the route's `except` gives the empty measurements.
    Raised,
    /// Not followed exactly here: the native 503.
    Refused,
}

impl From<bt::Error> for Stop {
    fn from(e: bt::Error) -> Self {
        match e {
            bt::Error::Raised(_) => Stop::Raised,
            bt::Error::Unsupported(_) => Stop::Refused,
        }
    }
}

/// What the route places: `_plain(measured)`, `_plain(history)` and the prompt rate.
pub(crate) struct Measured {
    pub measured: String,
    pub history: String,
    pub prompt_tps: f64,
}

pub struct Telemetry {
    database: PathBuf,
    stats: Arc<BackendStats>,
    clock: Clock,
    /// One ingest at a time (Python's `_LOCK` serialises its store calls).
    ingest: tokio::sync::Mutex<()>,
    /// Backends skipped by a bound, until this clock value.
    backoff: std::sync::Mutex<HashMap<String, f64>>,
    /// The (backend, reason) pairs already logged.
    logged: std::sync::Mutex<HashSet<(String, &'static str)>>,
}

/// How long a backend skipped by a bound is left unread.
pub const BACKOFF_S: f64 = 600.0;

fn now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

impl Telemetry {
    pub fn new(data_dir: &std::path::Path, stats: Arc<BackendStats>) -> Self {
        Self::with_clock(data_dir, stats, Arc::new(now))
    }

    pub(crate) fn with_clock(
        data_dir: &std::path::Path,
        stats: Arc<BackendStats>,
        clock: Clock,
    ) -> Self {
        Telemetry {
            database: data_dir.join("model_loader.db"),
            stats,
            clock,
            ingest: tokio::sync::Mutex::new(()),
            backoff: std::sync::Mutex::new(HashMap::new()),
            logged: std::sync::Mutex::new(HashSet::new()),
        }
    }

    fn backing_off(&self, name: &str, now: f64) -> bool {
        self.backoff
            .lock()
            .ok()
            .and_then(|b| b.get(name).copied())
            .is_some_and(|until| now < until)
    }

    /// Skips `name` for [`BACKOFF_S`], logged once per backend and reason.
    fn back_off(&self, name: &str, now: f64, reason: &'static str) {
        if let Ok(mut b) = self.backoff.lock() {
            b.insert(name.to_owned(), now + BACKOFF_S);
        }
        let first = self
            .logged
            .lock()
            .is_ok_and(|mut l| l.insert((name.to_owned(), reason)));
        if first {
            eprintln!(
                "noevia-model-manager: telemetry ingest skipped backend {name} ({reason}); not \
                 read again for {BACKOFF_S} s; logged once per backend and reason"
            );
        }
    }

    /// `telemetry.ingest(services._effective_container_names())`.
    pub(crate) async fn ingest(&self) -> Result<(), Stop> {
        let _one = self.ingest.lock().await;
        let now = (self.clock)();
        let db = self.database.clone();
        let last =
            blocking(move || store::Store::open(&db)?.float_setting(LAST_INGEST_KEY)).await?;
        if now - last < INGEST_INTERVAL_S {
            return Ok(());
        }
        let names = self.stats.effective_names().await;
        for name in names {
            if self.backing_off(&name, now) {
                continue;
            }
            match tokio::time::timeout(LOG_TIMEOUT, self.stats.telemetry_logs(&name)).await {
                Ok(Logs::Text(text)) => {
                    // Process and release this tail before asking Docker for another. The
                    // transport cap applies per backend, not to the whole inventory (#2040).
                    let db = self.database.clone();
                    let backend = name.clone();
                    let reason = blocking(move || {
                        let store = store::Store::open(&db)?;
                        ingest_one(&store, &backend, &text)
                    })
                    .await?;
                    if let Some(reason) = reason {
                        self.back_off(&name, now, reason);
                    }
                }
                Ok(Logs::Skip) => {}
                Ok(Logs::TooLarge) => self.back_off(&name, now, "log answer over 64 MiB"),
                Err(_) => self.back_off(&name, now, "log read over 60 s"),
            }
        }
        let db = self.database.clone();
        blocking(move || {
            store::Store::open(&db)?
                .set_setting(LAST_INGEST_KEY, &gguf::py_float_repr(now))
                .map_err(|_| Stop::Raised)
        })
        .await?;
        Ok(())
    }

    /// The route's `try: ingest; stats_for; config_history except: Stats(), []`.
    pub(crate) async fn measure(&self, section: &str, model_rel: &str) -> Result<Measured, Stop> {
        match self.ingest().await {
            Ok(()) => {}
            Err(Stop::Raised) => return Ok(empty()),
            Err(Stop::Refused) => return Err(Stop::Refused),
        }
        self.read(section, model_rel).await
    }

    /// `stats_for` and `config_history` of one section, rendered (`Err(Raised)` never leaves:
    /// it is the route's empty measurements).
    pub(crate) async fn read(&self, section: &str, model_rel: &str) -> Result<Measured, Stop> {
        let db = self.database.clone();
        let (section, model_rel) = (section.to_owned(), model_rel.to_owned());
        let read = blocking(move || {
            let store = store::Store::open(&db).ok();
            let stats = match store
                .as_ref()
                .map(|s| s.recent_timings(&model_rel, &section, STATS_LIMIT))
            {
                Some(Ok(rows)) => bt::stats_for(&rows),
                Some(Err(store::Read::Refused)) => return Err(Stop::Refused),
                // `stats_for`'s own `except`: `Stats()`.
                Some(Err(store::Read::Raised)) | None => bt::Stats::default(),
            };
            let history = match store
                .as_ref()
                .map(|s| s.timings_by_instance(&model_rel, &section))
            {
                Some(Ok(rows)) if !rows.is_empty() => {
                    let instances = distinct(&rows);
                    let argvs = match store.as_ref().map(|s| s.server_configs_for(&instances)) {
                        Some(Ok(a)) => Some(a),
                        Some(Err(store::Read::Refused)) => return Err(Stop::Refused),
                        Some(Err(store::Read::Raised)) | None => None,
                    };
                    bt::config_history(&rows, argvs.as_deref(), 2, 3)?
                }
                Some(Ok(_)) => Vec::new(),
                Some(Err(store::Read::Refused)) => return Err(Stop::Refused),
                Some(Err(store::Read::Raised)) | None => Vec::new(),
            };
            Ok((stats, history))
        })
        .await;
        match read {
            Ok((stats, history)) => Ok(render(&stats, &history)),
            Err(Stop::Raised) => Ok(empty()),
            Err(Stop::Refused) => Err(Stop::Refused),
        }
    }
}

/// Most lines of one tail read (`tail=20000`, split as `str.splitlines` splits).
const MAX_LINES: usize = 40_000;
/// Most distinct ports whose stored instances one tail looks up.
const MAX_PORTS: usize = 4096;

/// One backend's log read; the docker-py client's timeout is not followed past this.
const LOG_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// The distinct instances in first-seen order, in one pass.
fn distinct(rows: &[bt::Row]) -> Vec<String> {
    rows.iter()
        .map(|r| r.instance.clone())
        .collect::<indexmap::IndexSet<String>>()
        .into_iter()
        .collect()
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, Stop> + Send + 'static,
) -> Result<T, Stop> {
    crate::docker::spawn_blocking_admitted(work)
        .await
        .map_err(|_| Stop::Refused)?
}

/// One backend of the ingest loop. `Ok(Some(reason))`: skipped by a bound, nothing written.
fn ingest_one(store: &store::Store, name: &str, text: &str) -> Result<Option<&'static str>, Stop> {
    // Docker answers at most 20,000 lines; Python splits on more separators than a newline, so
    // a tail past twice that is skipped here (it is not a server log).
    if bt::splitlines(text).len() > MAX_LINES {
        return Ok(Some("tail over 40,000 lines"));
    }
    let covered = store.float_setting(&format!("{WATERMARK_KEY}{name}"))?;
    let since = store.float_setting(&format!("{CONTINUOUS_KEY}{name}"))?;
    let (first, newest) = bt::log_span(text)?;
    let gap_free = first != 0.0 && covered != 0.0 && since != 0.0 && first <= covered;
    let mut refused = false;
    let mut bounded = false;
    let mut looked_up = 0usize;
    let mut lookup = |port: &str| {
        // One lookup per distinct port (the parser caches them); a tail naming more ports than
        // this is skipped.
        looked_up += 1;
        if looked_up > MAX_PORTS {
            bounded = true;
            return Err(());
        }
        match store.server_instances_on_port(name, port) {
            Ok(rows) => Ok(rows),
            Err(store::Read::Raised) => Err(()),
            Err(store::Read::Refused) => {
                refused = true;
                Err(())
            }
        }
    };
    let parsed = bt::parse_log(
        text,
        Some(bt::Backend {
            covered_until: if gap_free { covered } else { 0.0 },
            continuous_since: since,
            lookup: &mut lookup,
        }),
    );
    if refused {
        return Err(Stop::Refused);
    }
    if bounded {
        return Ok(Some("tail over 4,096 ports"));
    }
    let (samples, configs) = parsed?;
    let rows: Vec<&bt::Sample> = samples
        .iter()
        .filter(|s| s.gen_tokens > 0 && s.ts > 0.0)
        .collect();
    // `try: ... except Exception: continue`.
    let _ = (|| -> rusqlite::Result<()> {
        store.record_server_configs(name, &configs)?;
        store.record_timings(name, &rows)?;
        if !gap_free && first != 0.0 {
            store.set_setting(
                &format!("{CONTINUOUS_KEY}{name}"),
                &gguf::py_float_repr(first),
            )?;
        }
        if newest > covered {
            store.set_setting(
                &format!("{WATERMARK_KEY}{name}"),
                &gguf::py_float_repr(newest),
            )?;
        }
        Ok(())
    })();
    Ok(None)
}

fn f(x: f64) -> Value {
    Value::Float(x)
}

fn of(x: Option<f64>) -> Value {
    x.map_or(Value::Null, Value::Float)
}

fn n(x: i64) -> Value {
    super::analyze::int(i128::from(x))
}

fn dict(d: &bt::Dict) -> Value {
    Value::Obj(
        d.iter()
            .map(|(k, v)| (k.clone(), Value::Str(v.clone())))
            .collect(),
    )
}

fn pairs(items: Vec<(&str, Value)>) -> Value {
    Value::Obj(items.into_iter().map(|(k, v)| (k.to_owned(), v)).collect())
}

fn stats_value(s: &bt::Stats) -> Value {
    pairs(vec![
        ("n", n(s.n as i64)),
        ("gen_p50", f(s.gen_p50)),
        ("gen_p25", f(s.gen_p25)),
        ("gen_p75", f(s.gen_p75)),
        ("prompt_p50", f(s.prompt_p50)),
        ("draft_acc_p50", of(s.draft_acc_p50)),
        ("draft_len_p50", of(s.draft_len_p50)),
        ("draft_n", n(s.draft_n as i64)),
        ("last_ts", f(s.last_ts)),
        (
            "spec_types",
            Value::Arr(s.spec_types.iter().map(|t| Value::Str(t.clone())).collect()),
        ),
        ("single_config", Value::Bool(s.single_config)),
        ("config_count", n(s.config_count as i64)),
    ])
}

fn run_value(r: &bt::ConfigRun) -> Value {
    pairs(vec![
        ("instance", Value::Str(r.instance.clone())),
        ("n", n(r.n as i64)),
        ("gen_p50", f(r.gen_p50)),
        ("gen_p25", f(r.gen_p25)),
        ("gen_p75", f(r.gen_p75)),
        ("prompt_p50", f(r.prompt_p50)),
        ("draft_acc_p50", of(r.draft_acc_p50)),
        ("draft_len_p50", of(r.draft_len_p50)),
        ("last_ts", f(r.last_ts)),
        ("argv", dict(&r.argv)),
        ("is_current", Value::Bool(r.is_current)),
        ("diff", dict(&r.diff)),
        ("rel_pct", n(r.rel_pct)),
    ])
}

/// `_plain(measured)` and `_plain(history)` as the route renders them.
fn render(stats: &bt::Stats, history: &[bt::ConfigRun]) -> Measured {
    let mut measured = String::new();
    super::render::write(&mut measured, &stats_value(stats));
    let mut shown = String::new();
    super::render::write(
        &mut shown,
        &Value::Arr(history.iter().map(run_value).collect()),
    );
    Measured {
        measured,
        history: shown,
        // `float(measured.prompt_p50 or 0)`.
        prompt_tps: stats.prompt_p50,
    }
}

fn empty() -> Measured {
    render(&bt::Stats::default(), &[])
}

#[cfg(test)]
mod tests;
