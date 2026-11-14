//! The account's workspace as core hands it to a chat turn: workspace.cjs `get(user)` (the load the
//! Node server runs at the start of every signed-in request) and projects.cjs `getProject(id)`,
//! read only, for a native preparer that answers what core's turn reads (track R14 step 1).
//!
//! What a turn sees, and where it comes from:
//! - `projects`: projects.json as the load leaves it (rust-projects.cjs `readProjectsFile`: missing,
//!   not JSON, `null`, a `projects` that is `null` or not a list = no projects), each project with
//!   the load's fix-ups ([`crate::workspace`]: a `files` entry that is not an object dropped (#1236),
//!   the retired `lemonade` provider read as the default's, `modes` naming Chat first (#1576)).
//! - `free_chats`: free-chats.json when it holds a list, else none (workspace.cjs `freeChats`).
//! - `preferences`: `readJson(preferences.json, {}) || {}`.
//! - [`View::auto_roles`]: `readJson(auto-roles.json, null)`, read again at every call, as the
//!   workspace's getter does.
//! - [`View::text`]: a source's text as core's turn reads it, `String(file.content || '')` (the
//!   inline value, coerced; else the text file its `textRef` names through source-text.cjs's
//!   accessor, `''` when that cannot be read).
//! - [`View::project`]: `getProject(id)`: the first project with that id, after its own fix-ups (a
//!   chat-attachments project still on Diary's `routing: 'manual'` template healed to `auto` (#352),
//!   the instruction skills reconciled), and the project to save when getProject would have saved.
//!   The view never writes: the caller saves [`Project::save`] through the store
//!   ([`store::update`], as `context.rs` does), or answers from memory as Node's turn does.
//!
//! Left to their owners: the provider rows (the front's `provider_routes::view`, which also opens the
//! keys), the account's instructions and memory files (read by the preparer itself).
//!
//! Differences from Node, by design (each covered by the recording, `tests/workspace_view.rs`):
//! - Nothing is written: Node's load writes the #1576 `modes` migration (after a backup), moves
//!   inline source text to its file (#1806, which the front's startup migration has already done)
//!   and seals plaintext provider keys; it also makes a missing account directory. The view applies
//!   the same fix-ups in memory on every read. Node's save after a getProject heal also writes the
//!   other projects' load fix-ups; a caller of [`Project::save`] writes only that project. After a
//!   load that saved, Node's list has also lost a second project with an id already listed (its
//!   store merge keys projects by id); the view keeps the file's list (getProject answers the first
//!   either way, and the front's writers never store two projects with one id).
//! - A projects.json holding a project that is not an object or a list is refused
//!   ([`Refused::Damaged`]): Node's load throws for a `null` one (500) and keeps a number, string or
//!   boolean one; the front's project routes refuse all of them (`free_chats::workspace_projects`).
//! - A file holding a value this port cannot represent is refused ([`Refused::Unreadable`]), never
//!   read as less than Node would see; so is getProject over a project shape Node never writes
//!   ([`Refused::Unsupported`]: its `files` a string, a file that is not an object). projects.json
//!   or free-chats.json that exists but cannot be opened (EACCES, ELOOP, ...) is
//!   [`Refused::Unreadable`] too, where Node's load throws.
//! - A source's text file ([`crate::sources::text`]) is opened without following a symlink at any
//!   step (the account directory included) and only up to 64 MiB, and a project id with no
//!   character of `[A-Za-z0-9_-]` has no text folder: each of these reads as `''`, where Node's
//!   `readFileSync` follows links, has no cap, and reads `project-sources/<ref>.txt`.
//! - Strings are `js_json::wtf16` strings: a lone surrogate (an inline text, a name) is held as its
//!   mapped character (U+10F800..), and a text file is decoded as UTF-8 (invalid bytes as U+FFFD,
//!   as `readFileSync(file, 'utf8')`); a text file holding a real U+10F800.. character, which only
//!   that private-use range can produce, is therefore indistinguishable from a mapped one.
use crate::{chats, free_chats, sources, store, workspace};
use js_json::JValue;
use std::path::{Path, PathBuf};

pub const PREFERENCES_FILE: &str = "preferences.json";
pub const AUTO_ROLES_FILE: &str = "auto-roles.json";

/// Why there is no view: the turn stays with core.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    /// Not a user id workspace.cjs `userDir` accepts.
    InvalidUser,
    /// projects.json holds a project that is not an object or a list.
    Damaged,
    /// A file holds a value this port cannot represent.
    Unreadable,
    /// getProject over a project of a shape Node never writes (`files` a string, a file that is
    /// not an object or has no string name: [`crate::skills`]); Node iterates or throws there.
    Unsupported,
}

/// The workspace of one account, as of one read.
#[derive(Debug, Clone)]
pub struct View {
    dir: PathBuf,
    pub projects: Vec<JValue>,
    pub free_chats: Vec<JValue>,
    pub preferences: JValue,
}

/// What `getProject(id)` hands the turn.
#[derive(Debug, Clone, PartialEq)]
pub struct Project {
    /// The project as Node holds it afterwards.
    pub project: JValue,
    /// The project to write in place of the stored one when getProject saved (the #352 heal or a
    /// changed reconcile), else `None`.
    pub save: Option<JValue>,
}

fn read(file: &Path) -> Result<Option<JValue>, Refused> {
    store::read_json(file).map_err(|_| Refused::Unreadable)
}

/// rust-projects.cjs `readProjectsFile`/`readFreeChatsFile`: a missing file is `None`; one that
/// cannot be opened throws there, and is [`Refused::Unreadable`] here; text that is not JSON is
/// `None` (Node's fallback).
fn read_strict(file: &Path) -> Result<Option<JValue>, Refused> {
    let bytes = match std::fs::read(file) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(Refused::Unreadable),
    };
    match js_json::wtf16::parse(&String::from_utf8_lossy(&bytes)) {
        Ok(v) => Ok(Some(v)),
        Err(js_json::wtf16::Refusal::Invalid) => Ok(None),
        Err(js_json::wtf16::Refusal::Unsupported) => Err(Refused::Unreadable),
    }
}

/// workspace.cjs `get(user)`, read only. A missing account directory is an empty workspace.
pub fn load(data_dir: &Path, user: &str, default_provider: &str) -> Result<View, Refused> {
    let dir = store::user_dir(data_dir, user).map_err(|_| Refused::InvalidUser)?;
    // free_chats::workspace_projects, with its two refusals told apart.
    let projects = match read_strict(&dir.join(store::PROJECTS_FILE))? {
        Some(doc) => match doc.get("projects") {
            JValue::Arr(items) => items.clone(),
            _ => Vec::new(),
        },
        None => Vec::new(),
    };
    if !projects
        .iter()
        .all(|p| matches!(p, JValue::Obj(_) | JValue::Arr(_)))
    {
        return Err(Refused::Damaged);
    }
    let projects = projects
        .into_iter()
        .map(|p| workspace::loaded(p, default_provider))
        .collect();
    let free_chats = free_chats::as_list(read_strict(&dir.join(chats::FREE_CHATS_FILE))?);
    let preferences = match read(&dir.join(PREFERENCES_FILE))? {
        Some(v) if v.truthy() => v,
        _ => JValue::Obj(Vec::new()),
    };
    Ok(View {
        dir,
        projects,
        free_chats,
        preferences,
    })
}

impl View {
    /// The account's directory (`workspace.dir`).
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// `workspace.autoRoles`: the saved roles, `null` when none (or not JSON).
    pub fn auto_roles(&self) -> Result<JValue, Refused> {
        Ok(read(&self.dir.join(AUTO_ROLES_FILE))?.unwrap_or(JValue::Null))
    }

    /// `getProject(id)`: `None` when no project has that id.
    pub fn project(&self, id: &str) -> Result<Option<Project>, Refused> {
        let Some(found) = self
            .projects
            .iter()
            .find(|p| p.get("id").as_str() == Some(id))
        else {
            return Ok(None);
        };
        let effects = chats::get_project_effects(found).map_err(|_| Refused::Unsupported)?;
        // Node's save is queued and serialises the project after getProject returns, so it writes
        // the reconciled project even when only the heal asked for the save.
        let save = effects.saved.is_some().then(|| effects.memory.clone());
        Ok(Some(Project {
            project: effects.memory,
            save,
        }))
    }

    /// A source's text as core's turn reads it: `String(file.content || '')` (`''` when its file
    /// cannot be read; `None` for an inline value `String()` throws on, an object with its own
    /// `toString`).
    pub fn text(&self, project_id: &str, file: &JValue) -> Option<String> {
        if let JValue::Obj(items) = file {
            if let Some((_, content)) = items.iter().find(|(k, _)| k == "content") {
                return match content {
                    JValue::Str(s) => Some(s.clone()),
                    v if !v.truthy() => Some(String::new()),
                    v => js_json::to_js_string(v).ok(),
                };
            }
        }
        Some(sources::text(&self.dir, project_id, file).unwrap_or_default())
    }
}
