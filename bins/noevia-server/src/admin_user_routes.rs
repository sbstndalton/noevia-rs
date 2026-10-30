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
        for wait in [0u64, 50, 250, 1000] {
            if wait > 0 {
                tokio::time::sleep(Duration::from_millis(wait)).await;
            }
            let (root, user) = (dir_root.clone(), dir_id.clone());
            let removed = tokio::task::spawn_blocking(move || remove_folder(&root, &user)).await;
            if matches!(removed, Ok(true)) {
                return true;
            }
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

/// Removes `users/<id>` below the data directory: true when it is gone (it may never have been).
/// Only a canonical account id names a folder, and a symlink in its place is removed as a link,
/// never followed.
pub fn remove_folder(data_dir: &std::path::Path, id: &str) -> bool {
    let Ok(dir) = server_projects::store::user_dir(data_dir, id) else {
        return true;
    };
    match std::fs::symlink_metadata(&dir) {
        Err(e) => e.kind() == std::io::ErrorKind::NotFound,
        Ok(m) if m.file_type().is_symlink() || !m.is_dir() => std::fs::remove_file(&dir).is_ok(),
        Ok(_) => match std::fs::remove_dir_all(&dir) {
            Ok(()) => true,
            Err(e) => e.kind() == std::io::ErrorKind::NotFound,
        },
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
}
