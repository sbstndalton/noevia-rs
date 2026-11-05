//! What GET /api/reasoning-settings does with the account's projects (core routes/reasoning-settings.cjs, which
//! asked `getProject(id)` for `reasoningEffort`, `provider` and `model`), from projects.json as it is on disk now: the
//! file the front alone writes and Node re-reads at each request start.
//!
//! The workspace load comes first on every request, with or without a project id (its 500 for a project that is not an
//! object). A named project is then `getProject`'s: its routing repair (a context project that was never chosen) and the
//! instruction-skills reconcile, which Node saves when they changed something (sbstndalton/noevia#2388 item 2). Those saves
//! are made here under the projects lock, on the file as it is by then; a failed save is the answer, as Node's held
//! response was replaced by the store's failure.

use crate::chats;
use crate::free_chats;
use crate::store;
use crate::{Ctx, Reply};
use js_json::JValue;

/// The three fields of a project the reasoning-settings answer depends on, as stored.
#[derive(Debug, Clone, PartialEq)]
pub struct Facts {
    pub effort: JValue,
    pub provider: JValue,
    pub model: JValue,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Lookup {
    /// No `projectId` in the query.
    None,
    /// `PROJECTS.find((p) => p.id === id)` found nothing (Node's 404).
    Missing,
    Found(Facts),
}

fn facts_of(p: &JValue) -> Facts {
    Facts {
        effort: p.get("reasoningEffort").clone(),
        provider: p.get("provider").clone(),
        model: p.get("model").clone(),
    }
}

/// The workspace load, and `getProject(id)` when the query named one.
pub fn project(ctx: Ctx<'_>, id: Option<&str>) -> Result<Lookup, Reply> {
    let dir = store::user_dir(ctx.data_dir, ctx.user_id).map_err(|e| chats::store_failure(&e))?;
    let projects = free_chats::workspace_projects(&dir)?;
    let Some(id) = id else {
        return Ok(Lookup::None);
    };
    let Ok(Some(at)) = chats::locate(&projects, id) else {
        return Ok(Lookup::Missing);
    };
    let Some(found) = projects.get(at) else {
        return Ok(Lookup::Missing);
    };
    let effects = chats::get_project_effects(found).map_err(|_| Reply::internal())?;
    if effects.saved.is_none() {
        return Ok(Lookup::Found(facts_of(&effects.memory)));
    }
    // getProject saved something: do it again on the file as it is now, under the lock.
    let done = store::update(&dir, ctx.switch, |projects| {
        let Ok(Some(at)) = chats::locate(projects, id) else {
            return (Ok(Lookup::Missing), false);
        };
        let Some(p) = projects.get_mut(at) else {
            return (Ok(Lookup::Missing), false);
        };
        match chats::get_project_effects(p) {
            Err(_) => (Err(Reply::internal()), false),
            Ok(effects) => {
                // Node's save is made after the reconcile has assigned its result, so what is written is the
                // project as it is in memory by then (a healed project also gains its `instructionSkills`).
                let changed = effects.saved.is_some();
                if changed {
                    *p = effects.memory.clone();
                }
                (Ok(Lookup::Found(facts_of(&effects.memory))), changed)
            }
        }
    });
    match done {
        Ok(result) => result,
        Err(e) => Err(chats::store_failure(&e)),
    }
}
