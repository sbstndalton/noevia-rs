//! Which chat `GET /api/chats/{chatId}/export` downloads (#1471): core routes/chat-lists.cjs's lookup,
//! answer for answer, over the user's files on disk. The Markdown and the file name are
//! crates/server-export `single`'s; the route is bins/noevia-server project_routes.rs.
//!
//! - The id from the URL is `decodeURIComponent`-decoded (a malformed escape is 400 `invalid chat id`,
//!   core http.cjs `decodePathPart`, #812), then reduced to `[A-Za-z0-9_-]` (chat-lists.cjs
//!   `safeChatId`); one that reduces to nothing is the same 400. Only the reduced id is looked up.
//! - The lists are the workspace load's ([`free_chats::workspace_projects`], which is Node's 500 for a
//!   project entry the load throws on, and free-chats.json as [`free_chats::as_list`]): the first free
//!   chat whose `id` is the reduced id; else, unless the id is tombstoned, the first in the projects'
//!   `chats` in list order, Diary-internal projects skipped (`diaryExtras.internalProject`, which throws
//!   on a project id that is not a string). A `chats` that is missing or falsy is `[]`; any other value
//!   that is not a list throws in `.find` (Node's 500).
//! - No chat, or a tombstoned id (`deleted-chats.json`, an unreadable or non-list file being no
//!   tombstones): 404 `This chat was not found.`
//! - The transcript is projects.cjs `readHistory(id)`: `.history || []`, every read failure `[]`.
//!
//! Stricter than Node, as a refusal (500): a list, tombstone file or transcript this port cannot hold
//! (a character the lossless reader refuses, see [`store::read_json`]).
use crate::transcript::{self, TranscriptError};
use crate::{chats, free_chats, store, Ctx, Reply};
use js_json::JValue;

const DIARY_EXTRAS: &str = "cowork-diary-extras";
const CHAT_CONTEXT_PREFIX: &str = "cowork-chat-context-";

/// What the lookup found.
#[derive(Debug)]
pub enum Lookup {
    /// The chat's meta as the list holds it, its reduced id and its transcript.
    Found {
        chat: JValue,
        id: String,
        history: JValue,
    },
    /// The answer to send instead (400, 404 or 500).
    Answer(Reply),
}

fn not_found() -> Lookup {
    Lookup::Answer(Reply::error(404, "This chat was not found."))
}

/// `c && c.id === id`.
fn is(chat: &JValue, id: &str) -> bool {
    chat.truthy() && chat.get("id").as_str() == Some(id)
}

/// The chat the URL segment `raw` names in the user's own lists.
pub fn find(ctx: Ctx<'_>, raw: &str) -> Lookup {
    let Some(decoded) = crate::js::decode_uri_component(raw) else {
        return Lookup::Answer(Reply::error(400, "invalid chat id"));
    };
    let id = crate::history::safe_chat_id(&decoded);
    if id.is_empty() {
        return Lookup::Answer(Reply::error(400, "invalid chat id"));
    }
    let dir = match store::user_dir(ctx.data_dir, ctx.user_id) {
        Ok(d) => d,
        Err(_) => return Lookup::Answer(Reply::internal()),
    };
    // The workspace load, before any route runs.
    let projects = match free_chats::workspace_projects(&dir) {
        Ok(p) => p,
        Err(reply) => return Lookup::Answer(reply),
    };
    let free = match store::read_json(&dir.join(chats::FREE_CHATS_FILE)) {
        Ok(v) => free_chats::as_list(v),
        Err(_) => return Lookup::Answer(Reply::internal()),
    };
    let deleted = match chats::read_tombstones(&dir) {
        Ok(d) => d.contains(&id),
        Err(_) => return Lookup::Answer(Reply::internal()),
    };
    let mut chat = free.into_iter().find(|c| is(c, &id));
    if chat.is_none() && !deleted {
        for project in projects {
            let Some(pid) = project.get("id").as_str() else {
                return Lookup::Answer(Reply::internal());
            };
            if pid == DIARY_EXTRAS || pid.starts_with(CHAT_CONTEXT_PREFIX) {
                continue;
            }
            let hit = match project.get("chats") {
                JValue::Arr(list) => list.iter().find(|c| is(c, &id)).cloned(),
                v if !v.truthy() => None,
                _ => return Lookup::Answer(Reply::internal()),
            };
            if hit.is_some() {
                chat = hit;
                break;
            }
        }
    }
    let Some(chat) = chat else {
        return not_found();
    };
    if deleted {
        return not_found();
    }
    let history = match transcript::read_listed(ctx.data_dir, ctx.user_id, &id) {
        Ok(current) => current.history,
        // The user's directory is gone (removed meanwhile): projects.cjs reads `[]`.
        Err(TranscriptError::Store(store::StoreError::Gone)) => JValue::Arr(Vec::new()),
        Err(_) => return Lookup::Answer(Reply::internal()),
    };
    Lookup::Found { chat, id, history }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    const USER: &str = "11111111-2222-4333-8444-555555555555";

    fn ctx(root: &std::path::Path) -> Ctx<'_> {
        Ctx {
            data_dir: root,
            user_id: USER,
            switch: server_store::RustProjects::owned(),
            now_ms: 0,
        }
    }

    fn put(root: &std::path::Path, name: &str, text: &str) {
        let dir = root.join("users").join(USER);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(name), text).unwrap();
    }

    fn status(l: &Lookup) -> u16 {
        match l {
            Lookup::Found { .. } => 200,
            Lookup::Answer(r) => r.status,
        }
    }

    #[test]
    fn free_first_then_projects_never_internal_or_tombstoned() {
        let root = tempfile::tempdir().unwrap();
        let r = root.path();
        put(
            r,
            "free-chats.json",
            r#"[null,"f",{"id":"f1","title":"Free"}]"#,
        );
        put(
            r,
            "projects.json",
            r#"{"projects":[{"id":"cowork-diary-extras","chats":[{"id":"x1"}]},{"id":"p","chats":[{"id":"p1"},{"id":"f1","title":"Shadow"}]},{"id":"q","chats":"ab"}]}"#,
        );
        put(r, "deleted-chats.json", r#"["gone"]"#);
        put(
            r,
            "history-f1.json",
            r#"{"history":[{"role":"user","content":"hi"}]}"#,
        );
        match find(ctx(r), "f1") {
            Lookup::Found { chat, id, history } => {
                assert_eq!(chat.get("title").as_str(), Some("Free"));
                assert_eq!(id, "f1");
                assert!(matches!(history, JValue::Arr(ref v) if v.len() == 1));
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(status(&find(ctx(r), "p1")), 200);
        // `.find` over a string throws, once the walk reaches it.
        assert_eq!(status(&find(ctx(r), "x1")), 500);
        assert_eq!(status(&find(ctx(r), "gone")), 404);
        assert_eq!(status(&find(ctx(r), "%E0%A4%A")), 400);
        assert_eq!(status(&find(ctx(r), "%20")), 400);
        // `c%201` is `c 1`, reduced to `c1`.
        assert_eq!(status(&find(ctx(r), "f%201")), 200);
    }
}
