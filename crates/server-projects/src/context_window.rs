//! A chat's last context meter: core routes/chat-lists.cjs `GET /api/chats/{chatId}/context-window`,
//! with the store reads it makes (chat-context.cjs `read`, chat-lists.cjs `readTombstones` and
//! `safeChatId`), answer for answer. Moved from Node for sbstndalton/noevia#1071; a read only, so it
//! takes no lock and writes nothing.
//!
//! - The chat id from the URL is percent-decoded as `decodeURIComponent`; a malformed escape is 400
//!   `invalid chat id` (core http.cjs `decodePathPart`, #812).
//! - A tombstoned chat (deleted-chats.json holds `safeChatId(id) || id`) answers `{ meter: null }`,
//!   exactly like one that never existed (#554), even when its state file was written back late.
//! - Otherwise `{ meter }` from `context-<sha256(JSON.stringify(id))>.json` in the user's directory,
//!   the decoded id unreduced (as the chat turn names the file), or `{ meter: null }` when the file is
//!   missing, not JSON, or its `meter` is falsy.
//!
//! Fixed against Node: a state file holding JSON `null` answers `{ meter: null }`; Node's
//! `read(dir, id).meter` throws on it and answers 500 (sbstndalton/noevia#2141).
//!
//! Stricter than Node, as a refusal (500): a state file or tombstone list this port cannot hold (a
//! lone surrogate escape, see [`store::read_json`]).

use crate::chats::TOMBSTONES_FILE;
use crate::js;
use crate::store;
use crate::{Ctx, Reply};
use js_json::wtf16;
use js_json::JValue;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// chat-lists.cjs `safeChatId`: the id reduced to `[A-Za-z0-9_-]`.
fn safe_chat_id(id: &str) -> String {
    id.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .collect()
}

/// chat-context.cjs `stateFile(dir, id)`: `context-<sha256 hex of JSON.stringify(String(id))>.json`.
pub fn state_file(dir: &Path, id: &str) -> PathBuf {
    let mut quoted = String::new();
    js_json::quote(id, &mut quoted);
    let digest = Sha256::digest(quoted.as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    dir.join(format!("context-{hex}.json"))
}

fn meter_reply(meter: JValue) -> Reply {
    let out = JValue::obj([("meter", meter)]);
    match wtf16::stringify(&out) {
        Some(text) => Reply::json(200, text),
        None => Reply::internal(),
    }
}

/// GET /api/chats/{chatId}/context-window; `raw_chat` is the path segment as the URL spelled it.
pub fn get(ctx: Ctx<'_>, raw_chat: &str) -> Reply {
    let Some(id) = js::decode_uri_component(raw_chat) else {
        return Reply::error(400, "invalid chat id");
    };
    let Ok(dir) = store::user_dir(ctx.data_dir, ctx.user_id) else {
        return Reply::internal();
    };
    let tombstones = match store::read_json(&dir.join(TOMBSTONES_FILE)) {
        Ok(v) => v,
        Err(_) => return Reply::internal(),
    };
    let safe = safe_chat_id(&id);
    let key = if safe.is_empty() {
        id.as_str()
    } else {
        safe.as_str()
    };
    if let Some(JValue::Arr(ids)) = &tombstones {
        if ids.iter().any(|t| t.as_str() == Some(key)) {
            return meter_reply(JValue::Null);
        }
    }
    let state = match store::read_json(&state_file(&dir, &id)) {
        Ok(v) => v,
        Err(_) => return Reply::internal(),
    };
    let meter = match state {
        Some(JValue::Obj(_)) => {
            let m = state
                .as_ref()
                .map(|s| s.get("meter").clone())
                .unwrap_or(JValue::Null);
            if m.truthy() {
                m
            } else {
                JValue::Null
            }
        }
        _ => JValue::Null,
    };
    meter_reply(meter)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    const USER: &str = "11111111-2222-4333-8444-555555555555";

    fn setup() -> (tempfile::TempDir, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let dir = store::user_dir(root.path(), USER).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        (root, dir)
    }

    fn call(root: &Path, raw: &str) -> Reply {
        get(
            Ctx {
                data_dir: root,
                user_id: USER,
                switch: server_store::RustProjects::owned(),
                now_ms: 0,
            },
            raw,
        )
    }

    fn null_meter() -> Reply {
        Reply::json(200, "{\"meter\":null}".into())
    }

    #[test]
    fn state_file_is_chat_contexts_name() {
        // Node: crypto.createHash('sha256').update(JSON.stringify(id)).digest('hex').
        for (id, hex) in [
            (
                "c1",
                "b32a2bd4df0aed7e435a79c9ebbaa1a15f4d63c50208cd5b7b5ad8dbddd667ad",
            ),
            (
                "a\"b\u{1}\u{e9}",
                "232b3f3ed67d731db14870081a5c9acbd418652a657c33c797f869d8c3d952ae",
            ),
        ] {
            assert_eq!(
                state_file(Path::new("/d"), id),
                PathBuf::from(format!("/d/context-{hex}.json")),
                "{id}"
            );
        }
    }

    #[test]
    fn nothing_recorded_is_null() {
        let (root, _dir) = setup();
        assert_eq!(call(root.path(), "c1"), null_meter());
    }

    #[test]
    fn recorded_meter_is_served_as_stored() {
        let (root, dir) = setup();
        std::fs::write(
            state_file(&dir, "c 1"),
            "{\"meter\":{\"model\":\"qa-synthetic\",\"used\":5,\"parts\":[{\"name\":\"x\",\"tokens\":1.5}]},\"other\":1}",
        )
        .unwrap();
        assert_eq!(
            call(root.path(), "c%201"),
            Reply::json(
                200,
                "{\"meter\":{\"model\":\"qa-synthetic\",\"used\":5,\"parts\":[{\"name\":\"x\",\"tokens\":1.5}]}}"
                    .into()
            )
        );
    }

    #[test]
    fn tombstoned_chat_reads_null_even_with_late_state() {
        let (root, dir) = setup();
        std::fs::write(state_file(&dir, "c-gone"), "{\"meter\":{\"used\":5}}").unwrap();
        assert_eq!(
            call(root.path(), "c-gone"),
            Reply::json(200, "{\"meter\":{\"used\":5}}".into())
        );
        std::fs::write(dir.join(TOMBSTONES_FILE), "[\"c-gone\"]").unwrap();
        assert_eq!(call(root.path(), "c-gone"), null_meter());
    }

    #[test]
    fn tombstone_matches_the_safe_id_or_the_raw_one() {
        let (root, dir) = setup();
        std::fs::write(state_file(&dir, "a b"), "{\"meter\":{\"used\":1}}").unwrap();
        std::fs::write(state_file(&dir, "%%"), "{\"meter\":{\"used\":2}}").unwrap();
        std::fs::write(dir.join(TOMBSTONES_FILE), "[\"ab\",\"%%\",7]").unwrap();
        assert_eq!(call(root.path(), "a%20b"), null_meter());
        // safeChatId('%%') is '', so the raw id is the key.
        assert_eq!(call(root.path(), "%25%25"), null_meter());
    }

    #[test]
    fn falsy_or_odd_state_is_null() {
        let (root, dir) = setup();
        for body in [
            "not json",
            "{\"meter\":0}",
            "{\"meter\":\"\"}",
            "{\"meter\":false}",
            "{}",
            "[1]",
            "\"meter\"",
            "42",
            // Node throws here (500); the port answers as for a missing meter.
            "null",
        ] {
            std::fs::write(state_file(&dir, "c1"), body).unwrap();
            assert_eq!(call(root.path(), "c1"), null_meter(), "{body}");
        }
        std::fs::write(state_file(&dir, "c1"), "{\"meter\":1}").unwrap();
        assert_eq!(
            call(root.path(), "c1"),
            Reply::json(200, "{\"meter\":1}".into())
        );
    }

    #[test]
    fn malformed_escape_is_400() {
        let (root, _dir) = setup();
        assert_eq!(
            call(root.path(), "%E0%A4"),
            Reply::error(400, "invalid chat id")
        );
    }

    #[test]
    fn another_users_state_is_never_read() {
        let (root, _dir) = setup();
        let other = store::user_dir(root.path(), "99999999-2222-4333-8444-555555555555").unwrap();
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(state_file(&other, "c1"), "{\"meter\":{\"used\":9}}").unwrap();
        assert_eq!(call(root.path(), "c1"), null_meter());
    }
}
