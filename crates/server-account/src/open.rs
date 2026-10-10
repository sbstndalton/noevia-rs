//! routes/auth.cjs `open`: what a signed-out browser may call (setup status and completion,
//! password and passkey sign-in, invitation acceptance, recovery), over auth.cjs `setup`,
//! `passwordLogin`, `authenticationOptions`/`authenticationVerify`, `acceptInvite` and
//! `completeRecovery`.

use crate::users::{self, audit};
use crate::util;
use crate::{read_object, Account, Fault, Handled, Outcome, Reply, Request, PUBLIC_AUTH_ROUTES};
use js_json::JValue;
use server_store::rusqlite::types::Value;
use server_store::StoreError;

/// auth.cjs password sign-in limits (#927), all 15-minute windows.
pub const LOGIN_WINDOW_MS: i64 = 15 * 60 * 1000;
pub const LOGIN_FAILURES_PER_ACCOUNT: u64 = 5;
pub const LOGIN_FAILURES_BEFORE_429: u64 = 30;
pub const LOGIN_FAILURES_PER_ADDRESS: u64 = 200;
/// `rateLimited(key)` defaults: 5 per 15 minutes.
const DEFAULT_LIMIT: u64 = 5;
const DEFAULT_WINDOW: i64 = 15 * 60 * 1000;

pub fn open(acct: &Account, req: &Request, now: i64) -> Result<Option<Outcome>, Fault> {
    let (p, m) = (req.path.as_str(), req.method.as_str());
    if p == "/api/setup/status" && m == "GET" {
        let (count, origin) = acct.writer.read(|r| Ok((user_count(r)?, acct.origin(r))))?;
        let origin = origin?;
        let shown = if origin.is_empty() {
            acct.settings.public_origin.clone()
        } else {
            origin
        };
        return Ok(Some(
            Reply::json(
                200,
                &JValue::obj([
                    ("configured", JValue::from(count > 0)),
                    ("publicOrigin", JValue::from(shown)),
                ]),
            )
            .into(),
        ));
    }
    if PUBLIC_AUTH_ROUTES.contains(&p) && m != "GET" {
        let ok = acct
            .writer
            .read(|r| acct.auth.origin_valid(r, &req.creds))?;
        if !ok {
            return Ok(Some(Reply::error(403, "origin not allowed").into()));
        }
    }
    let out = match (p, m) {
        ("/api/setup/complete", "POST") => setup(acct, req, &read_object(req)?, now)?,
        ("/api/auth/login/password", "POST") => password_login(acct, req, &read_object(req)?, now)?,
        ("/api/auth/login/passkey/options", "POST") => {
            let body = read_object(req)?;
            crate::passkeys::authentication_options(acct, req, body.get("username"), now)?
        }
        ("/api/auth/login/passkey/verify", "POST") => {
            // Every failure, malformed bodies included, is the same 401.
            match read_object(req)
                .and_then(|body| crate::passkeys::authentication_verify(acct, req, &body, now))
            {
                Ok(Some(o)) => o,
                _ => Reply::error(401, "sign-in failed").into(),
            }
        }
        ("/api/auth/invitations/accept", "POST") => {
            accept_invite(acct, req, &read_object(req)?, now)?
        }
        ("/api/auth/recovery/complete", "POST") => {
            let result = read_object(req).and_then(|body| complete_recovery(acct, &body, now));
            match result {
                Ok(true) => Reply::json(200, &JValue::obj([("ok", JValue::from(true))])).into(),
                Ok(false) => Reply::error(400, "recovery link is invalid or expired").into(),
                // `catch (e) { json(res, 400, { error: e.message }) }`: every thrown message.
                Err(Fault::Status(_, msg)) => Reply::error(400, &msg).into(),
                Err(Fault::Internal) => Reply::error(400, "Internal error").into(),
                Err(f) => return Err(f),
            }
        }
        _ => return Ok(None),
    };
    Ok(Some(out))
}

pub fn user_count(r: &server_store::Reader<'_>) -> Result<i64, StoreError> {
    Ok(
        r.row("SELECT count(*) FROM users", [], |row| row.get::<_, i64>(0))?
            .unwrap_or(0),
    )
}

fn setting(r: &server_store::Reader<'_>, key: &str) -> Result<Option<Value>, StoreError> {
    r.row("SELECT value FROM settings WHERE key=?1", [key], |row| {
        row.get::<_, Value>(0)
    })
}

/// auth.cjs `createPasswordHash(password)`: Argon2id with @node-rs/argon2's output for these
/// parameters (m=19456 KiB, t=2, p=1, 16-byte salt, 32-byte hash).
pub fn create_password_hash(password: &JValue) -> Result<String, Fault> {
    let Some(pw) = password.as_str() else {
        return Err(Fault::Status(
            400,
            "password must be 12-128 characters".into(),
        ));
    };
    let n = js_json::js_len(pw);
    if !(12..=128).contains(&n) {
        return Err(Fault::Status(
            400,
            "password must be 12-128 characters".into(),
        ));
    }
    hash_password(pw)
}

pub fn hash_password(pw: &str) -> Result<String, Fault> {
    use argon2::password_hash::{PasswordHasher, SaltString};
    let params = argon2::Params::new(19456, 2, 1, Some(32)).map_err(|_| Fault::Internal)?;
    let salt = SaltString::generate(&mut rand_core::OsRng);
    argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params)
        .hash_password(pw.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|_| Fault::Internal)
}

fn reply_signed_in(
    status: u16,
    user: JValue,
    issued: users::Issued,
    extra: Option<(&str, JValue)>,
) -> Outcome {
    let mut body = vec![
        ("user".to_string(), user),
        ("csrfToken".to_string(), JValue::from(issued.csrf)),
    ];
    if let Some((k, v)) = extra {
        body.push((k.to_string(), v));
    }
    Reply::json(status, &JValue::Obj(body))
        .with_cookies(issued.cookies)
        .into()
}

/// auth.cjs `setup(req, res, body)`.
fn setup(acct: &Account, req: &Request, body: &JValue, now: i64) -> Handled {
    let (count, expected, origin) = acct.writer.read(|r| {
        Ok((
            user_count(r)?,
            setting(r, "setup_code_hash")?,
            acct.origin(r),
        ))
    })?;
    let origin = origin?;
    if count != 0 {
        return Ok(Reply::error(409, "setup already complete").into());
    }
    let limited = acct.rate.lock().map_err(|_| Fault::Internal)?.limited(
        &format!("setup:{}", req.client_ip),
        DEFAULT_LIMIT,
        DEFAULT_WINDOW,
        now,
    );
    if limited {
        return Ok(Reply::error(429, "try again later").into());
    }
    let expected = match expected {
        Some(Value::Text(t)) if !t.is_empty() => t,
        _ => return Ok(Reply::error(401, "setup could not be completed").into()),
    };
    let code = util::js_string_or(body.get("setupCode"), "")?;
    if util::digest(&code) != expected {
        return Ok(Reply::error(401, "setup could not be completed").into());
    }
    if !util::username_ok(&util::js_string_or(body.get("username"), "")?) {
        return Ok(Reply::error(400, "invalid username").into());
    }
    let chosen = if body.get("publicOrigin").truthy() {
        util::js_string(body.get("publicOrigin"))?
    } else {
        origin.clone()
    };
    let selected = chosen.strip_suffix('/').unwrap_or(&chosen).to_string();
    if !util::acceptable_public_origin(&selected) {
        return Ok(Reply::error(
            400,
            "use https://, or a private-network address (a LAN IP, a bare LAN hostname, or localhost) over http://",
        )
        .into());
    }
    let password_hash = match create_password_hash(body.get("password")) {
        Ok(h) => h,
        Err(Fault::Status(_, m)) => return Ok(Reply::error(400, &m).into()),
        Err(f) => return Err(f),
    };
    let env_origin = acct
        .settings
        .public_origin
        .strip_suffix('/')
        .unwrap_or(&acct.settings.public_origin)
        .to_string();
    // `body.username.toLowerCase()` throws for a non-string that passed the regex as String(x).
    let Some(username) = body.get("username").as_str() else {
        return Err(Fault::Internal);
    };
    let display_src = if body.get("displayName").truthy() {
        util::js_string(body.get("displayName"))?
    } else {
        username.to_string()
    };
    let display = util::slice16(util::js_trim(&display_src), 80);
    let id = util::uuid();
    let diary = i64::from(body.get("diaryEnabled").truthy());
    let raced = acct.writer.write(|tx| {
        // Hashing above took time: a concurrent setup may have finished meanwhile.
        if user_count(&tx.reader())? != 0 || tx.delete_setting_if("setup_code_hash", &expected)? != 1
        {
            return Ok(true);
        }
        tx.execute(
            "INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,created_at,updated_at) VALUES(?1,?2,?3,?4,'admin',?5,?6,?7,?7)",
            (&id, username, username.to_lowercase(), &display, &password_hash, util::random_token(32), now),
        )?;
        tx.execute(
            "INSERT INTO user_features(user_id,diary_enabled,onboarded,updated_at) VALUES(?1,?2,0,?3)",
            (&id, diary, now),
        )?;
        tx.set_setting("public_origin", &selected)?;
        // core PR #58 (noevia#1254): an address other than PUBLIC_ORIGIN is the administrator's
        // choice, so the address served now is the one served after a restart.
        if !env_origin.is_empty() && selected != env_origin {
            tx.set_setting("public_origin_admin", &selected)?;
        }
        Ok(false)
    })?;
    if raced {
        return Ok(Reply::error(409, "setup already complete").into());
    }
    let _ = std::fs::remove_file(acct.settings.data_dir.join("first-run-setup-code"));
    let out = acct.writer.write(|tx| {
        audit(
            tx,
            "setup.complete",
            Some(&id),
            Some(&id),
            &JValue::obj::<&str>([]),
            now,
        )?;
        Ok(())
    });
    out?;
    let (user, issued) = acct
        .writer
        .write(|tx| {
            let user = users::public_user_by_id(&tx.reader(), &id)?;
            let issued = users::issue_session(acct, tx, req, &id, now).map_err(store_fault)?;
            Ok((user, issued))
        })
        .map_err(Fault::from)?;
    let user = user.ok_or(Fault::Internal)?;
    Ok(reply_signed_in(
        201,
        user,
        issued,
        Some(("migrationRequired", JValue::from(true))),
    ))
}

fn store_fault(f: Fault) -> StoreError {
    match f {
        Fault::Unavailable => StoreError::NotReady("store".into()),
        _ => StoreError::Sqlite("route".into()),
    }
}

/// auth.cjs `passwordLogin(req, res, body)`.
fn password_login(acct: &Account, req: &Request, body: &JValue, now: i64) -> Handled {
    let address = req.client_ip.as_str();
    let username = util::js_string_or(body.get("username"), "")?.to_lowercase();
    let key = format!("login:{address}:{username}");
    let failures = format!("login-failed:{address}");
    let fail = |status: u16| -> Handled { Ok(Reply::error(status, "sign-in failed").into()) };
    let (probing, charged) = {
        let mut rate = acct.rate.lock().map_err(|_| Fault::Internal)?;
        if rate.blocked(&failures, LOGIN_FAILURES_PER_ADDRESS - 1, now) {
            return fail(429);
        }
        if rate.limited(&key, LOGIN_FAILURES_PER_ACCOUNT, LOGIN_WINDOW_MS, now) {
            return fail(429);
        }
        let probing = rate.blocked(&failures, LOGIN_FAILURES_BEFORE_429, now);
        let charged = rate.charge(&failures, LOGIN_FAILURES_PER_ADDRESS, LOGIN_WINDOW_MS, now);
        (probing, charged)
    };
    let row = acct.writer.read(|r| {
        r.row(
            "SELECT id, password_hash, disabled_at, credential_epoch FROM users WHERE username_norm=?1",
            [&username],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Value>(1)?,
                    row.get::<_, Value>(2)?,
                    row.get::<_, Value>(3)?,
                ))
            },
        )
    })?;
    let password = util::js_string_or(body.get("password"), "")?;
    // Always one Argon2 verification, so the time does not tell whether the username exists.
    let usable = row
        .as_ref()
        .filter(|(_, _, disabled, _)| !server_auth::js::truthy(disabled));
    let ok = match usable {
        Some((_, Value::Text(h), _, _)) => server_auth::password::verify(h, &password),
        _ => server_auth::password::burn(&password),
    };
    let signed_in = match (ok, usable) {
        (true, Some((id, _, _, epoch))) => acct
            .writer
            .write(|tx| {
                users::issue_session_if_current(acct, tx, req, id, epoch, now).map_err(store_fault)
            })
            .map_err(Fault::from)?
            .map(|s| (id.clone(), s)),
        _ => None,
    };
    let Some((id, (user, issued))) = signed_in else {
        return fail(if probing { 429 } else { 401 });
    };
    {
        let mut rate = acct.rate.lock().map_err(|_| Fault::Internal)?;
        rate.clear(&key);
        rate.release(&failures, charged.window, now);
    }
    acct.writer.write(|tx| {
        audit(
            tx,
            "auth.password",
            Some(&id),
            Some(&id),
            &JValue::obj::<&str>([]),
            now,
        )
    })?;
    Ok(reply_signed_in(200, user, issued, None))
}

/// auth.cjs `acceptInvite(req, res, body)`.
fn accept_invite(acct: &Account, req: &Request, body: &JValue, now: i64) -> Handled {
    let token = util::js_string_or(body.get("token"), "")?;
    let invite = acct.writer.read(|r| {
        r.row(
            "SELECT token_hash, role FROM invitations WHERE token_hash=?1 AND used_at IS NULL AND expires_at>?2",
            (util::digest(&token), now),
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Value>(1)?)),
        )
    })?;
    let Some((token_hash, role)) = invite else {
        return Ok(Reply::error(400, "invitation is invalid or expired").into());
    };
    if !util::username_ok(&util::js_string_or(body.get("username"), "")?) {
        return Ok(Reply::error(400, "invalid username").into());
    }
    let password_hash = match create_password_hash(body.get("password")) {
        Ok(h) => h,
        Err(Fault::Status(_, m)) => return Ok(Reply::error(400, &m).into()),
        Err(f) => return Err(f),
    };
    let id = util::uuid();
    let diary = i64::from(body.get("diaryEnabled").truthy());
    let username = body.get("username").as_str().map(str::to_string);
    let display_src = if body.get("displayName").truthy() {
        util::js_string(body.get("displayName"))
    } else {
        util::js_string(body.get("username"))
    };
    enum Accept {
        Ok,
        Raced,
    }
    let result = acct.writer.write(|tx| {
        let claimed = tx.execute(
            "UPDATE invitations SET used_at=?1 WHERE token_hash=?2 AND used_at IS NULL AND expires_at>?1",
            (now, &token_hash),
        )?;
        if claimed != 1 {
            return Ok(Accept::Raced);
        }
        // `body.username.toLowerCase()` throwing, or the username taken: "unavailable" (and the
        // claim rolls back with the transaction, as Node's throw does).
        let (Some(username), Ok(display_src)) = (username.as_deref(), display_src.as_ref()) else {
            return Err(StoreError::Sqlite("username".into()));
        };
        tx.execute(
            "INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,created_at,updated_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?8)",
            (&id, username, username.to_lowercase(), util::slice16(display_src, 80), &role, &password_hash, util::random_token(32), now),
        )?;
        tx.execute(
            "INSERT INTO user_features(user_id,diary_enabled,onboarded,updated_at) VALUES(?1,?2,0,?3)",
            (&id, diary, now),
        )?;
        Ok(Accept::Ok)
    });
    match result {
        Ok(Accept::Ok) => {}
        Ok(Accept::Raced) => {
            return Ok(Reply::error(400, "invitation is invalid or expired").into())
        }
        Err(StoreError::Sqlite(_)) => {
            return Ok(Reply::error(409, "username is unavailable").into())
        }
        Err(e) => return Err(e.into()),
    }
    let (user, issued) = acct
        .writer
        .write(|tx| {
            let user = users::public_user_by_id(&tx.reader(), &id)?;
            audit(
                tx,
                "invite.accept",
                Some(&id),
                Some(&id),
                &JValue::obj::<&str>([]),
                now,
            )?;
            let issued = users::issue_session(acct, tx, req, &id, now).map_err(store_fault)?;
            Ok((user, issued))
        })
        .map_err(Fault::from)?;
    Ok(reply_signed_in(
        201,
        user.ok_or(Fault::Internal)?,
        issued,
        None,
    ))
}

/// auth.cjs `completeRecovery(body)`: `Ok(true)` once the account's credentials are replaced.
fn complete_recovery(acct: &Account, body: &JValue, now: i64) -> Result<bool, Fault> {
    let token = util::js_string_or(body.get("token"), "")?;
    let row = acct.writer.read(|r| {
        r.row(
            "SELECT token_hash, user_id FROM recoveries WHERE token_hash=?1 AND used_at IS NULL AND expires_at>?2",
            (util::digest(&token), now),
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
    })?;
    let Some((token_hash, user_id)) = row else {
        return Ok(false);
    };
    // `await createPasswordHash(body.password)` throws its message (the route answers 400 with it).
    let password_hash = create_password_hash(body.get("password"))?;
    let revoked = acct.writer.write(|tx| {
        let r = tx.execute(
            "UPDATE recoveries SET used_at=?1 WHERE token_hash=?2 AND used_at IS NULL",
            (now, &token_hash),
        )?;
        if r != 1 {
            return Ok(None);
        }
        tx.execute(
            "UPDATE users SET password_hash=?1,updated_at=?2,credential_epoch=credential_epoch+1 WHERE id=?3",
            (&password_hash, now, &user_id),
        )?;
        tx.execute("DELETE FROM sessions WHERE user_id=?1", [&user_id])?;
        tx.execute("DELETE FROM device_grants WHERE user_id=?1", [&user_id])?;
        tx.execute("DELETE FROM device_authorizations WHERE user_id=?1", [&user_id])?;
        let recoveries = tx.execute(
            "DELETE FROM recoveries WHERE user_id=?1 AND token_hash<>?2 AND used_at IS NULL AND expires_at>?3",
            (&user_id, &token_hash, now),
        )?;
        let passkeys = tx.execute("DELETE FROM passkeys WHERE user_id=?1", [&user_id])?;
        let app_passwords = tx.execute("DELETE FROM app_passwords WHERE user_id=?1", [&user_id])?;
        let has_connectors = tx
            .reader()
            .row(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name='diary_connectors'",
                [],
                |r| r.get::<_, i64>(0),
            )?
            .is_some();
        let connectors = if has_connectors {
            tx.execute("DELETE FROM diary_connectors WHERE user_id=?1", [&user_id])?
        } else {
            0
        };
        tx.execute("DELETE FROM challenges WHERE user_id=?1", [&user_id])?;
        let revoked = JValue::obj([
            ("recoveries", JValue::from(recoveries as i64)),
            ("passkeys", JValue::from(passkeys as i64)),
            ("appPasswords", JValue::from(app_passwords as i64)),
            ("diaryConnectors", JValue::from(connectors as i64)),
        ]);
        audit(
            tx,
            "recovery.complete",
            Some(&user_id),
            Some(&user_id),
            &JValue::obj([("revoked", revoked.clone())]),
            now,
        )?;
        Ok(Some(revoked))
    })?;
    Ok(revoked.is_some())
}
