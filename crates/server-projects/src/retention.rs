//! Settings → Data → Delete old chats, the read: core routes/account.cjs `GET
//! /api/account/retention`, with the reads it makes (chat-retention.cjs `read` and `expired`, the
//! workspace's free chats and its projects without the Diary-internal ones, index.cjs
//! `retentionLists`), answer for answer. Moved from Node for sbstndalton/noevia#1071; a read
//! only, so it takes no lock and writes nothing. `PUT` (which deletes the chats that are due) stays
//! Node's: it recounts what is due itself and refuses (409) when that is more than the person
//! confirmed (`confirmDeletes`), so a preview read here never becomes a delete on its own.
//!
//! - `days`: `chat-retention.json`'s `days` when it is one of [`PERIODS`], else `0` (also for a
//!   missing file, invalid JSON or a value that is not an object).
//! - `periods`: [`PERIODS`].
//! - `preview`: for each period, how many chats [`expired`] would delete now.
//!
//! What it counts is read fresh from disk for every request: `free-chats.json` (a value that is
//! not a list is `[]`, workspace.cjs since #1420) and `projects.json`'s projects (see
//! `free_chats::workspace_projects`), leaving out `cowork-diary-extras` and every
//! `cowork-chat-context-*` project (diary-extras.cjs `internalProject`). Rust is the only writer
//! of both files (core rust-projects.cjs), so this is what Node's cached view refreshes to at the
//! start of each of its requests. Accepted difference: Node also counted a change of its own that
//! was still queued for the store (a save in flight); this read sees it once it is written.
//!
//! Reads that fail are read as Node reads them: a file that cannot be read at all (missing,
//! EACCES/EIO, a directory in its place) or is not JSON is "absent" ([`store::read_json`]), so
//! an unreadable `chat-retention.json` is `days: 0` (Node's `read()` catch-all), and an unreadable
//! `projects.json` or `free-chats.json` counts no chats.
//!
//! Node's 500s are kept: a project that is not an object, an id that is not a string
//! (`project.id.startsWith` throws), a project whose `chats` is truthy but not a list
//! (`.filter` throws). Stricter than Node, as a refusal (500): any of the three files holding what
//! this port cannot represent (a lone surrogate escape, see [`store::read_json`]); Node would
//! have read `days` or counted the chats.

use crate::chats::{self, FREE_CHATS_FILE};
use crate::free_chats;
use crate::store;
use crate::{Ctx, Reply};
use js_json::wtf16;
use js_json::JValue;
use std::path::Path;

/// chat-retention.cjs `PERIODS`, in days.
pub const PERIODS: [u32; 3] = [30, 90, 365];
/// chat-retention.cjs `FILE`, in the user's directory.
pub const FILE: &str = "chat-retention.json";
const DAY_MS: f64 = 86_400_000.0;

const DIARY_EXTRAS: &str = "cowork-diary-extras";
const CHAT_CONTEXT_PREFIX: &str = "cowork-chat-context-";

/// One chat [`expired`] would delete: `{ projectId, id }` (`projectId` is `null` for a free chat).
#[derive(Debug, Clone, PartialEq)]
pub struct Due {
    pub project_id: JValue,
    pub id: JValue,
}

impl Due {
    /// `{ projectId, id }` as Node builds it (an `undefined` member is left out when stringified).
    pub fn to_json(&self) -> JValue {
        JValue::obj([
            ("projectId", self.project_id.clone()),
            ("id", self.id.clone()),
        ])
    }
}

/// Where Node's `expired()` throws a TypeError (a `null` project, a truthy `chats` that is not a
/// list).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Thrown;

/// chat-retention.cjs `old(chat)`: a truthy chat, not pinned, with a numeric `updatedAt` before the
/// cutoff.
fn old(chat: &JValue, cutoff: f64) -> bool {
    chat.truthy()
        && !chat.get("pinned").truthy()
        && chat
            .get("updatedAt")
            .as_num()
            .is_some_and(|t| t.is_finite() && t < cutoff)
}

/// chat-retention.cjs `expired({ freeChats, projects, days, now })`: the free chats first, then
/// each project's chats, in list order. `days == 0` is off (nothing is due).
pub fn expired(
    free: &[JValue],
    projects: &[JValue],
    days: u32,
    now_ms: f64,
) -> Result<Vec<Due>, Thrown> {
    if days == 0 {
        return Ok(Vec::new());
    }
    let cutoff = now_ms - f64::from(days) * DAY_MS;
    let mut due: Vec<Due> = free
        .iter()
        .filter(|c| old(c, cutoff))
        .map(|c| Due {
            project_id: JValue::Null,
            id: c.get("id").clone(),
        })
        .collect();
    for project in projects {
        // `project.chats` throws on null and undefined; any other value reads a property.
        if project.is_nullish() {
            return Err(Thrown);
        }
        let list = project.get("chats");
        let chats = match list {
            JValue::Arr(items) => items.as_slice(),
            v if !v.truthy() => &[],
            _ => return Err(Thrown),
        };
        for chat in chats.iter().filter(|c| old(c, cutoff)) {
            due.push(Due {
                project_id: project.get("id").clone(),
                id: chat.get("id").clone(),
            });
        }
    }
    Ok(due)
}

/// chat-retention.cjs `read(dir).days`.
pub fn stored_days(dir: &Path) -> Result<u32, store::StoreError> {
    let days = match store::read_json(&dir.join(FILE))? {
        Some(v) => v.get("days").as_num(),
        None => None,
    };
    Ok(days
        .and_then(|d| PERIODS.iter().copied().find(|p| f64::from(*p) == d))
        .unwrap_or(0))
}

/// index.cjs `retentionLists()`: the free chats and the projects that are not Diary-internal, or
/// Node's 500 where its workspace load or `internalProject` throws.
fn lists(dir: &Path) -> Result<(Vec<JValue>, Vec<JValue>), Reply> {
    let projects = free_chats::workspace_projects(dir)?;
    let mut visible = Vec::with_capacity(projects.len());
    for project in projects {
        let Some(id) = project.get("id").as_str() else {
            return Err(Reply::internal());
        };
        if id == DIARY_EXTRAS || id.starts_with(CHAT_CONTEXT_PREFIX) {
            continue;
        }
        visible.push(project);
    }
    let stored =
        store::read_json(&dir.join(FREE_CHATS_FILE)).map_err(|e| chats::store_failure(&e))?;
    Ok((free_chats::as_list(stored), visible))
}

/// GET /api/account/retention: `{ days, periods, preview: { 30: n, 90: n, 365: n } }`.
pub fn get(ctx: Ctx<'_>) -> Reply {
    let dir = match store::user_dir(ctx.data_dir, ctx.user_id) {
        Ok(d) => d,
        Err(e) => return chats::store_failure(&e),
    };
    let (free, projects) = match lists(&dir) {
        Ok(l) => l,
        Err(reply) => return reply,
    };
    // Node's `now()` once per period; one instant here (they are the same millisecond or later).
    #[allow(clippy::cast_precision_loss)]
    let now = ctx.now_ms as f64;
    let mut preview = Vec::with_capacity(PERIODS.len());
    for days in PERIODS {
        match expired(&free, &projects, days, now) {
            Ok(due) => preview.push((days.to_string(), JValue::Num(due.len() as f64))),
            Err(Thrown) => return Reply::internal(),
        }
    }
    let days = match stored_days(&dir) {
        Ok(d) => d,
        Err(e) => return chats::store_failure(&e),
    };
    let out = JValue::obj([
        ("days", JValue::Num(f64::from(days))),
        (
            "periods",
            JValue::Arr(PERIODS.iter().map(|p| JValue::Num(f64::from(*p))).collect()),
        ),
        ("preview", JValue::Obj(preview)),
    ]);
    match wtf16::stringify(&out) {
        Some(text) => Reply::json(200, text),
        None => Reply::internal(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const USER: &str = "11111111-2222-4333-8444-555555555555";
    const NOW: i64 = 1_700_000_000_000;
    const DAY: i64 = 86_400_000;

    fn setup() -> (tempfile::TempDir, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let dir = store::user_dir(root.path(), USER).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        (root, dir)
    }

    fn call(root: &Path) -> Reply {
        get(Ctx {
            data_dir: root,
            user_id: USER,
            switch: server_store::RustProjects::owned(),
            now_ms: NOW,
        })
    }

    fn body(r: &Reply) -> String {
        match &r.body {
            crate::Body::Json(t) => t.clone(),
            crate::Body::Bytes { .. } => String::from("<bytes>"),
        }
    }

    fn write(dir: &Path, name: &str, text: &str) {
        std::fs::write(dir.join(name), text).unwrap();
    }

    fn age(days: i64) -> i64 {
        NOW - days * DAY - 1
    }

    #[test]
    fn nothing_stored_is_off_with_an_empty_preview() {
        let (root, _dir) = setup();
        let r = call(root.path());
        assert_eq!(r.status, 200);
        assert_eq!(
            body(&r),
            r#"{"days":0,"periods":[30,90,365],"preview":{"30":0,"90":0,"365":0}}"#
        );
    }

    #[test]
    fn counts_each_period_skipping_pinned_undated_and_internal() {
        let (root, dir) = setup();
        write(
            &dir,
            FREE_CHATS_FILE,
            &format!(
                r#"[{{"id":"qa-f1","updatedAt":{}}},{{"id":"qa-f2","updatedAt":{},"pinned":true}},{{"id":"qa-f3"}},null,{{"id":"qa-f4","updatedAt":"{}"}}]"#,
                age(400),
                age(400),
                age(400)
            ),
        );
        write(
            &dir,
            store::PROJECTS_FILE,
            &format!(
                r#"{{"projects":[{{"id":"qa-p","chats":[{{"id":"qa-c1","updatedAt":{}}},{{"id":"qa-c2","updatedAt":{}}}]}},{{"id":"cowork-diary-extras","chats":[{{"id":"qa-d","updatedAt":1}}]}},{{"id":"cowork-chat-context-x","chats":[{{"id":"qa-x","updatedAt":1}}]}},{{"id":"qa-empty","chats":null}}]}}"#,
                age(100),
                age(31)
            ),
        );
        write(&dir, FILE, r#"{"days":90,"lastSweep":5}"#);
        let r = call(root.path());
        assert_eq!(
            body(&r),
            r#"{"days":90,"periods":[30,90,365],"preview":{"30":3,"90":2,"365":1}}"#
        );
    }

    #[test]
    fn a_stored_period_that_is_not_one_reads_off() {
        let (root, dir) = setup();
        for text in [
            r#"{"days":45}"#,
            r#"{"days":"30"}"#,
            "null",
            "30",
            "not json",
            "[]",
        ] {
            write(&dir, FILE, text);
            assert!(
                body(&call(root.path())).starts_with(r#"{"days":0,"#),
                "{text}"
            );
        }
        write(&dir, FILE, r#"{"days":365.0}"#);
        assert!(body(&call(root.path())).starts_with(r#"{"days":365,"#));
    }

    #[test]
    fn an_unreadable_retention_file_reads_off_not_500() {
        // Node's `read()` catches everything: a path that cannot be read as a file (here a
        // directory, as EACCES/EIO would be) is days 0.
        let (root, dir) = setup();
        std::fs::create_dir(dir.join(FILE)).unwrap();
        let r = call(root.path());
        assert_eq!(r.status, 200);
        assert_eq!(
            body(&r),
            r#"{"days":0,"periods":[30,90,365],"preview":{"30":0,"90":0,"365":0}}"#
        );
    }

    #[test]
    fn a_free_list_that_is_not_a_list_counts_nothing() {
        let (root, dir) = setup();
        write(&dir, FREE_CHATS_FILE, r#"{"id":"qa","updatedAt":1}"#);
        assert_eq!(call(root.path()).status, 200);
    }

    #[test]
    fn node_throws_are_500() {
        for projects in [
            r#"{"projects":[{"name":"no id","chats":[]}]}"#,
            r#"{"projects":[{"id":7}]}"#,
            r#"{"projects":[{"id":"qa","chats":{"0":{}}}]}"#,
            r#"{"projects":[{"id":"qa","chats":"x"}]}"#,
            r#"{"projects":[null]}"#,
            r#"{"projects":[[]]}"#,
        ] {
            let (root, dir) = setup();
            write(&dir, store::PROJECTS_FILE, projects);
            assert_eq!(call(root.path()), Reply::internal(), "{projects}");
        }
    }

    #[test]
    fn a_projects_file_without_a_list_is_empty() {
        for projects in ["not json", "null", r#"{"projects":5}"#, "{}"] {
            let (root, dir) = setup();
            write(&dir, store::PROJECTS_FILE, projects);
            assert_eq!(call(root.path()).status, 200, "{projects}");
        }
    }

    #[test]
    fn expired_lists_free_then_projects_with_ids() {
        let free = [JValue::obj([
            ("id", JValue::Str("qa-f".into())),
            ("updatedAt", JValue::Num(0.0)),
        ])];
        let projects = [JValue::obj([
            ("id", JValue::Str("qa-p".into())),
            (
                "chats",
                JValue::Arr(vec![JValue::obj([("updatedAt", JValue::Num(0.0))])]),
            ),
        ])];
        let due = expired(&free, &projects, 30, 1e12).unwrap();
        assert_eq!(
            due.iter()
                .map(|d| wtf16::stringify(&d.to_json()).unwrap())
                .collect::<Vec<_>>(),
            [
                r#"{"projectId":null,"id":"qa-f"}"#,
                r#"{"projectId":"qa-p"}"#
            ]
        );
        assert_eq!(expired(&free, &projects, 0, 1e12).unwrap(), []);
        // The cutoff itself is not before the cutoff.
        assert_eq!(expired(&free, &[], 30, 30.0 * DAY_MS).unwrap(), []);
        assert_eq!(
            expired(&free, &[], 30, 30.0 * DAY_MS + 1.0).unwrap().len(),
            1
        );
    }
}
