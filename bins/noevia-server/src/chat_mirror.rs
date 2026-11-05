//! The chat change-journal consumer and the Diary mirror / chat brain schedules and their work
//! (sbstndalton/noevia#2153, #741, #742), moved from noevia-core `chat-change-journal.cjs`,
//! `chat-vault-mirror.cjs` and `chat-brain.cjs`; capability `rust-chat-mirror-v1`, executors
//! `rust-chat-mirror-exec-v1`.
//!
//! This process decides *when*: it reads each active account's journal in-process
//! ([`server_projects::journal`]), debounces and coalesces the mirror passes, times the per-chat
//! idle builds of the brains, and retries with backoff (crate `chat-mirror-sched`). It also does
//! the work natively (crate `chat-mirror-exec`, wired in [`crate::chat_mirror_work`]): the Diary
//! file writes, the note rendering, the per-user index, the tombstone trash pass, the brain store
//! and the reasoner call. Only the engine admission and the provider call of the reasoner are still
//! Node's, behind the [`crate::chat_mirror_work::EngineSeam`] trait (see there).
//!
//! Node's own triggers (a project change, an import, its request starts) reach this schedule on
//! the project store's loopback listener with the same secret:
//!
//!   POST /v1/chat-mirror/schedule {user, chatId|null, immediate?}   POST /v1/chat-mirror/touch {user}
//!
//! The front's activity record (`UI_DATA_DIR/chat-activity.json`, written by the front when it
//! answers chat routes itself) is merged on every tick, as before.
use chat_mirror_sched::brain::{self, Brains};
use chat_mirror_sched::journal::{
    self, Activity, Change, ChangeSink, Consumer, JournalRead, JournalSource, ReadError,
};
use chat_mirror_sched::mirror::{self, Mirror};
use chat_mirror_sched::{BoxFut, BrainExecutor, Log, MirrorExecutor, StderrLog};
use serde_json::Value;
use server_projects::journal as store_journal;
use server_projects::store::StoreError;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

/// The front's record of authenticated requests.
pub const ACTIVITY_FILE: &str = "chat-activity.json";

/// The schedule's clocks (the defaults are core's; tests use short ones).
#[derive(Debug, Clone, Copy)]
pub struct Options {
    pub mirror: mirror::Timings,
    pub brain: brain::Timings,
    /// How often active users' journals are looked at.
    pub interval: Duration,
    /// How long after a user's last request their journal keeps being read.
    pub active: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            mirror: mirror::Timings::default(),
            brain: brain::Timings::default(),
            interval: journal::INTERVAL,
            active: journal::ACTIVE,
        }
    }
}

pub struct ChatMirror {
    consumer: Consumer,
    mirror: Mirror,
}

impl ChatMirror {
    /// Builds the schedule over the native executors and starts its timer. `None` without a data
    /// directory.
    pub fn start(app: &crate::App) -> Option<Arc<ChatMirror>> {
        let data_dir = app.config.data_dir.clone()?;
        let host = Arc::new(crate::chat_mirror_work::AppHost {
            data_dir: data_dir.clone(),
            identity: Arc::clone(&app.identity),
            feature_context: app.feature_context.clone(),
            diary: app.diary.clone(),
            engine: Arc::new(crate::chat_mirror_work::NodeEngine {
                research: Arc::clone(&app.config.research),
            }),
        });
        Self::start_with(
            crate::chat_mirror_work::NativeWork::new(host),
            Some(data_dir),
            Options::default(),
        )
    }

    /// The schedule over `work` (tests give a recording one).
    pub fn start_with<W: MirrorExecutor + BrainExecutor>(
        work: Arc<W>,
        data_dir: Option<PathBuf>,
        options: Options,
    ) -> Option<Arc<ChatMirror>> {
        let data_dir = data_dir?;
        let log: Arc<dyn Log> = Arc::new(StderrLog);
        let mirror = Mirror::new(work.clone(), Arc::clone(&log), options.mirror);
        let built_mirror = mirror.clone();
        let brains = Brains::new(
            work,
            Arc::new(move |user: &str, chat: &str| built_mirror.schedule(user, Some(chat), false)),
            Arc::clone(&log),
            options.brain,
        );
        let sink = Arc::new(Sink {
            mirror: mirror.clone(),
            brains,
        });
        let source = Arc::new(Files {
            data_dir: data_dir.clone(),
        });
        let activity = ActivityFile::new(data_dir.join(ACTIVITY_FILE));
        let activity: Activity = Arc::new(move || activity.read());
        let consumer = Consumer::new(
            source,
            sink,
            log,
            Some(activity),
            journal::system_clock(),
            options.interval,
            options.active,
        );
        consumer.start();
        Some(Arc::new(ChatMirror { consumer, mirror }))
    }

    /// A save of the mirror preferences that turns the mirror on runs that account's pass at once.
    /// Installed once per process, by the server.
    pub fn install_nudge_hook(&self) {
        let mirror = self.mirror.clone();
        server_account::vault_mirror::set_nudge_hook(Box::new(move |_dir, user| {
            mirror.schedule(user, None, true);
        }));
    }

    /// A request from `user` started (Node's, reported over the store): read their journal now.
    pub fn touch(&self, user: &str) {
        self.consumer.touch(user);
    }

    /// Node asks for `user`'s mirror to look again (a project change, an import).
    pub fn schedule(&self, user: &str, chat: Option<&str>, immediate: bool) {
        self.mirror.schedule(user, chat, immediate);
    }
}

struct Sink {
    mirror: Mirror,
    brains: Brains,
}

impl ChangeSink for Sink {
    fn changed(&self, user: &str, chat: &str, change: &Change) {
        self.mirror.schedule(user, Some(chat), false);
        if change.brain {
            self.brains
                .schedule(user, change.brain_chat.as_deref().unwrap_or(chat));
        }
    }
    fn rescanned(&self, user: &str) {
        self.mirror.schedule(user, None, false);
    }
}

/// The journals as the store writes them.
struct Files {
    data_dir: PathBuf,
}

impl JournalSource for Files {
    fn read(&self, user: &str, after: u64) -> BoxFut<Result<JournalRead, ReadError>> {
        let dir = self.data_dir.clone();
        let user = user.to_string();
        Box::pin(async move {
            let done =
                tokio::task::spawn_blocking(move || store_journal::read(&dir, &user, after)).await;
            match done {
                Ok(Ok(c)) => Ok(JournalRead {
                    entries: c.entries,
                    oldest_seq: c.oldest_seq,
                    latest_seq: c.latest_seq,
                    more: c.more,
                    start: store_journal::start_id().to_string(),
                }),
                Ok(Err(StoreError::Gone)) => Err(ReadError::Gone),
                Ok(Err(e)) => Err(ReadError::Failed(format!("{e:?}"))),
                Err(_) => Err(ReadError::Failed("journal read stopped".into())),
            }
        })
    }

    fn stamp(&self, user: &str) -> Option<String> {
        file_stamp(&self.data_dir, user)
    }
}

/// `users/<user>/chat-changes.jsonl`'s stamp from one stat: size, mtime and inode (a rotation
/// renames a new file in). `None`: not a user id. `absent`: no such file yet.
pub fn file_stamp(data_dir: &Path, user: &str) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    let ok = user.len() == 36 && user.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-');
    if !ok {
        return None;
    }
    let path = data_dir.join("users").join(user).join(store_journal::FILE);
    Some(match std::fs::metadata(path) {
        Ok(m) => format!("{}:{}.{}:{}", m.len(), m.mtime(), m.mtime_nsec(), m.ino()),
        Err(_) => "absent".to_string(),
    })
}

/// `{ [userId]: epochMs }` from the front's activity file, re-read only when its signal changed.
/// `None` while it cannot be read (logged once per run of failures).
struct ActivityFile {
    file: PathBuf,
    cache: Mutex<ActivityCache>,
}

/// The file's signal (size, mtime) at the last good read, what it held (`None`: unreadable), and
/// whether the failure was already logged.
type ActivityCache = (
    Option<(u64, SystemTime)>,
    Option<HashMap<String, f64>>,
    bool,
);

impl ActivityFile {
    fn new(file: PathBuf) -> Arc<Self> {
        Arc::new(ActivityFile {
            file,
            cache: Mutex::new((None, Some(HashMap::new()), false)),
        })
    }

    fn read(&self) -> Option<HashMap<String, f64>> {
        let signal = std::fs::metadata(&self.file)
            .ok()
            .and_then(|m| m.modified().ok().map(|t| (m.len(), t)));
        let mut cache = match self.cache.lock() {
            Ok(c) => c,
            Err(p) => p.into_inner(),
        };
        if signal.is_some() && cache.0 == signal && cache.1.is_some() {
            return cache.1.clone();
        }
        let loaded = parse_activity(&self.file);
        match loaded {
            Ok(map) => {
                *cache = (signal, Some(map.clone()), false);
                Some(map)
            }
            Err(()) => {
                if !cache.2 {
                    eprintln!(
                        "noevia-server: chat-activity: the front's activity file could not be read"
                    );
                }
                *cache = (None, None, true);
                None
            }
        }
    }
}

/// `{}` when the file is missing; an error when it exists and cannot be read or parsed.
fn parse_activity(file: &Path) -> Result<HashMap<String, f64>, ()> {
    let text = match std::fs::read_to_string(file) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(HashMap::new()),
        Err(_) => return Err(()),
    };
    let doc: Value = serde_json::from_str(&text).map_err(|_| ())?;
    let mut out = HashMap::new();
    if doc.get("version").and_then(Value::as_u64) == Some(1) {
        if let Some(seen) = doc.get("seen").and_then(Value::as_object) {
            for (user, at) in seen {
                if let Some(at) = at.as_f64().filter(|a| a.is_finite()) {
                    if !user.is_empty() {
                        out.insert(user.clone(), at);
                    }
                }
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    const U: &str = "11111111-2222-4333-8444-555555555555";

    #[test]
    fn the_stamp_changes_with_the_journal_and_refuses_a_path_that_is_not_a_user_id() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("users").join(U);
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(file_stamp(root.path(), "../etc"), None);
        assert_eq!(file_stamp(root.path(), "x"), None);
        assert_eq!(file_stamp(root.path(), U).as_deref(), Some("absent"));
        std::fs::write(dir.join(store_journal::FILE), "a\n").unwrap();
        let one = file_stamp(root.path(), U).unwrap();
        assert_ne!(one, "absent");
        std::fs::write(dir.join(store_journal::FILE), "a\nb\n").unwrap();
        assert_ne!(file_stamp(root.path(), U).unwrap(), one);
    }

    #[test]
    fn the_activity_file_missing_is_nobody_malformed_is_unreadable_and_a_bad_entry_is_skipped() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join(ACTIVITY_FILE);
        let source = ActivityFile::new(file.clone());
        assert_eq!(source.read(), Some(HashMap::new()));
        std::fs::write(
            &file,
            r#"{"version":1,"seen":{"a":5,"b":"x","":3,"c":7.5}}"#,
        )
        .unwrap();
        assert_eq!(
            source.read(),
            Some(HashMap::from([
                ("a".to_string(), 5.0),
                ("c".to_string(), 7.5)
            ]))
        );
        std::fs::write(&file, "not json").unwrap();
        assert_eq!(source.read(), None);
        std::fs::write(&file, r#"{"version":2,"seen":{"a":5}}"#).unwrap();
        assert_eq!(
            source.read(),
            Some(HashMap::new()),
            "an unknown version is nobody"
        );
    }
}
