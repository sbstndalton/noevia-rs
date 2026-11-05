//! What GET /api/reasoning-settings does with the account's projects (core routes/reasoning-settings.cjs, which
//! asked `getProject(id)` for `reasoningEffort`, `provider` and `model`), from projects.json as it is on disk now: the
//! file the front alone writes and Node re-reads at each request start.
//!
//! The workspace load comes first on every request, with or without a project id: its normalizations and migrations and the
//! saves they make, and its 500 for a project that is not an object (see [`crate::workspace_load`]). A named project is then
//! `getProject`'s: its routing repair (a context project that was never chosen) and the instruction-skills reconcile, which
//! Node saves when they changed something (sbstndalton/noevia#2388 item 2). Those saves are made under the projects lock, on
//! the file as it is by then; a failed save is the answer, as Node's held response was replaced by the store's failure.

use crate::workspace_load::{self, Target};
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

/// The workspace load (its migrations and the saves they make), and `getProject(id)` when the query named one.
pub fn project(ctx: Ctx<'_>, id: Option<&str>) -> Result<Lookup, Reply> {
    Ok(match workspace_load::open(ctx, id)? {
        Target::Failed(reply) => return Err(reply),
        Target::NoQuery => Lookup::None,
        Target::Missing => Lookup::Missing,
        Target::Found(p) => Lookup::Found(facts_of(&p)),
    })
}
