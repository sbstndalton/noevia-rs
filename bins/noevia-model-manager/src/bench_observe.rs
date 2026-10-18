//! Native observations of benchmark requests (noevia#1318 step 3f).
//! Sampled usage, a separate tail-400 contention read, then tail-800 cold load timing.
//! I/O absence keeps Python's best-effort defaults. Bounds, parser disagreement and a total
//! deadline are explicit job errors: never silently ask the child under native ownership.
use crate::{
    backend_stats::{BackendStats, Logs},
    bench_runner::{Observed, Raised},
};
use std::{future::Future, pin::Pin, sync::Arc, time::Duration};
pub const LOG_CAP: usize = 8 * 1024 * 1024;
const MAX_LINES: usize = 1600;
const MAX_LINE: usize = 64 * 1024;
const DEADLINE: Duration = Duration::from_secs(10);
pub type Observer = Arc<
    dyn Fn(String) -> Pin<Box<dyn Future<Output = Result<Observed, Raised>> + Send>> + Send + Sync,
>;
fn refused(reason: &str) -> Raised {
    Raised(format!(
        "RuntimeError: native benchmark observation refused ({reason})"
    ))
}
fn checked(text: &str) -> Result<(), Raised> {
    let lines = || {
        text.split([
            '\n', '\r', '\x0b', '\x0c', '\x1c', '\x1d', '\x1e', '\u{85}', '\u{2028}', '\u{2029}',
        ])
    };
    if text.len() > LOG_CAP || lines().count() > MAX_LINES || lines().any(|l| l.len() > MAX_LINE) {
        return Err(refused("log bound"));
    }
    Ok(())
}
/// Pure contention reading; parser is the actual native counterpart of telemetry.parse_log.
pub fn contention(text: &str, aliases: &[String], since: f64) -> Result<bool, Raised> {
    checked(text)?;
    let (samples, _) = backend_telemetry::parse_log(text, None)
        .map_err(|_| refused("unsupported contention log"))?;
    Ok(samples
        .iter()
        .any(|s| s.ts >= since && !s.alias.is_empty() && !aliases.contains(&s.alias)))
}
/// Keep the earliest marker of the first completed episode, including repeated waits.
pub fn load_ms(text: &str, alias: &str) -> Result<Option<f64>, Raised> {
    checked(text)?;
    let starts = [
        format!("ensure_model: model name={alias} is not loaded"),
        format!("ensure_model: waiting until model name={alias} is fully loaded"),
        format!("to make room for name={alias}"),
    ];
    let mut start = None;
    for line in text.split([
        '\n', '\r', '\x0b', '\x0c', '\x1c', '\x1d', '\x1e', '\u{85}', '\u{2028}', '\u{2029}',
    ]) {
        if starts.iter().any(|m| line.contains(m)) {
            if start.is_none() {
                let ts = backend_telemetry::parse_ts(line)
                    .map_err(|_| refused("unsupported load timestamp"))?;
                start = (ts != 0.0).then_some(ts);
            }
        } else if start.is_some() && line.contains("llama_server: listening on http://127.0.0.1:") {
            let end = backend_telemetry::parse_ts(line)
                .map_err(|_| refused("unsupported load timestamp"))?;
            if let Some(begin) = start {
                if end != 0.0 && end >= begin {
                    return Ok(Some(model_autoconfig::pyfloat::round_nd(
                        (end - begin) * 1000.0,
                        1,
                    )));
                }
            }
            start = None;
        }
    }
    Ok(None)
}
struct Request {
    backend: String,
    alias: String,
    aliases: Vec<String>,
    since: f64,
    cold: bool,
}
fn request(query: &str) -> Result<Request, Raised> {
    if query.len() > 64 * 1024 {
        return Err(refused("query bound"));
    }
    let p: Vec<_> = form_urlencoded::parse(query.as_bytes()).collect();
    let first = |key| {
        p.iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.to_string())
            .unwrap_or_default()
    };
    let backend = first("backend");
    let alias = first("alias");
    let since: f64 = first("since").parse().map_err(|_| refused("since"))?;
    let aliases: Vec<_> = p
        .iter()
        .filter(|(k, _)| k == "aliases")
        .map(|(_, v)| v.to_string())
        .collect();
    if backend.is_empty()
        || backend.len() > 256
        || alias.len() > 1024
        || aliases.len() > 1024
        || !since.is_finite()
    {
        return Err(refused("request"));
    }
    Ok(Request {
        backend,
        alias,
        aliases,
        since,
        cold: first("cold") == "1",
    })
}
async fn read(stats: &BackendStats, r: Request) -> Result<Observed, Raised> {
    // Snapshot cached usage before any I/O, matching Python's observation order.
    let peak_vram_json = stats
        .benchmark_peak(&r.backend)
        .ok_or_else(|| refused("sample bound"))?;
    // Identity was resolved by the runner; refuse an unexpected target before any log read.
    if !stats.effective_names().await.contains(&r.backend) {
        return Err(refused("backend not configured"));
    }
    let contended = match stats.benchmark_logs(&r.backend, 400).await {
        Logs::Text(text) => {
            crate::docker::spawn_blocking_admitted(move || contention(&text, &r.aliases, r.since))
                .await
                .map_err(|_| refused("worker"))??
        }
        Logs::Skip => false,
        Logs::TooLarge => return Err(refused("log bound")),
    };
    let load_ms = if r.cold {
        match stats.benchmark_logs(&r.backend, 800).await {
            Logs::Text(text) => {
                crate::docker::spawn_blocking_admitted(move || load_ms(&text, &r.alias))
                    .await
                    .map_err(|_| refused("worker"))??
            }
            Logs::Skip => None,
            Logs::TooLarge => return Err(refused("log bound")),
        }
    } else {
        None
    };
    Ok(Observed {
        peak_vram_json,
        contended,
        load_ms,
    })
}
pub fn observer(stats: Arc<BackendStats>) -> Observer {
    Arc::new(move |query| {
        let stats = stats.clone();
        Box::pin(async move {
            let r = request(&query)?;
            tokio::time::timeout(DEADLINE, read(&stats, r))
                .await
                .map_err(|_| refused("deadline"))?
        })
    })
}
#[cfg(test)]
mod tests;
