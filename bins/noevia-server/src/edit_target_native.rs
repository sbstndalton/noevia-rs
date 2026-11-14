//! The chat turn's edit-target lookup in the front (track R14 step 3b): what core's
//! `rust-chat-native.cjs` `/internal/chat/tools/edit-target` bridge answers, from the account's
//! workspace ([`server_projects::workspace_view`]), its storage row (never its secret) and the
//! `edit-target-lookup` crate. Behind `NOEVIA_RUST_EDIT_TARGET=1` (read once; unset or any other
//! value = the bridge, as today).
//!
//! The bridge, as ported: its argument checks (`text(v, cap)`: a string of at most `cap` UTF-16
//! units; an edit tool name; `projectId` null or a non-empty string) answer 400, which the turn
//! reads as [`EditTarget::Failed`]; an account that is gone answers 404 (`Failed` too); then, as the
//! user, `resolveEditTarget(projectId ? getProject(projectId) : null, args, { storageAccount })`
//! with `editStorageAccount(user)` (`storageAccount(getStorage(user, true))` for a webdav, nextcloud
//! or s3 row, else none) and `targetDigest(path, account)`. A load that throws in core answers
//! `{ error: 'The file this would change could not be looked up' }`.
//!
//! Differences, each leaving the answer to core instead of guessing ([`lookup`] gives `None`): a
//! project or argument shape core never stores (`Lookup::Unsupported`, `workspace_view`'s
//! `Unsupported`), and a storage row that cannot be read (core reads the row it then signs).
//! getProject's own save (the #352 heal, a changed skills reconcile) is not made here: it changes
//! no field the lookup reads, and the next getProject (core's or the front's) makes it.
use std::path::Path;
use std::sync::{Arc, OnceLock};

use chat_turn::host::EditTarget;
use edit_target_lookup::{resolve_edit_target, target_digest, Lookup};
use server_projects::workspace_view::{self, Refused};

use crate::App;

pub const SWITCH: &str = "NOEVIA_RUST_EDIT_TARGET";

/// core project-edit-target.cjs `EDIT_TOOLS`.
const EDIT_TOOLS: [&str; 2] = ["project_append_file", "project_replace_text"];
/// rust-chat-native.cjs `USER_CAP`, `BODY_CAP` (UTF-16 units).
const USER_CAP: usize = 200;
const BODY_CAP: usize = 16 * 1024 * 1024;

/// Whether the switch is on (`1`), read once.
pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var(SWITCH).as_deref() == Ok("1"))
}

/// `text(v, cap)` for a string the front already holds: within the cap in UTF-16 units.
fn within(s: &str, cap: usize) -> bool {
    js_json::wtf16::len(s) <= cap
}

/// The bridge's body as the front sends it is over core's `readJson(req, BODY_CAP)` (bytes).
fn body_over_cap(user: &str, project_id: Option<&str>, name: &str, args: &str) -> bool {
    use js_json::JValue;
    let body = JValue::obj([
        ("user", JValue::Str(user.to_owned())),
        (
            "projectId",
            project_id.map_or(JValue::Null, |p| JValue::Str(p.to_owned())),
        ),
        ("name", JValue::Str(name.to_owned())),
        ("args", JValue::Str(args.to_owned())),
    ]);
    js_json::wtf16::stringify(&body).is_none_or(|t| t.len() > BODY_CAP)
}

/// The answer, from what was read: `None` leaves the call to core's bridge. `live`: the account has a
/// `users` row that is not disabled (core's `asUser`; `false` answers 404).
pub fn answer(
    data_dir: &Path,
    user: &str,
    live: bool,
    project_id: Option<&str>,
    name: &str,
    args: &str,
    account: Option<&str>,
) -> Option<EditTarget> {
    if user.is_empty()
        || !within(user, USER_CAP)
        || !within(name, 200)
        || !within(args, BODY_CAP)
        || project_id.is_some_and(|p| p.is_empty() || !within(p, 200))
        || !EDIT_TOOLS.contains(&name)
        || body_over_cap(user, project_id, name, args)
    {
        return Some(EditTarget::Failed);
    }
    if !live {
        return Some(EditTarget::Failed);
    }
    // A live account whose folder is not there yet: core makes it and answers; left to core.
    let Ok(dir) = server_projects::store::user_dir(data_dir, user) else {
        return Some(EditTarget::Failed);
    };
    if !dir.is_dir() {
        return None;
    }
    let default = std::env::var("DEFAULT_PROVIDER_ID")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "default".to_owned());
    let view = match workspace_view::load(data_dir, user, &default) {
        Ok(v) => v,
        Err(Refused::InvalidUser) => return Some(EditTarget::Failed),
        // A projects.json the view refuses: core reads some of these (a number project, a value
        // wtf16 cannot hold) and throws on others; left to core either way.
        Err(Refused::Damaged | Refused::Unreadable | Refused::Unsupported) => return None,
    };
    let project = match project_id {
        None => None,
        Some(id) => match view.project(id) {
            Ok(p) => p.map(|p| p.project),
            Err(_) => return None,
        },
    };
    let pid = project
        .as_ref()
        .and_then(|p| p.get("id").as_str())
        .unwrap_or_default()
        .to_owned();
    // `String(file.content || '')`; an inline value core cannot coerce leaves the call to core.
    let unreadable = std::cell::Cell::new(false);
    let content = |file: &js_json::JValue| match view.text(&pid, file) {
        Some(t) => t,
        None => {
            unreadable.set(true);
            String::new()
        }
    };
    let found = resolve_edit_target(project.as_ref(), args, account, &content);
    if unreadable.get() {
        return None;
    }
    Some(match found {
        Lookup::Target { path, account } => EditTarget::Found {
            digest: target_digest(&path, account.as_deref()),
            path,
            account,
        },
        Lookup::Refused(error) => EditTarget::Refused(error),
        Lookup::Unsupported => return None,
    })
}

/// project-edit-target.cjs `storageAccount(getStorage(user, true))` for a browsable kind
/// (storage-client.cjs `isBrowsable`), else none; the region as auth.cjs normalises it.
#[cfg(feature = "rust-storage-reads")]
fn account_digest(row: Option<server_storage_reads::connection_row::Account>) -> Option<String> {
    row.and_then(|r| {
        matches!(r.kind.as_str(), "webdav" | "nextcloud" | "s3").then(|| {
            server_projects::research_report::storage_account(
                &r.kind,
                &r.base_url,
                &r.bucket,
                &s3_sign::normalize_region(&r.region),
                &r.username,
            )
        })
    })
}

/// core `editStorageAccount(user)`: `Ok(None)` for no row or a local one, `Err` when the row
/// cannot be read.
#[cfg(feature = "rust-storage-reads")]
pub(crate) fn storage_account(app: &App, user: &str) -> Result<Option<String>, ()> {
    let store = app.identity.store_blocking().map_err(|_| ())?;
    let row = server_storage_reads::connection_row::account(&store, user).map_err(|_| ())?;
    Ok(account_digest(row))
}

/// core `asUser`: a `users` row that is not disabled; `Err` when it cannot be read.
fn live(app: &App, user: &str) -> Result<bool, ()> {
    let writer = app.writes.writer().map_err(|_| ())?;
    writer
        .read(|r| {
            r.row(
                "SELECT 1 FROM users WHERE id=?1 AND (disabled_at IS NULL OR disabled_at=0)",
                [user],
                |row| row.get::<_, i64>(0),
            )
        })
        .map(|row| row.is_some())
        .map_err(|_| ())
}

#[cfg(not(feature = "rust-storage-reads"))]
fn storage_account(_app: &App, _user: &str) -> Result<Option<String>, ()> {
    Err(())
}

/// The lookup for a turn's edit tool call: `None` leaves it to core's bridge.
pub async fn lookup(
    app: &Arc<App>,
    user: &str,
    project_id: Option<&str>,
    name: &str,
    args: &str,
) -> Option<EditTarget> {
    let data_dir = app.config.data_dir.clone()?;
    let (app, user, project_id, name, args) = (
        Arc::clone(app),
        user.to_owned(),
        project_id.map(str::to_owned),
        name.to_owned(),
        args.to_owned(),
    );
    tokio::task::spawn_blocking(move || {
        let live = live(&app, &user).ok()?;
        let account = storage_account(&app, &user).ok()?;
        answer(
            &data_dir,
            &user,
            live,
            project_id.as_deref(),
            &name,
            &args,
            account.as_deref(),
        )
    })
    .await
    .ok()
    .flatten()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    const USER: &str = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";

    fn workspace(projects: &str) -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("users").join(USER);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("projects.json"), projects).unwrap();
        root
    }

    fn args(name: &str) -> String {
        format!("{{\"name\":\"{name}\"}}")
    }

    fn ask(root: &tempfile::TempDir, project: Option<&str>, args: &str) -> Option<EditTarget> {
        answer(
            root.path(),
            USER,
            true,
            project,
            "project_append_file",
            args,
            None,
        )
    }

    #[test]
    fn the_bridge_checks_fail_the_lookup() {
        let root = workspace(r#"{"projects":[{"id":"qa-p","files":[{"name":"a.txt"}]}]}"#);
        let d = root.path();
        let a = args("a.txt");
        let other = "bbbbbbbb-bbbb-4ccc-8ddd-eeeeeeeeeeee";
        for (user, live, project, name) in [
            ("", true, Some("qa-p"), "project_append_file"),
            (USER, true, Some(""), "project_append_file"),
            (USER, true, Some("qa-p"), "project_create_file"),
            ("not-a-user", true, Some("qa-p"), "project_append_file"),
            // A removed or disabled account: core's asUser answers 404.
            (USER, false, Some("qa-p"), "project_append_file"),
        ] {
            assert_eq!(
                answer(d, user, live, project, name, &a, None),
                Some(EditTarget::Failed),
                "{user} {live} {project:?} {name}"
            );
        }
        let long = "p".repeat(201);
        assert_eq!(
            answer(d, USER, true, Some(&long), "project_append_file", &a, None),
            Some(EditTarget::Failed)
        );
        // A body over core's 16 MiB byte cap, within 16 Mi UTF-16 units.
        let big = format!("{{\"name\":\"{}\"}}", "\u{4e2d}".repeat(6 * 1024 * 1024));
        assert_eq!(ask(&root, Some("qa-p"), &big), Some(EditTarget::Failed));
        // A live account whose folder core would make first: left to core.
        assert_eq!(
            answer(
                d,
                other,
                true,
                Some("qa-p"),
                "project_append_file",
                &a,
                None
            ),
            None
        );
    }

    #[test]
    fn a_project_file_resolves_to_its_target_and_digest() {
        let root = workspace(
            r#"{"projects":[{"id":"qa-p","projectFolder":"noevia projects/QA","files":[{"name":"a.txt"},{"name":"b.md","document":{"state":"queued"},"textRef":"0000000000000000000000000000000000000000000000000000000000000000"}]}]}"#,
        );
        let found = answer(
            root.path(),
            USER,
            true,
            Some("qa-p"),
            "project_replace_text",
            &args("a.txt"),
            Some("qa-acct"),
        );
        let path = "noevia projects/QA/Text/a.txt".to_owned();
        assert_eq!(
            found,
            Some(EditTarget::Found {
                digest: target_digest(&path, Some("qa-acct")),
                path,
                account: Some("qa-acct".into()),
            })
        );
        // A queued document whose text file is missing reads as blank: waiting.
        assert_eq!(
            ask(&root, Some("qa-p"), &args("b.md")),
            Some(EditTarget::Refused(
                "\"b.md\" is waiting to be read (document reading is not available yet), so it cannot be edited yet. Nothing was changed.".into()
            ))
        );
        // No project, or one that does not exist: not in a project.
        for project in [None, Some("qa-none")] {
            assert_eq!(
                ask(&root, project, &args("a.txt")),
                Some(EditTarget::Refused(
                    "this chat is not in a project, so there are no project files to edit.".into()
                ))
            );
        }
    }

    #[test]
    fn what_the_view_refuses_or_core_reads_otherwise_goes_to_core() {
        for projects in [
            r#"{"projects":[null]}"#,
            r#"{"projects":[5,{"id":"qa-p","files":[{"name":"a.txt"}]}]}"#,
            r#"{"projects":[{"id":"qa-p","files":[{"name":"a.txt"}],"x":1e400}]}"#,
            r#"{"projects":[{"id":"qa-p","files":"abc"}]}"#,
            r#"{"projects":[{"id":"qa-p","projectFolder":7,"files":[{"name":"a.txt"}]}]}"#,
        ] {
            let root = workspace(projects);
            assert_eq!(ask(&root, Some("qa-p"), &args("a.txt")), None, "{projects}");
        }
        let root = workspace(r#"{"projects":[{"id":"qa-p","files":[{"name":"a.txt"}]}]}"#);
        assert_eq!(ask(&root, Some("qa-p"), r#"{"name":1e400}"#), None);
    }

    /// Recorded from core: `storageAccount({ ...row, region: normalizeS3Region(row.region) })`.
    #[cfg(feature = "rust-storage-reads")]
    #[test]
    fn the_storage_account_is_cores() {
        use server_storage_reads::connection_row::Account;
        let row = |kind: &str, base_url: &str, bucket: &str, region: &str, username: &str| {
            Some(Account {
                kind: kind.into(),
                base_url: base_url.into(),
                bucket: bucket.into(),
                region: region.into(),
                username: username.into(),
            })
        };
        for (r, want) in [
            (
                row(
                    "s3",
                    "https://qa-s3.invalid//",
                    "qa-bucket",
                    " EU-West-1 ",
                    "qa-user",
                ),
                "ba5a196e4be4625925dc8c688aa1fcaf71fe645df061b374d8532b65c3722453",
            ),
            (
                row(
                    "webdav",
                    "https://qa-dav.invalid/remote.php/dav/",
                    "",
                    "",
                    "qa-dav",
                ),
                "1e400f7a9d9bc6e74336993c03e04b96b871d8601368fb1cd9a1a8641dad832c",
            ),
            (
                row(
                    "nextcloud",
                    "https://qa-nc.invalid",
                    "",
                    "bad region!",
                    "qa-nc",
                ),
                "a05c7c8a9df5e28f65981a4895f58ae376826d6e099c0a965fd04496222e0e95",
            ),
        ] {
            assert_eq!(account_digest(r).as_deref(), Some(want));
        }
        assert_eq!(account_digest(row("local", "", "", "", "")), None);
        assert_eq!(account_digest(None), None);
    }
}
