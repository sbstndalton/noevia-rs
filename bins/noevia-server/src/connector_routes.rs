//! Settings -> Connectors for Google Drive (sbstndalton/noevia#2190 round 7), moved from core
//! `routes/connectors.cjs`, and the broker's internal operations Node's chat tools call:
//!
//! Public (any signed-in account, each sees and changes only its own; the Rust front is the single
//! writer of `tool_policies`, core refuses the routes with `RUST_GOOGLE_DRIVE_OWNED`):
//! - `POST /api/connectors/gdrive/connect`      start Google's device sign-in for this account
//! - `POST /api/connectors/gdrive/disconnect`   revoke and forget this account's connection
//! - `PUT  /api/connectors/gdrive/policy`       `{tools:[name...], mode:'allow'|'ask'|'block'}`
//! - `PUT  /api/connectors/gdrive/backup-copy`  `{enabled}` (the administrator whose Drive holds backups)
//!
//! `GET /api/connectors` and `PUT /api/connectors/nextcloud/policy` are this front's since
//! sbstndalton/noevia#2202 round 2 (nextcloud_routes.rs, capability `rust-connectors`): the Nextcloud
//! half lists the toolbox tools the Nextcloud MCP server offers and whether each writes, which comes
//! from the discovery snapshot Node publishes (mcp_discovery.rs), joined with the account's storage
//! connection and its tool permissions, both this front's.
//!
//! Internal (`POST /v1/gdrive/<op>` on the store listener, loopback + per-start secret, `{ user,
//! admin, ... }` from the authenticated request):
//! - `session { rejected? }` -> `{ accessToken, expiresAt, generation }` or 401/409/502
//!   `{ error, code, status }` with code `disconnected`, `reconnect` or `transient`
//! - `reject { generation }` -> `{ dropped }`; `disconnect` -> `{ ok, state }`
//! - `reseal {}` -> `{ current, rotated, empty, failed }`: secrets:rotate re-seals every account's
//!   token file under the current secrets.key, under the same fence as every other change
//!
//! (`view` and `policy` served core's `GET /api/connectors` and the policy writes; the front answers
//! those itself since sbstndalton/noevia#2202 round 2 and both ops are gone.)
//!
//! `admin: true` is checked against `users.role`; a claim the table does not confirm is a member.
//!
//! A refresh token never leaves this process; an access token is in no log.
use crate::gdrive::{self, now_ms, Fail, Slot};
use crate::{identity, reply, App};
use axum::body::Body;
use axum::http::{Method, Request, Response, StatusCode};
use axum::response::IntoResponse;
use http_body_util::BodyExt;
use js_json::JValue;
use std::sync::Arc;
use std::time::Duration;

pub const CONNECT: &str = "/api/connectors/gdrive/connect";
pub const DISCONNECT: &str = "/api/connectors/gdrive/disconnect";
pub const POLICY: &str = "/api/connectors/gdrive/policy";
pub const BACKUP_COPY: &str = "/api/connectors/gdrive/backup-copy";
/// sbstndalton/noevia#2202 round 2: `GET /api/connectors` and `PUT /api/connectors/nextcloud/policy`
/// (nextcloud_routes.rs).
pub const LIST: &str = "/api/connectors";
pub const NEXTCLOUD_POLICY: &str = "/api/connectors/nextcloud/policy";
pub const ROUTES: &[&str] = &[
    CONNECT,
    DISCONNECT,
    POLICY,
    BACKUP_COPY,
    LIST,
    NEXTCLOUD_POLICY,
];

const BODY_LIMIT: usize = 1024 * 1024;
const BODY_DEADLINE: Duration = Duration::from_secs(10);
const UNREACHABLE: &str = "Google Drive could not be reached.";
/// A failure to store tool permissions: the same table serves the Nextcloud permissions, so it is
/// not Google Drive's to blame.
const POLICY_UNSAVED: &str = "The tool permissions could not be saved.";

/// core gdrive-tools.cjs TOOLS (the order Settings lists them), their labels, and READS.
pub const TOOLS: [(&str, &str); 7] = [
    ("drive_search_files", "Search files"),
    ("drive_read_file", "Read file content"),
    ("drive_get_metadata", "Get file metadata"),
    ("drive_list_recent", "List recent files"),
    ("drive_create_file", "Create file"),
    ("drive_update_file", "Update file"),
    ("drive_trash_file", "Trash file"),
];
const READS: [&str; 4] = [
    "drive_search_files",
    "drive_read_file",
    "drive_get_metadata",
    "drive_list_recent",
];

/// `isWriteTool(name)` for the Drive tools: anything not in the box's `reads` list writes.
pub fn is_write(name: &str) -> bool {
    !READS.contains(&name)
}

fn s(v: &str) -> JValue {
    JValue::Str(v.to_owned())
}

pub(crate) fn answer(status: u16, body: &JValue) -> Response<Body> {
    reply::json_text(
        StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
        js_json::stringify(body).unwrap_or_else(|| "{}".into()),
        true,
    )
}

pub(crate) fn failed(e: &Fail) -> Response<Body> {
    let status = if e.status > 0 && e.status < 600 {
        e.status
    } else {
        502
    };
    answer(
        status,
        &JValue::obj([("error", JValue::Str(e.message.clone()))]),
    )
}

/// The mode that applies to one call (tool-policy.cjs `mode`): the stored row, or the default.
pub(crate) fn mode_of(app: &App, user: &str, tool: &str, write: bool) -> &'static str {
    let stored = app.writes.writer().ok().and_then(|w| {
        w.read(|r| {
            r.row(
                "SELECT mode FROM tool_policies WHERE user_id=?1 AND tool=?2",
                [user, tool],
                |row| row.get::<_, String>(0),
            )
        })
        .ok()
        .flatten()
    });
    let units: Option<Vec<u16>> = stored.as_deref().map(|m| m.encode_utf16().collect());
    policy_leaves::mode(units.as_deref(), write).name()
}

/// Core's `view(user)`: the Google Drive connector object for this account.
pub(crate) async fn view(app: &Arc<App>, user: &str, admin: bool) -> Result<JValue, Fail> {
    let drive = gdrive::of(app).ok_or_else(|| Fail::new(UNREACHABLE, 502))?;
    let slot = drive.slot_for(user, admin)?;
    let st = drive.state(&slot);
    let backup = if slot == Slot::Backup {
        match app.offsite.as_ref() {
            Some(service) if service.settings.use_dir() => {
                let (copy_enabled, copy) = service.copy_view();
                JValue::obj([
                    ("enabled", JValue::Bool(service.enabled(app).await)),
                    ("copyEnabled", JValue::Bool(copy_enabled)),
                    ("copy", copy),
                    ("lastBackup", service.last_backup()),
                ])
            }
            _ => JValue::Null,
        }
    } else {
        JValue::Null
    };
    let (a, u) = (Arc::clone(app), user.to_owned());
    let tools = tokio::task::spawn_blocking(move || {
        JValue::Arr(
            TOOLS
                .iter()
                .map(|(name, label)| {
                    JValue::obj([
                        ("name", s(name)),
                        ("label", s(label)),
                        ("write", JValue::Bool(is_write(name))),
                        ("mode", s(mode_of(&a, &u, name, is_write(name)))),
                    ])
                })
                .collect(),
        )
    })
    .await
    .map_err(|_| Fail::new(UNREACHABLE, 502))?;
    let mut out = vec![
        ("id", s("gdrive")),
        ("name", s("Google Drive")),
        ("configured", JValue::Bool(st.configured)),
        ("state", s(st.state)),
        ("email", st.email.as_deref().map_or(JValue::Null, s)),
        (
            "connectedAt",
            st.connected_at.map_or(JValue::Null, JValue::Num),
        ),
    ];
    if let Some(c) = &st.user_code {
        out.push(("userCode", s(c)));
    }
    if let Some(u) = &st.verification_url {
        out.push(("verificationUrl", s(u)));
    }
    if let Some(e) = st.expires_at {
        out.push(("expiresAt", JValue::Num(e as f64)));
    }
    if let Some(m) = &st.message {
        out.push(("message", s(m)));
    }
    out.push(("backup", backup));
    out.push(("tools", tools));
    Ok(JValue::obj(out))
}

pub(crate) async fn read_object(body: Body) -> Result<JValue, Box<Response<Body>>> {
    let mut body = body;
    let read = async {
        let mut out = Vec::new();
        while let Some(frame) = body.frame().await {
            let Ok(frame) = frame else {
                return Err(StatusCode::BAD_REQUEST);
            };
            if let Ok(data) = frame.into_data() {
                if out.len() + data.len() > BODY_LIMIT {
                    return Err(StatusCode::PAYLOAD_TOO_LARGE);
                }
                out.extend_from_slice(&data);
            }
        }
        Ok(out)
    };
    let bytes = match tokio::time::timeout(BODY_DEADLINE, read).await {
        Ok(Ok(b)) => b,
        Ok(Err(StatusCode::PAYLOAD_TOO_LARGE)) => {
            return Err(Box::new(reply::error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "Request exceeds size limit",
                true,
            )))
        }
        _ => {
            return Err(Box::new(reply::error(
                StatusCode::BAD_REQUEST,
                "invalid JSON",
                true,
            )))
        }
    };
    if bytes.is_empty() {
        return Ok(JValue::Obj(Vec::new()));
    }
    match js_json::parse(&String::from_utf8_lossy(&bytes)) {
        Ok(v @ JValue::Obj(_)) => Ok(v),
        Ok(_) => Err(Box::new(reply::error(
            StatusCode::BAD_REQUEST,
            "request body must be a JSON object",
            true,
        ))),
        Err(_) => Err(Box::new(reply::error(
            StatusCode::BAD_REQUEST,
            "invalid JSON",
            true,
        ))),
    }
}

/// `[].concat(body.tools || [])` filtered to the names `offered` accepts.
pub(crate) fn wanted_tools(body: &JValue, offered: impl Fn(&str) -> bool) -> Vec<String> {
    let tools = body.get("tools");
    let items: Vec<&JValue> = match tools {
        JValue::Arr(a) => a.iter().collect(),
        v if v.truthy() => vec![v],
        _ => Vec::new(),
    };
    items
        .into_iter()
        .filter_map(|t| t.as_str())
        .filter(|t| offered(t))
        .map(str::to_owned)
        .collect()
}

/// The write flag `set()` checks per tool: the caller's, for `allow` (the only mode a write may not take),
/// none otherwise. Flags that do not line up with the tools are not trusted: every tool counts as a write, so
/// `allow` is refused rather than granted on a guess.
fn write_flags(mode: &JValue, tools: usize, writes: &[bool]) -> Vec<bool> {
    let flags: Vec<bool> = if mode.as_str() == Some("allow") {
        writes.to_vec()
    } else {
        vec![false; tools]
    };
    if flags.len() == tools {
        flags
    } else {
        vec![true; tools]
    }
}

/// tool-policy.cjs `set()`: the checks in core's order, then the upserts and the audit row in one
/// transaction. `writes` are the caller's write flags per tool (only read for `allow`).
pub(crate) fn set_policy(
    app: &App,
    user: &str,
    tools: &[String],
    mode: &JValue,
    writes: &[bool],
) -> Result<(), Fail> {
    let units: Option<Vec<u16>> = mode.as_str().map(|m| m.encode_utf16().collect());
    let flags = write_flags(mode, tools.len(), writes);
    let stored = policy_leaves::check_set(units.as_deref(), &flags)
        .map_err(|r| Fail::new(r.message(), 400))?;
    let writer = app
        .writes
        .writer()
        .map_err(|_| Fail::new(POLICY_UNSAVED, 502))?;
    let now = now_ms();
    let detail = js_json::stringify(&JValue::obj([
        ("tools", JValue::Arr(tools.iter().map(|t| s(t)).collect())),
        ("mode", s(stored.name())),
    ]))
    .unwrap_or_else(|| "{}".into());
    writer
        .write(|tx| {
            for t in tools {
                tx.upsert_tool_policy(server_store::RustToolPolicies::owned(), user, t, stored.name(), now)?;
            }
            tx.execute(
                "INSERT INTO audit_events(actor_user_id,target_user_id,action,detail,created_at) VALUES(?1,?1,?2,?3,?4)",
                server_store::rusqlite::params![user, "tool.policy", detail, now],
            )?;
            Ok(())
        })
        .map_err(|_| Fail::new(POLICY_UNSAVED, 502))
}

/// Why a connector request stopped: a failure with Node's words, or a ready refusal.
enum Stop {
    Fail(Fail),
    Reply(Box<Response<Body>>),
}

impl From<Fail> for Stop {
    fn from(e: Fail) -> Stop {
        Stop::Fail(e)
    }
}

pub async fn serve(
    app: &Arc<App>,
    conn: crate::serve::Conn,
    req: Request<Body>,
    path: String,
) -> Response<Body> {
    if (path == LIST && req.method() == Method::GET)
        || (path == NEXTCLOUD_POLICY && req.method() == Method::PUT)
    {
        #[cfg(feature = "rust-storage-reads")]
        {
            return crate::nextcloud_routes::serve(app, req, path).await;
        }
        #[cfg(not(feature = "rust-storage-reads"))]
        {
            // The storage features carry the connection this page reads: without them Node's refusal (503) answers.
            return app.proxy.forward(conn.peer, req).await;
        }
    }
    let Some(drive) = gdrive::of(app) else {
        return app.proxy.forward(conn.peer, req).await;
    };
    let method = req.method().clone();
    if !(ROUTES.contains(&path.as_str())
        && ((method == Method::POST && (path == CONNECT || path == DISCONNECT))
            || (method == Method::PUT && (path == POLICY || path == BACKUP_COPY))))
    {
        return app.proxy.forward(conn.peer, req).await;
    }
    let (parts, body) = req.into_parts();
    let creds = identity::creds_of(&parts);
    let gate = Arc::clone(&app.identity);
    let now = now_ms();
    let gate_path = path.clone();
    let id = match tokio::task::spawn_blocking(move || gate.gate_blocking(&creds, &gate_path, now))
        .await
    {
        Ok(Ok(id)) => id,
        Ok(Err(rejection)) => return rejection.into_response(),
        Err(_) => return identity::Rejection::Unavailable.into_response(),
    };
    let user = id.user.id.clone();
    let admin = id.user.role == server_auth::Role::Admin;
    let result: Result<(), Stop> = async {
        match path.as_str() {
            CONNECT => {
                let slot = drive.slot_for(&user, admin)?;
                if slot == Slot::Backup {
                    let service = app
                        .offsite
                        .as_ref()
                        .ok_or_else(|| Fail::new(UNREACHABLE, 502))?;
                    service.connect_google(&user, None).await?;
                } else {
                    drive.connect(slot, Some(user.clone()), None).await?;
                }
            }
            DISCONNECT => {
                let slot = drive.slot_for(&user, admin)?;
                drive.disconnect(slot).await?;
            }
            POLICY => {
                let body = read_object(body).await.map_err(Stop::Reply)?;
                let names: Vec<&str> = TOOLS.iter().map(|(n, _)| *n).collect();
                let tools = wanted_tools(&body, |t| names.contains(&t));
                let writes: Vec<bool> = tools.iter().map(|t| is_write(t)).collect();
                let (a, u, mode) = (Arc::clone(app), user.clone(), body.get("mode").clone());
                tokio::task::spawn_blocking(move || set_policy(&a, &u, &tools, &mode, &writes))
                    .await
                    .map_err(|_| Fail::new(POLICY_UNSAVED, 502))??;
            }
            _ => {
                if drive.slot_for(&user, admin)? != Slot::Backup {
                    return Err(Fail::new(
                        "Only the administrator whose Drive holds the backups can change this.",
                        403,
                    )
                    .into());
                }
                let body = read_object(body).await.map_err(Stop::Reply)?;
                let service = app
                    .offsite
                    .as_ref()
                    .ok_or_else(|| Fail::new(UNREACHABLE, 502))?;
                service
                    .set_drive_copy(body.get("enabled").as_bool() == Some(true))
                    .await;
            }
        }
        Ok(())
    }
    .await;
    match result {
        Ok(()) => match view(app, &user, admin).await {
            Ok(v) => answer(200, &v),
            Err(e) => failed(&e),
        },
        // A body that could not be read keeps its own refusal (400/413 with its words).
        Err(Stop::Reply(r)) => *r,
        Err(Stop::Fail(e)) => failed(&e),
    }
}

// ---------------------------------------------------------------------------------------------
// The internal operations.

fn internal(status: u16, body: &JValue) -> Response<Body> {
    let mut res = Response::new(Body::from(js_json::stringify(body).unwrap_or_default()));
    *res.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    res.headers_mut().insert(
        "content-type",
        axum::http::HeaderValue::from_static("application/json"),
    );
    res.headers_mut().insert(
        "cache-control",
        axum::http::HeaderValue::from_static("no-store"),
    );
    res
}

fn internal_fail(e: &Fail) -> Response<Body> {
    let mut pairs = vec![("error", JValue::Str(e.message.clone()))];
    if let Some(code) = e.code {
        pairs.push(("code", s(code)));
    }
    pairs.push(("status", JValue::Num(f64::from(e.status))));
    internal(
        if (400..600).contains(&e.status) {
            e.status
        } else {
            502
        },
        &JValue::obj(pairs),
    )
}

/// Whether `users.role` of this account is `admin` right now: `Ok(false)` for a member and for an
/// id the table does not know, `Err` (503) when the table cannot be read. An unreadable table is
/// not an answer: calling the caller a member would send an administrator to their own Drive
/// instead of the backup's, and the request would look like it worked.
async fn is_admin(app: &Arc<App>, user: &str) -> Result<bool, Fail> {
    let (a, u) = (Arc::clone(app), user.to_owned());
    let unreadable = || Fail::new("The account could not be checked. Try again shortly.", 503);
    tokio::task::spawn_blocking(move || {
        let writer = a.writes.writer().map_err(|_| ())?;
        let role = writer
            .read(|r| {
                r.row("SELECT role FROM users WHERE id=?1", [u.as_str()], |row| {
                    row.get::<_, String>(0)
                })
            })
            .map_err(|_| ())?;
        Ok::<bool, ()>(role.is_some_and(|role| role == "admin"))
    })
    .await
    .map_err(|_| unreadable())?
    .map_err(|()| unreadable())
}

/// `POST /v1/gdrive/<op>` after the store secret was checked.
pub async fn internal_op(app: Option<Arc<App>>, op: &str, raw: &[u8]) -> Response<Body> {
    let unavailable = || internal(503, &JValue::obj([("error", s("unavailable"))]));
    let Some(app) = app else { return unavailable() };
    let Some(drive) = gdrive::of(&app) else {
        return unavailable();
    };
    let body = match js_json::parse(&String::from_utf8_lossy(raw)) {
        Ok(b @ JValue::Obj(_)) => b,
        _ => return internal(400, &JValue::obj([("error", s("invalid JSON"))])),
    };
    if op == "reseal" {
        // secrets:rotate: every account's token file under the retired key is sealed under the
        // current one, here, under the same fence as every other change to those files. Counts only.
        let d = Arc::clone(&drive);
        return match tokio::task::spawn_blocking(move || d.reseal_users()).await {
            Ok(Ok(r)) => internal(
                200,
                &JValue::obj([
                    ("current", JValue::Num(r.current as f64)),
                    ("rotated", JValue::Num(r.rotated as f64)),
                    ("empty", JValue::Num(r.empty as f64)),
                    ("failed", JValue::Num(r.failed as f64)),
                ]),
            ),
            Ok(Err(e)) => internal_fail(&e),
            Err(_) => internal_fail(&Fail::new("The re-seal did not finish.", 502)),
        };
    }
    let user = match body.get("user") {
        JValue::Str(u) if !u.is_empty() && u.len() <= 256 => u.clone(),
        _ => return internal(400, &JValue::obj([("error", s("invalid user"))])),
    };
    // Node says whether the account is an administrator; a claim of `true` is checked against the
    // users table, so a mistaken or forged request cannot reach the backup slot (the one Drive that
    // is not the account's own). Anything short of a confirmed administrator is a member.
    let admin = if body.get("admin").as_bool() == Some(true) {
        match is_admin(&app, &user).await {
            Ok(admin) => admin,
            Err(e) => return internal_fail(&e),
        }
    } else {
        false
    };
    drive.publish_once();
    match op {
        "session" => {
            let slot = match drive.slot_for(&user, admin) {
                Ok(s) => s,
                Err(e) => return internal_fail(&e),
            };
            let rejected = body.get("rejected").as_str().map(str::to_owned);
            match drive.token(&slot, rejected.as_deref()).await {
                Ok((token, until, generation)) => internal(
                    200,
                    &JValue::obj([
                        ("accessToken", JValue::Str(token.to_string())),
                        ("expiresAt", JValue::Num(until as f64)),
                        ("generation", JValue::Str(generation)),
                    ]),
                ),
                Err(e) => internal_fail(&e),
            }
        }
        "reject" => {
            let generation = body.get("generation").as_str().unwrap_or("");
            let dropped = drive
                .slot_for(&user, admin)
                .map(|slot| drive.reject(&slot, generation))
                .unwrap_or(false);
            internal(200, &JValue::obj([("dropped", JValue::Bool(dropped))]))
        }
        "disconnect" => match drive.slot_for(&user, admin) {
            Ok(slot) => match drive.disconnect(slot).await {
                Ok(st) => internal(
                    200,
                    &JValue::obj([("ok", JValue::Bool(true)), ("state", s(st.state))]),
                ),
                Err(e) => internal_fail(&e),
            },
            Err(e) => internal_fail(&e),
        },
        _ => internal(404, &JValue::obj([("error", s("not found"))])),
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
mod tests {
    use super::*;

    /// #2241: the write flags and the mismatch rule of `set_policy`, before any account or database.
    #[test]
    fn allow_is_refused_for_a_write_and_for_flags_that_do_not_line_up_with_the_tools() {
        let units = |m: &str| m.encode_utf16().collect::<Vec<u16>>();
        let check = |mode: &str, tools: usize, writes: &[bool]| {
            let flags = write_flags(&JValue::Str(mode.into()), tools, writes);
            policy_leaves::check_set(Some(&units(mode)), &flags).map(|m| m.name().to_owned())
        };
        // Reads may be allowed; a write may not; the other modes take either.
        assert_eq!(check("allow", 2, &[false, false]), Ok("allow".to_owned()));
        assert!(
            check("allow", 2, &[false, true]).is_err(),
            "one write refuses the call"
        );
        assert_eq!(check("ask", 2, &[true, true]), Ok("ask".to_owned()));
        assert_eq!(check("block", 1, &[true]), Ok("block".to_owned()));
        // Fewer or more flags than tools: counted as writes, so `allow` is refused ...
        assert!(check("allow", 2, &[false]).is_err());
        assert!(check("allow", 1, &[false, false]).is_err());
        assert!(check("allow", 2, &[]).is_err());
        // ... and the same mismatch costs `ask` and `block` nothing; no tools at all is refused.
        assert_eq!(check("ask", 2, &[false]), Ok("ask".to_owned()));
        assert_eq!(check("block", 2, &[]), Ok("block".to_owned()));
        assert!(check("ask", 0, &[]).is_err());
        // A mode that is not text is refused whatever the flags.
        assert!(
            policy_leaves::check_set(None, &write_flags(&JValue::Num(1.0), 1, &[false])).is_err()
        );
    }

    #[test]
    fn the_tool_list_is_the_file_core_checks_its_own_against() {
        let doc =
            js_json::parse(include_str!("../tests/fixtures/gdrive-tools-list.v1.json")).unwrap();
        let JValue::Arr(tools) = doc.get("tools") else {
            panic!("tools")
        };
        assert_eq!(tools.len(), TOOLS.len());
        for (listed, (name, label)) in tools.iter().zip(TOOLS) {
            assert_eq!(listed.get("name").as_str(), Some(name));
            assert_eq!(listed.get("label").as_str(), Some(label));
            assert_eq!(
                listed.get("write").as_bool(),
                Some(is_write(name)),
                "{name}"
            );
        }
    }

    #[test]
    fn every_translatable_message_id_the_files_name_is_sent_by_this_front() {
        // core's tests/server/locale-624-remaining.test.cjs proves each id has a catalogue key in every
        // language; this proves the front sends exactly those ids, with those words and params.
        let doc =
            js_json::parse(include_str!("../tests/fixtures/gdrive-messages.v1.json")).unwrap();
        let JValue::Arr(messages) = doc.get("messages") else {
            panic!("messages")
        };
        let sources = [include_str!("offsite_mirror.rs"), include_str!("gdrive.rs")].concat();
        let mut ids: Vec<&str> = Vec::new();
        for m in messages {
            let id = m.get("id").as_str().unwrap();
            ids.push(id);
            assert!(sources.contains(&format!("\"{id}\"")), "{id} is sent");
            // The English, with {name} for a parameter, is the format string the front builds.
            let english = m.get("english").as_str().unwrap();
            let mut probe = english.to_owned();
            if let JValue::Arr(params) = m.get("params") {
                for p in params {
                    probe = probe.replace(&format!("{{{}}}", p.as_str().unwrap()), "\u{1}");
                }
            }
            for piece in probe.split('\u{1}').filter(|p| p.len() > 12) {
                assert!(sources.contains(piece), "{id}: {piece}");
            }
        }
        ids.sort_unstable();
        assert_eq!(
            ids,
            [
                "differentStore",
                "driveAnswered",
                "fileDiffers",
                "staleShare",
                "storeEmpty",
                "tooManyStale",
                "waitingFirstBackup"
            ]
        );
    }
}
