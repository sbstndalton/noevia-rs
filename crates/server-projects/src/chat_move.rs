//! Slice7 foundation. Rust owns canonical chat generations and recoverable metadata moves.
//! The public POST uses the same store transaction; packaging requires matched Node grant
//! enforcement, journal reader and POST refusal. Lock order: user/transcript -> projects -> free.
//! File-only writers fence while holding their file lock; they NEVER recover recursively.
//! Strict durability failures retain the prepared intent and fail closed.
use crate::store::StoreError;
use crate::{
    chats, frame, history::safe_chat_id, journal, lock, partial, skills, store, store_api,
    transcript,
};
use js_json::{wtf16, JValue};
use rand_core::{OsRng, RngCore};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet};
use std::io::{Read, Write};
use std::path::Path;

pub const FILE: &str = "chat-move-state.json";
pub const MOVE_MARKER: &str = "noevia-chat-move-foundation/1";
const STATE_LIMIT: usize = store_api::TEXT_LIMIT * 4 + 1024 * 1024;

/// Node's readBody default; its move route wraps oversize/read failure as invalid JSON.
pub const BODY_LIMIT: usize = 1024 * 1024;
pub fn post(
    ctx: crate::Ctx<'_>,
    encoded_chat: &str,
    body: &[u8],
    over: bool,
    start: &str,
) -> crate::Reply {
    let Some(chat) = crate::js::decode_uri_component(encoded_chat) else {
        return crate::Reply::error(400, "invalid chat id");
    };
    if over {
        return crate::Reply::error(400, "invalid JSON");
    }
    let parsed = match wtf16::parse(&String::from_utf8_lossy(body)) {
        Ok(value) => value,
        Err(wtf16::Refusal::Invalid) => return crate::Reply::error(400, "invalid JSON"),
        Err(_) => return crate::Reply::internal(),
    };
    let destination = match parsed.get("projectId") {
        JValue::Null if parsed.is_object() => Holder::Free,
        JValue::Str(id) if parsed.is_object() => Holder::Project(id.clone()),
        _ => return crate::Reply::error(400, "projectId (or null) required"),
    };
    let patch = match parsed.get("frame") {
        JValue::Undefined => None,
        value => {
            if !matches!(value, JValue::Null) && frame::normalize(value).is_none() {
                return crate::Reply::error(400, "invalid frame");
            }
            Some(value)
        }
    };
    match move_chat(ctx.data_dir, ctx.user_id, &chat, destination, patch, start) {
        Ok(moved) => {
            let id = |holder: &Holder| match holder {
                Holder::Project(id) => Value::String(id.clone()),
                _ => Value::Null,
            };
            crate::Reply::json(
                200,
                json!({"ok":true,"from":id(&moved.from),"projectId":id(&moved.to)}).to_string(),
            )
        }
        Err(MoveError::Invalid) => crate::Reply::error(400, "invalid chat move or start id"),
        Err(MoveError::NotFound) => crate::Reply::error(404, "no such chat or project"),
        Err(MoveError::Ambiguous) => crate::Reply::json(
            409,
            json!({"error":"ambiguous canonical chat identity","code":"CHAT_ID_AMBIGUOUS"}).to_string(),
        ),
        Err(MoveError::Capacity) => crate::Reply::json(
            409,
            json!({"error":"the move would evict chat metadata","code":"CHAT_DESTINATION_FULL"}).to_string(),
        ),
        Err(MoveError::Store(StoreError::Gone)) => crate::Reply::error(410, "account no longer exists"),
        Err(MoveError::Store(StoreError::InvalidUser)) => crate::Reply::error(400, "invalid user"),
        Err(MoveError::Store(StoreError::Busy)) => crate::Reply {
            status: 503,
            body: crate::Body::Bytes {
                headers: vec![
                    ("content-type", "application/json; charset=utf-8".into()),
                    ("retry-after", "1".into()),
                ],
                bytes: json!({"error":"The project store is busy. Try again shortly.","code":"PROJECTS_BUSY"})
                    .to_string().into_bytes(),
            },
        },
        Err(_) => crate::Reply::internal(),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Holder {
    Free,
    Project(String),
    Absent,
}
impl Holder {
    pub fn json(&self) -> Value {
        match self {
            Self::Free => json!({"kind":"free"}),
            Self::Project(id) => json!({"kind":"project","id":id}),
            Self::Absent => json!({"kind":"absent"}),
        }
    }
    fn parse(v: &Value) -> Result<Self, StoreError> {
        match v.get("kind").and_then(Value::as_str) {
            Some("free") if v.as_object().is_some_and(|o| o.len() == 1) => Ok(Self::Free),
            Some("absent") if v.as_object().is_some_and(|o| o.len() == 1) => Ok(Self::Absent),
            Some("project") if v.as_object().is_some_and(|o| o.len() == 2) => {
                Ok(Self::Project(string(v, "id")?.to_owned()))
            }
            _ => Err(StoreError::Corrupt),
        }
    }
}
#[derive(Debug)]
pub enum MoveError {
    Store(StoreError),
    Invalid,
    NotFound,
    Ambiguous,
    Capacity,
}
impl From<StoreError> for MoveError {
    fn from(e: StoreError) -> Self {
        Self::Store(e)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantContext {
    pub canonical: String,
    pub holder: Holder,
    pub generation: String,
    pub start: String,
    pub tombstoned: bool,
}
impl GrantContext {
    pub fn json(&self) -> Value {
        json!({"canonicalChatId":self.canonical,"holder":self.holder.json(),
            "generation":self.generation,"start":self.start,"tombstoned":self.tombstoned})
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Moved {
    pub from: Holder,
    pub to: Holder,
    pub generation: String,
}

#[derive(Debug, Clone)]
struct Prepared {
    canonical: String,
    raw: String,
    generation: u64,
    event: String,
    from: Holder,
    to: Holder,
    before: [String; 2],
    after: [String; 2],
}
#[derive(Debug, Default)]
struct State {
    generations: BTreeMap<String, u64>,
    pending: Option<Prepared>,
    completed: Option<Value>,
}
fn string<'a>(v: &'a Value, key: &str) -> Result<&'a str, StoreError> {
    v.get(key)
        .and_then(Value::as_str)
        .ok_or(StoreError::Corrupt)
}
fn number(s: &str) -> Result<u64, StoreError> {
    let n = s.parse::<u64>().map_err(|_| StoreError::Corrupt)?;
    if n.to_string() != s {
        return Err(StoreError::Corrupt);
    }
    Ok(n)
}
fn canonical(s: &str) -> bool {
    !s.is_empty() && safe_chat_id(s) == s
}
fn version(s: &str) -> bool {
    s.is_empty()
        || (s.len() == 64
            && s.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
}
fn read_text(dir: &Path, name: &str, limit: usize) -> Result<Option<String>, StoreError> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut options = std::fs::OpenOptions::new();
    options.read(true).custom_flags(libc::O_NOFOLLOW);
    let file = match options.open(dir.join(name)) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    if !file.metadata()?.is_file() {
        return Err(StoreError::Corrupt);
    }
    let mut text = String::new();
    file.take((limit as u64) + 1).read_to_string(&mut text)?;
    if text.len() > limit {
        return Err(StoreError::Corrupt);
    }
    Ok(Some(text))
}
fn state(dir: &Path) -> Result<State, StoreError> {
    let Some(text) = read_text(dir, FILE, STATE_LIMIT)? else {
        return Ok(State::default());
    };
    let v: Value = serde_json::from_str(&text).map_err(|_| StoreError::Corrupt)?;
    if v.get("version") != Some(&json!(1)) {
        return Err(StoreError::Corrupt);
    }
    let obj = v
        .get("generations")
        .and_then(Value::as_object)
        .ok_or(StoreError::Corrupt)?;
    // Kept for forensic recovery even after the active fence clears; it never authorizes a move.
    let mut out = State {
        completed: v.get("completed").filter(|v| !v.is_null()).cloned(),
        ..State::default()
    };
    for (key, value) in obj {
        if !canonical(key) {
            return Err(StoreError::Corrupt);
        }
        out.generations.insert(
            key.clone(),
            number(value.as_str().ok_or(StoreError::Corrupt)?)?,
        );
    }
    let p = v.get("pending").ok_or(StoreError::Corrupt)?;
    if !p.is_null() {
        let key = string(p, "canonical")?.to_string();
        let raw = string(p, "raw")?.to_string();
        let generation = number(string(p, "generation")?)?;
        let event = string(p, "event")?.to_string();
        if !canonical(&key)
            || safe_chat_id(&raw) != key
            || event.len() != 32
            || !event.bytes().all(|b| b.is_ascii_hexdigit())
            || out.generations.get(&key).copied() != Some(generation)
        {
            return Err(StoreError::Corrupt);
        }
        let mut before = [String::new(), String::new()];
        let mut after = [String::new(), String::new()];
        for (i, key) in ["projects", "free"].iter().enumerate() {
            let item = p.get(key).ok_or(StoreError::Corrupt)?;
            let bv = string(item, "before")?;
            let av = string(item, "after")?;
            if !version(bv) || av.len() > store_api::TEXT_LIMIT {
                return Err(StoreError::Corrupt);
            }
            let parsed = wtf16::parse(av).map_err(|_| StoreError::Corrupt)?;
            if (i == 0 && !matches!(parsed.get("projects"), JValue::Arr(_)))
                || (i == 1 && !matches!(parsed, JValue::Arr(_)))
            {
                return Err(StoreError::Corrupt);
            }
            *before.get_mut(i).ok_or(StoreError::Corrupt)? = bv.into();
            *after.get_mut(i).ok_or(StoreError::Corrupt)? = av.into();
        }
        out.pending = Some(Prepared {
            canonical: key,
            raw,
            generation,
            event,
            from: Holder::parse(p.get("from").ok_or(StoreError::Corrupt)?)?,
            to: Holder::parse(p.get("to").ok_or(StoreError::Corrupt)?)?,
            before,
            after,
        });
    }
    Ok(out)
}
fn encoded(s: &State) -> Result<String, StoreError> {
    let generations: BTreeMap<_, _> = s
        .generations
        .iter()
        .map(|(k, v)| (k, v.to_string()))
        .collect();
    let pending=s.pending.as_ref().map(|p| json!({
        "canonical":p.canonical,"raw":p.raw,"generation":p.generation.to_string(),"event":p.event,
        "from":p.from.json(),"to":p.to.json(),
        "projects":{"before":p.before.first(),"after":p.after.first()},
        "free":{"before":p.before.get(1),"after":p.after.get(1)}
    }));
    let text =
        json!({"version":1,"generations":generations,"pending":pending,"completed":s.completed})
            .to_string();
    if text.len() > STATE_LIMIT {
        return Err(StoreError::Corrupt);
    }
    Ok(text)
}

/// A writer calls this AFTER taking its file lock, BEFORE read/CAS/change. No recovery/lock
/// acquisition here: a cascade can already hold the user lock. Corruption is also fail closed.
pub(crate) fn fence(dir: &Path) -> Result<(), StoreError> {
    transcript::existing_dir(dir)?;
    if state(dir)?.pending.is_some() {
        return Err(StoreError::Busy);
    }
    // A completion rename whose parent fsync failed is not silently accepted by a new writer:
    // establish strict durability again before clearing the fence for this call.
    sync_state(dir)?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    TempSynced,
    Renamed,
    ParentSynced,
    Prepared,
    FirstList,
    SecondList,
    Journal,
    Completed,
}
type Fault<'a> = &'a dyn Fn(Step) -> Result<(), StoreError>;

/// Strict write: own directory descriptor, no symlinks, exclusive unpredictable temp, file fsync,
/// rename, parent fsync. Every error is returned. A failed phase is not proof of publication.
fn strict_write(dir: &Path, name: &str, text: &str, fault: Fault<'_>) -> Result<(), StoreError> {
    use rustix::fs::{open, openat, renameat, Mode, OFlags};
    transcript::existing_dir(dir)?;
    let directory = std::fs::File::from(
        open(
            dir,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(std::io::Error::from)?,
    );
    let mut random = [0u8; 16];
    OsRng
        .try_fill_bytes(&mut random)
        .map_err(|_| StoreError::Corrupt)?;
    let nonce = random
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    let temp = format!("{name}.move-{nonce}.tmp");
    let mut output = std::fs::File::from(
        openat(
            &directory,
            &temp,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )
        .map_err(std::io::Error::from)?,
    );
    output.write_all(text.as_bytes())?;
    output.sync_all()?;
    fault(Step::TempSynced)?;
    renameat(&directory, &temp, &directory, name).map_err(std::io::Error::from)?;
    fault(Step::Renamed)?;
    directory.sync_all()?;
    fault(Step::ParentSynced)?;
    transcript::existing_dir(dir)?;
    Ok(())
}
#[cfg(test)]
thread_local! { static SYNC_REFUSAL: std::cell::RefCell<Option<std::path::PathBuf>> = const { std::cell::RefCell::new(None) }; }
fn sync_state(dir: &Path) -> Result<(), StoreError> {
    use std::os::unix::fs::OpenOptionsExt;
    let result = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(dir.join(FILE));
    match result {
        Ok(f) => {
            f.sync_all()?;
            #[cfg(test)]
            SYNC_REFUSAL.with(|refusal| {
                if refusal.borrow().as_deref() == Some(dir) {
                    Err(StoreError::Corrupt)
                } else {
                    Ok(())
                }
            })?;
            std::fs::File::open(dir)?.sync_all()?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    transcript::existing_dir(dir)?;
    Ok(())
}

fn snapshot(dir: &Path, name: &str) -> Result<(String, Option<String>), StoreError> {
    let text = read_text(dir, name, store_api::TEXT_LIMIT)?;
    let v = text
        .as_ref()
        .map(|s| store_api::version_of(s.as_bytes()))
        .unwrap_or_default();
    Ok((v, text))
}
const NAMES: [&str; 2] = [store::PROJECTS_FILE, chats::FREE_CHATS_FILE];
fn recover_held(dir: &Path, s: &mut State, fault: Fault<'_>) -> Result<(), StoreError> {
    finish_held(dir, s, fault, journal::MoveDelivery::Recovery)
}
fn finish_held(
    dir: &Path,
    s: &mut State,
    fault: Fault<'_>,
    delivery: journal::MoveDelivery,
) -> Result<(), StoreError> {
    let Some(p) = s.pending.as_ref() else {
        return Ok(());
    };
    // Check BOTH versions before changing either one. Unknown state is never overwritten.
    let now = [
        snapshot(dir, store::PROJECTS_FILE)?.0,
        snapshot(dir, chats::FREE_CHATS_FILE)?.0,
    ];
    for ((v, before), after) in now.iter().zip(&p.before).zip(&p.after) {
        if v != before && v != &store_api::version_of(after.as_bytes()) {
            return Err(StoreError::Corrupt);
        }
    }
    // Preserve Node's destination-first invariant. Project->free writes free first; all other
    // cases write projects first, leaving the chat never in neither list during publication.
    let order = if p.to == Holder::Free { [1, 0] } else { [0, 1] };
    for (position, i) in order.into_iter().enumerate() {
        let after = p.after.get(i).ok_or(StoreError::Corrupt)?;
        if now.get(i).ok_or(StoreError::Corrupt)? != &store_api::version_of(after.as_bytes()) {
            strict_write(dir, NAMES.get(i).ok_or(StoreError::Corrupt)?, after, fault)?;
        } else {
            // A prior failed/uncertain fsync cannot be promoted into success just because bytes
            // look right: sync the actual after-image and parent again before advancing.
            std::fs::File::open(dir.join(NAMES.get(i).ok_or(StoreError::Corrupt)?))?.sync_all()?;
            std::fs::File::open(dir)?.sync_all()?;
        }
        fault(if position == 0 {
            Step::FirstList
        } else {
            Step::SecondList
        })?;
    }
    journal::record_move(
        dir,
        &p.canonical,
        &p.event,
        &p.generation.to_string(),
        &p.raw,
        delivery,
    )?;
    fault(Step::Journal)?;
    // Retain bounded completion evidence; the active intent above held the full after-images.
    s.completed = Some(
        json!({"canonical":p.canonical,"generation":p.generation.to_string(),
        "event":p.event,"from":p.from.json(),"to":p.to.json(),
        "projectsVersion":store_api::version_of(p.after.first().ok_or(StoreError::Corrupt)?.as_bytes()),
        "freeVersion":store_api::version_of(p.after.get(1).ok_or(StoreError::Corrupt)?.as_bytes())}),
    );
    let pending = s.pending.take();
    if let Err(e) = strict_write(dir, FILE, &encoded(s)?, fault) {
        s.pending = pending;
        return Err(e);
    }
    fault(Step::Completed)?;
    Ok(())
}
fn with_held<T>(
    dir: &Path,
    run: impl FnOnce(&mut State) -> Result<T, MoveError>,
) -> Result<T, MoveError> {
    transcript::existing_dir(dir)?;
    let _user = transcript::lock_user(dir)?;
    let _projects = lock::acquire(&dir.join(store::PROJECTS_FILE)).map_err(StoreError::from)?;
    let _free = lock::acquire(&dir.join(chats::FREE_CHATS_FILE)).map_err(StoreError::from)?;
    transcript::existing_dir(dir)?;
    let mut s = state(dir)?;
    sync_state(dir)?;
    run(&mut s)
}
fn internal(id: &str) -> bool {
    id == "cowork-diary-extras" || id.starts_with("cowork-chat-context-")
}
fn list(v: &JValue) -> Result<Vec<JValue>, MoveError> {
    match v {
        JValue::Undefined | JValue::Null => Ok(Vec::new()),
        JValue::Arr(xs) => Ok(xs.clone()),
        _ => Err(MoveError::Store(StoreError::Corrupt)),
    }
}
fn occurrences(
    projects: &[JValue],
    free: &[JValue],
    safe: &str,
) -> Result<Vec<(Holder, JValue)>, MoveError> {
    let mut found = Vec::new();
    let mut scan = |holder: Holder, items: &[JValue]| {
        for item in items {
            if let Some(id) = item.get("id").as_str() {
                if item.is_object() && safe_chat_id(id) == safe {
                    found.push((holder.clone(), item.clone()));
                }
            }
        }
    };
    scan(Holder::Free, free);
    for p in projects {
        let id = p
            .get("id")
            .as_str()
            .ok_or(MoveError::Store(StoreError::Corrupt))?;
        scan(Holder::Project(id.into()), &list(p.get("chats"))?);
    }
    Ok(found)
}
fn read_lists(dir: &Path) -> Result<(Vec<JValue>, Vec<JValue>), MoveError> {
    let projects = match read_text(dir, store::PROJECTS_FILE, store_api::TEXT_LIMIT)? {
        None => Vec::new(),
        Some(text) => match wtf16::parse(&text)
            .map_err(|_| StoreError::Corrupt)?
            .get("projects")
        {
            JValue::Arr(items) => items.clone(),
            _ => return Err(StoreError::Corrupt.into()),
        },
    };
    let free = match read_text(dir, chats::FREE_CHATS_FILE, store_api::TEXT_LIMIT)? {
        None => Vec::new(),
        Some(text) => match wtf16::parse(&text).map_err(|_| StoreError::Corrupt)? {
            JValue::Arr(items) => items,
            _ => return Err(StoreError::Corrupt.into()),
        },
    };
    Ok((projects, free))
}
fn is_tombstoned(dir: &Path, safe: &str) -> Result<bool, StoreError> {
    let Some(text) = read_text(dir, chats::TOMBSTONES_FILE, store_api::TEXT_LIMIT)? else {
        return Ok(false);
    };
    match wtf16::parse(&text).map_err(|_| StoreError::Corrupt)? {
        JValue::Arr(ids) => Ok(ids
            .iter()
            .filter_map(JValue::as_str)
            .any(|id| safe_chat_id(id) == safe)),
        _ => Err(StoreError::Corrupt),
    }
}

fn context_held(dir: &Path, raw: &str, start: &str, s: &State) -> Result<GrantContext, MoveError> {
    let safe = safe_chat_id(raw);
    if safe.is_empty() || !partial::valid_start_id(start) {
        return Err(MoveError::Invalid);
    }
    let (projects, free) = read_lists(dir)?;
    let found = occurrences(&projects, &free, &safe)?;
    if found.len() > 1 {
        return Err(MoveError::Ambiguous);
    }
    Ok(GrantContext {
        canonical: safe.clone(),
        holder: found.first().map(|x| x.0.clone()).unwrap_or(Holder::Absent),
        generation: s.generations.get(&safe).copied().unwrap_or(0).to_string(),
        start: start.into(),
        tombstoned: is_tombstoned(dir, &safe)?,
    })
}
pub fn grant_context(
    data: &Path,
    user: &str,
    raw: &str,
    start: &str,
) -> Result<GrantContext, MoveError> {
    let dir = store::user_dir(data, user)?;
    with_held(&dir, |s| {
        recover_held(&dir, s, &|_| Ok(()))?;
        context_held(&dir, raw, start, s)
    })
}
/// Commit an executor transcript only while its persisted permission context is
/// still exactly current. The existing lock order covers context, lists and transcript.
pub fn write_bound_transcript(
    data: &Path,
    user: &str,
    expected: &GrantContext,
    history: &[JValue],
    base: &transcript::Base,
    still_live: impl Fn() -> bool,
) -> Result<transcript::Saved, MoveError> {
    let dir = store::user_dir(data, user)?;
    with_held(&dir, |state| {
        recover_held(&dir, state, &|_| Ok(()))?;
        let current = context_held(&dir, &expected.canonical, &expected.start, state)?;
        if current != *expected || current.tombstoned || current.holder == Holder::Absent {
            return Err(MoveError::NotFound);
        }
        if !still_live() {
            return Err(MoveError::NotFound);
        }
        transcript::write_as_held(
            data,
            user,
            &expected.canonical,
            history,
            base,
            None,
            transcript::Origin::Turn,
        )
        .map_err(|error| match error {
            transcript::TranscriptError::Store(error) => MoveError::Store(error),
            transcript::TranscriptError::InvalidChat
            | transcript::TranscriptError::Unsanitisable => MoveError::Invalid,
        })
    })
}

/// Original chat partial file, written only while its canonical owner/generation
/// is held. Account/session authority must surround this call, as for transcripts.
pub fn write_bound_partial(
    data: &Path,
    user: &str,
    expected: &GrantContext,
    entry: Option<&JValue>,
) -> Result<(), MoveError> {
    let dir = store::user_dir(data, user)?;
    with_held(&dir, |state| {
        recover_held(&dir, state, &|_| Ok(()))?;
        let current = context_held(&dir, &expected.canonical, &expected.start, state)?;
        if current != *expected || current.tombstoned || current.holder == Holder::Absent {
            return Err(MoveError::NotFound);
        }
        let file = crate::partial::file_for(&dir, &expected.canonical);
        let parent = file.parent().ok_or(MoveError::Invalid)?;
        if let Some(entry) = entry {
            match std::fs::create_dir(parent) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(StoreError::from(e).into()),
            }
            transcript::existing_dir(parent)?;
            let text = js_json::wtf16::stringify(entry).ok_or(MoveError::Invalid)?;
            if text.len() > 8 * 1024 * 1024 {
                return Err(MoveError::Invalid);
            }
            strict_write(
                parent,
                file.file_name()
                    .and_then(|n| n.to_str())
                    .ok_or(MoveError::Invalid)?,
                &text,
                &|_| Ok(()),
            )?;
        } else {
            match std::fs::symlink_metadata(parent) {
                Ok(_) => transcript::existing_dir(parent)?,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(e) => return Err(StoreError::from(e).into()),
            }
            match std::fs::remove_file(&file) {
                Ok(()) => {
                    std::fs::File::open(parent)
                        .and_then(|file| file.sync_all())
                        .map_err(StoreError::from)?;
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(StoreError::from(e).into()),
            }
        }
        Ok(())
    })
}

pub fn recover(data: &Path, user: &str) -> Result<(), MoveError> {
    let dir = store::user_dir(data, user)?;
    with_held(&dir, |s| Ok(recover_held(&dir, s, &|_| Ok(()))?))
}
fn moved_list(current: &[JValue], meta: JValue) -> Result<Vec<JValue>, MoveError> {
    let merged = chats::merge_chats(current, &[meta], &HashSet::new(), &HashSet::new())
        .map_err(|_| MoveError::Store(StoreError::Corrupt))?;
    // Never use mergeChats' cap or dedupe as an implicit eviction contract.
    let ids = |xs: &[JValue]| -> Vec<String> {
        xs.iter()
            .filter_map(|x| x.get("id").as_str().map(str::to_owned))
            .collect()
    };
    if current.len() > chats::LIST_CAP
        || merged.len() < current.len()
        || ids(current).iter().any(|id| !ids(&merged).contains(id))
    {
        return Err(MoveError::Capacity);
    }
    Ok(merged)
}
fn set_list(
    projects: &mut [JValue],
    free: &mut Vec<JValue>,
    holder: &Holder,
    items: Vec<JValue>,
) -> Result<(), MoveError> {
    match holder {
        Holder::Free => *free = items,
        Holder::Project(id) => {
            let p = store::find_mut(projects, id).ok_or(MoveError::NotFound)?;
            store::set(p, "chats", JValue::Arr(items));
        }
        Holder::Absent => return Err(MoveError::NotFound),
    }
    Ok(())
}
fn get_list(
    projects: &[JValue],
    free: &[JValue],
    holder: &Holder,
) -> Result<Vec<JValue>, MoveError> {
    match holder {
        Holder::Free => Ok(free.to_vec()),
        Holder::Project(id) => list(
            store::find(projects, id)
                .ok_or(MoveError::NotFound)?
                .get("chats"),
        ),
        Holder::Absent => Err(MoveError::NotFound),
    }
}
pub fn move_chat(
    data: &Path,
    user: &str,
    raw: &str,
    to: Holder,
    patch: Option<&JValue>,
    start: &str,
) -> Result<Moved, MoveError> {
    move_with(data, user, raw, to, patch, start, &|_| Ok(()))
}
/// Fault hook is used only by deterministic synthetic crash/fsync tests.
pub fn move_with(
    data: &Path,
    user: &str,
    raw: &str,
    to: Holder,
    patch: Option<&JValue>,
    start: &str,
    fault: Fault<'_>,
) -> Result<Moved, MoveError> {
    let dir = store::user_dir(data, user)?;
    with_held(&dir, |s| {
        recover_held(&dir, s, fault)?;
        let context = context_held(&dir, raw, start, s)?;
        if context.tombstoned || context.holder == Holder::Absent || to == Holder::Absent {
            return Err(MoveError::NotFound);
        }
        let (mut projects, mut free) = read_lists(&dir)?;
        let found = occurrences(&projects, &free, &context.canonical)?;
        let (from, mut meta) = found.first().cloned().ok_or(MoveError::NotFound)?;
        let original_meta = meta.clone();
        // Metadata routing keeps exact raw-id matching; aliases share only the security identity.
        if meta.get("id").as_str() != Some(raw) {
            return Err(MoveError::NotFound);
        }
        if matches!(&from,Holder::Project(id) if internal(id)) {
            return Err(MoveError::NotFound);
        }
        if let Holder::Project(id) = &to {
            let target = store::find(&projects, id).ok_or(MoveError::NotFound)?;
            if internal(id) || (from != to && target.get("archived") == &JValue::Bool(true)) {
                return Err(MoveError::NotFound);
            }
            let effects = chats::get_project_effects(target)
                .map_err(|_| MoveError::Store(StoreError::Corrupt))?;
            let slot = store::find_mut(&mut projects, id).ok_or(MoveError::NotFound)?;
            *slot = effects.memory;
        }
        if let Some(value) = patch {
            let value = if value == &JValue::Null {
                JValue::Null
            } else {
                frame::normalize(value).ok_or(MoveError::Invalid)?
            };
            store::set(&mut meta, "frame", value);
        }
        let source = get_list(&projects, &free, &from)?;
        let destination = get_list(&projects, &free, &to)?;
        if from != to && destination.len() >= chats::LIST_CAP {
            return Err(MoveError::Capacity);
        }
        let source = source
            .into_iter()
            .filter(|m| m.get("id").as_str() != Some(raw))
            .collect::<Vec<_>>();
        set_list(&mut projects, &mut free, &from, source)?;
        let destination = get_list(&projects, &free, &to)?;
        let moved = moved_list(&destination, meta)?;
        if !moved.iter().any(|m| m.get("id").as_str() == Some(raw)) {
            return Err(MoveError::Capacity);
        }
        // A retry after a committed response was lost is now a same-holder no-op. Do not make
        // a fresh live event for it (or rebuild a stale/missing brain from duplicate delivery).
        if from == to
            && moved.iter().find(|m| m.get("id").as_str() == Some(raw)) == Some(&original_meta)
        {
            return Ok(Moved {
                from,
                to,
                generation: context.generation,
            });
        }
        set_list(&mut projects, &mut free, &to, moved)?;
        for project in &mut projects {
            skills::reconcile(project).map_err(|_| MoveError::Store(StoreError::Corrupt))?;
        }
        let before = [
            snapshot(&dir, store::PROJECTS_FILE)?.0,
            snapshot(&dir, chats::FREE_CHATS_FILE)?.0,
        ];
        let after = [
            store::serialise(projects),
            wtf16::stringify_pretty(&JValue::Arr(free)).ok_or(StoreError::Corrupt)?,
        ];
        if after.iter().any(|a| a.len() > store_api::TEXT_LIMIT) {
            return Err(MoveError::Capacity);
        }
        let old = s.generations.get(&context.canonical).copied().unwrap_or(0);
        let generation = if from != to {
            old.checked_add(1).ok_or(StoreError::Corrupt)?
        } else {
            old
        };
        let mut random = [0u8; 16];
        OsRng
            .try_fill_bytes(&mut random)
            .map_err(|_| StoreError::Corrupt)?;
        let event = random
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        s.generations.insert(context.canonical.clone(), generation);
        s.pending = Some(Prepared {
            canonical: context.canonical,
            raw: raw.into(),
            generation,
            event,
            from: from.clone(),
            to: to.clone(),
            before,
            after,
        });
        strict_write(&dir, FILE, &encoded(s)?, fault)?;
        fault(Step::Prepared)?;
        finish_held(&dir, s, fault, journal::MoveDelivery::Live)?;
        Ok(Moved {
            from,
            to,
            generation: generation.to_string(),
        })
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use std::cell::Cell;
    const USER: &str = "11111111-2222-4333-8444-555555555555";
    const OTHER: &str = "99999999-2222-4333-8444-555555555555";
    const START: &str = "0123456789abcdef0123456789abcdef";
    fn setup() -> (tempfile::TempDir, std::path::PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let dir = store::user_dir(root.path(), USER).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(store::PROJECTS_FILE),
            r#"{"projects":[{"id":"qa-p","chats":[]},{"id":"free","chats":[]}]}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join(chats::FREE_CHATS_FILE),
            r#"[{"id":"qa c!","title":"keep title","updatedAt":1,"extra":{"qa":true}}]"#,
        )
        .unwrap();
        (root, dir)
    }
    fn go(root: &Path, to: Holder) -> Result<Moved, MoveError> {
        move_chat(root, USER, "qa c!", to, None, START)
    }
    fn texts(dir: &Path) -> [Vec<u8>; 2] {
        [
            std::fs::read(dir.join(store::PROJECTS_FILE)).unwrap(),
            std::fs::read(dir.join(chats::FREE_CHATS_FILE)).unwrap(),
        ]
    }
    fn boom() -> StoreError {
        StoreError::Io(std::io::Error::other("synthetic crash/fsync failure"))
    }

    #[test]
    fn bound_turn_commit_rechecks_holder_revision_and_live_fence() {
        let (root, _) = setup();
        let binding = grant_context(root.path(), USER, "qac", START).unwrap();
        let current = transcript::read(root.path(), USER, "qac").unwrap();
        let history =
            vec![js_json::wtf16::parse(r#"{"role":"assistant","content":"native"}"#).unwrap()];
        let base = transcript::Base::Revision(current.revision.clone());
        assert!(
            write_bound_transcript(root.path(), USER, &binding, &history, &base, || false).is_err()
        );
        assert_eq!(
            transcript::read(root.path(), USER, "qac").unwrap().revision,
            current.revision
        );
        assert!(matches!(
            write_bound_transcript(root.path(), USER, &binding, &history, &base, || true).unwrap(),
            transcript::Saved::Saved { .. }
        ));
        go(root.path(), Holder::Project("qa-p".into())).unwrap();
        assert!(
            write_bound_transcript(root.path(), USER, &binding, &history, &base, || true).is_err()
        );
    }

    #[test]
    fn roundtrip_generation_is_canonical_persistent_and_holders_are_tagged() {
        let (root, dir) = setup();
        let initial = grant_context(root.path(), USER, "qac", START).unwrap();
        assert_eq!(initial.holder, Holder::Free);
        assert_eq!(initial.generation, "0");
        assert_eq!(
            go(root.path(), Holder::Project("free".into()))
                .unwrap()
                .generation,
            "1"
        );
        let moved = grant_context(root.path(), USER, "qa c!", START).unwrap();
        assert_eq!(moved.canonical, "qac");
        assert_eq!(moved.holder.json(), json!({"kind":"project","id":"free"}));
        assert_ne!(moved.holder.json(), initial.holder.json());
        assert_eq!(go(root.path(), Holder::Free).unwrap().generation, "2");
        let next =
            grant_context(root.path(), USER, "qac", "fedcba9876543210fedcba9876543210").unwrap();
        assert_eq!(next.holder, initial.holder);
        assert_eq!(next.generation, "2");
        assert_ne!(next.start, initial.start);
        assert!(state(&dir).unwrap().pending.is_none());
        assert!(state(&dir).unwrap().completed.is_some());
        let (projects, free) = read_lists(&dir).unwrap();
        assert!(projects
            .iter()
            .all(|p| list(p.get("chats")).unwrap().is_empty()));
        assert_eq!(free[0].get("extra").get("qa"), &JValue::Bool(true));
        let events = journal::read(root.path(), USER, 0).unwrap().entries;
        assert_eq!(events.len(), 2);
        assert!(events.iter().all(|e| e["op"] == "move"
            && e["origin"] == "other"
            && e["eventId"].as_str().unwrap().len() == 32));
        assert_eq!(events[0]["generation"], "1");
        assert_eq!(events[1]["generation"], "2");
        assert_ne!(events[0]["eventId"], events[1]["eventId"]);
    }

    #[test]
    fn retry_after_a_committed_move_does_not_make_a_new_live_event() {
        let (root, dir) = setup();
        go(root.path(), Holder::Project("qa-p".into())).unwrap();
        let before = std::fs::read(dir.join(journal::FILE)).unwrap();
        assert_eq!(
            go(root.path(), Holder::Project("qa-p".into()))
                .unwrap()
                .generation,
            "1"
        );
        assert_eq!(std::fs::read(dir.join(journal::FILE)).unwrap(), before);
    }

    #[test]
    fn same_holder_frame_update_preserves_generation_and_unrelated_metadata() {
        let (root, dir) = setup();
        go(root.path(), Holder::Project("qa-p".into())).unwrap();
        let frame =
            wtf16::parse(r#"{"kind":"idea","source":"user","tags":[" QA "],"projectId":"qa-p"}"#)
                .unwrap();
        let got = move_chat(
            root.path(),
            USER,
            "qa c!",
            Holder::Project("qa-p".into()),
            Some(&frame),
            START,
        )
        .unwrap();
        assert_eq!(got.generation, "1");
        let (projects, _) = read_lists(&dir).unwrap();
        let meta = &list(store::find(&projects, "qa-p").unwrap().get("chats")).unwrap()[0];
        assert_eq!(meta.get("title").as_str(), Some("keep title"));
        assert_eq!(meta.get("frame").get("kind").as_str(), Some("idea"));
        assert_eq!(meta.get("extra").get("qa"), &JValue::Bool(true));
    }

    #[test]
    fn crash_and_uncertain_sync_at_every_publication_boundary_recover_safely() {
        // Each strict write has temp, rename and parent fsync phases: intent, two lists,
        // completion. No blind assumption that a successful rename made the parent durable.
        for phase in [Step::TempSynced, Step::Renamed, Step::ParentSynced] {
            for nth in 1..=4 {
                let (root, dir) = setup();
                let n = Cell::new(0);
                let fault = |s| {
                    if s == phase {
                        n.set(n.get() + 1);
                        if n.get() == nth {
                            return Err(boom());
                        }
                    }
                    Ok(())
                };
                assert!(
                    move_with(
                        root.path(),
                        USER,
                        "qa c!",
                        Holder::Project("qa-p".into()),
                        None,
                        START,
                        &fault
                    )
                    .is_err(),
                    "{phase:?}/{nth}"
                );
                recover(root.path(), USER).unwrap();
                let context = grant_context(root.path(), USER, "qac", START).unwrap();
                if context.generation == "0" {
                    assert_eq!(context.holder, Holder::Free); // reservation not published
                } else {
                    assert_eq!(context.generation, "1");
                    assert_eq!(context.holder, Holder::Project("qa-p".into()));
                }
                assert!(state(&dir).unwrap().pending.is_none());
            }
        }
        for phase in [
            Step::Prepared,
            Step::FirstList,
            Step::SecondList,
            Step::Journal,
            Step::Completed,
        ] {
            let (root, dir) = setup();
            // Only count the requested high-level phase.
            let fired = Cell::new(false);
            let fault = |s| {
                if s == phase && !fired.replace(true) {
                    Err(boom())
                } else {
                    Ok(())
                }
            };
            assert!(move_with(
                root.path(),
                USER,
                "qa c!",
                Holder::Project("qa-p".into()),
                None,
                START,
                &fault
            )
            .is_err());
            recover(root.path(), USER).unwrap();
            assert_eq!(
                grant_context(root.path(), USER, "qac", START)
                    .unwrap()
                    .generation,
                "1"
            );
            assert!(state(&dir).unwrap().pending.is_none());
        }
    }

    #[test]
    fn failed_journal_keeps_notification_obligation_and_duplicate_identity() {
        let (root, dir) = setup();
        std::fs::create_dir(dir.join(journal::FILE)).unwrap();
        assert!(go(root.path(), Holder::Project("qa-p".into())).is_err());
        assert!(state(&dir).unwrap().pending.is_some());
        assert!(matches!(fence(&dir), Err(StoreError::Busy)));
        std::fs::remove_dir(dir.join(journal::FILE)).unwrap();
        // A crash after durable journal append replays the same event id, not a new live move.
        let fired = Cell::new(false);
        let fault = |s| {
            if s == Step::Journal && !fired.replace(true) {
                Err(boom())
            } else {
                Ok(())
            }
        };
        assert!(with_held(&dir, |s| Ok(recover_held(&dir, s, &fault)?)).is_err());
        let first = journal::read(root.path(), USER, 0).unwrap().entries[0]["eventId"].clone();
        recover(root.path(), USER).unwrap();
        let all = journal::read(root.path(), USER, 0).unwrap().entries;
        assert_eq!(all.len(), 2);
        assert_eq!(all[1]["eventId"], first);
        assert_eq!(all[1]["generation"], "1");
        assert!(all.iter().all(|e| e["delivery"] == "recovery"));
        assert!(all
            .iter()
            .all(|e| e["rawChatId"] == "qa c!" && e["chatId"] == "qac"));
        assert!(state(&dir).unwrap().pending.is_none());
    }

    #[test]
    fn live_commit_then_same_or_new_start_recovery_preserves_identity_and_provenance() {
        for next_start in [START, "22222222222222222222222222222222"] {
            let (root, dir) = setup();
            assert!(move_with(
                root.path(),
                USER,
                "qa c!",
                Holder::Project("qa-p".into()),
                None,
                START,
                &|step| if step == Step::Journal {
                    Err(boom())
                } else {
                    Ok(())
                }
            )
            .is_err());
            let first = journal::read(root.path(), USER, 0).unwrap().entries[0].clone();
            assert_eq!(first["delivery"], "live");
            assert_eq!(first["rawChatId"], "qa c!");
            assert_eq!(first["chatId"], "qac");
            // The actual metadata id, not the reduced transcript id, finds the current brain meta.
            let (projects, free) = read_lists(&dir).unwrap();
            let found = occurrences(&projects, &free, "qac").unwrap();
            assert_eq!(found[0].1.get("id").as_str(), first["rawChatId"].as_str());
            let context = grant_context(root.path(), USER, "qac", next_start).unwrap();
            assert_eq!(context.start, next_start);
            let entries = journal::read(root.path(), USER, 0).unwrap().entries;
            assert_eq!(entries.len(), 2);
            assert_eq!(entries[1]["delivery"], "recovery");
            for key in ["eventId", "generation", "rawChatId", "chatId"] {
                assert_eq!(entries[1][key], first[key]);
            }
        }
    }

    #[test]
    fn completion_rename_uncertainty_resyncs_before_writer_or_grant_without_explicit_recover() {
        for phase in [Step::Renamed, Step::ParentSynced] {
            let (root, dir) = setup();
            let count = Cell::new(0);
            assert!(move_with(
                root.path(),
                USER,
                "qa c!",
                Holder::Project("qa-p".into()),
                None,
                START,
                &|step| {
                    if step == phase {
                        count.set(count.get() + 1);
                        if count.get() == 4 {
                            return Err(boom());
                        }
                    }
                    Ok(())
                }
            )
            .is_err());
            assert_eq!(
                count.get(),
                4,
                "fault is completion, not preparation/list publication"
            );
            assert!(
                state(&dir).unwrap().pending.is_none(),
                "completion rename is visible"
            );
            let before = texts(&dir);
            // Persistent resync refusal cannot be promoted to success from the visible clear.
            SYNC_REFUSAL.with(|v| *v.borrow_mut() = Some(dir.clone()));
            assert!(store::update(
                &dir,
                server_store::RustProjects::owned(),
                |_| -> ((), bool) { panic!("resync refusal must precede the list writer") }
            )
            .is_err());
            assert!(grant_context(root.path(), USER, "qac", START).is_err());
            SYNC_REFUSAL.with(|v| *v.borrow_mut() = None);
            assert_eq!(texts(&dir), before);
            // Both public entry points establish durability directly, BEFORE explicit recover().
            store::update(&dir, server_store::RustProjects::owned(), |_| ((), false)).unwrap();
            assert_eq!(
                grant_context(root.path(), USER, "qac", START)
                    .unwrap()
                    .generation,
                "1"
            );
            assert!(state(&dir).unwrap().pending.is_none());
        }
    }

    #[test]
    fn every_participating_writer_fences_before_cas_or_change_and_never_recovers_inside() {
        let (root, dir) = setup();
        assert!(move_with(
            root.path(),
            USER,
            "qa c!",
            Holder::Project("qa-p".into()),
            None,
            START,
            &|s| if s == Step::Prepared {
                Err(boom())
            } else {
                Ok(())
            }
        )
        .is_err());
        let before = texts(&dir);
        let called = Cell::new(false);
        assert!(matches!(
            store::update(&dir, server_store::RustProjects::owned(), |_| {
                called.set(true);
                ((), true)
            }),
            Err(StoreError::Busy)
        ));
        assert!(matches!(
            store::update_free(&dir, server_store::RustProjects::owned(), |_| {
                called.set(true);
                ((), Some(vec![]))
            }),
            Err(StoreError::Busy)
        ));
        assert!(!called.get());
        assert_eq!(texts(&dir), before);
        for which in [
            store_api::StoreFile::Projects,
            store_api::StoreFile::FreeChats,
        ] {
            let (version, text) = snapshot(&dir, which.name()).unwrap();
            assert!(matches!(
                store_api::write(
                    root.path(),
                    USER,
                    which,
                    &version,
                    text.as_deref().unwrap(),
                    server_store::RustProjects::owned()
                ),
                Err(store_api::WriteError::Store(StoreError::Busy))
            ));
        }
        recover(root.path(), USER).unwrap();
        fence(&dir).unwrap();
    }

    #[test]
    fn unknown_after_crash_writer_is_not_clobbered_and_recovery_stays_fenced() {
        let (root, dir) = setup();
        assert!(move_with(
            root.path(),
            USER,
            "qa c!",
            Holder::Project("qa-p".into()),
            None,
            START,
            &|s| if s == Step::FirstList {
                Err(boom())
            } else {
                Ok(())
            }
        )
        .is_err());
        let alien = r#"[{"id":"qa-later","title":"unrelated write after crash"}]"#;
        std::fs::write(dir.join(chats::FREE_CHATS_FILE), alien).unwrap();
        let before = texts(&dir);
        assert!(matches!(
            recover(root.path(), USER),
            Err(MoveError::Store(StoreError::Corrupt))
        ));
        assert_eq!(texts(&dir), before);
        assert!(matches!(fence(&dir), Err(StoreError::Busy)));
    }

    #[test]
    fn canonical_alias_collisions_empty_ids_and_bad_start_fail_closed() {
        let (root, dir) = setup();
        assert!(matches!(
            grant_context(root.path(), USER, "!!!", START),
            Err(MoveError::Invalid)
        ));
        assert!(matches!(
            grant_context(root.path(), USER, "qac", "bad"),
            Err(MoveError::Invalid)
        ));
        // Exact raw metadata matching is preserved even though aliases share generation.
        assert!(matches!(
            move_chat(
                root.path(),
                USER,
                "qac",
                Holder::Project("qa-p".into()),
                None,
                START
            ),
            Err(MoveError::NotFound)
        ));
        std::fs::write(
            dir.join(store::PROJECTS_FILE),
            r#"{"projects":[{"id":"qa-p","chats":[{"id":"qac"}]}]}"#,
        )
        .unwrap();
        assert!(matches!(
            grant_context(root.path(), USER, "qa c!", START),
            Err(MoveError::Ambiguous)
        ));
        assert!(matches!(
            go(root.path(), Holder::Project("qa-p".into())),
            Err(MoveError::Ambiguous)
        ));
        assert!(!dir.join(FILE).exists());
    }

    #[test]
    fn full_destinations_refuse_without_evicting_old_chat_or_any_unrelated_meta() {
        for to_free in [false, true] {
            let (root, dir) = setup();
            let many = (0..chats::LIST_CAP)
                .map(|i| json!({"id":format!("qa-{i}"),"updatedAt":100+i}))
                .collect::<Vec<_>>();
            if to_free {
                std::fs::write(dir.join(chats::FREE_CHATS_FILE), json!(many).to_string()).unwrap();
                std::fs::write(
                    dir.join(store::PROJECTS_FILE),
                    r#"{"projects":[{"id":"qa-p","chats":[{"id":"qa c!","updatedAt":1}]}]}"#,
                )
                .unwrap();
            } else {
                std::fs::write(
                    dir.join(store::PROJECTS_FILE),
                    json!({"projects":[{"id":"qa-p","chats":many}]}).to_string(),
                )
                .unwrap();
            }
            let before = texts(&dir);
            assert!(matches!(
                go(
                    root.path(),
                    if to_free {
                        Holder::Free
                    } else {
                        Holder::Project("qa-p".into())
                    }
                ),
                Err(MoveError::Capacity)
            ));
            assert_eq!(texts(&dir), before);
            assert!(!dir.join(FILE).exists());
        }
    }

    #[test]
    fn a_full_holder_can_change_frame_in_place_without_evicting_any_metadata() {
        let (root, dir) = setup();
        let mut many = (0..chats::LIST_CAP - 1)
            .map(|i| json!({"id":format!("qa-{i}"),"updatedAt":100+i}))
            .collect::<Vec<_>>();
        many.push(json!({"id":"qa c!","updatedAt":1,"title":"oldest"}));
        std::fs::write(dir.join(chats::FREE_CHATS_FILE), json!(many).to_string()).unwrap();
        let got = move_chat(
            root.path(),
            USER,
            "qa c!",
            Holder::Free,
            Some(&JValue::Null),
            START,
        )
        .unwrap();
        assert_eq!(got.generation, "0");
        let (_, free) = read_lists(&dir).unwrap();
        assert_eq!(free.len(), chats::LIST_CAP);
        for item in many {
            assert!(free
                .iter()
                .any(|m| m.get("id").as_str() == item["id"].as_str()));
        }
    }

    #[test]
    fn deleted_aliases_internal_and_archived_targets_refuse_before_reservation() {
        let (root, dir) = setup();
        std::fs::write(dir.join(chats::TOMBSTONES_FILE), r#"["qa🌟c"]"#).unwrap();
        let context = grant_context(root.path(), USER, "qa c!", START).unwrap();
        assert!(context.tombstoned);
        assert!(matches!(
            go(root.path(), Holder::Project("qa-p".into())),
            Err(MoveError::NotFound)
        ));
        std::fs::remove_file(dir.join(chats::TOMBSTONES_FILE)).unwrap();
        std::fs::write(dir.join(store::PROJECTS_FILE),
            r#"{"projects":[{"id":"qa-p","archived":true,"chats":[]},{"id":"cowork-diary-extras","chats":[]}]}"#).unwrap();
        let before = texts(&dir);
        for id in ["qa-p", "cowork-diary-extras", "missing"] {
            assert!(matches!(
                go(root.path(), Holder::Project(id.into())),
                Err(MoveError::NotFound)
            ));
        }
        assert_eq!(texts(&dir), before);
        assert!(!dir.join(FILE).exists());
        // No percent-decoding in internal payloads: raw JSON identity is already decoded by
        // the future public route. Unicode aliases still share the same safe-id generation.
        std::fs::write(dir.join(chats::FREE_CHATS_FILE), r#"[{"id":"qa🌟c"}]"#).unwrap();
        assert_eq!(
            grant_context(root.path(), USER, "qac", START)
                .unwrap()
                .holder,
            Holder::Free
        );
    }

    #[test]
    fn journal_overflow_is_an_error_and_retains_the_pending_notification() {
        let (root, dir) = setup();
        std::fs::write(
            dir.join(journal::FILE),
            format!("{}\n", json!({"seq":u64::MAX,"chatId":"qa-old"})),
        )
        .unwrap();
        let before = std::fs::read(dir.join(journal::FILE)).unwrap();
        assert!(matches!(
            go(root.path(), Holder::Project("qa-p".into())),
            Err(MoveError::Store(StoreError::Corrupt))
        ));
        assert_eq!(std::fs::read(dir.join(journal::FILE)).unwrap(), before);
        assert!(state(&dir).unwrap().pending.is_some());
        assert!(matches!(fence(&dir), Err(StoreError::Busy)));
    }

    #[test]
    fn prepared_state_symlink_never_reads_or_writes_a_sibling_account() {
        use std::os::unix::fs::symlink;
        let (root, dir) = setup();
        let outside = root.path().join("qa-outside");
        std::fs::write(&outside, "sentinel").unwrap();
        symlink(&outside, dir.join(FILE)).unwrap();
        assert!(grant_context(root.path(), USER, "qac", START).is_err());
        assert!(go(root.path(), Holder::Project("qa-p".into())).is_err());
        assert_eq!(std::fs::read_to_string(outside).unwrap(), "sentinel");
    }

    #[test]
    fn generation_overflow_corruption_and_account_deletion_never_reset_or_recreate() {
        let (root, dir) = setup();
        let mut s = State::default();
        s.generations.insert("qac".into(), u64::MAX);
        strict_write(&dir, FILE, &encoded(&s).unwrap(), &|_| Ok(())).unwrap();
        let before = texts(&dir);
        assert!(matches!(
            go(root.path(), Holder::Project("qa-p".into())),
            Err(MoveError::Store(StoreError::Corrupt))
        ));
        assert_eq!(texts(&dir), before);
        std::fs::write(
            dir.join(FILE),
            r#"{"version":1,"generations":{"qac":"01"},"pending":null}"#,
        )
        .unwrap();
        assert!(matches!(
            grant_context(root.path(), USER, "qac", START),
            Err(MoveError::Store(StoreError::Corrupt))
        ));
        assert!(matches!(fence(&dir), Err(StoreError::Corrupt)));
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(matches!(
            recover(root.path(), USER),
            Err(MoveError::Store(StoreError::Gone))
        ));
        assert!(matches!(
            grant_context(root.path(), USER, "qac", START),
            Err(MoveError::Store(StoreError::Gone))
        ));
        assert!(!dir.exists());
        assert!(matches!(
            grant_context(root.path(), OTHER, "qac", START),
            Err(MoveError::Store(StoreError::Gone))
        ));
        assert!(!store::user_dir(root.path(), OTHER).unwrap().exists());
    }

    #[test]
    fn move_versus_delete_never_revives_a_deleted_chat_or_decrements_generation() {
        for _ in 0..20 {
            let (root, _dir) = setup();
            std::thread::scope(|scope| {
                let moving = scope.spawn(|| go(root.path(), Holder::Project("qa-p".into())));
                let deleting = scope.spawn(|| {
                    for kind in [
                        crate::chat_delete::Kind::Free,
                        crate::chat_delete::Kind::Project("qa-p"),
                    ] {
                        let result = crate::chat_delete::delete(
                            root.path(),
                            USER,
                            "qa c!",
                            kind,
                            journal::Origin::Remove,
                            server_store::RustProjects::owned(),
                        )
                        .unwrap();
                        if result.deleted {
                            return true;
                        }
                    }
                    false
                });
                let result = moving.join().unwrap();
                assert!(result.is_ok() || matches!(result, Err(MoveError::NotFound)));
                assert!(deleting.join().unwrap());
            });
            let context = grant_context(root.path(), USER, "qac", START).unwrap();
            assert!(context.tombstoned);
            assert_eq!(context.holder, Holder::Absent);
            assert!(context.generation == "0" || context.generation == "1");
            assert!(matches!(
                go(root.path(), Holder::Free),
                Err(MoveError::NotFound)
            ));
        }
    }

    #[test]
    fn account_removal_at_prepared_boundary_wins_without_recreation() {
        let (root, dir) = setup();
        let fault = |step| {
            if step == Step::Prepared {
                std::fs::remove_dir_all(&dir).unwrap();
                return Err(boom());
            }
            Ok(())
        };
        assert!(move_with(
            root.path(),
            USER,
            "qa c!",
            Holder::Project("qa-p".into()),
            None,
            START,
            &fault
        )
        .is_err());
        assert!(matches!(
            recover(root.path(), USER),
            Err(MoveError::Store(StoreError::Gone))
        ));
        assert!(!dir.exists());
    }

    #[test]
    fn concurrent_moves_and_list_writes_preserve_both_counters_and_unrelated_entries() {
        let (root, dir) = setup();
        std::thread::scope(|scope| {
            let a = scope.spawn(|| go(root.path(), Holder::Project("qa-p".into())));
            let b = scope.spawn(|| go(root.path(), Holder::Project("free".into())));
            let c = scope.spawn(|| {
                store::update_free(&dir, server_store::RustProjects::owned(), |stored| {
                    let mut items = crate::free_chats::as_list(stored);
                    items.push(JValue::obj([("id", JValue::Str("qa-survivor".into()))]));
                    ((), Some(items))
                })
            });
            assert!(a.join().unwrap().is_ok());
            assert!(b.join().unwrap().is_ok());
            c.join().unwrap().unwrap();
        });
        assert_eq!(
            grant_context(root.path(), USER, "qac", START)
                .unwrap()
                .generation,
            "2"
        );
        let s = state(&dir).unwrap();
        assert!(s.pending.is_none());
        let (projects, free) = read_lists(&dir).unwrap();
        assert_eq!(occurrences(&projects, &free, "qac").unwrap().len(), 1);
        assert_eq!(
            free.iter()
                .filter(|m| m.get("id").as_str() == Some("qa-survivor"))
                .count(),
            1
        );
    }
}
