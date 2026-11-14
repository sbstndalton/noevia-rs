//! core index.cjs's legacy WebDAV storage setup (track R14 step 2), run by the front for the
//! account of a signed-in request, behind `NOEVIA_RUST_LEGACY_WEBDAV=1` (read once; unset = only
//! Node's request gate runs it, as today).
//!
//! Core, at the start of every signed-in request it handles: an account whose own role is `admin`
//! (the user's role, not a device token's), with the Diary add-on on, whose workspace holds
//! `migration.json` (a legacy single-user install migrated in), on a deployment with
//! `CORPUS_BACKEND=webdav`, and whose storage is `local` (no row, or a local one) gets a WebDAV
//! row from `WEBDAV_BASE_URL` (trailing slashes cut), `WEBDAV_USERNAME`, `WEBDAV_PASSWORD` and
//! `CORPUS_ROOT`, saved by the front (`project_store.rs` `/v1/storage/legacy-webdav`). A failure
//! leaves it for the next request. The row is written only while the account still has none or a
//! local one, checked inside the write ([`When::Local`]), so Node's gate and this one running for
//! the same request write it once.
//!
//! Differences: an account whose row is configured (or was saved here) is not looked at again by
//! this process (it cannot become `local` but by a save from Settings, which the user makes); the
//! other accounts are looked at at most once a minute each, not on every request. The password is
//! never logged; a failure is reported once a minute without detail.
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

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

/// The preconditions, from what was read: the user's role, the Diary add-on, whether
/// `users/<id>/migration.json` exists, and the storage row's kind (`None`: no row).
pub fn verdict(role: Option<&str>, diary: bool, migration: bool, kind: Option<&str>) -> Verdict {
    if kind.is_some_and(|k| k != "local") {
        return Verdict::Configured;
    }
    if role == Some("admin") && diary && migration {
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
    let facts = writer.read(|r| {
        let role = r.row("SELECT role FROM users WHERE id=?1", [user], |row| {
            row.get::<_, String>(0)
        })?;
        let diary = r
            .row(
                "SELECT diary_enabled FROM user_features WHERE user_id=?1",
                [user],
                |row| row.get::<_, Option<i64>>(0),
            )?
            .flatten()
            .is_some_and(|v| v != 0);
        let kind = r.row(
            "SELECT kind FROM storage_connections WHERE user_id=?1",
            [user],
            |row| row.get::<_, String>(0),
        )?;
        Ok((role, diary, kind))
    });
    let Ok((role, diary, kind)) = facts else {
        return warn();
    };
    let migration = server_projects::store::user_dir(&data_dir, user)
        .is_ok_and(|dir| dir.join("migration.json").exists());
    match verdict(role.as_deref(), diary, migration, kind.as_deref()) {
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
        for (role, diary, migration, kind, want) in [
            (Some("admin"), true, true, None, Save),
            (Some("admin"), true, true, Some("local"), Save),
            (Some("admin"), true, true, Some("webdav"), Configured),
            (Some("member"), true, true, Some("nextcloud"), Configured),
            (Some("member"), true, true, None, No),
            (Some("admin"), false, true, None, No),
            (Some("admin"), true, false, None, No),
            (None, true, true, None, No),
            (Some("Admin"), true, true, None, No),
        ] {
            assert_eq!(
                verdict(role, diary, migration, kind),
                want,
                "{role:?} {diary} {migration} {kind:?}"
            );
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
