//! What GET /api/reasoning-settings reads of one project (core routes/reasoning-settings.cjs, which asked
//! `getProject(id)` for `reasoningEffort`, `provider` and `model`), from projects.json as it is on disk now: the
//! file the front alone writes and Node re-reads at each request start. Read only: Node's `getProject` also saved a
//! legacy routing repair and an instruction-skill reconcile as a side effect of the lookup; those stay with the
//! chat turn's lookup, which still runs them, and neither changes the three fields read here.

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

/// `PROJECTS.find((p) => p.id === id)`: `Ok(None)` when no project has that id (Node's 404), or Node's 500 where its
/// workspace load throws.
pub fn project(ctx: Ctx<'_>, id: &str) -> Result<Option<Facts>, Reply> {
    let dir = store::user_dir(ctx.data_dir, ctx.user_id).map_err(|e| chats::store_failure(&e))?;
    let projects = free_chats::workspace_projects(&dir)?;
    Ok(store::find(&projects, id).map(|p| Facts {
        effort: p.get("reasoningEffort").clone(),
        provider: p.get("provider").clone(),
        model: p.get("model").clone(),
    }))
}
