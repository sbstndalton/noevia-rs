//! Swap and lifecycle records: appended to `router-events.jsonl` and kept (last 50) for
//! `GET /router/status`.

use serde_json::{Map, Value};
use std::collections::VecDeque;
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

const KEEP: usize = 50;

/// `YYYY-MM-DDTHH:MM:SSZ` for the current time (UTC).
pub fn utc_now() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = i64::try_from(secs / 86_400).unwrap_or(0);
    let rem = secs % 86_400;
    // Civil date from days since 1970-01-01 (proleptic Gregorian).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

pub struct Events {
    file: Mutex<Option<std::fs::File>>,
    history: Mutex<VecDeque<Value>>,
}

impl Events {
    pub fn open(logdir: &Path) -> std::io::Result<Events> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(logdir.join("router-events.jsonl"))?;
        Ok(Events {
            file: Mutex::new(Some(file)),
            history: Mutex::new(VecDeque::new()),
        })
    }

    pub fn emit(&self, kind: &str, fields: Map<String, Value>) {
        let mut record = Map::new();
        record.insert("t".into(), Value::String(utc_now()));
        record.insert("kind".into(), Value::String(kind.into()));
        record.extend(fields);
        let record = Value::Object(record);
        tracing::info!(target: "noevia_router", "{record}");
        if let Some(file) = self.file.lock().unwrap_or_else(|p| p.into_inner()).as_mut() {
            let _ = writeln!(file, "{record}");
        }
        let mut history = self.history.lock().unwrap_or_else(|p| p.into_inner());
        history.push_back(record);
        while history.len() > KEEP {
            history.pop_front();
        }
    }

    pub fn recent(&self, n: usize) -> Vec<Value> {
        let history = self.history.lock().unwrap_or_else(|p| p.into_inner());
        history
            .iter()
            .skip(history.len().saturating_sub(n))
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn utc_shape() {
        let t = super::utc_now();
        assert_eq!(t.len(), 20);
        assert!(t.starts_with("20") && t.ends_with('Z'), "{t}");
    }
}
