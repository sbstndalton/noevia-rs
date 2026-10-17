//! noevia#1318: native `PUT /api/v1/settings` (the Hugging Face token).
//!
//! Python (`api.put_settings`, `db.set_setting`):
//!
//! ```python
//! if "hfToken" in body:
//!     db.set_setting("hf_token", str(body.get("hfToken") or "").strip())
//! result = get_settings()
//! if body.get("test"):
//!     try: ok, message = await hf.validate_token(db.get_setting("hf_token", ""))
//!     except Exception as e: ok, message = False, f"Test failed: {e}"
//!     result["test"] = {"ok": ok, "message": message}
//! return result
//! ```
//!
//! The write is native: one upsert into `kv` on a read-write connection that never creates the
//! store or changes its journal mode. The answer is the native settings read. The token test
//! calls Hugging Face, which this side does not do yet, so it is asked of the Python child's
//! private `GET /api/v1/internal/hf-token-test` (which reads the stored token itself) and placed
//! verbatim. The token never appears in an answer or a log.
use crate::prompts::JsonValue as Json;
use rusqlite::{Connection, OpenFlags};
use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

/// The child's private token test.
pub const TEST_PATH: &str = "/api/v1/internal/hf-token-test";
/// What the test answers when the child cannot run it (the token is already saved).
pub const TEST_UNAVAILABLE: &str =
    r#"{"ok":false,"message":"Test failed: the token test is unavailable"}"#;
const BUSY_WAIT: Duration = Duration::from_millis(4000);
const DEADLINE: Duration = Duration::from_millis(4500);

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The settings answer (without `test`), and whether the body asked for the test.
    Saved(Vec<u8>, bool),
    Json(u16, Vec<u8>),
    Failed,
}

pub struct SettingsWrite {
    root: PathBuf,
    lock: tokio::sync::Mutex<()>,
}

/// The new token when the body names one: `str(body.get("hfToken") or "").strip()`.
pub(crate) fn token(body: &Json) -> Result<Option<String>, ()> {
    let Json::Obj(pairs) = body else {
        return Err(());
    };
    if !pairs.iter().any(|(k, _)| k == "hfToken") {
        return Ok(None);
    }
    let raw = crate::prompts::py_str(body.get("hfToken")).map_err(|_| ())?;
    Ok(Some(model_autoconfig::pyval::py_strip(&raw).to_owned()))
}

fn write(database: &Path, value: &str, deadline: Instant) -> rusqlite::Result<()> {
    let db = Connection::open_with_flags(
        database,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    db.set_limit(
        rusqlite::limits::Limit::SQLITE_LIMIT_SQL_LENGTH,
        crate::sqlite_read::SQL_CAP,
    )?;
    db.busy_timeout(BUSY_WAIT)?;
    db.progress_handler(1000, Some(move || Instant::now() >= deadline))?;
    db.execute(
        "INSERT INTO kv(key, value) VALUES(?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        ["hf_token", value],
    )?;
    Ok(())
}

impl SettingsWrite {
    pub fn new(root: &Path) -> Self {
        Self {
            root: root.to_owned(),
            lock: tokio::sync::Mutex::new(()),
        }
    }

    pub async fn put(&self, body: &Json) -> Outcome {
        let deadline = Instant::now() + DEADLINE;
        let Ok(new) = token(body) else {
            return Outcome::Failed;
        };
        let test = body.get("test").is_some_and(Json::truthy);
        if let Some(value) = &new {
            let cap = usize::try_from(crate::settings::VALUE_CAP).unwrap_or(0);
            if value.len() > cap {
                // The native read refuses values over this; never store one it cannot read.
                return Outcome::Json(
                    400,
                    format!("{{\"detail\":\"hfToken exceeds {cap} bytes\"}}").into_bytes(),
                );
            }
        }
        let Ok(_guard) = tokio::time::timeout_at(deadline.into(), self.lock.lock()).await else {
            return Outcome::Json(503, br#"{"detail":"service is busy"}"#.to_vec());
        };
        let database = self.root.join("model_loader.db");
        let job = move || -> Result<Vec<u8>, ()> {
            if let Some(value) = new {
                write(&database, &value, deadline).map_err(|e| {
                    crate::sqlite_read::log_once("settings_write", &e);
                })?;
            }
            let stored = crate::settings::read(&database, deadline).map_err(|_| ())?;
            crate::settings::render(stored.as_ref()).ok_or(())
        };
        // noevia#2006: admitted: a dropped request keeps its permit until the write returns.
        match crate::docker::spawn_blocking_admitted(job).await {
            Ok(Ok(bytes)) => Outcome::Saved(bytes, test),
            _ => Outcome::Failed,
        }
    }
}

/// The settings answer with the child's `{"ok","message"}` placed as `test`.
pub fn with_test(settings: &[u8], test: &[u8]) -> Vec<u8> {
    let Some((_, open)) = settings.split_last() else {
        return settings.to_vec();
    };
    let mut out = open.to_vec();
    out.extend_from_slice(b",\"test\":");
    out.extend_from_slice(test);
    out.push(b'}');
    out
}

/// The child's test answer, if it is exactly `{"ok": bool, "message": str}`.
pub fn valid_test(bytes: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return false;
    };
    match model_files::json::parse(text) {
        Ok(Json::Obj(pairs)) => {
            pairs.len() == 2
                && matches!(pairs.first(), Some((k, Json::Bool(_))) if k == "ok")
                && matches!(pairs.get(1), Some((k, Json::Str(_))) if k == "message")
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn json(t: &str) -> Json {
        crate::prompts::parse_json(t).unwrap()
    }

    #[test]
    fn token_forms() {
        assert_eq!(token(&json(r#"{"test":true}"#)), Ok(None));
        assert_eq!(
            token(&json(r#"{"hfToken":"  hf_x  "}"#)),
            Ok(Some("hf_x".into()))
        );
        assert_eq!(token(&json(r#"{"hfToken":null}"#)), Ok(Some(String::new())));
        assert_eq!(token(&json(r#"{"hfToken":12}"#)), Ok(Some("12".into())));
    }

    #[test]
    fn test_answers_are_checked_and_spliced() {
        assert!(valid_test(br#"{"ok":true,"message":"authenticated as x"}"#));
        assert!(!valid_test(br#"{"ok":1,"message":"x"}"#));
        assert!(!valid_test(b"nope"));
        assert_eq!(
            with_test(
                br#"{"hasToken":true,"tokenHint":""}"#,
                br#"{"ok":true,"message":"m"}"#
            ),
            br#"{"hasToken":true,"tokenHint":"","test":{"ok":true,"message":"m"}}"#.to_vec()
        );
    }

    /// noevia#2006: a save dropped while it writes (the client went away) keeps the request's
    /// admission permit until the write has returned.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_dropped_save_keeps_its_permit_until_the_write_ends() {
        use crate::test_support::{all_permits_back, dropped_in_blocking_work, test_state};
        let dir = tempfile::tempdir().unwrap();
        let mut state = test_state(crate::LEGACY_BASE);
        state.settings_write = Some(std::sync::Arc::new(SettingsWrite::new(dir.path())));
        let state = std::sync::Arc::new(state);
        let request = axum::http::Request::put("/api/v1/settings")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(r#"{"hfToken":"synthetic-token"}"#))
            .unwrap();
        let open = dropped_in_blocking_work(&state, request).await;
        open.send(()).unwrap();
        all_permits_back(&state).await;
    }
}
