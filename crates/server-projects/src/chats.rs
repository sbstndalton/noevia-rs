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

fn store_failure(e: &StoreError) -> Reply {
    crate::assets::store_failure(e)
}

fn invalid_json() -> Reply {
    Reply::error(400, "invalid JSON")
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
fn locate(projects: &[JValue], id: &str) -> Result<Option<usize>, ()> {
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
struct Effects {
    /// The project as Node holds it afterwards (reconcile always assigns `instructionSkills`).
    memory: JValue,
    /// What Node saved, if it saved: after the heal, then again after reconcile when that changed
    /// something. (An unchanged reconcile is not saved, though Node keeps its assignment.)
    saved: Option<JValue>,
}

fn get_project_effects(project: &JValue) -> Result<Effects, skills::Unsupported> {
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

/// The POST body's metas as routes/projects.cjs builds them; `Err` is the reply (a 400).
fn incoming(body: &[u8], admin: bool, now_ms: i64) -> Result<Vec<JValue>, Reply> {
    let body = wtf16::parse(&String::from_utf8_lossy(body)).map_err(|_| invalid_json())?;
    if body.is_nullish() {
        return Err(invalid_json()); // body.chats on null: a TypeError, caught as invalid JSON
    }
    let JValue::Arr(chats) = body.get("chats") else {
        return Err(Reply::error(400, "chats array required"));
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
        let text = |key: &str, fallback: &str, cap: usize| -> Result<String, Reply> {
            let v = c.get(key);
            let s = if v.truthy() {
                js_json::to_js_string(v).map_err(|_| invalid_json())?
            } else {
                fallback.to_string()
            };
            Ok(wtf16::slice(&s, cap))
        };
        let title = text("title", "New task", 120)?;
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

/// Why the part of the POST inside Node's `try` did not complete.
enum SaveFailure {
    /// Node throws there: its catch answers 400 invalid JSON.
    Throw,
    /// Data Node never writes and this port does not reproduce: 500.
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
fn merge_chats(
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
fn list_ids(list: &JValue, into: &mut HashSet<String>) -> Result<(), ()> {
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
    match free {
        // readJson's fallback, [].
        None => {}
        // Node iterates its cached copy: null, a number or an object throws.
        Some(v @ (JValue::Arr(_) | JValue::Str(_))) => {
            list_ids(&v, &mut elsewhere).map_err(|()| SaveFailure::Throw)?
        }
        Some(_) => return Err(SaveFailure::Throw),
    }
    for (i, p) in projects.iter().enumerate() {
        let p = if i == at { &memory } else { p };
        if p.truthy() && p.get("id").as_str() != Some(list_id.as_str()) {
            list_ids(p.get("chats"), &mut elsewhere).map_err(|()| SaveFailure::Throw)?;
        }
    }
    // readTombstones: the strings of a JSON array; anything else (or no file) is none.
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
    // skippedElsewhere(chats, elsewhere).
    let mut seen = HashSet::new();
    let mut skipped = Vec::new();
    for c in incoming {
        if let Some(id) = c.get("id").as_str() {
            if elsewhere.contains(id) && seen.insert(id.to_string()) {
                skipped.push(id.to_string());
            }
        }
    }
    Ok(skipped)
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
            Err(reply) => return (reply.clone(), healed),
        };
        // On a failure only getProject's save is written, as in Node (it came first).
        match save_chats(&dir, projects, at, memory, chats) {
            Ok(skipped) => {
                let mut out = vec![("ok".to_string(), JValue::Bool(true))];
                if !skipped.is_empty() {
                    out.push((
                        "skipped".to_string(),
                        JValue::Arr(skipped.into_iter().map(JValue::Str).collect()),
                    ));
                }
                let text = wtf16::stringify(&JValue::Obj(out)).unwrap_or_default();
                (Reply::json(200, text), true)
            }
            Err(SaveFailure::Throw) => (invalid_json(), healed),
            Err(SaveFailure::Unsupported) => (Reply::internal(), healed),
        }
    });
    match saved {
        Ok(reply) => reply,
        Err(e) => store_failure(&e),
    }
}
