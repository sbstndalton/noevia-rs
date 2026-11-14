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
//! - [`View::text`]: a source's text (source-text.cjs's `content`: an inline text as it is, else the
//!   text file its `textRef` names, `''` when that cannot be read).
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
//!   ([`Refused::Unsupported`]: its `files` a string, a file that is not an object).
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

/// workspace.cjs `get(user)`, read only. A missing account directory is an empty workspace.
pub fn load(data_dir: &Path, user: &str, default_provider: &str) -> Result<View, Refused> {
    let dir = store::user_dir(data_dir, user).map_err(|_| Refused::InvalidUser)?;
    // free_chats::workspace_projects, with its two refusals told apart.
    let projects = match read(&dir.join(store::PROJECTS_FILE))? {
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
    let free_chats = free_chats::as_list(read(&dir.join(chats::FREE_CHATS_FILE))?);
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
        Ok(Some(Project {
            project: effects.memory,
            save: effects.saved,
        }))
    }

    /// A source's text, as `file.content` reads in core (`''` when its file cannot be read).
    pub fn text(&self, project_id: &str, file: &JValue) -> String {
        sources::text(&self.dir, project_id, file).unwrap_or_default()
    }
}
