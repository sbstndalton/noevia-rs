//! The writes core's request gate makes as a side effect of `requestAuth.authenticate(req)`
//! (device-auth.cjs createRequestAuth over auth.cjs `authenticate` and device-auth.cjs
//! `authenticate`), for when Rust owns `sessions` and `device_grants` (M3, `NOEVIA_RUST_AUTH=1`):
//!
//! - a `cowork_session` cookie whose row is live: `UPDATE sessions SET last_seen_at=now`
//!   (`row && !row.disabled_at && row.expires_at > now && row.last_seen_at + IDLE_MS > now`);
//! - a `cowork_session` cookie whose row is not live or missing:
//!   `DELETE FROM sessions WHERE id_hash=digest(raw)`;
//! - with native-client sign-in on, a device access token (`Bearer nva_…`, no session cookie)
//!   that authenticates: `UPDATE device_grants SET last_used_at=now` when the last use is at least
//!   a minute old.
//!
//! The front runs this before every request it serves or proxies (Node, read-only for these
//! tables while the switch is on, no longer does), so idle expiry and Settings' "last used" stay
//! what they were. The conditions are Node's own, on the raw column values with JS comparison
//! semantics; a column of an unexpected type compares false, exactly where JS would also refuse,
//! so a row is never kept alive that Node would have deleted.

use crate::identity::{Authenticator, IDLE_MS};
use crate::js;
use crate::request::{self, Creds};
use server_store::rusqlite::types::Value;
use server_store::{StoreError, Writer};

/// device-auth.cjs: a grant's `last_used_at` is written at most once a minute.
pub const GRANT_TOUCH_MS: i64 = 60 * 1000;

/// What [`Authenticator::upkeep`] wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Upkept {
    /// Nothing to do (no credential, the legacy bearer, an unknown or refused device token, a
    /// grant used less than a minute ago, or both a bearer and a session cookie).
    Nothing,
    /// The session's `last_seen_at` moved to now.
    Touched,
    /// The cookie's session row was deleted (or was already gone: Node's DELETE runs either way).
    Deleted,
    /// The device grant's `last_used_at` moved to now.
    GrantTouched,
}

fn gt(a: Option<f64>, b: f64) -> bool {
    a.is_some_and(|a| a > b)
}

impl Authenticator {
    /// Node's per-request writes for `creds` at `now_ms`, in one write transaction.
    pub fn upkeep(&self, w: &Writer, creds: &Creds, now_ms: i64) -> Result<Upkept, StoreError> {
        w.write(|tx| {
            let r = tx.reader();
            let bearer = request::bearer_token(creds);
            if !bearer.is_empty() && self.native_client_auth(&r)? {
                if request::has_session_cookie(creds) {
                    return Ok(Upkept::Nothing);
                }
                return device_upkeep(tx, bearer, now_ms);
            }
            let Some(raw) = creds.cookie("cowork_session").filter(|v| !v.is_empty()) else {
                return Ok(Upkept::Nothing);
            };
            let id_hash = js::digest(&raw);
            let row = r.row(
                "SELECT u.disabled_at, s.expires_at, s.last_seen_at FROM sessions s JOIN users u ON u.id=s.user_id WHERE s.id_hash=?1",
                [&id_hash],
                |row| {
                    Ok((
                        row.get::<_, Value>(0)?,
                        row.get::<_, Value>(1)?,
                        row.get::<_, Value>(2)?,
                    ))
                },
            )?;
            let now = now_ms as f64;
            if let Some((disabled_at, expires_at, last_seen_at)) = row {
                let live = !js::truthy(&disabled_at)
                    && gt(js::number(&expires_at), now)
                    && gt(js::number(&last_seen_at).map(|l| l + IDLE_MS), now);
                if live {
                    tx.execute(
                        "UPDATE sessions SET last_seen_at=?1 WHERE id_hash=?2",
                        (now_ms, &id_hash),
                    )?;
                    return Ok(Upkept::Touched);
                }
            }
            tx.execute("DELETE FROM sessions WHERE id_hash=?1", [&id_hash])?;
            Ok(Upkept::Deleted)
        })
    }
}

/// device-auth.cjs `authenticate`'s touch.
fn device_upkeep(
    tx: &server_store::WriteTx<'_>,
    raw: &str,
    now_ms: i64,
) -> Result<Upkept, StoreError> {
    let r = tx.reader();
    let at = now_ms as f64;
    let row = r.row(
        "SELECT t.kind, t.expires_at, g.id, g.user_id, g.expires_at, g.last_used_at
         FROM device_tokens t JOIN device_grants g ON g.id=t.grant_id WHERE t.token_hash=?1",
        [js::digest(raw)],
        |row| {
            Ok((
                row.get::<_, Value>(0)?,
                row.get::<_, Value>(1)?,
                row.get::<_, Value>(2)?,
                row.get::<_, Value>(3)?,
                row.get::<_, Value>(4)?,
                row.get::<_, Value>(5)?,
            ))
        },
    )?;
    let Some((kind, token_expires, grant_id, user_id, grant_expires, last_used)) = row else {
        return Ok(Upkept::Nothing);
    };
    if js::text(&kind) != Some("access")
        || !gt(js::number(&token_expires), at)
        || !gt(js::number(&grant_expires), at)
    {
        return Ok(Upkept::Nothing);
    }
    let disabled = r.row(
        "SELECT disabled_at FROM users WHERE id=?1",
        [&user_id],
        |row| row.get::<_, Value>(0),
    )?;
    match disabled {
        Some(d) if !js::truthy(&d) => {}
        _ => return Ok(Upkept::Nothing),
    }
    // `at - row.last_used_at >= 60 * 1000`.
    let due = js::number(&last_used).is_some_and(|l| at - l >= GRANT_TOUCH_MS as f64);
    if !due {
        return Ok(Upkept::Nothing);
    }
    tx.execute(
        "UPDATE device_grants SET last_used_at=?1 WHERE id=?2",
        (now_ms, &grant_id),
    )?;
    Ok(Upkept::GrantTouched)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::AuthConfig;
    use server_store::rusqlite::Connection;

    const SCHEMA: &str = include_str!("../../server-store/tests/fixtures/node-schema.sql");
    const NOW: i64 = 1_800_000_000_000;

    fn setup(native: bool) -> (tempfile::TempDir, Connection, Writer, Authenticator) {
        let dir = tempfile::tempdir().unwrap();
        let node = Connection::open(server_store::db_path(dir.path())).unwrap();
        node.execute_batch(SCHEMA).unwrap();
        node.execute_batch(&format!(
            "INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,created_at,updated_at) VALUES('u1','A','a','A','member','x','w1',1,1);
             INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,disabled_at,created_at,updated_at) VALUES('u2','B','b','B','member','x','w2',5,1,1);
             INSERT INTO sessions VALUES('{live}','u1','c',1,{n1},{far},'ua','ip');
             INSERT INTO sessions VALUES('{idle}','u1','c',1,{idle_at},{far},'ua','ip');
             INSERT INTO sessions VALUES('{off}','u2','c',1,{n1},{far},'ua','ip');
             INSERT INTO device_grants VALUES('g1','u1','Mac',1,{old},{far},'ip','ua');
             INSERT INTO device_grants VALUES('g2','u1','Mac',1,{recent},{far},'ip','ua');
             INSERT INTO device_tokens VALUES('{d1}','g1','access',1,{far},NULL,NULL);
             INSERT INTO device_tokens VALUES('{d2}','g2','access',1,{far},NULL,NULL);",
            live = js::digest("live"),
            idle = js::digest("idle"),
            off = js::digest("off"),
            d1 = js::digest("nva_one"),
            d2 = js::digest("nva_two"),
            n1 = NOW - 1000,
            idle_at = NOW - IDLE_MS as i64,
            far = NOW + 86_400_000,
            old = NOW - GRANT_TOUCH_MS,
            recent = NOW - GRANT_TOUCH_MS + 1,
        ))
        .unwrap();
        let writer = Writer::open(
            dir.path(),
            server_store::RustAuth::from_env_value(Some("1")).unwrap(),
        )
        .unwrap();
        let auth = Authenticator::new(AuthConfig {
            public_origin: String::new(),
            additional_origins: vec![],
            legacy_token: String::new(),
            legacy_compat: false,
            trust_proxy: true,
            native_client_auth_env: Some(native),
            origin_from_settings: false,
        });
        (dir, node, writer, auth)
    }

    fn creds(cookie: Option<&str>, auth: Option<&str>) -> Creds {
        Creds {
            cookie: cookie.map(str::to_string),
            authorization: auth.map(str::to_string),
            method: "GET".into(),
            ..Creds::default()
        }
    }

    fn seen(node: &Connection, raw: &str) -> Option<i64> {
        node.query_row(
            "SELECT last_seen_at FROM sessions WHERE id_hash=?1",
            [js::digest(raw)],
            |r| r.get(0),
        )
        .ok()
    }

    fn used(node: &Connection, id: &str) -> i64 {
        node.query_row(
            "SELECT last_used_at FROM device_grants WHERE id=?1",
            [id],
            |r| r.get(0),
        )
        .unwrap()
    }

    #[test]
    fn sessions_like_node() {
        let (_d, node, w, a) = setup(false);
        let up = |c: &str| a.upkeep(&w, &creds(Some(c), None), NOW).unwrap();
        assert_eq!(up("cowork_session=live"), Upkept::Touched);
        assert_eq!(seen(&node, "live"), Some(NOW));
        // Idle by exactly IDLE_MS: `last_seen_at + IDLE_MS > now` fails, the row goes.
        assert_eq!(up("cowork_session=idle"), Upkept::Deleted);
        assert_eq!(seen(&node, "idle"), None);
        assert_eq!(up("cowork_session=off"), Upkept::Deleted);
        assert_eq!(seen(&node, "off"), None);
        assert_eq!(up("cowork_session=unknown"), Upkept::Deleted);
        assert_eq!(up("cowork_session="), Upkept::Nothing);
        assert_eq!(up("theme=dark"), Upkept::Nothing);
        // Without native-client sign-in a bearer is not looked at: the cookie decides.
        assert_eq!(
            a.upkeep(
                &w,
                &creds(Some("cowork_session=live"), Some("Bearer nva_one")),
                NOW + 5
            )
            .unwrap(),
            Upkept::Touched
        );
        assert_eq!(used(&node, "g1"), NOW - GRANT_TOUCH_MS);
    }

    #[test]
    fn device_grants_like_node() {
        let (_d, node, w, a) = setup(true);
        let up = |c: Option<&str>, b: &str| a.upkeep(&w, &creds(c, Some(b)), NOW).unwrap();
        assert_eq!(up(None, "Bearer nva_one"), Upkept::GrantTouched);
        assert_eq!(used(&node, "g1"), NOW);
        // Used less than a minute ago: not written.
        assert_eq!(up(None, "Bearer nva_two"), Upkept::Nothing);
        assert_eq!(used(&node, "g2"), NOW - GRANT_TOUCH_MS + 1);
        // A bearer with a session cookie authenticates nothing and writes nothing.
        assert_eq!(
            up(Some("cowork_session=live"), "Bearer nva_one"),
            Upkept::Nothing
        );
        assert_eq!(seen(&node, "live"), Some(NOW - 1000));
        assert_eq!(up(None, "Bearer nva_unknown"), Upkept::Nothing);
        // A non-nva bearer falls back to the session path.
        assert_eq!(
            up(Some("cowork_session=live"), "Bearer legacy"),
            Upkept::Touched
        );
    }
}
