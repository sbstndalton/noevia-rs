//! The free-chat list: core routes/chat-lists.cjs `GET` and `POST /api/freechats`, with the store
//! calls they make (projects.cjs `chatIdsElsewhere(null)`, `saveFreeChats`, workspace.cjs
//! `saveFreeChats`) and chat-lists.cjs `mergeChats`, answer for answer. The third M4 slice; the
//! front answers it only in a build with the cargo feature `rust-free-chats`.
//!
//! - GET: `{ chats }`, the stored list as it is (Node serves its workspace copy unsanitised): the
//!   file read fresh, `[]` when it is missing or not JSON (workspace.cjs `readJson(file, [])`), or
//!   when it holds anything but a list (`null`, a string, a number, an object: workspace.cjs loads
//!   that as an empty list since #1420).
//! - POST `{ chats: [...] }`: each meta normalised as Node does (id reduced to `[A-Za-z0-9_-]`, 80
//!   units; title 120 (`New chat` when falsy) and preview 200 UTF-16 units; `mode: 'cowork'` kept
//!   for an admin only), then merged into the stored list: same-id entries replaced keeping the
//!   stored frame, routing flags and first `createdAt`; tombstoned ids (deleted-chats.json) never
//!   come back; ids a project of the account holds are skipped and reported (`skipped`, #765);
//!   newest first, 1,000 at most. Written under `free-chats.json.lock`, the lock Node's own writers
//!   of the file (a chat's move, a free chat's delete, the routing flags, an import, the retention
//!   sweep) take while the slice is on (core rust-projects.cjs `createFreeChatsFile`, which merges
//!   its saves per item, so neither side drops the other's metas). A stored value that is not a
//!   list is merged into as an empty list (#1420).
//! - Only reading the request is a 400 (`invalid JSON`; `chats array required` for any body that
//!   is not an object with a `chats` array, `null` included). A save that fails once the request
//!   was understood answers as core http.cjs `saveFailureReply` (#1419): a busy store 503
//!   `PROJECTS_BUSY` with `Retry-After: 1`, a removed account 410, anything else 500 `could not
//!   save the free chats; nothing was changed`.
//!
//! The user's directory is never created here: when it is gone (the account was removed while the
//! request was in flight) the save answers Node's `assertActive` 410 and writes nothing
//! (sbstndalton/noevia#1425; since #1419 Node's own POST answers that 410 as well).
//!
//! `DELETE /api/freechats/{id}` is [`crate::chat_delete`] (sbstndalton/noevia#1071 slice 6).
//!
//! Untrusted input it hardens: the JSON body (at most 1 MiB, Node's readBody limit; 1,000 metas;
//! lone surrogates kept as Node keeps them), and the stored lists it reads back (free-chats.json,
//! projects.json for the other lists' ids, deleted-chats.json).
//!
//! Stricter than Node, as refusals (500): a stored meta whose `updatedAt` is a string or an object
//! (Node's sort is then inconsistent); and what [`crate::chats`] refuses in a body or a file.

use crate::chats::{self, Incoming, SaveFailure, FREE_CHATS_FILE};
use crate::store;
use crate::{Ctx, Reply};
use js_json::wtf16;
use js_json::JValue;
use std::collections::HashSet;

/// routes/chat-lists.cjs's `saveFailureReply` message for the free-chat list.
pub const FREE_CHATS_SAVE_FAILED: &str = "could not save the free chats; nothing was changed";

/// The free list core's workspace holds for a stored value (workspace.cjs since #1420:
/// `Array.isArray(list) ? list : []`), which GET serves and POST merges into.
pub(crate) fn as_list(stored: Option<JValue>) -> Vec<JValue> {
    match stored {
        Some(JValue::Arr(items)) => items,
        _ => Vec::new(),
    }
}

/// The projects workspace.cjs `get()` loads before any route runs (switch off: `(readJson(file,
/// { projects: [] }) || {}).projects`, kept only when it is an array, else `[]` (core#126,
/// sbstndalton/noevia#1806: a file holding `null` or a `projects` that is not an array loads as an
/// empty list), then its `provider` and `modes` loops), or the 500 Node answers when that load
/// throws: an entry that is `null` or a primitive (`project.modes = [...]` throws in strict mode).
pub(crate) fn workspace_projects(dir: &std::path::Path) -> Result<Vec<JValue>, Reply> {
    let stored =
        store::read_json(&dir.join(store::PROJECTS_FILE)).map_err(|e| chats::store_failure(&e))?;
    let projects = match stored {
        Some(v) => match v.get("projects") {
            JValue::Arr(items) => items.clone(),
            _ => return Ok(Vec::new()),
        },
        None => return Ok(Vec::new()),
    };
    if projects
        .iter()
        .all(|p| matches!(p, JValue::Obj(_) | JValue::Arr(_)))
    {
        Ok(projects)
    } else {
        Err(Reply::internal())
    }
}

/// GET /api/freechats.
pub fn get(ctx: Ctx<'_>) -> Reply {
    let dir = match store::user_dir(ctx.data_dir, ctx.user_id) {
        Ok(d) => d,
        Err(e) => return chats::store_failure(&e),
    };
    if let Err(reply) = workspace_projects(&dir) {
        return reply;
    }
    let stored = match store::read_json(&dir.join(FREE_CHATS_FILE)) {
        Ok(v) => v,
        Err(e) => return chats::store_failure(&e),
    };
    // routes/chat-lists.cjs reads the list before the status line (#1420): `Array.from` of an
    // array never throws, so its 500 `could not read the free chats` has no case here.
    let out = JValue::obj([("chats", JValue::Arr(as_list(stored)))]);
    Reply::json(200, wtf16::stringify(&out).unwrap_or_default())
}

/// `chatIdsElsewhere(null)`: every chat id in every project of the account, as
/// `for (const proj of PROJECTS) if (proj && proj.id !== null) for (const c of proj.chats || [])`
/// reads them.
fn ids_in_projects(projects: &[JValue]) -> Result<HashSet<String>, SaveFailure> {
    let mut ids = HashSet::new();
    for p in projects {
        if p.truthy() && p.get("id") != &JValue::Null {
            chats::list_ids(p.get("chats"), &mut ids).map_err(|()| SaveFailure::Throw)?;
        }
    }
    Ok(ids)
}

/// The part of the POST inside Node's `try`, from `chatIdsElsewhere` on, over the stored value:
/// the list to write and `skipped`.
fn save(
    dir: &std::path::Path,
    projects: &[JValue],
    stored: Option<JValue>,
    incoming: &[JValue],
) -> Result<(Vec<JValue>, Vec<String>), SaveFailure> {
    let elsewhere = ids_in_projects(projects)?;
    let tombstones = chats::read_tombstones(dir)?;
    // `Array.from(FREE_CHATS)`: the workspace's list, empty for a missing, non-JSON or non-list
    // file (#1420; before it, a non-list stored value made `FREE_CHATS.splice` throw).
    let current = as_list(stored);
    let merged = chats::merge_chats(&current, incoming, &tombstones, &elsewhere)?;
    Ok((merged, chats::skipped_elsewhere(incoming, &elsewhere)))
}

/// POST /api/freechats. `body` was read up to [`chats::BODY_LIMIT`]; `over` when it was longer;
/// `admin`: the signed-in user's role is admin (`authn.user.role`).
pub fn post(ctx: Ctx<'_>, body: &[u8], over: bool, admin: bool) -> Reply {
    let dir = match store::user_dir(ctx.data_dir, ctx.user_id) {
        Ok(d) => d,
        Err(e) => return chats::store_failure(&e),
    };
    if let Err(reply) = workspace_projects(&dir) {
        return reply;
    }
    if over {
        // readBody rejects before the route's try: errorResponse's 413.
        return Reply::error(413, "Request exceeds size limit");
    }
    let incoming = match chats::incoming_metas(body, admin, ctx.now_ms, "New chat") {
        Ok(chats) => chats,
        Err(Incoming::Request(reply)) => return reply,
        Err(Incoming::Throw) => return chats::save_failed(FREE_CHATS_SAVE_FAILED),
    };
    let saved = store::update_free(&dir, ctx.switch, |stored| {
        // projects.json as it is now (Node's request-start view; its writes are atomic renames).
        let projects = match workspace_projects(&dir) {
            Ok(p) => p,
            Err(reply) => return (reply, None),
        };
        match save(&dir, &projects, stored, &incoming) {
            Ok((list, skipped)) => (chats::saved_reply(skipped), Some(list)),
            Err(SaveFailure::Throw | SaveFailure::Unsupported) => {
                (chats::save_failed(FREE_CHATS_SAVE_FAILED), None)
            }
        }
    });
    match saved {
        Ok(reply) => reply,
        // The lock, the re-read and the write are saveFreeChats's, inside core's `try`.
        Err(e) => chats::save_failure(&e, FREE_CHATS_SAVE_FAILED),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    // #1420: a stored list is served and merged as it is; anything else is the empty list.
    #[test]
    fn only_a_stored_list_is_a_list() {
        let s = |v: Option<&str>| {
            wtf16::stringify(&JValue::Arr(as_list(v.map(|v| wtf16::parse(v).unwrap())))).unwrap()
        };
        assert_eq!(
            s(Some("[{\"id\":\"a\"},null,3]")),
            "[{\"id\":\"a\"},null,3]"
        );
        assert_eq!(s(Some("[]")), "[]");
        for other in [
            "null",
            "\"ab\"",
            "\"\\ud83d\\ude00x\"",
            "5",
            "true",
            "{\"a\":1}",
            "{\"length\":1,\"0\":\"x\"}",
        ] {
            assert_eq!(s(Some(other)), "[]", "{other}");
        }
        assert_eq!(s(None), "[]");
    }

    #[test]
    fn other_lists_ids_as_node_iterates_them() {
        let projects = match wtf16::parse(
            "[{\"id\":\"p\",\"chats\":[{\"id\":\"a\"},null,{\"id\":3}]},null,{\"id\":null,\"chats\":[{\"id\":\"skipped\"}]},{\"chats\":[{\"id\":\"no-id\"}]},{\"id\":\"s\",\"chats\":\"str\"},7]",
        )
        .unwrap()
        {
            JValue::Arr(p) => p,
            _ => unreachable!(),
        };
        let ids = ids_in_projects(&projects).ok().unwrap();
        let mut ids: Vec<_> = ids.into_iter().collect();
        ids.sort();
        assert_eq!(ids, ["a", "no-id"]);
        let throws = vec![wtf16::parse("{\"id\":\"p\",\"chats\":{}}").unwrap()];
        assert!(matches!(ids_in_projects(&throws), Err(SaveFailure::Throw)));
    }
}
