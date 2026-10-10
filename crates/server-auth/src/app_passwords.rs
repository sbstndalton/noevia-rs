//! core app-passwords.cjs `verifyDav(username, password, scope)`, read-only.
//!
//! Same verdicts; two differences, both on the safe side. With no matching row Node returns at
//! once, so the answer time tells whether an app-password id exists for that username and scope;
//! here a decoy Argon2 verification runs first ([`crate::password::burn`]). And Node's final
//! `UPDATE ... SET last_used_at` (which also proves the row survived the hashing) is a re-read of
//! the same condition here: `last_used_at` is not moved (Node keeps that column).

use crate::js;
use crate::password;
use server_store::rusqlite::types::Value;
use server_store::{Store, StoreError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DavIdentity {
    pub user_id: String,
    pub credential_id: String,
    pub scope: String,
}

/// `/^nv_dav_([a-f0-9]{32})\.[A-Za-z0-9_-]{43}$/`: the id, or `None`.
fn credential_id(password: &str) -> Option<&str> {
    let rest = password.strip_prefix("nv_dav_")?;
    if rest.len() != 32 + 1 + 43 {
        return None;
    }
    let (id, tail) = rest.split_at_checked(32)?;
    let secret = tail.strip_prefix('.')?;
    let id_ok = id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    let secret_ok = secret
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    (id_ok && secret_ok).then_some(id)
}

/// `verifyDav`. Argon2 runs outside the read transaction (it takes tens of milliseconds); the
/// row is read again after it, as Node's guarded UPDATE does.
pub fn verify_dav(
    store: &Store,
    username: &str,
    password: &str,
    scope: &str,
) -> Result<Option<DavIdentity>, StoreError> {
    if scope != "lan" && scope != "public" {
        return Ok(None);
    }
    let Some(id) = credential_id(password) else {
        return Ok(None);
    };
    // `username.length > 32` counts UTF-16 units.
    if username.encode_utf16().count() > 32 {
        return Ok(None);
    }
    let norm = username.to_lowercase();
    let row = store.read(|r| {
        r.row(
            "SELECT a.id AS id, a.user_id AS user_id, a.scope AS scope, a.password_hash AS password_hash
             FROM app_passwords a JOIN users u ON u.id=a.user_id
             WHERE a.id=?1 AND u.username_norm=?2 AND u.disabled_at IS NULL AND a.scope=?3",
            [id, norm.as_str(), scope],
            |row| {
                Ok((
                    row.get::<_, Value>("id")?,
                    row.get::<_, Value>("user_id")?,
                    row.get::<_, Value>("scope")?,
                    row.get::<_, Value>("password_hash")?,
                ))
            },
        )
    })?;
    let Some((cred, user, row_scope, hash)) = row else {
        password::burn(password);
        return Ok(None);
    };
    let (Some(cred), Some(user), Some(row_scope), Some(hash)) = (
        js::text(&cred),
        js::text(&user),
        js::text(&row_scope),
        js::text(&hash),
    ) else {
        password::burn(password);
        return Ok(None);
    };
    if !password::verify(hash, password) {
        return Ok(None);
    }
    // Revocation or account disable while Argon2 ran takes effect now.
    let still = store.read(|r| {
        r.row(
            "SELECT 1 FROM app_passwords WHERE id=?1 AND password_hash=?2
             AND EXISTS (SELECT 1 FROM users WHERE id=app_passwords.user_id AND disabled_at IS NULL)",
            [cred, hash],
            |row| row.get::<_, i64>(0),
        )
    })?;
    Ok(still.map(|_| DavIdentity {
        user_id: user.to_string(),
        credential_id: cred.to_string(),
        scope: row_scope.to_string(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_shape() {
        let ok = format!("nv_dav_{}.{}", "a".repeat(32), "x".repeat(43));
        assert_eq!(credential_id(&ok), Some("a".repeat(32).as_str()));
        for bad in [
            format!("nv_dav_{}.{}", "A".repeat(32), "x".repeat(43)),
            format!("nv_dav_{}.{}", "a".repeat(31), "x".repeat(44)),
            format!("nv_dav_{}.{}\n", "a".repeat(32), "x".repeat(43)),
            format!("nv_dav_{}.{}=", "a".repeat(32), "x".repeat(42)),
            format!("NV_DAV_{}.{}", "a".repeat(32), "x".repeat(43)),
            format!("nv_dav_{}é{}", "a".repeat(32), "x".repeat(42)),
            String::new(),
        ] {
            assert_eq!(credential_id(&bad), None, "{bad}");
        }
    }
}
