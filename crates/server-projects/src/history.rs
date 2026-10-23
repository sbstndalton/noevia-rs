//! A chat's transcript as the client loads it: core routes/chat-lists.cjs `GET
//! /api/chats/{chatId}/history`, with the store reads it makes (projects.cjs `readHistory`,
//! chat-partial.cjs `partialFor`), answer for answer. Moved from Node for sbstndalton/noevia#1071
//! (slice 2); a read only, so it takes no lock and writes nothing. `POST` (the save, with its
//! compare-and-swap on `revision`) stays Node's for now.
//!
//! - The chat id from the URL is percent-decoded as `decodeURIComponent` (a malformed escape is
//!   400 `invalid chat id`, core http.cjs `decodePathPart`, #812), then reduced to `[A-Za-z0-9_-]`
//!   (chat-lists.cjs `safeChatId`); an id that reduces to nothing is 400 `invalid chat id` too.
//! - `history` is `JSON.parse(history-<id>.json).history || []` from the user's directory: `[]`
//!   when the file is missing or not JSON, or its `history` is missing or falsy; any other value is
//!   served as stored (Node does not check it is an array).
//! - `revision` is the sha256 hex of `JSON.stringify(history)`, byte for byte, because Node's
//!   `POST` compares a save's `baseRevision` with the revision it computes the same way.
//! - `partial` (#1507) is the chat's saved partial reply (`chat-partials/<sha256 of id>.json`)
//!   without its internal `running` marker, present only when it is a plain object and the reply
//!   that wrote it is not still running ([`partial::state`]).
//!
//! Fixed against Node:
//! - Node hid a partial while a reply ran for the same chat id in its own memory, a set shared by
//!   every account and cleared by the first of two concurrent replies to end
//!   (sbstndalton/noevia#2144). Here the partial file itself says which start wrote it, and the file
//!   is per account, so another account's reply never hides it and a second running reply keeps it
//!   hidden.
//! - A deleted (tombstoned) chat answers exactly like one that never existed, `{ history: [] }`
//!   with no `partial`, as the context meter does (#554); Node served whatever a late writer or a
//!   delete that could not remove the files had left (sbstndalton/noevia#2147).
//!
//! Stricter than Node, as a refusal (500): a transcript or tombstone list this port cannot hold (a
//! character the lossless reader refuses, see [`store::read_json`]). Such a partial is left out
//! instead, as if still running, so the transcript itself stays readable.

use crate::chats::TOMBSTONES_FILE;
use crate::js;
use crate::partial::{self, State};
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

/// workspace.cjs `historyPath(id)` for an id already reduced by [`safe_chat_id`].
pub fn history_file(dir: &Path, safe_id: &str) -> PathBuf {
    dir.join(format!("history-{safe_id}.json"))
}

/// The revision Node's history routes answer: sha256 hex of `JSON.stringify(history)`.
pub fn revision_of(history: &JValue) -> Option<String> {
    let text = wtf16::stringify(history)?;
    Some(
        Sha256::digest(text.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect(),
    )
}

/// projects.cjs `readHistory`: `.history || []` of the parsed file, `[]` on any failure.
fn read_history(dir: &Path, safe_id: &str) -> Result<JValue, store::StoreError> {
    let stored = match store::read_json(&history_file(dir, safe_id))? {
        Some(v @ JValue::Obj(_)) => v.get("history").clone(),
        // `null.history` throws (caught: []); a primitive or an array has no `history`.
        _ => JValue::Undefined,
    };
    Ok(if stored.truthy() {
        stored
    } else {
        JValue::Arr(Vec::new())
    })
}

/// chat-partial.cjs `partialFor`, with the running check made from the file's marker.
fn offered_partial(dir: &Path, safe_id: &str, start_id: &str) -> Option<JValue> {
    let saved = store::read_json(&partial::file_for(dir, safe_id)).ok()??;
    match partial::state(&saved, start_id) {
        State::Running => None,
        State::Interrupted => partial::offered(saved),
    }
}

fn answer(history: JValue, partial: Option<JValue>) -> Reply {
    let Some(revision) = revision_of(&history) else {
        return Reply::internal();
    };
    let mut items = vec![
        ("history".to_string(), history),
        ("revision".to_string(), JValue::Str(revision)),
    ];
    if let Some(p) = partial {
        items.push(("partial".to_string(), p));
    }
    match wtf16::stringify(&JValue::Obj(items)) {
        Some(text) => Reply::json(200, text),
        None => Reply::internal(),
    }
}

/// GET /api/chats/{chatId}/history; `raw_chat` is the path segment as the URL spelled it and
/// `start_id` this container start's NOEVIA_START_ID (empty when the front runs without Node, so
/// no reply can be running and every saved partial is offered).
pub fn get(ctx: Ctx<'_>, raw_chat: &str, start_id: &str) -> Reply {
    let Some(id) = js::decode_uri_component(raw_chat) else {
        return Reply::error(400, "invalid chat id");
    };
    let safe = safe_chat_id(&id);
    if safe.is_empty() {
        return Reply::error(400, "invalid chat id");
    }
    let Ok(dir) = store::user_dir(ctx.data_dir, ctx.user_id) else {
        return Reply::internal();
    };
    let tombstones = match store::read_json(&dir.join(TOMBSTONES_FILE)) {
        Ok(v) => v,
        Err(_) => return Reply::internal(),
    };
    if let Some(JValue::Arr(ids)) = &tombstones {
        if ids.iter().any(|t| t.as_str() == Some(safe.as_str())) {
            return answer(JValue::Arr(Vec::new()), None);
        }
    }
    let history = match read_history(&dir, &safe) {
        Ok(h) => h,
        Err(_) => return Reply::internal(),
    };
    answer(history, offered_partial(&dir, &safe, start_id))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    const USER: &str = "11111111-2222-4333-8444-555555555555";
    const OTHER: &str = "99999999-2222-4333-8444-555555555555";
    const NOW: &str = "0123456789abcdef0123456789abcdef";
    const BEFORE: &str = "ffffffffffffffffffffffffffffffff";
    /// Node: crypto.createHash('sha256').update(JSON.stringify([])).digest('hex').
    const EMPTY_REVISION: &str = "4f53cda18c2baa0c0354bb5f9a3ecbe5ed12ab4d8e11ba873c2f11161202b945";

    fn setup() -> (tempfile::TempDir, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let dir = store::user_dir(root.path(), USER).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        (root, dir)
    }

    fn call_as(root: &Path, user: &str, raw: &str, start: &str) -> Reply {
        get(
            Ctx {
                data_dir: root,
                user_id: user,
                switch: server_store::RustProjects::owned(),
                now_ms: 0,
            },
            raw,
            start,
        )
    }

    fn call(root: &Path, raw: &str) -> Reply {
        call_as(root, USER, raw, NOW)
    }

    fn empty() -> Reply {
        Reply::json(
            200,
            format!("{{\"history\":[],\"revision\":\"{EMPTY_REVISION}\"}}"),
        )
    }

    fn write_partial(dir: &Path, id: &str, body: &str) {
        let file = partial::file_for(dir, id);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, body).unwrap();
    }

    #[test]
    fn revisions_are_nodes_sha256_of_json_stringify() {
        // Node: crypto.createHash('sha256').update(JSON.stringify(v)).digest('hex').
        for (text, hex) in [
            ("[]", EMPTY_REVISION),
            (
                r#"[{"role":"user","content":"qa é \ud83d"},{"n":1e21,"m":0.1}]"#,
                "97472c31022b4ae4d2e38582a54b08b69837acb4efd4d57d0458dc2a54c26921",
            ),
        ] {
            let v = wtf16::parse(text).unwrap();
            assert_eq!(revision_of(&v).unwrap(), hex, "{text}");
        }
    }

    #[test]
    fn nothing_saved_is_an_empty_history() {
        let (root, _dir) = setup();
        assert_eq!(call(root.path(), "c1"), empty());
    }

    #[test]
    fn the_saved_transcript_is_served_as_stored_with_its_revision() {
        let (root, dir) = setup();
        let history = r#"[{"role":"user","content":"qa hello"},{"role":"assistant","content":"qa hi","sources":[{"1":2,"b":1,"0":3}]}]"#;
        std::fs::write(
            history_file(&dir, "c-1"),
            format!("{{\n  \"history\": {history},\n  \"x\": 1\n}}"),
        )
        .unwrap();
        // JS key order: array-index keys first.
        let served = r#"[{"role":"user","content":"qa hello"},{"role":"assistant","content":"qa hi","sources":[{"0":3,"1":2,"b":1}]}]"#;
        let revision = revision_of(&wtf16::parse(served).unwrap()).unwrap();
        assert_eq!(
            call(root.path(), "c-1"),
            Reply::json(
                200,
                format!("{{\"history\":{served},\"revision\":\"{revision}\"}}")
            )
        );
    }

    #[test]
    fn the_url_id_is_decoded_then_reduced_like_safe_chat_id() {
        let (root, dir) = setup();
        std::fs::write(history_file(&dir, "ab"), r#"{"history":[1]}"#).unwrap();
        let revision = revision_of(&wtf16::parse("[1]").unwrap()).unwrap();
        let want = Reply::json(
            200,
            format!("{{\"history\":[1],\"revision\":\"{revision}\"}}"),
        );
        assert_eq!(call(root.path(), "a%20b"), want);
        assert_eq!(call(root.path(), "a.b"), want);
    }

    #[test]
    fn a_bad_id_is_400() {
        let (root, _dir) = setup();
        for raw in ["%E0%A4", "%", "%25%25", "..", "%2F"] {
            assert_eq!(
                call(root.path(), raw),
                Reply::error(400, "invalid chat id"),
                "{raw}"
            );
        }
    }

    #[test]
    fn odd_or_falsy_transcripts_are_empty_and_truthy_ones_are_kept() {
        let (root, dir) = setup();
        for body in [
            "not json",
            "null",
            "42",
            "\"history\"",
            "[1]",
            "{}",
            r#"{"history":null}"#,
            r#"{"history":0}"#,
            r#"{"history":""}"#,
            r#"{"history":false}"#,
        ] {
            std::fs::write(history_file(&dir, "c1"), body).unwrap();
            assert_eq!(call(root.path(), "c1"), empty(), "{body}");
        }
        // Node does not check the stored value is an array; neither is it changed here, so the
        // revision still matches what Node's POST computes.
        std::fs::write(history_file(&dir, "c1"), r#"{"history":{"a":1}}"#).unwrap();
        let revision = revision_of(&wtf16::parse(r#"{"a":1}"#).unwrap()).unwrap();
        assert_eq!(
            call(root.path(), "c1"),
            Reply::json(
                200,
                format!("{{\"history\":{{\"a\":1}},\"revision\":\"{revision}\"}}")
            )
        );
    }

    #[test]
    fn an_interrupted_partial_is_offered_without_its_marker() {
        let (root, dir) = setup();
        for body in [
            // A crash or restart cut off the previous start's reply.
            format!(
                r#"{{"role":"assistant","content":"qa part","savedAt":1,"interrupted":true,"running":{{"start":"{BEFORE}"}}}}"#
            ),
            // Saved before the marker existed.
            r#"{"role":"assistant","content":"qa part","savedAt":1,"interrupted":true}"#
                .to_string(),
        ] {
            write_partial(&dir, "c1", &body);
            assert_eq!(
                call(root.path(), "c1"),
                Reply::json(
                    200,
                    format!("{{\"history\":[],\"revision\":\"{EMPTY_REVISION}\",\"partial\":{{\"role\":\"assistant\",\"content\":\"qa part\",\"savedAt\":1,\"interrupted\":true}}}}")
                ),
                "{body}"
            );
        }
    }

    #[test]
    fn a_running_replys_partial_is_hidden() {
        let (root, dir) = setup();
        write_partial(
            &dir,
            "c1",
            &format!(
                r#"{{"content":"qa streaming","interrupted":true,"running":{{"start":"{NOW}"}}}}"#
            ),
        );
        assert_eq!(call(root.path(), "c1"), empty());
        // A front with no start id (no Node beside it) runs no reply: it is offered.
        assert_eq!(
            call_as(root.path(), USER, "c1", ""),
            Reply::json(
                200,
                format!("{{\"history\":[],\"revision\":\"{EMPTY_REVISION}\",\"partial\":{{\"content\":\"qa streaming\",\"interrupted\":true}}}}")
            )
        );
    }

    #[test]
    fn another_accounts_running_reply_never_hides_this_accounts_partial() {
        // sbstndalton/noevia#2144: Node's in-memory set was keyed by the chat id alone, so a reply
        // running for the same chat id in another account hid this account's interrupted partial.
        let (root, dir) = setup();
        let other = store::user_dir(root.path(), OTHER).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        write_partial(
            &other,
            "c1",
            &format!(r#"{{"content":"qa other","running":{{"start":"{NOW}"}}}}"#),
        );
        write_partial(
            &dir,
            "c1",
            &format!(r#"{{"content":"qa mine","running":{{"start":"{BEFORE}"}}}}"#),
        );
        assert_eq!(
            call(root.path(), "c1"),
            Reply::json(
                200,
                format!("{{\"history\":[],\"revision\":\"{EMPTY_REVISION}\",\"partial\":{{\"content\":\"qa mine\"}}}}")
            )
        );
        // ... and the other account's running partial is neither offered nor read here.
        assert_eq!(call_as(root.path(), OTHER, "c1", NOW), empty());
    }

    #[test]
    fn a_partial_that_is_not_a_plain_object_is_not_offered() {
        let (root, dir) = setup();
        for body in ["[1]", "null", "\"x\"", "7", "not json"] {
            write_partial(&dir, "c1", body);
            assert_eq!(call(root.path(), "c1"), empty(), "{body}");
        }
    }

    #[test]
    fn the_partial_is_keyed_by_the_reduced_id() {
        let (root, dir) = setup();
        write_partial(&dir, "ab", r#"{"content":"qa"}"#);
        assert_eq!(
            call(root.path(), "a%20b"),
            Reply::json(
                200,
                format!("{{\"history\":[],\"revision\":\"{EMPTY_REVISION}\",\"partial\":{{\"content\":\"qa\"}}}}")
            )
        );
    }

    #[test]
    fn a_deleted_chat_reads_like_one_that_never_existed() {
        let (root, dir) = setup();
        std::fs::write(
            history_file(&dir, "c-gone"),
            r#"{"history":[{"role":"user","content":"qa"}]}"#,
        )
        .unwrap();
        write_partial(&dir, "c-gone", r#"{"content":"qa late"}"#);
        assert_ne!(call(root.path(), "c-gone"), empty());
        std::fs::write(dir.join(TOMBSTONES_FILE), "[7,\"c-gone\"]").unwrap();
        assert_eq!(call(root.path(), "c-gone"), empty());
        assert_eq!(call(root.path(), "c-gone%21"), empty());
    }

    #[test]
    fn another_users_transcript_is_never_read() {
        let (root, _dir) = setup();
        let other = store::user_dir(root.path(), OTHER).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(
            history_file(&other, "c1"),
            r#"{"history":[{"content":"qa theirs"}]}"#,
        )
        .unwrap();
        assert_eq!(call(root.path(), "c1"), empty());
    }

    #[test]
    fn a_transcript_this_port_cannot_hold_is_refused_but_such_a_partial_is_left_out() {
        let (root, dir) = setup();
        // U+10F800 is the lossless reader's own range: refused.
        write_partial(&dir, "c1", "{\"content\":\"\u{10F800}\"}");
        assert_eq!(call(root.path(), "c1"), empty());
        std::fs::write(history_file(&dir, "c1"), "{\"history\":[\"\u{10F800}\"]}").unwrap();
        assert_eq!(call(root.path(), "c1"), Reply::internal());
    }
}
