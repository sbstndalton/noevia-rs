//! auth.cjs's account helpers: `publicUser(row)`, `issueSession`, `issueSessionIfCurrent`,
//! `audit`, `credentialEpoch`, the challenge table, and SQL values as better-sqlite3 hands them
//! to JS.

use crate::util;
use crate::{Account, Fault, Request};
use js_json::JValue;
use server_store::rusqlite::types::Value;
use server_store::{Reader, StoreError, WriteTx};

/// auth.cjs CHALLENGE_MS.
pub const CHALLENGE_MS: i64 = 5 * 60 * 1000;
/// auth.cjs MAX_PENDING_CHALLENGES / MAX_PENDING_REGISTRATIONS.
pub const MAX_PENDING_CHALLENGES: i64 = 4096;
pub const MAX_PENDING_REGISTRATIONS: i64 = 256;

/// A column value as better-sqlite3 gives it to JS (a BLOB is a Buffer, which JSON.stringify
/// writes as `{"type":"Buffer","data":[...]}`).
pub fn sql_js(v: &Value) -> JValue {
    match v {
        Value::Null => JValue::Null,
        Value::Integer(i) => JValue::Num(*i as f64),
        Value::Real(f) => JValue::Num(*f),
        Value::Text(t) => JValue::Str(t.clone()),
        Value::Blob(b) => JValue::obj([
            ("type", JValue::from("Buffer")),
            (
                "data",
                JValue::Arr(b.iter().map(|x| JValue::Num(f64::from(*x))).collect()),
            ),
        ]),
    }
}

/// Rows of `sql` as JS objects, with the column names (aliases) as keys, in order.
pub fn rows_js<P: server_store::rusqlite::Params>(
    r: &Reader<'_>,
    sql: &str,
    params: P,
) -> Result<Vec<JValue>, StoreError> {
    r.rows(sql, params, |row| {
        let stmt = row.as_ref();
        let mut out = Vec::new();
        for i in 0..stmt.column_count() {
            let name = stmt.column_name(i)?.to_string();
            out.push((name, sql_js(&row.get::<_, Value>(i)?)));
        }
        Ok(JValue::Obj(out))
    })
}

/// `publicUser(row)` from server-auth's validated user.
pub fn public_user(u: &server_auth::PublicUser) -> JValue {
    JValue::obj([
        ("id", JValue::from(u.id.as_str())),
        ("username", JValue::from(u.username.as_str())),
        ("displayName", JValue::from(u.display_name.as_str())),
        ("role", JValue::from(u.role.as_str())),
        ("disabled", JValue::from(u.disabled)),
        ("diaryEnabled", JValue::from(u.diary_enabled)),
        ("onboarded", JValue::from(u.onboarded)),
    ])
}

/// `publicUser(db.prepare('SELECT * FROM users WHERE id=?').get(id))`, `None` without a row.
pub fn public_user_by_id(r: &Reader<'_>, id: &str) -> Result<Option<JValue>, StoreError> {
    let row = r.row(
        "SELECT id, username, display_name, role, disabled_at FROM users WHERE id=?1",
        [id],
        |row| {
            Ok((
                row.get::<_, Value>(0)?,
                row.get::<_, Value>(1)?,
                row.get::<_, Value>(2)?,
                row.get::<_, Value>(3)?,
                row.get::<_, Value>(4)?,
            ))
        },
    )?;
    let Some((uid, username, display, role, disabled)) = row else {
        return Ok(None);
    };
    Ok(Some(public_user_values(
        r, &uid, &username, &display, &role, &disabled,
    )?))
}

fn public_user_values(
    r: &Reader<'_>,
    id: &Value,
    username: &Value,
    display: &Value,
    role: &Value,
    disabled: &Value,
) -> Result<JValue, StoreError> {
    let feature = r.row(
        "SELECT diary_enabled, onboarded FROM user_features WHERE user_id=?1",
        [id],
        |row| Ok((row.get::<_, Value>(0)?, row.get::<_, Value>(1)?)),
    )?;
    let (diary, onboarded) = match feature {
        Some((d, o)) => (
            server_auth::js::truthy(&d),
            matches!(o, Value::Null) || server_auth::js::truthy(&o),
        ),
        None => (false, true),
    };
    Ok(JValue::obj([
        ("id", sql_js(id)),
        ("username", sql_js(username)),
        ("displayName", sql_js(display)),
        ("role", sql_js(role)),
        ("disabled", JValue::from(server_auth::js::truthy(disabled))),
        ("diaryEnabled", JValue::from(diary)),
        ("onboarded", JValue::from(onboarded)),
    ]))
}

/// `listUsers()`: every account, oldest first, as publicUser.
pub fn list_users(r: &Reader<'_>) -> Result<Vec<JValue>, StoreError> {
    let rows = r.rows(
        "SELECT id, username, display_name, role, disabled_at FROM users ORDER BY created_at",
        [],
        |row| {
            Ok((
                row.get::<_, Value>(0)?,
                row.get::<_, Value>(1)?,
                row.get::<_, Value>(2)?,
                row.get::<_, Value>(3)?,
                row.get::<_, Value>(4)?,
            ))
        },
    )?;
    rows.iter()
        .map(|(i, u, d, ro, di)| public_user_values(r, i, u, d, ro, di))
        .collect()
}

/// auth.cjs `audit(action, actor, target, detail)`: `actor || null`, `target || null`.
pub fn audit(
    tx: &WriteTx<'_>,
    action: &str,
    actor: Option<&str>,
    target: Option<&str>,
    detail: &JValue,
    now: i64,
) -> Result<(), StoreError> {
    let nn = |v: Option<&str>| v.filter(|s| !s.is_empty()).map(str::to_string);
    tx.execute(
        "INSERT INTO audit_events(actor_user_id,target_user_id,action,detail,created_at) VALUES(?1,?2,?3,?4,?5)",
        (
            nn(actor),
            nn(target),
            action,
            js_json::stringify(detail).unwrap_or_else(|| "{}".into()),
            now,
        ),
    )?;
    Ok(())
}

/// `credentialEpoch(userId)`: the account's credential epoch, or `None` without the account.
pub fn credential_epoch(r: &Reader<'_>, user_id: &str) -> Result<Option<Value>, StoreError> {
    r.row(
        "SELECT credential_epoch FROM users WHERE id=?1",
        [user_id],
        |row| row.get::<_, Value>(0),
    )
}

/// What a new session sets.
pub struct Issued {
    pub csrf: String,
    pub cookies: Vec<String>,
}

/// auth.cjs `issueSession(req, res, user)`.
pub fn issue_session(
    acct: &Account,
    tx: &WriteTx<'_>,
    req: &Request,
    user_id: &str,
    now: i64,
) -> Result<Issued, Fault> {
    let raw = util::random_token(32);
    let csrf = util::random_token(32);
    tx.execute(
        "INSERT INTO sessions(id_hash,user_id,csrf_hash,created_at,last_seen_at,expires_at,user_agent,ip) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
        (
            util::digest(&raw),
            user_id,
            util::digest(&csrf),
            now,
            now,
            now + util::ABSOLUTE_MS,
            util::slice16(&req.user_agent, 300),
            req.client_ip.as_str(),
        ),
    )?;
    // Host-scoped cookies: Secure when the request's own Origin (else the public address) is https.
    let request_origin = req.creds.origin.clone().unwrap_or_default();
    let base = if request_origin.is_empty() {
        acct.origin(&tx.reader())?
    } else {
        request_origin
    };
    let secure = base.starts_with("https://");
    Ok(Issued {
        csrf: csrf.clone(),
        cookies: util::session_cookies(&raw, &csrf, secure),
    })
}

/// auth.cjs `issueSessionIfCurrent(req, res, userId, epoch)`: a session only while the account is
/// enabled and its credential epoch is still `epoch`.
pub fn issue_session_if_current(
    acct: &Account,
    tx: &WriteTx<'_>,
    req: &Request,
    user_id: &str,
    epoch: &Value,
    now: i64,
) -> Result<Option<(JValue, Issued)>, Fault> {
    let current = tx.reader().row(
        "SELECT id FROM users WHERE id=?1 AND credential_epoch=?2 AND disabled_at IS NULL",
        (user_id, epoch),
        |row| row.get::<_, String>(0),
    )?;
    let Some(id) = current else { return Ok(None) };
    let user = public_user_by_id(&tx.reader(), &id)?.ok_or(Fault::Internal)?;
    let issued = issue_session(acct, tx, req, &id, now)?;
    Ok(Some((user, issued)))
}

/// auth.cjs `saveChallenge(userId, kind, challenge)`: the challenge token, or the capacity error.
pub fn save_challenge(
    acct: &Account,
    user_id: Option<&str>,
    kind: &str,
    challenge: &str,
    now: i64,
) -> Result<Result<String, ()>, Fault> {
    let token = util::random_token(32);
    let saved = acct.writer.write(|tx| {
        tx.execute("DELETE FROM challenges WHERE expires_at<=?1", [now])?;
        let count = |tx: &WriteTx<'_>| -> Result<i64, StoreError> {
            Ok(tx
                .reader()
                .row("SELECT count(*) FROM challenges WHERE kind=?1", [kind], |r| {
                    r.get::<_, i64>(0)
                })?
                .unwrap_or(0))
        };
        if kind == "authenticate" {
            while count(tx)? >= MAX_PENDING_CHALLENGES {
                // Anonymous (unknown-username) sign-in challenges go first, then the oldest.
                let n = tx.execute(
                    "DELETE FROM challenges WHERE id_hash=(SELECT id_hash FROM challenges WHERE kind='authenticate' ORDER BY (user_id IS NOT NULL), expires_at LIMIT 1)",
                    [],
                )?;
                if n == 0 {
                    return Ok(false);
                }
            }
        } else {
            let cap = if kind == "register" {
                MAX_PENDING_REGISTRATIONS
            } else {
                MAX_PENDING_CHALLENGES
            };
            if count(tx)? >= cap {
                return Ok(false);
            }
        }
        tx.execute(
            "INSERT INTO challenges(id_hash,user_id,kind,challenge,expires_at) VALUES(?1,?2,?3,?4,?5)",
            (util::digest(&token), user_id, kind, challenge, now + CHALLENGE_MS),
        )?;
        Ok(true)
    })?;
    Ok(if saved { Ok(token) } else { Err(()) })
}

/// A stored challenge row.
pub struct Challenge {
    pub user_id: Option<String>,
    pub challenge: String,
}

/// auth.cjs `takeChallenge(token, kind)`: the live row, deleted as it is read.
pub fn take_challenge(
    acct: &Account,
    token: &str,
    kind: &str,
    now: i64,
) -> Result<Option<Challenge>, Fault> {
    Ok(acct.writer.write(|tx| {
        let id = util::digest(token);
        let row = tx.reader().row(
            "SELECT id_hash, user_id, challenge FROM challenges WHERE id_hash=?1 AND kind=?2 AND expires_at>?3",
            (&id, kind, now),
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, String>(2)?,
                ))
            },
        )?;
        let Some((id_hash, user_id, challenge)) = row else {
            return Ok(None);
        };
        tx.execute("DELETE FROM challenges WHERE id_hash=?1", [&id_hash])?;
        Ok(Some(Challenge { user_id, challenge }))
    })?)
}
