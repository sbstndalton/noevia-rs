//! `GET|PUT /api/chat-vault-mirror/preferences` (sbstndalton/noevia#2190 round 3), ported from core
//! routes/chat-vault-mirror.cjs and chat-vault-mirror.cjs `readPreferences`/`writePreferences`:
//! "Mirror chats to Diary", the signed-in account's own setting. Rust is the only writer of
//! `users/<id>/chat-vault-mirror.json`; Node's mirror reads it on every use (`readPreferences`).
//!
//! Node answered a PUT that turned the mirror on (while it is available) by scheduling the
//! account's mirror at once. The mirror stays in Node, so a save here that turns it on writes the
//! nudge Node's mirror polls: `UI_DATA_DIR/chat-vault-mirror-nudges.json`,
//! `{version:1,nudges:{<userId>:<epoch ms>}}` (core rust-vault-mirror-preferences.cjs re-reads it
//! by its file signal and runs that account's mirror immediately). Node's mirror applies its own
//! `enabled(userId)` gate before it writes anything, as it did before.
//!
//! Order follows core: the session gate, 405 for other methods, the body (PUT only: 1 MiB 413,
//! `invalid JSON` 400), the validation (`enabled must be true or false`), then the save with its
//! `chat-vault-mirror.preferences` audit row (actor = target, `{"enabled":…}`) under the same
//! IMMEDIATE writer fence as the account routes. `available` is Node's `vaultMirrorAvailable`:
//! the chatFraming feature as Node's `features.enabled` answers it, and the account's Diary add-on.
use crate::{files, Account, Fault, Outcome, Reply, Request, BODY_LIMIT};
use js_json::JValue;
use server_auth::Identity;
use server_features::Feature;
use server_store::{json, Reader, StoreError};
use std::path::Path;
use std::sync::Mutex;

pub const PATH: &str = "/api/chat-vault-mirror/preferences";
const FILE: &str = "chat-vault-mirror.json";
/// The nudges Node's mirror polls (in UI_DATA_DIR).
pub const NUDGES_FILE: &str = "chat-vault-mirror-nudges.json";
const INVALID: &str = "enabled must be true or false";

/// One writer of the nudge file in this process.
static NUDGES: Mutex<()> = Mutex::new(());

/// `readPreferences(dir)`: `{enabled: data?.enabled === true}`; a missing or unreadable file is off.
fn read(dir: &Path) -> bool {
    std::fs::read(dir.join(FILE))
        .ok()
        .and_then(|bytes| {
            crate::framing_preferences::project_booleans(
                &String::from_utf8_lossy(&bytes),
                &["enabled"],
            )
            .ok()
        })
        .is_some_and(|v| v.get("enabled") == &JValue::Bool(true))
}

/// The PUT body as Node's `readJson` gives it to `writePreferences`: only a literal boolean
/// `enabled` counts; `{}` for an empty body.
fn body(req: &Request) -> Result<JValue, Outcome> {
    if req.body.over || req.body.bytes.len() > BODY_LIMIT {
        return Err(Reply::error(413, "Request exceeds size limit").into());
    }
    let raw = String::from_utf8_lossy(&req.body.bytes);
    let text = if raw.is_empty() { "{}" } else { raw.as_ref() };
    crate::framing_preferences::project_booleans(text, &["enabled"])
        .map_err(|()| Reply::error(400, "invalid JSON").into())
}

/// Node's `vaultMirrorAvailable(userId)`: `features.enabled('chatFraming') && diaryEnabled(userId)`,
/// in that order. `Err` is Node's throw from `features.enabled` while the feature runtime context
/// is unavailable (its 503, answered by the route's catch with the message only).
fn available(acct: &Account, reader: &Reader<'_>, user: &str) -> Result<bool, StoreError> {
    let Some(controller) = acct.feature_context.as_ref() else {
        return Err(StoreError::NotReady("feature context".into()));
    };
    let view = controller.view(reader)?;
    if !crate::features::evaluate(reader, &view, Feature::ChatFraming)?.enabled {
        return Ok(false);
    }
    Ok(reader
        .row(
            "SELECT diary_enabled FROM user_features WHERE user_id=?1",
            [user],
            |r| r.get::<_, Option<i64>>(0),
        )?
        .flatten()
        .is_some_and(|v| v != 0))
}

fn context_unavailable() -> Outcome {
    Reply::error(503, "Feature runtime context is unavailable.").into()
}

/// Records that `user` turned the mirror on, for Node's mirror to run at once. Best effort: a
/// failure is logged with no detail and never changes the answer (the mirror still runs on its
/// next scheduled pass).
fn nudge(data_dir: &Path, user: &str, now: i64) {
    let _guard = NUDGES.lock().unwrap_or_else(|p| p.into_inner());
    let file = data_dir.join(NUDGES_FILE);
    let mut nudges = std::fs::read(&file)
        .ok()
        .and_then(|b| js_json::parse(&String::from_utf8_lossy(&b)).ok())
        .map(|doc| match doc.get("nudges") {
            JValue::Obj(items) => items.clone(),
            _ => Vec::new(),
        })
        .unwrap_or_default();
    js_json::wtf16::set(&mut nudges, user, JValue::Num(now as f64));
    let doc = JValue::obj([
        ("version", JValue::Num(1.0)),
        ("nudges", JValue::Obj(nudges)),
    ]);
    let written = js_json::stringify(&doc)
        .ok_or(())
        .and_then(|text| write_private(&file, &text).map_err(|_| ()));
    if written.is_err() {
        eprintln!("noevia-server: chat vault mirror nudge could not be written");
    }
}

fn write_private(file: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt;
    let mut tmp = file.as_os_str().to_owned();
    tmp.push(format!(".{}.tmp", std::process::id()));
    let tmp = std::path::PathBuf::from(tmp);
    let mut out = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    out.write_all(text.as_bytes())?;
    out.sync_all()?;
    drop(out);
    std::fs::rename(&tmp, file)
}

fn view(enabled: bool, available: bool) -> JValue {
    JValue::obj([
        ("enabled", JValue::Bool(enabled)),
        ("available", JValue::Bool(available)),
    ])
}

pub(crate) fn handle(
    acct: &Account,
    req: &Request,
    authn: &Identity,
    current_time: &dyn Fn() -> i64,
) -> Result<Option<Outcome>, Fault> {
    if req.path != PATH {
        return Ok(None);
    }
    if !matches!(req.method.as_str(), "GET" | "PUT") {
        return Ok(Some(Reply::error(405, "method not allowed").into()));
    }
    let dir = files::user_dir(acct, &authn.user.id)?;
    let put = if req.method == "PUT" {
        match body(req) {
            Ok(b) => Some(b),
            Err(o) => return Ok(Some(o)),
        }
    } else {
        None
    };
    let mut turned_on = false;
    let result = acct.writer.write(|tx| {
        let reader = tx.reader();
        let current = match acct
            .auth
            .gate_in(&reader, &req.creds, &req.path, current_time())?
        {
            Ok(current) if current.user.id == authn.user.id => current,
            Ok(_) => {
                return Ok(Ok(crate::framing_preferences::refusal(
                    server_auth::identity::Refusal::Unauthorized,
                )))
            }
            Err(why) => return Ok(Ok(crate::framing_preferences::refusal(why))),
        };
        if let Err(error) = crate::framing_preferences::ensure_workspace(acct, &current, &dir) {
            return Ok(Err(error));
        }
        if let Err(why) = acct
            .auth
            .gate_in(&reader, &req.creds, &req.path, current_time())?
        {
            return Ok(Ok(crate::framing_preferences::refusal(why)));
        }
        let user = current.user.id.as_str();
        let Some(put) = put.as_ref() else {
            let on = read(&dir);
            return Ok(Ok(match available(acct, &reader, user) {
                Ok(open) => Reply::json(200, &view(on, open)).into(),
                Err(_) => context_unavailable(),
            }));
        };
        let JValue::Bool(enabled) = put.get("enabled") else {
            return Ok(Ok(Reply::error(400, INVALID).into()));
        };
        let record = JValue::obj([("enabled", JValue::Bool(*enabled))]);
        let text =
            js_json::stringify(&record).ok_or_else(|| StoreError::NotReady("json".into()))?;
        if json::write_owned_text(&dir, FILE, &text, acct.switch).is_err() {
            return Ok(Ok(Reply::error(400, "Invalid mirror preferences").into()));
        }
        crate::users::audit(
            tx,
            "chat-vault-mirror.preferences",
            Some(user),
            Some(user),
            &record,
            current_time(),
        )?;
        // Node saved and audited before it asked for `available`: a refusal there still commits.
        let Ok(open) = available(acct, &reader, user) else {
            return Ok(Ok(context_unavailable()));
        };
        turned_on = *enabled && open;
        Ok(Ok(Reply::json(200, &view(*enabled, open)).into()))
    })?;
    if turned_on {
        nudge(&acct.settings.data_dir, &authn.user.id, current_time());
    }
    result.map(Some)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn read_is_core_read_preferences() {
        let d = tempfile::tempdir().unwrap();
        assert!(!read(d.path()));
        for (text, on) in [
            (r#"{"enabled":true}"#, true),
            (r#"{"enabled":false}"#, false),
            (r#"{"enabled":"true"}"#, false),
            (r#"{"enabled":1}"#, false),
            ("not json", false),
            ("[true]", false),
            (r#"{"x":{"enabled":true}}"#, false),
        ] {
            std::fs::write(d.path().join(FILE), text).unwrap();
            assert_eq!(read(d.path()), on, "{text}");
        }
    }

    #[test]
    fn nudges_keep_other_accounts_and_replace_the_time() {
        let d = tempfile::tempdir().unwrap();
        nudge(d.path(), "a", 5);
        nudge(d.path(), "b", 6);
        nudge(d.path(), "a", 7);
        let text = std::fs::read_to_string(d.path().join(NUDGES_FILE)).unwrap();
        assert_eq!(text, r#"{"version":1,"nudges":{"a":7,"b":6}}"#);
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(d.path().join(NUDGES_FILE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
