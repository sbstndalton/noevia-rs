//! Settings → Models → Routing, the write: core routes/models.cjs `PUT /api/routing-default`, answer for answer
//! (sbstndalton/noevia#2384). The signed-in user's default (`preferences.json`'s `defaultRouting`) is saved, and on
//! `applyToExisting === true` every project of the account whose routing differs is switched, stamped with the
//! time, and counted. The read is crates/server-routing-default.
//!
//! Order, as in the route: the workspace load (its 500s), the body (`JSON.parse`, any failure `invalid JSON`), the
//! value check (`routing must be 'auto' or 'manual'`), the preferences save, then the projects. A preferences save
//! that failed answers 500 and changes nothing; a projects save that failed answers its own status with the
//! default already saved (Node saved it first too, and answered the store's failure for the projects).
//!
//! `preferences.json` keeps every other key it has, in JS order (`{ ...preferences, defaultRouting }`), written as
//! `JSON.stringify(value, null, 2)`, mode 0600, by a rename in the account's directory, which is never created
//! here. Projects are changed under the shared projects.json lock on what is on disk now (Node merged its view
//! against the same file), only `routing` and `updatedAt`; nothing else in them is rewritten.
//!
//! Deliberate deviation from the real Core route (sbstndalton/noevia#2388, item 1): `applyToExisting` leaves the internal
//! projects (the Diary extras project and every `cowork-chat-context-*` shadow project, core diary-extras.cjs
//! `internalProject`) alone and out of `updated`. Node switched, stamped and counted them, changing them from a
//! user-facing default. The differential fixture records the corrected expectation (its oracle names both).
//!
//! Differences, all stricter or documented: a `preferences.json` holding a non-empty string (which Node would spread
//! into character keys) is a 500; `updatedAt` is the one instant of the request, where Node read the clock per
//! project; and the Node cache of `preferences` is gone (core workspace.cjs re-reads the file).

use crate::chats;
use crate::free_chats;
use crate::routing_default_input as input;
use crate::store;
use crate::{Ctx, Reply};
use js_json::wtf16;
use js_json::JValue;
use std::path::Path;

const DIARY_EXTRAS: &str = "cowork-diary-extras";
const CHAT_CONTEXT_PREFIX: &str = "cowork-chat-context-";

/// core diary-extras.cjs `internalProject`: not a project the person made or can see.
fn internal(project: &JValue) -> bool {
    project
        .get("id")
        .as_str()
        .is_some_and(|id| id == DIARY_EXTRAS || id.starts_with(CHAT_CONTEXT_PREFIX))
}

/// The per-user file core's workspace loads as `preferences`.
pub const FILE: &str = "preferences.json";
/// core readBody's default cap.
pub const BODY_LIMIT: usize = 1024 * 1024;

/// `readJson(file, {}) || {}` then `{ ...it, defaultRouting }`: the object written.
fn with_default(stored: Option<JValue>, routing: &str) -> Option<JValue> {
    let mut items: Vec<(String, JValue)> = match stored {
        Some(JValue::Obj(items)) => items,
        // Spread of an array: its elements under their indices.
        Some(JValue::Arr(elements)) => elements
            .into_iter()
            .enumerate()
            .map(|(i, v)| (i.to_string(), v))
            .collect(),
        // Spread of a non-empty string is one key per UTF-16 unit: refused rather than imitated.
        Some(JValue::Str(s)) if !s.is_empty() => return None,
        // Missing, unreadable, null, false, 0, "", or any number or boolean: nothing to keep.
        _ => Vec::new(),
    };
    wtf16::set(
        &mut items,
        "defaultRouting",
        JValue::Str(routing.to_string()),
    );
    Some(JValue::Obj(items))
}

/// Persists the preferences without recreating an account directory removed meanwhile.
fn save(dir: &Path, text: &str) -> Result<(), store::StoreError> {
    use rustix::fs::{open, openat, renameat, unlinkat, AtFlags, Mode, OFlags};
    use std::io::Write;
    let parent = std::fs::File::from(
        open(
            dir,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(std::io::Error::from)?,
    );
    let temp = format!(".preferences-{}.tmp", crate::js::random36(24));
    let result = (|| -> Result<(), std::io::Error> {
        let mut file = std::fs::File::from(openat(
            &parent,
            temp.as_str(),
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )?);
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        renameat(&parent, temp.as_str(), &parent, FILE)?;
        parent.sync_all()
    })();
    if result.is_err() {
        let _ = unlinkat(&parent, temp.as_str(), AtFlags::empty());
    }
    result.map_err(store::StoreError::from)
}

#[allow(clippy::cast_precision_loss)]
/// PUT /api/routing-default. `failed`: the body could not be read (over the cap or the stream failed).
pub fn put(ctx: Ctx<'_>, body: &[u8], failed: bool) -> Reply {
    let dir = match store::user_dir(ctx.data_dir, ctx.user_id) {
        Ok(d) => d,
        Err(e) => return chats::store_failure(&e),
    };
    // currentWorkspace(): the load throws (a 500) on a project that is not an object.
    if let Err(reply) = free_chats::workspace_projects(&dir) {
        return reply;
    }
    if failed {
        return Reply::error(400, "invalid JSON");
    }
    let Some(parsed) = input::parse(body) else {
        return Reply::error(400, "invalid JSON");
    };
    let routing = match parsed.routing.as_deref() {
        Some(r @ ("auto" | "manual")) => r.to_string(),
        _ => return Reply::error(400, "routing must be 'auto' or 'manual'"),
    };
    let held = match crate::lock::acquire(&dir.join(FILE)) {
        Ok(h) => h,
        Err(_) => return Reply::error(503, "Routing settings are busy"),
    };
    let stored = match store::read_json(&dir.join(FILE)) {
        Ok(v) => v,
        Err(_) => return Reply::internal(),
    };
    let Some(next) = with_default(stored, &routing).and_then(|v| wtf16::stringify_pretty(&v))
    else {
        return Reply::internal();
    };
    if save(&dir, &next).is_err() {
        return Reply::internal();
    }
    drop(held);
    let mut updated = 0usize;
    if parsed.apply {
        let now = JValue::Num(ctx.now_ms as f64);
        let changed = store::update(&dir, ctx.switch, |projects| {
            for project in projects.iter_mut() {
                if !internal(project) && project.get("routing").as_str() != Some(routing.as_str()) {
                    store::set(project, "routing", JValue::Str(routing.clone()));
                    store::set(project, "updatedAt", now.clone());
                    updated += 1;
                }
            }
            ((), updated > 0)
        });
        if let Err(e) = changed {
            return chats::store_failure(&e);
        }
    }
    let mut text = String::from("{\"routing\":");
    js_json::quote(&routing, &mut text);
    text.push_str(&format!(",\"updated\":{updated}}}"));
    Reply::json(200, text)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::too_many_lines)]
mod tests {
    use super::*;

    fn body_text(reply: &Reply) -> String {
        match &reply.body {
            crate::Body::Json(text) => text.clone(),
            crate::Body::Bytes { .. } => String::from("<bytes>"),
        }
    }

    fn setup(case: &serde_json::Value, user: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("users").join(user);
        std::fs::create_dir_all(&dir).unwrap();
        if let Some(text) = case["initial"]["preferences"].as_str() {
            std::fs::write(dir.join(FILE), text).unwrap();
        }
        if let Some(text) = case["initial"]["projects"].as_str() {
            std::fs::write(dir.join("projects.json"), text).unwrap();
        }
        (root, dir)
    }

    /// The request body a case names: text, raw bytes, or a padded document of a given size.
    fn request(case: &serde_json::Value) -> Vec<u8> {
        let b = &case["body"];
        if let Some(hex) = b["hex"].as_str() {
            (0..hex.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
                .collect()
        } else if let Some(pad) = b["pad"].as_u64() {
            format!(
                "{{\"pad\":\"{}\",\"routing\":\"{}\"}}",
                "x".repeat(pad as usize),
                b["routing"].as_str().unwrap()
            )
            .into_bytes()
        } else {
            b["text"].as_str().unwrap().as_bytes().to_vec()
        }
    }

    /// Cases where this port is deliberately stricter than the real Core route (see the module header): the
    /// reply is Node's 500 and nothing is written.
    const REFUSED: [&str; 1] = ["file-string"];

    #[test]
    fn put_matches_real_core_route_and_workspace_store() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../tests/fixtures/routing-default-put-node.json"
        ))
        .unwrap();
        let user = fixture["user"].as_str().unwrap();
        let now = fixture["now"].as_i64().unwrap();
        let cases = fixture["cases"].as_array().unwrap();
        assert!(cases.len() >= 50);
        for case in cases {
            let name = case["name"].as_str().unwrap();
            let (root, dir) = setup(case, user);
            let ctx = Ctx {
                data_dir: root.path(),
                user_id: user,
                switch: server_store::RustProjects::owned(),
                now_ms: now,
            };
            let bytes = request(case);
            // The route reads at most 1 MiB; a larger body is a failed read.
            let reply = put(ctx, &bytes, bytes.len() > BODY_LIMIT);
            let prefs = std::fs::read_to_string(dir.join(FILE)).ok();
            let projects = std::fs::read_to_string(dir.join("projects.json")).ok();
            if REFUSED.contains(&name) {
                assert_eq!(
                    (reply.status, body_text(&reply)),
                    (500, "{\"error\":\"Internal error\"}".into()),
                    "{name}"
                );
                assert_eq!(
                    prefs.as_deref(),
                    case["initial"]["preferences"].as_str(),
                    "{name}"
                );
                continue;
            }
            let expected = &case["expected"];
            assert_eq!(
                reply.status,
                expected["reply"]["status"].as_u64().unwrap() as u16,
                "{name}"
            );
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&body_text(&reply)).unwrap(),
                expected["reply"]["body"],
                "{name}"
            );
            assert_eq!(
                prefs.as_deref(),
                expected["preferences"].as_str(),
                "{name}: preferences.json"
            );
            assert_eq!(
                projects.as_deref(),
                expected["projects"].as_str(),
                "{name}: projects.json"
            );
            // No temp file is left behind, and the file keeps mode 0600.
            let leftovers: Vec<_> = std::fs::read_dir(&dir)
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .filter(|n| n.starts_with(".preferences-"))
                .collect();
            assert!(leftovers.is_empty(), "{name}: {leftovers:?}");
            if prefs.is_some() && expected["preferences"] != case["initial"]["preferences"] {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(dir.join(FILE))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777;
                assert_eq!(mode, 0o600, "{name}");
            }
        }
    }

    /// sbstndalton/noevia#2388 (item 1): applyToExisting leaves the internal projects alone and out of the count.
    #[test]
    fn apply_to_existing_skips_the_diary_and_chat_context_projects() {
        let user = "11111111-2222-4333-8444-555555555555";
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("users").join(user);
        std::fs::create_dir_all(&dir).unwrap();
        let project = |id: &str| serde_json::json!({"id": id, "routing": "manual", "updatedAt": 1, "modes": ["chat"]});
        let ids = [
            "mine",
            "cowork-diary-extras",
            "cowork-chat-context-abc",
            "cowork-chat-contextual",
        ];
        let initial = serde_json::json!({"projects": ids.map(project)});
        std::fs::write(
            dir.join("projects.json"),
            serde_json::to_string_pretty(&initial).unwrap(),
        )
        .unwrap();
        let ctx = Ctx {
            data_dir: root.path(),
            user_id: user,
            switch: server_store::RustProjects::owned(),
            now_ms: 99,
        };
        let reply = put(ctx, br#"{"routing":"auto","applyToExisting":true}"#, false);
        assert_eq!(body_text(&reply), r#"{"routing":"auto","updated":2}"#);
        let after: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("projects.json")).unwrap())
                .unwrap();
        let by_id = |id: &str| {
            after["projects"]
                .as_array()
                .unwrap()
                .iter()
                .find(|p| p["id"] == id)
                .unwrap()
                .clone()
        };
        for id in ["mine", "cowork-chat-contextual"] {
            assert_eq!(
                (by_id(id)["routing"].clone(), by_id(id)["updatedAt"].clone()),
                (serde_json::json!("auto"), serde_json::json!(99)),
                "{id}"
            );
        }
        for id in ["cowork-diary-extras", "cowork-chat-context-abc"] {
            assert_eq!(by_id(id), project(id), "{id} is untouched");
        }
    }

    #[test]
    fn an_unloadable_workspace_is_node_s_500_before_anything_is_read_or_written() {
        // A project that is not an object makes core's workspace load throw, before the body is looked at.
        let case =
            serde_json::json!({"initial":{"preferences":null,"projects":"{\"projects\":[null]}"}});
        let (root, dir) = setup(&case, "11111111-2222-4333-8444-555555555555");
        let ctx = Ctx {
            data_dir: root.path(),
            user_id: "11111111-2222-4333-8444-555555555555",
            switch: server_store::RustProjects::owned(),
            now_ms: 1,
        };
        for body in [&b"{\"routing\":\"auto\"}"[..], b"{", b""] {
            let reply = put(ctx, body, false);
            assert_eq!(
                (reply.status, body_text(&reply)),
                (500, "{\"error\":\"Internal error\"}".into())
            );
        }
        assert!(!dir.join(FILE).exists());
    }

    #[test]
    fn a_removed_account_is_never_recreated() {
        let root = tempfile::tempdir().unwrap();
        let user = "11111111-2222-4333-8444-555555555555";
        let ctx = Ctx {
            data_dir: root.path(),
            user_id: user,
            switch: server_store::RustProjects::owned(),
            now_ms: 1,
        };
        let reply = put(
            ctx,
            br#"{"routing":"manual","applyToExisting":true}"#,
            false,
        );
        assert_ne!(reply.status, 200, "{}", body_text(&reply));
        assert!(!root.path().join("users").join(user).exists());
    }

    #[test]
    fn a_projects_write_that_fails_leaves_the_saved_default_and_answers_the_store_s_failure() {
        let root = tempfile::tempdir().unwrap();
        let user = "11111111-2222-4333-8444-555555555555";
        let dir = root.path().join("users").join(user);
        std::fs::create_dir_all(&dir).unwrap();
        // A projects.json that cannot be changed: a directory in its place makes the read-modify-write fail.
        std::fs::create_dir(dir.join("projects.json")).unwrap();
        let ctx = Ctx {
            data_dir: root.path(),
            user_id: user,
            switch: server_store::RustProjects::owned(),
            now_ms: 1,
        };
        let reply = put(
            ctx,
            br#"{"routing":"manual","applyToExisting":true}"#,
            false,
        );
        assert_ne!(reply.status, 200);
        assert_eq!(
            std::fs::read_to_string(dir.join(FILE)).unwrap(),
            "{\n  \"defaultRouting\": \"manual\"\n}"
        );
    }
}
