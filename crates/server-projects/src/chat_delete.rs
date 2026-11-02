//! A chat's delete, one operation (sbstndalton/noevia#1071 slice 6): core projects.cjs
//! `deleteChat` / `deleteFreeChat` / `removeChatAfterSave` and `purgeProjectChats`, moved behind the
//! Rust store so the whole cascade runs in one hold of the per-user transcript lock
//! ([`crate::transcript::LOCK_NAME`]).
//!
//! Order (Node's, #554 and #1914):
//! 1. the tombstone (`deleted-chats.json`, the raw id and its `[A-Za-z0-9_-]` reduction, as
//!    chat-lists.cjs `addTombstone` records them; at most [`TOMBSTONE_CAP`], the newest kept);
//! 2. the list without the chat (`free-chats.json`, or the project's `chats` in `projects.json`),
//!    written under that file's own lock as every other list save is ([`store::update_free`],
//!    [`store::update`]). A list save that fails restores the prior ordered tombstone list
//!    (including old ids displaced by the cap) and removes nothing;
//! 3. only then the chat's own files: its transcript (`history-<safe id>.json`), context meter
//!    (`context-<sha256(JSON.stringify(id))>.json`), brain note (`chat-brains/<sha256(id)>.json`)
//!    and saved partial reply (`chat-partials/<sha256(id)>.json`; Node never removed it,
//!    sbstndalton/noevia#2147).
//!
//! All of it under the transcript lock, which every transcript writer takes and under which each
//! re-reads the tombstones ([`crate::transcript`]): a running turn's late write either lands before
//! the delete (and is removed by it) or finds the tombstone and writes nothing. Node unlinked the
//! transcript with nothing held, so a late write could recreate it (sbstndalton/noevia#2154).
//!
//! The chat's attachments project (`cowork-chat-context-<id>`, with its RAG index files and
//! storage folder) is not touched here: Node's indexer writes those files, and Node frees the
//! project, its own RAG files included, once the chat is tombstoned and in no list (core projects.cjs
//! `removeOrphanChatContexts`, on the next workspace load; decision D4 of the #1071 design).
//!
//! Crash safety: a crash after the tombstone leaves the chat tombstoned, so no writer brings it
//! back and the next list save drops it. A delete asked again finishes the job: a chat that is
//! tombstoned and in no list is a chat the user deleted (ids are random and a tombstoned id never
//! returns, the proof Node's orphan reaper uses), so its own leftover files are removed
//! ([`Outcome::swept`]). A bounded workspace cleanup also retries this already-confirmed deletion; no other file is removed.
//!
//! Each successful delete or recovery retry journals one entry ([`crate::journal`], op `remove`, the
//! caller's origin), so Node's change-journal reader updates the chat's vault note. Recovery notices
//! are at least once: a crash can remove the transcript before the journal append succeeds.
//!
//! The user's directory is never created ([`StoreError::Gone`]). Every path is inside it.

use crate::chats::{self, FREE_CHATS_FILE, TOMBSTONES_FILE};
use crate::context_window;
use crate::free_chats;
use crate::history::{history_file, safe_chat_id};
use crate::journal::{self, Origin};
use crate::js;
use crate::partial;
use crate::skills;
use crate::store::{self, StoreError};
use crate::transcript;
use crate::{Ctx, Reply};
use js_json::wtf16;
use js_json::JValue;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// The release pairing marker (sbstndalton/noevia#2161's explicit markers): this front's internal
/// store answers `POST /v1/chat/delete`. noevia deploy/tools/assemble-release.sh reads the quoted
/// literal from this file to pair it with a core that deletes through it
/// (`'noevia-chat-delete-client/1'` in core server/project-store-client.cjs). Keep the literal.
pub const CHAT_DELETE_STORE_MARKER: &str = "noevia-chat-delete-store/1";

/// chat-lists.cjs `TOMBSTONE_CAP`.
pub const TOMBSTONE_CAP: usize = 5000;
/// chat-brain.cjs `BRAIN_DIR`.
pub const BRAIN_DIR: &str = "chat-brains";
/// The routes' `saveFailureReply` message (routes/chat-lists.cjs, routes/projects.cjs).
pub const DELETE_FAILED: &str = "could not delete the chat; nothing was changed";

/// Which list the chat is deleted from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind<'a> {
    /// `free-chats.json` (DELETE /api/freechats/{id}).
    Free,
    /// The `chats` of this project (DELETE /api/projects/{p}/chats/{c}).
    Project(&'a str),
    /// A chat no list holds any more (a deleted project's chats, core `purgeProjectChats`): only
    /// the tombstone and the files. Refused ([`DeleteError::Listed`]) while any list holds it.
    Unlisted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Outcome {
    /// The chat was taken out of its list (or, [`Kind::Unlisted`], tombstoned) by this call.
    pub deleted: bool,
    /// Not in the list asked, but already tombstoned and in no list: its leftover files were
    /// removed (a delete that a crash cut short, asked again).
    pub swept: bool,
    /// Files that could not be removed (logged); a later delete of the same chat retries them.
    pub leftovers: usize,
}

#[derive(Debug)]
pub enum DeleteError {
    /// An empty chat id.
    InvalidChat,
    /// [`Kind::Unlisted`] for a chat a list still holds: nothing changed.
    Listed,
    /// A stored list of a shape Node throws on (its TypeError, a 500): nothing changed.
    Unsupported,
    Store(StoreError),
}

impl From<StoreError> for DeleteError {
    fn from(e: StoreError) -> Self {
        DeleteError::Store(e)
    }
}

/// The steps after which a test may stop the operation as a crash would (no clean-up at all).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Tombstoned,
    ListSaved,
    TranscriptRemoved,
    MeterRemoved,
    BrainRemoved,
    PartialRemoved,
}

fn sha_hex(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// chat-brain.cjs `fileFor(dir, chatId)`.
pub fn brain_file(dir: &Path, chat_id: &str) -> PathBuf {
    dir.join(BRAIN_DIR)
        .join(format!("{}.json", sha_hex(chat_id)))
}

/// The ids a delete covers: the raw id and its reduction (`addTombstone`'s pair), once each.
fn ids_of(raw: &str) -> Vec<String> {
    let safe = safe_chat_id(raw);
    let mut ids = vec![raw.to_string()];
    if !safe.is_empty() && safe != raw {
        ids.push(safe);
    }
    ids
}

/// The tombstone list's strings (chat-lists.cjs `readTombstones`, an ordered Set).
fn tombstone_list(dir: &Path) -> Result<Vec<String>, StoreError> {
    let mut out: Vec<String> = Vec::new();
    if let Some(JValue::Arr(items)) = store::read_json(&dir.join(TOMBSTONES_FILE))? {
        for v in items {
            if let JValue::Str(s) = v {
                if !out.contains(&s) {
                    out.push(s);
                }
            }
        }
    }
    Ok(out)
}

fn write_tombstones(dir: &Path, ids: &[String]) -> Result<(), StoreError> {
    let list = JValue::Arr(ids.iter().cloned().map(JValue::Str).collect());
    let text = wtf16::stringify(&list).ok_or(StoreError::Corrupt)?;
    transcript::write_atomic(&dir.join(TOMBSTONES_FILE), &text)
}

/// Add both tombstone forms, preserving the entire previous order for rollback.
fn add_tombstone(dir: &Path, raw: &str) -> Result<Vec<String>, StoreError> {
    let added = ids_of(raw);
    let previous = tombstone_list(dir)?;
    let mut ids = previous.clone();
    ids.retain(|x| !added.contains(x));
    ids.extend(added);
    let from = ids.len().saturating_sub(TOMBSTONE_CAP);
    write_tombstones(dir, ids.get(from..).ok_or(StoreError::Corrupt)?)?;
    Ok(previous)
}

/// Restore the complete prior order, including ids displaced by the cap.
/// The caller still holds the transcript lock, so no intervening delete is lost.
fn take_back(dir: &Path, previous: Option<&[String]>) {
    let Some(previous) = previous else { return };
    if let Err(e) = write_tombstones(dir, previous) {
        eprintln!(
            "noevia-server: could not take back a chat tombstone after a failed delete: {e:?}"
        );
    }
}

fn tombstoned(dir: &Path, raw: &str) -> Result<bool, StoreError> {
    let ids = tombstone_list(dir)?;
    Ok(ids_of(raw).iter().any(|i| ids.contains(i)))
}

/// Whether `c` is a chat meta with one of `ids`.
fn is_chat(c: &JValue, ids: &[String]) -> bool {
    c.is_object()
        && c.get("id")
            .as_str()
            .is_some_and(|id| ids.iter().any(|i| i == id))
}

/// Whether any list of the account (the free chats, any project's chats) holds the chat.
fn listed_anywhere(dir: &Path, raw: &str) -> Result<bool, StoreError> {
    let ids = ids_of(raw);
    let held = |list: &JValue| matches!(list, JValue::Arr(items) if items.iter().any(|c| is_chat(c, &ids)));
    if let Some(free) = store::read_json(&dir.join(FREE_CHATS_FILE))? {
        if held(&free) {
            return Ok(true);
        }
    }
    Ok(store::read(dir)?.iter().any(|p| held(p.get("chats"))))
}

/// What happened inside a list's lock.
enum InList {
    /// Not in that list: nothing written.
    Absent,
    /// Taken out (the prior tombstone list is saved in the caller).
    Removed,
    /// Node throws on this data: nothing written.
    Unsupported,
    /// A test's crash.
    Crashed(StoreError),
    /// The tombstone could not be written: nothing else either.
    Failed(StoreError),
}

/// The free list without the chat: under `free-chats.json.lock`, the tombstone first.
fn remove_from_free(
    dir: &Path,
    raw: &str,
    previous: &mut Option<Vec<String>>,
    switch: server_store::RustProjects,
    fault: &dyn Fn(Step) -> Result<(), StoreError>,
) -> Result<InList, StoreError> {
    store::update_free(dir, switch, |stored| {
        // `Array.from(FREE_CHATS).filter((c) => c.id !== chatId)`.
        let list = free_chats::as_list(stored);
        let ids = [raw.to_string()];
        let kept: Vec<JValue> = list.iter().filter(|c| !is_chat(c, &ids)).cloned().collect();
        if kept.len() == list.len() {
            return (InList::Absent, None);
        }
        match add_tombstone(dir, raw) {
            Ok(prior) => *previous = Some(prior),
            Err(e) => return (InList::Failed(e), None),
        }
        if let Err(e) = fault(Step::Tombstoned) {
            return (InList::Crashed(e), None);
        }
        (InList::Removed, Some(kept))
    })
}

/// The project's list without the chat: under the projects.json lock, `getProject`'s own changes
/// kept (the context project's heal, the instruction-skills reconcile), every project reconciled
/// as `saveProjects` does, the tombstone first.
fn remove_from_project(
    dir: &Path,
    project_id: &str,
    raw: &str,
    previous: &mut Option<Vec<String>>,
    switch: server_store::RustProjects,
    fault: &dyn Fn(Step) -> Result<(), StoreError>,
) -> Result<InList, StoreError> {
    store::update(dir, switch, |projects| {
        let at = match chats::locate(projects, project_id) {
            Err(()) => return (InList::Unsupported, false),
            Ok(None) => return (InList::Absent, false),
            Ok(Some(i)) => i,
        };
        let Some(project) = projects.get(at) else {
            return (InList::Unsupported, false);
        };
        let Ok(effects) = chats::get_project_effects(project) else {
            return (InList::Unsupported, false);
        };
        let healed = effects.saved.is_some();
        let mut target = effects.memory;
        let ids = [raw.to_string()];
        // `(p.chats || []).filter((c) => c.id !== chatId)`.
        let (before, kept): (usize, Vec<JValue>) = match target.get("chats") {
            JValue::Arr(items) => (
                items.len(),
                items
                    .iter()
                    .filter(|c| !is_chat(c, &ids))
                    .cloned()
                    .collect(),
            ),
            v if !v.truthy() => (0, Vec::new()),
            _ => return (InList::Unsupported, false),
        };
        if kept.len() == before {
            // getProject saved its own changes before Node's delete looked for the chat.
            if healed {
                if let Some(slot) = projects.get_mut(at) {
                    *slot = target;
                }
            }
            return (InList::Absent, healed);
        }
        store::set(&mut target, "chats", JValue::Arr(kept));
        // saveProjects(PROJECTS): every project reconciled, all planned before any is changed.
        let mut plans = Vec::with_capacity(projects.len());
        for (i, p) in projects.iter().enumerate() {
            let p = if i == at { &target } else { p };
            match skills::plan(p) {
                Ok(plan) => plans.push(plan),
                Err(_) => return (InList::Unsupported, false),
            }
        }
        match add_tombstone(dir, raw) {
            Ok(prior) => *previous = Some(prior),
            Err(e) => return (InList::Failed(e), false),
        }
        if let Err(e) = fault(Step::Tombstoned) {
            return (InList::Crashed(e), false);
        }
        if let Some(slot) = projects.get_mut(at) {
            *slot = target;
        }
        for (p, plan) in projects.iter_mut().zip(plans) {
            if let skills::Plan::Set { value, .. } = plan {
                store::set(p, "instructionSkills", value);
            }
        }
        (InList::Removed, true)
    })
}

/// Removes `file` if it is there; whether it was, or the error (logged by the caller).
fn remove_file(file: &Path) -> Result<bool, std::io::Error> {
    use rustix::fs::{open, openat, unlinkat, AtFlags, Mode, OFlags};
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let parent = file
        .parent()
        .ok_or_else(|| std::io::Error::other("missing parent"))?;
    let name = file
        .file_name()
        .ok_or_else(|| std::io::Error::other("missing name"))?;
    // Hold a nofollow-opened directory fd through unlinkat. Renaming/swapping the directory
    // pathname cannot redirect the unlink into a sibling account. Leaf symlinks are unlinked.
    let result = (|| -> Result<(), rustix::io::Errno> {
        if let Some(account) = parent.parent().filter(|_| {
            matches!(
                parent.file_name().and_then(|n| n.to_str()),
                Some(BRAIN_DIR | partial::DIR)
            )
        }) {
            let account_fd = open(account, flags, Mode::empty())?;
            let parent_name = parent.file_name().ok_or(rustix::io::Errno::INVAL)?;
            let parent_fd = openat(&account_fd, parent_name, flags, Mode::empty())?;
            unlinkat(&parent_fd, name, AtFlags::empty())
        } else {
            let parent_fd = open(parent, flags, Mode::empty())?;
            unlinkat(&parent_fd, name, AtFlags::empty())
        }
    })();
    match result {
        Ok(()) => Ok(true),
        Err(rustix::io::Errno::NOENT) => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// The chat's own files, each step reported to `fault`. Returns whether the transcript existed and
/// how many files could not be removed.
fn remove_files(
    dir: &Path,
    raw: &str,
    fault: &dyn Fn(Step) -> Result<(), StoreError>,
) -> Result<(bool, usize), StoreError> {
    let ids = ids_of(raw);
    let safe = safe_chat_id(raw);
    let mut leftovers = 0usize;
    let mut gone = |file: PathBuf| -> bool {
        match remove_file(&file) {
            Ok(was) => was,
            Err(e) => {
                leftovers += 1;
                eprintln!(
                    "noevia-server: a deleted chat's file could not be removed ({}): {e}",
                    file.file_name().and_then(|n| n.to_str()).unwrap_or("?")
                );
                false
            }
        }
    };
    // workspace.cjs historyPath(id): the reduced id; an id that reduces to nothing has none.
    let had_transcript = !safe.is_empty() && gone(history_file(dir, &safe));
    fault(Step::TranscriptRemoved)?;
    for id in &ids {
        gone(context_window::state_file(dir, id));
    }
    fault(Step::MeterRemoved)?;
    for id in &ids {
        gone(brain_file(dir, id));
    }
    fault(Step::BrainRemoved)?;
    for id in &ids {
        gone(partial::file_for(dir, id));
    }
    fault(Step::PartialRemoved)?;
    Ok((had_transcript, leftovers))
}

/// Deletes the chat `chat` (the id as the client names it) of `user` from `kind`'s list. See the
/// module. `origin` is journalled on the change's entry ([`Origin::Remove`] for a user's delete).
pub fn delete(
    data_dir: &Path,
    user: &str,
    chat: &str,
    kind: Kind<'_>,
    origin: Origin,
    switch: server_store::RustProjects,
) -> Result<Outcome, DeleteError> {
    delete_with(data_dir, user, chat, kind, origin, switch, &|_| Ok(()))
}

/// [`delete`], `fault` asked after each [`Step`]: an error there stops the operation at once, as a
/// crash would (tests).
pub fn delete_with(
    data_dir: &Path,
    user: &str,
    chat: &str,
    kind: Kind<'_>,
    origin: Origin,
    switch: server_store::RustProjects,
    fault: &dyn Fn(Step) -> Result<(), StoreError>,
) -> Result<Outcome, DeleteError> {
    if chat.is_empty() {
        return Err(DeleteError::InvalidChat);
    }
    let dir = store::user_dir(data_dir, user)?;
    transcript::existing_dir(&dir)?;
    let _held = transcript::lock_user(&dir)?;
    let mut previous = None;
    let in_list = match kind {
        Kind::Free => remove_from_free(&dir, chat, &mut previous, switch, fault),
        Kind::Project(p) => remove_from_project(&dir, p, chat, &mut previous, switch, fault),
        Kind::Unlisted => {
            if listed_anywhere(&dir, chat)? {
                return Err(DeleteError::Listed);
            }
            add_tombstone(&dir, chat)?;
            fault(Step::Tombstoned)?;
            Ok(InList::Removed)
        }
    };
    let deleted = match in_list {
        Ok(InList::Removed) => true,
        Ok(InList::Absent) => false,
        Ok(InList::Unsupported) => return Err(DeleteError::Unsupported),
        Ok(InList::Crashed(e) | InList::Failed(e)) => return Err(e.into()),
        Err(e) => {
            // The list save failed: restore all prior ids and their order, including those
            // displaced by the cap. No other delete can intervene under this lock.
            take_back(&dir, previous.as_deref());
            return Err(e.into());
        }
    };
    fault(Step::ListSaved)?;
    if !deleted {
        // Already deleted (tombstoned) and in no list: a delete a crash cut short; finish it.
        if tombstoned(&dir, chat)? && !listed_anywhere(&dir, chat)? {
            let (_, leftovers) = remove_files(&dir, chat, fault)?;
            // A crash or failed append can leave no transcript and no journal entry.
            // Always nudge the idempotent reader on recovery, even when no files remain.
            note(&dir, chat, origin);
            return Ok(Outcome {
                deleted: false,
                swept: true,
                leftovers,
            });
        }
        return Ok(Outcome::default());
    }
    let (_, leftovers) = remove_files(&dir, chat, fault)?;
    note(&dir, chat, origin);
    Ok(Outcome {
        deleted: true,
        swept: false,
        leftovers,
    })
}

/// Finish already-confirmed deletions on a later workspace read (#2254). Capped tombstone-derived paths
/// select possible leftovers within the read budget; every deletion rechecks the tombstone and all lists while
/// holding the same transcript lock as writers. No retention choice or undeleted chat is affected.
pub fn reap_deleted(ctx: Ctx<'_>, limit: usize) -> usize {
    use std::collections::HashSet;
    let started = std::time::Instant::now();
    let Ok(dir) = store::user_dir(ctx.data_dir, ctx.user_id) else {
        return 0;
    };
    let Ok(ids) = tombstone_list(&dir) else {
        return 0;
    };
    if ids.is_empty() || limit == 0 {
        return 0;
    }
    let Ok(projects) = free_chats::workspace_projects(&dir) else {
        return 0;
    };
    let Ok(free) = store::read_json(&dir.join(FREE_CHATS_FILE)) else {
        return 0;
    };
    let free = free_chats::as_list(free);
    let mut listed = HashSet::new();
    let mut remember = |items: &[JValue]| {
        for chat in items {
            if let Some(id) = chat.get("id").as_str() {
                listed.extend(ids_of(id));
            }
        }
    };
    remember(&free);
    for project in &projects {
        if let JValue::Arr(items) = project.get("chats") {
            remember(items);
        }
    }
    // Probe only paths derived from capped tombstones, never enumerate account directories.
    // A symlink is a leftover too: remove_file unlinks its leaf without following it.
    let exists = |file: PathBuf| std::fs::symlink_metadata(file).is_ok();
    let mut attempted = 0;
    let mut removed = 0;
    for id in ids.iter().rev().take(TOMBSTONE_CAP) {
        if attempted >= limit || started.elapsed() >= std::time::Duration::from_secs(1) {
            break;
        }
        let forms = ids_of(id);
        if id.is_empty() || forms.iter().any(|id| listed.contains(id)) {
            continue;
        }
        let safe = safe_chat_id(id);
        let possible = (!safe.is_empty() && exists(history_file(&dir, &safe)))
            || forms.iter().any(|raw| {
                exists(context_window::state_file(&dir, raw))
                    || exists(brain_file(&dir, raw))
                    || exists(partial::file_for(&dir, raw))
            });
        if !possible {
            continue;
        }
        attempted += 1;
        let swept = (|| -> Result<bool, StoreError> {
            transcript::existing_dir(&dir)?;
            let _held = transcript::lock_user(&dir)?;
            if !tombstoned(&dir, id)? || listed_anywhere(&dir, id)? {
                return Ok(false);
            }
            let (_, leftovers) = remove_files(&dir, id, &|_| Ok(()))?;
            note(&dir, id, Origin::Remove);
            Ok(leftovers == 0)
        })();
        if matches!(swept, Ok(true)) {
            removed += 1;
        }
    }
    removed
}

fn note(dir: &Path, raw: &str, origin: Origin) {
    let safe = safe_chat_id(raw);
    let id = if safe.is_empty() { raw } else { safe.as_str() };
    if let Err(e) = journal::record(dir, id, None, journal::Op::Remove, origin, journal::LIMITS) {
        eprintln!("noevia-server: chat change journal append failed (remove): {e:?}");
    }
}

/// `{ ok: true }`, `{ error: 'no such chat' }` or the save failure, as the Node routes answered.
fn route_reply(result: Result<Outcome, DeleteError>) -> Reply {
    match result {
        Ok(Outcome { deleted: true, .. }) => Reply::json(200, "{\"ok\":true}".into()),
        Ok(_) => Reply::error(404, "no such chat"),
        Err(DeleteError::Store(e)) => chats::save_failure(&e, DELETE_FAILED),
        // An empty id never reaches here (the path segment is non-empty); a chat a list holds is
        // never asked as unlisted by a route.
        Err(DeleteError::InvalidChat | DeleteError::Listed | DeleteError::Unsupported) => {
            chats::save_failed(DELETE_FAILED)
        }
    }
}

/// DELETE /api/freechats/{id} (core routes/chat-lists.cjs).
pub fn delete_free_route(ctx: Ctx<'_>, raw_chat: &str) -> Reply {
    let Some(id) = js::decode_uri_component(raw_chat) else {
        return Reply::error(400, "invalid chat id");
    };
    let dir = match store::user_dir(ctx.data_dir, ctx.user_id) {
        Ok(d) => d,
        Err(e) => return chats::store_failure(&e),
    };
    // Node's workspace load, before any route.
    if let Err(reply) = free_chats::workspace_projects(&dir) {
        return reply;
    }
    route_reply(delete(
        ctx.data_dir,
        ctx.user_id,
        &id,
        Kind::Free,
        Origin::Remove,
        ctx.switch,
    ))
}

/// DELETE /api/projects/{p}/chats/{c} (core routes/projects.cjs).
pub fn delete_project_route(ctx: Ctx<'_>, raw_project: &str, raw_chat: &str) -> Reply {
    let (Some(project), Some(id)) = (
        js::decode_uri_component(raw_project),
        js::decode_uri_component(raw_chat),
    ) else {
        return Reply::error(400, "invalid chat id");
    };
    let dir = match store::user_dir(ctx.data_dir, ctx.user_id) {
        Ok(d) => d,
        Err(e) => return chats::store_failure(&e),
    };
    if let Err(reply) = free_chats::workspace_projects(&dir) {
        return reply;
    }
    route_reply(delete(
        ctx.data_dir,
        ctx.user_id,
        &id,
        Kind::Project(&project),
        Origin::Remove,
        ctx.switch,
    ))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::transcript::{Appended, Base, Saved};

    const USER: &str = "11111111-2222-4333-8444-555555555555";
    const OTHER: &str = "99999999-2222-4333-8444-555555555555";

    fn on() -> server_store::RustProjects {
        server_store::RustProjects::owned()
    }

    fn setup() -> (tempfile::TempDir, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let dir = store::user_dir(root.path(), USER).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        (root, dir)
    }

    fn list(text: &str) -> Vec<JValue> {
        match wtf16::parse(text).unwrap() {
            JValue::Arr(items) => items,
            _ => panic!("not a list"),
        }
    }

    /// The chat's four files, as Node and the Rust store write them.
    fn files(dir: &Path, id: &str) -> Vec<PathBuf> {
        vec![
            history_file(dir, &safe_chat_id(id)),
            context_window::state_file(dir, id),
            brain_file(dir, id),
            partial::file_for(dir, id),
        ]
    }

    fn seed_files(root: &Path, dir: &Path, id: &str) {
        transcript::write(
            root,
            USER,
            id,
            &list("[{\"role\":\"user\",\"content\":\"qa\"}]"),
            &Base::Any,
            None,
        )
        .unwrap();
        for f in files(dir, id).iter().skip(1) {
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(f, "{\"qa\":true}").unwrap();
        }
    }

    fn seed_free(root: &Path, dir: &Path) {
        std::fs::write(
            dir.join(FREE_CHATS_FILE),
            "[{\"id\":\"qa-c1\",\"title\":\"QA one\"},{\"id\":\"qa-c2\",\"title\":\"QA two\"}]",
        )
        .unwrap();
        seed_files(root, dir, "qa-c1");
        seed_files(root, dir, "qa-c2");
    }

    fn free_ids(dir: &Path) -> Vec<String> {
        free_chats::as_list(store::read_json(&dir.join(FREE_CHATS_FILE)).unwrap())
            .iter()
            .filter_map(|c| c.get("id").as_str().map(str::to_string))
            .collect()
    }

    fn tombstones(dir: &Path) -> Vec<String> {
        tombstone_list(dir).unwrap()
    }

    fn journal_ops(root: &Path) -> Vec<(String, String, String)> {
        journal::read(root, USER, 0)
            .unwrap()
            .entries
            .iter()
            .map(|e| {
                (
                    e["chatId"].as_str().unwrap().to_string(),
                    e["op"].as_str().unwrap().to_string(),
                    e["origin"].as_str().unwrap().to_string(),
                )
            })
            .collect()
    }

    fn del(root: &Path, id: &str, kind: Kind<'_>) -> Result<Outcome, DeleteError> {
        delete(root, USER, id, kind, Origin::Remove, on())
    }

    #[test]
    fn the_release_marker_is_the_literal_assemble_release_reads() {
        assert_eq!(CHAT_DELETE_STORE_MARKER, "noevia-chat-delete-store/1");
        assert!(include_str!("chat_delete.rs").contains("\"noevia-chat-delete-store/1\""));
    }

    #[test]
    fn a_free_chat_delete_runs_the_whole_cascade_and_journals_it() {
        let (root, dir) = setup();
        seed_free(root.path(), &dir);
        let got = del(root.path(), "qa-c1", Kind::Free).unwrap();
        assert_eq!(
            got,
            Outcome {
                deleted: true,
                swept: false,
                leftovers: 0
            }
        );
        assert_eq!(free_ids(&dir), ["qa-c2"]);
        assert_eq!(tombstones(&dir), ["qa-c1"]);
        for f in files(&dir, "qa-c1") {
            assert!(!f.exists(), "{}", f.display());
        }
        // sbstndalton/noevia#2147: the saved partial reply goes too. The other chat keeps all.
        for f in files(&dir, "qa-c2") {
            assert!(f.exists(), "{}", f.display());
        }
        let ops = journal_ops(root.path());
        assert_eq!(
            ops.last().unwrap(),
            &("qa-c1".into(), "remove".into(), "remove".into())
        );
        // The list is Node's atomicJson bytes.
        assert_eq!(
            std::fs::read_to_string(dir.join(FREE_CHATS_FILE)).unwrap(),
            "[\n  {\n    \"id\": \"qa-c2\",\n    \"title\": \"QA two\"\n  }\n]"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join(TOMBSTONES_FILE)).unwrap(),
            "[\"qa-c1\"]"
        );
    }

    #[test]
    fn a_project_chat_delete_saves_the_project_and_reconciles() {
        let (root, dir) = setup();
        std::fs::write(
            dir.join(store::PROJECTS_FILE),
            "{\"projects\":[{\"id\":\"qa-p\",\"name\":\"QA\",\"chats\":[{\"id\":\"qa-c1\"},{\"id\":\"qa-c3\"}]},{\"id\":\"qa-q\",\"chats\":[{\"id\":\"qa-c4\"}]}]}",
        )
        .unwrap();
        seed_files(root.path(), &dir, "qa-c1");
        // Another project's id and an unknown project are 404s: nothing is tombstoned.
        assert_eq!(
            del(root.path(), "qa-c1", Kind::Project("qa-q")).unwrap(),
            Outcome::default()
        );
        assert_eq!(
            del(root.path(), "qa-c1", Kind::Project("nope")).unwrap(),
            Outcome::default()
        );
        assert_eq!(
            del(root.path(), "qa-c1", Kind::Free).unwrap(),
            Outcome::default()
        );
        assert!(tombstones(&dir).is_empty());
        assert!(history_file(&dir, "qa-c1").exists());
        assert!(
            del(root.path(), "qa-c1", Kind::Project("qa-p"))
                .unwrap()
                .deleted
        );
        let projects = store::read(&dir).unwrap();
        let ids = |p: &JValue| -> Vec<String> {
            match p.get("chats") {
                JValue::Arr(c) => c
                    .iter()
                    .map(|c| c.get("id").as_str().unwrap().to_string())
                    .collect(),
                _ => vec![],
            }
        };
        assert_eq!(ids(&projects[0]), ["qa-c3"]);
        assert_eq!(ids(&projects[1]), ["qa-c4"]);
        // saveProjects reconciled every project (instructionSkills set).
        assert!(projects
            .iter()
            .all(|p| !matches!(p.get("instructionSkills"), JValue::Undefined)));
        for f in files(&dir, "qa-c1") {
            assert!(!f.exists());
        }
    }

    #[test]
    fn a_raw_id_tombstones_both_forms_and_removes_files_for_both() {
        let (root, dir) = setup();
        std::fs::write(dir.join(FREE_CHATS_FILE), "[{\"id\":\"qa c!9\"}]").unwrap();
        seed_files(root.path(), &dir, "qa c!9");
        for f in files(&dir, "qac9").iter().skip(1) {
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(f, "{}").unwrap();
        }
        assert!(del(root.path(), "qa c!9", Kind::Free).unwrap().deleted);
        assert_eq!(tombstones(&dir), ["qa c!9", "qac9"]);
        for f in files(&dir, "qa c!9").into_iter().chain(files(&dir, "qac9")) {
            assert!(!f.exists(), "{}", f.display());
        }
        assert_eq!(journal_ops(root.path()).last().unwrap().0, "qac9");
    }

    #[test]
    fn a_crash_after_any_step_leaves_the_chat_tombstoned_and_a_second_delete_finishes() {
        let steps = [
            Step::Tombstoned,
            Step::ListSaved,
            Step::TranscriptRemoved,
            Step::MeterRemoved,
            Step::BrainRemoved,
            Step::PartialRemoved,
        ];
        for step in steps {
            let (root, dir) = setup();
            seed_free(root.path(), &dir);
            let crash = |s: Step| {
                if s == step {
                    Err(StoreError::Io(std::io::Error::other("qa crash")))
                } else {
                    Ok(())
                }
            };
            let err = delete_with(
                root.path(),
                USER,
                "qa-c1",
                Kind::Free,
                Origin::Remove,
                on(),
                &crash,
            );
            assert!(
                matches!(err, Err(DeleteError::Store(StoreError::Io(_)))),
                "{step:?}"
            );
            // Hidden for good: tombstoned, and no writer brings the transcript back.
            assert_eq!(tombstones(&dir), ["qa-c1"], "{step:?}");
            assert_eq!(
                transcript::write(root.path(), USER, "qa-c1", &list("[1]"), &Base::Any, None)
                    .unwrap(),
                Saved::Deleted,
                "{step:?}"
            );
            // Asked again: the list delete when the list still holds it, else the sweep.
            let listed = free_ids(&dir);
            let again = del(root.path(), "qa-c1", Kind::Free).unwrap();
            if step == Step::Tombstoned {
                assert_eq!(listed, ["qa-c1", "qa-c2"], "the list was never saved");
                assert!(again.deleted, "{step:?}");
            } else {
                assert_eq!(
                    again,
                    Outcome {
                        deleted: false,
                        swept: true,
                        leftovers: 0
                    },
                    "{step:?}"
                );
            }
            assert_eq!(free_ids(&dir), ["qa-c2"], "{step:?}");
            for f in files(&dir, "qa-c1") {
                assert!(!f.exists(), "{step:?} {}", f.display());
            }
            for f in files(&dir, "qa-c2") {
                assert!(f.exists(), "{step:?} {}", f.display());
            }
            assert_eq!(tombstones(&dir), ["qa-c1"]);
            assert_eq!(
                journal_ops(root.path()).last().unwrap(),
                &("qa-c1".into(), "remove".into(), "remove".into()),
                "{step:?}"
            );
        }
    }

    #[test]
    fn a_failed_list_save_takes_back_only_the_tombstones_it_added() {
        let (root, dir) = setup();
        seed_free(root.path(), &dir);
        std::fs::write(dir.join(TOMBSTONES_FILE), "[\"qa-old\"]").unwrap();
        // The list's temp file cannot be created: the save fails after the tombstone.
        let blocker = dir.join(format!("{FREE_CHATS_FILE}.{}.tmp", std::process::id()));
        std::fs::create_dir_all(blocker.join("qa")).unwrap();
        let err = del(root.path(), "qa-c1", Kind::Free).unwrap_err();
        assert!(matches!(err, DeleteError::Store(_)), "{err:?}");
        assert_eq!(tombstones(&dir), ["qa-old"]);
        assert_eq!(free_ids(&dir), ["qa-c1", "qa-c2"]);
        for f in files(&dir, "qa-c1") {
            assert!(f.exists(), "nothing removed: {}", f.display());
        }
        assert!(journal_ops(root.path()).iter().all(|e| e.1 != "remove"));
        // A chat tombstoned before (a crash's leftover) keeps its tombstone when the retry fails.
        std::fs::write(dir.join(TOMBSTONES_FILE), "[\"qa-c1\"]").unwrap();
        del(root.path(), "qa-c1", Kind::Free).unwrap_err();
        assert_eq!(tombstones(&dir), ["qa-c1"]);
        // The project list too.
        std::fs::remove_dir_all(&blocker).unwrap();
        std::fs::write(dir.join(TOMBSTONES_FILE), "[]").unwrap();
        std::fs::write(
            dir.join(store::PROJECTS_FILE),
            "{\"projects\":[{\"id\":\"qa-p\",\"chats\":[{\"id\":\"qa-c2\"}]}]}",
        )
        .unwrap();
        let blocker = dir.join(format!(
            "{}.{}.tmp",
            store::PROJECTS_FILE,
            std::process::id()
        ));
        std::fs::create_dir_all(blocker.join("qa")).unwrap();
        del(root.path(), "qa-c2", Kind::Project("qa-p")).unwrap_err();
        assert!(tombstones(&dir).is_empty());
        assert!(history_file(&dir, "qa-c2").exists());
    }

    #[test]
    fn a_retry_after_journal_append_failure_notifies_even_without_a_transcript() {
        let (root, dir) = setup();
        seed_free(root.path(), &dir);
        let before = journal::read(root.path(), USER, 0).unwrap();
        // Failure is in the journal append itself, after all files have been removed.
        let journal_file = dir.join(journal::FILE);
        std::fs::remove_file(&journal_file).unwrap();
        std::fs::create_dir(&journal_file).unwrap();
        assert!(del(root.path(), "qa-c1", Kind::Free).unwrap().deleted);
        assert!(!history_file(&dir, "qa-c1").exists());
        std::fs::remove_dir(&journal_file).unwrap();
        // Restore the older journal so an already-running reader retains a valid cursor.
        // The next successful append must have a newer sequence, not just rely on a rescan.
        let text = before
            .entries
            .iter()
            .map(|e| format!("{}\n", serde_json::to_string(e).unwrap()))
            .collect::<String>();
        std::fs::write(&journal_file, text).unwrap();
        assert!(del(root.path(), "qa-c1", Kind::Free).unwrap().swept);
        let all = journal::read(root.path(), USER, 0).unwrap();
        let after = all.entries.last().unwrap();
        assert_eq!(after["chatId"], "qa-c1");
        assert_eq!(after["op"], "remove");
        assert!(
            after["seq"].as_u64().unwrap()
                > before.entries.last().unwrap()["seq"].as_u64().unwrap()
        );
    }

    #[test]
    fn failed_list_save_restores_the_complete_capped_tombstone_order_and_both_ids() {
        for preexisting in [false, true] {
            for project in [false, true] {
                let (root, dir) = setup();
                let raw = "qa raw!id";
                let mut previous: Vec<String> =
                    (0..TOMBSTONE_CAP).map(|i| format!("qa-old-{i}")).collect();
                if preexisting {
                    previous[123] = raw.into();
                    previous[321] = "qarawid".into();
                }
                write_tombstones(&dir, &previous).unwrap();
                let (file, kind) = if project {
                    std::fs::write(
                        dir.join(store::PROJECTS_FILE),
                        "{\"projects\":[{\"id\":\"qa-p\",\"chats\":[{\"id\":\"qa raw!id\"}]}]}",
                    )
                    .unwrap();
                    (store::PROJECTS_FILE, Kind::Project("qa-p"))
                } else {
                    std::fs::write(dir.join(FREE_CHATS_FILE), "[{\"id\":\"qa raw!id\"}]").unwrap();
                    (FREE_CHATS_FILE, Kind::Free)
                };
                let blocker = dir.join(format!("{file}.{}.tmp", std::process::id()));
                std::fs::create_dir_all(blocker.join("qa")).unwrap();
                assert!(del(root.path(), raw, kind).is_err());
                assert_eq!(
                    tombstones(&dir),
                    previous,
                    "project={project} prior-pair={preexisting}"
                );
                assert!(listed_anywhere(&dir, raw).unwrap());
                assert!(journal_ops(root.path()).is_empty());
            }
        }
    }

    #[test]
    fn symlinked_derived_data_parent_directories_do_not_unlink_another_account() {
        use std::os::unix::fs::symlink;
        for folder in [BRAIN_DIR, partial::DIR] {
            let (root, dir) = setup();
            seed_free(root.path(), &dir);
            let other = store::user_dir(root.path(), OTHER).unwrap();
            let outside = other.join(folder);
            std::fs::create_dir_all(&outside).unwrap();
            for id in ["qa-c1", "qa-c2"] {
                let name = format!("{}.json", sha_hex(id));
                std::fs::write(outside.join(name), "sibling sentinel").unwrap();
            }
            std::fs::remove_dir_all(dir.join(folder)).unwrap();
            symlink(&outside, dir.join(folder)).unwrap();
            let result = del(root.path(), "qa-c1", Kind::Free).unwrap();
            assert!(result.deleted);
            assert_eq!(result.leftovers, 1);
            for id in ["qa-c1", "qa-c2"] {
                assert_eq!(
                    std::fs::read_to_string(outside.join(format!("{}.json", sha_hex(id)))).unwrap(),
                    "sibling sentinel"
                );
            }
            assert!(std::fs::symlink_metadata(dir.join(folder))
                .unwrap()
                .file_type()
                .is_symlink());
        }
    }

    #[test]
    fn derived_data_leaf_symlinks_are_unlinked_without_following_their_targets() {
        use std::os::unix::fs::symlink;
        let (root, dir) = setup();
        seed_free(root.path(), &dir);
        let sentinel = root.path().join("outside-sentinel");
        std::fs::write(&sentinel, "keep").unwrap();
        for path in [brain_file(&dir, "qa-c1"), partial::file_for(&dir, "qa-c1")] {
            std::fs::remove_file(&path).unwrap();
            symlink(&sentinel, &path).unwrap();
        }
        assert_eq!(del(root.path(), "qa-c1", Kind::Free).unwrap().leftovers, 0);
        assert_eq!(std::fs::read_to_string(sentinel).unwrap(), "keep");
        assert!(std::fs::symlink_metadata(brain_file(&dir, "qa-c1")).is_err());
        assert!(std::fs::symlink_metadata(partial::file_for(&dir, "qa-c1")).is_err());
    }

    #[test]
    fn a_late_write_racing_the_delete_never_resurrects_the_transcript() {
        for round in 0..20 {
            let (root, dir) = setup();
            seed_free(root.path(), &dir);
            std::thread::scope(|s| {
                let writers: Vec<_> = (0..4)
                    .map(|i| {
                        let root = root.path();
                        s.spawn(move || {
                            for j in 0..25 {
                                let h = vec![JValue::Num(f64::from(i * 100 + j))];
                                let _ =
                                    transcript::write(root, USER, "qa-c1", &h, &Base::Any, None);
                                let _ = transcript::append_if_last(
                                    root,
                                    USER,
                                    "qa-c1",
                                    "qa",
                                    &JValue::Null,
                                );
                            }
                        })
                    })
                    .collect();
                assert!(
                    del(root.path(), "qa-c1", Kind::Free).unwrap().deleted,
                    "{round}"
                );
                for w in writers {
                    w.join().unwrap();
                }
            });
            assert!(
                !history_file(&dir, "qa-c1").exists(),
                "round {round}: resurrected"
            );
            assert_eq!(
                transcript::append_if_last(root.path(), USER, "qa-c1", "qa", &JValue::Null)
                    .unwrap(),
                Appended::Deleted
            );
        }
    }

    #[test]
    fn a_repeated_delete_is_a_404_and_repeats_the_idempotent_removal_notice() {
        let (root, dir) = setup();
        seed_free(root.path(), &dir);
        assert!(del(root.path(), "qa-c1", Kind::Free).unwrap().deleted);
        let journalled = journal_ops(root.path()).len();
        let again = del(root.path(), "qa-c1", Kind::Free).unwrap();
        assert_eq!(
            again,
            Outcome {
                deleted: false,
                swept: true,
                leftovers: 0
            }
        );
        // Recovery notifications are at least once, even when no files remain.
        assert_eq!(journal_ops(root.path()).len(), journalled + 1);
        assert_eq!(journal_ops(root.path()).last().unwrap().1, "remove");
        assert_eq!(tombstones(&dir), ["qa-c1"]);
        // A chat neither listed nor tombstoned is a plain 404: no tombstone, no sweep.
        seed_files(root.path(), &dir, "qa-never");
        assert_eq!(
            del(root.path(), "qa-never", Kind::Free).unwrap(),
            Outcome::default()
        );
        assert!(history_file(&dir, "qa-never").exists());
        assert_eq!(tombstones(&dir), ["qa-c1"]);
    }

    #[test]
    fn a_tombstoned_chat_still_listed_somewhere_is_not_swept() {
        let (root, dir) = setup();
        std::fs::write(
            dir.join(store::PROJECTS_FILE),
            "{\"projects\":[{\"id\":\"qa-p\",\"chats\":[{\"id\":\"qa-c1\"}]}]}",
        )
        .unwrap();
        std::fs::write(dir.join(TOMBSTONES_FILE), "[\"qa-c1\"]").unwrap();
        seed_files(root.path(), &dir, "qa-c1");
        // History was written before the tombstone; free-list delete does not hold it.
        assert_eq!(
            del(root.path(), "qa-c1", Kind::Free).unwrap(),
            Outcome::default()
        );
        assert!(brain_file(&dir, "qa-c1").exists());
    }

    #[test]
    fn unlisted_is_refused_while_a_list_holds_the_chat() {
        let (root, dir) = setup();
        seed_free(root.path(), &dir);
        assert!(matches!(
            del(root.path(), "qa-c1", Kind::Unlisted),
            Err(DeleteError::Listed)
        ));
        assert!(tombstones(&dir).is_empty());
        assert!(history_file(&dir, "qa-c1").exists());
        // A deleted project's chat (in no list) is tombstoned and its files removed.
        seed_files(root.path(), &dir, "qa-gone");
        assert!(del(root.path(), "qa-gone", Kind::Unlisted).unwrap().deleted);
        assert_eq!(tombstones(&dir), ["qa-gone"]);
        for f in files(&dir, "qa-gone") {
            assert!(!f.exists());
        }
    }

    #[test]
    fn one_user_never_deletes_another_users_chat() {
        let (root, dir) = setup();
        seed_free(root.path(), &dir);
        let other = store::user_dir(root.path(), OTHER).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join(FREE_CHATS_FILE), "[{\"id\":\"qa-c1\"}]").unwrap();
        std::fs::write(history_file(&other, "qa-c1"), "{\"history\":[]}").unwrap();
        assert!(
            delete(
                root.path(),
                OTHER,
                "qa-c1",
                Kind::Free,
                Origin::Remove,
                on()
            )
            .unwrap()
            .deleted
        );
        assert!(!history_file(&other, "qa-c1").exists());
        assert_eq!(free_ids(&dir), ["qa-c1", "qa-c2"]);
        for f in files(&dir, "qa-c1") {
            assert!(f.exists());
        }
        assert!(tombstones(&dir).is_empty());
        // An id cannot climb out of the user's directory.
        assert_eq!(
            del(
                root.path(),
                "../99999999-2222-4333-8444-555555555555/qa-c2",
                Kind::Free
            )
            .unwrap(),
            Outcome::default()
        );
        for bad in ["", "../x", "not-a-uuid"] {
            assert!(matches!(
                delete(root.path(), bad, "qa-c1", Kind::Free, Origin::Remove, on()),
                Err(DeleteError::Store(StoreError::InvalidUser))
            ));
        }
        assert!(matches!(
            del(root.path(), "", Kind::Free),
            Err(DeleteError::InvalidChat)
        ));
    }

    #[test]
    fn a_removed_account_is_gone_and_not_recreated() {
        let root = tempfile::tempdir().unwrap();
        assert!(matches!(
            del(root.path(), "qa-c1", Kind::Free),
            Err(DeleteError::Store(StoreError::Gone))
        ));
        assert!(!store::user_dir(root.path(), USER).unwrap().exists());
    }

    #[test]
    fn the_routes_answer_as_node_did() {
        let (root, dir) = setup();
        seed_free(root.path(), &dir);
        std::fs::write(
            dir.join(store::PROJECTS_FILE),
            "{\"projects\":[{\"id\":\"qa p\",\"chats\":[{\"id\":\"qa-c5\"}]}]}",
        )
        .unwrap();
        let ctx = Ctx {
            data_dir: root.path(),
            user_id: USER,
            switch: on(),
            now_ms: 0,
        };
        assert_eq!(
            delete_free_route(ctx, "qa-c1"),
            Reply::json(200, "{\"ok\":true}".into())
        );
        assert_eq!(
            delete_free_route(ctx, "qa-c1"),
            Reply::error(404, "no such chat")
        );
        assert_eq!(
            delete_free_route(ctx, "%E0%A4%A"),
            Reply::error(400, "invalid chat id")
        );
        assert_eq!(
            delete_project_route(ctx, "qa%20p", "qa-c5"),
            Reply::json(200, "{\"ok\":true}".into())
        );
        assert_eq!(
            delete_project_route(ctx, "qa%20p", "qa-c5"),
            Reply::error(404, "no such chat")
        );
        assert_eq!(
            delete_project_route(ctx, "%zz", "qa-c5"),
            Reply::error(400, "invalid chat id")
        );
        // A projects.json Node's workspace load throws on: its 500.
        std::fs::write(dir.join(store::PROJECTS_FILE), "{\"projects\":[null]}").unwrap();
        assert_eq!(delete_free_route(ctx, "qa-c2"), Reply::internal());
        // A busy store: 503 PROJECTS_BUSY with Retry-After.
        std::fs::write(dir.join(store::PROJECTS_FILE), "{\"projects\":[]}").unwrap();
        let r = chats::save_failure(&StoreError::Busy, DELETE_FAILED);
        assert_eq!(r.status, 503);
        let gone = Ctx {
            user_id: OTHER,
            ..ctx
        };
        assert_eq!(
            delete_free_route(gone, "qa-c2"),
            Reply::error(410, "account no longer exists")
        );
    }
    fn cleanup_ctx(root: &Path) -> Ctx<'_> {
        Ctx {
            data_dir: root,
            user_id: USER,
            switch: on(),
            now_ms: 1,
        }
    }

    #[test]
    fn workspace_finishes_a_confirmed_delete_without_touching_another_chat_or_account() {
        let (root, dir) = setup();
        seed_free(root.path(), &dir);
        std::fs::write(dir.join("projects.json"), r#"{"projects":[]}"#).unwrap();
        std::fs::write(
            dir.join(FREE_CHATS_FILE),
            r#"[{"id":"qa-c2","title":"keep"}]"#,
        )
        .unwrap();
        write_tombstones(&dir, &["qa-c1".to_owned()]).unwrap();
        let other = store::user_dir(root.path(), OTHER).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        let other_file = history_file(&other, "qa-c1");
        std::fs::write(&other_file, "other account").unwrap();
        assert_eq!(crate::workspace::get(cleanup_ctx(root.path())).status, 200);
        assert!(files(&dir, "qa-c1").iter().all(|p| !p.exists()));
        assert!(files(&dir, "qa-c2").iter().all(|p| p.exists()));
        assert_eq!(
            std::fs::read_to_string(other_file).unwrap(),
            "other account"
        );
        let before = journal_ops(root.path());
        assert!(before
            .iter()
            .any(|(id, op, _)| id == "qa-c1" && op == "remove"));
        assert_eq!(crate::workspace::get(cleanup_ctx(root.path())).status, 200);
        assert_eq!(
            journal_ops(root.path()),
            before,
            "no repeated recovery entry without leftovers"
        );
    }

    #[test]
    fn cleanup_is_bounded_and_retries_the_remaining_deleted_chats() {
        let (root, dir) = setup();
        seed_free(root.path(), &dir);
        std::fs::write(dir.join(FREE_CHATS_FILE), "[]").unwrap();
        write_tombstones(&dir, &["qa-c1".to_owned(), "qa-c2".to_owned()]).unwrap();
        assert_eq!(reap_deleted(cleanup_ctx(root.path()), 1), 1);
        assert_eq!(
            files(&dir, "qa-c1").iter().filter(|p| p.exists()).count(),
            4
        );
        assert!(files(&dir, "qa-c2").iter().all(|p| !p.exists()));
        assert_eq!(reap_deleted(cleanup_ctx(root.path()), 1), 1);
        assert!(files(&dir, "qa-c1").iter().all(|p| !p.exists()));
        assert_eq!(reap_deleted(cleanup_ctx(root.path()), 1), 0);
    }

    #[test]
    fn a_tombstone_never_authorizes_removing_files_while_a_list_still_holds_the_chat() {
        let (root, dir) = setup();
        seed_free(root.path(), &dir);
        std::fs::write(dir.join(FREE_CHATS_FILE), r#"[{"id":"qa-c2"}]"#).unwrap();
        std::fs::write(
            dir.join("projects.json"),
            r#"{"projects":[{"id":"p","chats":[{"id":"qa-c1"}]}]}"#,
        )
        .unwrap();
        write_tombstones(&dir, &["qa-c1".to_owned(), "qa-c2".to_owned()]).unwrap();
        assert_eq!(reap_deleted(cleanup_ctx(root.path()), 10), 0);
        assert!(files(&dir, "qa-c1")
            .iter()
            .chain(files(&dir, "qa-c2").iter())
            .all(|p| p.exists()));
    }

    #[cfg(unix)]
    #[test]
    fn cleanup_unlinks_a_deleted_leaf_symlink_without_following_it() {
        let (root, dir) = setup();
        std::fs::write(dir.join(FREE_CHATS_FILE), "[]").unwrap();
        write_tombstones(&dir, &["qa-c1".to_owned()]).unwrap();
        let outside = root.path().join("outside-history.json");
        std::fs::write(&outside, "preserve").unwrap();
        let link = history_file(&dir, "qa-c1");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        assert_eq!(reap_deleted(cleanup_ctx(root.path()), 1), 1);
        assert!(!link.exists());
        assert_eq!(std::fs::read_to_string(outside).unwrap(), "preserve");
    }
}
