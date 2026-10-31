//! `POST /api/import/conversations` (sbstndalton/noevia#2191 round 4, group
//! `rust-conversation-import`): core routes/import.cjs with conversation-import.cjs (the recovery
//! journal) and chat-import.cjs (`planImport`), answer for answer.
//!
//! Settings → Data → Import conversations reads a conversations.json made by the export and only
//! ever adds chats: never overwrites or deletes one. Content is untrusted: only user/assistant text,
//! a failed reply's failure sentence and tool names survive, with the history caps.
//!
//! The journal is Node's, file for file, so an import Node left half done is finished here:
//! `conversation-imports/<sha256 of JSON.stringify(data)>/` holds one `<n>.json` per chat
//! (`{history}`) and `manifest.json` (`{version, plan, files, projectsCreated, capped?}`), written
//! with `atomicJson` (temp + rename, 0600). Every import first finishes the journals left by
//! earlier ones (each audited, then removed); an import of the same file as a finished journal
//! answers that journal's result. Groups are applied free chats first, then each project
//! (matched by name, case-insensitively, or created through [`crate::lifecycle::create`]);
//! transcripts are published only where none is ([`crate::transcript::Base::Absent`], origin
//! `import`), a taken id gets a new one; lists are merged as `mergeChats` merges them; a chat the
//! list cap dropped is `capped` and its transcript taken back (origin `remove`) only while it is
//! still what this import published. Imports of one account run one at a time.
//!
//! sbstndalton/noevia#2196: `imported-chats.json` in the user's directory (`{version, chats}`, written
//! with `atomicJson`, ids and hashes only) remembers which local chat each source chat became, so a
//! re-import skips it whichever id it got. A source chat is its export id, or the SHA-256 of its title
//! and cleaned history when it has no usable id. It is skipped while that local chat is listed, even
//! if the file's copy changed (existing chats are never touched; the reason says so); a chat the user
//! deleted can be restored from the export again. The file appears with the first import after
//! upgrade; nothing existing is rewritten, and chats imported before it have no entry.
use crate::chats::{self, FREE_CHATS_FILE};
use crate::transcript::{self, Base, Saved};
use crate::{free_chats, journal::Origin, js, store, Ctx, Reply};
use js_json::{wtf16, JValue};
use sha2::Digest;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

/// The route (exact pathname; routes.toml).
pub const ROUTE: &str = "/api/import/conversations";
/// The audit action core routes/import.cjs records.
pub const AUDIT_ACTION: &str = "import.conversations";
/// routes/import.cjs MAX_BYTES.
pub const MAX_BYTES: usize = 64 * 1024 * 1024;
/// chat-export.cjs FORMAT.
pub const FORMAT: &str = "noevia-conversations-v1";
/// chat-import.cjs MAX_CHATS.
pub const MAX_CHATS: usize = 2000;
const MAX_MESSAGES: usize = 5000;
const MAX_CHARS: usize = 200_000;
/// chat-sources.cjs STORED_FAILURE_MAX.
const STORED_FAILURE_MAX: usize = 1000;
const MAX_ID_ATTEMPTS: usize = 100;
const TOO_LARGE: &str = "That file is over 64 MB. Split the export and import each part.";
pub const NOT_EXPORT: &str = "Choose a conversations.json from a noevia conversations export.";
const INTERRUPTED: &str = "Import was interrupted. Some conversations may already be available. Retry the same file to finish; unfinished imports are kept for recovery.";
const ROOT: &str = "conversation-imports";
const LEDGER: &str = "imported-chats.json";
const ALREADY: &str = "already in this account";
const DIFFERS: &str = "already in this account; the copy in this file differs and was not imported";
const REPEATED: &str = "repeated in this file";

/// What the route needs besides the request: a project's create (the id it made, or the status
/// it refused with) and the audit row (core auth.cjs `audit('import.conversations', …)`).
pub struct ImportEnv<'a> {
    pub create: &'a dyn Fn(&str) -> Result<String, u16>,
    pub audit: &'a dyn Fn(&JValue) -> Result<(), ()>,
}

/// Why an import stopped: a planning refusal (400 with its words) or anything else (Node's 503).
#[derive(Debug)]
enum Fail {
    Refused(String),
    Status(u16),
    Interrupted,
}

impl From<std::io::Error> for Fail {
    fn from(_: std::io::Error) -> Self {
        Fail::Interrupted
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// index.cjs `newId`: `c-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`.
fn new_id() -> String {
    format!("c-{}-{}", now_ms(), js::random36(6))
}

/// chat-import.cjs SAFE_ID `/^c-[A-Za-z0-9_-]{1,80}$/`.
fn safe_id(id: &str) -> bool {
    id.strip_prefix("c-").is_some_and(|rest| {
        (1..=80).contains(&rest.len())
            && rest
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    })
}

/// JS `String(v)` (a missing value is `undefined`).
fn js_string(v: &JValue) -> String {
    match v {
        JValue::Undefined => "undefined".into(),
        other => crate::files::js_string(other),
    }
}

fn field<'a>(v: &'a mut JValue, key: &str) -> Option<&'a mut JValue> {
    match v {
        JValue::Obj(items) => items.iter_mut().find(|(k, _)| k == key).map(|(_, v)| v),
        _ => None,
    }
}

fn items_mut(v: &mut JValue) -> Option<&mut Vec<JValue>> {
    match v {
        JValue::Arr(items) => Some(items),
        _ => None,
    }
}

fn items(v: &JValue) -> &[JValue] {
    match v {
        JValue::Arr(items) => items,
        _ => &[],
    }
}

/// chat-sources.cjs `sanitizeFailure`.
fn sanitize_failure(v: &JValue) -> Option<String> {
    let s = v.as_str()?;
    let text = wtf16::slice(js::trim(s), STORED_FAILURE_MAX);
    (!text.is_empty()).then_some(text)
}

/// chat-import.cjs `cleanHistory(history, { maxMessages: 5000, maxChars: 200000 })`.
fn clean_history(history: &JValue) -> Vec<JValue> {
    let kept: Vec<&JValue> = items(history)
        .iter()
        .filter(|m| {
            m.truthy()
                && matches!(m.get("role").as_str(), Some("user" | "assistant"))
                && matches!(m.get("content"), JValue::Str(_))
        })
        .collect();
    let from = kept.len().saturating_sub(MAX_MESSAGES);
    kept.into_iter()
        .skip(from)
        .map(|m| {
            let role = m.get("role").as_str().unwrap_or_default();
            let mut out = vec![
                ("role".to_string(), JValue::Str(role.to_string())),
                (
                    "content".to_string(),
                    JValue::Str(wtf16::slice(
                        m.get("content").as_str().unwrap_or(""),
                        MAX_CHARS,
                    )),
                ),
            ];
            if role == "assistant" {
                if let Some(failure) = sanitize_failure(m.get("failure")) {
                    out.push(("failure".into(), JValue::Str(failure)));
                }
            }
            let tools: Vec<JValue> = items(m.get("toolCalls"))
                .iter()
                .filter(|t| t.truthy() && matches!(t.get("name"), JValue::Str(_)))
                .take(50)
                .map(|t| {
                    JValue::obj([
                        (
                            "name",
                            JValue::Str(wtf16::slice(t.get("name").as_str().unwrap_or(""), 120)),
                        ),
                        ("status", JValue::Str("done".into())),
                    ])
                })
                .collect();
            if !tools.is_empty() {
                out.push(("toolCalls".into(), JValue::Arr(tools)));
            }
            JValue::Obj(out)
        })
        .collect()
}

/// A visible project as the import context lists it: `{ id, name }`.
struct Listed {
    id: JValue,
    name: JValue,
}

/// diary-extras.cjs `internalProject`.
fn internal(p: &JValue) -> bool {
    p.get("id")
        .as_str()
        .is_some_and(|id| id == "cowork-diary-extras" || id.starts_with("cowork-chat-context-"))
}

/// What planning reads of the account, from disk (the front holds no cache).
struct Account {
    projects: Vec<Listed>,
    existing: HashSet<String>,
    tombstones: HashSet<String>,
    ledger: Ledger,
}

/// Source key (`id:<export id>` or `hash:<content hash>`) to `(local chat id, content hash)`.
type Ledger = HashMap<String, (String, String)>;

/// A ledger that is valid JSON of another shape counts as empty; an unreadable one fails the import.
fn read_ledger(dir: &Path) -> Result<Ledger, Fail> {
    let Some(value) = read_json(&dir.join(LEDGER))? else {
        return Ok(Ledger::new());
    };
    let JValue::Obj(chats) = value.get("chats") else {
        return Ok(Ledger::new());
    };
    Ok(chats
        .iter()
        .filter_map(|(key, entry)| {
            let chat = entry.get("chat").as_str()?;
            let hash = entry.get("hash").as_str()?;
            Some((key.clone(), (chat.to_string(), hash.to_string())))
        })
        .collect())
}

/// Adds what this import made visible and forgets entries whose chat is no longer listed. A journal
/// without source keys (an older import's) adds nothing and leaves the file alone.
fn record_ledger(dir: &Path, files: &[JValue], visible: &HashSet<String>) -> Result<(), Fail> {
    let added: Vec<(&str, &str, &str)> = files
        .iter()
        .filter_map(|f| {
            let chat = f.get("id").as_str()?;
            Some((f.get("source").as_str()?, chat, f.get("hash").as_str()?))
        })
        .filter(|(_, chat, _)| visible.contains(*chat))
        .collect();
    if added.is_empty() {
        return Ok(());
    }
    let mut ledger = read_ledger(dir)?;
    ledger.retain(|_, (chat, _)| visible.contains(chat));
    for (source, chat, hash) in added {
        ledger.insert(source.to_string(), (chat.to_string(), hash.to_string()));
    }
    let mut chats: Vec<(String, JValue)> = ledger
        .into_iter()
        .map(|(key, (chat, hash))| {
            (
                key,
                JValue::obj([("chat", JValue::Str(chat)), ("hash", JValue::Str(hash))]),
            )
        })
        .collect();
    chats.sort_by(|a, b| a.0.cmp(&b.0));
    atomic_json(
        &dir.join(LEDGER),
        &JValue::obj([("version", JValue::Num(1.0)), ("chats", JValue::Obj(chats))]),
    )
}

/// What identifies a chat's content: its title and cleaned history, not when or where it was filed.
fn content_hash(title: &str, history: &[JValue]) -> Result<String, Fail> {
    let text = wtf16::stringify(&JValue::Arr(vec![
        JValue::Str(title.to_string()),
        JValue::Arr(history.to_vec()),
    ]))
    .ok_or(Fail::Interrupted)?;
    Ok(sha2::Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

fn read_projects(dir: &Path) -> Result<Vec<JValue>, Fail> {
    free_chats::workspace_projects(dir).map_err(|_| Fail::Interrupted)
}

fn free_list(dir: &Path) -> Result<Vec<JValue>, Fail> {
    Ok(free_chats::as_list(
        store::read_json(&dir.join(FREE_CHATS_FILE)).map_err(|_| Fail::Interrupted)?,
    ))
}

fn tombstones(dir: &Path) -> Result<HashSet<String>, Fail> {
    chats::read_tombstones(dir).map_err(|_| Fail::Interrupted)
}

/// conversation-import.cjs `persistedChatIds(dir)` (also what `existingChatIds` reads here: the
/// front has no other copy of the lists than the files).
fn persisted(dir: &Path) -> Result<HashSet<String>, Fail> {
    let mut ids = HashSet::new();
    let lists = free_list(dir)?;
    let projects = read_projects(dir)?;
    for c in lists
        .iter()
        .chain(projects.iter().flat_map(|p| items(p.get("chats"))))
    {
        if let Some(id) = c.get("id").as_str() {
            ids.insert(id.to_string());
        }
    }
    Ok(ids)
}

fn account(dir: &Path) -> Result<Account, Fail> {
    let projects = read_projects(dir)?
        .iter()
        .filter(|p| !internal(p))
        .map(|p| Listed {
            id: p.get("id").clone(),
            name: p.get("name").clone(),
        })
        .collect();
    Ok(Account {
        projects,
        existing: persisted(dir)?,
        tombstones: tombstones(dir)?,
        ledger: read_ledger(dir)?,
    })
}

/// chat-import.cjs `planImport`: `{ freeChats, projectChats, histories, skipped, imported }`, the
/// histories apart (in plan order).
/// Each planned chat's id, cleaned history, source key and content hash, in plan order.
type Histories = Vec<(String, Vec<JValue>, String, String)>;

fn plan(data: &JValue, acc: &Account) -> Result<(JValue, Histories), Fail> {
    let chats_list = match (data, data.get("format").as_str(), data.get("chats")) {
        (JValue::Obj(_), Some(FORMAT), JValue::Arr(list)) => list,
        _ => return Err(Fail::Refused(NOT_EXPORT.into())),
    };
    if chats_list.len() > MAX_CHATS {
        return Err(Fail::Refused(format!(
            "An import holds at most {MAX_CHATS} chats. Split the file and import each part."
        )));
    }
    let mut by_name: HashMap<String, &Listed> = HashMap::new();
    for p in &acc.projects {
        by_name.insert(js::trim(&js_string(&p.name)).to_lowercase(), p);
    }
    let mut free = Vec::new();
    let mut groups: Vec<(String, JValue, JValue, Vec<JValue>)> = Vec::new();
    let mut histories = Vec::new();
    let mut skipped = Vec::new();
    let mut used: HashSet<String> = HashSet::new();
    let mut first_of_id: HashMap<&str, String> = HashMap::new();
    let mut seen_hashes: HashSet<String> = HashSet::new();
    for raw in chats_list {
        if !matches!(raw, JValue::Obj(_) | JValue::Arr(_)) {
            continue;
        }
        let title_v = raw.get("title");
        let title = wtf16::slice(
            &if title_v.truthy() {
                js_string(title_v)
            } else {
                String::new()
            },
            200,
        );
        let id = raw.get("id").as_str().unwrap_or("");
        let history = clean_history(raw.get("history"));
        let hash = content_hash(&title, &history)?;
        // The first chat of an id goes by the id; a later one with other content by its hash.
        let mut source = format!("hash:{hash}");
        let mut reason = None;
        if safe_id(id) {
            match first_of_id.get(id) {
                None => {
                    first_of_id.insert(id, hash.clone());
                    source = format!("id:{id}");
                }
                Some(first) if *first == hash => reason = Some(REPEATED),
                Some(_) => {}
            }
        }
        if reason.is_none() && source.starts_with("hash:") && !seen_hashes.insert(hash.clone()) {
            reason = Some(REPEATED);
        }
        if reason.is_none() {
            reason = match acc.ledger.get(&source) {
                Some((chat, known)) if acc.existing.contains(chat) => {
                    Some(if *known == hash { ALREADY } else { DIFFERS })
                }
                _ if safe_id(id) && acc.existing.contains(id) => Some(ALREADY),
                _ => None,
            };
        }
        if let Some(reason) = reason {
            let shown = if title.is_empty() {
                "Untitled chat".to_string()
            } else {
                title
            };
            skipped.push(JValue::obj([
                ("title", JValue::Str(shown)),
                ("reason", JValue::Str(reason.into())),
            ]));
            continue;
        }
        let mut chat_id = if safe_id(id) && !acc.tombstones.contains(id) && !used.contains(id) {
            id.to_string()
        } else {
            new_id()
        };
        while used.contains(&chat_id) || acc.existing.contains(&chat_id) {
            chat_id = new_id();
        }
        used.insert(chat_id.clone());
        let updated = match raw.get("updatedAt") {
            JValue::Num(n) if n.is_finite() => JValue::Num(*n),
            _ => JValue::Num(now_ms() as f64),
        };
        let meta = JValue::obj([
            ("id", JValue::Str(chat_id.clone())),
            ("title", JValue::Str(title)),
            ("updatedAt", updated),
        ]);
        histories.push((chat_id, history, source, hash));
        let project = raw.get("project");
        let project_name = match project.get("name") {
            JValue::Str(s) if project.truthy() => wtf16::slice(js::trim(s), 120),
            _ => String::new(),
        };
        if project_name.is_empty() {
            free.push(meta);
            continue;
        }
        let key = project_name.to_lowercase();
        match groups.iter_mut().find(|g| g.0 == key) {
            Some(g) => g.3.push(meta),
            None => {
                let (pid, name) = match by_name.get(&key) {
                    Some(m) => (m.id.clone(), m.name.clone()),
                    None => (JValue::Null, JValue::Str(project_name)),
                };
                groups.push((key, pid, name, vec![meta]));
            }
        }
    }
    let imported = free.len() + groups.iter().map(|g| g.3.len()).sum::<usize>();
    let project_chats = groups
        .into_iter()
        .map(|(_, pid, name, chats)| {
            JValue::obj([
                ("projectId", pid),
                ("name", name),
                ("chats", JValue::Arr(chats)),
            ])
        })
        .collect();
    Ok((
        JValue::obj([
            ("freeChats", JValue::Arr(free)),
            ("projectChats", JValue::Arr(project_chats)),
            ("skipped", JValue::Arr(skipped)),
            ("imported", JValue::Num(imported as f64)),
        ]),
        histories,
    ))
}

/// conversation-import.cjs `atomicJson`: the temp file (0600) renamed over.
fn atomic_json(file: &Path, value: &JValue) -> Result<(), Fail> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let text = wtf16::stringify(value).ok_or(Fail::Interrupted)?;
    let mut tmp = file.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    f.write_all(text.as_bytes())?;
    drop(f);
    std::fs::rename(&tmp, file)?;
    Ok(())
}

/// `readJson(file, fallback)`: `None` for a missing file; a file that is not JSON fails the import.
fn read_json(file: &Path) -> Result<Option<JValue>, Fail> {
    match std::fs::read(file) {
        Ok(bytes) => wtf16::parse(&String::from_utf8_lossy(&bytes))
            .map(Some)
            .map_err(|_| Fail::Interrupted),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(Fail::Interrupted),
    }
}

fn staged_history(directory: &Path, file: &JValue) -> Result<Vec<JValue>, Fail> {
    let stage = file.get("stage").as_str().unwrap_or("");
    if stage.is_empty() || stage.contains('/') || stage == "." || stage == ".." {
        return Ok(Vec::new());
    }
    Ok(match read_json(&directory.join(stage))? {
        Some(v) => match v.get("history") {
            JValue::Arr(h) => h.clone(),
            _ => Vec::new(),
        },
        None => Vec::new(),
    })
}

/// conversation-import.cjs `resultFor(record)`.
fn result_for(record: &JValue) -> (f64, Vec<JValue>, JValue) {
    let plan = record.get("plan");
    let capped = items(record.get("capped"));
    let imported = match plan.get("imported") {
        JValue::Num(n) => *n,
        _ => f64::NAN,
    } - capped.len() as f64;
    let mut skipped: Vec<JValue> = items(plan.get("skipped")).to_vec();
    skipped.extend(capped.iter().map(|c| {
        JValue::obj([
            ("title", c.get("title").clone()),
            ("reason", JValue::Str("conversation list is full".into())),
        ])
    }));
    (imported, skipped, record.get("projectsCreated").clone())
}

fn result_value(imported: f64, skipped: Vec<JValue>, created: JValue) -> JValue {
    JValue::obj([
        ("imported", JValue::Num(imported)),
        ("skipped", JValue::Arr(skipped)),
        ("projectsCreated", created),
    ])
}

fn capped_ids(record: &JValue) -> HashSet<String> {
    items(record.get("capped"))
        .iter()
        .filter_map(|c| c.get("id").as_str().map(str::to_owned))
        .collect()
}

/// The chats of group `g` (0: the free chats, then each project group).
fn group_chats(record: &mut JValue, g: usize) -> Option<&mut Vec<JValue>> {
    let plan = field(record, "plan")?;
    if g == 0 {
        return items_mut(field(plan, "freeChats")?);
    }
    let group = items_mut(field(plan, "projectChats")?)?.get_mut(g - 1)?;
    items_mut(field(group, "chats")?)
}

fn group_of(record: &mut JValue, g: usize) -> Option<&mut JValue> {
    items_mut(field(field(record, "plan")?, "projectChats")?)?.get_mut(g.checked_sub(1)?)
}

fn set_key(v: &mut JValue, key: &str, value: JValue) {
    store::set(v, key, value);
}

fn publish(ctx: Ctx<'_>, chat: &str, history: &[JValue]) -> Result<Saved, Fail> {
    transcript::write_as(
        ctx.data_dir,
        ctx.user_id,
        chat,
        history,
        &Base::Absent,
        None,
        Origin::Import,
    )
    .map_err(|_| Fail::Interrupted)
}

/// conversation-import.cjs `applyRecord(directory, record, ctx, newId)`.
fn apply_record(
    ctx: Ctx<'_>,
    dir: &Path,
    directory: &Path,
    record: &mut JValue,
    env: &ImportEnv<'_>,
) -> Result<JValue, Fail> {
    let manifest = directory.join("manifest.json");
    let groups = 1 + items(record.get("plan").get("projectChats")).len();
    for g in 0..groups {
        let saved = persisted(dir)?;
        let deleted = tombstones(dir)?;
        let capped = capped_ids(record);
        let chats_now = group_chats(record, g)
            .map(|c| c.clone())
            .unwrap_or_default();
        let mut active: Vec<usize> = (0..chats_now.len())
            .filter(|&i| {
                chats_now
                    .get(i)
                    .and_then(|c| c.get("id").as_str())
                    .is_some_and(|id| {
                        !saved.contains(id) && !deleted.contains(id) && !capped.contains(id)
                    })
            })
            .collect();
        if active.is_empty() {
            continue;
        }
        let mut project_id: Option<String> = None;
        if g > 0 {
            let group = group_of(record, g).ok_or(Fail::Interrupted)?;
            project_id = group.get("projectId").as_str().map(str::to_owned);
            let projects = read_projects(dir)?;
            if let Some(id) = &project_id {
                if !projects
                    .iter()
                    .filter(|p| !internal(p))
                    .any(|p| p.get("id").as_str() == Some(id))
                {
                    project_id = None;
                }
            }
            if project_id.is_none() {
                let wanted = js_string(group.get("name")).to_lowercase();
                project_id = projects
                    .iter()
                    .filter(|p| !internal(p))
                    .find(|p| {
                        p.get("name")
                            .as_str()
                            .is_some_and(|n| js::trim(n).to_lowercase() == wanted)
                    })
                    .and_then(|p| p.get("id").as_str().map(str::to_owned));
                if project_id.is_none() {
                    // External folder allocation is intentionally never rolled back.
                    let name = js_string(group.get("name"));
                    let id = (env.create)(&name).map_err(Fail::Status)?;
                    let created = match record.get("projectsCreated") {
                        JValue::Num(n) => *n,
                        _ => 0.0,
                    } + 1.0;
                    set_key(record, "projectsCreated", JValue::Num(created));
                    project_id = Some(id);
                }
                let group = group_of(record, g).ok_or(Fail::Interrupted)?;
                set_key(
                    group,
                    "projectId",
                    JValue::Str(project_id.clone().unwrap_or_default()),
                );
                atomic_json(&manifest, record)?;
            }
        }
        // Project creation can await storage; account changes during it win.
        let visible = persisted(dir)?;
        let deleted = tombstones(dir)?;
        active.retain(|&i| {
            chats_now
                .get(i)
                .and_then(|c| c.get("id").as_str())
                .is_some_and(|id| !visible.contains(id) && !deleted.contains(id))
        });
        for &i in &active {
            let chat_id = chats_now
                .get(i)
                .and_then(|c| c.get("id").as_str())
                .unwrap_or_default()
                .to_string();
            let fi = items(record.get("files"))
                .iter()
                .position(|f| f.get("id").as_str() == Some(chat_id.as_str()));
            let file = fi
                .and_then(|fi| items(record.get("files")).get(fi))
                .cloned()
                .unwrap_or(JValue::Undefined);
            let history = staged_history(directory, &file)?;
            let mut published = publish(ctx, &chat_id, &history)?;
            let mut attempt = 0;
            while let Saved::Conflict { current, proposed } = &published {
                if current.revision == *proposed {
                    break;
                }
                // Never overwrite a pre-existing unlisted transcript or a concurrent file.
                if attempt >= MAX_ID_ATTEMPTS {
                    return Err(Fail::Interrupted);
                }
                attempt += 1;
                let taken: HashSet<String> = items(record.get("files"))
                    .iter()
                    .filter_map(|f| f.get("id").as_str().map(str::to_owned))
                    .collect();
                let id = loop {
                    let id = new_id();
                    if !visible.contains(&id) && !deleted.contains(&id) && !taken.contains(&id) {
                        break id;
                    }
                };
                if let Some(f) = fi.and_then(|fi| {
                    field(record, "files")
                        .and_then(items_mut)
                        .and_then(|l| l.get_mut(fi))
                }) {
                    set_key(f, "id", JValue::Str(id.clone()));
                }
                if let Some(c) = group_chats(record, g).and_then(|l| l.get_mut(i)) {
                    set_key(c, "id", JValue::Str(id.clone()));
                }
                atomic_json(&manifest, record)?;
                published = publish(ctx, &id, &history)?;
            }
            // `Deleted`: tombstoned meanwhile; nothing was written, the final pass reports it.
        }
        if active.is_empty() {
            continue;
        }
        let metas: Vec<JValue> = {
            let list = group_chats(record, g)
                .map(|c| c.clone())
                .unwrap_or_default();
            active
                .iter()
                .filter_map(|&i| list.get(i).cloned())
                .collect()
        };
        if g == 0 {
            // addFreeChats: FREE_CHATS merged with the chats and the tombstones, then saved.
            store::update_free(dir, ctx.switch, |stored| {
                let current = free_chats::as_list(stored);
                match tombstones(dir).and_then(|t| {
                    chats::merge_chats(&current, &metas, &t, &HashSet::new())
                        .map_err(|_| Fail::Interrupted)
                }) {
                    Ok(list) => (Ok(()), Some(list)),
                    Err(e) => (Err(e), None),
                }
            })
            .map_err(|_| Fail::Interrupted)??;
        } else if let Some(pid) = &project_id {
            // addProjectChats: saveChats(projectId, chats), with getProject's effects.
            store::update(dir, ctx.switch, |projects| {
                let Ok(Some(at)) = chats::locate(projects, pid) else {
                    return (Ok(()), false);
                };
                let Some(effects) = projects
                    .get(at)
                    .and_then(|p| chats::get_project_effects(p).ok())
                else {
                    return (Err(Fail::Interrupted), false);
                };
                match chats::save_chats(dir, projects, at, effects.memory, &metas) {
                    Ok(_) => (Ok(()), true),
                    Err(_) => (Err(Fail::Interrupted), false),
                }
            })
            .map_err(|_| Fail::Interrupted)??;
        }
        let committed = persisted(dir)?;
        let deleted_now = tombstones(dir)?;
        let dropped: Vec<JValue> = metas
            .iter()
            .filter(|c| {
                c.get("id")
                    .as_str()
                    .is_some_and(|id| !committed.contains(id) && !deleted_now.contains(id))
            })
            .map(|c| {
                JValue::obj([
                    ("id", c.get("id").clone()),
                    ("title", c.get("title").clone()),
                ])
            })
            .collect();
        if !dropped.is_empty() {
            let mut all: Vec<JValue> = items(record.get("capped")).to_vec();
            all.extend(dropped);
            let mut seen = HashSet::new();
            all.retain(|c| seen.insert(wtf16::stringify(c.get("id")).unwrap_or_default()));
            set_key(record, "capped", JValue::Arr(all));
            atomic_json(&manifest, record)?;
        }
    }
    // A deleted imported chat may have left a file when its own unlink failed: remove only what
    // this import published, only if tombstoned and still absent from the lists.
    let visible = persisted(dir)?;
    let deleted = tombstones(dir)?;
    let files: Vec<JValue> = items(record.get("files")).to_vec();
    let id_of = |f: &JValue| f.get("id").as_str().unwrap_or("").to_string();
    for f in &files {
        let id = id_of(f);
        if deleted.contains(&id) && !visible.contains(&id) {
            take_back(ctx, &id, &staged_history(directory, f)?)?;
        }
    }
    let capped = capped_ids(record);
    for f in &files {
        let id = id_of(f);
        if capped.contains(&id) {
            take_back(ctx, &id, &staged_history(directory, f)?)?;
        }
    }
    if files.iter().any(|f| {
        let id = id_of(f);
        !visible.contains(&id) && !deleted.contains(&id) && !capped.contains(&id)
    }) {
        return Err(Fail::Interrupted);
    }
    record_ledger(dir, &files, &visible)?;
    let (mut imported, mut skipped, created) = result_for(record);
    let plan = record.get("plan");
    let mut removed = Vec::new();
    for c in items(plan.get("freeChats")).iter().chain(
        items(plan.get("projectChats"))
            .iter()
            .flat_map(|g| items(g.get("chats"))),
    ) {
        if c.get("id")
            .as_str()
            .is_some_and(|id| deleted.contains(id) && !visible.contains(id))
        {
            removed.push(c.get("title").clone());
        }
    }
    imported -= removed.len() as f64;
    skipped.extend(removed.into_iter().map(|title| {
        JValue::obj([
            ("title", title),
            (
                "reason",
                JValue::Str("deleted while import was pending".into()),
            ),
        ])
    }));
    Ok(result_value(imported, skipped, created))
}

fn take_back(ctx: Ctx<'_>, chat: &str, history: &[JValue]) -> Result<(), Fail> {
    transcript::remove_if_as(ctx.data_dir, ctx.user_id, chat, history, Origin::Remove)
        .map(|_| ())
        .map_err(|_| Fail::Interrupted)
}

fn hex64(name: &str) -> bool {
    name.len() == 64
        && name
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// One account's imports run one at a time (conversation-import.cjs `locks`, per directory).
fn account_lock(dir: &Path) -> Arc<Mutex<()>> {
    static LOCKS: OnceLock<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> = OnceLock::new();
    let map = LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut held = map
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    Arc::clone(held.entry(dir.to_path_buf()).or_default())
}

fn remove_dir(directory: &Path) -> Result<(), Fail> {
    // rmSync(recursive) without following a symlink out (remove_dir_all does not follow them).
    std::fs::remove_dir_all(directory).map_err(Fail::from)
}

/// conversation-import.cjs `importConversations(data, ctx, newId, onComplete)`.
fn import(ctx: Ctx<'_>, dir: &Path, data: &JValue, env: &ImportEnv<'_>) -> Result<JValue, Fail> {
    let lock = account_lock(dir);
    let _held = lock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // Reject invalid input before recovering or writing anything.
    plan(data, &account(dir)?)?;
    let fingerprint: String =
        sha2::Sha256::digest(wtf16::stringify(data).ok_or(Fail::Interrupted)?.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
    let root = dir.join(ROOT);
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&root)?;
    }
    let mut resumed = None;
    for entry in std::fs::read_dir(&root)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !entry.file_type()?.is_dir() || !hex64(&name) {
            continue;
        }
        let directory = root.join(&name);
        let record = read_json(&directory.join("manifest.json"))?;
        let Some(mut record) = record.filter(JValue::truthy) else {
            // No promotion before the manifest.
            remove_dir(&directory)?;
            continue;
        };
        let result = apply_record(ctx, dir, &directory, &mut record, env)?;
        (env.audit)(&result).map_err(|()| Fail::Interrupted)?;
        remove_dir(&directory)?;
        if name == fingerprint {
            resumed = Some(result);
        }
    }
    if let Some(result) = resumed {
        return Ok(result);
    }
    let (plan, histories) = plan(data, &account(dir)?)?;
    if histories.is_empty() {
        let result = result_value(0.0, items(plan.get("skipped")).to_vec(), JValue::Num(0.0));
        (env.audit)(&result).map_err(|()| Fail::Interrupted)?;
        return Ok(result);
    }
    let directory = root.join(&fingerprint);
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().mode(0o700).create(&directory)?;
    }
    let mut files = Vec::with_capacity(histories.len());
    for (index, (id, history, source, hash)) in histories.into_iter().enumerate() {
        let stage = format!("{index}.json");
        atomic_json(
            &directory.join(&stage),
            &JValue::obj([("history", JValue::Arr(history))]),
        )?;
        files.push(JValue::obj([
            ("id", JValue::Str(id)),
            ("stage", JValue::Str(stage)),
            ("source", JValue::Str(source)),
            ("hash", JValue::Str(hash)),
        ]));
    }
    let mut record = JValue::obj([
        ("version", JValue::Num(1.0)),
        ("plan", plan),
        ("files", JValue::Arr(files)),
        ("projectsCreated", JValue::Num(0.0)),
    ]);
    atomic_json(&directory.join("manifest.json"), &record)?;
    let result = apply_record(ctx, dir, &directory, &mut record, env)?;
    (env.audit)(&result).map_err(|()| Fail::Interrupted)?;
    remove_dir(&directory)?;
    Ok(result)
}

/// POST /api/import/conversations; `body` is the request as read up to [`MAX_BYTES`] (`over` past it).
pub fn post(ctx: Ctx<'_>, body: &[u8], over: bool, env: &ImportEnv<'_>) -> Reply {
    if over {
        return Reply::error(413, TOO_LARGE);
    }
    let Ok(data) = wtf16::parse(&String::from_utf8_lossy(body)) else {
        return Reply::error(400, NOT_EXPORT);
    };
    let dir = match store::user_dir(ctx.data_dir, ctx.user_id) {
        Ok(d) => d,
        Err(_) => return Reply::error(503, INTERRUPTED),
    };
    match import(ctx, &dir, &data, env) {
        Ok(result) => Reply::json(200, wtf16::stringify(&result).unwrap_or_default()),
        Err(Fail::Refused(message)) => Reply::error(400, &message),
        Err(Fail::Status(status)) => Reply::error(status, INTERRUPTED),
        Err(Fail::Interrupted) => Reply::error(503, INTERRUPTED),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;

    fn acc() -> Account {
        Account {
            projects: vec![Listed {
                id: JValue::Str("p1".into()),
                name: JValue::Str(" Work ".into()),
            }],
            existing: ["c-old".to_string()].into_iter().collect(),
            tombstones: ["c-gone".to_string()].into_iter().collect(),
            ledger: Ledger::new(),
        }
    }

    #[test]
    fn plans_as_chat_import_cjs() {
        let data = wtf16::parse(
            r#"{"format":"noevia-conversations-v1","chats":[
            {"id":"c-old","title":"Old"},
            {"id":"c-new","title":"New","updatedAt":5,"project":{"name":" work "},"history":[
              {"role":"user","content":"hi","reasoning":"x"},{"role":"system","content":"no"},
              {"role":"assistant","content":"yo","failure":"  broke  ","toolCalls":[{"name":"t","args":1},{"x":1}]}]},
            {"id":"c-gone","title":"","project":{"name":"Fresh"}},
            {"id":"bad id"},
            null, 3]}"#,
        )
        .unwrap();
        let (plan, histories) = plan(&data, &acc()).unwrap();
        assert_eq!(plan.get("imported"), &JValue::Num(3.0));
        assert_eq!(
            wtf16::stringify(plan.get("skipped")).unwrap(),
            r#"[{"title":"Old","reason":"already in this account"}]"#
        );
        let groups = items(plan.get("projectChats"));
        assert_eq!(groups[0].get("projectId"), &JValue::Str("p1".into()));
        assert_eq!(groups[0].get("name"), &JValue::Str(" Work ".into()));
        assert_eq!(groups[1].get("projectId"), &JValue::Null);
        assert_eq!(groups[1].get("name"), &JValue::Str("Fresh".into()));
        // A tombstoned id comes back under a new one; an unsafe id too.
        let gone = items(groups[1].get("chats"))[0].get("id").as_str().unwrap();
        assert!(gone.starts_with("c-") && gone != "c-gone");
        assert_eq!(items(plan.get("freeChats")).len(), 1);
        assert_eq!(
            wtf16::stringify(&JValue::Arr(histories[0].1.clone())).unwrap(),
            r#"[{"role":"user","content":"hi"},{"role":"assistant","content":"yo","failure":"broke","toolCalls":[{"name":"t","status":"done"}]}]"#
        );
    }

    #[test]
    fn a_known_source_is_skipped_while_its_chat_is_listed() {
        let hash = |title: &str, content: &str| {
            let message = wtf16::parse(&format!(r#"{{"role":"user","content":"{content}"}}"#));
            content_hash(title, &[message.unwrap()]).unwrap()
        };
        let chat = |id: &str, content: &str| {
            format!(
                r#"{{"id":"{id}","title":"T","history":[{{"role":"user","content":"{content}"}}]}}"#
            )
        };
        let export = |chats: &[String]| {
            wtf16::parse(&format!(
                r#"{{"format":"noevia-conversations-v1","chats":[{}]}}"#,
                chats.join(",")
            ))
            .unwrap()
        };
        let reasons = |plan: &JValue| -> Vec<String> {
            items(plan.get("skipped"))
                .iter()
                .map(|s| s.get("reason").as_str().unwrap().to_string())
                .collect()
        };
        let mut a = acc();
        for (key, title, content) in [("id:c-gone", "T", "same"), ("id:c-moved", "T", "old")] {
            a.ledger
                .insert(key.into(), ("c-old".into(), hash(title, content)));
        }
        a.ledger
            .insert("id:c-stale".into(), ("c-deleted".into(), hash("T", "x")));
        let (p, h) = plan(
            &export(&[
                chat("c-gone", "same"),
                chat("c-moved", "new"),
                chat("c-stale", "x"),
                chat("c-twice", "a"),
                chat("c-twice", "a"),
                chat("c-twice", "b"),
                r#"{"title":"No id","history":[]}"#.into(),
                r#"{"id":"bad id","title":"No id","history":[]}"#.into(),
            ]),
            &a,
        )
        .unwrap();
        assert_eq!(reasons(&p), [ALREADY, DIFFERS, REPEATED, REPEATED]);
        // A chat the user deleted comes back; a repeated id with other content is its own chat.
        let sources: Vec<&str> = h.iter().map(|x| x.2.split(':').next().unwrap()).collect();
        assert_eq!(sources, ["id", "id", "hash", "hash"]);
        assert_eq!(h[0].2, "id:c-stale");
    }

    #[test]
    fn refuses_what_is_not_an_export() {
        for text in [
            "null",
            "[]",
            r#"{"format":"x","chats":[]}"#,
            r#"{"format":"noevia-conversations-v1","chats":{}}"#,
        ] {
            assert!(matches!(
                plan(&wtf16::parse(text).unwrap(), &acc()),
                Err(Fail::Refused(m)) if m == NOT_EXPORT
            ));
        }
        let many = format!(
            r#"{{"format":"noevia-conversations-v1","chats":[{}]}}"#,
            vec!["{}"; 2001].join(",")
        );
        assert!(matches!(
            plan(&wtf16::parse(&many).unwrap(), &acc()),
            Err(Fail::Refused(m)) if m.starts_with("An import holds at most 2000 chats.")
        ));
    }

    #[test]
    fn ids_have_node_shape() {
        let id = new_id();
        assert!(safe_id(&id), "{id}");
        assert!(safe_id("c-a_b-1") && !safe_id("c-") && !safe_id("x-1") && !safe_id("c-a b"));
        assert!(!safe_id(&format!("c-{}", "a".repeat(81))));
    }
}
