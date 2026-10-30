//! Source-processing status journals: the front is their one owning process (sbstndalton/noevia#2191
//! round 6, [`crate::source_work`]); Node keeps no client. Rust is the sole writer of source-kind
//! jobs/<uuid>.jsonl. Other job kinds stay untouched.
use crate::{lock, store};
use js_json::{wtf16, JValue};
use rand_core::{OsRng, RngCore};
use serde_json::{json, Value};
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const RETAIN_MS: u64 = 15 * 60 * 1000;
const MAX_JOBS: usize = 16;
const MAX_ACTIVE: usize = 2;
/// The most request tombstones kept: past it [`abandon`] drops the oldest, so the list can never
/// refuse a start. The front itself writes none for a refusal it saw (it ends the job with
/// [`interrupt`]); a tombstone is only for a start reply that was lost.
const MAX_ABANDONED: usize = 256;
/// A job that has written no row for this long is not running (an operation that waits for a
/// sidecar for minutes still writes its stages); the next start interrupts it, so a task that was
/// lost can never hold one of the two slots until a restart.
const ACTIVE_STALE_MS: u64 = 6 * 60 * 60 * 1000;
const MAX_JOURNAL_BYTES: u64 = 64 * 1024 * 1024;
const MAX_ROWS: usize = 10_000;
const MAX_STAGE_BYTES: usize = 300;
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

// JSON.parse and JSON.stringify preserve escaped lone UTF-16 surrogates in source results.
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
        owner: user.join("source-jobs-owner.json"),
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

/// A fresh random v4 UUID (a job id, a request key, an owner id).
pub fn new_id() -> String {
    uuid()
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
    let tmp = p.user.join(format!(".source-jobs-owner-{}", uuid()));
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
    if v.pointer("/start").unwrap_or(&Value::Null) != start
        || v.pointer("/born").unwrap_or(&Value::Null) != born
        || v.pointer("/owner").unwrap_or(&Value::Null) != owner
    {
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
        if rows
            .first()
            .and_then(|row| row.pointer("/data/request"))
            .unwrap_or(&Value::Null)
            .as_str()
            .is_some_and(|r| requests.iter().any(|a| a == r))
        {
            append(
                p,
                id,
                rows,
                "job.interrupted",
                json!({"reason":"Source processing was not confirmed; refresh or re-upload to retry."}),
            )?;
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
                if row.pointer("/job").unwrap_or(&Value::Null) != id
                    || row.pointer("/seq").unwrap_or(&Value::Null).as_u64() != Some(expected)
                {
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

fn source_rows(p: &Paths, id: &str) -> Result<Option<Vec<Value>>, Error> {
    let Some(raw) = read_regular(&job_path(p, id)?, MAX_JOURNAL_BYTES)? else {
        return Ok(None);
    };
    let first = raw.split(|b| *b == b'\n').next().ok_or(Error::Corrupt)?;
    let first = parse_js(first).map_err(|_| Error::Corrupt)?;
    if first.pointer("/type").unwrap_or(&Value::Null) != "job.created"
        || first.pointer("/job").unwrap_or(&Value::Null) != id
        || first.pointer("/seq").unwrap_or(&Value::Null).as_u64() != Some(1)
    {
        return Err(Error::Corrupt);
    }
    if first.pointer("/data/kind").unwrap_or(&Value::Null) != "source" {
        return Ok(None);
    }
    read_rows(p, id)
}

fn all_source(p: &Paths) -> Result<Vec<(String, Vec<Value>)>, Error> {
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
            if let Some(rows) = source_rows(p, id)? {
                out.push((id.to_string(), rows));
            }
        }
    }
    Ok(out)
}

fn status(rows: &[Value]) -> &'static str {
    let mut state = "queued";
    for row in rows {
        state = match row.pointer("/type").unwrap_or(&Value::Null).as_str() {
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
    // A legacy unchained source journal may have a torn trailing line. Cut it before appending.
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
        .filter(|(_, rows)| terminal(status(rows)))
        .collect();
    done.sort_by_key(|(_, rows)| {
        std::cmp::Reverse(
            rows.last()
                .and_then(|r| r.pointer("/at").unwrap_or(&Value::Null).as_u64())
                .unwrap_or(0),
        )
    });
    let now = now_ms();
    for (i, (id, rows)) in done.iter().enumerate() {
        let updated = rows
            .last()
            .and_then(|r| r.pointer("/at").unwrap_or(&Value::Null).as_u64())
            .unwrap_or(0);
        if now.saturating_sub(updated) > RETAIN_MS || i >= MAX_JOBS {
            fs::remove_file(job_path(p, id)?)?;
        }
    }
    Ok(())
}

/// Register the current Node process and interrupt source jobs from any previous process.
/// The process birth value is Linux /proc/self/stat start ticks, captured at Node startup.
pub fn recover(
    ctx: Context<'_>,
    user: &str,
    start: &str,
    born: &str,
    owner: &str,
) -> Result<usize, Error> {
    check_start(ctx, start)?;
    if !valid_owner(owner) {
        return Err(Error::BadRequest);
    }
    let incoming = born_num(born)?;
    with_lock(ctx, user, |p| {
        let mut abandoned_requests = Vec::new();
        if let Some(v) = read_owner(p)? {
            let old_start = v
                .pointer("/start")
                .unwrap_or(&Value::Null)
                .as_str()
                .ok_or(Error::Corrupt)?;
            let old_born = born_num(
                v.pointer("/born")
                    .unwrap_or(&Value::Null)
                    .as_str()
                    .ok_or(Error::Corrupt)?,
            )?;
            let old_owner = v
                .pointer("/owner")
                .unwrap_or(&Value::Null)
                .as_str()
                .ok_or(Error::Corrupt)?;
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
        for (id, rows) in all_source(p)? {
            if terminal(status(&rows)) {
                continue;
            }
            if rows
                .first()
                .and_then(|row| row.pointer("/data/request"))
                .unwrap_or(&Value::Null)
                .as_str()
                .is_some_and(|r| abandoned_requests.iter().any(|a| a == r))
            {
                append(
                    p,
                    &id,
                    &rows,
                    "job.interrupted",
                    json!({"reason":"Source processing was not confirmed; refresh or re-upload to retry."}),
                )?;
                count += 1;
                continue;
            }
            if rows
                .first()
                .and_then(|row| row.pointer("/data/owner"))
                .unwrap_or(&Value::Null)
                == owner
            {
                continue;
            }
            let reason = if status(&rows) == "waiting_approval" {
                "The server restarted while waiting for approval; start again to be asked again."
            } else {
                "The server restarted before this finished."
            };
            append(p, &id, &rows, "job.interrupted", json!({"reason":reason}))?;
            count += 1;
        }
        Ok(count)
    })
}

pub fn start(
    ctx: Context<'_>,
    user: &str,
    start: &str,
    born: &str,
    owner: &str,
    project: &str,
    request: &str,
) -> Result<(String, String), Error> {
    check_start(ctx, start)?;
    if !valid_id(request) {
        return Err(Error::BadRequest);
    }
    with_lock(ctx, user, |p| {
        current(p, start, born, owner)?;
        let owner_state = read_owner(p)?.ok_or(Error::Stale)?;
        let abandoned_requests = abandoned(&owner_state)?;
        if abandoned_requests.iter().any(|a| a == request) {
            return Err(Error::Stale);
        }
        let before = all_source(p)?;
        reconcile_abandoned(p, &abandoned_requests, &before)?;
        let mut all = all_source(p)?;
        if interrupt_stale(p, &all, request)? {
            all = all_source(p)?;
        }
        if let Some((id, rows)) = all.iter().find(|(_, rows)| {
            rows.first()
                .and_then(|row| row.pointer("/data/request"))
                .unwrap_or(&Value::Null)
                == request
        }) {
            if rows
                .first()
                .and_then(|row| row.pointer("/data/owner"))
                .unwrap_or(&Value::Null)
                != owner
                || rows
                    .first()
                    .and_then(|row| row.pointer("/data/projectId"))
                    .unwrap_or(&Value::Null)
                    != project
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
                        json!({"error":"Project removed before source processing began.","result":{"status":404}}),
                    )?;
                }
                return Err(Error::NotFound);
            }
            let run = rows
                .first()
                .and_then(|row| row.pointer("/data/run"))
                .unwrap_or(&Value::Null)
                .as_str()
                .ok_or(Error::Corrupt)?
                .to_string();
            if status(rows) == "queued" {
                append(p, id, rows, "job.started", json!({}))?;
            }
            return Ok((id.clone(), run));
        }
        if !project_exists(p, project)? {
            return Err(Error::NotFound);
        }
        if all
            .iter()
            .filter(|(_, rows)| !terminal(status(rows)))
            .count()
            >= MAX_ACTIVE
        {
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
            json!({"kind":"source","projectId":project,"parentId":null,"capabilities":["project.sources.write"],"owner":owner,"run":run,"request":request}),
        )?;
        let Some(rows) = source_rows(p, &id)? else {
            return Err(Error::Corrupt);
        };
        append(p, &id, &rows, "job.started", json!({}))?;
        Ok((id, run))
    })
}

/// Interrupts the active jobs (but the one of `request`) that have been silent for
/// [`ACTIVE_STALE_MS`]; whether any was.
fn interrupt_stale(p: &Paths, all: &[(String, Vec<Value>)], request: &str) -> Result<bool, Error> {
    let now = now_ms();
    let mut any = false;
    for (id, rows) in all {
        if terminal(status(rows)) {
            continue;
        }
        let first = rows.first();
        if first
            .and_then(|row| row.pointer("/data/request"))
            .and_then(Value::as_str)
            == Some(request)
        {
            continue;
        }
        let last = rows
            .last()
            .and_then(|r| r.pointer("/at"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        if now.saturating_sub(last) > ACTIVE_STALE_MS {
            append(
                p,
                id,
                rows,
                "job.interrupted",
                json!({"reason":"Source processing stopped answering; refresh or re-upload to retry."}),
            )?;
            any = true;
        }
    }
    Ok(any)
}

/// Ends a job whose operation did not begin, when the caller itself saw that (the worker refused
/// it, or the request went away before the worker answered): `job.interrupted`, so its slot is
/// free and it never runs, and no request tombstone is written. Ending one that is already over
/// is `Ok`; a job of another owner or run is [`Error::Stale`].
pub fn interrupt(
    ctx: Context<'_>,
    user: &str,
    start: &str,
    born: &str,
    owner: &str,
    ids: (&str, &str),
) -> Result<(), Error> {
    let (id, run) = ids;
    check_start(ctx, start)?;
    with_lock(ctx, user, |p| {
        current(p, start, born, owner)?;
        let rows = owned_rows(p, id, owner, run)?;
        if !terminal(status(&rows)) {
            append(
                p,
                id,
                &rows,
                "job.interrupted",
                json!({"reason":"Source processing did not begin; refresh or re-upload to retry."}),
            )?;
        }
        Ok(())
    })
}

/// A start whose reply never reached Node is not executable. Persist a request tombstone
/// before terminalizing its journal, so an earlier in-flight start cannot commit afterward.
pub fn abandon(
    ctx: Context<'_>,
    user: &str,
    start: &str,
    born: &str,
    owner: &str,
    request: &str,
) -> Result<(), Error> {
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
                // The oldest tombstone goes (its job, if it ever began, is ended first), so the
                // list is bounded and never refuses another.
                let all = all_source(p)?;
                reconcile_abandoned(p, &requests, &all)?;
                let excess = requests.len() + 1 - MAX_ABANDONED;
                requests.drain(..excess);
            }
            requests.push(request.to_owned());
            write_owner(p, start, born, owner, &requests)?;
        }
        let all = all_source(p)?;
        reconcile_abandoned(p, &requests, &all)
    })
}

fn owned_rows(p: &Paths, id: &str, owner: &str, run: &str) -> Result<Vec<Value>, Error> {
    let rows = source_rows(p, id)?.ok_or(Error::NotFound)?;
    if rows
        .first()
        .and_then(|row| row.pointer("/data/owner"))
        .unwrap_or(&Value::Null)
        != owner
        || rows
            .first()
            .and_then(|row| row.pointer("/data/run"))
            .unwrap_or(&Value::Null)
            != run
    {
        return Err(Error::Stale);
    }
    Ok(rows)
}

pub struct ProgressUpdate<'a> {
    pub id: &'a str,
    pub run: &'a str,
    pub stage: &'a str,
    pub n: u64,
}

pub fn progress(
    ctx: Context<'_>,
    user: &str,
    start: &str,
    born: &str,
    owner: &str,
    update: ProgressUpdate<'_>,
) -> Result<(), Error> {
    let ProgressUpdate { id, run, stage, n } = update;
    check_start(ctx, start)?;
    if stage.len() > MAX_STAGE_BYTES {
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
            .find(|r| r.pointer("/type").unwrap_or(&Value::Null) == "progress");
        let last_n = previous
            .and_then(|r| r.pointer("/data/n").unwrap_or(&Value::Null).as_u64())
            .unwrap_or(0);
        if n == last_n
            && previous.is_some_and(|r| r.pointer("/data/stage").unwrap_or(&Value::Null) == stage)
        {
            return Ok(());
        }
        if n != last_n + 1 {
            return Err(Error::Stale);
        }
        // An in-flight job must be able to terminalize even if its project was deleted.
        append(p, id, &rows, "progress", json!({"stage":stage,"n":n}))?;
        Ok(())
    })
}

pub struct CompletionUpdate<'a> {
    pub id: &'a str,
    pub run: &'a str,
    pub ok: bool,
    pub result: &'a Value,
    pub error: Option<&'a str>,
}

pub fn finish(
    ctx: Context<'_>,
    user: &str,
    start: &str,
    born: &str,
    owner: &str,
    update: CompletionUpdate<'_>,
) -> Result<(), Error> {
    let CompletionUpdate {
        id,
        run,
        ok,
        result,
        error,
    } = update;
    check_start(ctx, start)?;
    if stringify_js(result)?.len() > MAX_RESULT_BYTES {
        return Err(Error::BadRequest);
    }
    with_lock(ctx, user, |p| {
        current(p, start, born, owner)?;
        let rows = owned_rows(p, id, owner, run)?;
        // Owner/run fencing authorizes terminalization after project deletion.
        let data = if ok {
            json!({"result":result})
        } else {
            json!({"error":error.unwrap_or("Source processing failed; refresh or re-upload to retry."),"result":result})
        };
        let ty = if ok { "job.completed" } else { "job.failed" };
        if terminal(status(&rows)) {
            return if rows.last().is_some_and(|row| {
                row.pointer("/type").unwrap_or(&Value::Null) == ty
                    && row.pointer("/data").unwrap_or(&Value::Null) == &data
            }) {
                Ok(())
            } else {
                Err(Error::Stale)
            };
        }
        append(p, id, &rows, ty, data)?;
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
        let Some(rows) = source_rows(p, id)? else {
            return Ok(None);
        };
        if rows
            .first()
            .and_then(|row| row.pointer("/data/projectId"))
            .unwrap_or(&Value::Null)
            != project
        {
            return Ok(None);
        }
        let mut stage: Option<&str> = None;
        let mut error: Option<&str> = None;
        let mut result = Value::Null;
        for row in &rows {
            if row.pointer("/type").unwrap_or(&Value::Null) == "progress" {
                stage = row
                    .pointer("/data/stage")
                    .unwrap_or(&Value::Null)
                    .as_str()
                    .or(stage);
            }
            if row.pointer("/type").unwrap_or(&Value::Null) == "job.failed" {
                error = row.pointer("/data/error").unwrap_or(&Value::Null).as_str();
                result = row.pointer("/data/result").unwrap_or(&Value::Null).clone();
            }
            if row.pointer("/type").unwrap_or(&Value::Null) == "job.completed"
                || row.pointer("/type").unwrap_or(&Value::Null) == "job.cancelled"
            {
                result = row.pointer("/data/result").unwrap_or(&Value::Null).clone();
            }
        }
        Ok(Some(
            json!({"status":status(&rows),"stage":stage,"error":error,"result":result}),
        ))
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    const USER: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    const START: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const OWNER1: &str = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
    const OWNER2: &str = "cccccccc-cccc-4ccc-8ccc-cccccccccccc";
    const REQ1: &str = "11111111-1111-4111-8111-111111111111";
    const REQ2: &str = "22222222-2222-4222-8222-222222222222";
    const REQ3: &str = "33333333-3333-4333-8333-333333333333";

    fn fixture() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let user = dir.path().join("users").join(USER);
        fs::create_dir_all(&user).unwrap();
        fs::write(
            user.join("projects.json"),
            r#"{"projects":[{"id":"a"},{"id":"b"}]}"#,
        )
        .unwrap();
        (dir, user)
    }
    fn ctx<'a>(dir: &'a Path) -> Context<'a> {
        Context {
            data_dir: dir,
            start_id: START,
        }
    }

    #[test]
    fn exact_start_retry_returns_one_journal_and_preserves_two_active_cap() {
        let (dir, user) = fixture();
        let c = ctx(dir.path());
        recover(c, USER, START, "100", OWNER1).unwrap();
        let first = start(c, USER, START, "100", OWNER1, "a", REQ1).unwrap();
        let retry = start(c, USER, START, "100", OWNER1, "a", REQ1).unwrap();
        assert_eq!(first, retry);
        let second = start(c, USER, START, "100", OWNER1, "a", REQ2).unwrap();
        assert_ne!(first, second);
        assert!(matches!(
            start(c, USER, START, "100", OWNER1, "a", REQ3),
            Err(Error::TooMany)
        ));
        let files = fs::read_dir(user.join("jobs")).unwrap().count();
        assert_eq!(files, 2);
    }

    #[test]
    fn new_start_prunes_completed_source_journals_to_sixteen() {
        let (dir, user) = fixture();
        let c = ctx(dir.path());
        recover(c, USER, START, "100", OWNER1).unwrap();
        for i in 0..17 {
            let request = format!("00000000-0000-4000-8000-{i:012x}");
            let (id, run) = start(c, USER, START, "100", OWNER1, "a", &request).unwrap();
            finish(
                c,
                USER,
                START,
                "100",
                OWNER1,
                CompletionUpdate {
                    id: &id,
                    run: &run,
                    ok: true,
                    result: &json!({"status":200}),
                    error: None,
                },
            )
            .unwrap();
        }
        assert_eq!(fs::read_dir(user.join("jobs")).unwrap().count(), 17);
        let (active, _) = start(c, USER, START, "100", OWNER1, "a", REQ1).unwrap();
        assert_eq!(fs::read_dir(user.join("jobs")).unwrap().count(), 17);
        assert!(user.join("jobs").join(format!("{active}.jsonl")).exists());
        assert_eq!(
            all_source(&paths(c, USER).unwrap())
                .unwrap()
                .iter()
                .filter(|(_, rows)| terminal(status(rows)))
                .count(),
            16
        );
    }

    #[test]
    fn deleted_project_job_finishes_and_frees_slot() {
        let (dir, user) = fixture();
        let c = ctx(dir.path());
        recover(c, USER, START, "100", OWNER1).unwrap();
        let (one, run1) = start(c, USER, START, "100", OWNER1, "a", REQ1).unwrap();
        start(c, USER, START, "100", OWNER1, "a", REQ2).unwrap();
        fs::write(user.join("projects.json"), r#"{"projects":[{"id":"b"}]}"#).unwrap();
        progress(
            c,
            USER,
            START,
            "100",
            OWNER1,
            ProgressUpdate {
                id: &one,
                run: &run1,
                stage: "done",
                n: 1,
            },
        )
        .unwrap();
        finish(
            c,
            USER,
            START,
            "100",
            OWNER1,
            CompletionUpdate {
                id: &one,
                run: &run1,
                ok: true,
                result: &json!({"status":200}),
                error: None,
            },
        )
        .unwrap();
        assert!(read(c, USER, "a", &one).unwrap().is_none());
        assert!(start(c, USER, START, "100", OWNER1, "b", REQ3).is_ok());
    }

    #[test]
    fn newer_process_interrupts_and_fences_late_old_recovery() {
        let (dir, _) = fixture();
        let c = ctx(dir.path());
        recover(c, USER, START, "100", OWNER1).unwrap();
        let (id, run) = start(c, USER, START, "100", OWNER1, "a", REQ1).unwrap();
        assert_eq!(recover(c, USER, START, "200", OWNER2).unwrap(), 1);
        assert!(matches!(
            recover(c, USER, START, "100", OWNER1),
            Err(Error::Stale)
        ));
        assert!(matches!(
            recover(c, USER, START, "200", OWNER1),
            Err(Error::Stale)
        ));
        assert!(matches!(
            progress(
                c,
                USER,
                START,
                "100",
                OWNER1,
                ProgressUpdate {
                    id: &id,
                    run: &run,
                    stage: "late",
                    n: 1
                }
            ),
            Err(Error::Stale)
        ));
        assert_eq!(
            read(c, USER, "a", &id)
                .unwrap()
                .unwrap()
                .get("status")
                .unwrap(),
            "interrupted"
        );
    }

    #[test]
    fn node_surrogates_round_trip_in_legacy_and_new_journals() {
        let (dir, user) = fixture();
        let c = ctx(dir.path());
        let legacy = "44444444-4444-4444-8444-444444444444";
        let rows = format!(
            "{{\"job\":\"{legacy}\",\"seq\":1,\"type\":\"job.created\",\"at\":1,\"data\":{{\"kind\":\"source\",\"projectId\":\"a\",\"parentId\":null,\"capabilities\":[]}}}}\n{{\"job\":\"{legacy}\",\"seq\":2,\"type\":\"job.completed\",\"at\":2,\"data\":{{\"result\":{{\"status\":200,\"body\":{{\"text\":\"\\ud800\"}}}}}}}}\n"
        );
        fs::create_dir(user.join("jobs")).unwrap();
        fs::write(user.join("jobs").join(format!("{legacy}.jsonl")), rows).unwrap();
        let old = read(c, USER, "a", legacy).unwrap().unwrap();
        assert!(stringify_js(&old).unwrap().contains(r"\ud800"));
        recover(c, USER, START, "100", OWNER1).unwrap();
        let (id, run) = start(c, USER, START, "100", OWNER1, "a", REQ1).unwrap();
        let result = parse_js(br#"{"status":200,"body":{"text":"\ud800"}}"#).unwrap();
        finish(
            c,
            USER,
            START,
            "100",
            OWNER1,
            CompletionUpdate {
                id: &id,
                run: &run,
                ok: true,
                result: &result,
                error: None,
            },
        )
        .unwrap();
        let written = fs::read_to_string(user.join("jobs").join(format!("{id}.jsonl"))).unwrap();
        assert!(written.contains(r"\ud800"));
        assert!(stringify_js(&read(c, USER, "a", &id).unwrap().unwrap())
            .unwrap()
            .contains(r"\ud800"));
    }

    #[test]
    fn unrelated_lone_surrogate_metadata_does_not_hide_project() {
        let (dir, user) = fixture();
        fs::write(
            user.join("projects.json"),
            r#"{"projects":[{"id":"a"},{"id":"b","title":"\ud800"}]}"#,
        )
        .unwrap();
        let c = ctx(dir.path());
        recover(c, USER, START, "100", OWNER1).unwrap();
        assert!(start(c, USER, START, "100", OWNER1, "a", REQ1).is_ok());
    }

    #[test]
    fn exact_start_retry_survives_project_deletion() {
        let (dir, user) = fixture();
        let c = ctx(dir.path());
        recover(c, USER, START, "100", OWNER1).unwrap();
        let first = start(c, USER, START, "100", OWNER1, "a", REQ1).unwrap();
        fs::write(user.join("projects.json"), r#"{"projects":[{"id":"b"}]}"#).unwrap();
        assert!(matches!(
            start(c, USER, START, "100", OWNER1, "a", REQ1),
            Err(Error::NotFound)
        ));
        assert_eq!(read(c, USER, "b", &first.0).unwrap(), None);
        let journal = source_rows(&paths(c, USER).unwrap(), &first.0)
            .unwrap()
            .unwrap();
        assert_eq!(status(&journal), "failed");
        assert_eq!(
            journal
                .last()
                .unwrap()
                .pointer("/data/result/status")
                .unwrap(),
            404
        );
        assert!(matches!(
            start(c, USER, START, "100", OWNER1, "a", REQ2),
            Err(Error::NotFound)
        ));
        assert!(start(c, USER, START, "100", OWNER1, "b", REQ3).is_ok());
    }

    #[test]
    fn unrelated_job_with_corrupt_tail_is_not_rewritten_or_counted() {
        let (dir, user) = fixture();
        let c = ctx(dir.path());
        let other = "55555555-5555-4555-8555-555555555555";
        fs::create_dir(user.join("jobs")).unwrap();
        let bytes = format!(
            r#"{{"job":"{other}","seq":1,"type":"job.created","at":1,"data":{{"kind":"code","projectId":"a"}}}}
this is not JSON
"#
        );
        fs::write(user.join("jobs").join(format!("{other}.jsonl")), &bytes).unwrap();
        recover(c, USER, START, "100", OWNER1).unwrap();
        start(c, USER, START, "100", OWNER1, "a", REQ1).unwrap();
        assert_eq!(
            fs::read_to_string(user.join("jobs").join(format!("{other}.jsonl"))).unwrap(),
            bytes
        );
    }

    #[cfg(unix)]
    #[test]
    fn source_owner_lock_symlink_is_refused_without_touching_target() {
        use std::os::unix::fs::symlink;
        let (dir, user) = fixture();
        let target = dir.path().join("target");
        fs::write(&target, "untouched").unwrap();
        symlink(&target, user.join("source-jobs-owner.json.lock")).unwrap();
        assert!(recover(ctx(dir.path()), USER, START, "100", OWNER1).is_err());
        assert_eq!(fs::read_to_string(target).unwrap(), "untouched");
    }

    #[test]
    fn valid_last_event_without_newline_is_preserved_on_append() {
        let (dir, user) = fixture();
        let c = ctx(dir.path());
        recover(c, USER, START, "100", OWNER1).unwrap();
        let (id, run) = start(c, USER, START, "100", OWNER1, "a", REQ1).unwrap();
        let file = user.join("jobs").join(format!("{id}.jsonl"));
        let mut bytes = fs::read(&file).unwrap();
        assert_eq!(bytes.pop(), Some(b'\n'));
        fs::write(&file, bytes).unwrap();
        progress(
            c,
            USER,
            START,
            "100",
            OWNER1,
            ProgressUpdate {
                id: &id,
                run: &run,
                stage: "working",
                n: 1,
            },
        )
        .unwrap();
        assert_eq!(
            read(c, USER, "a", &id)
                .unwrap()
                .unwrap()
                .get("stage")
                .unwrap(),
            "working"
        );
        assert_eq!(fs::read_to_string(file).unwrap().lines().count(), 3);
    }

    #[test]
    fn node_writer_and_reader_interoperate_when_core_is_available() {
        use std::process::Command;
        let (Ok(core), Ok(devrun)) = (
            std::env::var("NOEVIA_CORE_DIR"),
            std::env::var("NOEVIA_DEVRUN"),
        ) else {
            return;
        };
        let (dir, user) = fixture();
        let module = Path::new(&core).join("server/jobs.cjs");
        let create_script = r#"
const { createJobs } = require(process.argv[1]);
const jobs = createJobs({ dir: process.argv[2], kinds: ['source'], retainMs: 900000, maxJobs: 16 });
const id = jobs.create({ kind: 'source', projectId: 'a', capabilities: ['project.sources.write'] });
jobs.append(id, 'job.started');
jobs.append(id, 'job.completed', { result: { status: 200, body: { text: '\uD800' } } });
process.stdout.write(id);
"#;
        let made = Command::new(&devrun)
            .args(["node", "-e", create_script])
            .arg(&module)
            .arg(&user)
            .output()
            .unwrap();
        assert!(
            made.status.success(),
            "{}",
            String::from_utf8_lossy(&made.stderr)
        );
        let legacy = String::from_utf8(made.stdout).unwrap();
        let old = read(ctx(dir.path()), USER, "a", &legacy).unwrap().unwrap();
        assert!(stringify_js(&old).unwrap().contains(r"\ud800"));
        recover(ctx(dir.path()), USER, START, "100", OWNER1).unwrap();
        let (id, run) = start(ctx(dir.path()), USER, START, "100", OWNER1, "a", REQ1).unwrap();
        let result = parse_js(br#"{"status":200,"body":{"text":"\ud800"}}"#).unwrap();
        finish(
            ctx(dir.path()),
            USER,
            START,
            "100",
            OWNER1,
            CompletionUpdate {
                id: &id,
                run: &run,
                ok: true,
                result: &result,
                error: None,
            },
        )
        .unwrap();
        let read_script = r#"
const { createJobs } = require(process.argv[1]);
const jobs = createJobs({ dir: process.argv[2], kinds: ['source'], retainMs: 900000, maxJobs: 16 });
const job = jobs.get(process.argv[3]);
if (job?.status !== 'completed' || JSON.stringify(job.result?.body?.text) !== '"\\ud800"' ||
    !jobs.list({ kind: 'source', strict: true }).some((j) => j.id === job.id)) process.exit(1);
"#;
        let read = Command::new(&devrun)
            .args(["node", "-e", read_script])
            .arg(&module)
            .arg(&user)
            .arg(&id)
            .output()
            .unwrap();
        assert!(
            read.status.success(),
            "{}",
            String::from_utf8_lossy(&read.stderr)
        );
    }

    #[test]
    fn concurrent_starts_admit_only_two_source_jobs() {
        use std::sync::{Arc, Barrier};
        let (dir, _) = fixture();
        let c = ctx(dir.path());
        recover(c, USER, START, "100", OWNER1).unwrap();
        let gate = Arc::new(Barrier::new(3));
        let results = std::thread::scope(|scope| {
            [REQ1, REQ2, REQ3]
                .into_iter()
                .map(|request| {
                    let gate = Arc::clone(&gate);
                    scope.spawn(move || {
                        gate.wait();
                        start(c, USER, START, "100", OWNER1, "a", request)
                    })
                })
                .collect::<Vec<_>>()
                .into_iter()
                .map(|thread| thread.join().unwrap())
                .collect::<Vec<_>>()
        });
        assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 2);
        assert_eq!(
            results
                .iter()
                .filter(|r| matches!(r, Err(Error::TooMany)))
                .count(),
            1
        );
    }

    #[test]
    fn abandoned_start_releases_slot_and_fences_late_request() {
        let (dir, user) = fixture();
        let c = ctx(dir.path());
        recover(c, USER, START, "100", OWNER1).unwrap();
        let (id, _) = start(c, USER, START, "100", OWNER1, "a", REQ1).unwrap();
        abandon(c, USER, START, "100", OWNER1, REQ1).unwrap();
        abandon(c, USER, START, "100", OWNER1, REQ1).unwrap();
        assert_eq!(
            read(c, USER, "a", &id)
                .unwrap()
                .unwrap()
                .get("status")
                .unwrap(),
            "interrupted"
        );
        recover(c, USER, START, "100", OWNER1).unwrap(); // same owner preserves tombstone
        assert!(matches!(
            start(c, USER, START, "100", OWNER1, "a", REQ1),
            Err(Error::Stale)
        ));
        assert!(start(c, USER, START, "100", OWNER1, "a", REQ2).is_ok());
        assert!(start(c, USER, START, "100", OWNER1, "a", REQ3).is_ok());
        let files = fs::read_dir(user.join("jobs")).unwrap().count();
        assert_eq!(files, 3);
    }

    #[test]
    fn abandon_arriving_before_delayed_start_persists_tombstone() {
        let (dir, _) = fixture();
        let c = ctx(dir.path());
        recover(c, USER, START, "100", OWNER1).unwrap();
        abandon(c, USER, START, "100", OWNER1, REQ1).unwrap();
        assert!(matches!(
            start(c, USER, START, "100", OWNER1, "a", REQ1),
            Err(Error::Stale)
        ));
        assert!(start(c, USER, START, "100", OWNER1, "a", REQ2).is_ok());
    }

    fn owner_file(user: &Path) -> Value {
        serde_json::from_slice(&fs::read(user.join("source-jobs-owner.json")).unwrap()).unwrap()
    }

    fn request(n: usize) -> String {
        format!("{n:08x}-1111-4111-8111-111111111111")
    }

    /// Review #2191 r6+7: past 256 tombstones both `abandon` and `start` answered `Busy` for good.
    #[test]
    fn the_tombstone_list_is_bounded_and_never_refuses_a_start() {
        let (dir, user) = fixture();
        let c = ctx(dir.path());
        recover(c, USER, START, "100", OWNER1).unwrap();
        for n in 0..300 {
            abandon(c, USER, START, "100", OWNER1, &request(n)).unwrap();
        }
        let kept = owner_file(&user)["abandoned"].as_array().unwrap().clone();
        assert_eq!(kept.len(), MAX_ABANDONED);
        // The newest are kept, the oldest dropped.
        assert_eq!(kept.last().unwrap(), &Value::from(request(299)));
        assert_eq!(
            kept.first().unwrap(),
            &Value::from(request(300 - MAX_ABANDONED))
        );
        assert!(matches!(
            start(c, USER, START, "100", OWNER1, "a", &request(299)),
            Err(Error::Stale)
        ));
        assert!(start(c, USER, START, "100", OWNER1, "a", REQ1).is_ok());
        // A list the store already holds at its bound does not stop a start either.
        recover(c, USER, START, "100", OWNER1).unwrap();
        assert!(start(c, USER, START, "100", OWNER1, "a", REQ2).is_ok());
    }

    /// Dropping the oldest tombstone ends its job first, so it cannot run after losing its fence
    /// (a crash between writing a tombstone and ending the job leaves exactly that).
    #[test]
    fn dropping_a_tombstone_ends_a_job_that_had_begun() {
        let (dir, user) = fixture();
        let c = ctx(dir.path());
        recover(c, USER, START, "100", OWNER1).unwrap();
        let (id, _) = start(c, USER, START, "100", OWNER1, "a", REQ1).unwrap();
        let mut full = vec![REQ1.to_owned()];
        full.extend((1..MAX_ABANDONED).map(request));
        fs::write(
            user.join("source-jobs-owner.json"),
            serde_json::json!({"start":START,"born":"100","owner":OWNER1,"abandoned":full})
                .to_string(),
        )
        .unwrap();
        assert_eq!(
            read(c, USER, "a", &id).unwrap().unwrap()["status"],
            "running"
        );
        abandon(c, USER, START, "100", OWNER1, REQ2).unwrap();
        assert_eq!(
            read(c, USER, "a", &id).unwrap().unwrap()["status"],
            "interrupted"
        );
        let kept = owner_file(&user)["abandoned"].as_array().unwrap().clone();
        assert_eq!(kept.len(), MAX_ABANDONED);
        assert_eq!(kept.last().unwrap(), &Value::from(REQ2));
        assert!(!kept.contains(&Value::from(REQ1)));
    }

    /// A refusal the caller saw ends the job with no tombstone and frees its slot.
    #[test]
    fn interrupt_frees_the_slot_and_writes_no_tombstone() {
        let (dir, user) = fixture();
        let c = ctx(dir.path());
        recover(c, USER, START, "100", OWNER1).unwrap();
        let (id1, run1) = start(c, USER, START, "100", OWNER1, "a", REQ1).unwrap();
        let (id2, run2) = start(c, USER, START, "100", OWNER1, "a", REQ2).unwrap();
        assert!(matches!(
            start(c, USER, START, "100", OWNER1, "a", REQ3),
            Err(Error::TooMany)
        ));
        interrupt(c, USER, START, "100", OWNER1, (&id1, &run1)).unwrap();
        interrupt(c, USER, START, "100", OWNER1, (&id1, &run1)).unwrap();
        assert_eq!(
            read(c, USER, "a", &id1).unwrap().unwrap()["status"],
            "interrupted"
        );
        assert_eq!(
            read(c, USER, "a", &id2).unwrap().unwrap()["status"],
            "running"
        );
        assert_eq!(owner_file(&user)["abandoned"], serde_json::json!([]));
        assert!(start(c, USER, START, "100", OWNER1, "a", REQ3).is_ok());
        // Another run of the job, or another owner, cannot end it.
        assert!(matches!(
            interrupt(c, USER, START, "100", OWNER1, (&id2, REQ1)),
            Err(Error::Stale)
        ));
        // A finished job stays as it was.
        finish(
            c,
            USER,
            START,
            "100",
            OWNER1,
            CompletionUpdate {
                id: &id2,
                run: &run2,
                ok: true,
                result: &serde_json::json!({"status":200,"body":{}}),
                error: None,
            },
        )
        .unwrap();
        interrupt(c, USER, START, "100", OWNER1, (&id2, &run2)).unwrap();
        assert_eq!(
            read(c, USER, "a", &id2).unwrap().unwrap()["status"],
            "completed"
        );
    }

    /// A job that wrote nothing for hours holds no slot: the next start interrupts it.
    #[test]
    fn a_silent_active_job_is_interrupted_by_the_next_start() {
        let (dir, user) = fixture();
        let c = ctx(dir.path());
        recover(c, USER, START, "100", OWNER1).unwrap();
        let (id1, _) = start(c, USER, START, "100", OWNER1, "a", REQ1).unwrap();
        let (id2, _) = start(c, USER, START, "100", OWNER1, "a", REQ2).unwrap();
        // Both are active; the first last wrote seven hours ago.
        let file = user.join("jobs").join(format!("{id1}.jsonl"));
        let old = now_ms() - 7 * 60 * 60 * 1000;
        let aged: String = fs::read_to_string(&file)
            .unwrap()
            .lines()
            .map(|l| {
                let mut row: Value = serde_json::from_str(l).unwrap();
                row["at"] = Value::from(old);
                format!("{row}\n")
            })
            .collect();
        fs::write(&file, aged).unwrap();
        assert!(start(c, USER, START, "100", OWNER1, "a", REQ3).is_ok());
        assert_eq!(
            read(c, USER, "a", &id1).unwrap().unwrap()["status"],
            "interrupted"
        );
        assert_eq!(
            read(c, USER, "a", &id2).unwrap().unwrap()["status"],
            "running"
        );
    }
}
