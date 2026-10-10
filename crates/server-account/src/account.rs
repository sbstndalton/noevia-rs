//! routes/auth.cjs `account`: what a signed-in account does to itself (session, sign-out,
//! appearance, profile, app passwords, the Diary preference, onboarding, passkeys, sessions) and
//! the administrator's /api/admin/users, /api/admin/invitations, disabling and recovery links.
//! Account deletion, sharing and secrets rotation stay Node's ([`Outcome::Pass`]).

use crate::users::{self, audit, rows_js};
use crate::util;
use crate::{decode_segment, read_object, Account, Fault, Handled, Outcome, Reply, Request};
use js_json::JValue;
use server_auth::{Credential, Identity};
use server_store::rusqlite::types::Value;

fn ok_body() -> JValue {
    JValue::obj([("ok", JValue::from(true))])
}

/// `p.match(/^<prefix>([^/]+)$/)`.
fn segment<'a>(p: &'a str, prefix: &str) -> Option<&'a str> {
    p.strip_prefix(prefix)
        .filter(|rest| !rest.is_empty() && !rest.contains('/'))
}

pub fn account(
    acct: &Account,
    req: &Request,
    authn: &Identity,
    now: i64,
) -> Result<Option<Outcome>, Fault> {
    let (p, m) = (req.path.as_str(), req.method.as_str());
    let uid = authn.user.id.as_str();
    let legacy = matches!(authn.credential, Credential::Legacy);
    let reply = |r: Reply| Ok(Some(Outcome::Reply(r)));
    if p == "/api/auth/session" && m == "GET" {
        let header = req.creds.cookie.clone().unwrap_or_default();
        let csrf = header
            .split(';')
            .map(util::js_trim)
            .find(|x| x.starts_with("cowork_csrf="))
            .unwrap_or("");
        let token = if legacy {
            JValue::Null
        } else {
            let raw = csrf.get(12..).unwrap_or("");
            JValue::from(server_auth::js::decode_uri_component(raw).ok_or(Fault::Internal)?)
        };
        return reply(Reply::json(
            200,
            &JValue::obj([
                ("user", users::public_user(&authn.user)),
                ("csrfToken", token),
                ("legacy", JValue::from(legacy)),
            ]),
        ));
    }
    if p == "/api/auth/logout" && m == "POST" {
        if let Some(raw) = req.creds.cookie("cowork_session").filter(|v| !v.is_empty()) {
            acct.writer.write(|tx| {
                tx.execute(
                    "DELETE FROM sessions WHERE id_hash=?1",
                    [util::digest(&raw)],
                )
            })?;
        }
        return reply(Reply::json(200, &ok_body()).with_cookies(util::cleared_cookies()));
    }
    if p == "/api/profile/appearance" {
        return match m {
            "GET" => {
                let row = acct.writer.read(|r| {
                    r.row(
                        "SELECT value FROM user_appearance WHERE user_id=?1",
                        [uid],
                        |row| row.get::<_, String>(0),
                    )
                })?;
                let v = match row {
                    Some(t) => js_json::parse(&t).map_err(|_| Fault::Internal)?,
                    None => JValue::Null,
                };
                reply(Reply::json(200, &v))
            }
            "PUT" => {
                let body = read_object(req)?;
                let Some(value) = appearance(&body) else {
                    return reply(Reply::error(
                        400,
                        "Choose a valid mode and a palette for both light and dark.",
                    ));
                };
                let text = js_json::stringify(&value).unwrap_or_default();
                acct.writer.write(|tx| {
                    tx.execute(
                        "INSERT INTO user_appearance(user_id,value,updated_at) VALUES(?1,?2,?3) ON CONFLICT(user_id) DO UPDATE SET value=excluded.value,updated_at=excluded.updated_at",
                        (uid, &text, now),
                    )
                })?;
                reply(Reply::json(200, &value))
            }
            _ => reply(Reply::error(405, "method not allowed")),
        };
    }
    if p == "/api/profile" && m == "GET" {
        let current = match &authn.credential {
            Credential::Session { id_hash, .. } => Some(id_hash.clone()),
            _ => None,
        };
        let (passkeys, sessions) = acct.writer.read(|r| {
            let pk = rows_js(r, "SELECT id,name,device_type AS deviceType,backed_up AS backedUp,created_at AS createdAt,last_used_at AS lastUsedAt FROM passkeys WHERE user_id=?1 ORDER BY created_at", [uid])?;
            let ss = rows_js(r, "SELECT id_hash AS id,created_at AS createdAt,last_seen_at AS lastSeenAt,expires_at AS expiresAt,user_agent AS userAgent,ip FROM sessions WHERE user_id=?1 ORDER BY last_seen_at DESC", [uid])?;
            Ok((pk, ss))
        })?;
        let sessions = sessions
            .into_iter()
            .map(|s| {
                let mine = current
                    .as_deref()
                    .is_some_and(|c| s.get("id").as_str() == Some(c));
                match s {
                    JValue::Obj(mut items) => {
                        items.push(("current".into(), JValue::from(mine)));
                        JValue::Obj(items)
                    }
                    other => other,
                }
            })
            .collect();
        return reply(Reply::json(
            200,
            &JValue::obj([
                ("user", users::public_user(&authn.user)),
                ("passkeys", JValue::Arr(passkeys)),
                ("sessions", JValue::Arr(sessions)),
            ]),
        ));
    }
    if p == "/api/profile/app-passwords" && m == "GET" {
        let list = acct.writer.read(|r| list_app_passwords(r, uid))?;
        return reply(Reply::json(
            200,
            &JValue::obj([
                ("appPasswords", JValue::Arr(list)),
                (
                    "sharingAvailable",
                    JValue::from(acct.settings.dav_available),
                ),
            ]),
        ));
    }
    if p == "/api/profile/app-passwords" && m == "POST" {
        let body = read_object(req)?;
        return Ok(Some(match create_app_password(acct, uid, &body, now)? {
            Ok(v) => Reply::json(201, &v).into(),
            Err(msg) => Reply::error(400, &msg).into(),
        }));
    }
    if let Some(id) = segment(p, "/api/profile/app-passwords/") {
        let hex32 = id.len() == 32 && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
        if hex32 && m == "DELETE" {
            let removed = acct.writer.write(|tx| {
                let n = tx.execute(
                    "DELETE FROM app_passwords WHERE id=?1 AND user_id=?2",
                    (id, uid),
                )?;
                if n > 0 {
                    audit(
                        tx,
                        "app-password.revoke",
                        Some(uid),
                        Some(uid),
                        &JValue::obj([("id", JValue::from(id))]),
                        now,
                    )?;
                }
                Ok(n > 0)
            })?;
            return reply(Reply::json(if removed { 200 } else { 404 }, &ok_body()));
        }
    }
    if p == "/api/profile" && m == "PATCH" {
        let body = read_object(req)?;
        let name = util::slice16(
            util::js_trim(&util::js_string(body.get("displayName"))?),
            80,
        );
        acct.writer.write(|tx| {
            tx.execute(
                "UPDATE users SET display_name=?1,updated_at=?2 WHERE id=?3",
                (&name, now, uid),
            )
        })?;
        return reply(Reply::json(200, &ok_body()));
    }
    if p == "/api/profile/features" && m == "PUT" {
        let enabled = read_object(req)?.get("diaryEnabled").truthy();
        acct.writer.write(|tx| {
            tx.execute(
                "INSERT INTO user_features(user_id,diary_enabled,updated_at) VALUES(?1,?2,?3) ON CONFLICT(user_id) DO UPDATE SET diary_enabled=excluded.diary_enabled,updated_at=excluded.updated_at",
                (uid, i64::from(enabled), now),
            )?;
            audit(tx, "feature.diary", Some(uid), Some(uid), &JValue::obj([("enabled", JValue::from(enabled))]), now)
        })?;
        return reply(Reply::json(
            200,
            &JValue::obj([("diaryEnabled", JValue::from(enabled))]),
        ));
    }
    if p == "/api/profile/onboarding" && m == "POST" {
        acct.writer.write(|tx| {
            tx.execute(
                "INSERT INTO user_features(user_id,diary_enabled,onboarded,updated_at) VALUES(?1,0,1,?2) ON CONFLICT(user_id) DO UPDATE SET onboarded=1,updated_at=excluded.updated_at",
                (uid, now),
            )
        })?;
        return reply(Reply::json(
            200,
            &JValue::obj([("onboarded", JValue::from(true))]),
        ));
    }
    if p == "/api/auth/passkeys/register/options" && m == "POST" {
        return Ok(Some(crate::passkeys::registration_options(acct, uid, now)?));
    }
    if p == "/api/auth/passkeys/register/verify" && m == "POST" {
        let body = read_object(req)?;
        return Ok(Some(crate::passkeys::registration_verify(
            acct, uid, &body, now,
        )?));
    }
    if let Some(raw) = segment(p, "/api/auth/passkeys/") {
        if m == "DELETE" {
            let id = decode_segment(raw).ok_or(Fault::Internal)?;
            return Ok(Some(delete_passkey(acct, uid, &id)?));
        }
        if m == "PATCH" {
            let id = decode_segment(raw).ok_or(Fault::Internal)?;
            let body = read_object(req)?;
            let name = util::slice16(util::js_trim(&util::js_string(body.get("name"))?), 80);
            let n = acct.writer.write(|tx| {
                tx.execute(
                    "UPDATE passkeys SET name=?1 WHERE id=?2 AND user_id=?3",
                    (&name, &id, uid),
                )
            })?;
            return reply(Reply::json(
                if n > 0 { 200 } else { 404 },
                &JValue::obj([("ok", JValue::from(n > 0))]),
            ));
        }
    }
    if let Some(raw) = segment(p, "/api/auth/sessions/") {
        if m == "DELETE" {
            let id = decode_segment(raw).ok_or(Fault::Internal)?;
            let n = acct.writer.write(|tx| {
                tx.execute(
                    "DELETE FROM sessions WHERE id_hash=?1 AND user_id=?2",
                    (&id, uid),
                )
            })?;
            return reply(Reply::json(if n > 0 { 200 } else { 404 }, &ok_body()));
        }
    }
    if p.starts_with("/api/admin/") {
        return admin(acct, req, authn, now);
    }
    Ok(None)
}

/// appearance.cjs `validateAppearance`.
fn appearance(v: &JValue) -> Option<JValue> {
    const PALETTES: &[&str] = &["warm", "cool", "neutral", "sage", "iris"];
    let theme = v
        .get("theme")
        .as_str()
        .filter(|t| ["light", "dark", "system"].contains(t))?;
    let light = v.get("light").as_str().filter(|t| PALETTES.contains(t))?;
    let dark = v.get("dark").as_str().filter(|t| PALETTES.contains(t))?;
    Some(JValue::obj([
        ("theme", JValue::from(theme)),
        ("light", JValue::from(light)),
        ("dark", JValue::from(dark)),
    ]))
}

fn list_app_passwords(
    r: &server_store::Reader<'_>,
    uid: &str,
) -> Result<Vec<JValue>, server_store::StoreError> {
    rows_js(
        r,
        "SELECT id,name,scope,created_at AS createdAt,last_used_at AS lastUsedAt FROM app_passwords WHERE user_id=?1 ORDER BY created_at,id",
        [uid],
    )
}

fn active(r: &server_store::Reader<'_>, uid: &str) -> Result<bool, server_store::StoreError> {
    Ok(r.row(
        "SELECT id FROM users WHERE id=?1 AND disabled_at IS NULL",
        [uid],
        |row| row.get::<_, String>(0),
    )?
    .is_some())
}

/// app-passwords.cjs `create(userId, { name, scope })`: the reply body, or the thrown message.
fn create_app_password(
    acct: &Account,
    uid: &str,
    body: &JValue,
    now: i64,
) -> Result<Result<JValue, String>, Fault> {
    let name = body.get("name").as_str();
    let bad_name = match name {
        None => true,
        Some(n) => {
            let t = util::js_trim(n);
            t.is_empty()
                || js_json::js_len(t) > 80
                || n.chars().any(|c| (c as u32) < 0x20 || c == '\u{7f}')
        }
    };
    if bad_name {
        return Ok(Err("Use a device name of 1–80 characters.".into()));
    }
    let name = util::js_trim(name.unwrap_or("")).to_string();
    let scope = body
        .get("scope")
        .as_str()
        .filter(|s| *s == "lan" || *s == "public");
    let Some(scope) = scope.map(str::to_string) else {
        return Ok(Err("Choose a LAN or public credential scope.".into()));
    };
    let (is_active, count, epoch) = acct.writer.read(|r| {
        Ok((
            active(r, uid)?,
            list_app_passwords(r, uid)?.len(),
            users::credential_epoch(r, uid)?,
        ))
    })?;
    if !is_active {
        return Ok(Err("Account unavailable.".into()));
    }
    let limited = acct.rate.lock().map_err(|_| Fault::Internal)?.limited(
        &format!("app-password:create:{uid}"),
        5,
        60_000,
        now,
    );
    if limited {
        return Ok(Err(
            "Wait a minute before generating another app password.".into()
        ));
    }
    if count >= 20 {
        return Ok(Err(
            "Revoke an app password before creating another (limit 20).".into(),
        ));
    }
    let id = util::random_hex(16);
    let password = format!("nv_dav_{id}.{}", util::random_token(32));
    let hash = crate::open::hash_password(&password)?;
    let created_at = now;
    let saved = acct.writer.write(|tx| {
        let r = tx.reader();
        // Recheck after hashing: disable/delete, a recovery and concurrent minting win.
        if !active(&r, uid)? || users::credential_epoch(&r, uid)? != epoch {
            return Ok(Err("Account unavailable.".to_string()));
        }
        if list_app_passwords(&r, uid)?.len() >= 20 {
            return Ok(Err("Revoke an app password before creating another (limit 20).".to_string()));
        }
        tx.execute(
            "INSERT INTO app_passwords(id,user_id,name,scope,password_hash,created_at) VALUES(?1,?2,?3,?4,?5,?6)",
            (&id, uid, &name, &scope, &hash, created_at),
        )?;
        audit(
            tx,
            "app-password.create",
            Some(uid),
            Some(uid),
            &JValue::obj([("id", JValue::from(id.as_str())), ("scope", JValue::from(scope.as_str()))]),
            now,
        )?;
        Ok(Ok(()))
    })?;
    if let Err(m) = saved {
        return Ok(Err(m));
    }
    Ok(Ok(JValue::obj([
        ("id", JValue::from(id.as_str())),
        ("name", JValue::from(name)),
        ("scope", JValue::from(scope)),
        ("createdAt", JValue::from(created_at)),
        ("lastUsedAt", JValue::Null),
        ("password", JValue::from(password)),
    ])))
}

/// auth.cjs `deletePasskey` and its route (#863: never the last way to sign in).
fn delete_passkey(acct: &Account, uid: &str, id: &str) -> Handled {
    let out = acct.writer.write(|tx| {
        let r = tx.reader();
        if r.row(
            "SELECT id FROM passkeys WHERE id=?1 AND user_id=?2",
            (id, uid),
            |row| row.get::<_, String>(0),
        )?
        .is_none()
        {
            return Ok(None);
        }
        let hash = r.row(
            "SELECT password_hash FROM users WHERE id=?1",
            [uid],
            |row| row.get::<_, Value>(0),
        )?;
        let has_password = matches!(&hash, Some(Value::Text(t)) if !util::js_trim(t).is_empty());
        let others: i64 = r
            .row(
                "SELECT count(*) FROM passkeys WHERE user_id=?1 AND id<>?2",
                (uid, id),
                |row| row.get(0),
            )?
            .unwrap_or(0);
        if !has_password && others == 0 {
            return Ok(Some(Err(())));
        }
        let n = tx.execute("DELETE FROM passkeys WHERE id=?1 AND user_id=?2", (id, uid))?;
        Ok(Some(Ok(n > 0)))
    })?;
    Ok(match out {
        None | Some(Ok(false)) => Reply::json(404, &ok_body()).into(),
        Some(Ok(true)) => Reply::json(200, &ok_body()).into(),
        Some(Err(())) => Reply::error(
            409,
            "This is your only way to sign in. Add a password or another passkey before removing it.",
        )
        .into(),
    })
}

/// The /api/admin/ block of routes/auth.cjs `account` for the paths Rust owns.
fn admin(
    acct: &Account,
    req: &Request,
    authn: &Identity,
    now: i64,
) -> Result<Option<Outcome>, Fault> {
    let (p, m) = (req.path.as_str(), req.method.as_str());
    let actor = authn.user.id.as_str();
    let reply = |r: Reply| Ok(Some(Outcome::Reply(r)));
    let ours = (p == "/api/admin/users" && m == "GET")
        || (p == "/api/admin/invitations" && m == "POST")
        || (users_sub(p, "/disabled").is_some() && m == "PUT")
        || (users_sub(p, "/recovery").is_some() && m == "POST");
    if !ours {
        return Ok(None);
    }
    if authn.user.role != server_auth::Role::Admin {
        return reply(Reply::error(403, "administrator required"));
    }
    if p == "/api/admin/users" {
        let list = acct.writer.read(users::list_users)?;
        return reply(Reply::json(
            200,
            &JValue::obj([("users", JValue::Arr(list))]),
        ));
    }
    if p == "/api/admin/invitations" {
        let body = read_object(req)?;
        // `createInvite(adminId, role = 'member')`: the default applies to undefined only.
        let role = match body.get("role") {
            JValue::Undefined => JValue::from("member"),
            other => other.clone(),
        };
        let stored = if role.as_str() == Some("admin") {
            "admin"
        } else {
            "member"
        };
        let token = util::random_token(32);
        let expires = now + 86_400_000;
        acct.writer.write(|tx| {
            tx.execute(
                "INSERT INTO invitations(token_hash,created_by,role,expires_at,created_at) VALUES(?1,?2,?3,?4,?5)",
                (util::digest(&token), actor, stored, expires, now),
            )?;
            audit(tx, "invite.create", Some(actor), None, &JValue::obj([("role", role.clone())]), now)
        })?;
        return reply(Reply::json(
            201,
            &JValue::obj([
                ("token", JValue::from(token)),
                ("expiresAt", JValue::from(expires)),
            ]),
        ));
    }
    if let Some(raw) = users_sub(p, "/disabled") {
        // Inside the route's try: a malformed escape is a 400 like every other throw.
        let Some(target) = decode_segment(raw) else {
            return reply(Reply::error(400, "URI malformed"));
        };
        let disabled = match read_object(req) {
            Ok(b) => b.get("disabled").truthy(),
            Err(Fault::Status(_, msg)) => return reply(Reply::error(400, &msg)),
            Err(f) => return Err(f),
        };
        let out = acct.writer.write(|tx| {
            let r = tx.reader();
            let role = r.row("SELECT role FROM users WHERE id=?1", [&target], |row| {
                row.get::<_, Value>(0)
            })?;
            let Some(role) = role else {
                return Ok(Ok(false));
            };
            if matches!(&role, Value::Text(t) if t == "admin") && disabled {
                let admins: i64 = r
                    .row(
                        "SELECT count(*) FROM users WHERE role='admin' AND disabled_at IS NULL",
                        [],
                        |row| row.get(0),
                    )?
                    .unwrap_or(0);
                if admins <= 1 {
                    return Ok(Err("cannot disable the last administrator"));
                }
            }
            tx.execute(
                "UPDATE users SET disabled_at=?1,updated_at=?2 WHERE id=?3",
                (disabled.then_some(now), now, &target),
            )?;
            if disabled {
                tx.execute("DELETE FROM sessions WHERE user_id=?1", [&target])?;
                tx.execute("DELETE FROM device_grants WHERE user_id=?1", [&target])?;
                // A disabled admin's still-open invitations must not outlive them.
                tx.execute(
                    "DELETE FROM invitations WHERE created_by=?1 AND used_at IS NULL",
                    [&target],
                )?;
            }
            audit(
                tx,
                if disabled {
                    "user.disable"
                } else {
                    "user.enable"
                },
                Some(actor),
                Some(&target),
                &JValue::obj::<&str>([]),
                now,
            )?;
            Ok(Ok(true))
        })?;
        return match out {
            Ok(found) => reply(Reply::json(if found { 200 } else { 404 }, &ok_body())),
            Err(msg) => reply(Reply::error(400, msg)),
        };
    }
    if let Some(raw) = users_sub(p, "/recovery") {
        let target = decode_segment(raw).ok_or(Fault::Internal)?;
        let token = util::random_token(32);
        let expires = now + 3_600_000;
        let made = acct.writer.write(|tx| {
            let exists = tx
                .reader()
                .row("SELECT 1 FROM users WHERE id=?1", [&target], |row| row.get::<_, i64>(0))?
                .is_some();
            if !exists {
                return Ok(false);
            }
            tx.execute(
                "INSERT INTO recoveries(token_hash,user_id,created_by,expires_at,created_at) VALUES(?1,?2,?3,?4,?5)",
                (util::digest(&token), &target, actor, expires, now),
            )?;
            audit(tx, "recovery.create", Some(actor), Some(&target), &JValue::obj::<&str>([]), now)?;
            Ok(true)
        })?;
        return reply(if made {
            Reply::json(
                201,
                &JValue::obj([
                    ("token", JValue::from(token)),
                    ("expiresAt", JValue::from(expires)),
                ]),
            )
        } else {
            Reply::error(404, "no such user")
        });
    }
    Ok(None)
}

/// `/^\/api\/admin\/users\/([^/]+)<suffix>$/`.
fn users_sub<'a>(p: &'a str, suffix: &str) -> Option<&'a str> {
    p.strip_prefix("/api/admin/users/")?
        .strip_suffix(suffix)
        .filter(|id| !id.is_empty() && !id.contains('/'))
}
