//! A free chat's attachments project: core routes/projects.cjs `GET` and `POST
//! /api/chats/{chatId}/context`, with the store calls they make (projects.cjs `getProject`,
//! `saveProjects`; diary-extras.cjs `chatProjectId`, `newProject`; chat-lists.cjs
//! `readTombstones`), answer for answer. The first project-writes slice of sbstndalton/noevia#1264;
//! the front answers it only in a build with the cargo feature `rust-project-writes`, under the
//! switch `NOEVIA_RUST_PROJECT_WRITES` (off by default, separate from `NOEVIA_RUST_PROJECTS`).
//!
//! - The chat id from the URL is percent-decoded, then must match `^[a-zA-Z0-9_-]{1,100}$`; the
//!   project is `cowork-chat-context-<chat id>`. Anything else is 400 `Invalid chat identifier`.
//! - GET: `{ project }`, the project as `getProject` leaves it, or `{ project: null }`.
//!   `getProject`'s own writes come first and are saved, as Node saves them: a context project
//!   still on Diary's `routing: 'manual'` template without `routingChosen` is healed to `auto`
//!   (#352), and its instruction skills are reconciled.
//! - POST: the same, and when the project does not exist it is created (after `getProject`, so a
//!   project that appeared meanwhile is answered as it is): a tombstoned chat id (deleted-chats.json)
//!   is 410 `This chat was deleted.` (#788); otherwise Diary's template with this id, the name
//!   `Chat attachments <the id as the URL spelled it>`, no instructions, the `core` toolbox, Auto
//!   routing and the Automatic tools mode (#1522), appended to the list; then every project's
//!   instruction skills are reconciled (`saveProjects`) and the file written.
//! - Node's workspace load answers 500 for a projects.json holding a `null` or primitive project
//!   (its `provider` loop throws), so does this port.
//!
//! Every change is made under the projects.json lock shared with Node, on the file as it is then
//! ([`store::update`]); the project is looked up again there, so two concurrent creates make one,
//! and the tombstones are read there too (#1906).
//!
//! Fixed against Node (sbstndalton/noevia#1902): a malformed percent escape in the chat id is 400
//! `Invalid chat identifier`, not Node's 500 (its `decodeURIComponent` throws there).
//!
//! Stricter than Node, as a refusal (500, nothing written): a projects.json that is not JSON (Node's
//! uncoordinated load reads it as an empty list and its next save overwrites it), and what
//! [`skills`] refuses.

use crate::chats::{self, Effects};
use crate::free_chats;
use crate::js;
use crate::skills;
use crate::store;
use crate::{Ctx, Reply};
use js_json::wtf16;
use js_json::JValue;

/// diary-extras.cjs `chatProjectId`'s prefix.
pub const CONTEXT_PREFIX: &str = "cowork-chat-context-";
/// diary-extras.cjs `chatProjectId`: at most this many id characters.
const CHAT_ID_MAX: usize = 100;
/// toolboxes.cjs `DEFAULT_TOOLBOXES`.
const DEFAULT_TOOLBOXES: &[&str] = &["core"];

fn invalid() -> Reply {
    Reply::error(400, "Invalid chat identifier")
}

/// diary-extras.cjs `chatProjectId(chatId)`, for the decoded id.
fn chat_project_id(chat: &str) -> Option<String> {
    let ok = !chat.is_empty()
        && chat.chars().count() <= CHAT_ID_MAX
        && chat
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    ok.then(|| format!("{CONTEXT_PREFIX}{chat}"))
}

fn project_reply(project: JValue) -> Reply {
    let out = JValue::obj([("project", project)]);
    Reply::json(200, wtf16::stringify(&out).unwrap_or_default())
}

/// The project Node creates: `{ ...diaryExtras.newProject(), id, name, instructions: '', toolboxes:
/// [...DEFAULT_TOOLBOXES], routing: 'auto', toolsMode: 'auto' }`, keys in that object's order.
fn new_project(id: &str, raw_chat: &str, now_ms: i64) -> JValue {
    let now = JValue::Num(now_ms as f64);
    JValue::obj([
        ("id", JValue::Str(id.to_string())),
        ("name", JValue::Str(format!("Chat attachments {raw_chat}"))),
        ("goal", JValue::Str(String::new())),
        ("instructions", JValue::Str(String::new())),
        ("files", JValue::Arr(Vec::new())),
        ("assets", JValue::Arr(Vec::new())),
        ("memories", JValue::Arr(Vec::new())),
        ("chats", JValue::Arr(Vec::new())),
        ("sourceFolders", JValue::Arr(Vec::new())),
        (
            "toolboxes",
            JValue::Arr(
                DEFAULT_TOOLBOXES
                    .iter()
                    .map(|t| JValue::Str((*t).to_string()))
                    .collect(),
            ),
        ),
        ("routing", JValue::Str("auto".into())),
        ("createdAt", now.clone()),
        ("updatedAt", now),
        ("toolsMode", JValue::Str("auto".into())),
    ])
}

/// What workspace.cjs's load refuses: any project that is not an object or array.
fn loadable(projects: &[JValue]) -> bool {
    projects
        .iter()
        .all(|p| matches!(p, JValue::Obj(_) | JValue::Arr(_)))
}

/// What one request does under the lock, on the projects as they are now.
enum Outcome {
    Project(JValue),
    Missing,
    Deleted,
    Fail,
}

/// `getProject(id)`'s effects on `projects[at]`: the project to answer and whether to write.
fn heal(projects: &mut [JValue], at: usize) -> (Outcome, bool) {
    let Some(slot) = projects.get_mut(at) else {
        return (Outcome::Fail, false);
    };
    match chats::get_project_effects(slot) {
        Err(_) => (Outcome::Fail, false),
        Ok(Effects { memory, saved }) => {
            let changed = saved.is_some();
            if let Some(v) = saved {
                *slot = v;
            }
            (Outcome::Project(memory), changed)
        }
    }
}

/// `PROJECTS.push(project); saveProjects(PROJECTS)`: every project reconciled (planned for all
/// before any is changed, so a refusal writes nothing), the new one included. Returns the project
/// as Node answers it.
fn create(projects: &mut Vec<JValue>, project: JValue) -> Option<JValue> {
    projects.push(project);
    let mut plans = Vec::with_capacity(projects.len());
    for p in projects.iter() {
        plans.push(skills::plan(p).ok()?);
    }
    for (p, plan) in projects.iter_mut().zip(plans) {
        if let skills::Plan::Set { value, .. } = plan {
            store::set(p, "instructionSkills", value);
        }
    }
    projects.last().cloned()
}

fn serve(ctx: Ctx<'_>, raw_chat: &str, post: bool) -> Reply {
    // #1902: a malformed escape is the route's 400, not Node's URIError 500.
    let Some(chat) = js::decode_uri_component(raw_chat) else {
        return invalid();
    };
    let Some(id) = chat_project_id(&chat) else {
        return invalid();
    };
    let dir = match store::user_dir(ctx.data_dir, ctx.user_id) {
        Ok(d) => d,
        Err(e) => return chats::store_failure(&e),
    };
    // The request's workspace load, then getProject on it, all without the lock: only a write
    // takes it, and it looks again there.
    let projects = match free_chats::workspace_projects(&dir) {
        Ok(p) => p,
        Err(reply) => return reply,
    };
    let at = match chats::locate(&projects, &id) {
        Err(()) => return Reply::internal(),
        Ok(at) => at,
    };
    match at.and_then(|i| projects.get(i)) {
        Some(p) => match chats::get_project_effects(p) {
            Err(_) => return Reply::internal(),
            Ok(Effects {
                memory,
                saved: None,
            }) => return project_reply(memory),
            Ok(_) => {}
        },
        None if !post => return project_reply(JValue::Null),
        None => {}
    }
    let done = store::update(&dir, ctx.switch, |projects| {
        if !loadable(projects) {
            return (Outcome::Fail, false);
        }
        match chats::locate(projects, &id) {
            Err(()) => (Outcome::Fail, false),
            Ok(Some(at)) => heal(projects, at),
            Ok(None) if !post => (Outcome::Missing, false),
            // #1906: the tombstones are read under the lock, on the create path only, so a chat
            // deleted while this request waited for it is not given its attachments back.
            Ok(None) => match chats::read_tombstones(&dir) {
                Err(_) => (Outcome::Fail, false),
                Ok(t) if t.contains(&chat) => (Outcome::Deleted, false),
                Ok(_) => match create(projects, new_project(&id, raw_chat, ctx.now_ms)) {
                    Some(project) => (Outcome::Project(project), true),
                    None => (Outcome::Fail, false),
                },
            },
        }
    });
    match done {
        Ok(Outcome::Project(p)) => project_reply(p),
        Ok(Outcome::Missing) => project_reply(JValue::Null),
        Ok(Outcome::Deleted) => Reply::error(410, "This chat was deleted."),
        Ok(Outcome::Fail) => Reply::internal(),
        Err(e) => chats::store_failure(&e),
    }
}

/// GET /api/chats/{chatId}/context; `raw_chat` is the path segment as the URL spelled it.
pub fn get(ctx: Ctx<'_>, raw_chat: &str) -> Reply {
    serve(ctx, raw_chat, false)
}

/// POST /api/chats/{chatId}/context. Node reads no body.
pub fn post(ctx: Ctx<'_>, raw_chat: &str) -> Reply {
    serve(ctx, raw_chat, true)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn chat_ids_as_diary_extras_accepts_them() {
        assert_eq!(
            chat_project_id("a-b_C9").unwrap(),
            "cowork-chat-context-a-b_C9"
        );
        assert_eq!(chat_project_id(&"x".repeat(100)).unwrap().len(), 120);
        for bad in ["", "a.b", "a b", "a/b", "é", "a\u{0}", &"x".repeat(101)] {
            assert_eq!(chat_project_id(bad), None, "{bad:?}");
        }
    }

    /// sbstndalton/noevia#1906: a chat deleted after the request started but before it took the
    /// lock (Node tombstones it, then removes the attachments project) is not given one back.
    #[test]
    fn a_tombstone_written_before_the_lock_is_seen_by_the_create() {
        let root = tempfile::tempdir().unwrap();
        let user = "11111111-2222-4333-8444-555555555555";
        let dir = root.path().join("users").join(user);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(store::PROJECTS_FILE), "{\n  \"projects\": []\n}").unwrap();
        let ctx = Ctx {
            data_dir: root.path(),
            user_id: user,
            switch: server_store::RustProjects::from_env_value(Some("1")).unwrap(),
            now_ms: 5,
        };
        // Hold the lock so the request has done its unlocked reads and waits for it.
        let held = crate::lock::acquire(&dir.join(store::PROJECTS_FILE)).unwrap();
        let reply = std::thread::scope(|s| {
            let request = s.spawn(|| post(ctx, "gone"));
            std::thread::sleep(std::time::Duration::from_millis(300));
            std::fs::write(dir.join(chats::TOMBSTONES_FILE), "[\"gone\"]").unwrap();
            drop(held);
            request.join().unwrap()
        });
        assert_eq!(reply, Reply::error(410, "This chat was deleted."));
        assert!(store::read(&dir).unwrap().is_empty(), "nothing created");
    }

    #[test]
    fn concurrent_creates_make_one_project() {
        let root = tempfile::tempdir().unwrap();
        let user = "11111111-2222-4333-8444-555555555555";
        let dir = root.path().join("users").join(user);
        std::fs::create_dir_all(&dir).unwrap();
        let ctx = || Ctx {
            data_dir: root.path(),
            user_id: user,
            switch: server_store::RustProjects::from_env_value(Some("1")).unwrap(),
            now_ms: 5,
        };
        std::thread::scope(|s| {
            for _ in 0..8 {
                s.spawn(|| assert_eq!(post(ctx(), "c1").status, 200));
            }
        });
        let projects = store::read(&dir).unwrap();
        assert_eq!(projects.len(), 1);
        assert_eq!(
            projects[0].get("id").as_str(),
            Some("cowork-chat-context-c1")
        );
    }
}
