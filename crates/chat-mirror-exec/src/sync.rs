//! One mirror pass for one account (core `createChatVaultMirror(...).sync`): write what changed,
//! move what was renamed, trash what was positively deleted, then save the per-user index. The
//! calls to the Diary, their order and their bodies are core's, so the vault ends byte for byte
//! where core would have left it.

use crate::jsx;
use crate::note::{self, ROOT};
use crate::{Diary, FileError, MirrorSource};
use js_json::{wtf16, JValue};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};

/// The index's notes: JS object semantics (array-index keys first, then insertion order).
#[derive(Debug, Clone, Default)]
struct Notes(Vec<(String, JValue)>);

impl Notes {
    fn get(&self, k: &str) -> &JValue {
        self.0
            .iter()
            .find(|(key, _)| key == k)
            .map_or(&JValue::Undefined, |(_, v)| v)
    }
    fn set(&mut self, k: &str, v: JValue) {
        wtf16::set(&mut self.0, k, v);
    }
    fn delete(&mut self, k: &str) {
        self.0.retain(|(key, _)| key != k);
    }
    fn keys(&self) -> Vec<String> {
        self.0.iter().map(|(k, _)| k.clone()).collect()
    }
}

/// `{ ...value }` for a JSON value (strings are not spread: nothing writes one here).
fn spread(v: &JValue) -> Vec<(String, JValue)> {
    let mut out = match v {
        JValue::Obj(items) => items.clone(),
        JValue::Arr(items) => items
            .iter()
            .enumerate()
            .filter(|(_, x)| !matches!(x, JValue::Undefined))
            .map(|(i, x)| (i.to_string(), x.clone()))
            .collect(),
        _ => Vec::new(),
    };
    wtf16::order_keys(&mut out);
    out
}

struct Read {
    content: Option<String>,
    version: JValue,
}

/// An error of one note: `taken` remembers a name as someone else's.
struct NoteError {
    message: String,
    taken: Option<String>,
}

impl From<FileError> for NoteError {
    fn from(e: FileError) -> Self {
        NoteError {
            message: e.message,
            taken: None,
        }
    }
}
impl From<note::RenderError> for NoteError {
    fn from(e: note::RenderError) -> Self {
        NoteError {
            message: e.0.into(),
            taken: None,
        }
    }
}

fn hash(text: &str) -> String {
    Sha256::digest(jsx::utf8(text))
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// `current === base + '.md' || /^<base> \(\d+\)\.md$/.test(current)`.
fn keeps_name(current: &str, base: &str) -> bool {
    if current == format!("{base}.md") {
        return true;
    }
    current
        .strip_prefix(base)
        .and_then(|r| r.strip_prefix(" ("))
        .and_then(|r| r.strip_suffix(").md"))
        .is_some_and(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))
}

/// The intent journal of one pass: per chat, the Diary mutations about to be made, saved durably
/// before each one, so a pass that stops (a crash, a dropped future, a deadline that expired after
/// the Diary committed) leaves a record the next pass reconciles instead of mistaking its own note
/// for someone else's.
struct Journal(Vec<(String, JValue)>);

impl Journal {
    fn push(
        &mut self,
        source: &dyn MirrorSource,
        user: &str,
        chat: &str,
        intent: JValue,
    ) -> Result<(), FileError> {
        match self.0.iter_mut().find(|(k, _)| k == chat) {
            Some((_, JValue::Arr(items))) => items.push(intent),
            _ => self.0.push((chat.to_owned(), JValue::Arr(vec![intent]))),
        }
        source
            .write_intents(user, &JValue::Obj(self.0.clone()))
            .map_err(|message| FileError {
                status: None,
                message: format!("intent not saved: {message}"),
            })
    }
    /// The chat's mutations are in the index about to be saved: no longer pending.
    fn settle(&mut self, chat: &str) {
        self.0.retain(|(k, _)| k != chat);
    }
}

fn intent(op: &str, fields: Vec<(&str, JValue)>) -> JValue {
    let mut items = vec![("op".to_owned(), JValue::from(op))];
    items.extend(fields.into_iter().map(|(k, v)| (k.to_owned(), v)));
    JValue::Obj(items)
}

/// What the previous pass may have done: each journalled mutation is checked against the Diary
/// and adopted when it took effect. A write is ours when the note there holds exactly the text we
/// sent; a move when the destination exists and the old path does not; a trash when the note is
/// gone. A read the Diary does not answer fails the pass (the journal stays for the next one).
async fn reconcile(
    user: &str,
    notes: &mut Notes,
    journal: &JValue,
    files: &dyn Diary,
) -> Result<(), String> {
    let JValue::Obj(chats) = journal else {
        return Err("refused: the intent journal is misshapen".into());
    };
    for (chat, list) in chats {
        let JValue::Arr(list) = list else {
            return Err("refused: the intent journal is misshapen".into());
        };
        for i in list {
            let path = i.get("path");
            if !note::inside_mirror(path) {
                return Err(format!(
                    "refused: an intent of {chat} is outside the mirror"
                ));
            }
            let p = path.as_str().unwrap_or("");
            let read = |at: String| async move {
                read_note(files, user, &at)
                    .await
                    .map_err(|e| format!("reconcile: {}", e.message))
            };
            match i.get("op").as_str() {
                Some("write") => {
                    let now = read(p.to_owned()).await?;
                    let wanted = i.get("hash").as_str();
                    if now.content.as_deref().map(hash).as_deref() == wanted && wanted.is_some() {
                        let meta = i.get("meta").as_str().unwrap_or("");
                        notes.set(chat, index_entry(p, now.version, meta));
                    }
                }
                Some("move") => {
                    let dest = i.get("destination");
                    if !note::inside_mirror(dest) {
                        return Err(format!(
                            "refused: an intent of {chat} is outside the mirror"
                        ));
                    }
                    let d = dest.as_str().unwrap_or("");
                    let (old, new) = (read(p.to_owned()).await?, read(d.to_owned()).await?);
                    if old.content.is_none() && new.content.is_some() {
                        let meta = i.get("meta").as_str().unwrap_or("");
                        notes.set(chat, index_entry(d, new.version, meta));
                    }
                }
                Some("trash") => {
                    if read(p.to_owned()).await?.content.is_none() {
                        notes.delete(chat);
                    }
                }
                _ => return Err("refused: the intent journal is misshapen".into()),
            }
        }
    }
    Ok(())
}

/// What one pass came to: `Ok(true)` every note went through, `Ok(false)` some failed (the rest
/// ran, the index was saved), `Err` the pass threw before the index was saved (nothing saved).
///
/// Fail closed (review 2026-11-05): an unreadable, linked, corrupt or misshapen index or intent
/// journal is an `Err` before any Diary call (core read such an index as `{}`, and its own notes
/// then looked like someone else's).
pub async fn sync(
    user: &str,
    dirty: &HashSet<String>,
    source: &dyn MirrorSource,
    files: &dyn Diary,
) -> Result<bool, String> {
    let log = |parts: &[&str]| source.log(&parts.join(" "));
    let state = source.read_index(user)?;
    crate::check_index(&state)?;
    let mut notes = Notes(spread(state.get("notes")));
    let pending = source.read_intents(user)?;
    if matches!(&pending, JValue::Obj(items) if !items.is_empty()) {
        reconcile(user, &mut notes, &pending, files).await?;
        let mut out = spread(&state);
        wtf16::set(&mut out, "notes", JValue::Obj(notes.0.clone()));
        source.write_index(user, &JValue::Obj(out))?;
        source.write_intents(user, &JValue::Obj(Vec::new()))?;
        log(&["chat-vault-mirror: reconciled an interrupted pass"]);
    }
    let mut journal = Journal(Vec::new());

    // A name remembered as someone else's (#758) is freed once that note is gone from the vault.
    for key in notes.keys() {
        if !key.starts_with("taken:") {
            continue;
        }
        let entry = notes.get(&key);
        let path = if entry.is_nullish() {
            JValue::Undefined
        } else {
            entry.get("path").clone()
        };
        if !note::inside_mirror(&path) {
            notes.delete(&key);
            continue;
        }
        if let Ok(r) = read_note(files, user, path.as_str().unwrap_or("")).await {
            if r.content.is_none() {
                notes.delete(&key);
            }
        }
    }

    let lists = source.lists(user)?;
    let mut live: Vec<(JValue, Option<String>)> = Vec::new();
    for chat in &lists.free_chats {
        if chat.truthy() && chat.get("id").as_str().is_some() {
            live.push((chat.clone(), None));
        }
    }
    for project in &lists.projects {
        let chats = if project.is_nullish() {
            &JValue::Undefined
        } else {
            project.get("chats")
        };
        let chats: &[JValue] = match chats {
            JValue::Arr(c) => c,
            c if !c.truthy() => &[],
            _ => return Err("project.chats is not iterable".into()),
        };
        for chat in chats {
            if chat.truthy() && chat.get("id").as_str().is_some() {
                let name = project.get("name").as_str().unwrap_or("Project").to_owned();
                live.push((chat.clone(), Some(name)));
            }
        }
    }
    let key = |c: &JValue| {
        let v = [c.get("createdAt"), c.get("updatedAt")]
            .into_iter()
            .find(|v| v.truthy())
            .cloned()
            .unwrap_or(JValue::Num(0.0));
        jsx::to_number(&v)
    };
    live.sort_by(|a, b| {
        let d = key(&a.0) - key(&b.0);
        if d.is_nan() || d == 0.0 {
            let ia = a.0.get("id").as_str().unwrap_or("");
            let ib = b.0.get("id").as_str().unwrap_or("");
            jsx::locale_compare(ia, ib)
        } else if d < 0.0 {
            std::cmp::Ordering::Less
        } else {
            std::cmp::Ordering::Greater
        }
    });
    let id_of = |c: &JValue| c.get("id").as_str().unwrap_or("").to_owned();
    let live_ids: HashSet<String> = live.iter().map(|(c, _)| id_of(c)).collect();

    // Paths: a chat keeps its note while its folder and title stay the same.
    let mut taken: HashSet<String> = HashSet::new();
    for (id, n) in &notes.0 {
        if live_ids.contains(id) || (!n.is_nullish() && n.get("foreign").truthy()) {
            if n.is_nullish() {
                return Err("cannot read properties of null (reading 'path')".into());
            }
            if let Some(p) = n.get("path").as_str() {
                taken.insert(p.to_owned());
            }
        }
    }
    let mut want: HashMap<String, String> = HashMap::new();
    for (chat, project) in &live {
        let id = id_of(chat);
        let folder = note::folder_for(project.as_deref());
        let title = chat.get("title");
        let title = if title.is_nullish() {
            String::new()
        } else {
            js_json::to_js_string(title).map_err(|_| "cannot convert title".to_owned())?
        };
        let base = format!("{folder}/{}", note::safe_name(&title, "Untitled chat"));
        let entry = notes.get(&id);
        let current = if entry.is_nullish() {
            &JValue::Undefined
        } else {
            entry.get("path")
        };
        if let Some(cur) = current.as_str().filter(|c| !c.is_empty()) {
            if keeps_name(cur, &base) {
                want.insert(id, cur.to_owned());
                continue;
            }
            taken.remove(cur);
        }
        let mut path = format!("{base}.md");
        let mut n = 2u64;
        while taken.contains(&path) {
            path = format!("{base} ({n}).md");
            n += 1;
        }
        taken.insert(path.clone());
        want.insert(id, path);
    }

    let mut ok = true;
    let mut made: HashSet<String> = HashSet::new();
    for (chat, project) in &live {
        let id = id_of(chat);
        let Some(target) = want.get(&id).cloned() else {
            continue;
        };
        if !note::inside_mirror(&JValue::Str(target.clone())) {
            ok = false;
            log(&["chat-vault-mirror: refused path", &target]);
            continue;
        }
        let frame = chat.get("frame");
        let links = if frame.is_nullish() {
            &JValue::Undefined
        } else {
            frame.get("links")
        };
        let mut seen: HashSet<&str> = HashSet::new();
        let mut link_names: Vec<String> = Vec::new();
        if let JValue::Arr(items) = links {
            for l in items {
                if let Some(s) = l.as_str() {
                    if seen.insert(s) && s != id {
                        if let Some(p) = want.get(s) {
                            link_names.push(note::note_name(p));
                        }
                    }
                }
            }
        }
        let brain = source.brain(user, &id);
        let mut print = vec![
            chat.get("title").clone(),
            chat.get("createdAt").clone(),
            chat.get("updatedAt").clone(),
            if frame.is_nullish() {
                JValue::Null
            } else {
                frame.clone()
            },
            project.clone().map_or(JValue::Null, JValue::Str),
            JValue::Arr(link_names.iter().cloned().map(JValue::Str).collect()),
        ];
        if let Some(b) = &brain {
            print.push(b.clone());
        }
        let meta = hash(&wtf16::stringify(&JValue::Arr(print)).unwrap_or_default());
        let entry = notes.get(&id).clone();
        if entry.truthy()
            && entry.get("path").as_str() == Some(target.as_str())
            && entry.get("meta").as_str() == Some(meta.as_str())
            && !dirty.contains(&id)
        {
            continue;
        }
        match write_one(
            user,
            chat,
            project.as_deref(),
            &target,
            &entry,
            &link_names,
            brain.as_ref(),
            &meta,
            &mut made,
            &mut journal,
            source,
            files,
        )
        .await
        {
            Ok(saved) => {
                notes.set(&id, saved);
                journal.settle(&id);
            }
            Err(e) => {
                ok = false;
                if let Some(t) = e.taken {
                    notes.set(
                        &format!("taken:{t}"),
                        JValue::obj([
                            ("path", JValue::Str(t.clone())),
                            ("foreign", JValue::Bool(true)),
                        ]),
                    );
                }
                log(&["chat-vault-mirror: note failed", &id, &e.message]);
            }
        }
    }

    // Deleted (tombstoned) chats: the note goes to the Diary's Trash. Absence alone never trashes.
    let indexed: Vec<String> = notes
        .keys()
        .into_iter()
        .filter(|k| !k.starts_with("taken:"))
        .collect();
    let mut gone: Vec<String> = Vec::new();
    if live.is_empty() && !indexed.is_empty() {
        log(&[
            "chat-vault-mirror: empty chat list with mirrored notes, trash pass skipped",
            &indexed.len().to_string(),
        ]);
    } else {
        let tombstones = source.deleted(user)?;
        gone = indexed
            .into_iter()
            .filter(|id| !live_ids.contains(id) && tombstones.contains(id))
            .collect();
    }
    for id in gone {
        let entry = notes.get(&id).clone();
        let path = if entry.is_nullish() {
            JValue::Undefined
        } else {
            entry.get("path").clone()
        };
        if !note::inside_mirror(&path) {
            notes.delete(&id);
            continue;
        }
        let p = path.as_str().unwrap_or("").to_owned();
        let trashed: Result<(), FileError> = async {
            let stat = match files
                .ops(
                    user,
                    JValue::obj([
                        ("op", JValue::from("stat")),
                        ("path", JValue::Str(p.clone())),
                    ]),
                )
                .await
            {
                Ok(s) => s,
                Err(e) if e.status == Some(404) => JValue::Undefined,
                Err(e) => return Err(e),
            };
            if stat.truthy() && !stat.get("isDir").truthy() {
                journal.push(
                    source,
                    user,
                    &id,
                    intent("trash", vec![("path", JValue::Str(p.clone()))]),
                )?;
                files
                    .ops(
                        user,
                        JValue::obj([
                            ("op", JValue::from("delete")),
                            ("path", JValue::Str(p.clone())),
                            ("version", stat.get("version").clone()),
                        ]),
                    )
                    .await?;
            }
            Ok(())
        }
        .await;
        match trashed {
            Ok(()) => {
                notes.delete(&id);
                journal.settle(&id);
            }
            Err(e) => {
                ok = false;
                log(&["chat-vault-mirror: trash failed", &id, &e.message]);
            }
        }
    }

    let mut out = spread(&state);
    wtf16::set(&mut out, "notes", JValue::Obj(notes.0));
    source.write_index(user, &JValue::Obj(out))?;
    // What failed may still have taken effect (a deadline after the Diary committed): those
    // intents stay for the next pass to reconcile; the rest are in the index now.
    source.write_intents(user, &JValue::Obj(journal.0))?;
    Ok(ok)
}

async fn read_note(files: &dyn Diary, user: &str, path: &str) -> Result<Read, FileError> {
    match files.read(user, path).await {
        Ok(r) => Ok(Read {
            content: r.get("content").as_str().map(str::to_owned),
            version: match r.get("version") {
                JValue::Undefined => JValue::Null,
                v => v.clone(),
            },
        }),
        Err(e) if e.status == Some(404) => Ok(Read {
            content: None,
            version: JValue::Null,
        }),
        Err(e) => Err(e),
    }
}

async fn ensure_folder(
    files: &dyn Diary,
    user: &str,
    folder: &str,
    made: &mut HashSet<String>,
) -> Result<(), FileError> {
    for path in [ROOT, folder] {
        if made.contains(path) {
            continue;
        }
        // 405: it already exists (or the storage creates folders on write). Either way, go on.
        if let Err(e) = files.mkdir(user, path).await {
            if e.status != Some(405) {
                return Err(e);
            }
        }
        made.insert(path.to_owned());
    }
    Ok(())
}

fn taken_error(target: &str) -> NoteError {
    NoteError {
        message: "a note noevia did not write already has this name".into(),
        taken: Some(target.to_owned()),
    }
}

fn index_entry(path: &str, version: JValue, meta: &str) -> JValue {
    JValue::obj([
        ("path", JValue::Str(path.to_owned())),
        ("version", version),
        ("meta", JValue::Str(meta.to_owned())),
    ])
}

#[allow(clippy::too_many_arguments)]
async fn write_one(
    user: &str,
    chat: &JValue,
    project: Option<&str>,
    target: &str,
    entry: &JValue,
    link_names: &[String],
    brain: Option<&JValue>,
    meta: &str,
    made: &mut HashSet<String>,
    journal: &mut Journal,
    source: &dyn MirrorSource,
    files: &dyn Diary,
) -> Result<JValue, NoteError> {
    let folder = target.rsplit_once('/').map_or(target, |(f, _)| f);
    ensure_folder(files, user, folder, made).await?;
    let id = chat.get("id").as_str().unwrap_or("");
    let history = source.history(user, id).map_err(|m| NoteError {
        message: m,
        taken: None,
    })?;
    let content = note::render_note(chat, project, &history, link_names, brain)?;
    let old_path = entry.get("path");
    let at: String = match old_path {
        JValue::Str(p) if !p.is_empty() && p != target => p.clone(),
        p if p.truthy() && p.as_str().is_none() => {
            return Err(NoteError {
                message: "the index holds a path that is not text".into(),
                taken: None,
            })
        }
        _ => target.to_owned(),
    };
    let entry_version = entry.get("version");
    let mut current = read_note(files, user, &at).await?;
    if at != target {
        // Renamed or moved: move the note itself, so links in the vault follow it (#757).
        let dest = read_note(files, user, target).await?;
        let ours = dest.content.is_some()
            && !jsx::loose_null(entry_version)
            && jsx::strict_eq(&dest.version, entry_version);
        if dest.content.is_some() && !ours {
            return Err(taken_error(target));
        }
        if current.content.is_some() && dest.content.is_none() {
            journal.push(
                source,
                user,
                id,
                intent(
                    "move",
                    vec![
                        ("path", JValue::Str(at.clone())),
                        ("destination", JValue::Str(target.to_owned())),
                        ("meta", entry.get("meta").clone()),
                    ],
                ),
            )?;
            files
                .ops(
                    user,
                    JValue::obj([
                        ("op", JValue::from("move")),
                        ("path", JValue::Str(at.clone())),
                        ("destination", JValue::Str(target.to_owned())),
                        ("overwrite", JValue::Bool(false)),
                        ("version", current.version.clone()),
                    ]),
                )
                .await?;
            current = read_note(files, user, target).await?;
        } else {
            let at_inside = note::inside_mirror(&JValue::Str(at.clone()));
            if ours
                && current.content.is_some()
                && at_inside
                && !jsx::strict_eq(&current.version, entry_version)
            {
                source.log(&format!(
                    "chat-vault-mirror: old note skipped, edited {id} {at}"
                ));
            } else if ours
                && current.content.is_some()
                && at_inside
                && jsx::strict_eq(&current.version, entry_version)
            {
                // #764: the destination is already this chat's own last write; the old note goes to Trash.
                if let Err(e) = files
                    .ops(
                        user,
                        JValue::obj([
                            ("op", JValue::from("delete")),
                            ("path", JValue::Str(at.clone())),
                            ("version", current.version.clone()),
                        ]),
                    )
                    .await
                {
                    source.log(&format!(
                        "chat-vault-mirror: old note trash failed {id} {}",
                        e.message
                    ));
                }
            }
            current = dest;
        }
    } else if !entry.truthy() && current.content.is_some() {
        // A note noevia did not write is in the way: never overwrite it; take the next free name.
        return Err(taken_error(target));
    }
    if current.content.as_deref() == Some(content.as_str()) {
        return Ok(index_entry(target, current.version, meta));
    }
    journal.push(
        source,
        user,
        id,
        intent(
            "write",
            vec![
                ("path", JValue::Str(target.to_owned())),
                ("hash", JValue::Str(hash(&content))),
                ("meta", JValue::Str(meta.to_owned())),
            ],
        ),
    )?;
    // Edited in the vault since noevia last wrote it: keep that copy in Trash first (#757).
    if current.content.is_some()
        && entry.truthy()
        && (jsx::loose_null(entry_version) || !jsx::strict_eq(&current.version, entry_version))
    {
        files
            .ops(
                user,
                JValue::obj([
                    ("op", JValue::from("preserve")),
                    ("path", JValue::Str(target.to_owned())),
                    ("version", current.version.clone()),
                ]),
            )
            .await?;
    }
    let written = files
        .write(
            user,
            JValue::obj([
                ("path", JValue::Str(target.to_owned())),
                ("content", JValue::Str(content)),
                ("version", current.version),
            ]),
        )
        .await?;
    let v = if written.is_nullish() {
        JValue::Null
    } else {
        written.get("version").clone()
    };
    Ok(index_entry(
        target,
        if v.is_nullish() { JValue::Null } else { v },
        meta,
    ))
}
