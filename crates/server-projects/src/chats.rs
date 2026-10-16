//! A project's chat list: core routes/projects.cjs `GET` and `POST /api/projects/{id}/chats`, with
//! the store calls they make (projects.cjs `getProject`, `loadChats`, `saveChats`,
//! `chatIdsElsewhere`, `saveProjects`) and chat-lists.cjs `mergeChats`, answer for answer.
//!
//! - GET: `{ chats }`, the project's chat metas that are objects with a string id. `getProject`'s
//!   own writes come first: a standalone chat's context project still on the template's
//!   `routing: 'manual'` is healed to `auto` (#352), and the project's instruction skills are
//!   reconciled; Node saves either change, so Rust writes it too.
//! - POST `{ chats: [...] }`: each meta normalised as Node does (id reduced to `[A-Za-z0-9_-]`, 80
//!   units; title 120 and preview 200 UTF-16 units; `mode: 'cowork'` kept for an admin only), then
//!   merged into the stored list: same-id entries replaced keeping the stored frame, routing flags
//!   and first `createdAt`; tombstoned ids (deleted-chats.json) never come back; ids another list
//!   of the account holds (its free chats, its other projects) are skipped and reported
//!   (`skipped`, #765); newest first, 1,000 at most. Then every project's instruction skills are
//!   reconciled (`saveProjects`) and the file is written under the lock shared with Node.
//! - Only reading the request is a 400 (`invalid JSON`; `chats array required` for any body that
//!   is not an object with a `chats` array, `null` included). A save that fails once the request
//!   was understood answers as core http.cjs `saveFailureReply` (#1406): a busy store 503
//!   `PROJECTS_BUSY` with `Retry-After: 1`, a removed account 410, anything else 500 `could not
//!   save the chats; nothing was changed`.
//!
//! Untrusted input it hardens: the project id from the URL (`decodeURIComponent`, compared, never
//! a path), the JSON body (at most 1 MiB, Node's readBody limit; 1,000 metas; lone surrogates kept
//! as Node keeps them), and the stored lists it reads back.
//!
//! Stricter than Node, as refusals: a stored list or meta of a shape Node never writes (a chat
//! whose `updatedAt` is a string or an object, which Node's sort would order inconsistently), a
//! number past the `f64` range in the body (Node: `Infinity`, 400 here), and a body or file
//! holding a character of U+10F800..=U+10FFFF ([`js_json::wtf16`]).

use crate::frame;
use crate::js;
use crate::skills;
use crate::store::{self, StoreError};
use crate::{Ctx, Reply};
use js_json::wtf16;
use js_json::JValue;
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// chat-lists.cjs LIST_CAP.
pub const LIST_CAP: usize = 1000;
/// http.cjs readBody's default limit.
pub const BODY_LIMIT: usize = 1024 * 1024;
/// chat-lists.cjs tombstoneFile.
pub const TOMBSTONES_FILE: &str = "deleted-chats.json";
/// workspace.cjs free chats.
pub const FREE_CHATS_FILE: &str = "free-chats.json";
const CONTEXT_PREFIX: &str = "cowork-chat-context-";

pub(crate) fn store_failure(e: &StoreError) -> Reply {
    crate::assets::store_failure(e)
}

pub(crate) fn invalid_json() -> Reply {
    Reply::error(400, "invalid JSON")
}

/// routes/projects.cjs's `saveFailureReply` message for a project's chat list.
pub const CHATS_SAVE_FAILED: &str = "could not save the chats; nothing was changed";

/// core http.cjs `saveFailureReply(json, res, err, fallback)` (#1406/#1419): the answer to a save
/// that failed after the request was understood. Never a 400: the error's own status is kept (a
/// busy store's 503 `PROJECTS_BUSY`, which says when to retry, and `assertActive`'s 410), and
/// anything else is a 500 with the route's fixed message, so no internal error text is shown.
pub(crate) fn save_failure(e: &StoreError, fallback: &str) -> Reply {
    match e {
        // rust-projects.cjs busy(): its message and code, and the Retry-After json() is given.
        StoreError::Busy => {
            let mut text = String::from("{\"error\":");
            js_json::quote("The project store is busy. Try again shortly.", &mut text);
            text.push_str(",\"code\":\"PROJECTS_BUSY\"}");
            Reply {
                status: 503,
                body: crate::Body::Bytes {
                    headers: vec![
                        ("content-type", "application/json".into()),
                        ("cache-control", "no-store".into()),
                        ("retry-after", "1".into()),
                    ],
                    bytes: text.into_bytes(),
                },
            }
        }
        // workspace.cjs assertActive: status 410 and no code.
        StoreError::Gone => Reply::error(410, "account no longer exists"),
        _ => Reply::error(500, fallback),
    }
}

/// [`save_failure`] for a failure inside the save that is not the store's (Node's catch gets a
/// TypeError, or this port refuses a shape there): the route's fixed 500.
pub(crate) fn save_failed(fallback: &str) -> Reply {
    Reply::error(500, fallback)
}

/// `'key' in o` for a value from JSON.parse (own keys only: Object.prototype has none of the
/// names asked here).
fn has(o: &JValue, key: &str) -> bool {
    matches!(o, JValue::Obj(items) if items.iter().any(|(k, _)| k == key))
}

fn remove(o: &mut JValue, key: &str) {
    if let JValue::Obj(items) = o {
        items.retain(|(k, _)| k != key);
    }
}

/// `PROJECTS.find((p) => p.id === id)`: `Err` where JS throws (a `null` entry before the match).
pub(crate) fn locate(projects: &[JValue], id: &str) -> Result<Option<usize>, ()> {
    for (i, p) in projects.iter().enumerate() {
        if p.is_nullish() {
            return Err(());
        }
        if p.get("id").as_str() == Some(id) {
            return Ok(Some(i));
        }
    }
    Ok(None)
}

/// What `getProject` does to the project it returns.
pub(crate) struct Effects {
    /// The project as Node holds it afterwards (reconcile always assigns `instructionSkills`).
    pub(crate) memory: JValue,
    /// What Node saved, if it saved: after the heal, then again after reconcile when that changed
    /// something. (An unchanged reconcile is not saved, though Node keeps its assignment.)
    pub(crate) saved: Option<JValue>,
}

pub(crate) fn get_project_effects(project: &JValue) -> Result<Effects, skills::Unsupported> {
    let mut memory = project.clone();
    let mut saved = None;
    let heal = !memory.get("routingChosen").truthy()
        && memory.get("routing").as_str() == Some("manual")
        && memory
            .get("id")
            .as_str()
            .is_some_and(|id| id.starts_with(CONTEXT_PREFIX));
    if heal {
        store::set(&mut memory, "routing", JValue::Str("auto".into()));
        saved = Some(memory.clone());
    }
    if skills::reconcile(&mut memory)? {
        saved = Some(memory.clone());
    }
    Ok(Effects { memory, saved })
}

/// `sanitizeChats(chats)`: `Err` where JS throws (a truthy non-array has no `filter`).
fn sanitize(chats: &JValue) -> Result<Vec<JValue>, ()> {
    match chats {
        JValue::Arr(items) => Ok(items
            .iter()
            .filter(|c| c.is_object() && c.get("id").as_str().is_some())
            .cloned()
            .collect()),
        v if !v.truthy() => Ok(Vec::new()),
        _ => Err(()),
    }
}

fn chats_reply(project: &JValue) -> Reply {
    let Ok(chats) = sanitize(project.get("chats")) else {
        return Reply::internal();
    };
    let out = JValue::obj([("chats", JValue::Arr(chats))]);
    Reply::json(200, wtf16::stringify(&out).unwrap_or_default())
}

/// GET /api/projects/{id}/chats.
pub fn get(ctx: Ctx<'_>, raw_project: &str) -> Reply {
    let Some(id) = js::decode_uri_component(raw_project) else {
        return Reply::internal();
    };
    let dir = match store::user_dir(ctx.data_dir, ctx.user_id) {
        Ok(d) => d,
        Err(e) => return store_failure(&e),
    };
    let projects = match store::read(&dir) {
        Ok(p) => p,
        Err(e) => return store_failure(&e),
    };
    let project = match locate(&projects, &id) {
        Err(()) => return Reply::internal(),
        Ok(None) => return Reply::error(404, "no such project"),
        Ok(Some(i)) => match projects.get(i) {
            Some(p) => p,
            None => return Reply::internal(),
        },
    };
    match get_project_effects(project) {
        Err(_) => return Reply::internal(),
        Ok(Effects {
            memory,
            saved: None,
        }) => return chats_reply(&memory),
        Ok(_) => {}
    }
    // getProject saved: do it again on the file as it is now, under the lock.
    let saved = store::update(&dir, ctx.switch, |projects| match locate(projects, &id) {
        Err(()) => (Err(Reply::internal()), false),
        Ok(None) => (Err(Reply::error(404, "no such project")), false),
        Ok(Some(i)) => match projects.get_mut(i) {
            None => (Err(Reply::internal()), false),
            Some(p) => match get_project_effects(p) {
                Err(_) => (Err(Reply::internal()), false),
                Ok(Effects { memory, saved }) => {
                    let changed = saved.is_some();
                    if let Some(v) = saved {
                        *p = v;
                    }
                    (Ok(memory), changed)
                }
            },
        },
    });
    match saved {
        Ok(Ok(project)) => chats_reply(&project),
        Ok(Err(reply)) => reply,
        Err(e) => store_failure(&e),
    }
}

/// Why a chat-list POST body gave no metas.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Incoming {
    /// The request itself is bad, before core's `try`: this 400.
    Request(Reply),
    /// Building the metas threw inside core's `try` (a title or preview `String()` cannot
    /// convert): its catch, `saveFailureReply`'s 500 (#1406/#1419).
    Throw,
}

/// The POST body's metas as routes/projects.cjs builds them.
fn incoming(body: &[u8], admin: bool, now_ms: i64) -> Result<Vec<JValue>, Incoming> {
    incoming_metas(body, admin, now_ms, "New task")
}

/// A chat-list POST body's metas as core builds them (routes/projects.cjs for a project's list,
/// routes/chat-lists.cjs for the free list: the same normalisation, `untitled` the only
/// difference).
pub(crate) fn incoming_metas(
    body: &[u8],
    admin: bool,
    now_ms: i64,
    untitled: &str,
) -> Result<Vec<JValue>, Incoming> {
    let body = wtf16::parse(&String::from_utf8_lossy(body))
        .map_err(|_| Incoming::Request(invalid_json()))?;
    // `!body || typeof body !== 'object' || !Array.isArray(body.chats)`, before the `try` (#1406):
    // a `null` body is this 400 too, no longer a TypeError caught as `invalid JSON`.
    let chats = match &body {
        JValue::Obj(_) | JValue::Arr(_) => match body.get("chats") {
            JValue::Arr(chats) => chats,
            _ => return Err(Incoming::Request(Reply::error(400, "chats array required"))),
        },
        _ => return Err(Incoming::Request(Reply::error(400, "chats array required"))),
    };
    let mut out = Vec::new();
    for c in chats
        .iter()
        .filter(|c| c.is_object() && c.get("id").as_str().is_some())
        .take(LIST_CAP)
    {
        let id: String = js::id_chars(c.get("id").as_str().unwrap_or(""))
            .chars()
            .take(80)
            .collect();
        let text = |key: &str, fallback: &str, cap: usize| -> Result<String, Incoming> {
            let v = c.get(key);
            let s = if v.truthy() {
                js_json::to_js_string(v).map_err(|_| Incoming::Throw)?
            } else {
                fallback.to_string()
            };
            Ok(wtf16::slice(&s, cap))
        };
        let title = text("title", untitled, 120)?;
        let updated = c.get("updatedAt").as_num().unwrap_or(now_ms as f64);
        let preview = text("preview", "", 200)?;
        let mut meta = vec![
            ("id".to_string(), JValue::Str(id)),
            ("title".to_string(), JValue::Str(title)),
            ("updatedAt".to_string(), JValue::Num(updated)),
            ("preview".to_string(), JValue::Str(preview)),
            (
                "pinned".to_string(),
                JValue::Bool(c.get("pinned") == &JValue::Bool(true)),
            ),
            (
                "archived".to_string(),
                JValue::Bool(c.get("archived") == &JValue::Bool(true)),
            ),
        ];
        if has(c, "frame") {
            meta.push(("frame".to_string(), c.get("frame").clone()));
        }
        for key in ["forceLocal", "allowCloud"] {
            if let Some(b) = c.get(key).as_bool() {
                meta.push((key.to_string(), JValue::Bool(b)));
            }
        }
        if let Some(n) = c.get("createdAt").as_num().filter(|n| n.is_finite()) {
            meta.push(("createdAt".to_string(), JValue::Num(n)));
        }
        if admin && c.get("mode").as_str() == Some("cowork") {
            meta.push(("mode".to_string(), JValue::Str("cowork".into())));
        }
        out.push(JValue::Obj(meta));
    }
    Ok(out)
}

/// Why the part of the POST inside Node's `try` did not complete. Both answer
/// `saveFailureReply`'s fixed 500 (#1406/#1419); before core d1e998b Node's catch made a throw
/// there a 400 `invalid JSON`.
pub(crate) enum SaveFailure {
    /// Node throws there (a TypeError: no status, so its catch answers the fixed 500).
    Throw,
    /// Data Node never writes and this port does not reproduce: refused with the same 500.
    Unsupported,
}

/// chat-lists.cjs `withKeptFields(current, chat)`.
fn with_kept_fields(current: Option<&JValue>, chat: &JValue) -> JValue {
    let mut next = chat.clone();
    let cur_has = |k: &str| current.is_some_and(|c| has(c, k));
    let cur_get = |k: &str| current.map_or(JValue::Undefined, |c| c.get(k).clone());
    if !has(chat, "frame") {
        if cur_has("frame") {
            store::set(&mut next, "frame", cur_get("frame"));
        }
    } else if chat.get("frame") != &JValue::Null {
        match frame::normalize(chat.get("frame")) {
            Some(f) => store::set(&mut next, "frame", f),
            None if cur_has("frame") => store::set(&mut next, "frame", cur_get("frame")),
            None => remove(&mut next, "frame"),
        }
    }
    for key in ["forceLocal", "allowCloud"] {
        let stored = cur_get(key);
        if !has(chat, key) {
            if stored.as_bool().is_some() {
                store::set(&mut next, key, stored);
            }
        } else if chat.get(key).as_bool().is_none() {
            if stored.as_bool().is_some() {
                store::set(&mut next, key, stored);
            } else {
                remove(&mut next, key);
            }
        }
    }
    let finite = |v: &JValue| v.as_num().filter(|n| n.is_finite());
    let created = finite(&cur_get("createdAt")).or_else(|| finite(chat.get("createdAt")));
    match created {
        Some(n) => store::set(&mut next, "createdAt", JValue::Num(n)),
        None => remove(&mut next, "createdAt"),
    }
    next
}

/// `(chat.updatedAt || 0)` as the sort compares it; `None` where JS would compare NaN (an
/// inconsistent comparator: V8's order then is its algorithm's, not reproduced here).
fn sort_key(chat: &JValue) -> Option<f64> {
    match chat.get("updatedAt") {
        v if !v.truthy() => Some(0.0),
        JValue::Num(n) => Some(*n),
        JValue::Bool(true) => Some(1.0),
        _ => None,
    }
}

/// chat-lists.cjs `mergeChats(current, incoming, tombstones, elsewhere)`; `current` is the stored
/// list's items (objects with a string id are kept).
pub(crate) fn merge_chats(
    current: &[JValue],
    incoming: &[JValue],
    tombstones: &HashSet<String>,
    elsewhere: &HashSet<String>,
) -> Result<Vec<JValue>, SaveFailure> {
    // A Map keyed by id: a replaced entry keeps its place, a new one goes last.
    let mut list: Vec<JValue> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    fn upsert(list: &mut Vec<JValue>, index: &mut HashMap<String, usize>, id: &str, v: JValue) {
        match index.get(id).and_then(|&i| list.get_mut(i)) {
            Some(slot) => *slot = v,
            None => {
                index.insert(id.to_string(), list.len());
                list.push(v);
            }
        }
    }
    for chat in current {
        if let Some(id) = chat.get("id").as_str() {
            if chat.is_object() && !tombstones.contains(id) {
                upsert(&mut list, &mut index, id, chat.clone());
            }
        }
    }
    for chat in incoming {
        let Some(id) = chat.get("id").as_str() else {
            continue;
        };
        if id.is_empty() || tombstones.contains(id) || elsewhere.contains(id) {
            continue;
        }
        let stored = index.get(id).and_then(|&i| list.get(i));
        let next = with_kept_fields(stored, chat);
        upsert(&mut list, &mut index, id, next);
    }
    let mut keyed = Vec::with_capacity(list.len());
    for chat in list {
        let key = sort_key(&chat).ok_or(SaveFailure::Unsupported)?;
        keyed.push((key, chat));
    }
    // Array.prototype.sort is stable; (b - a) puts the newest first; -0 and 0 compare equal.
    keyed.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    Ok(keyed
        .into_iter()
        .take(LIST_CAP)
        .map(|(_, chat)| chat)
        .collect())
}

/// The ids of a list's chats as `for (const c of list) if (c && typeof c.id === 'string')` reads
/// them; `Err` where `for...of` throws (a truthy value that is not iterable).
pub(crate) fn list_ids(list: &JValue, into: &mut HashSet<String>) -> Result<(), ()> {
    match list {
        JValue::Arr(items) => {
            for c in items {
                if let Some(id) = c.get("id").as_str() {
                    into.insert(id.to_string());
                }
            }
            Ok(())
        }
        // A string iterates its characters, none of which has an id.
        JValue::Str(_) => Ok(()),
        v if !v.truthy() => Ok(()),
        _ => Err(()),
    }
}

/// `saveChats(projectId, chats)` from `chatIdsElsewhere` on, and the `saveProjects` it ends with:
/// `memory` (the project as `getProject` left it, at `projects[at]`) gets the merged list and every
/// project is reconciled. Nothing is changed unless all of it succeeds. Returns `skipped`.
fn save_chats(
    dir: &Path,
    projects: &mut [JValue],
    at: usize,
    memory: JValue,
    incoming: &[JValue],
) -> Result<Vec<String>, SaveFailure> {
    let list_id = memory.get("id").as_str().unwrap_or("").to_string();
    // chatIdsElsewhere(p.id): the free chats, then every other project's chats.
    let mut elsewhere = HashSet::new();
    let free =
        store::read_json(&dir.join(FREE_CHATS_FILE)).map_err(|_| SaveFailure::Unsupported)?;
    // workspace.cjs loads a free-chats.json that is not a list (null, an object, a string, a
    // number) as an empty list (#1420), and readJson's fallback for a missing file is [] too.
    if let Some(v @ JValue::Arr(_)) = free {
        list_ids(&v, &mut elsewhere).map_err(|()| SaveFailure::Throw)?;
    }
    for (i, p) in projects.iter().enumerate() {
        let p = if i == at { &memory } else { p };
        if p.truthy() && p.get("id").as_str() != Some(list_id.as_str()) {
            list_ids(p.get("chats"), &mut elsewhere).map_err(|()| SaveFailure::Throw)?;
        }
    }
    let tombstones = read_tombstones(dir)?;
    let merged = match memory.get("chats") {
        JValue::Arr(items) => merge_chats(items, incoming, &tombstones, &elsewhere)?,
        v if !v.truthy() || matches!(v, JValue::Str(_)) => {
            merge_chats(&[], incoming, &tombstones, &elsewhere)?
        }
        _ => return Err(SaveFailure::Throw),
    };
    let mut target = memory;
    store::set(&mut target, "chats", JValue::Arr(merged));
    // saveProjects(PROJECTS): reconcile runs on every project (a TypeError on one Node cannot
    // reconcile, inside the route's try). Planned for all before any is changed.
    let mut plans = Vec::with_capacity(projects.len());
    for (i, p) in projects.iter().enumerate() {
        let p = if i == at { &target } else { p };
        plans.push(skills::plan(p).map_err(|_| SaveFailure::Throw)?);
    }
    if let Some(slot) = projects.get_mut(at) {
        *slot = target;
    }
    for (p, plan) in projects.iter_mut().zip(plans) {
        if let skills::Plan::Set { value, .. } = plan {
            store::set(p, "instructionSkills", value);
        }
    }
    Ok(skipped_elsewhere(incoming, &elsewhere))
}

/// chat-lists.cjs `readTombstones(dir)`: the strings of a JSON array in deleted-chats.json;
/// anything else (or no file) is none.
pub(crate) fn read_tombstones(dir: &Path) -> Result<HashSet<String>, SaveFailure> {
    let mut tombstones = HashSet::new();
    if let Some(JValue::Arr(ids)) =
        store::read_json(&dir.join(TOMBSTONES_FILE)).map_err(|_| SaveFailure::Unsupported)?
    {
        for id in ids {
            if let JValue::Str(s) = id {
                tombstones.insert(s);
            }
        }
    }
    Ok(tombstones)
}

/// chat-lists.cjs `skippedElsewhere(incoming, elsewhere)`: the incoming ids another list holds,
/// once each, in order.
pub(crate) fn skipped_elsewhere(incoming: &[JValue], elsewhere: &HashSet<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut skipped = Vec::new();
    for c in incoming {
        if let Some(id) = c.get("id").as_str() {
            if elsewhere.contains(id) && seen.insert(id.to_string()) {
                skipped.push(id.to_string());
            }
        }
    }
    skipped
}

/// `{ ok: true }`, with `skipped` when there are any.
pub(crate) fn saved_reply(skipped: Vec<String>) -> Reply {
    let mut out = vec![("ok".to_string(), JValue::Bool(true))];
    if !skipped.is_empty() {
        out.push((
            "skipped".to_string(),
            JValue::Arr(skipped.into_iter().map(JValue::Str).collect()),
        ));
    }
    Reply::json(200, wtf16::stringify(&JValue::Obj(out)).unwrap_or_default())
}

/// POST /api/projects/{id}/chats. `body` was read up to [`BODY_LIMIT`]; `over` when it was
/// longer; `admin`: the signed-in user's role is admin (`authn.user.role`).
pub fn post(ctx: Ctx<'_>, raw_project: &str, body: &[u8], over: bool, admin: bool) -> Reply {
    let Some(id) = js::decode_uri_component(raw_project) else {
        return Reply::internal();
    };
    if over {
        return Reply::error(413, "Request exceeds size limit");
    }
    let dir = match store::user_dir(ctx.data_dir, ctx.user_id) {
        Ok(d) => d,
        Err(e) => return store_failure(&e),
    };
    let projects = match store::read(&dir) {
        Ok(p) => p,
        Err(e) => return store_failure(&e),
    };
    match locate(&projects, &id) {
        Err(()) => return Reply::internal(),
        Ok(None) => return Reply::error(404, "no such project"),
        Ok(Some(_)) => {}
    }
    let parsed = incoming(body, admin, ctx.now_ms);
    let saved = store::update(&dir, ctx.switch, |projects| {
        let at = match locate(projects, &id) {
            Err(()) => return (Reply::internal(), false),
            Ok(None) => return (Reply::error(404, "no such project"), false),
            Ok(Some(at)) => at,
        };
        // getProject, before the body is parsed: its save stands whatever follows.
        let Some(Ok(Effects { memory, saved })) = projects.get(at).map(get_project_effects) else {
            return (Reply::internal(), false);
        };
        let healed = saved.is_some();
        if let (Some(v), Some(slot)) = (saved, projects.get_mut(at)) {
            *slot = v;
        }
        let chats = match &parsed {
            Ok(chats) => chats,
            Err(Incoming::Request(reply)) => return (reply.clone(), healed),
            Err(Incoming::Throw) => return (save_failed(CHATS_SAVE_FAILED), healed),
        };
        // On a failure only getProject's save is written, as in Node (it came first).
        match save_chats(&dir, projects, at, memory, chats) {
            Ok(skipped) => (saved_reply(skipped), true),
            Err(SaveFailure::Throw | SaveFailure::Unsupported) => {
                (save_failed(CHATS_SAVE_FAILED), healed)
            }
        }
    });
    match saved {
        Ok(reply) => reply,
        // The lock, the re-read and the write are saveProjects's, inside core's `try`.
        Err(e) => save_failure(&e, CHATS_SAVE_FAILED),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;
    use crate::{free_chats, Body};

    const USER: &str = "11111111-2222-4333-8444-555555555555";
    const PROJECTS: &str = "{\n  \"projects\": [\n    {\n      \"id\": \"proj-a\",\n      \"name\": \"A\",\n      \"chats\": [],\n      \"instructionSkills\": {}\n    }\n  ]\n}";
    const SAVE: &[u8] = br#"{"chats":[{"id":"c1","title":"T","updatedAt":5}]}"#;

    fn workspace() -> (tempfile::TempDir, std::path::PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("users").join(USER);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(store::PROJECTS_FILE), PROJECTS).unwrap();
        (root, dir)
    }

    fn ctx(root: &Path) -> Ctx<'_> {
        Ctx {
            data_dir: root,
            user_id: USER,
            switch: server_store::RustProjects::from_env_value(Some("1")).unwrap(),
            now_ms: 1_700_000_000_000,
        }
    }

    const BUSY: &str =
        r#"{"error":"The project store is busy. Try again shortly.","code":"PROJECTS_BUSY"}"#;

    /// rust-projects.cjs busy() through http.cjs saveFailureReply and json(): 503, its message
    /// and code, and Retry-After: 1 (#1406/#1419).
    fn assert_busy(r: &Reply) {
        assert_eq!(r.status, 503);
        let Body::Bytes { headers, bytes } = &r.body else {
            panic!("a busy save carries Retry-After: {r:?}")
        };
        assert_eq!(
            headers,
            &vec![
                ("content-type", "application/json".to_string()),
                ("cache-control", "no-store".to_string()),
                ("retry-after", "1".to_string()),
            ]
        );
        assert_eq!(std::str::from_utf8(bytes).unwrap(), BUSY);
    }

    #[test]
    fn a_busy_project_store_is_a_503_with_retry_after_and_nothing_changes() {
        let (root, dir) = workspace();
        let _held = crate::lock::acquire(&dir.join(store::PROJECTS_FILE)).unwrap();
        assert_busy(&post(ctx(root.path()), "proj-a", SAVE, false, false));
        assert_eq!(
            std::fs::read_to_string(dir.join(store::PROJECTS_FILE)).unwrap(),
            PROJECTS
        );
    }

    #[test]
    fn a_busy_free_list_is_a_503_with_retry_after_and_nothing_changes() {
        let (root, dir) = workspace();
        let _held = crate::lock::acquire(&dir.join(FREE_CHATS_FILE)).unwrap();
        assert_busy(&free_chats::post(ctx(root.path()), SAVE, false, false));
        assert!(!dir.join(FREE_CHATS_FILE).exists());
    }

    #[test]
    fn save_failures_keep_a_status_or_say_nothing_was_changed() {
        let gone = Reply::error(410, "account no longer exists");
        for fallback in [CHATS_SAVE_FAILED, free_chats::FREE_CHATS_SAVE_FAILED] {
            assert_busy(&save_failure(&StoreError::Busy, fallback));
            assert_eq!(save_failure(&StoreError::Gone, fallback), gone);
            // Anything else never shows its own text and is never a 400.
            for e in [
                StoreError::Io(std::io::Error::other("synthetic disk failure")),
                StoreError::Corrupt,
                StoreError::InvalidUser,
            ] {
                let r = save_failure(&e, fallback);
                assert_eq!(r, Reply::error(500, fallback));
                let Body::Json(text) = &r.body else {
                    panic!("json")
                };
                assert!(!text.contains("synthetic"), "{text}");
            }
        }
        assert_eq!(
            CHATS_SAVE_FAILED,
            "could not save the chats; nothing was changed"
        );
        assert_eq!(
            free_chats::FREE_CHATS_SAVE_FAILED,
            "could not save the free chats; nothing was changed"
        );
    }

    #[test]
    fn only_reading_the_request_is_a_400() {
        let required = Incoming::Request(Reply::error(400, "chats array required"));
        for body in [
            "null",
            "5",
            "0",
            "true",
            "false",
            "\"x\"",
            "\"\"",
            "[]",
            "{}",
            "{\"chats\":null}",
            "{\"chats\":{}}",
            "{\"chats\":\"ab\"}",
        ] {
            assert_eq!(
                incoming_metas(body.as_bytes(), false, 1, "New task"),
                Err(required.clone()),
                "{body}"
            );
        }
        for body in ["", "{", "nul", "{\"chats\":[}"] {
            assert_eq!(
                incoming_metas(body.as_bytes(), false, 1, "New task"),
                Err(Incoming::Request(invalid_json())),
                "{body}"
            );
        }
        // A title String() cannot convert throws inside core's try: the save failure, not a 400.
        assert_eq!(
            incoming_metas(
                br#"{"chats":[{"id":"a","title":{"toString":1}}]}"#,
                false,
                1,
                "New task"
            ),
            Err(Incoming::Throw)
        );
        assert_eq!(
            incoming_metas(br#"{"chats":[]}"#, false, 1, "New task"),
            Ok(Vec::new())
        );
    }

    #[test]
    fn a_free_list_that_is_not_a_list_holds_no_ids_for_a_project_save() {
        for stored in [
            "null",
            "{\"a\":1}",
            "5",
            "\"c1\"",
            "{\"length\":1,\"0\":{\"id\":\"c1\"}}",
        ] {
            let (root, dir) = workspace();
            std::fs::write(dir.join(FREE_CHATS_FILE), stored).unwrap();
            let r = post(ctx(root.path()), "proj-a", SAVE, false, false);
            assert_eq!(r, saved_reply(Vec::new()), "{stored}");
            assert!(std::fs::read_to_string(dir.join(store::PROJECTS_FILE))
                .unwrap()
                .contains("\"c1\""));
            // The free list itself is not rewritten by a project's save.
            assert_eq!(
                std::fs::read_to_string(dir.join(FREE_CHATS_FILE)).unwrap(),
                stored
            );
        }
    }
}
