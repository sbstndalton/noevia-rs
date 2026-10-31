//! Reads the Code and Browser task journals that core server/jobs.cjs writes
//! (`users/<id>/jobs/<uuid>.jsonl`), and folds them into the views core code-service.cjs and
//! browser-service.cjs answered with. Read-only: Node stays the only writer of these two kinds.
//!
//! **Reading beside a live writer.** jobs.cjs appends one event per synchronous write and, only
//! when the last line lost its newline in a crash, first cuts the file back to its last complete
//! line. So a reader can see:
//!
//! - a last line that has no newline yet. It is dropped from a plain journal, as Node drops a torn
//!   last line. A hash-chained journal fails closed on one, as in Node, but only once the file has
//!   stopped changing, because Node could never see its own write half done and this reader can;
//! - a cut racing a read, which could join an old fragment to the tail of a new line. A read counts
//!   only if the file's size, mtime and inode did not move across it; otherwise it is read again.
//!   A line that does not parse is also re-read if the file moved, so it reads as damage only when
//!   it is on disk at rest.
//!
//! Corruption never becomes an empty journal: the task reads as `unreadable` (Node's 409 on a get).
//!
//! **Fixed on purpose against Node:** a stray `x.jsonl` whose name is not a job id took the whole
//! list down with a 400; it is skipped. An unreadable journal was listed in every project and for
//! every kind, deep research's included; it now shows only where its first line says it belongs,
//! and only a journal with no readable first line is still shown everywhere. A journal is read up
//! to [`MAX_JOURNAL_BYTES`]; a larger one is unreadable rather than loaded.
use crate::store;
use js_json::{to_js_string, wtf16, JValue};
use prompt_framing::json as lifecycle_json;
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

pub const MAX_JOURNAL_BYTES: u64 = 64 * 1024 * 1024;
const READ_ATTEMPTS: usize = 6;
const SETTLE: Duration = Duration::from_millis(15);
const MAX_ASSISTANT_OUTPUT_BYTES: usize = 32 * 1024;
const MAX_CODE_PLAN_ENTRIES: usize = 20;
const MAX_CODE_PLAN_ENTRY_BYTES: usize = 200;
const MAX_TAIL_VIEW: usize = 16 * 1024;
const MAX_NETWORK_REPORTS: usize = 32;
const LIFECYCLE_DEPTH: usize = 64;

/// Which journals a route reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Surface {
    Code,
    Browser,
}

impl Surface {
    pub fn kind(self) -> &'static str {
        match self {
            Surface::Code => "code",
            Surface::Browser => "browser",
        }
    }
}

/// One task as jobs.cjs's `derive` left it, plus what its first line said if the rest is damaged.
#[derive(Debug, Clone)]
pub struct Job {
    pub id: JValue,
    pub kind: JValue,
    pub project_id: JValue,
    pub capabilities: JValue,
    pub status: JValue,
    pub stage: JValue,
    pub steps: Vec<JValue>,
    pub artifacts: Vec<JValue>,
    pub plan: JValue,
    pub assistant_output: JValue,
    pub checkpoint: JValue,
    pub uncertain: Vec<JValue>,
    pub result: JValue,
    pub error: JValue,
    pub created_at: JValue,
    pub updated_at: JValue,
    pub review: Option<JValue>,
    /// `Some` only on a journal with lifecycle authority events.
    pub authoritative: Option<Authority>,
}

#[derive(Debug, Clone)]
pub struct Authority {
    pub revision: JValue,
    pub stages: Vec<JValue>,
    pub lifecycle: JValue,
}

/// A journal's state on disk.
#[derive(Debug)]
pub enum Loaded {
    /// No such file, or no event in it.
    Absent,
    Job(Box<Job>),
    /// Damaged. `kind` and `project` are its first line's, when that line can be read.
    Unreadable {
        kind: Option<String>,
        project: Option<String>,
    },
}

/// `/^[0-9a-f-]{36}$/`, core's job id.
pub fn valid_id(id: &str) -> bool {
    id.len() == 36
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b) || b == b'-')
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Stamp(u64, Option<SystemTime>, u64);

fn stamp_of(m: &fs::Metadata) -> Stamp {
    #[cfg(unix)]
    let ino = std::os::unix::fs::MetadataExt::ino(m);
    #[cfg(not(unix))]
    let ino = 0;
    Stamp(m.len(), m.modified().ok(), ino)
}

enum Bytes {
    Absent,
    Refused,
    Read(Vec<u8>, Stamp),
}

/// One read of a regular file that nothing changed across, or the last read if it kept changing.
fn read_settled(path: &Path) -> Bytes {
    let mut last = None;
    for attempt in 0..READ_ATTEMPTS {
        if attempt > 0 {
            std::thread::sleep(SETTLE);
        }
        let before = match fs::symlink_metadata(path) {
            Ok(m) if m.file_type().is_file() => m,
            Ok(_) => return Bytes::Refused,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Bytes::Absent,
            Err(_) => return Bytes::Refused,
        };
        if before.len() > MAX_JOURNAL_BYTES {
            return Bytes::Refused;
        }
        let mut raw = Vec::new();
        let read = fs::File::open(path).and_then(|f| {
            f.take(MAX_JOURNAL_BYTES + 1).read_to_end(&mut raw)?;
            fs::symlink_metadata(path)
        });
        let after = match read {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Bytes::Absent,
            Err(_) => return Bytes::Refused,
        };
        if raw.len() as u64 > MAX_JOURNAL_BYTES {
            return Bytes::Refused;
        }
        let stamp = stamp_of(&after);
        if stamp_of(&before) == stamp {
            return Bytes::Read(raw, stamp);
        }
        last = Some((raw, stamp));
    }
    last.map_or(Bytes::Refused, |(raw, stamp)| Bytes::Read(raw, stamp))
}

enum Parsed {
    Rows(Vec<JValue>),
    /// A chained journal with a last line that is not whole yet, or a line that does not parse.
    Damaged,
}

fn chain_hash(row: &JValue) -> bool {
    row.get("hash").truthy()
}

/// jobs.cjs `readJournal`, after the read.
fn parse_rows(id: &str, raw: &[u8]) -> Parsed {
    let text = String::from_utf8_lossy(raw);
    let lines: Vec<&str> = text.split('\n').filter(|l| !l.is_empty()).collect();
    let mut rows = Vec::with_capacity(lines.len());
    let count = lines.len();
    for (i, line) in lines.into_iter().enumerate() {
        match wtf16::parse(line) {
            Ok(row) => rows.push(row),
            Err(_) => {
                let chained = rows.first().is_some_and(chain_hash);
                if i + 1 == count && !chained {
                    break;
                }
                return Parsed::Damaged;
            }
        }
    }
    // Node reads a property of each row, and a null row throws there. A row that is not an object
    // has no event in it either way, so any such row is damage.
    if rows.iter().any(|r| !r.is_object()) {
        return Parsed::Damaged;
    }
    let mut previous = JValue::Null;
    let any_first = rows.first().is_some_and(chain_hash);
    for (i, row) in rows.iter().enumerate() {
        if !(any_first || chain_hash(row)) {
            continue;
        }
        let JValue::Obj(items) = row else {
            return Parsed::Damaged;
        };
        let hash = row.get("hash").clone();
        let payload = JValue::Obj(items.iter().filter(|(k, _)| k != "hash").cloned().collect());
        let Some(text) = wtf16::stringify(&payload) else {
            return Parsed::Damaged;
        };
        let digest: String = Sha256::digest(text.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        if row.get("job").as_str() != Some(id)
            || row.get("seq").as_num() != Some((i + 1) as f64)
            || *row.get("previous") != previous
            || hash.as_str() != Some(&digest)
        {
            return Parsed::Damaged;
        }
        previous = hash;
    }
    Parsed::Rows(rows)
}

/// What the first complete line says, for a journal whose whole cannot be read.
fn first_line_owner(raw: &[u8]) -> (Option<String>, Option<String>) {
    let text = String::from_utf8_lossy(raw);
    let Some(first) = text.split('\n').find(|l| !l.is_empty()) else {
        return (None, None);
    };
    let Ok(row) = wtf16::parse(first) else {
        return (None, None);
    };
    if row.get("type").as_str() != Some("job.created") {
        return (None, None);
    }
    let data = row.get("data");
    (
        data.get("kind").as_str().map(str::to_owned),
        data.get("projectId").as_str().map(str::to_owned),
    )
}

/// Loads one journal, stable against a concurrent append.
pub fn load(path: &Path, id: &str) -> Loaded {
    let mut last_raw = Vec::new();
    for attempt in 0..READ_ATTEMPTS {
        if attempt > 0 {
            std::thread::sleep(SETTLE);
        }
        let (raw, stamp) = match read_settled(path) {
            Bytes::Absent => return Loaded::Absent,
            Bytes::Refused => {
                return Loaded::Unreadable {
                    kind: None,
                    project: None,
                }
            }
            Bytes::Read(raw, stamp) => (raw, stamp),
        };
        if let Parsed::Rows(rows) = parse_rows(id, &raw) {
            if rows.is_empty() {
                return Loaded::Absent;
            }
            return Loaded::Job(Box::new(derive(&rows)));
        }
        last_raw = raw;
        // Damaged: judged only once the file is at rest. If it has not moved, this is what is on disk.
        std::thread::sleep(SETTLE);
        match fs::symlink_metadata(path) {
            Ok(m) if stamp_of(&m) == stamp => break,
            Ok(_) => {}
            Err(_) => return Loaded::Absent,
        }
    }
    let (kind, project) = first_line_owner(&last_raw);
    Loaded::Unreadable { kind, project }
}

fn jobs_dir(data_dir: &Path, user: &str) -> Result<PathBuf, store::StoreError> {
    Ok(store::user_dir(data_dir, user)?.join("jobs"))
}

/// One task of [`get`].
pub enum Found {
    Task(Box<Job>),
    Unreadable,
    Missing,
}

/// jobs.cjs `get`, scoped as core's `owned`: another kind or project is simply not there.
pub fn get(
    data_dir: &Path,
    user: &str,
    surface: Surface,
    project: &str,
    id: &str,
) -> Result<Found, store::StoreError> {
    if !valid_id(id) {
        return Ok(Found::Missing);
    }
    let path = jobs_dir(data_dir, user)?.join(format!("{id}.jsonl"));
    Ok(match load(&path, id) {
        Loaded::Absent => Found::Missing,
        Loaded::Unreadable { .. } => Found::Unreadable,
        Loaded::Job(job) => {
            if job.kind.as_str() == Some(surface.kind()) && job.project_id.as_str() == Some(project)
            {
                Found::Task(job)
            } else {
                Found::Missing
            }
        }
    })
}

fn stub(id: &str) -> Job {
    Job {
        id: JValue::Str(id.to_owned()),
        status: JValue::Str("unreadable".into()),
        ..empty_job()
    }
}

/// jobs.cjs `list({ projectId, kind })`, newest first. `project` of `None` is every project;
/// `active` keeps only tasks that have not ended, and never lists an unreadable one (core's active
/// list filters on a project id such a task does not have).
pub fn list(
    data_dir: &Path,
    user: &str,
    surface: Surface,
    project: Option<&str>,
    active: bool,
) -> Result<Vec<Job>, store::StoreError> {
    let dir = jobs_dir(data_dir, user)?;
    let plain = fs::symlink_metadata(&dir).is_ok_and(|m| m.file_type().is_dir());
    let Some(entries) = plain.then(|| fs::read_dir(&dir).ok()).flatten() else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(id) = name.to_str().and_then(|n| n.strip_suffix(".jsonl")) else {
            continue;
        };
        if !valid_id(id) {
            continue;
        }
        match load(&entry.path(), id) {
            Loaded::Absent => {}
            Loaded::Unreadable { kind, project: own } => {
                let kind_ok = kind.as_deref().is_none_or(|k| k == surface.kind());
                let project_ok = own
                    .as_deref()
                    .is_none_or(|p| project.is_none_or(|want| want == p));
                if !active && kind_ok && project_ok {
                    out.push(stub(id));
                }
            }
            Loaded::Job(job) => {
                let kept = job.kind.as_str() == Some(surface.kind())
                    && project.is_none_or(|want| job.project_id.as_str() == Some(want))
                    && (!active || !terminal(&job.status));
                if kept {
                    out.push(*job);
                }
            }
        }
    }
    let created = |j: &Job| j.created_at.as_num().unwrap_or(0.0);
    out.sort_by(|a, b| {
        created(b)
            .total_cmp(&created(a))
            .then_with(|| a.id.as_str().cmp(&b.id.as_str()))
    });
    Ok(out)
}

fn terminal(status: &JValue) -> bool {
    matches!(
        status.as_str(),
        Some("completed" | "failed" | "cancelled" | "interrupted")
    )
}

fn empty_job() -> Job {
    Job {
        id: JValue::Undefined,
        kind: JValue::Undefined,
        project_id: JValue::Undefined,
        capabilities: JValue::Undefined,
        status: JValue::Undefined,
        stage: JValue::Undefined,
        steps: Vec::new(),
        artifacts: Vec::new(),
        plan: JValue::Undefined,
        assistant_output: JValue::Undefined,
        checkpoint: JValue::Undefined,
        uncertain: Vec::new(),
        result: JValue::Undefined,
        error: JValue::Undefined,
        created_at: JValue::Undefined,
        updated_at: JValue::Undefined,
        review: None,
        authoritative: None,
    }
}

fn nullish_or(v: &JValue, or: JValue) -> JValue {
    if v.is_nullish() {
        or
    } else {
        v.clone()
    }
}

fn or_null(v: &JValue) -> JValue {
    if v.truthy() {
        v.clone()
    } else {
        JValue::Null
    }
}

fn obj(items: Vec<(&str, JValue)>) -> JValue {
    JValue::Obj(items.into_iter().map(|(k, v)| (k.to_owned(), v)).collect())
}

fn s(text: &str) -> JValue {
    JValue::Str(text.to_owned())
}

/// core jobs.cjs `clipUtf8`: `(clipped, whether anything was left out)`; a lone surrogate reads as U+FFFD.
fn clip_utf8(text: &str, max: usize) -> (String, bool) {
    let mut out = String::new();
    for c in text.chars() {
        let c = if wtf16::lone_unit(c).is_some() {
            '\u{FFFD}'
        } else {
            c
        };
        if out.len() + c.len_utf8() > max {
            return (out, true);
        }
        out.push(c);
    }
    (out, false)
}

/// core code-plan.cjs `boundCodePlan`: `{ subQuestions, truncated }`.
fn bound_code_plan(status: &str, data: &JValue) -> JValue {
    let entries: &[JValue] = match data.get("subQuestions") {
        JValue::Arr(a) => a,
        _ => &[],
    };
    let mut truncated =
        data.get("truncated") == &JValue::Bool(true) || entries.len() > MAX_CODE_PLAN_ENTRIES;
    let questions = entries
        .iter()
        .take(MAX_CODE_PLAN_ENTRIES)
        .map(|entry| {
            let original = if entry.is_nullish() {
                String::new()
            } else {
                to_js_string(entry).unwrap_or_default()
            };
            let (text, cut) = clip_utf8(&original, MAX_CODE_PLAN_ENTRY_BYTES);
            truncated |= cut;
            JValue::Str(text)
        })
        .collect();
    obj(vec![
        ("status", s(status)),
        ("question", JValue::Null),
        ("subQuestions", JValue::Arr(questions)),
        ("truncated", JValue::Bool(truncated)),
    ])
}

fn to_review_js(v: &JValue) -> review_verdict::Js {
    use review_verdict::Js;
    let units = |text: &str| -> Vec<u16> {
        let mut out = Vec::with_capacity(text.len());
        for c in text.chars() {
            match wtf16::lone_unit(c) {
                Some(unit) => out.push(unit),
                None => {
                    let mut buf = [0u16; 2];
                    out.extend_from_slice(c.encode_utf16(&mut buf));
                }
            }
        }
        out
    };
    match v {
        JValue::Undefined => Js::Undefined,
        JValue::Null => Js::Null,
        JValue::Bool(b) => Js::Bool(*b),
        JValue::Num(n) => Js::Num(*n),
        JValue::Str(text) => Js::Str(units(text)),
        JValue::Arr(items) => Js::Arr(items.iter().map(to_review_js).collect()),
        JValue::Obj(items) => Js::Obj(
            items
                .iter()
                .map(|(k, val)| (units(k), to_review_js(val)))
                .collect(),
        ),
    }
}

/// core code-review-verdict.cjs `boundReviewEvent`; a fault keeps a failed review that names nothing.
fn bound_review(kind: &str, data: &JValue) -> JValue {
    let tag = review_verdict::Js::Str(kind.encode_utf16().collect());
    let bound = review_verdict::bound_review_event(&tag, &to_review_js(data)).ok();
    let parsed = bound.and_then(|event| {
        let mut out = Vec::new();
        review_verdict::event_json(&mut out, &event);
        wtf16::parse(&String::from_utf8_lossy(&out)).ok()
    });
    parsed.unwrap_or_else(|| {
        obj(vec![
            ("status", s("failed")),
            ("reviewer", s("planner")),
            ("baseSha", JValue::Null),
            ("headSha", JValue::Null),
            ("code", s("invalid")),
            ("reason", s("The recorded verdict could not be read.")),
        ])
    })
}

/// core task-lifecycle.cjs `foldInput`: only what the fold reads, then the shared Rust fold.
fn lifecycle_state(rows: &[JValue], authoritative: bool) -> JValue {
    let mut input = Vec::new();
    for row in rows {
        let Some(kind) = row.get("type").as_str() else {
            continue;
        };
        let fields: &[&str] = match kind {
            "task.stage" => &["to", "from", "revision", "reportHash"],
            "task.revision" => &["n", "headSha", "planHash"],
            "progress" => &["stage"],
            "job.completed" | "job.failed" | "job.cancelled" | "job.interrupted" => &[],
            _ => continue,
        };
        let mut event = vec![("type", s(kind))];
        if !fields.is_empty() {
            let data = row.get("data");
            let kept = fields
                .iter()
                .filter(|f| !matches!(data.get(f), JValue::Undefined))
                .map(|f| ((*f).to_owned(), data.get(f).clone()))
                .collect();
            event.push(("data", JValue::Obj(kept)));
        }
        input.push(obj(event));
    }
    let Some(text) = wtf16::stringify(&JValue::Arr(input)) else {
        return JValue::Null;
    };
    let Some(events) = lifecycle_json::parse_utf8(text.as_bytes(), LIFECYCLE_DEPTH) else {
        return JValue::Null;
    };
    let from = lifecycle_json::Value::Str(
        task_lifecycle::INITIAL_STATE
            .name()
            .encode_utf16()
            .collect(),
    );
    match task_lifecycle::fold(&events, &from, authoritative) {
        Ok(Ok(state)) => s(state.name()),
        _ => JValue::Null,
    }
}

/// jobs.cjs `derive`: the fold over one whole journal.
pub fn derive(rows: &[JValue]) -> Job {
    let authoritative = rows
        .iter()
        .any(|r| matches!(r.get("type").as_str(), Some("task.stage" | "task.revision")));
    let mut job = Job {
        id: JValue::Null,
        kind: JValue::Null,
        project_id: JValue::Null,
        capabilities: JValue::Arr(Vec::new()),
        status: s("queued"),
        stage: JValue::Null,
        plan: JValue::Null,
        assistant_output: JValue::Null,
        checkpoint: JValue::Null,
        result: JValue::Null,
        error: JValue::Null,
        created_at: JValue::Null,
        updated_at: JValue::Null,
        ..empty_job()
    };
    let mut revision = JValue::Null;
    let mut revision_n = 0.0;
    let mut stages = Vec::new();
    let mut steady = true;
    let empty = JValue::Obj(Vec::new());
    for e in rows {
        let at = e.get("at");
        let d = if e.get("data").truthy() {
            e.get("data")
        } else {
            &empty
        };
        job.updated_at = at.clone();
        let Some(kind) = e.get("type").as_str() else {
            continue;
        };
        match kind {
            "job.created" => {
                job.id = e.get("job").clone();
                job.kind = d.get("kind").clone();
                job.project_id = nullish_or(d.get("projectId"), JValue::Null);
                job.capabilities = if d.get("capabilities").truthy() {
                    d.get("capabilities").clone()
                } else {
                    JValue::Arr(Vec::new())
                };
                job.created_at = at.clone();
            }
            "job.started" => job.status = s("running"),
            "progress" => job.stage = nullish_or(d.get("stage"), job.stage.clone()),
            "step.started" => job.steps.push(obj(vec![
                ("id", d.get("id").clone()),
                ("title", d.get("title").clone()),
                ("status", s("running")),
            ])),
            "step.completed" => {
                let id = d.get("id");
                let running = |x: &&mut JValue| {
                    x.get("id") == id && x.get("status").as_str() == Some("running")
                };
                let found = if authoritative {
                    job.steps.iter_mut().rev().find(running).is_some()
                } else {
                    false
                };
                let done = if d.get("failed").truthy() {
                    "failed"
                } else {
                    "completed"
                };
                let step = if found {
                    job.steps.iter_mut().rev().find(running)
                } else {
                    job.steps.iter_mut().find(|x| x.get("id") == id)
                };
                if let Some(JValue::Obj(items)) = step {
                    wtf16::set(items, "status", s(done));
                }
            }
            "approval.requested" => job.status = s("waiting_approval"),
            "approval.decided" => {
                if !terminal(&job.status) {
                    job.status = s("running");
                }
            }
            "tool.uncertain" => job.uncertain.push(d.clone()),
            "artifact.created" => job.artifacts.push(d.clone()),
            "plan.proposed" | "plan.edited" if job.kind.as_str() == Some("code") => {
                job.plan = bound_code_plan(kind.strip_prefix("plan.").unwrap_or(kind), d);
            }
            "plan.skipped" if job.kind.as_str() == Some("code") => {
                job.plan = bound_code_plan("skipped", &empty);
            }
            "assistant.output" => {
                let (previous, was_cut) = match &job.assistant_output {
                    JValue::Obj(_) => (
                        job.assistant_output
                            .get("text")
                            .as_str()
                            .unwrap_or("")
                            .to_owned(),
                        job.assistant_output.get("truncated") == &JValue::Bool(true),
                    ),
                    _ => (String::new(), false),
                };
                let incoming = d.get("text").as_str().unwrap_or("");
                let remaining = MAX_ASSISTANT_OUTPUT_BYTES.saturating_sub(previous.len());
                let (text, cut) = if was_cut {
                    (String::new(), !incoming.is_empty())
                } else {
                    clip_utf8(incoming, remaining)
                };
                let marked = d.get("truncated") == &JValue::Bool(true);
                if !text.is_empty() || marked || !previous.is_empty() {
                    job.assistant_output = obj(vec![
                        ("text", JValue::Str(format!("{previous}{text}"))),
                        ("truncated", JValue::Bool(was_cut || marked || cut)),
                    ]);
                }
            }
            "checkpoint.created" => job.checkpoint = d.clone(),
            "review.requested" | "review.completed" | "review.failed" => {
                job.review = Some(bound_review(kind, d));
            }
            "task.stage" => {
                if d.get("revision").as_num() != Some(revision_n) {
                    steady = false;
                }
                let mut stage = vec![
                    ("from", nullish_or(d.get("from"), JValue::Null)),
                    ("to", nullish_or(d.get("to"), JValue::Null)),
                    ("revision", nullish_or(d.get("revision"), JValue::Num(0.0))),
                    ("reason", nullish_or(d.get("reason"), JValue::Null)),
                ];
                if d.get("reportHash").truthy() {
                    stage.push(("reportHash", d.get("reportHash").clone()));
                }
                stage.push(("at", at.clone()));
                stages.push(obj(stage));
            }
            "task.revision" => {
                if d.get("n").as_num() != Some(revision_n + 1.0) {
                    steady = false;
                }
                revision_n = d.get("n").as_num().unwrap_or(0.0);
                revision = obj(vec![
                    ("n", d.get("n").clone()),
                    ("headSha", nullish_or(d.get("headSha"), JValue::Null)),
                    ("planHash", nullish_or(d.get("planHash"), JValue::Null)),
                    ("at", at.clone()),
                ]);
            }
            "job.completed" => {
                job.status = s("completed");
                job.result = nullish_or(d.get("result"), JValue::Null);
            }
            "job.failed" => {
                job.status = s("failed");
                job.error = nullish_or(d.get("error"), s("failed"));
                job.result = nullish_or(d.get("result"), JValue::Null);
            }
            "job.cancelled" => {
                job.status = s("cancelled");
                job.result = nullish_or(d.get("result"), JValue::Null);
            }
            "job.interrupted" => {
                job.status = s("interrupted");
                job.error = nullish_or(d.get("reason"), s("interrupted"));
            }
            _ => {}
        }
    }
    if authoritative {
        job.authoritative = Some(Authority {
            revision,
            stages,
            lifecycle: if steady {
                lifecycle_state(rows, true)
            } else {
                JValue::Null
            },
        });
    }
    job
}

fn network_reports(job: &Job) -> Option<JValue> {
    let reports: Vec<JValue> = job
        .artifacts
        .iter()
        .filter(|a| a.get("kind").as_str() == Some("native-network-report"))
        .map(|a| a.get("network").clone())
        .collect();
    if reports.is_empty() {
        return None;
    }
    let skip = reports.len().saturating_sub(MAX_NETWORK_REPORTS);
    Some(JValue::Arr(reports.into_iter().skip(skip).collect()))
}

fn view_tests(a: &JValue) -> JValue {
    let tail = a.get("tail").as_str().unwrap_or("");
    let over = tail.len() > MAX_TAIL_VIEW;
    let shown = if over {
        let from = tail.len() - MAX_TAIL_VIEW;
        String::from_utf8_lossy(tail.as_bytes().get(from..).unwrap_or(&[])).into_owned()
    } else {
        tail.to_owned()
    };
    let whole = |v: &JValue| {
        v.as_num()
            .is_some_and(|n| n.fract() == 0.0 && n.abs() <= 9e15)
    };
    obj(vec![
        (
            "passed",
            JValue::Bool(a.get("passed") == &JValue::Bool(true)),
        ),
        (
            "exitCode",
            if whole(a.get("exitCode")) {
                a.get("exitCode").clone()
            } else {
                JValue::Null
            },
        ),
        ("signal", a.get("signal").as_str().map_or(JValue::Null, s)),
        (
            "timedOut",
            JValue::Bool(a.get("timedOut") == &JValue::Bool(true)),
        ),
        ("headSha", a.get("headSha").as_str().map_or(JValue::Null, s)),
        (
            "durationMs",
            if a.get("durationMs").as_num().is_some() {
                a.get("durationMs").clone()
            } else {
                JValue::Null
            },
        ),
        ("tail", JValue::Str(shown)),
        (
            "truncated",
            JValue::Bool(a.get("truncated") == &JValue::Bool(true) || over),
        ),
    ])
}

/// core auditor-report.cjs `pipelineView`.
fn pipeline_view(job: &Job, stages: &[JValue]) -> Option<JValue> {
    let meta = job
        .artifacts
        .iter()
        .find(|a| a.get("kind").as_str() == Some("pipeline"))?;
    let mut revisions: Vec<(f64, Vec<(&str, JValue)>)> = Vec::new();
    fn at<'a>(
        list: &'a mut Vec<(f64, Vec<(&'static str, JValue)>)>,
        n: f64,
    ) -> &'a mut Vec<(&'static str, JValue)> {
        let index = list.iter().position(|(r, _)| *r == n).unwrap_or_else(|| {
            list.push((
                n,
                vec![
                    ("revision", JValue::Num(n)),
                    ("headSha", JValue::Null),
                    ("baseSha", JValue::Null),
                    ("planHash", JValue::Null),
                    ("tests", JValue::Null),
                    ("review", JValue::Null),
                    ("completeness", JValue::Null),
                ],
            ));
            list.len() - 1
        });
        match list.get_mut(index) {
            Some(entry) => &mut entry.1,
            None => unreachable!("just found or pushed"),
        }
    }
    fn put(fields: &mut [(&'static str, JValue)], key: &str, value: JValue) {
        if let Some(slot) = fields.iter_mut().find(|(k, _)| *k == key) {
            slot.1 = value;
        }
    }
    let nullish = |v: &JValue| nullish_or(v, JValue::Null);
    let (mut plan, mut audit) = (JValue::Null, JValue::Null);
    for a in &job.artifacts {
        let Some(n) = a.get("revision").as_num().filter(|n| n.fract() == 0.0) else {
            continue;
        };
        match a.get("kind").as_str() {
            Some("plan") => {
                put(
                    at(&mut revisions, n),
                    "planHash",
                    nullish(a.get("planHash")),
                );
                plan = obj(vec![
                    ("planHash", nullish(a.get("planHash"))),
                    ("revision", JValue::Num(n)),
                    ("plan", nullish(a.get("plan"))),
                ]);
            }
            Some("revision") => {
                let fields = at(&mut revisions, n);
                put(fields, "headSha", nullish(a.get("headSha")));
                put(fields, "baseSha", nullish(a.get("baseSha")));
            }
            Some("test-report") => put(at(&mut revisions, n), "tests", view_tests(a)),
            Some("review-verdict") => put(
                at(&mut revisions, n),
                "review",
                obj(vec![
                    ("verdict", a.get("verdict").clone()),
                    ("summary", nullish_or(a.get("summary"), s(""))),
                    (
                        "findings",
                        if matches!(a.get("findings"), JValue::Arr(_)) {
                            a.get("findings").clone()
                        } else {
                            JValue::Arr(Vec::new())
                        },
                    ),
                    ("headSha", nullish(a.get("headSha"))),
                ]),
            ),
            Some("audit-report") => {
                audit = obj(vec![
                    ("revision", JValue::Num(n)),
                    ("headSha", nullish(a.get("headSha"))),
                    ("overall", a.get("overall").clone()),
                    (
                        "checks",
                        if a.get("checks").truthy() {
                            a.get("checks").clone()
                        } else {
                            JValue::Arr(Vec::new())
                        },
                    ),
                    (
                        "evidence",
                        if a.get("evidence").truthy() {
                            a.get("evidence").clone()
                        } else {
                            JValue::Null
                        },
                    ),
                    ("writeUp", nullish(a.get("writeUp"))),
                ]);
            }
            _ => {}
        }
    }
    for stage in stages {
        if stage.get("to").as_str() == Some("reviewing") && stage.get("reportHash").truthy() {
            if let Some(n) = stage.get("revision").as_num() {
                if revisions.iter().any(|(r, _)| *r == n) {
                    put(
                        at(&mut revisions, n),
                        "completeness",
                        obj(vec![("reportHash", stage.get("reportHash").clone())]),
                    );
                }
            }
        }
    }
    revisions.retain(|(r, _)| *r > 0.0);
    revisions.sort_by(|a, b| a.0.total_cmp(&b.0));
    let evidence = revisions
        .into_iter()
        .map(|(_, fields)| obj(fields))
        .collect();
    let max_loops = meta.get("maxLoops");
    Some(obj(vec![
        (
            "maxLoops",
            if max_loops.as_num().is_some_and(|n| n.fract() == 0.0) {
                max_loops.clone()
            } else {
                JValue::Null
            },
        ),
        (
            "merge",
            JValue::Bool(meta.get("merge") == &JValue::Bool(true)),
        ),
        ("plan", plan),
        ("evidence", JValue::Arr(evidence)),
        ("audit", audit),
    ]))
}

/// core code-service.cjs `view`; `approval` is Node's waiting card for this task, or null.
pub fn code_view(job: &Job, approval: &JValue) -> JValue {
    let cp = &job.checkpoint;
    let mut v = vec![
        ("id", job.id.clone()),
        ("status", job.status.clone()),
        ("stage", job.stage.clone()),
        ("error", job.error.clone()),
        ("task", or_null(cp.get("task"))),
        ("branch", or_null(cp.get("branch"))),
        ("baseSha", or_null(cp.get("baseSha"))),
        ("headSha", or_null(cp.get("headSha"))),
        ("meta", or_null(cp.get("meta"))),
        ("identityHash", or_null(cp.get("identityHash"))),
        ("createdAt", job.created_at.clone()),
        ("updatedAt", job.updated_at.clone()),
        ("capabilities", job.capabilities.clone()),
        ("steps", steps_of(job)),
        ("plan", job.plan.clone()),
        ("assistantOutput", job.assistant_output.clone()),
        ("approval", approval.clone()),
        ("result", or_null(&job.result)),
    ];
    if let Some(network) = network_reports(job) {
        v.push(("network", network));
    }
    if let Some(review) = &job.review {
        v.push(("review", review.clone()));
    }
    if let Some(auth) = &job.authoritative {
        v.push(("lifecycle", auth.lifecycle.clone()));
        v.push(("revision", auth.revision.clone()));
        v.push(("stages", JValue::Arr(auth.stages.clone())));
        if let Some(pipeline) = pipeline_view(job, &auth.stages) {
            v.push(("pipeline", pipeline));
        }
    }
    obj(v)
}

/// A stub (an unreadable journal) has no steps at all, as core's view of one has none.
fn steps_of(job: &Job) -> JValue {
    if job.status.as_str() == Some("unreadable") {
        JValue::Undefined
    } else {
        JValue::Arr(job.steps.clone())
    }
}

/// core browser-service.cjs `view`.
pub fn browser_view(job: &Job, approval: &JValue) -> JValue {
    let domains = job.checkpoint.get("domains");
    let mut v = vec![
        ("id", job.id.clone()),
        ("status", job.status.clone()),
        ("stage", job.stage.clone()),
        ("error", job.error.clone()),
        (
            "domains",
            if domains.truthy() {
                domains.clone()
            } else {
                JValue::Arr(Vec::new())
            },
        ),
        ("createdAt", job.created_at.clone()),
        ("updatedAt", job.updated_at.clone()),
        ("capabilities", job.capabilities.clone()),
        ("steps", steps_of(job)),
        ("approval", approval.clone()),
        ("result", or_null(&job.result)),
    ];
    if let Some(network) = network_reports(job) {
        v.push(("network", network));
    }
    v.push(("uncertain", JValue::Arr(job.uncertain.clone())));
    obj(v)
}

/// The row of `GET /api/code/active` (core routes/code.cjs).
pub fn active_row(job: &Job, project_name: &JValue, approval: &JValue) -> JValue {
    let action = approval.get("action");
    obj(vec![
        ("id", job.id.clone()),
        ("projectId", job.project_id.clone()),
        ("projectName", project_name.clone()),
        ("title", or_null(job.checkpoint.get("task"))),
        ("status", job.status.clone()),
        ("stage", job.stage.clone()),
        ("updatedAt", job.updated_at.clone()),
        ("approvalAction", or_null(action)),
    ])
}

/// core diary-extras.cjs `internalProject`: never named in the account-wide active list.
pub fn internal_project(id: &str) -> bool {
    id == "cowork-diary-extras" || id.starts_with("cowork-chat-context-")
}
