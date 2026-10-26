//! Deep-research status journals. Node executes research and saves files; Rust alone owns
//! deep_research jobs/<uuid>.jsonl. Other job kinds stay untouched.
use crate::{lock, store};
use js_json::{wtf16, JValue};
use rand_core::{OsRng, RngCore};
use serde_json::{json, Value};
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

// Match serde_json read indexing: missing keys or nonobjects read as Null.
fn at<'a>(value: &'a Value, pointer: &str) -> &'a Value {
    value.pointer(pointer).unwrap_or(&Value::Null)
}

#[derive(Clone, Copy)]
pub struct Owner<'a> {
    pub user: &'a str,
    pub start: &'a str,
    pub born: &'a str,
    pub owner: &'a str,
}

pub struct FileIntent<'a> {
    pub name: &'a str,
    pub hash: &'a str,
    pub bytes: u64,
    pub target: &'a str,
}

pub struct Artifact<'a> {
    pub name: &'a str,
    pub hash: &'a str,
    pub bytes: u64,
}

/// Paired release guard: Node research journal clients require this internal store.
pub const RESEARCH_JOBS_STORE_MARKER: &str = "noevia-research-jobs-store/1";

const RETAIN_MS: u64 = 30 * 24 * 60 * 60 * 1000;
const MAX_JOBS: usize = 50;
const MAX_ABANDONED: usize = 256;
const MAX_UNCERTAIN: usize = 256;
const MAX_JOURNAL_BYTES: u64 = 64 * 1024 * 1024;
const MAX_ROWS: usize = 10_000;
const MAX_STAGE_BYTES: usize = 4096;
const MAX_RESULT_BYTES: usize = 32 * 1024 * 1024;

#[derive(Debug)]
pub enum Error {
    BadRequest,
    Gone,
    NotFound,
    Busy,
    Stale,
    TooMany,
    Corrupt,
    Io,
}

impl From<std::io::Error> for Error {
    fn from(_: std::io::Error) -> Self {
        Self::Io
    }
}

// JSON.parse and JSON.stringify preserve escaped lone UTF-16 surrogates in research results.
// Keep their mapped representation inside serde values, and restore the original escapes at
// every journal and HTTP boundary. A plain serde parser would reject valid legacy Node rows.
fn to_serde(v: JValue) -> Result<Value, Error> {
    Ok(match v {
        JValue::Undefined | JValue::Null => Value::Null,
        JValue::Bool(v) => Value::Bool(v),
        JValue::Num(v) => {
            let number = if v.fract() == 0.0 && (0.0..=9_007_199_254_740_991.0).contains(&v) {
                serde_json::Number::from(v as u64)
            } else if v.fract() == 0.0 && (-9_007_199_254_740_991.0..0.0).contains(&v) {
                serde_json::Number::from(v as i64)
            } else {
                serde_json::Number::from_f64(v).ok_or(Error::BadRequest)?
            };
            Value::Number(number)
        }
        JValue::Str(v) => Value::String(v),
        JValue::Arr(v) => Value::Array(v.into_iter().map(to_serde).collect::<Result<_, _>>()?),
        JValue::Obj(v) => Value::Object(
            v.into_iter()
                .map(|(k, val)| Ok((k, to_serde(val)?)))
                .collect::<Result<_, Error>>()?,
        ),
    })
}
fn from_serde(v: &Value) -> JValue {
    match v {
        Value::Null => JValue::Null,
        Value::Bool(v) => JValue::Bool(*v),
        Value::Number(v) => JValue::Num(v.as_f64().unwrap_or(0.0)),
        Value::String(v) => JValue::Str(v.clone()),
        Value::Array(v) => JValue::Arr(v.iter().map(from_serde).collect()),
        Value::Object(v) => JValue::Obj(
            v.iter()
                .map(|(k, val)| (k.clone(), from_serde(val)))
                .collect(),
        ),
    }
}
pub fn parse_js(raw: &[u8]) -> Result<Value, Error> {
    let text = std::str::from_utf8(raw).map_err(|_| Error::BadRequest)?;
    to_serde(wtf16::parse(text).map_err(|_| Error::BadRequest)?)
}
pub fn stringify_js(v: &Value) -> Result<String, Error> {
    wtf16::stringify(&from_serde(v)).ok_or(Error::Corrupt)
}

#[derive(Clone, Copy)]
pub struct Context<'a> {
    pub data_dir: &'a Path,
    pub start_id: &'a str,
}

#[derive(Clone)]
struct Paths {
    user: PathBuf,
    jobs: PathBuf,
    owner: PathBuf,
}

fn plain_dir(path: &Path) -> Result<bool, Error> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_dir() => Ok(true),
        Ok(_) => Err(Error::Corrupt),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(_) => Err(Error::Io),
    }
}

fn paths(ctx: Context<'_>, user: &str) -> Result<Paths, Error> {
    let user = store::user_dir(ctx.data_dir, user).map_err(|_| Error::BadRequest)?;
    if !plain_dir(&ctx.data_dir.join("users"))? || !plain_dir(&user)? {
        return Err(Error::Gone);
    }
    let jobs = user.join("jobs");
    if jobs.exists() && !plain_dir(&jobs)? {
        return Err(Error::Corrupt);
    }
    Ok(Paths {
        owner: user.join("research-jobs-owner.json"),
        user,
        jobs,
    })
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn uuid() -> String {
    let mut b = [0u8; 16];
    OsRng.fill_bytes(&mut b);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    format!(
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        u32::from_be_bytes([b[0], b[1], b[2], b[3]]),
        u16::from_be_bytes([b[4], b[5]]),
        u16::from_be_bytes([b[6], b[7]]),
        u16::from_be_bytes([b[8], b[9]]),
        u64::from_be_bytes([0, 0, b[10], b[11], b[12], b[13], b[14], b[15]])
    )
}

fn valid_id(id: &str) -> bool {
    id.len() == 36
        && id.bytes().all(|b| {
            b.is_ascii_lowercase() && (b'a'..=b'f').contains(&b) || b.is_ascii_digit() || b == b'-'
        })
}
fn valid_start(start: &str) -> bool {
    start.len() == 32
        && start
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn valid_owner(owner: &str) -> bool {
    valid_id(owner)
}
fn valid_project(project: &str) -> bool {
    !project.is_empty() && project.len() <= 200 && !project.chars().any(char::is_control)
}
fn born_num(born: &str) -> Result<u128, Error> {
    if born.is_empty() || born.len() > 39 || !born.bytes().all(|b| b.is_ascii_digit()) {
        return Err(Error::BadRequest);
    }
    born.parse().map_err(|_| Error::BadRequest)
}

fn check_start(ctx: Context<'_>, start: &str) -> Result<(), Error> {
    if !valid_start(start) {
        return Err(Error::BadRequest);
    }
    if start != ctx.start_id {
        return Err(Error::Stale);
    }
    Ok(())
}

fn with_lock<T>(
    ctx: Context<'_>,
    user: &str,
    f: impl FnOnce(&Paths) -> Result<T, Error>,
) -> Result<T, Error> {
    let p = paths(ctx, user)?;
    let lock_path = lock::path_for(&p.owner);
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let guard = options.open(&lock_path)?;
    if !guard.metadata()?.file_type().is_file() {
        return Err(Error::Corrupt);
    }
    let deadline = Instant::now() + Duration::from_millis(lock::WAIT_MS as u64);
    loop {
        match guard.try_lock() {
            Ok(()) => break,
            Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(lock::RETRY_MS))
            }
            Err(TryLockError::WouldBlock) => return Err(Error::Busy),
            Err(TryLockError::Error(_)) => return Err(Error::Io),
        }
    }
    let _guard = guard;
    if !plain_dir(&p.user)? {
        return Err(Error::Gone);
    }
    f(&p)
}

fn read_regular(path: &Path, max: u64) -> Result<Option<Vec<u8>>, Error> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let f = match options.open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(Error::Io),
    };
    let meta = f.metadata()?;
    if !meta.file_type().is_file() || meta.len() > max {
        return Err(Error::Corrupt);
    }
    let mut raw = Vec::new();
    f.take(max + 1).read_to_end(&mut raw)?;
    if raw.len() as u64 > max {
        return Err(Error::Corrupt);
    }
    Ok(Some(raw))
}

fn read_owner(p: &Paths) -> Result<Option<Value>, Error> {
    let Some(raw) = read_regular(&p.owner, 32 * 1024)? else {
        return Ok(None);
    };
    serde_json::from_slice(&raw)
        .map(Some)
        .map_err(|_| Error::Corrupt)
}

fn write_owner(
    p: &Paths,
    start: &str,
    born: &str,
    owner: &str,
    abandoned: &[String],
) -> Result<(), Error> {
    let tmp = p.user.join(format!(".research-jobs-owner-{}", uuid()));
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options.open(&tmp)?;
    let written = (|| {
        file.write_all(
            json!({"start":start,"born":born,"owner":owner,"abandoned":abandoned})
                .to_string()
                .as_bytes(),
        )?;
        file.sync_all()?;
        fs::rename(&tmp, &p.owner)?;
        File::open(&p.user)?.sync_all()?;
        Ok::<(), Error>(())
    })();
    if written.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    written
}

fn current(p: &Paths, start: &str, born: &str, owner: &str) -> Result<(), Error> {
    let Some(v) = read_owner(p)? else {
        return Err(Error::Stale);
    };
    if at(&v, "/start") != start || at(&v, "/born") != born || at(&v, "/owner") != owner {
        return Err(Error::Stale);
    }
    Ok(())
}

fn abandoned(v: &Value) -> Result<Vec<String>, Error> {
    let Some(rows) = v.get("abandoned") else {
        return Ok(Vec::new());
    };
    let rows = rows.as_array().ok_or(Error::Corrupt)?;
    if rows.len() > MAX_ABANDONED {
        return Err(Error::Corrupt);
    }
    rows.iter()
        .map(|r| {
            let request = r.as_str().ok_or(Error::Corrupt)?;
            if !valid_id(request) {
                return Err(Error::Corrupt);
            }
            Ok(request.to_owned())
        })
        .collect()
}

fn reconcile_abandoned(
    p: &Paths,
    requests: &[String],
    all: &[(String, Vec<Value>)],
) -> Result<(), Error> {
    for (id, rows) in all {
        if terminal(status(rows)) {
            continue;
        }
        if at(rows.first().unwrap_or(&Value::Null), "/data/request")
            .as_str()
            .is_some_and(|r| requests.iter().any(|a| a == r))
        {
            if let Some(report) = rows.iter().rev().find(|r| {
                at(r, "/type") == "tool.started" && at(r, "/data/tool") == "research.report.result"
            }) {
                append(
                    p,
                    id,
                    rows,
                    if cancelled(rows) {
                        "job.cancelled"
                    } else {
                        "job.failed"
                    },
                    json!({"error":"Research finished but its save could not be confirmed. Review saved files before retrying.","result":at(report, "/data/result")}),
                )?;
            } else {
                append(
                    p,
                    id,
                    rows,
                    "job.interrupted",
                    json!({"reason":"Research start was not confirmed; start again."}),
                )?;
            }
        }
    }
    Ok(())
}

fn project_exists(p: &Paths, project: &str) -> Result<bool, Error> {
    if !valid_project(project) {
        return Err(Error::BadRequest);
    }
    let Some(raw) = read_regular(&p.user.join("projects.json"), 32 * 1024 * 1024)? else {
        return Ok(false);
    };
    let text = String::from_utf8_lossy(&raw);
    let parsed = wtf16::parse(&text).map_err(|_| Error::Corrupt)?;
    match parsed.get("projects") {
        JValue::Undefined | JValue::Null => Ok(false),
        JValue::Arr(rows) => Ok(rows
            .iter()
            .any(|row| row.get("id").as_str() == Some(project))),
        _ => Err(Error::Corrupt),
    }
}
fn job_path(p: &Paths, id: &str) -> Result<PathBuf, Error> {
    if !valid_id(id) {
        return Err(Error::NotFound);
    }
    Ok(p.jobs.join(format!("{id}.jsonl")))
}

fn read_rows(p: &Paths, id: &str) -> Result<Option<Vec<Value>>, Error> {
    let file = job_path(p, id)?;
    let Some(raw) = read_regular(&file, MAX_JOURNAL_BYTES)? else {
        return Ok(None);
    };
    let mut rows = Vec::new();
    let mut parts = raw
        .split(|b| *b == b'\n')
        .filter(|part| !part.is_empty())
        .peekable();
    while let Some(part) = parts.next() {
        if rows.len() >= MAX_ROWS {
            return Err(Error::Corrupt);
        }
        match parse_js(part) {
            Ok(row) => {
                let expected = u64::try_from(rows.len()).unwrap_or(u64::MAX) + 1;
                if at(&row, "/job") != id || at(&row, "/seq").as_u64() != Some(expected) {
                    return Err(Error::Corrupt);
                }
                rows.push(row);
            }
            Err(_) if parts.peek().is_none() && raw.last() != Some(&b'\n') => break,
            Err(_) => return Err(Error::Corrupt),
        }
    }
    if rows.is_empty() {
        return Err(Error::Corrupt);
    }
    Ok(Some(rows))
}

fn research_rows(p: &Paths, id: &str) -> Result<Option<Vec<Value>>, Error> {
    let Some(raw) = read_regular(&job_path(p, id)?, MAX_JOURNAL_BYTES)? else {
        return Ok(None);
    };
    let first = raw.split(|b| *b == b'\n').next().ok_or(Error::Corrupt)?;
    let first = parse_js(first).map_err(|_| Error::Corrupt)?;
    if at(&first, "/type") != "job.created"
        || at(&first, "/job") != id
        || at(&first, "/seq").as_u64() != Some(1)
    {
        return Err(Error::Corrupt);
    }
    if at(&first, "/data/kind") != "deep_research" {
        return Ok(None);
    }
    read_rows(p, id)
}

fn all_research(p: &Paths) -> Result<Vec<(String, Vec<Value>)>, Error> {
    if !plain_dir(&p.jobs)? {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in fs::read_dir(&p.jobs)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            return Err(Error::Corrupt);
        };
        if let Some(id) = name.strip_suffix(".jsonl") {
            if !valid_id(id) {
                return Err(Error::Corrupt);
            }
            if let Some(rows) = research_rows(p, id)? {
                out.push((id.to_string(), rows));
            }
        }
    }
    Ok(out)
}

fn status(rows: &[Value]) -> &'static str {
    let mut state = "queued";
    for row in rows {
        state = match at(row, "/type").as_str() {
            Some("job.started" | "approval.decided") => "running",
            Some("approval.requested") => "waiting_approval",
            Some("job.completed") => "completed",
            Some("job.failed") => "failed",
            Some("job.cancelled") => "cancelled",
            Some("job.interrupted") => "interrupted",
            _ => state,
        };
    }
    state
}
fn terminal(state: &str) -> bool {
    matches!(state, "completed" | "failed" | "cancelled" | "interrupted")
}

fn append(p: &Paths, id: &str, rows: &[Value], ty: &str, data: Value) -> Result<Value, Error> {
    if !plain_dir(&p.jobs)? {
        fs::create_dir(&p.jobs)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&p.jobs, fs::Permissions::from_mode(0o700))?;
        }
    }
    let file = job_path(p, id)?;
    let mut options = OpenOptions::new();
    options.read(true).write(true).append(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let mut f = options.open(file)?;
    // A legacy unchained research journal may have a torn trailing line. Cut it before appending.
    let len = f.metadata()?.len();
    if len > 0 {
        f.seek(SeekFrom::End(-1))?;
        let mut last = [0];
        f.read_exact(&mut last)?;
        if last[0] != b'\n' {
            let mut whole = Vec::new();
            f.seek(SeekFrom::Start(0))?;
            f.read_to_end(&mut whole)?;
            let keep = whole.iter().rposition(|b| *b == b'\n').map_or(0, |x| x + 1);
            // A complete JSON event lacking only its separator is already in read_rows().
            // Preserve it; only discard an invalid torn fragment.
            if parse_js(whole.get(keep..).ok_or(Error::Corrupt)?)
                .ok()
                .as_ref()
                == rows.last()
            {
                f.write_all(b"\n")?;
            } else {
                f.set_len(u64::try_from(keep).map_err(|_| Error::Corrupt)?)?;
            }
        }
    }
    let row = json!({"job":id,"seq":rows.len()+1,"type":ty,"at":now_ms(),"data":data});
    f.write_all(stringify_js(&row)?.as_bytes())?;
    f.write_all(b"\n")?;
    f.sync_all()?;
    File::open(&p.jobs)?.sync_all()?;
    Ok(row)
}

fn prune(p: &Paths, all: &[(String, Vec<Value>)]) -> Result<(), Error> {
    let mut done: Vec<_> = all
        .iter()
        .filter(|(_, rows)| terminal(status(rows)) && pending_file(rows).is_none())
        .collect();
    done.sort_by_key(|(_, rows)| {
        std::cmp::Reverse(rows.last().and_then(|r| at(r, "/at").as_u64()).unwrap_or(0))
    });
    let now = now_ms();
    for (i, (id, rows)) in done.iter().enumerate() {
        let updated = rows.last().and_then(|r| at(r, "/at").as_u64()).unwrap_or(0);
        if now.saturating_sub(updated) > RETAIN_MS || i >= MAX_JOBS {
            fs::remove_file(job_path(p, id)?)?;
        }
    }
    Ok(())
}

fn owned_rows(p: &Paths, id: &str, owner: &str, run: &str) -> Result<Vec<Value>, Error> {
    let rows = research_rows(p, id)?.ok_or(Error::NotFound)?;
    if at(rows.first().unwrap_or(&Value::Null), "/data/owner") != owner
        || at(rows.first().unwrap_or(&Value::Null), "/data/run") != run
    {
        return Err(Error::Stale);
    }
    Ok(rows)
}

fn cancelled(rows: &[Value]) -> bool {
    rows.iter()
        .any(|r| at(r, "/type") == "tool.started" && at(r, "/data/tool") == "research.cancel")
}

fn pending_file(rows: &[Value]) -> Option<Value> {
    let mut pending = None;
    for row in rows {
        if at(row, "/type") == "tool.started" && at(row, "/data/tool") == "research.report.write" {
            pending = Some(at(row, "/data").clone());
        } else if at(row, "/type") == "tool.completed"
            && at(row, "/data/tool") == "research.report.write"
        {
            if pending.as_ref().is_some_and(|p: &Value| {
                at(p, "/name") == at(row, "/data/name") && at(p, "/hash") == at(row, "/data/hash")
            }) {
                pending = None;
            }
        } else if at(row, "/type") == "artifact.created"
            && pending
                .as_ref()
                .is_some_and(|p: &Value| at(p, "/name") == at(row, "/data/name"))
        {
            pending = None;
        }
    }
    pending
}

fn finished_report(rows: &[Value]) -> bool {
    let Some(last) = rows
        .iter()
        .rev()
        .find(|r| at(r, "/type") == "job.failed" || at(r, "/type") == "job.cancelled")
    else {
        return false;
    };
    let result = at(last, "/data/result");
    if at(last, "/type") == "job.failed" {
        at(result, "/markdown").is_string() && at(result, "/sources").is_array()
    } else {
        at(result, "/sections").as_u64().unwrap_or(0) > 0
    }
}

fn snapshot(id: &str, rows: &[Value]) -> Value {
    let mut stage = Value::Null;
    let mut plan = Value::Null;
    let mut checkpoint = Value::Null;
    let mut artifacts = Vec::new();
    let mut error = Value::Null;
    let mut result = Value::Null;
    for row in rows {
        let data = at(row, "/data");
        match at(row, "/type").as_str() {
            Some("progress") => stage = at(data, "/stage").clone(),
            Some("checkpoint.created") => {
                checkpoint = data.clone();
                if let Some(obj) = checkpoint.as_object_mut() {
                    obj.remove("n");
                }
            }
            Some("plan.proposed" | "plan.edited" | "plan.skipped") => {
                plan = json!({"status":at(row, "/type").as_str().unwrap_or("").trim_start_matches("plan."), "question":at(data, "/question"), "subQuestions":at(data, "/subQuestions").as_array().cloned().unwrap_or_default()});
            }
            Some("artifact.created") => artifacts.push(data.clone()),
            Some("job.completed" | "job.cancelled") => result = at(data, "/result").clone(),
            Some("job.failed") => {
                error = at(data, "/error").clone();
                result = at(data, "/result").clone();
            }
            Some("job.interrupted") => error = at(data, "/reason").clone(),
            _ => {}
        }
    }
    json!({"id":id,"kind":"deep_research","projectId":at(rows.first().unwrap_or(&Value::Null), "/data/projectId"),
      "status":status(rows),"stage":stage,"plan":plan,"error":error,"result":result,
      "createdAt":at(rows.first().unwrap_or(&Value::Null), "/at"),"updatedAt":rows.last().map(|r| at(r, "/at")),
      "checkpoint":checkpoint,"artifacts":artifacts,"pendingFile":pending_file(rows),
      "cancelRequested":cancelled(rows)})
}

fn plan_type(mode: &str) -> Result<&'static str, Error> {
    match mode {
        "proposed" => Ok("plan.proposed"),
        "edited" => Ok("plan.edited"),
        "skipped" => Ok("plan.skipped"),
        _ => Err(Error::BadRequest),
    }
}

/// A Node boot may recover this tenant repeatedly. Only a newer process may interrupt old work.
pub fn recover(ctx: Context<'_>, identity: Owner<'_>) -> Result<usize, Error> {
    let Owner {
        user,
        start,
        born,
        owner,
    } = identity;
    check_start(ctx, start)?;
    if !valid_owner(owner) {
        return Err(Error::BadRequest);
    }
    let incoming = born_num(born)?;
    with_lock(ctx, user, |p| {
        let mut abandoned_requests = Vec::new();
        if let Some(v) = read_owner(p)? {
            let old_start = at(&v, "/start").as_str().ok_or(Error::Corrupt)?;
            let old_born = born_num(at(&v, "/born").as_str().ok_or(Error::Corrupt)?)?;
            let old_owner = at(&v, "/owner").as_str().ok_or(Error::Corrupt)?;
            if old_start == start
                && (incoming < old_born || (incoming == old_born && old_owner != owner))
            {
                return Err(Error::Stale);
            }
            if old_start == start && old_born == incoming && old_owner == owner {
                abandoned_requests = abandoned(&v)?;
            }
        }
        write_owner(p, start, born, owner, &abandoned_requests)?;
        let mut count = 0;
        for (id, rows) in all_research(p)? {
            if terminal(status(&rows)) {
                continue;
            }
            if at(rows.first().unwrap_or(&Value::Null), "/data/request")
                .as_str()
                .is_some_and(|r| abandoned_requests.iter().any(|a| a == r))
                || at(rows.first().unwrap_or(&Value::Null), "/data/owner") != owner
            {
                if let Some(report) = rows.iter().rev().find(|r| {
                    at(r, "/type") == "tool.started"
                        && at(r, "/data/tool") == "research.report.result"
                }) {
                    append(
                        p,
                        &id,
                        &rows,
                        if cancelled(&rows) {
                            "job.cancelled"
                        } else {
                            "job.failed"
                        },
                        json!({"error":"Research finished but its save could not be confirmed. Review saved files before retrying.","result":at(report, "/data/result")}),
                    )?;
                } else {
                    append(
                        p,
                        &id,
                        &rows,
                        "job.interrupted",
                        json!({"reason":"The server restarted before this finished."}),
                    )?;
                }
                count += 1;
            }
        }
        Ok(count)
    })
}

/// Atomic one-active-per-project admission; the returned run is required for every mutation.
pub fn start(
    ctx: Context<'_>,
    identity: Owner<'_>,
    project: &str,
    request: &str,
    question: &str,
    mode: &str,
    questions: &Value,
) -> Result<(String, String), Error> {
    let Owner {
        user,
        start,
        born,
        owner,
    } = identity;
    check_start(ctx, start)?;
    if !valid_id(request)
        || !valid_project(project)
        || question.is_empty()
        || question.chars().count() > 2000
        || stringify_js(questions)?.len() > 4096
    {
        return Err(Error::BadRequest);
    }
    let plan_event = plan_type(mode)?;
    if !questions.is_array()
        || (mode != "skipped" && questions.as_array().is_some_and(Vec::is_empty))
    {
        return Err(Error::BadRequest);
    }
    with_lock(ctx, user, |p| {
        current(p, start, born, owner)?;
        let owner_state = read_owner(p)?.ok_or(Error::Stale)?;
        let abandoned_requests = abandoned(&owner_state)?;
        if abandoned_requests.iter().any(|a| a == request) {
            return Err(Error::Stale);
        }
        if abandoned_requests.len() >= MAX_ABANDONED {
            return Err(Error::Busy);
        }
        let before = all_research(p)?;
        reconcile_abandoned(p, &abandoned_requests, &before)?;
        let all = all_research(p)?;
        if let Some((id, rows)) = all
            .iter()
            .find(|(_, rows)| at(rows.first().unwrap_or(&Value::Null), "/data/request") == request)
        {
            if at(rows.first().unwrap_or(&Value::Null), "/data/owner") != owner
                || at(rows.first().unwrap_or(&Value::Null), "/data/projectId") != project
                || at(rows.first().unwrap_or(&Value::Null), "/data/question") != question
                || at(rows.first().unwrap_or(&Value::Null), "/data/mode") != mode
                || at(rows.first().unwrap_or(&Value::Null), "/data/questions") != questions
            {
                return Err(Error::Stale);
            }
            if !project_exists(p, project)? {
                if !terminal(status(rows)) {
                    append(
                        p,
                        id,
                        rows,
                        "job.failed",
                        json!({"error":"Project removed before research began.","result":null}),
                    )?;
                }
                return Err(Error::NotFound);
            }
            let run = at(rows.first().unwrap_or(&Value::Null), "/data/run")
                .as_str()
                .ok_or(Error::Corrupt)?
                .to_owned();
            if status(rows) == "queued" {
                let mut next = rows.clone();
                if !next.iter().any(|r| at(r, "/type") == plan_event) {
                    append(
                        p,
                        id,
                        &next,
                        plan_event,
                        if mode == "skipped" {
                            json!({"question":question})
                        } else {
                            json!({"question":question,"subQuestions":questions})
                        },
                    )?;
                    next = research_rows(p, id)?.ok_or(Error::Corrupt)?;
                }
                append(p, id, &next, "job.started", json!({}))?;
            }
            return Ok((id.clone(), run));
        }
        if !project_exists(p, project)? {
            return Err(Error::NotFound);
        }
        if all
            .iter()
            .filter(|(_, rows)| pending_file(rows).is_some())
            .count()
            >= MAX_UNCERTAIN
        {
            return Err(Error::Busy);
        }
        if all.iter().any(|(_, rows)| {
            at(rows.first().unwrap_or(&Value::Null), "/data/projectId") == project
                && !terminal(status(rows))
        }) {
            return Err(Error::TooMany);
        }
        prune(p, &all)?;
        let id = uuid();
        let run = uuid();
        append(
            p,
            &id,
            &[],
            "job.created",
            json!({"kind":"deep_research","projectId":project,"parentId":null,"capabilities":["web.read","project.sources.read"],"owner":owner,"run":run,"request":request,"question":question,"mode":mode,"questions":questions}),
        )?;
        let rows = research_rows(p, &id)?.ok_or(Error::Corrupt)?;
        append(
            p,
            &id,
            &rows,
            plan_event,
            if mode == "skipped" {
                json!({"question":question})
            } else {
                json!({"question":question,"subQuestions":questions})
            },
        )?;
        let rows = research_rows(p, &id)?.ok_or(Error::Corrupt)?;
        append(p, &id, &rows, "job.started", json!({}))?;
        Ok((id, run))
    })
}

/// Tombstone a request before interrupting any uncertain reservation.
pub fn abandon(ctx: Context<'_>, identity: Owner<'_>, request: &str) -> Result<(), Error> {
    let Owner {
        user,
        start,
        born,
        owner,
    } = identity;
    check_start(ctx, start)?;
    if !valid_id(request) {
        return Err(Error::BadRequest);
    }
    with_lock(ctx, user, |p| {
        current(p, start, born, owner)?;
        let state = read_owner(p)?.ok_or(Error::Stale)?;
        let mut requests = abandoned(&state)?;
        if !requests.iter().any(|a| a == request) {
            if requests.len() >= MAX_ABANDONED {
                return Err(Error::Busy);
            }
            requests.push(request.to_owned());
            write_owner(p, start, born, owner, &requests)?;
        }
        let all = all_research(p)?;
        reconcile_abandoned(p, &requests, &all)
    })
}

/// Sequence numbers span both progress and checkpoints, so no progress can pass a terminal row.
pub fn event(
    ctx: Context<'_>,
    identity: Owner<'_>,
    id: &str,
    run: &str,
    n: u64,
    kind: &str,
    data: &Value,
) -> Result<(), Error> {
    let Owner {
        user,
        start,
        born,
        owner,
    } = identity;
    check_start(ctx, start)?;
    if kind != "progress" && kind != "checkpoint.created" {
        return Err(Error::BadRequest);
    }
    if stringify_js(data)?.len() > 32 * 1024 {
        return Err(Error::BadRequest);
    }
    if kind == "progress"
        && at(data, "/stage")
            .as_str()
            .is_none_or(|s| s.len() > MAX_STAGE_BYTES)
    {
        return Err(Error::BadRequest);
    }
    with_lock(ctx, user, |p| {
        current(p, start, born, owner)?;
        let rows = owned_rows(p, id, owner, run)?;
        if terminal(status(&rows)) {
            return Err(Error::Stale);
        }
        let previous = rows
            .iter()
            .rev()
            .find(|r| at(r, "/type") == "progress" || at(r, "/type") == "checkpoint.created");
        let last = previous
            .and_then(|r| at(r, "/data/n").as_u64())
            .unwrap_or(0);
        let mut stored = data.clone();
        stored
            .as_object_mut()
            .ok_or(Error::BadRequest)?
            .insert("n".to_owned(), json!(n));
        if n == last
            && previous.is_some_and(|r| at(r, "/type") == kind && at(r, "/data") == &stored)
        {
            return Ok(());
        }
        if n != last + 1 {
            return Err(Error::Stale);
        }
        append(p, id, &rows, kind, stored)?;
        Ok(())
    })
}

/// Persist cancellation intent without throwing away completed sections. Node then aborts its
/// local controller; finish writes the partial result as job.cancelled after the runner settles.
pub fn cancel(
    ctx: Context<'_>,
    identity: Owner<'_>,
    project: &str,
    id: &str,
    run: &str,
) -> Result<Value, Error> {
    let Owner {
        user,
        start,
        born,
        owner,
    } = identity;
    check_start(ctx, start)?;
    with_lock(ctx, user, |p| {
        current(p, start, born, owner)?;
        if !project_exists(p, project)? {
            return Err(Error::NotFound);
        }
        let rows = research_rows(p, id)?.ok_or(Error::NotFound)?;
        if at(rows.first().unwrap_or(&Value::Null), "/data/projectId") != project {
            return Err(Error::NotFound);
        }
        if terminal(status(&rows)) {
            return Ok(snapshot(id, &rows));
        }
        if at(rows.first().unwrap_or(&Value::Null), "/data/owner") != owner
            || at(rows.first().unwrap_or(&Value::Null), "/data/run") != run
        {
            return Err(Error::Stale);
        }
        if cancelled(&rows) {
            return Ok(snapshot(id, &rows));
        }
        append(
            p,
            id,
            &rows,
            "tool.started",
            json!({"tool":"research.cancel"}),
        )?;
        let rows = research_rows(p, id)?.ok_or(Error::Corrupt)?;
        Ok(snapshot(id, &rows))
    })
}

/// Terminal retry is idempotent. A committed cancellation wins over completion.
pub fn finish(
    ctx: Context<'_>,
    identity: Owner<'_>,
    id: &str,
    run: &str,
    ok: bool,
    result: &Value,
    error: Option<&str>,
) -> Result<(), Error> {
    let Owner {
        user,
        start,
        born,
        owner,
    } = identity;
    check_start(ctx, start)?;
    if stringify_js(result)?.len() > MAX_RESULT_BYTES {
        return Err(Error::BadRequest);
    }
    with_lock(ctx, user, |p| {
        current(p, start, born, owner)?;
        let rows = owned_rows(p, id, owner, run)?;
        let ty = if cancelled(&rows) {
            "job.cancelled"
        } else if ok {
            "job.completed"
        } else {
            "job.failed"
        };
        if ty == "job.completed" && pending_file(&rows).is_some() {
            return Err(Error::Stale);
        }
        let data = if ty == "job.failed" {
            json!({"error":error.unwrap_or("Research failed."),"result":result})
        } else if result.is_null() {
            json!({})
        } else {
            json!({"result":result})
        };
        if terminal(status(&rows)) {
            return if rows
                .iter()
                .rev()
                .find(|r| {
                    matches!(
                        at(r, "/type").as_str(),
                        Some("job.completed" | "job.failed" | "job.cancelled" | "job.interrupted")
                    )
                })
                .is_some_and(|r| at(r, "/type") == ty && at(r, "/data") == &data)
            {
                Ok(())
            } else {
                Err(Error::Stale)
            };
        }
        append(p, id, &rows, ty, data)?;
        Ok(())
    })
}

fn file_rows(
    p: &Paths,
    id: &str,
    owner: &str,
    run: &str,
    project: &str,
) -> Result<Vec<Value>, Error> {
    let rows = research_rows(p, id)?.ok_or(Error::NotFound)?;
    if at(rows.first().unwrap_or(&Value::Null), "/data/projectId") != project
        || !project_exists(p, project)?
    {
        return Err(Error::NotFound);
    }
    if !terminal(status(&rows))
        && (at(rows.first().unwrap_or(&Value::Null), "/data/owner") != owner
            || at(rows.first().unwrap_or(&Value::Null), "/data/run") != run)
    {
        return Err(Error::Stale);
    }
    Ok(rows)
}

fn valid_hash(hash: &str) -> bool {
    hash.len() == 64
        && hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn valid_file(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 240
        && !name.contains('/')
        && !name.contains('\\')
        && !name.chars().any(char::is_control)
        && (name.ends_with(".md") || name.ends_with(".sources.json"))
}
fn save_allowed(rows: &[Value]) -> bool {
    // The sidecar is the final file of a report. Once recorded, no further report writes
    // may be prepared, even when a failed/cancelled terminal result remains retryable.
    if rows.iter().any(|r| {
        at(r, "/type") == "artifact.created"
            && at(r, "/data/name")
                .as_str()
                .is_some_and(|n| n.ends_with(".sources.json"))
    }) {
        return false;
    }
    !terminal(status(rows)) || finished_report(rows)
}

/// Save the complete report in the journal before any external project-file write. If the
/// final terminal response is lost, recovery can still retain the result for safe reconciliation.
pub fn report(
    ctx: Context<'_>,
    identity: Owner<'_>,
    id: &str,
    run: &str,
    result: &Value,
) -> Result<(), Error> {
    let Owner {
        user,
        start,
        born,
        owner,
    } = identity;
    check_start(ctx, start)?;
    if stringify_js(result)?.len() > 24 * 1024 * 1024
        || !at(result, "/markdown").is_string()
        || !at(result, "/sources").is_array()
    {
        return Err(Error::BadRequest);
    }
    with_lock(ctx, user, |p| {
        current(p, start, born, owner)?;
        let rows = owned_rows(p, id, owner, run)?;
        if terminal(status(&rows)) {
            return Err(Error::Stale);
        }
        if let Some(prior) = rows.iter().find(|r| {
            at(r, "/type") == "tool.started" && at(r, "/data/tool") == "research.report.result"
        }) {
            return if at(prior, "/data/result") == result {
                Ok(())
            } else {
                Err(Error::Stale)
            };
        }
        append(
            p,
            id,
            &rows,
            "tool.started",
            json!({"tool":"research.report.result","result":result}),
        )?;
        Ok(())
    })
}

/// Durable intent BEFORE Node calls a potentially ambiguous external file writer.
pub fn prepare_file(
    ctx: Context<'_>,
    identity: Owner<'_>,
    project: &str,
    id: &str,
    run: &str,
    file: FileIntent<'_>,
) -> Result<(), Error> {
    let FileIntent {
        name,
        hash,
        bytes,
        target,
    } = file;
    let Owner {
        user,
        start,
        born,
        owner,
    } = identity;
    check_start(ctx, start)?;
    if !valid_file(name) || !valid_hash(hash) || !valid_hash(target) || bytes > 25 * 1024 * 1024 {
        return Err(Error::BadRequest);
    }
    with_lock(ctx, user, |p| {
        current(p, start, born, owner)?;
        let rows = file_rows(p, id, owner, run, project)?;
        if !save_allowed(&rows) {
            return Err(Error::Stale);
        }
        if rows
            .iter()
            .any(|r| at(r, "/type") == "artifact.created" && at(r, "/data/name") == name)
        {
            return Err(Error::Stale);
        }
        let data = json!({"tool":"research.report.write","name":name,"hash":hash,"bytes":bytes,"target":target});
        if let Some(pending) = pending_file(&rows) {
            return if pending == data {
                Ok(())
            } else {
                Err(Error::Stale)
            };
        }
        append(p, id, &rows, "tool.started", data)?;
        Ok(())
    })
}

/// Used only after positive file-write success or exact file-content reconciliation. It may
/// complete a cancelled/failed report's explicit partial save; never invents file existence.
pub fn artifact(
    ctx: Context<'_>,
    identity: Owner<'_>,
    project: &str,
    id: &str,
    run: &str,
    file: Artifact<'_>,
) -> Result<(), Error> {
    let Artifact { name, hash, bytes } = file;
    let Owner {
        user,
        start,
        born,
        owner,
    } = identity;
    check_start(ctx, start)?;
    with_lock(ctx, user, |p| {
        current(p, start, born, owner)?;
        let rows = file_rows(p, id, owner, run, project)?;
        if rows.iter().any(|r| {
            at(r, "/type") == "artifact.created"
                && at(r, "/data/name") == name
                && at(r, "/data/bytes") == bytes
                && at(r, "/data/hash") == hash
        }) {
            return Ok(());
        }
        if !save_allowed(&rows) {
            return Err(Error::Stale);
        }
        let Some(intent) = pending_file(&rows) else {
            return Err(Error::Stale);
        };
        if at(&intent, "/name") != name
            || at(&intent, "/hash") != hash
            || at(&intent, "/bytes") != bytes
        {
            return Err(Error::Stale);
        }
        append(
            p,
            id,
            &rows,
            "artifact.created",
            json!({"name":name,"bytes":bytes,"hash":hash}),
        )?;
        Ok(())
    })
}

/// Only a caller that has proved NO external write happened may clear an intent for retry.
pub fn clear_file(
    ctx: Context<'_>,
    identity: Owner<'_>,
    project: &str,
    id: &str,
    run: &str,
    name: &str,
    hash: &str,
) -> Result<(), Error> {
    let Owner {
        user,
        start,
        born,
        owner,
    } = identity;
    check_start(ctx, start)?;
    with_lock(ctx, user, |p| {
        current(p, start, born, owner)?;
        let rows = file_rows(p, id, owner, run, project)?;
        let Some(intent) = pending_file(&rows) else {
            return Ok(());
        };
        if at(&intent, "/name") != name || at(&intent, "/hash") != hash {
            return Err(Error::Stale);
        }
        append(
            p,
            id,
            &rows,
            "tool.completed",
            json!({"tool":"research.report.write","name":name,"hash":hash,"notWritten":true}),
        )?;
        Ok(())
    })
}

pub fn read(ctx: Context<'_>, user: &str, project: &str, id: &str) -> Result<Option<Value>, Error> {
    if !valid_project(project) || !valid_id(id) {
        return Ok(None);
    }
    with_lock(ctx, user, |p| {
        if !project_exists(p, project)? {
            return Ok(None);
        }
        let Some(rows) = research_rows(p, id)? else {
            return Ok(None);
        };
        if at(rows.first().unwrap_or(&Value::Null), "/data/projectId") != project {
            return Ok(None);
        }
        Ok(Some(snapshot(id, &rows)))
    })
}

pub fn list(ctx: Context<'_>, user: &str, project: &str) -> Result<Vec<Value>, Error> {
    if !valid_project(project) {
        return Ok(Vec::new());
    }
    with_lock(ctx, user, |p| {
        if !project_exists(p, project)? {
            return Ok(Vec::new());
        }
        let mut all = all_research(p)?;
        all.retain(|(_, rows)| {
            at(rows.first().unwrap_or(&Value::Null), "/data/projectId") == project
        });
        all.sort_by_key(|(_, rows)| {
            std::cmp::Reverse(
                at(rows.first().unwrap_or(&Value::Null), "/at")
                    .as_u64()
                    .unwrap_or(0),
            )
        });
        Ok(all
            .into_iter()
            .take(20)
            .map(|(id, rows)| snapshot(&id, &rows))
            .collect())
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    const USER: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    const START: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const OWNER1: &str = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
    const OWNER2: &str = "cccccccc-cccc-4ccc-8ccc-cccccccccccc";
    const REQ1: &str = "11111111-1111-4111-8111-111111111111";
    const REQ2: &str = "22222222-2222-4222-8222-222222222222";
    const REQ3: &str = "33333333-3333-4333-8333-333333333333";
    const HASH1: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const HASH2: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const TARGET1: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
    fn fixture() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let user = dir.path().join("users").join(USER);
        fs::create_dir_all(&user).unwrap();
        fs::write(
            user.join("projects.json"),
            r#"{"projects":[{"id":"a"},{"id":"b","title":"\uD800"}]}"#,
        )
        .unwrap();
        (dir, user)
    }
    fn ctx(dir: &Path) -> Context<'_> {
        Context {
            data_dir: dir,
            start_id: START,
        }
    }
    fn begin(c: Context<'_>, project: &str, request: &str) -> (String, String) {
        start(
            c,
            Owner {
                user: USER,
                start: START,
                born: "100",
                owner: OWNER1,
            },
            project,
            request,
            "Question?",
            "skipped",
            &json!([]),
        )
        .unwrap()
    }
    #[test]
    fn atomic_project_admission_idempotent_start_and_abandon_recovery() {
        let (dir, user) = fixture();
        let c = ctx(dir.path());
        recover(
            c,
            Owner {
                user: USER,
                start: START,
                born: "100",
                owner: OWNER1,
            },
        )
        .unwrap();
        let first = begin(c, "a", REQ1);
        assert_eq!(first, begin(c, "a", REQ1));
        assert!(matches!(
            start(
                c,
                Owner {
                    user: USER,
                    start: START,
                    born: "100",
                    owner: OWNER1
                },
                "a",
                REQ2,
                "Question?",
                "skipped",
                &json!([])
            ),
            Err(Error::TooMany)
        ));
        assert!(begin(c, "b", REQ3).0 != first.0);
        abandon(
            c,
            Owner {
                user: USER,
                start: START,
                born: "100",
                owner: OWNER1,
            },
            REQ1,
        )
        .unwrap();
        assert_eq!(
            read(c, USER, "a", &first.0)
                .unwrap()
                .unwrap()
                .pointer("/status")
                .unwrap()
                .clone(),
            "interrupted"
        );
        assert!(matches!(
            start(
                c,
                Owner {
                    user: USER,
                    start: START,
                    born: "100",
                    owner: OWNER1
                },
                "a",
                REQ1,
                "Question?",
                "skipped",
                &json!([])
            ),
            Err(Error::Stale)
        ));
        assert!(begin(c, "a", REQ2).0 != first.0);
        assert_eq!(fs::read_dir(user.join("jobs")).unwrap().count(), 3);
    }
    #[test]
    fn ordered_events_cancel_intent_partial_result_and_new_owner_fence() {
        let (dir, _) = fixture();
        let c = ctx(dir.path());
        recover(
            c,
            Owner {
                user: USER,
                start: START,
                born: "100",
                owner: OWNER1,
            },
        )
        .unwrap();
        let (id, run) = begin(c, "a", REQ1);
        event(
            c,
            Owner {
                user: USER,
                start: START,
                born: "100",
                owner: OWNER1,
            },
            &id,
            &run,
            1,
            "progress",
            &json!({"stage":"researching"}),
        )
        .unwrap();
        event(
            c,
            Owner {
                user: USER,
                start: START,
                born: "100",
                owner: OWNER1,
            },
            &id,
            &run,
            2,
            "checkpoint.created",
            &json!({"step":1,"question":"Q"}),
        )
        .unwrap();
        event(
            c,
            Owner {
                user: USER,
                start: START,
                born: "100",
                owner: OWNER1,
            },
            &id,
            &run,
            2,
            "checkpoint.created",
            &json!({"step":1,"question":"Q"}),
        )
        .unwrap();
        assert!(matches!(
            event(
                c,
                Owner {
                    user: USER,
                    start: START,
                    born: "100",
                    owner: OWNER1
                },
                &id,
                &run,
                4,
                "progress",
                &json!({"stage":"late"})
            ),
            Err(Error::Stale)
        ));
        let after = cancel(
            c,
            Owner {
                user: USER,
                start: START,
                born: "100",
                owner: OWNER1,
            },
            "a",
            &id,
            &run,
        )
        .unwrap();
        assert_eq!(after.pointer("/status").unwrap().clone(), "running");
        assert_eq!(after.pointer("/cancelRequested").unwrap().clone(), true);
        let partial = json!({"markdown":"# Q","sources":[],"sections":1,"question":"Q"});
        finish(
            c,
            Owner {
                user: USER,
                start: START,
                born: "100",
                owner: OWNER1,
            },
            &id,
            &run,
            true,
            &partial,
            None,
        )
        .unwrap();
        let job = read(c, USER, "a", &id).unwrap().unwrap();
        assert_eq!(job.pointer("/status").unwrap().clone(), "cancelled");
        assert_eq!(job.pointer("/result").unwrap().clone(), partial);
        assert_eq!(job.pointer("/checkpoint/step").unwrap().clone(), 1);
        recover(
            c,
            Owner {
                user: USER,
                start: START,
                born: "101",
                owner: OWNER2,
            },
        )
        .unwrap();
        assert!(matches!(
            event(
                c,
                Owner {
                    user: USER,
                    start: START,
                    born: "100",
                    owner: OWNER1
                },
                &id,
                &run,
                3,
                "progress",
                &json!({"stage":"late"})
            ),
            Err(Error::Stale)
        ));
        assert_eq!(
            cancel(
                c,
                Owner {
                    user: USER,
                    start: START,
                    born: "101",
                    owner: OWNER2
                },
                "a",
                &id,
                ""
            )
            .unwrap()
            .pointer("/status")
            .unwrap()
            .clone(),
            "cancelled"
        );
    }
    #[test]
    fn prepared_file_is_not_pruned_or_overwritten_and_hash_binds_artifact() {
        let (dir, user) = fixture();
        let c = ctx(dir.path());
        recover(
            c,
            Owner {
                user: USER,
                start: START,
                born: "100",
                owner: OWNER1,
            },
        )
        .unwrap();
        let (id, run) = begin(c, "a", REQ1);
        prepare_file(
            c,
            Owner {
                user: USER,
                start: START,
                born: "100",
                owner: OWNER1,
            },
            "a",
            &id,
            &run,
            FileIntent {
                name: "Research a.md",
                hash: HASH1,
                bytes: 5,
                target: TARGET1,
            },
        )
        .unwrap();
        assert!(
            matches!(
                prepare_file(
                    c,
                    Owner {
                        user: USER,
                        start: START,
                        born: "100",
                        owner: OWNER1
                    },
                    "a",
                    &id,
                    &run,
                    FileIntent {
                        name: "Research a.md",
                        hash: HASH1,
                        bytes: 5,
                        target: HASH2
                    }
                ),
                Err(Error::Stale)
            ),
            "an exact-key retry cannot redirect a prepared file to another target"
        );
        let result = json!({"markdown":"# Q","sources":[],"sections":1});
        finish(
            c,
            Owner {
                user: USER,
                start: START,
                born: "100",
                owner: OWNER1,
            },
            &id,
            &run,
            false,
            &result,
            Some("file outcome unknown"),
        )
        .unwrap();
        assert!(read(c, USER, "a", &id)
            .unwrap()
            .unwrap()
            .pointer("/pendingFile")
            .unwrap()
            .clone()
            .is_object());
        assert!(matches!(
            prepare_file(
                c,
                Owner {
                    user: USER,
                    start: START,
                    born: "100",
                    owner: OWNER1
                },
                "a",
                &id,
                &run,
                FileIntent {
                    name: "Research a.sources.json",
                    hash: HASH2,
                    bytes: 5,
                    target: TARGET1
                }
            ),
            Err(Error::Stale)
        ));
        for i in 0..51 {
            let request = format!("00000000-0000-4000-8000-{i:012x}");
            let (other, token) = begin(c, "b", &request);
            finish(
                c,
                Owner {
                    user: USER,
                    start: START,
                    born: "100",
                    owner: OWNER1,
                },
                &other,
                &token,
                true,
                &json!({"done":true}),
                None,
            )
            .unwrap();
        }
        let _ = begin(c, "b", REQ2); // pruning only resolved terminals
        assert!(user.join("jobs").join(format!("{id}.jsonl")).exists());
        artifact(
            c,
            Owner {
                user: USER,
                start: START,
                born: "100",
                owner: OWNER1,
            },
            "a",
            &id,
            &run,
            Artifact {
                name: "Research a.md",
                hash: HASH1,
                bytes: 5,
            },
        )
        .unwrap();
        assert!(matches!(
            artifact(
                c,
                Owner {
                    user: USER,
                    start: START,
                    born: "100",
                    owner: OWNER1
                },
                "a",
                &id,
                &run,
                Artifact {
                    name: "Research a.md",
                    hash: HASH2,
                    bytes: 5
                }
            ),
            Err(Error::Stale)
        ));
        assert!(read(c, USER, "a", &id)
            .unwrap()
            .unwrap()
            .pointer("/pendingFile")
            .unwrap()
            .clone()
            .is_null());
    }
    #[test]
    fn cancelled_report_survives_lost_terminal_and_new_owner_recovery() {
        let (dir, _) = fixture();
        let c = ctx(dir.path());
        recover(
            c,
            Owner {
                user: USER,
                start: START,
                born: "100",
                owner: OWNER1,
            },
        )
        .unwrap();
        let (id, run) = begin(c, "a", REQ1);
        cancel(
            c,
            Owner {
                user: USER,
                start: START,
                born: "100",
                owner: OWNER1,
            },
            "a",
            &id,
            &run,
        )
        .unwrap();
        let result = json!({"question":"Q?","markdown":"# partial","sources":[],"sections":1});
        report(
            c,
            Owner {
                user: USER,
                start: START,
                born: "100",
                owner: OWNER1,
            },
            &id,
            &run,
            &result,
        )
        .unwrap();
        // No terminal answer was committed; a newer owner must retain the user's cancel.
        assert_eq!(
            recover(
                c,
                Owner {
                    user: USER,
                    start: START,
                    born: "101",
                    owner: OWNER2
                }
            )
            .unwrap(),
            1
        );
        let saved = read(c, USER, "a", &id).unwrap().unwrap();
        assert_eq!(saved.pointer("/status").unwrap().clone(), "cancelled");
        assert_eq!(saved.pointer("/result").unwrap().clone(), result);
        assert!(saved
            .pointer("/artifacts")
            .unwrap()
            .clone()
            .as_array()
            .unwrap()
            .is_empty());
    }
    #[test]
    fn report_survives_two_file_save_outage_and_recovery_without_blind_rewrite() {
        let (dir, _) = fixture();
        let c = ctx(dir.path());
        recover(
            c,
            Owner {
                user: USER,
                start: START,
                born: "100",
                owner: OWNER1,
            },
        )
        .unwrap();
        let (id, run) = begin(c, "a", REQ1);
        let result = json!({"question":"Q?","markdown":"# Q","sources":[],"sections":1});
        report(
            c,
            Owner {
                user: USER,
                start: START,
                born: "100",
                owner: OWNER1,
            },
            &id,
            &run,
            &result,
        )
        .unwrap();
        report(
            c,
            Owner {
                user: USER,
                start: START,
                born: "100",
                owner: OWNER1,
            },
            &id,
            &run,
            &result,
        )
        .unwrap();
        prepare_file(
            c,
            Owner {
                user: USER,
                start: START,
                born: "100",
                owner: OWNER1,
            },
            "a",
            &id,
            &run,
            FileIntent {
                name: "Research a.md",
                hash: HASH1,
                bytes: 5,
                target: TARGET1,
            },
        )
        .unwrap();
        artifact(
            c,
            Owner {
                user: USER,
                start: START,
                born: "100",
                owner: OWNER1,
            },
            "a",
            &id,
            &run,
            Artifact {
                name: "Research a.md",
                hash: HASH1,
                bytes: 5,
            },
        )
        .unwrap();
        prepare_file(
            c,
            Owner {
                user: USER,
                start: START,
                born: "100",
                owner: OWNER1,
            },
            "a",
            &id,
            &run,
            FileIntent {
                name: "Research a.sources.json",
                hash: HASH2,
                bytes: 5,
                target: TARGET1,
            },
        )
        .unwrap();
        // Rust becomes unavailable before file outcome or terminal response can be recorded.
        assert_eq!(
            recover(
                c,
                Owner {
                    user: USER,
                    start: START,
                    born: "101",
                    owner: OWNER2
                }
            )
            .unwrap(),
            1
        );
        let failed = read(c, USER, "a", &id).unwrap().unwrap();
        assert_eq!(failed.pointer("/status").unwrap().clone(), "failed");
        assert_eq!(failed.pointer("/result").unwrap().clone(), result);
        assert_eq!(
            failed
                .pointer("/artifacts")
                .unwrap()
                .clone()
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            failed.pointer("/pendingFile/name").unwrap().clone(),
            "Research a.sources.json"
        );
        // The next process may acknowledge only the proven exact file, never issue another PUT.
        artifact(
            c,
            Owner {
                user: USER,
                start: START,
                born: "101",
                owner: OWNER2,
            },
            "a",
            &id,
            "",
            Artifact {
                name: "Research a.sources.json",
                hash: HASH2,
                bytes: 5,
            },
        )
        .unwrap();
        let saved = read(c, USER, "a", &id).unwrap().unwrap();
        assert!(saved.pointer("/pendingFile").unwrap().clone().is_null());
        assert_eq!(
            saved
                .pointer("/artifacts")
                .unwrap()
                .clone()
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert!(matches!(
            prepare_file(
                c,
                Owner {
                    user: USER,
                    start: START,
                    born: "101",
                    owner: OWNER2
                },
                "a",
                &id,
                "",
                FileIntent {
                    name: "Other.sources.json",
                    hash: HASH2,
                    bytes: 5,
                    target: TARGET1
                }
            ),
            Err(Error::Stale)
        ));
    }
    #[test]
    fn late_artifact_does_not_break_exact_terminal_retry_or_cross_boot_save() {
        let (dir, _) = fixture();
        let c = ctx(dir.path());
        recover(
            c,
            Owner {
                user: USER,
                start: START,
                born: "100",
                owner: OWNER1,
            },
        )
        .unwrap();
        let (id, run) = begin(c, "a", REQ1);
        let result = json!({"markdown":"# Q","sources":[],"sections":1});
        finish(
            c,
            Owner {
                user: USER,
                start: START,
                born: "100",
                owner: OWNER1,
            },
            &id,
            &run,
            false,
            &result,
            Some("save failed"),
        )
        .unwrap();
        recover(
            c,
            Owner {
                user: USER,
                start: START,
                born: "101",
                owner: OWNER2,
            },
        )
        .unwrap();
        prepare_file(
            c,
            Owner {
                user: USER,
                start: START,
                born: "101",
                owner: OWNER2,
            },
            "a",
            &id,
            "",
            FileIntent {
                name: "Research a.md",
                hash: HASH1,
                bytes: 5,
                target: TARGET1,
            },
        )
        .unwrap();
        artifact(
            c,
            Owner {
                user: USER,
                start: START,
                born: "101",
                owner: OWNER2,
            },
            "a",
            &id,
            "",
            Artifact {
                name: "Research a.md",
                hash: HASH1,
                bytes: 5,
            },
        )
        .unwrap();
        assert_eq!(
            read(c, USER, "a", &id)
                .unwrap()
                .unwrap()
                .pointer("/artifacts/0/hash")
                .unwrap()
                .clone(),
            HASH1
        );
        assert!(matches!(
            finish(
                c,
                Owner {
                    user: USER,
                    start: START,
                    born: "100",
                    owner: OWNER1
                },
                &id,
                &run,
                false,
                &result,
                Some("save failed")
            ),
            Err(Error::Stale)
        ));
        // Same owner exact delayed reply after late artifact is accepted.
        let (second, run2) = start(
            c,
            Owner {
                user: USER,
                start: START,
                born: "101",
                owner: OWNER2,
            },
            "b",
            REQ2,
            "Q?",
            "skipped",
            &json!([]),
        )
        .unwrap();
        let report = json!({"markdown":"# Q","sources":[],"sections":1});
        finish(
            c,
            Owner {
                user: USER,
                start: START,
                born: "101",
                owner: OWNER2,
            },
            &second,
            &run2,
            false,
            &report,
            Some("save failed"),
        )
        .unwrap();
        prepare_file(
            c,
            Owner {
                user: USER,
                start: START,
                born: "101",
                owner: OWNER2,
            },
            "b",
            &second,
            &run2,
            FileIntent {
                name: "Research b.md",
                hash: HASH1,
                bytes: 5,
                target: TARGET1,
            },
        )
        .unwrap();
        artifact(
            c,
            Owner {
                user: USER,
                start: START,
                born: "101",
                owner: OWNER2,
            },
            "b",
            &second,
            &run2,
            Artifact {
                name: "Research b.md",
                hash: HASH1,
                bytes: 5,
            },
        )
        .unwrap();
        finish(
            c,
            Owner {
                user: USER,
                start: START,
                born: "101",
                owner: OWNER2,
            },
            &second,
            &run2,
            false,
            &report,
            Some("save failed"),
        )
        .unwrap();
    }
    #[test]
    fn node_legacy_and_rust_journals_cross_replay_when_core_is_available() {
        use std::process::Command;
        let (Ok(core), Ok(devrun)) = (
            std::env::var("NOEVIA_CORE_DIR"),
            std::env::var("NOEVIA_DEVRUN"),
        ) else {
            return;
        };
        let (dir, user) = fixture();
        let module = Path::new(&core).join("server/jobs.cjs");
        let script = r#"const {createJobs}=require(process.argv[1]);const j=createJobs({dir:process.argv[2],kinds:['deep_research']});const id=j.create({kind:'deep_research',projectId:'a',capabilities:['web.read','project.sources.read']});j.append(id,'plan.skipped',{question:'\uD800',subQuestions:[]});j.append(id,'job.started');j.append(id,'job.completed',{result:{markdown:'\uD800',sources:[],sections:1}});process.stdout.write(id);"#;
        let out = Command::new(&devrun)
            .args(["node", "-e", script])
            .arg(&module)
            .arg(&user)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let legacy = String::from_utf8(out.stdout).unwrap();
        assert!(
            stringify_js(&read(ctx(dir.path()), USER, "a", &legacy).unwrap().unwrap())
                .unwrap()
                .contains(r"\ud800")
        );
        let c = ctx(dir.path());
        recover(
            c,
            Owner {
                user: USER,
                start: START,
                born: "100",
                owner: OWNER1,
            },
        )
        .unwrap();
        let (id, run) = begin(c, "a", REQ1);
        finish(
            c,
            Owner {
                user: USER,
                start: START,
                born: "100",
                owner: OWNER1,
            },
            &id,
            &run,
            true,
            &parse_js(br#"{"markdown":"\ud800","sources":[],"sections":1}"#).unwrap(),
            None,
        )
        .unwrap();
        let script = r#"const {createJobs}=require(process.argv[1]);const j=createJobs({dir:process.argv[2],kinds:['deep_research']});const x=j.get(process.argv[3]);if(x.status!=='completed'||JSON.stringify(x.result.markdown)!=='"\\ud800"'||!j.list({strict:true}).some(y=>y.id===x.id))process.exit(1);"#;
        let out = Command::new(&devrun)
            .args(["node", "-e", script])
            .arg(&module)
            .arg(&user)
            .arg(&id)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}
