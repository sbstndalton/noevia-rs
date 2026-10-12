//! The free-chat list: core routes/chat-lists.cjs `GET` and `POST /api/freechats`, with the store
//! calls they make (projects.cjs `chatIdsElsewhere(null)`, `saveFreeChats`, workspace.cjs
//! `saveFreeChats`) and chat-lists.cjs `mergeChats`, answer for answer. The third M4 slice; the
//! front answers it only in a build with the cargo feature `rust-free-chats`.
//!
//! - GET: `{ chats }`, the stored list as it is (Node serves its workspace copy unsanitised): the
//!   file read fresh, `[]` when it is missing or not JSON (workspace.cjs `readJson(file, [])`).
//! - POST `{ chats: [...] }`: each meta normalised as Node does (id reduced to `[A-Za-z0-9_-]`, 80
//!   units; title 120 (`New chat` when falsy) and preview 200 UTF-16 units; `mode: 'cowork'` kept
//!   for an admin only), then merged into the stored list: same-id entries replaced keeping the
//!   stored frame, routing flags and first `createdAt`; tombstoned ids (deleted-chats.json) never
//!   come back; ids a project of the account holds are skipped and reported (`skipped`, #765);
//!   newest first, 1,000 at most. Written under `free-chats.json.lock`, the lock Node's own writers
//!   of the file (a chat's move, a free chat's delete, the routing flags, an import, the retention
//!   sweep) take while the slice is on (core rust-projects.cjs `createFreeChatsFile`, which merges
//!   its saves per item, so neither side drops the other's metas).
//!
//! `DELETE /api/freechats/{id}` stays Node's: it also removes the chat's transcript, context meter,
//! brain note and attachments project, which this crate does not do.
//!
//! Untrusted input it hardens: the JSON body (at most 1 MiB, Node's readBody limit; 1,000 metas;
//! lone surrogates kept as Node keeps them), and the stored lists it reads back (free-chats.json,
//! projects.json for the other lists' ids, deleted-chats.json).
//!
//! Stricter than Node, as refusals (500): a stored free list that is `null` (Node's GET throws
//! after it wrote its status line) or an object with a `length` key (Node serialises it as an
//! array-like); a stored meta whose `updatedAt` is a string or an object (Node's sort is then
//! inconsistent); and what [`crate::chats`] refuses in a body or a file.

use crate::chats::{self, SaveFailure, FREE_CHATS_FILE};
use crate::store;
use crate::{Ctx, Reply};
use js_json::wtf16;
use js_json::JValue;
use std::collections::HashSet;

/// What `JSON.stringify({ chats: FREE_CHATS })` makes of the stored value: FREE_CHATS is a Proxy
/// over an array (index.cjs `arrayProxy`), so it serialises as an array whatever the workspace
/// holds: its `length` and indices are the stored value's. `None` where this port refuses.
fn as_served(stored: JValue) -> Option<Vec<JValue>> {
    match stored {
        JValue::Arr(items) => Some(items),
        // `"ab".length` and `"ab"[i]`: one string per UTF-16 unit, a pair cut in two.
        JValue::Str(s) => {
            let mut units = Vec::new();
            for c in s.chars() {
                let mut buf = [0u16; 2];
                let encoded = c.encode_utf16(&mut buf);
                if wtf16::lone_unit(c).is_some() || encoded.len() == 1 {
                    units.push(JValue::Str(c.to_string()));
                } else {
                    for unit in encoded.iter() {
                        units.push(JValue::Str(wtf16::lone_char(*unit)?.to_string()));
                    }
                }
            }
            Some(units)
        }
        // No `length`: an empty array.
        JValue::Num(_) | JValue::Bool(_) => Some(Vec::new()),
        JValue::Obj(ref items) if !items.iter().any(|(k, _)| k == "length") => Some(Vec::new()),
        _ => None,
    }
}

/// The projects workspace.cjs `get()` loads before any route runs (switch off: `readJson(file,
/// { projects: [] }).projects || []`, then its `provider` and `modes` loops), or the 500 Node
/// answers when that load throws: a file holding `null`, a truthy `projects` that is not an
/// array, an entry that is `null` or a primitive (`project.modes = [...]` throws in strict mode).
fn workspace_projects(dir: &std::path::Path) -> Result<Vec<JValue>, Reply> {
    let stored =
        store::read_json(&dir.join(store::PROJECTS_FILE)).map_err(|e| chats::store_failure(&e))?;
    let projects = match stored {
        None => return Ok(Vec::new()),
        Some(JValue::Null) => return Err(Reply::internal()),
        Some(v) => match v.get("projects") {
            JValue::Arr(items) => items.clone(),
            p if !p.truthy() => return Ok(Vec::new()),
            _ => return Err(Reply::internal()),
        },
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
        Ok(v) => v.unwrap_or(JValue::Arr(Vec::new())),
        Err(e) => return chats::store_failure(&e),
    };
    let Some(list) = as_served(stored) else {
        return Reply::internal();
    };
    let out = JValue::obj([("chats", JValue::Arr(list))]);
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
    // `Array.from(FREE_CHATS)` and then `FREE_CHATS.splice(...)`: only an array has `splice`, so
    // any other stored value throws (readJson's fallback, [], for a missing or non-JSON file).
    let current = match stored {
        None => Vec::new(),
        Some(JValue::Arr(items)) => items,
        Some(_) => return Err(SaveFailure::Throw),
    };
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
        Err(reply) => return reply,
    };
    let saved = store::update_free(&dir, ctx.switch, |stored| {
        // projects.json as it is now (Node's request-start view; its writes are atomic renames).
        let projects = match workspace_projects(&dir) {
            Ok(p) => p,
            Err(reply) => return (reply, None),
        };
        match save(&dir, &projects, stored, &incoming) {
            Ok((list, skipped)) => (chats::saved_reply(skipped), Some(list)),
            Err(SaveFailure::Throw) => (chats::invalid_json(), None),
            Err(SaveFailure::Unsupported) => (Reply::internal(), None),
        }
    });
    match saved {
        Ok(reply) => reply,
        Err(e) => chats::store_failure(&e),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn served_like_nodes_array_proxy() {
        let s = |v: &str| {
            wtf16::stringify(&JValue::Arr(as_served(wtf16::parse(v).unwrap()).unwrap())).unwrap()
        };
        assert_eq!(s("[{\"id\":\"a\"},null,3]"), "[{\"id\":\"a\"},null,3]");
        assert_eq!(s("\"ab\""), "[\"a\",\"b\"]");
        assert_eq!(s("\"\\ud83d\\ude00x\""), "[\"\\ud83d\",\"\\ude00\",\"x\"]");
        assert_eq!(s("\"\\ud83d\""), "[\"\\ud83d\"]");
        assert_eq!(s("5"), "[]");
        assert_eq!(s("true"), "[]");
        assert_eq!(s("{\"a\":1}"), "[]");
        assert!(as_served(JValue::Null).is_none());
        assert!(as_served(wtf16::parse("{\"length\":1}").unwrap()).is_none());
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
