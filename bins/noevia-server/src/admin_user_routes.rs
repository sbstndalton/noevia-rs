//! `DELETE /api/admin/users/{id}` (sbstndalton/noevia#2190, #2191 round 9, capability
//! `rust-admin-users`): an administrator deletes an account, complete. It was the one account route
//! Node kept (its cleanup spanned Node's own stores); core's `rust-admin-users.cjs` now refuses it
//! and the rows, the folder and every credential are the front's:
//!
//! 1. the gate (session, origin, CSRF) and the administrator check (403 `administrator required`);
//! 2. the id decoded (a bad escape is 400 `URI malformed`, as Node's catch made it) and the body
//!    read as a JSON object (an empty body is `{}`; invalid JSON, a body that is not an object and a
//!    body over 1 MiB are 400 with core's messages);
//! 3. the auth database: one transaction (crates/server-account `delete`) that checks the typed
//!    username and the last active administrator, revokes the account's invitations and recovery
//!    links and deletes the row, which removes its sessions, passkeys, device grants, app
//!    passwords, features, storage connection, tool policies and Diary connectors by cascade, and
//!    writes the audit row. The acting administrator is checked inside the same transaction, so two
//!    administrators cannot delete each other past the last-administrator rule. `{ ok: false }`
//!    (400) for an unknown account or a name that does not match, 400 `cannot delete the last
//!    active administrator`;
//! 4. everything the account left, as work the handler awaits and a dropped client cannot cut:
//!    its folder under `users/` (a retry covers a writer that adds a file meanwhile; Node's
//!    in-memory workspace is revoked by the folder's absence, core workspace.cjs), its Google Drive
//!    connection (revoked at Google, then forgotten), its MCP sign-ins and keys, its ChatGPT tokens
//!    and its Diary tenant. A broker that fails is asked again; the sweeps at the next start
//!    remove what an account that no longer exists left behind. A folder that could not be removed
//!    is the one failure the answer reports (500, fixed text).

use crate::identity::{self, Rejection};
use crate::{reply, App};
use axum::body::Body;
use axum::http::{Request, Response, StatusCode};
use axum::response::IntoResponse;
use js_json::{wtf16, JValue};
use server_account::delete::{Deleted, LAST_ADMIN};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// core `readJson`'s limit.
const BODY_LIMIT: usize = 1024 * 1024;
const SIDECAR_DEADLINE: Duration = Duration::from_secs(15);
const FOLDER_LEFT: &str = "The account was deleted, but its files could not be removed completely.";

/// `^\/api\/admin\/users\/([^/]+)$` on the WHATWG pathname: the id as the URL spelled it.
pub fn id_of(path: &str) -> Option<&str> {
    path.strip_prefix("/api/admin/users/")
        .filter(|id| !id.is_empty() && !id.contains('/'))
}

fn bad(message: &str) -> Response<Body> {
    reply::error(StatusCode::BAD_REQUEST, message, true)
}

pub async fn serve(
    app: &Arc<App>,
    _conn: crate::serve::Conn,
    req: Request<Body>,
    path: String,
) -> Response<Body> {
    let (parts, body) = req.into_parts();
    let Some(raw) = id_of(&path).map(str::to_owned) else {
        return reply::error(StatusCode::NOT_FOUND, "not found", true);
    };
    let creds = identity::creds_of(&parts);
    let (layer, now) = (Arc::clone(&app.identity), identity::now_ms());
    let (gate_creds, gate_path) = (creds.clone(), path.clone());
    let authn = match tokio::task::spawn_blocking(move || {
        layer.gate_blocking(&gate_creds, &gate_path, now)
    })
    .await
    {
        Ok(Ok(id)) => id,
        Ok(Err(rejection)) => return rejection.into_response(),
        Err(_) => return Rejection::Unavailable.into_response(),
    };
    if authn.user.role != server_auth::Role::Admin {
        return reply::error(StatusCode::FORBIDDEN, "administrator required", true);
    }
    let Some(data_dir) = app.config.data_dir.clone() else {
        return Rejection::Unavailable.into_response();
    };
    // Everything from here is spawned work this handler awaits: a client that goes away after the
    // body is read cannot leave an account half deleted.
    let work = Work {
        app: Arc::clone(app),
        data_dir,
        raw,
        actor: authn.user.id.clone(),
        creds,
        path,
        now,
    };
    let body = crate::account_routes::read_body(body, BODY_LIMIT)
        .await
        .ok();
    match tokio::spawn(work.run(body)).await {
        Ok(response) => response,
        Err(_) => reply::error(StatusCode::INTERNAL_SERVER_ERROR, "Internal error", true),
    }
}

struct Work {
    app: Arc<App>,
    data_dir: PathBuf,
    raw: String,
    actor: String,
    creds: server_auth::Creds,
    path: String,
    now: i64,
}

impl Work {
    async fn run(self, body: Option<server_account::Body>) -> Response<Body> {
        // `decodeURIComponent` inside the route's try.
        let Some(target) = decode(&self.raw) else {
            return bad("URI malformed");
        };
        let typed = match parse(body) {
            Ok(v) => v,
            Err(message) => return bad(message),
        };
        let app = Arc::clone(&self.app);
        let (creds, path, actor, now, id) = (
            self.creds.clone(),
            self.path.clone(),
            self.actor.clone(),
            self.now,
            target.clone(),
        );
        let outcome = tokio::task::spawn_blocking(move || {
            // The gate once more, for the person the first pass let through: the body was read
            // between the two for as long as the client took.
            let layer = Arc::clone(&app.identity);
            match layer.gate_blocking(&creds, &path, identity::now_ms()) {
                Ok(again) if again.user.id == actor => {}
                Ok(_) => return Err(Rejection::Refused(server_auth::Refusal::Unauthorized)),
                Err(rejection) => return Err(rejection),
            }
            #[cfg(debug_assertions)]
            hooks::before_write(&actor);
            let writer = app.writes.writer().map_err(|_| Rejection::Unavailable)?;
            writer
                .write(|tx| {
                    // The acting administrator is still an enabled administrator at commit.
                    let acting = tx.reader().row(
                        "SELECT role FROM users WHERE id=?1 AND disabled_at IS NULL",
                        [&actor],
                        |row| row.get::<_, String>(0),
                    )?;
                    if acting.as_deref() != Some("admin") {
                        return Ok(None);
                    }
                    server_account::delete::delete_user(tx, &actor, &id, typed.get("username"), now)
                        .map(Some)
                })
                .map_err(|_| Rejection::Unavailable)
        })
        .await;
        match outcome {
            Err(_) | Ok(Err(Rejection::Unavailable)) => {
                return reply::error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Account deletion is unavailable right now. Try again shortly.",
                    true,
                )
            }
            Ok(Err(rejection)) => return rejection.into_response(),
            Ok(Ok(None)) => {
                return reply::error(StatusCode::FORBIDDEN, "administrator required", true)
            }
            Ok(Ok(Some(Deleted::Refused))) => {
                return reply::json(
                    StatusCode::BAD_REQUEST,
                    &serde_json::json!({"ok": false}),
                    true,
                )
            }
            Ok(Ok(Some(Deleted::LastAdmin))) => return bad(LAST_ADMIN),
            Ok(Ok(Some(Deleted::Done))) => {}
        }
        let left = cleanup(&self.app, &self.data_dir, &target).await;
        if left.folder {
            return reply::error(StatusCode::INTERNAL_SERVER_ERROR, FOLDER_LEFT, true);
        }
        reply::json(StatusCode::OK, &serde_json::json!({"ok": true}), true)
    }
}

/// `decodeURIComponent`.
fn decode(raw: &str) -> Option<String> {
    server_projects::js::decode_uri_component(raw)
}

/// `requireJsonObject(readJson)`: the body as an object, or core's message.
fn parse(body: Option<server_account::Body>) -> Result<JValue, &'static str> {
    let Some(body) = body else {
        return Err("invalid request body");
    };
    if body.over {
        return Err("Request exceeds size limit");
    }
    let text = String::from_utf8_lossy(&body.bytes);
    let value = if text.is_empty() {
        JValue::Obj(Vec::new())
    } else {
        wtf16::parse(&text).map_err(|_| "invalid JSON")?
    };
    match value {
        JValue::Obj(_) => Ok(value),
        _ => Err("request body must be a JSON object"),
    }
}

/// What the cleanup could not finish.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Left {
    /// The account's folder could not be removed completely.
    pub folder: bool,
}

/// An account's leftovers outside the auth database. The account row is already gone: nothing here
/// can undo the deletion, so each part is retried and then left to the next start's sweep.
pub async fn cleanup(app: &Arc<App>, data_dir: &std::path::Path, id: &str) -> Left {
    let (dir_root, dir_id) = (data_dir.to_path_buf(), id.to_owned());
    let folder = async move {
        let mut last = None;
        for wait in [0u64, 50, 250, 1000] {
            if wait > 0 {
                tokio::time::sleep(Duration::from_millis(wait)).await;
            }
            let (root, user) = (dir_root.clone(), dir_id.clone());
            match tokio::task::spawn_blocking(move || try_remove_folder(&root, &user)).await {
                Ok(Ok(())) => return true,
                Ok(Err(failed)) => last = Some(failed),
                Err(_) => {}
            }
        }
        // The row is gone and nothing else will remove this: say where it is, so it can be removed by hand.
        match last {
            Some(failed) => eprintln!("noevia-server: warning: {}", failed.warning()),
            None => eprintln!(
                "noevia-server: warning: the folder of a deleted account was not removed ({})",
                folder_path(&dir_root, &dir_id).display()
            ),
        }
        false
    };
    let drive = async {
        if let Some(drive) = app.drive.as_ref() {
            drive.forget(id).await;
        }
    };
    let mcp = async {
        #[cfg(feature = "rust-storage-reads")]
        if let Some(mcp) = app.mcp.as_ref() {
            retry(|| mcp.forget_user(app, id)).await;
        }
    };
    let chatgpt = async {
        #[cfg(feature = "rust-chatgpt-status")]
        if let Some(broker) = app.chatgpt_oauth.as_ref() {
            retry(|| broker.disconnect(app, id, true)).await;
        }
    };
    let diary = async {
        if let Some(diary) = app.diary.as_ref() {
            diary.sidecar().delete_tenant(id, SIDECAR_DEADLINE).await;
        }
    };
    let (folder, (), (), (), ()) = tokio::join!(folder, drive, mcp, chatgpt, diary);
    Left { folder: !folder }
}

/// Asks again (twice more, after 200 ms and 1 s) when a broker could not do it.
#[cfg(any(feature = "rust-storage-reads", feature = "rust-chatgpt-status"))]
async fn retry<F, Fut, E>(mut call: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<(), E>>,
{
    for wait in [0u64, 200, 1000] {
        if wait > 0 {
            tokio::time::sleep(Duration::from_millis(wait)).await;
        }
        if call().await.is_ok() {
            return;
        }
    }
    eprintln!("noevia-server: a deleted account's credentials were not removed; the next start removes them");
}

/// Why `users/<id>` could not be removed: where it is and what the file system said.
#[derive(Debug)]
pub struct FolderFailure {
    pub path: PathBuf,
    pub error: std::io::Error,
}

impl FolderFailure {
    /// The log line (a warning: the account row is gone, so the folder stays until someone removes it).
    pub fn warning(&self) -> String {
        format!(
            "the folder of a deleted account could not be removed ({}): {}. Remove it by hand.",
            self.path.display(),
            self.error
        )
    }
}

/// `users/<id>` below the data directory, as [`remove_folder`] would name it.
fn folder_path(data_dir: &std::path::Path, id: &str) -> PathBuf {
    server_projects::store::user_dir(data_dir, id)
        .unwrap_or_else(|_| data_dir.join("users").join("(invalid id)"))
}

/// Removes `users/<id>` below the data directory: `Ok` when it is gone (it may never have been).
/// Only a canonical account id names a folder, and a symlink in its place is removed as a link,
/// never followed.
pub fn try_remove_folder(data_dir: &std::path::Path, id: &str) -> Result<(), FolderFailure> {
    let Ok(dir) = server_projects::store::user_dir(data_dir, id) else {
        return Ok(());
    };
    let failed = |error| FolderFailure {
        path: dir.clone(),
        error,
    };
    match std::fs::symlink_metadata(&dir) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(failed(e)),
        Ok(m) if m.file_type().is_symlink() || !m.is_dir() => {
            std::fs::remove_file(&dir).map_err(failed)
        }
        Ok(_) => match std::fs::remove_dir_all(&dir) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(failed(e)),
        },
    }
}

/// [`try_remove_folder`] as a yes or no.
pub fn remove_folder(data_dir: &std::path::Path, id: &str) -> bool {
    try_remove_folder(data_dir, id).is_ok()
}

/// How many orphan ids a start names in its log (the count is always given).
const ORPHANS_LISTED: usize = 20;

/// The `users/<uuid>` folders below the data directory whose id has no row in `known`, sorted. Only a
/// canonical account id counts as a candidate, and nothing is read from or removed in them. An empty
/// `known` proves nothing (an unreadable or fresh database) and names none.
pub fn orphan_folders(data_dir: &std::path::Path, known: &[String]) -> Vec<String> {
    if known.is_empty() {
        return Vec::new();
    }
    let Ok(entries) = std::fs::read_dir(data_dir.join("users")) else {
        return Vec::new();
    };
    let mut ids: Vec<String> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|name| server_projects::store::user_dir(data_dir, name).is_ok())
        .filter(|name| !known.contains(name))
        .collect();
    ids.sort();
    ids
}

/// The start-up line for [`orphan_folders`]: a count and the first ids. These folders are never
/// removed here: a database restored from an older copy makes live accounts look orphaned.
pub fn orphan_report(ids: &[String]) -> Option<String> {
    if ids.is_empty() {
        return None;
    }
    let shown = ids
        .iter()
        .take(ORPHANS_LISTED)
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    let more = ids.len().saturating_sub(ORPHANS_LISTED);
    Some(format!(
        "{} account folder(s) under users/ have no account row (left in place, never removed automatically): {shown}{}",
        ids.len(),
        if more > 0 {
            format!(" and {more} more")
        } else {
            String::new()
        }
    ))
}

/// At start: logs the account folders that have no row. Nothing is removed.
pub async fn report_orphan_folders(app: &Arc<App>) -> usize {
    let Some(data_dir) = app.config.data_dir.clone() else {
        return 0;
    };
    let a = Arc::clone(app);
    let found = tokio::task::spawn_blocking(move || -> Option<Vec<String>> {
        let writer = a.writes.writer().ok()?;
        let known: Vec<String> = writer
            .read(|r| r.rows("SELECT id FROM users", [], |row| row.get::<_, String>(0)))
            .ok()?;
        Some(orphan_folders(&data_dir, &known))
    })
    .await
    .ok()
    .flatten()
    .unwrap_or_default();
    if let Some(line) = orphan_report(&found) {
        eprintln!("noevia-server: warning: {line}");
    }
    found.len()
}

/// Test hooks, compiled in debug builds only (`cargo test` and the CI replays); a release build has
/// neither the module nor the call.
#[cfg(debug_assertions)]
#[doc(hidden)]
pub mod hooks {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    type Hook = Arc<dyn Fn() + Send + Sync>;
    static BEFORE_WRITE: Mutex<Option<HashMap<String, Hook>>> = Mutex::new(None);

    /// Runs `hook` when this administrator's delete has passed the second gate and not yet opened its
    /// transaction (a test disables the administrator there).
    pub fn on_before_write(actor: &str, hook: impl Fn() + Send + Sync + 'static) {
        if let Ok(mut map) = BEFORE_WRITE.lock() {
            map.get_or_insert_with(HashMap::new)
                .insert(actor.to_owned(), Arc::new(hook));
        }
    }

    pub(super) fn before_write(actor: &str) {
        let hook = BEFORE_WRITE
            .lock()
            .ok()
            .and_then(|map| map.as_ref().and_then(|m| m.get(actor).cloned()));
        if let Some(hook) = hook {
            hook();
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;

    const ID: &str = "00000000-0000-4000-8000-00000000000a";

    #[test]
    fn the_id_is_one_segment() {
        assert_eq!(id_of("/api/admin/users/abc"), Some("abc"));
        for no in [
            "/api/admin/users/",
            "/api/admin/users/a/b",
            "/api/admin/users",
            "/api/admin/users/a/disabled",
        ] {
            assert_eq!(id_of(no), None, "{no}");
        }
    }

    #[test]
    fn the_body_is_a_json_object_with_cores_messages() {
        let body = |t: &str| {
            Some(server_account::Body {
                bytes: t.as_bytes().to_vec(),
                over: false,
            })
        };
        assert!(matches!(parse(body("")), Ok(JValue::Obj(_))));
        assert!(matches!(
            parse(body(r#"{"username":"x"}"#)),
            Ok(JValue::Obj(_))
        ));
        assert_eq!(parse(body("nope")), Err("invalid JSON"));
        for not in ["null", "[]", "5", "\"s\""] {
            assert_eq!(
                parse(body(not)),
                Err("request body must be a JSON object"),
                "{not}"
            );
        }
        assert_eq!(
            parse(Some(server_account::Body {
                bytes: vec![],
                over: true
            })),
            Err("Request exceeds size limit")
        );
        assert_eq!(parse(None), Err("invalid request body"));
    }

    #[test]
    fn the_folder_goes_whole_and_a_link_is_not_followed() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("users").join(ID);
        std::fs::create_dir_all(dir.join("project-sources").join("p")).unwrap();
        std::fs::write(dir.join("project-sources").join("p").join("t.txt"), "t").unwrap();
        std::fs::write(dir.join("projects.json"), "{}").unwrap();
        assert!(remove_folder(root.path(), ID));
        assert!(!dir.exists());
        assert!(remove_folder(root.path(), ID), "already gone is gone");
        assert!(
            remove_folder(root.path(), "../etc"),
            "not an account id: nothing to remove"
        );
        // A link in its place: the link goes, the target stays.
        let outside = root.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("keep"), "k").unwrap();
        std::os::unix::fs::symlink(&outside, &dir).unwrap();
        assert!(remove_folder(root.path(), ID));
        assert!(std::fs::symlink_metadata(&dir).is_err());
        assert!(outside.join("keep").exists());
    }

    #[test]
    fn a_folder_that_cannot_be_removed_is_reported_with_its_path() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("users").join(ID);
        let inner = dir.join("project-sources");
        std::fs::create_dir_all(&inner).unwrap();
        std::fs::write(inner.join("t.txt"), "t").unwrap();
        // A directory its owner cannot write to: the file inside cannot be unlinked.
        std::fs::set_permissions(&inner, std::fs::Permissions::from_mode(0o500)).unwrap();
        let restore = || std::fs::set_permissions(&inner, std::fs::Permissions::from_mode(0o700));
        if std::fs::write(inner.join("probe"), "p").is_ok() {
            // Running as root: permissions do not stop it, so there is no failure to observe.
            restore().unwrap();
            return;
        }
        let failed = try_remove_folder(root.path(), ID).unwrap_err();
        assert_eq!(failed.path, dir, "the path of the folder");
        let line = failed.warning();
        assert!(
            line.contains(&dir.display().to_string()),
            "the warning names the path: {line}"
        );
        assert!(!remove_folder(root.path(), ID));
        assert!(dir.exists(), "nothing else removes it");
        restore().unwrap();
        assert!(remove_folder(root.path(), ID), "and it goes once it can");
    }

    #[test]
    fn folders_with_no_account_row_are_named_and_never_removed() {
        let root = tempfile::tempdir().unwrap();
        let users = root.path().join("users");
        let (a, b, c) = (
            "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
            "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
            "cccccccc-cccc-4ccc-8ccc-cccccccccccc",
        );
        for id in [a, b, c, "not-an-account", "shared"] {
            std::fs::create_dir_all(users.join(id).join("rag")).unwrap();
        }
        // A file shaped like an id is not a folder.
        std::fs::write(users.join("dddddddd-dddd-4ddd-8ddd-dddddddddddd"), "x").unwrap();
        let known = vec![a.to_owned()];
        let orphans = orphan_folders(root.path(), &known);
        assert_eq!(orphans, vec![b.to_owned(), c.to_owned()]);
        for id in [a, b, c] {
            assert!(users.join(id).join("rag").exists(), "{id} is still there");
        }
        // An empty or unreadable table proves nothing; no data directory has no folders.
        assert!(orphan_folders(root.path(), &[]).is_empty());
        assert!(orphan_folders(&root.path().join("nowhere"), &known).is_empty());
        // The line: a count and the ids.
        let line = orphan_report(&orphans).unwrap();
        assert!(line.starts_with("2 account folder(s)"), "{line}");
        assert!(
            line.contains(b) && line.contains(c) && !line.contains(a),
            "{line}"
        );
        assert!(line.contains("never removed"), "{line}");
        assert_eq!(orphan_report(&[]), None);
        let many: Vec<String> = (0..25).map(|i| format!("{i:036}")).collect();
        let line = orphan_report(&many).unwrap();
        assert!(line.starts_with("25 account folder(s)"), "{line}");
        assert!(line.ends_with("and 5 more"), "{line}");
    }
}
