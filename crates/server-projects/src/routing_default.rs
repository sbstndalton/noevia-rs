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
//! One serialization spans the preferences save and the projects apply (the preferences lock is held until the projects
//! are written; no path takes the two in the other order), so concurrent applies end with the default and the projects
//! agreeing. `updatedAt` is read from the clock as each project is changed, as Node did. A `projects.json` that cannot
//! be read as a list (a directory, not JSON) is the empty list Node's workspace load makes of it: 200, nothing updated,
//! nothing written. The Node cache of `preferences` is gone (core workspace.cjs re-reads the file).

use crate::chats;
use crate::free_chats;
use crate::routing_default_input as input;
use crate::store;
use crate::workspace_load;
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

/// `{ ...'ab' }`: each UTF-16 code unit of the string under its index.
fn spread_string(s: &str) -> Vec<(String, JValue)> {
    let mut units: Vec<u16> = Vec::new();
    for c in s.chars() {
        match wtf16::lone_unit(c) {
            Some(unit) => units.push(unit),
            None => {
                let mut buf = [0u16; 2];
                units.extend_from_slice(c.encode_utf16(&mut buf));
            }
        }
    }
    units
        .into_iter()
        .enumerate()
        .map(|(i, unit)| {
            let held = wtf16::lone_char(unit).or_else(|| char::from_u32(u32::from(unit)));
            (
                i.to_string(),
                JValue::Str(held.map(String::from).unwrap_or_default()),
            )
        })
        .collect()
}

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
        // Spread of a string: one key per UTF-16 unit (a pair is two keys, each a lone surrogate).
        Some(JValue::Str(s)) => spread_string(&s),
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

/// PUT /api/routing-default. `failed`: the body could not be read (over the cap or the stream failed).
pub fn put(ctx: Ctx<'_>, body: &[u8], failed: bool) -> Reply {
    put_with(ctx, body, failed, &crate::wall_ms, &real_sleep)
}

fn real_sleep(ms: u64) {
    std::thread::sleep(std::time::Duration::from_millis(ms));
}

/// [`put`] with the clock (`Date.now()`, read as each project is changed) and the wait between tries for the preferences
/// lock supplied.
#[allow(clippy::cast_precision_loss)]
fn put_with(
    ctx: Ctx<'_>,
    body: &[u8],
    failed: bool,
    clock: &dyn Fn() -> i64,
    wait: &dyn Fn(u64),
) -> Reply {
    let dir = match store::user_dir(ctx.data_dir, ctx.user_id) {
        Ok(d) => d,
        Err(e) => return chats::store_failure(&e),
    };
    // currentWorkspace(): the workspace load, with its migrations and the saves they make; it throws (a 500) on a project
    // that is not an object.
    if let Err(reply) = workspace_load::open(ctx, None) {
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
    // Held to the end: the default and the projects it switches are one section.
    let _held = match crate::lock::acquire_with(&dir.join(FILE), &crate::wall_ms, wait) {
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
    let mut updated = 0usize;
    if parsed.apply {
        // The list as Node's workspace holds it: a file that cannot be read as one is the empty list.
        let view = match free_chats::workspace_projects(&dir) {
            Ok(v) => v,
            Err(reply) => return reply,
        };
        let differs =
            |p: &JValue| !internal(p) && p.get("routing").as_str() != Some(routing.as_str());
        if view.iter().any(differs) {
            let changed = store::update(&dir, ctx.switch, |projects| {
                // saveProjects() writes the list as the load left it (its normalizations ride on this save).
                *projects = workspace_load::for_save(&dir, std::mem::take(projects));
                for project in projects.iter_mut() {
                    if differs(project) {
                        store::set(project, "routing", JValue::Str(routing.clone()));
                        store::set(project, "updatedAt", JValue::Num(clock() as f64));
                        updated += 1;
                    }
                }
                ((), updated > 0)
            });
            if let Err(e) = changed {
                return chats::store_failure(&e);
            }
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
        } else if case["initial"]["projectsDirectory"] == true {
            std::fs::create_dir(dir.join("projects.json")).unwrap();
        }
        if case["initial"]["blockBackups"] == true {
            std::fs::write(dir.join("migration-backups"), "in the way").unwrap();
        }
        (root, dir)
    }

    /// What the load wrote besides the two files: `migration-backups/*` and `project-sources/**`, by relative path.
    fn extras(dir: &std::path::Path) -> std::collections::BTreeMap<String, String> {
        fn walk(
            dir: &std::path::Path,
            rel: &str,
            out: &mut std::collections::BTreeMap<String, String>,
        ) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                let path = if rel.is_empty() {
                    name.clone()
                } else {
                    format!("{rel}/{name}")
                };
                if e.path().is_dir() {
                    walk(&e.path(), &path, out);
                } else if (path.starts_with("migration-backups/")
                    || path.starts_with("project-sources/"))
                    && !path.ends_with(".tmp")
                {
                    out.insert(path, std::fs::read_to_string(e.path()).unwrap());
                }
            }
        }
        let mut out = std::collections::BTreeMap::new();
        walk(dir, "", &mut out);
        out
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
            let reply = put_with(ctx, &bytes, bytes.len() > BODY_LIMIT, &|| now, &real_sleep);
            let prefs = std::fs::read_to_string(dir.join(FILE)).ok();
            let projects = std::fs::read_to_string(dir.join("projects.json")).ok();
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
            let want_extras: std::collections::BTreeMap<String, String> = match &expected["extras"]
            {
                serde_json::Value::Object(m) => m
                    .iter()
                    .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_owned()))
                    .collect(),
                _ => std::collections::BTreeMap::new(),
            };
            assert_eq!(
                extras(&dir),
                want_extras,
                "{name}: migration backups and moved texts"
            );
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
        let reply = put_with(
            ctx,
            br#"{"routing":"auto","applyToExisting":true}"#,
            false,
            &|| 99,
            &real_sleep,
        );
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

    const USER: &str = "11111111-2222-4333-8444-555555555555";

    fn account(projects: &[(&str, &str)]) -> (tempfile::TempDir, std::path::PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("users").join(USER);
        std::fs::create_dir_all(&dir).unwrap();
        let list: Vec<_> = projects
            .iter()
            .map(|(id, routing)| serde_json::json!({"id": id, "routing": routing, "updatedAt": 1, "modes": ["chat"]}))
            .collect();
        std::fs::write(
            dir.join("projects.json"),
            serde_json::to_string_pretty(&serde_json::json!({ "projects": list })).unwrap(),
        )
        .unwrap();
        (root, dir)
    }

    fn ctx_of(root: &tempfile::TempDir) -> Ctx<'_> {
        Ctx {
            data_dir: root.path(),
            user_id: USER,
            switch: server_store::RustProjects::owned(),
            now_ms: 1,
        }
    }

    /// Node's workspace load makes the empty list of a projects.json it cannot read as one: 200, nothing updated, no
    /// projects write, and the default saved.
    #[test]
    fn an_unreadable_projects_file_is_an_empty_list_not_a_failure() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("users").join(USER);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir(dir.join("projects.json")).unwrap();
        let reply = put(
            ctx_of(&root),
            br#"{"routing":"manual","applyToExisting":true}"#,
            false,
        );
        assert_eq!(
            (reply.status, body_text(&reply)),
            (200, r#"{"routing":"manual","updated":0}"#.into())
        );
        assert_eq!(
            std::fs::read_to_string(dir.join(FILE)).unwrap(),
            "{\n  \"defaultRouting\": \"manual\"\n}"
        );
        assert!(
            dir.join("projects.json").is_dir(),
            "nothing was written over it"
        );
    }

    /// Each project is stamped when it is changed (`Date.now()` in the loop), not with the request's start.
    #[test]
    fn each_project_is_stamped_when_it_is_changed() {
        let (root, dir) = account(&[
            ("a", "manual"),
            ("b", "auto"),
            ("c", "manual"),
            ("d", "manual"),
        ]);
        let tick = std::cell::Cell::new(100);
        let reply = put_with(
            ctx_of(&root),
            br#"{"routing":"auto","applyToExisting":true}"#,
            false,
            &|| {
                tick.set(tick.get() + 1);
                tick.get()
            },
            &real_sleep,
        );
        assert_eq!(body_text(&reply), r#"{"routing":"auto","updated":3}"#);
        let after: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("projects.json")).unwrap())
                .unwrap();
        let stamps: Vec<_> = after["projects"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["updatedAt"].clone())
            .collect();
        assert_eq!(
            serde_json::Value::Array(stamps),
            serde_json::json!([101, 1, 102, 103])
        );
    }

    /// The default and the projects it switches are one section: while one apply waits for the projects lock, no
    /// other can change the default under it. The second writer says so itself: its wait for the preferences lock is
    /// the signal, so nothing here depends on how long anything takes.
    #[test]
    fn the_default_and_the_projects_are_one_section() {
        let (root, dir) = account(&[("a", "manual"), ("b", "manual")]);
        let gate = crate::lock::acquire(&dir.join("projects.json")).unwrap();
        let (blocked_tx, blocked_rx) = std::sync::mpsc::channel::<()>();
        let saved =
            |want: &str| std::fs::read_to_string(dir.join(FILE)).is_ok_and(|t| t.contains(want));
        let (first, second) = std::thread::scope(|scope| {
            let a = scope.spawn(|| {
                put(
                    ctx_of(&root),
                    br#"{"routing":"auto","applyToExisting":true}"#,
                    false,
                )
            });
            // The first apply has saved its default and now waits for the projects lock the test holds.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while !saved("auto") {
                assert!(
                    std::time::Instant::now() < deadline,
                    "first writer never saved its default"
                );
                std::thread::yield_now();
            }
            let b = scope.spawn(|| {
                put_with(
                    ctx_of(&root),
                    br#"{"routing":"manual","applyToExisting":true}"#,
                    false,
                    &crate::wall_ms,
                    &|ms| {
                        // Each try that finds the preferences lock held reports it before it waits.
                        let _ = blocked_tx.send(());
                        real_sleep(ms);
                    },
                )
            });
            // Fails (instead of passing by luck) when the second writer never meets the lock, as when the default is
            // saved and unlocked before the projects are written.
            assert!(
                blocked_rx
                    .recv_timeout(std::time::Duration::from_secs(20))
                    .is_ok(),
                "the second apply never waited for the preferences lock"
            );
            assert!(
                saved("auto") && !saved("manual"),
                "the second apply changed the default while the first still held the section"
            );
            drop(gate);
            (a.join().unwrap(), b.join().unwrap())
        });
        assert_eq!((first.status, second.status), (200, 200));
        let default = std::fs::read_to_string(dir.join(FILE)).unwrap();
        let after: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("projects.json")).unwrap())
                .unwrap();
        let routing = if default.contains("manual") {
            "manual"
        } else {
            "auto"
        };
        for p in after["projects"].as_array().unwrap() {
            assert_eq!(
                p["routing"], routing,
                "the projects follow the default that ended up saved"
            );
        }
    }

    /// Many applies at once: one whole document, and the projects agree with the default that is saved.
    #[test]
    fn concurrent_applies_end_consistent() {
        let (root, dir) = account(&[("a", "auto"), ("b", "manual"), ("c", "auto")]);
        let statuses: Vec<u16> = std::thread::scope(|scope| {
            let all: Vec<_> = (0..12)
                .map(|i| {
                    let root = &root;
                    scope.spawn(move || {
                        let body: &[u8] = if i % 2 == 0 {
                            br#"{"routing":"manual","applyToExisting":true}"#
                        } else {
                            br#"{"routing":"auto","applyToExisting":true}"#
                        };
                        put(ctx_of(root), body, false).status
                    })
                })
                .collect();
            all.into_iter().map(|h| h.join().unwrap()).collect()
        });
        assert!(statuses.iter().all(|s| *s == 200), "{statuses:?}");
        let prefs: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join(FILE)).unwrap()).unwrap();
        let after: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("projects.json")).unwrap())
                .unwrap();
        for p in after["projects"].as_array().unwrap() {
            assert_eq!(p["routing"], prefs["defaultRouting"]);
        }
    }
}
