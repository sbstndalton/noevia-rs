//! core index.cjs's legacy WebDAV storage setup (track R14 step 2), run by the front for the
//! account of a signed-in request, behind `NOEVIA_RUST_LEGACY_WEBDAV=1` (read once; unset = only
//! Node's request gate runs it, as today).
//!
//! Core, at the start of every signed-in request it handles: an account signed in as `admin`
//! (`preAuth.user.role`: a device token signs in as a member, device-auth.cjs), with the Diary
//! add-on on, whose workspace holds
//! `migration.json` (a legacy single-user install migrated in), on a deployment with
//! `CORPUS_BACKEND=webdav`, and whose storage is `local` (no row, or a local one) gets a WebDAV
//! row from `WEBDAV_BASE_URL` (trailing slashes cut), `WEBDAV_USERNAME`, `WEBDAV_PASSWORD` and
//! `CORPUS_ROOT`, saved by the front (`project_store.rs` `/v1/storage/legacy-webdav`). A failure
//! leaves it for the next request. The row is written only while the account still has none or a
//! local one, checked inside the write ([`When::Local`]), so Node's gate and this one running for
//! the same request write it once.
//!
//! Differences: a request carrying any `Authorization: Bearer` header is skipped (a device token is
//! a member in core; core's legacy bearer is not an account the front's upkeep names, so core's own
//! gate still runs the setup for it). An account whose row is configured (or was saved here) is not
//! looked at again by this process, also if Settings later saves it back to `local` (core would run
//! the setup again on its next request; core's gate still does while it runs); the other accounts
//! are looked at at most once a minute each, not on every request. Column values are read as core
//! reads them (`!!diary_enabled`, `role === 'admin'`, a row of any kind but the string `local` is
//! configured). The password is never logged; a failure is reported once a minute without detail.
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use server_store::rusqlite::types::Value;

use crate::storage_integration_routes::{write_row, Row, When};
use crate::App;

pub const SWITCH: &str = "NOEVIA_RUST_LEGACY_WEBDAV";
const AGAIN_AFTER: Duration = Duration::from_secs(60);

/// Whether the switch is on (`1`) and the deployment keeps its corpus on WebDAV, read once.
pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var(SWITCH).as_deref() == Ok("1")
            && std::env::var("CORPUS_BACKEND").as_deref() == Ok("webdav")
    })
}

/// When each account was last looked at; `None` = settled (its row is configured).
fn seen() -> &'static Mutex<HashMap<String, Option<Instant>>> {
    static SEEN: OnceLock<Mutex<HashMap<String, Option<Instant>>>> = OnceLock::new();
    SEEN.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Whether `user` is due a look now (and marks it looked at).
fn due(user: &str) -> bool {
    let Ok(mut seen) = seen().lock() else {
        return false;
    };
    match seen.get(user) {
        Some(None) => false,
        Some(Some(at)) if at.elapsed() < AGAIN_AFTER => false,
        _ => {
            if seen.len() >= 4096 {
                seen.retain(|_, v| v.is_none());
            }
            seen.insert(user.to_owned(), Some(Instant::now()));
            true
        }
    }
}

fn settle(user: &str) {
    if let Ok(mut seen) = seen().lock() {
        seen.insert(user.to_owned(), None);
    }
}

/// Whether the request carries an `Authorization: Bearer` credential (device-auth.cjs `bearerToken`'s
/// header form): such a request is never an administrator's in core.
pub fn bearer(authorization: Option<&[u8]>) -> bool {
    authorization.is_some_and(|h| {
        let h = String::from_utf8_lossy(h);
        let h = h.trim_start();
        h.get(..6).is_some_and(|p| p.eq_ignore_ascii_case("bearer"))
            && h.get(6..)
                .is_some_and(|r| r.starts_with(char::is_whitespace))
    })
}

/// JS truthiness of a column value (`!!value`).
fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Integer(i) => *i != 0,
        Value::Real(f) => *f != 0.0 && !f.is_nan(),
        Value::Text(t) => !t.is_empty(),
        Value::Blob(_) => true,
    }
}

fn text(v: &Value) -> Option<&str> {
    match v {
        Value::Text(t) => Some(t),
        _ => None,
    }
}

/// The facts as core reads them: `role === 'admin'`, `!!diary_enabled`, and the storage kind (`None`:
/// no row; a row whose kind is not the string `local`, `null` included, counts as configured).
pub fn facts(
    role: Option<&Value>,
    diary: Option<&Value>,
    kind: Option<&Value>,
) -> (bool, bool, Option<bool>) {
    (
        role.and_then(text) == Some("admin"),
        diary.is_some_and(truthy),
        kind.map(|k| text(k) == Some("local")),
    )
}

/// What the account's facts say.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Not this account's setup (yet): look again later.
    No,
    /// The account's storage is configured: never again.
    Configured,
    /// Save the row.
    Save,
}

/// The preconditions, from [`facts`] and whether `users/<id>/migration.json` exists (`local`:
/// `None` no row, `Some(true)` a local one, `Some(false)` a configured one).
pub fn verdict(admin: bool, diary: bool, migration: bool, local: Option<bool>) -> Verdict {
    if local == Some(false) {
        return Verdict::Configured;
    }
    if admin && diary && migration {
        Verdict::Save
    } else {
        Verdict::No
    }
}

fn warn() {
    static LAST: Mutex<Option<Instant>> = Mutex::new(None);
    if let Ok(mut last) = LAST.lock() {
        if last.is_some_and(|t| t.elapsed() < AGAIN_AFTER) {
            return;
        }
        *last = Some(Instant::now());
    }
    eprintln!("noevia-server: legacy WebDAV storage setup was not saved; trying again later");
}

/// For the account of a signed-in request (blocking; never fails the request).
pub fn nudge(app: &App, user: &str) {
    if !enabled() || !due(user) {
        return;
    }
    let Some(data_dir) = app.config.data_dir.clone() else {
        return;
    };
    let Ok(writer) = app.writes.writer() else {
        return warn();
    };
    let read = writer.read(|r| {
        let one = |sql: &str| r.row(sql, [user], |row| row.get::<_, Value>(0));
        Ok((
            one("SELECT role FROM users WHERE id=?1")?,
            one("SELECT diary_enabled FROM user_features WHERE user_id=?1")?,
            one("SELECT kind FROM storage_connections WHERE user_id=?1")?,
        ))
    });
    let Ok((role, diary, kind)) = read else {
        return warn();
    };
    let (admin, diary, local) = facts(role.as_ref(), diary.as_ref(), kind.as_ref());
    let migration = server_projects::store::user_dir(&data_dir, user)
        .is_ok_and(|dir| dir.join("migration.json").exists());
    match verdict(admin, diary, migration, local) {
        Verdict::No => {}
        Verdict::Configured => settle(user),
        Verdict::Save => {
            let env = |k: &str| std::env::var(k).unwrap_or_default();
            let row = Row {
                kind: "webdav",
                base_url: env("WEBDAV_BASE_URL").trim_end_matches('/').to_owned(),
                bucket: String::new(),
                region: s3_sign::DEFAULT_REGION.to_owned(),
                username: env("WEBDAV_USERNAME"),
                secret: zeroize::Zeroizing::new(env("WEBDAV_PASSWORD")),
                corpus_root: env("CORPUS_ROOT"),
            };
            let previous = std::env::var("SECRETS_KEY_PREVIOUS")
                .ok()
                .map(zeroize::Zeroizing::new);
            let Ok(keys) = server_storage_reads::owner_secret::load_existing_keys(
                &data_dir,
                previous.as_deref().map(String::as_str),
            ) else {
                return warn();
            };
            match write_row(app, &keys, user, &row, When::Local) {
                // Saved now, or another writer configured it first.
                Ok(_) => settle(user),
                Err(_) => warn(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_admin_with_the_diary_and_a_migration_on_local_storage_is_set_up() {
        use Verdict::*;
        let t = |s: &str| Value::Text(s.into());
        for (role, diary, kind, migration, want) in [
            (Some(t("admin")), Some(Value::Integer(1)), None, true, Save),
            (
                Some(t("admin")),
                Some(Value::Integer(1)),
                Some(t("local")),
                true,
                Save,
            ),
            (Some(t("admin")), Some(Value::Real(1.0)), None, true, Save),
            (Some(t("admin")), Some(t("1")), None, true, Save),
            (Some(t("admin")), Some(t("0")), None, true, Save),
            (
                Some(t("admin")),
                Some(Value::Integer(1)),
                Some(t("webdav")),
                true,
                Configured,
            ),
            (
                Some(t("admin")),
                Some(Value::Integer(1)),
                Some(Value::Null),
                true,
                Configured,
            ),
            (
                Some(t("member")),
                Some(Value::Integer(1)),
                Some(t("nextcloud")),
                true,
                Configured,
            ),
            (Some(t("member")), Some(Value::Integer(1)), None, true, No),
            (Some(t("admin")), Some(Value::Integer(0)), None, true, No),
            (Some(t("admin")), Some(Value::Real(0.0)), None, true, No),
            (Some(t("admin")), Some(t("")), None, true, No),
            (Some(t("admin")), Some(Value::Null), None, true, No),
            (Some(t("admin")), None, None, true, No),
            (Some(t("admin")), Some(Value::Integer(1)), None, false, No),
            (None, Some(Value::Integer(1)), None, true, No),
            (Some(Value::Null), Some(Value::Integer(1)), None, true, No),
            (Some(t("Admin")), Some(Value::Integer(1)), None, true, No),
        ] {
            let (admin, diary_on, local) = facts(role.as_ref(), diary.as_ref(), kind.as_ref());
            assert_eq!(
                verdict(admin, diary_on, migration, local),
                want,
                "{role:?} {diary:?} {kind:?} {migration}"
            );
        }
    }

    #[test]
    fn a_bearer_request_is_never_an_administrators() {
        for (h, want) in [
            (Some(&b"Bearer nva_abc"[..]), true),
            (Some(&b"bearer   nva_abc"[..]), true),
            (Some(&b"  BEARER\tx"[..]), true),
            (Some(&b"Bearer"[..]), false),
            (Some(&b"Bearerx y"[..]), false),
            (Some(&b"Basic abc"[..]), false),
            (None, false),
        ] {
            assert_eq!(bearer(h), want, "{h:?}");
        }
    }

    #[test]
    fn an_account_is_looked_at_once_a_minute_and_never_once_settled() {
        let user = "aaaaaaaa-0000-4000-8000-000000000001";
        assert!(due(user));
        assert!(!due(user));
        settle(user);
        assert!(!due(user));
    }
}
