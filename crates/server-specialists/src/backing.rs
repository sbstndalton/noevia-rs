//! A coworker's backing project and conversation (sbstndalton/noevia#2446, slice 4a).
//!
//! Each coworker owns one hidden project, `specialist-<specialist id>`, and one chat in it,
//! `specialist-chat-<specialist id>`, its continuous conversation. Both ids follow from the
//! coworker's id, so every step below is idempotent and a repair never has to guess. The project
//! is one of the account's `projects.json` entries (the chat loop needs it there), kept out of
//! every list the way the Diary's and a free chat's hidden projects are
//! ([`server_projects::store::internal_project_id`]). Its `instructions` are the coworker's brief
//! and its `toolboxes` (with `toolsMode: "manual"`) are the coworker's toolboxes; nothing else of
//! the project is the coworker's to set.
//!
//! # Lock order
//!
//! The account's specialists lock (in this process, [`Store`]) is always taken first; inside it
//! `projects.json`'s lock ([`server_projects::store::update`]) and a chat delete's locks are
//! taken and released in turn, never the other way round. The chat delete
//! (`server_projects::chat_delete::delete_with`) nests its own two in one fixed order, the
//! transcript lock and then `projects.json`'s (sbstndalton/noevia#2473); nothing here takes
//! `projects.json`'s lock around it. Nothing that holds `projects.json`'s lock calls back into
//! this crate.
//!
//! # Write order and what a crash leaves
//!
//! The record in `specialists.json` is the truth; the project is a mirror of it.
//!
//! - **Create:** record (ids set), then project with its chat. A crash between leaves a record
//!   whose project is missing; the next read makes it. A failed project write takes the record
//!   back.
//! - **Patch:** record, then the same brief and toolboxes into the project in one write of
//!   `projects.json`. A crash between leaves a stale project; the next read mirrors again.
//! - **Delete:** the record goes first (that is the delete), then the chat by the existing delete
//!   path (its tombstone first), then the project. A crash after the record leaves an orphan
//!   `specialist-*` project (and maybe its chat); the next read of the coworkers finds a project
//!   with no record and removes it the same way. A project is never removed while its record is.
//! - **Older coworkers** (null ids) get both ids written and the project made by the same repair.
//!
//! The repair ([`Store::repair_if_needed`]) runs at the start of every list and get of
//! coworkers: a cheap unlocked look (projects first, then the record, so a project made after the
//! look can only be a new coworker's), and only if something is off it takes the lock, looks
//! again and fixes it.

use crate::lists::SPECIALISTS;
use crate::{now_ms, Error, Specialist, SpecialistList, Store, MAX_TOOLBOXES};
use js_json::JValue;
use server_projects::chat_delete::{self, Kind};
use server_projects::journal::Origin;
use server_projects::store::{self, StoreError, SPECIALIST_PREFIX};
use server_projects::{lifecycle, Ctx};
use server_store::RustProjects;
use std::collections::HashSet;
use std::path::PathBuf;

/// The chat's id prefix; the rest is the coworker's id.
pub const CHAT_PREFIX: &str = "specialist-chat-";

/// The account the hidden projects belong to: where its project store is.
#[derive(Debug, Clone)]
pub struct Backing {
    /// UI_DATA_DIR.
    pub data_dir: PathBuf,
    pub user_id: String,
}

/// The backing project's id for a coworker.
pub fn project_id_of(specialist: &str) -> String {
    format!("{SPECIALIST_PREFIX}{specialist}")
}

/// The conversation's chat id for a coworker.
pub fn chat_id_of(specialist: &str) -> String {
    format!("{CHAT_PREFIX}{specialist}")
}

fn conv(e: StoreError) -> Error {
    match e {
        StoreError::Gone => Error::NotFound,
        StoreError::Io(e) => Error::Io(e),
        other => Error::Io(std::io::Error::other(format!("project store: {other:?}"))),
    }
}

fn strings(v: &[String]) -> JValue {
    JValue::Arr(v.iter().cloned().map(JValue::Str).collect())
}

fn set_if(project: &mut JValue, key: &str, value: JValue) -> bool {
    if project.get(key) == &value {
        return false;
    }
    store::set(project, key, value);
    true
}

fn chat_meta(s: &Specialist, chat: &str, now: i64) -> JValue {
    JValue::obj([
        ("id", JValue::from(chat)),
        ("title", JValue::from(s.name.as_str())),
        ("createdAt", JValue::Num(now as f64)),
        ("updatedAt", JValue::Num(now as f64)),
    ])
}

/// Makes `projects` hold the coworker's project as its record says: made if missing, otherwise
/// its name, instructions (the brief) and toolboxes set, and its chat listed. Whether anything
/// changed. A record without ids has nothing to mirror.
pub(crate) fn apply(projects: &mut Vec<JValue>, s: &Specialist, now: i64) -> bool {
    let (Some(pid), Some(cid)) = (s.project_id.as_deref(), s.chat_id.as_deref()) else {
        return false;
    };
    let Some(project) = store::find_mut(projects, pid) else {
        projects.push(JValue::obj([
            ("id", JValue::from(pid)),
            ("name", JValue::from(s.name.as_str())),
            ("goal", JValue::from("")),
            ("instructions", JValue::from(s.brief.as_str())),
            ("files", JValue::Arr(Vec::new())),
            ("assets", JValue::Arr(Vec::new())),
            ("memories", JValue::Arr(Vec::new())),
            ("chats", JValue::Arr(vec![chat_meta(s, cid, now)])),
            ("sourceFolders", JValue::Arr(Vec::new())),
            ("toolboxes", strings(&s.toolboxes)),
            ("modes", JValue::Arr(vec![JValue::from("chat")])),
            ("routing", JValue::from("auto")),
            ("routingChosen", JValue::Bool(true)),
            ("toolsMode", JValue::from("manual")),
            ("createdAt", JValue::Num(now as f64)),
            ("updatedAt", JValue::Num(now as f64)),
        ]));
        return true;
    };
    let mut changed = false;
    changed |= set_if(project, "name", JValue::from(s.name.as_str()));
    changed |= set_if(project, "instructions", JValue::from(s.brief.as_str()));
    changed |= set_if(project, "toolboxes", strings(&s.toolboxes));
    changed |= set_if(project, "toolsMode", JValue::from("manual"));
    let listed = match project.get("chats") {
        JValue::Arr(chats) => chats.iter().any(|c| c.get("id").as_str() == Some(cid)),
        _ => false,
    };
    if !listed {
        match project.get("chats") {
            JValue::Arr(chats) => {
                let mut next = chats.clone();
                next.push(chat_meta(s, cid, now));
                store::set(project, "chats", JValue::Arr(next));
            }
            _ => store::set(project, "chats", JValue::Arr(vec![chat_meta(s, cid, now)])),
        }
        changed = true;
    }
    if changed {
        store::set(project, "updatedAt", JValue::Num(now as f64));
    }
    changed
}

/// The `specialist-*` projects no coworker of `live` owns: the project id and its chat ids.
/// `recorded` is whether `specialists.json` exists: without it nothing is an orphan, because a
/// record file that is missing (or was lost) must never be taken for "no coworkers" and wipe
/// every conversation. (Restoring an older `specialists.json` over newer projects is a backup
/// problem this cannot tell apart: the coworkers it lacks are then removed.)
fn orphans(
    projects: &[JValue],
    live: &HashSet<&str>,
    recorded: bool,
) -> Vec<(String, Vec<String>)> {
    if !recorded {
        return Vec::new();
    }
    projects
        .iter()
        .filter_map(|p| {
            let id = p.get("id").as_str()?;
            let owner = id.strip_prefix(SPECIALIST_PREFIX)?;
            if live.contains(owner) {
                return None;
            }
            let chats = match p.get("chats") {
                JValue::Arr(c) => c
                    .iter()
                    .filter_map(|c| c.get("id").as_str().map(str::to_owned))
                    .collect(),
                _ => Vec::new(),
            };
            Some((id.to_owned(), chats))
        })
        .collect()
}

impl Store {
    /// `(projectId, chatId)` for a new coworker: both `None` without a backing.
    pub(crate) fn backing_ids(&self, specialist: &str) -> (Option<String>, Option<String>) {
        if self.backing().is_some() {
            (
                Some(project_id_of(specialist)),
                Some(chat_id_of(specialist)),
            )
        } else {
            (None, None)
        }
    }

    /// `ids` as a coworker's toolboxes: each must be one of the account's known toolboxes,
    /// repeats dropped.
    pub(crate) fn toolbox_ids(&self, ids: &[String]) -> Result<Vec<String>, Error> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        if ids.len() > MAX_TOOLBOXES {
            return Err(Error::Invalid(format!("at most {MAX_TOOLBOXES} toolboxes")));
        }
        let Some(known) = self.known_toolboxes() else {
            return Err(Error::Invalid(
                "the toolboxes cannot be checked right now; try again shortly".into(),
            ));
        };
        let mut out: Vec<String> = Vec::new();
        for id in ids {
            if !known.contains(id) {
                return Err(Error::Invalid(
                    "toolboxes must be ids of toolboxes you can use".into(),
                ));
            }
            if !out.contains(id) {
                out.push(id.clone());
            }
        }
        Ok(out)
    }

    /// The record's project made to match it. Call inside the specialists lock.
    pub(crate) fn mirror(&self, s: &Specialist) -> Result<(), Error> {
        if self.backing().is_none() {
            return Ok(());
        }
        let now = now_ms();
        store::update(self.dir(), RustProjects::owned(), |projects| {
            ((), apply(projects, s, now))
        })
        .map_err(conv)
    }

    /// Removes a deleted coworker's chat (tombstone first) and project. Best effort: the record
    /// is already gone, and what stays is an orphan the next read removes.
    pub(crate) fn unback(&self, s: &Specialist) {
        let (Some(pid), Some(cid)) = (s.project_id.as_deref(), s.chat_id.as_deref()) else {
            return;
        };
        self.remove_backing(pid, &[cid.to_owned()]);
    }

    fn remove_backing(&self, project: &str, chats: &[String]) {
        let Some(b) = self.backing() else { return };
        for chat in chats {
            let gone = |kind| {
                chat_delete::delete(
                    &b.data_dir,
                    &b.user_id,
                    chat,
                    kind,
                    Origin::Remove,
                    RustProjects::owned(),
                )
            };
            // Out of the project's list, tombstoned, files removed. A chat the list no longer
            // holds (a half-made project) is tombstoned and swept as an unlisted one, so its
            // history never outlives the coworker.
            let done = match gone(Kind::Project(project)) {
                Ok(o) if o.deleted || o.swept => Ok(()),
                Ok(_) => gone(Kind::Unlisted).map(|_| ()),
                Err(e) => Err(e),
            };
            if let Err(e) = done {
                eprintln!("noevia-server: a coworker's chat was not removed: {e:?}");
                // The project stays too: removing it first would strand the chat's files.
                return;
            }
        }
        let ctx = Ctx {
            data_dir: &b.data_dir,
            user_id: &b.user_id,
            switch: RustProjects::owned(),
            now_ms: now_ms(),
        };
        if let Err(r) = lifecycle::remove(ctx, self.dir(), project, false) {
            eprintln!(
                "noevia-server: a coworker's project was not removed ({})",
                r.status
            );
        }
    }

    /// Whether the record file exists.
    fn recorded(&self) -> bool {
        self.dir().join(SPECIALISTS).exists()
    }

    /// Whether [`Store::repair_locked`] has anything to do, from an unlocked look: projects
    /// first, then the record.
    fn repair_needed(&self) -> Result<bool, Error> {
        let projects = store::read(self.dir()).map_err(conv)?;
        let list: SpecialistList = self.read(self.dir(), SPECIALISTS)?;
        let live: HashSet<&str> = list.specialists.iter().map(|s| s.id.as_str()).collect();
        if !orphans(&projects, &live, self.recorded()).is_empty() {
            return Ok(true);
        }
        let mut copy = projects.clone();
        Ok(list
            .specialists
            .iter()
            .any(|s| s.project_id.is_none() || s.chat_id.is_none() || apply(&mut copy, s, 0)))
    }

    /// Repairs what a crash, or a version before this one, left: ids for older coworkers, a
    /// missing or stale project, an orphan project.
    pub(crate) fn repair_if_needed(&self) -> Result<(), Error> {
        if self.backing().is_none() || !self.repair_needed()? {
            return Ok(());
        }
        self.locked(|| {
            let mut list: SpecialistList = self.read(self.dir(), SPECIALISTS)?;
            let mut dirty = false;
            for s in &mut list.specialists {
                if s.project_id.is_none() || s.chat_id.is_none() {
                    (s.project_id, s.chat_id) = self.backing_ids(&s.id);
                    dirty = true;
                }
            }
            if dirty {
                list.revision += 1;
                self.write(self.dir(), SPECIALISTS, &list)?;
            }
            let live: HashSet<&str> = list.specialists.iter().map(|s| s.id.as_str()).collect();
            let recorded = self.recorded();
            let now = now_ms();
            let strays = store::update(self.dir(), RustProjects::owned(), |projects| {
                let mut changed = false;
                for s in &list.specialists {
                    changed |= apply(projects, s, now);
                }
                (orphans(projects, &live, recorded), changed)
            })
            .map_err(conv)?;
            for (project, chats) in strays {
                self.remove_backing(&project, &chats);
            }
            Ok(())
        })
    }

    /// The coworker whose conversation is `chat_id` in project `project_id`: both must be that
    /// one coworker's own, which is the whole grant a conversation's tools have. `None` for any
    /// other chat or project, and for a chat of another account (its ids are not in this one's
    /// record). Slice 4b's tool handler starts here.
    pub fn resolve_conversation(
        &self,
        project_id: &str,
        chat_id: &str,
    ) -> Result<Option<Specialist>, Error> {
        if !project_id.starts_with(SPECIALIST_PREFIX) || !chat_id.starts_with(CHAT_PREFIX) {
            return Ok(None);
        }
        let list: SpecialistList = self.locked(|| self.read(self.dir(), SPECIALISTS))?;
        Ok(list.specialists.into_iter().find(|s| {
            s.project_id.as_deref() == Some(project_id) && s.chat_id.as_deref() == Some(chat_id)
        }))
    }
}
