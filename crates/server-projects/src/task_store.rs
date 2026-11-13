//! The writer half of core server/jobs.cjs `createJobs`, for one workspace's `jobs/` directory
//! (`<dir>/jobs/<uuid>.jsonl`): create, append, get, cancel, recover and prune, writing the bytes
//! jobs.cjs writes (sbstndalton/noevia-rs R7, step 1). The reads and the fold are
//! [`crate::task_journals`]'s, the same ones the front's task reads use, so what this store writes
//! is what those reads see. Nothing calls it yet: Node stays the only writer until the Code task
//! executor switch moves a workspace's Code tasks here.
//!
//! **One writer.** jobs.cjs assumes it is the only process writing a journal, and so does this
//! store: a host keeps one [`Store`] per directory, and Node must not write the same kinds there
//! while it does. Within the process every write takes the store's lock, so two threads never
//! interleave a read and its append.
//!
//! **Bytes.** Each event is one line, `JSON.stringify({job, seq, type, at, data})` plus `\n`;
//! a journal whose first row carries `hash` (or every journal of a `durable` store's kinds) is
//! hash-chained: `previous` is the last row's hash, `hash` the sha256 of the row's JSON before
//! it, and the file and the directory are fsynced. `data` is a [`JValue`] in the
//! [`js_json::wtf16`] form (a lone surrogate held as its mapped character), the form the reader
//! produces, so a value read back and written again keeps its bytes.
//!
//! **Fixed on purpose against Node** (each tested in tests/task_store.rs):
//! 1. A last line that parses but lost only its newline is kept and given one before the next
//!    row. jobs.cjs cut it off after counting it, so the next row skipped a seq and, in a chained
//!    journal, named a `previous` that was no longer there: the journal then read as damaged.
//! 2. A last line that does not parse but ends in a newline is damage, not a torn write: nothing is
//!    written after it (409, unreadable), so it stays the journal's last line for review. jobs.cjs
//!    wrote after it, which made the journal unreadable only after a restart.
//! 3. `job.created` is written only by [`Store::create`]: jobs.cjs's `append` took a second one
//!    on a live job, which would replace its kind and capabilities.
//! 4. Event `data` must be an object (or absent, which is `{}` as in Node). It is written in JS
//!    property order whatever order the caller built it in.
//! 5. A journal that is a symbolic link, not a regular file, hard-linked, or over
//!    [`crate::task_journals::MAX_JOURNAL_BYTES`] is unreadable (409), never followed or loaded,
//!    and no row is written that would take it over that size (409, "The job journal is full");
//!    a line that `JSON.parse` reads but this port cannot hold (a number past the `f64` range, a
//!    character of U+10F800..U+10FFFF) refuses the write rather than being dropped.
//! 6. Rows that are not objects make the journal unreadable (the reader's rule).
//! 7. Prune removes the finished journal by its own file name; jobs.cjs removed the file of the id
//!    its `job.created` row names (another job's, or a 400 when it has none). A finished job whose
//!    time is not a number sorts last (V8 leaves that order unspecified).
//!
//! Recover and cancel ask the host's `live`/`abort` without the store's lock.

use crate::task_journals::{self as journals, Job};
use js_json::{wtf16, JValue};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// jobs.cjs `TYPES`.
pub const TYPES: [&str; 25] = [
    "job.created",
    "job.started",
    "step.started",
    "step.completed",
    "progress",
    "approval.requested",
    "approval.decided",
    "tool.started",
    "tool.completed",
    "tool.uncertain",
    "artifact.created",
    "checkpoint.created",
    "job.completed",
    "job.failed",
    "job.cancelled",
    "job.interrupted",
    "plan.proposed",
    "plan.edited",
    "plan.skipped",
    "assistant.output",
    "review.requested",
    "review.completed",
    "review.failed",
    "task.stage",
    "task.revision",
];
/// The lifecycle authority events (#701): only the pipeline writes them, and this store has no
/// path for that yet, so every append of one is refused as jobs.cjs refuses one without the token.
pub const AUTHORITY_TYPES: [&str; 2] = ["task.stage", "task.revision"];
const REVIEW_TYPES: [&str; 3] = ["review.requested", "review.completed", "review.failed"];
const MAX_ASSISTANT_OUTPUT_EVENT_BYTES: usize = 1024;
const MAX_ASSISTANT_OUTPUT_EVENTS: usize = 64;
const TERMINAL: [&str; 4] = ["completed", "failed", "cancelled", "interrupted"];
const WAITING_REASON: &str =
    "The server restarted while waiting for approval; start again to be asked again.";
const RESTART_REASON: &str = "The server restarted before this finished.";
const UNREADABLE: &str = "Unreadable job journal; review required";

/// A refused operation: jobs.cjs's `Error`, with its `status` (`None`: an error without one,
/// which a route answers 500) and message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub status: Option<u16>,
    pub message: String,
}

impl Refusal {
    fn new(status: Option<u16>, message: impl Into<String>) -> Self {
        Refusal {
            status,
            message: message.into(),
        }
    }

    /// The HTTP status a route answers with.
    pub fn http_status(&self) -> u16 {
        self.status.unwrap_or(500)
    }

    /// The journal could not be read exactly (jobs.cjs `readJournal`'s 409).
    pub fn is_unreadable(&self) -> bool {
        self.status == Some(409) && self.message == UNREADABLE
    }
}

fn unreadable() -> Refusal {
    Refusal::new(Some(409), UNREADABLE)
}

fn io(error: &std::io::Error) -> Refusal {
    Refusal::new(None, format!("Job journal write failed: {}", error.kind()))
}

/// jobs.cjs `createJobs` options. code-service.cjs uses `kinds: ['code']`, 50 jobs, 14 days.
#[derive(Debug, Clone, PartialEq)]
pub struct Options {
    /// The kinds this store prunes, recovers and (when `durable`) chains; `None` is every kind.
    pub kinds: Option<Vec<String>>,
    pub durable: bool,
    pub retain_ms: f64,
    pub max_jobs: usize,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            kinds: None,
            durable: false,
            retain_ms: 7.0 * 86_400_000.0,
            max_jobs: 200,
        }
    }
}

/// What [`Store::create`] makes a job with (jobs.cjs `create`'s argument).
#[derive(Debug, Clone, Copy)]
pub struct NewJob<'a> {
    pub kind: &'a str,
    pub project_id: Option<&'a str>,
    pub parent_id: Option<&'a str>,
    pub capabilities: &'a [String],
}

type Clock = Box<dyn Fn() -> f64 + Send + Sync>;
type Ids = Box<dyn Fn() -> String + Send + Sync>;

/// One workspace's job store.
pub struct Store {
    root: PathBuf,
    options: Options,
    now: Clock,
    new_id: Ids,
    lock: Mutex<()>,
}

/// The milliseconds since the epoch, as `Date.now()`.
pub fn date_now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_millis() as f64)
}

impl Store {
    /// The store over `<dir>/jobs`, with the system clock and random v4 ids.
    pub fn new(dir: &Path, options: Options) -> Store {
        Store::with(
            dir,
            options,
            Box::new(date_now),
            Box::new(crate::source_jobs::new_id),
        )
    }

    /// The store with the host's clock (`Date.now()` milliseconds) and id source.
    pub fn with(dir: &Path, options: Options, now: Clock, new_id: Ids) -> Store {
        Store {
            root: dir.join("jobs"),
            options,
            now,
            new_id,
            lock: Mutex::new(()),
        }
    }

    fn guard(&self) -> std::sync::MutexGuard<'_, ()> {
        // A panic elsewhere while holding the lock leaves nothing half done here: every write is
        // one line, so the lock is taken again rather than poisoning the store.
        self.lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn file(&self, id: &str) -> Result<PathBuf, Refusal> {
        if !journals::valid_id(id) {
            return Err(Refusal::new(Some(400), "Invalid job id"));
        }
        Ok(self.root.join(format!("{id}.jsonl")))
    }

    /// `durable && (!kinds || kinds.includes(rows[0]?.data?.kind))`.
    fn chains_by_policy(&self, first: Option<&JValue>) -> bool {
        self.options.durable
            && match &self.options.kinds {
                None => true,
                Some(kinds) => first
                    .and_then(|row| row.get("data").get("kind").as_str())
                    .is_some_and(|kind| kinds.iter().any(|k| k == kind)),
            }
    }

    /// jobs.cjs `events(id)`: the journal's rows, `[]` when there is no file.
    fn read(&self, id: &str) -> Result<Journal, Refusal> {
        let path = self.file(id)?;
        let raw = match read_plain(&path) {
            Ok(Some(raw)) => raw,
            Ok(None) => return Ok(Journal::default()),
            Err(()) => return Err(unreadable()),
        };
        let rows = journals::parse_rows_with(id, &raw, &|first| self.chains_by_policy(first))
            .ok_or_else(unreadable)?;
        // What the end needs before a write; a refusal here stops writes only, never reads.
        let tail = tail_of(&raw, rows.len());
        Ok(Journal { rows, tail })
    }

    /// jobs.cjs `get`: the derived job, `None` when there is no journal or no event in it.
    pub fn get(&self, id: &str) -> Result<Option<Job>, Refusal> {
        let _held = self.guard();
        self.get_unlocked(id)
    }

    fn get_unlocked(&self, id: &str) -> Result<Option<Job>, Refusal> {
        let journal = self.read(id)?;
        Ok((!journal.rows.is_empty()).then(|| journals::derive(&journal.rows)))
    }

    /// jobs.cjs `append(id, type, data)` without the lifecycle authority: the event written.
    /// `data` of [`JValue::Undefined`] is `{}`.
    pub fn append(&self, id: &str, kind: &str, data: JValue) -> Result<JValue, Refusal> {
        let _held = self.guard();
        self.append_unlocked(id, kind, data, false)
    }

    fn append_unlocked(
        &self,
        id: &str,
        kind: &str,
        data: JValue,
        creating: bool,
    ) -> Result<JValue, Refusal> {
        if !TYPES.contains(&kind) {
            return Err(Refusal::new(
                None,
                format!("Unknown job event type: {kind}"),
            ));
        }
        if AUTHORITY_TYPES.contains(&kind) {
            return Err(Refusal::new(
                Some(403),
                "Task lifecycle events are server-only",
            ));
        }
        let journal = self.read(id)?;
        let current = &journal.rows;
        if kind == "job.created" && !creating {
            return Err(Refusal::new(None, "A job is created with create()"));
        }
        if creating && !current.is_empty() {
            return Err(Refusal::new(Some(409), "That job id is already in use"));
        }
        if current.is_empty() && kind != "job.created" {
            return Err(Refusal::new(Some(404), "No such job"));
        }
        let finished = (!current.is_empty()).then(|| journals::derive(current));
        let status = finished.as_ref().and_then(|j| j.status.as_str());
        let late_artifact = kind == "artifact.created" && status == Some("cancelled");
        if status.is_some_and(|s| TERMINAL.contains(&s)) && !late_artifact {
            return Err(Refusal::new(Some(409), "Job already finished"));
        }
        let mut data = match data {
            JValue::Undefined => JValue::Obj(Vec::new()),
            JValue::Obj(items) => js_object(JValue::Obj(items), 0)?,
            _ => return Err(Refusal::new(None, "Job event data is an object")),
        };
        let code = finished
            .as_ref()
            .is_some_and(|j| j.kind.as_str() == Some("code"));
        if kind == "assistant.output" {
            if !code {
                return Err(Refusal::new(None, "Assistant output belongs to Code jobs"));
            }
            let count = current
                .iter()
                .filter(|r| r.get("type").as_str() == Some(kind))
                .count();
            if count >= MAX_ASSISTANT_OUTPUT_EVENTS {
                return Err(Refusal::new(None, "Assistant output event limit reached"));
            }
            let incoming = data.get("text").as_str().unwrap_or("");
            let (text, cut) = journals::clip_utf8(incoming, MAX_ASSISTANT_OUTPUT_EVENT_BYTES);
            let truncated = *data.get("truncated") == JValue::Bool(true) || cut;
            data = JValue::obj([
                ("text", JValue::Str(text)),
                ("truncated", JValue::Bool(truncated)),
            ]);
        }
        if REVIEW_TYPES.contains(&kind) {
            if !code {
                return Err(Refusal::new(None, "Reviews belong to Code jobs"));
            }
            data = journals::bound_review(kind, &data);
        }
        if code && matches!(kind, "plan.proposed" | "plan.edited" | "plan.skipped") {
            data = if kind == "plan.skipped" {
                JValue::obj([
                    ("question", JValue::Null),
                    ("subQuestions", JValue::Arr(Vec::new())),
                    ("truncated", JValue::Bool(false)),
                ])
            } else {
                match journals::bound_code_plan("", &data) {
                    JValue::Obj(items) => {
                        JValue::Obj(items.into_iter().filter(|(k, _)| k != "status").collect())
                    }
                    other => other,
                }
            };
        }
        let mut event = vec![
            ("job".to_string(), JValue::Str(id.to_string())),
            ("seq".to_string(), JValue::Num((current.len() + 1) as f64)),
            ("type".to_string(), JValue::Str(kind.to_string())),
            ("at".to_string(), JValue::Num((self.now)())),
            ("data".to_string(), data),
        ];
        let sync = self.options.durable || current.first().is_some_and(|r| r.get("hash").truthy());
        if sync {
            let last = current.last().map(|r| r.get("hash").clone());
            let previous = last.filter(JValue::truthy).unwrap_or(JValue::Null);
            event.push(("previous".to_string(), previous));
            let payload = wtf16::stringify(&JValue::Obj(event.clone()))
                .ok_or_else(|| Refusal::new(None, "Job event cannot be written"))?;
            event.push(("hash".to_string(), JValue::Str(sha256_hex(&payload))));
        }
        let event = JValue::Obj(event);
        let mut line = wtf16::stringify(&event)
            .ok_or_else(|| Refusal::new(None, "Job event cannot be written"))?;
        line.push('\n');
        let tail = journal.tail.clone()?;
        self.write_line(id, &tail, &line, sync)?;
        Ok(event)
    }

    fn write_line(&self, id: &str, tail: &Tail, line: &str, sync: bool) -> Result<(), Refusal> {
        let path = self.file(id)?;
        make_dir(&self.root).map_err(|e| io(&e))?;
        let mut file = open_append(&path).map_err(|e| io(&e))?;
        let meta = file.metadata().map_err(|e| io(&e))?;
        if !meta.file_type().is_file() || links(&meta) > 1 {
            return Err(unreadable());
        }
        let mut bytes = Vec::with_capacity(line.len() + 1);
        let kept = match tail {
            Tail::Whole => meta.len(),
            Tail::NoNewline => {
                bytes.push(b'\n');
                meta.len()
            }
            Tail::Cut(at) => *at,
        };
        bytes.extend_from_slice(line.as_bytes());
        // A row that would take the journal past what any reader loads would make it unreadable.
        if kept.saturating_add(bytes.len() as u64) > journals::MAX_JOURNAL_BYTES {
            return Err(Refusal::new(Some(409), "The job journal is full"));
        }
        if let Tail::Cut(at) = tail {
            file.set_len(*at).map_err(|e| io(&e))?;
        }
        file.write_all(&bytes).map_err(|e| io(&e))?;
        if sync {
            file.sync_all().map_err(|e| io(&e))?;
            fs::File::open(&self.root)
                .and_then(|d| d.sync_all())
                .map_err(|e| io(&e))?;
        }
        Ok(())
    }

    /// jobs.cjs `create`: the new job's id.
    pub fn create(&self, job: NewJob<'_>) -> Result<String, Refusal> {
        let _held = self.guard();
        if job.kind.is_empty() {
            return Err(Refusal::new(None, "Job kind required"));
        }
        if let Some(parent_id) = job.parent_id.filter(|p| !p.is_empty()) {
            let parent = self
                .get_unlocked(parent_id)?
                .ok_or_else(|| Refusal::new(Some(404), "No such parent job"))?;
            let held: Vec<&str> = match &parent.capabilities {
                JValue::Arr(items) => items.iter().filter_map(JValue::as_str).collect(),
                _ => Vec::new(),
            };
            let extra: Vec<&str> = job
                .capabilities
                .iter()
                .map(String::as_str)
                .filter(|c| !held.contains(c))
                .collect();
            if !extra.is_empty() {
                return Err(Refusal::new(
                    Some(403),
                    format!(
                        "A child job cannot widen capabilities: {}",
                        extra.join(", ")
                    ),
                ));
            }
        }
        self.prune_unlocked()?;
        let id = (self.new_id)();
        let mut unique: Vec<JValue> = Vec::new();
        for cap in job.capabilities {
            let cap = JValue::Str(cap.clone());
            if !unique.contains(&cap) {
                unique.push(cap);
            }
        }
        let text = |v: Option<&str>| v.map_or(JValue::Null, |s| JValue::Str(s.to_string()));
        let data = JValue::obj([
            ("kind", JValue::Str(job.kind.to_string())),
            ("projectId", text(job.project_id)),
            ("parentId", text(job.parent_id)),
            ("capabilities", JValue::Arr(unique)),
        ]);
        self.append_unlocked(&id, "job.created", data, true)?;
        Ok(id)
    }

    /// jobs.cjs `cancel`: `None` when there is no such job; a finished job as it is. Otherwise
    /// `abort(id)` is asked to stop the host's live run of it (true: there was one, and it records
    /// the cancel itself) and, when there is none, `job.cancelled` is written here. `abort` only
    /// signals; it is called without the store's lock.
    pub fn cancel(&self, id: &str, abort: &dyn Fn(&str) -> bool) -> Result<Option<Job>, Refusal> {
        {
            let _held = self.guard();
            let Some(job) = self.get_unlocked(id)? else {
                return Ok(None);
            };
            if job.status.as_str().is_some_and(|s| TERMINAL.contains(&s)) {
                return Ok(Some(job));
            }
        }
        // Asked without the lock: the run being stopped writes its own `job.cancelled` through
        // this store, and may do so before `abort` returns.
        let aborted = abort(id);
        let _held = self.guard();
        if !aborted {
            match self.append_unlocked(id, "job.cancelled", JValue::Undefined, false) {
                // It finished in between: it stays as it ended.
                Err(e) if e.status == Some(409) && e.message == "Job already finished" => {}
                other => {
                    other?;
                }
            }
        }
        self.get_unlocked(id)
    }

    /// jobs.cjs `ids`: the names in the directory that are job journals.
    pub fn ids(&self) -> Vec<String> {
        let Ok(entries) = fs::read_dir(&self.root) else {
            return Vec::new();
        };
        let mut ids: Vec<String> = entries
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().into_string().ok()?;
                let id = name.strip_suffix(".jsonl")?;
                journals::valid_id(id).then(|| id.to_string())
            })
            .collect();
        // Node's readdirSync lists names in byte order (libuv sorts them); prune's ties keep it.
        ids.sort();
        ids
    }

    fn of_kind(&self, job: &Job) -> bool {
        match &self.options.kinds {
            None => true,
            Some(kinds) => job
                .kind
                .as_str()
                .is_some_and(|k| kinds.iter().any(|x| x == k)),
        }
    }

    /// jobs.cjs `recover`: after a restart, every unfinished job of this store's kinds that `live`
    /// does not name is interrupted. Unreadable journals are left for review. The count interrupted.
    pub fn recover(&self, live: &dyn Fn(&str) -> bool) -> Result<usize, Refusal> {
        let candidates: Vec<String> = {
            let _held = self.guard();
            let mut out = Vec::new();
            for id in self.ids() {
                if let Some(job) = self.readable(&id)? {
                    if !finished(&job) && self.of_kind(&job) {
                        out.push(id);
                    }
                }
            }
            out
        };
        // `live` is the host's, asked without the lock (a run it names may be appending).
        let idle: Vec<String> = candidates.into_iter().filter(|id| !live(id)).collect();
        let _held = self.guard();
        let mut count = 0;
        for id in idle {
            // Read again: it may have ended meanwhile.
            let Some(job) = self.readable(&id)? else {
                continue;
            };
            if finished(&job) {
                continue;
            }
            let reason = if job.status.as_str() == Some("waiting_approval") {
                WAITING_REASON
            } else {
                RESTART_REASON
            };
            let data = JValue::obj([("reason", JValue::Str(reason.to_string()))]);
            match self.append_unlocked(&id, "job.interrupted", data, false) {
                Ok(_) => count += 1,
                // Left for review, as an unreadable journal is.
                Err(e) if e.is_unreadable() => {}
                Err(e) => return Err(e),
            }
        }
        Ok(count)
    }

    /// The job, `None` when there is none or its journal is unreadable (jobs.cjs `getSafe`).
    fn readable(&self, id: &str) -> Result<Option<Job>, Refusal> {
        match self.get_unlocked(id) {
            Ok(job) => Ok(job),
            Err(e) if e.is_unreadable() => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// jobs.cjs `prune`: finished jobs of this store's kinds older than `retain_ms`, and all but the
    /// newest `max_jobs` of them, are removed. Unreadable journals are never removed.
    pub fn prune(&self) -> Result<(), Refusal> {
        let _held = self.guard();
        self.prune_unlocked()
    }

    fn prune_unlocked(&self) -> Result<(), Refusal> {
        let mut done: Vec<(f64, String)> = Vec::new();
        for id in self.ids() {
            if let Some(job) = self.readable(&id)? {
                if finished(&job) && self.of_kind(&job) {
                    done.push((js_number(&job.updated_at), id));
                }
            }
        }
        // `b.updatedAt - a.updatedAt`, a stable sort. A time that is not a number (NaN) sorts
        // last here, so the order is total; V8's order for it is unspecified.
        done.sort_by(|a, b| key(b.0).total_cmp(&key(a.0)));
        for (i, (updated, id)) in done.iter().enumerate() {
            // With NaN the age test is false, as in JS.
            if (self.now)() - updated > self.options.retain_ms || i >= self.options.max_jobs {
                // The file by its own name: jobs.cjs removed `file(job.id)`, the id its
                // `job.created` row names, which may be another job's file or none at all.
                let path = self.file(id)?;
                match fs::remove_file(&path) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(io(&e)),
                }
            }
        }
        Ok(())
    }
}

fn finished(job: &Job) -> bool {
    job.status.as_str().is_some_and(|s| TERMINAL.contains(&s))
}

fn key(n: f64) -> f64 {
    if n.is_nan() {
        f64::NEG_INFINITY
    } else {
        n
    }
}

/// JS `ToNumber` for the values a journal's `at` can hold: a number, `null` (0), a boolean, a
/// numeric string; anything else is NaN.
fn js_number(v: &JValue) -> f64 {
    match v {
        JValue::Num(n) => *n,
        JValue::Null => 0.0,
        JValue::Bool(b) => f64::from(u8::from(*b)),
        JValue::Str(s) => string_number(s),
        _ => f64::NAN,
    }
}

fn string_number(s: &str) -> f64 {
    let t = js_trim(s);
    if t.is_empty() {
        return 0.0;
    }
    let (sign, body) = match t.strip_prefix('-') {
        Some(rest) => (-1.0, rest),
        None => (1.0, t.strip_prefix('+').unwrap_or(t)),
    };
    if body == "Infinity" {
        return sign * f64::INFINITY;
    }
    for (prefix, radix) in [
        ("0x", 16),
        ("0X", 16),
        ("0o", 8),
        ("0O", 8),
        ("0b", 2),
        ("0B", 2),
    ] {
        if let Some(digits) = t.strip_prefix(prefix) {
            if digits.is_empty() || !digits.chars().all(|c| c.is_digit(radix)) {
                return f64::NAN;
            }
            return digits
                .chars()
                .filter_map(|c| c.to_digit(radix))
                .fold(0.0, |acc, d| acc * f64::from(radix) + f64::from(d));
        }
    }
    // A decimal literal: digits, at most one point, an optional exponent; nothing Rust's parser
    // takes beyond that ("inf", "nan").
    let decimal = {
        let (mantissa, exponent) = match body.find(['e', 'E']) {
            Some(i) => (body.get(..i).unwrap_or(""), body.get(i + 1..)),
            None => (body, None),
        };
        let digits = mantissa.chars().filter(char::is_ascii_digit).count();
        let points = mantissa.chars().filter(|c| *c == '.').count();
        let mantissa_ok = digits > 0 && points <= 1 && digits + points == mantissa.len();
        let exponent_ok = exponent.is_none_or(|e| {
            let e = e.strip_prefix(['+', '-']).unwrap_or(e);
            !e.is_empty() && e.chars().all(|c| c.is_ascii_digit())
        });
        mantissa_ok && exponent_ok
    };
    if !decimal {
        return f64::NAN;
    }
    body.parse::<f64>().map_or(f64::NAN, |n| sign * n)
}

/// `String.prototype.trim`'s whitespace and line terminators.
pub(crate) fn js_trim(s: &str) -> &str {
    s.trim_matches(|c: char| {
        matches!(
            c,
            '\u{9}'..='\u{d}'
                | ' '
                | '\u{a0}'
                | '\u{1680}'
                | '\u{2000}'..='\u{200a}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202f}'
                | '\u{205f}'
                | '\u{3000}'
                | '\u{feff}'
        )
    })
}

/// How deep event data may nest (JSON.parse-read data never needs more).
const MAX_DATA_DEPTH: usize = 128;

/// An object as a JS object holds it: array-index keys first, ascending, and a repeated key's
/// last value in its first place, at every level. What JSON.stringify then writes is what Node
/// would write, and what a reader parses back is the same object, so a chained row's hash holds.
fn js_object(v: JValue, depth: usize) -> Result<JValue, Refusal> {
    if depth > MAX_DATA_DEPTH {
        return Err(Refusal::new(None, "Job event data is nested too deep"));
    }
    Ok(match v {
        JValue::Obj(items) => {
            let mut out: Vec<(String, JValue)> = Vec::with_capacity(items.len());
            for (k, value) in items {
                let value = js_object(value, depth + 1)?;
                match out.iter_mut().find(|(have, _)| *have == k) {
                    Some(slot) => slot.1 = value,
                    None => out.push((k, value)),
                }
            }
            wtf16::order_keys(&mut out);
            JValue::Obj(out)
        }
        JValue::Arr(items) => JValue::Arr(
            items
                .into_iter()
                .map(|x| js_object(x, depth + 1))
                .collect::<Result<_, _>>()?,
        ),
        other => other,
    })
}

#[cfg(unix)]
fn links(meta: &fs::Metadata) -> u64 {
    std::os::unix::fs::MetadataExt::nlink(meta)
}

#[cfg(not(unix))]
fn links(_: &fs::Metadata) -> u64 {
    1
}

/// A journal as read for an append, and what its end needs before the next line.
struct Journal {
    rows: Vec<JValue>,
    tail: Result<Tail, Refusal>,
}

impl Default for Journal {
    fn default() -> Self {
        Journal {
            rows: Vec::new(),
            tail: Ok(Tail::Whole),
        }
    }
}

#[derive(Default, Debug, Clone, PartialEq, Eq)]
enum Tail {
    /// Ends in a newline, or is empty.
    #[default]
    Whole,
    /// The last row parsed but has no newline: one goes before the next row.
    NoNewline,
    /// A last line that did not parse was dropped: the file is cut back to this length.
    Cut(u64),
}

/// What the end of `raw` needs, given that the reader kept `kept` rows. Every line before the last
/// one was read (the reader refuses a damaged middle line), so only the last one can be missing.
fn tail_of(raw: &[u8], kept: usize) -> Result<Tail, Refusal> {
    let text = String::from_utf8_lossy(raw);
    let lines: Vec<&str> = text.split('\n').filter(|l| !l.is_empty()).collect();
    if lines.len() == kept {
        return Ok(if raw.last().is_some_and(|b| *b != b'\n') {
            Tail::NoNewline
        } else {
            Tail::Whole
        });
    }
    // The dropped line is the last non-empty one. JSON.parse would have read a line this port only
    // cannot hold: refused rather than cut. A line that does not parse but ends in a newline is not
    // a torn write (a row and its newline are one write): it is damage, kept for review, and
    // nothing is written after it.
    let last = lines.last().copied().unwrap_or("");
    if wtf16::parse(last) != Err(wtf16::Refusal::Invalid) || raw.last() == Some(&b'\n') {
        return Err(unreadable());
    }
    // The last non-empty segment is the dropped line: cut at its first byte (the raw bytes, not the
    // decoded text, whose length differs where a byte is not UTF-8).
    let end = raw.iter().rposition(|b| *b != b'\n').map_or(0, |i| i + 1);
    let start = raw
        .get(..end)
        .and_then(|head| head.iter().rposition(|b| *b == b'\n'))
        .map_or(0, |i| i + 1);
    Ok(Tail::Cut(start as u64))
}

fn sha256_hex(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The bytes of a regular file opened without following a link; `None` when there is none.
fn read_plain(path: &Path) -> Result<Option<Vec<u8>>, ()> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = match options.open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(()),
    };
    let meta = file.metadata().map_err(|_| ())?;
    if !meta.file_type().is_file() || meta.len() > journals::MAX_JOURNAL_BYTES {
        return Err(());
    }
    let mut raw = Vec::new();
    (&mut file)
        .take(journals::MAX_JOURNAL_BYTES + 1)
        .read_to_end(&mut raw)
        .map_err(|_| ())?;
    if raw.len() as u64 > journals::MAX_JOURNAL_BYTES {
        return Err(());
    }
    Ok(Some(raw))
}

/// `fs.mkdirSync(root, { recursive: true, mode: 0o700 })`.
fn make_dir(root: &Path) -> std::io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(root)
}

/// `fs.openSync(file, 'a+', 0o600)`, without following a link.
fn open_append(path: &Path) -> std::io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.read(true).append(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    options.open(path)
}
