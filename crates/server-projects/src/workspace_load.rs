//! What core's workspace load (workspace.cjs `createWorkspaceStore.get`, a fresh load) makes of an account's projects before
//! any route sees them, and what it saves; for the routes that start with `currentWorkspace()` (sbstndalton/noevia#2384).
//!
//! Normalizations, in memory only until some save carries them (every save writes the whole normalized list):
//! - workspace.cjs:240 (#1236): a project's `files` array drops the entries that are not objects;
//! - workspace.cjs:241: `provider === 'lemonade'` reads as the default provider's id.
//!
//! Migrations, saved at load (workspace.cjs:400 `if (modesMigrated || textMigrated) workspace.saveProjects()`):
//! - workspace.cjs:245-256 + project-modes.cjs:16-25 `migrate` (#1576): `modes` names chat first and keeps cowork and code, in
//!   that order; projects.json is first copied to `migration-backups/projects-pre-1576-<sha256 prefix>.json`. If the backup
//!   cannot be made the migration is not written (#1839: the original `modes` stay in what a later save writes);
//! - workspace.cjs:258 + source-text.cjs:361-369 `migrateOnLoad` (#1806): a source's inline `content` string moves to
//!   `project-sources/<id>/<sha256>.txt` (the entry keeps `textRef`, `chars`, `skillHeader`), after a copy of projects.json to
//!   `migration-backups/projects-pre-1806-…`. A backup or a write that fails makes the load itself fail (500).
//!
//! `getProject(id)` (projects.cjs:72) then runs on the normalized list: the routing repair of a never-chosen
//! `cowork-chat-context-*` project and the instruction-skills reconcile, which it saves when they changed something. Node
//! built the payload of any save after those ran, so a save that happens for any reason carries the reconcile's assignment
//! to the target project too.
//!
//! Not ported: the sealing of plaintext provider keys at load (workspace.cjs:189-207, 212-236, 397-400, providers.json, the providers
//! front's); source-text.cjs `collect` (it removes unreferenced texts only after 24 hours across saves of one process).

use crate::chats;
use crate::free_chats;
use crate::lifecycle;
use crate::store;
use crate::{Ctx, Reply};
use js_json::{wtf16, JValue};
use std::path::Path;

/// core index.cjs `DEFAULT_PROVIDER_ID`.
pub(crate) fn default_provider_id() -> String {
    std::env::var("DEFAULT_PROVIDER_ID")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "default".to_owned())
}

/// What the normalizations found to change that is saved at load.
#[derive(Default)]
pub(crate) struct Touched {
    /// project-modes.cjs:16-25 `migrate` returned true (an array entry counts: it takes a property JSON never carries).
    pub modes: bool,
    /// The modes this project had before, to put back when the backup could not be made (#1839).
    pub had: Option<JValue>,
}

/// workspace.cjs:240-248 for one project, in place.
pub(crate) fn normalize_one(project: &mut JValue, default_provider: &str) -> Touched {
    let mut touched = Touched::default();
    match project {
        JValue::Obj(_) => {
            if let JValue::Arr(files) = project.get("files") {
                if files
                    .iter()
                    .any(|f| !matches!(f, JValue::Obj(_) | JValue::Arr(_)))
                {
                    let kept: Vec<JValue> = files
                        .iter()
                        .filter(|f| matches!(f, JValue::Obj(_) | JValue::Arr(_)))
                        .cloned()
                        .collect();
                    store::set(project, "files", JValue::Arr(kept));
                }
            }
            if project.get("provider").as_str() == Some("lemonade") {
                store::set(project, "provider", JValue::from(default_provider));
            }
            // project-modes.cjs migrate(): chat first, then cowork and code if recorded, nothing else.
            let before: Option<Vec<JValue>> = match project.get("modes") {
                JValue::Arr(items) => Some(items.clone()),
                _ => None,
            };
            let mut next = vec![JValue::from("chat")];
            for mode in ["cowork", "code"] {
                if before
                    .as_ref()
                    .is_some_and(|b| b.iter().any(|m| m.as_str() == Some(mode)))
                {
                    next.push(JValue::from(mode));
                }
            }
            if before.as_ref() != Some(&next) {
                touched.had = match project.get("modes") {
                    JValue::Arr(items) => Some(JValue::Arr(items.clone())),
                    _ => None,
                };
                store::set(project, "modes", JValue::Arr(next));
                touched.modes = true;
            }
        }
        // An array is an object to `migrate`: it is "migrated" by a property the JSON of an array never has.
        JValue::Arr(_) => touched.modes = true,
        _ => {}
    }
    touched
}

fn inline_text(file: &JValue) -> Option<&str> {
    match file {
        JValue::Obj(items) => items
            .iter()
            .find(|(k, _)| k == "content")
            .and_then(|(_, v)| v.as_str()),
        _ => None,
    }
}

fn has_inline(projects: &[JValue]) -> bool {
    projects.iter().any(|p| match p.get("files") {
        JValue::Arr(files) => files.iter().any(|f| inline_text(f).is_some()),
        _ => false,
    })
}

/// source-text.cjs `backupProjectsFile(dir, tag)`: the bytes of projects.json, under a name made of them; never overwritten.
fn backup_projects_file(dir: &Path, tag: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let Ok(bytes) = std::fs::read(dir.join("projects.json")) else {
        return Ok(());
    };
    let folder = dir.join("migration-backups");
    store::create_dirs_below(dir, &folder)?;
    let sha = lifecycle::sha_hex_bytes(&bytes);
    let prefix = sha.get(..16).unwrap_or(&sha);
    let target = folder.join(format!("projects-pre-{tag}-{prefix}.json"));
    if target.exists() {
        return Ok(());
    }
    let tmp = folder.join(format!(
        "projects-pre-{tag}-{prefix}.json.{}.tmp",
        std::process::id()
    ));
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(&bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, &target)
}

/// What `Buffer.from(text, 'utf8')` writes: a lone surrogate (held here as a private mapped character) is U+FFFD.
fn utf8_of(text: &str) -> String {
    text.chars()
        .map(|c| {
            if wtf16::lone_unit(c).is_some() {
                '\u{fffd}'
            } else {
                c
            }
        })
        .collect()
}

/// source-text.cjs `externalize` for every project of the list: each inline text goes to its file, then the entry becomes
/// `{ …, textRef, chars, skillHeader? }`.
fn externalize_all(dir: &Path, projects: &mut [JValue]) -> std::io::Result<()> {
    for project in projects.iter_mut() {
        let Some(id) = project.get("id").as_str().map(str::to_owned) else {
            continue;
        };
        let JValue::Obj(fields) = project else {
            continue;
        };
        let Some((_, JValue::Arr(files))) = fields.iter_mut().find(|(k, _)| k == "files") else {
            continue;
        };
        for f in files.iter_mut() {
            let Some(text) = inline_text(f).map(str::to_owned) else {
                continue;
            };
            let bytes = utf8_of(&text);
            let reference = lifecycle::sha_hex_bytes(bytes.as_bytes());
            lifecycle::write_text(dir, &id, &reference, &bytes)?;
            if let JValue::Obj(items) = f {
                items.retain(|(k, _)| k != "content");
            }
            store::set(f, "textRef", JValue::Str(reference));
            #[allow(clippy::cast_precision_loss)]
            let chars = JValue::Num(wtf16::len(&text) as f64);
            store::set(f, "chars", chars);
            if lifecycle::header_names_skill(&text) {
                store::set(f, "skillHeader", JValue::Bool(true));
            } else if let JValue::Obj(items) = f {
                items.retain(|(k, _)| k != "skillHeader");
            }
        }
    }
    Ok(())
}

/// The normalizations over the whole list. Returns whether the modes migration is to be saved. With `backups` the copy of
/// projects.json it needs is made here; if that fails the migration is not written and the modes stay as stored (#1839).
fn normalize_all(dir: &Path, projects: &mut [JValue], backups: bool) -> bool {
    let default_provider = default_provider_id();
    let touched: Vec<Touched> = projects
        .iter_mut()
        .map(|p| normalize_one(p, &default_provider))
        .collect();
    if !touched.iter().any(|t| t.modes) {
        return false;
    }
    if !backups || backup_projects_file(dir, "1576").is_ok() {
        return true;
    }
    for (p, t) in projects.iter_mut().zip(touched) {
        if !t.modes {
            continue;
        }
        match (&mut *p, t.had) {
            (JValue::Obj(_), Some(had)) => store::set(p, "modes", had),
            (JValue::Obj(items), None) => items.retain(|(k, _)| k != "modes"),
            _ => {}
        }
    }
    false
}

/// The project `getProject` left, or why there is none.
pub(crate) enum Target {
    /// No project was asked for.
    NoQuery,
    /// `PROJECTS.find((p) => p.id === id)` found nothing.
    Missing,
    /// The project as Node holds it afterwards.
    Found(JValue),
    /// The workspace migrated successfully, but getProject threw afterwards.
    Failed(Reply),
}

struct Pass {
    projects: Vec<JValue>,
    target: Target,
    /// Whether anything saves: a migration at load, or `getProject`'s repair or reconcile.
    save: bool,
}

/// The load and `getProject(id)` over the list as it is now. `backups` is false for the read-only probe (no file is
/// touched); true under the lock, where the backups are made and the texts moved before the list is returned for writing.
fn pass(dir: &Path, raw: Vec<JValue>, id: Option<&str>, backups: bool) -> Result<Pass, Reply> {
    let mut projects = raw;
    let modes = normalize_all(dir, &mut projects, backups);
    let inline = has_inline(&projects);
    // The probe must not reconcile inline sources before externalize has assigned skillHeader.
    if inline && !backups {
        return Ok(Pass {
            projects,
            target: Target::NoQuery,
            save: true,
        });
    }
    if inline && backups {
        backup_projects_file(dir, "1806").map_err(|_| Reply::internal())?;
        externalize_all(dir, &mut projects).map_err(|_| Reply::internal())?;
    }
    let mut save = modes || inline;
    let target = match id {
        None => Target::NoQuery,
        Some(id) => match chats::locate(&projects, id) {
            Ok(Some(at)) => {
                let found = projects.get(at).ok_or_else(Reply::internal)?;
                let (effects, result) = chats::get_project_attempt(found);
                // Node saves its routing repair before reconcile can throw (projects.cjs:80-84).
                // Keep that partial save alongside any load migration, under this same lock.
                save = save || effects.saved.is_some();
                let memory = effects.memory;
                if let Some(slot) = projects.get_mut(at) {
                    *slot = memory.clone();
                }
                if result.is_err() {
                    return Ok(Pass {
                        projects,
                        target: Target::Failed(Reply::internal()),
                        save,
                    });
                }
                Target::Found(memory)
            }
            _ => Target::Missing,
        },
    };
    Ok(Pass {
        projects,
        target,
        save,
    })
}

/// The workspace load, then `getProject(id)` when one is named: saves what Node saved, under the projects lock.
pub(crate) fn open(ctx: Ctx<'_>, id: Option<&str>) -> Result<Target, Reply> {
    let dir = store::user_dir(ctx.data_dir, ctx.user_id).map_err(|e| chats::store_failure(&e))?;
    // The list as the workspace reads it (a file it cannot read is the empty list), and Node's 500 for an entry that is not an object.
    let raw = free_chats::workspace_projects(&dir)?;
    let probe = pass(&dir, raw, id, false)?;
    if !probe.save {
        return Ok(probe.target);
    }
    let done = store::update(&dir, ctx.switch, |projects| {
        match pass(&dir, std::mem::take(projects), id, true) {
            Err(reply) => (Err(reply), false),
            Ok(p) => {
                if p.save {
                    *projects = p.projects;
                }
                (Ok(p.target), p.save)
            }
        }
    });
    match done {
        Ok(result) => result,
        Err(e) => Err(chats::store_failure(&e)),
    }
}

/// The list as `saveProjects()` writes it after the load: normalized, with a modes migration whose backup could not be made
/// still as stored.
pub(crate) fn for_save(dir: &Path, mut projects: Vec<JValue>) -> Vec<JValue> {
    normalize_all(dir, &mut projects, true);
    projects
}
