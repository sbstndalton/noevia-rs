//! Native-client sign-in (core device-auth.cjs, routes/device-auth.cjs; #555): the RFC 8628 device
//! authorization grant, the refresh grant with reuse detection and its 60 s grace, approval in a
//! signed-in browser, and the per-device grants in Settings. Off unless
//! features.nativeClientAuth is on (every path is then a 404, as Node answers).

use crate::users::{self, audit, rows_js};
use crate::util;
use crate::{read_json, Account, Fault, Outcome, Reply, Request};
use js_json::JValue;
use server_auth::{Credential, Identity};
use server_store::rusqlite::types::Value;
use server_store::{StoreError, WriteTx};

pub const DEVICE_CODE_TTL_MS: i64 = 10 * 60 * 1000;
pub const POLL_INTERVAL_MS: i64 = 5 * 1000;
pub const SLOW_DOWN_STEP_MS: i64 = 5 * 1000;
pub const ACCESS_TTL_MS: i64 = 60 * 60 * 1000;
pub const REFRESH_IDLE_MS: i64 = 7 * 24 * 60 * 60 * 1000;
pub const GRANT_ABSOLUTE_MS: i64 = 30 * 24 * 60 * 60 * 1000;
pub const MAX_PENDING: i64 = 1024;
pub const REFRESH_GRACE_MS: i64 = 60 * 1000;
pub const DEVICE_GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:device_code";
const WINDOW: i64 = 15 * 60 * 1000;
const ACCESS_PREFIX: &str = "nva_";
const REFRESH_PREFIX: &str = "nvr_";

/// routes/device-auth.cjs `DEVICE_API`.
pub fn is_device_api(p: &str) -> bool {
    let Some(rest) = p.strip_prefix("/api/auth/") else {
        return false;
    };
    matches!(
        rest,
        "device/code" | "device/token" | "device/lookup" | "device/approve" | "devices"
    ) || rest
        .strip_prefix("devices/")
        .is_some_and(|id| !id.is_empty() && !id.contains('/'))
}

fn oauth(status: u16, error: &str, description: &str) -> Outcome {
    Reply::json(
        status,
        &JValue::obj([
            ("error", JValue::from(error)),
            ("error_description", JValue::from(description)),
        ]),
    )
    .into()
}

fn too_many(what: &str) -> Outcome {
    oauth(
        429,
        "slow_down",
        &format!("Too many {what}. Try again later."),
    )
}

fn limited(acct: &Account, key: &str, limit: u64, now: i64) -> Result<bool, Fault> {
    Ok(acct
        .device_rate
        .lock()
        .map_err(|_| Fault::Internal)?
        .limited(key, limit, WINDOW, now))
}

/// routes/device-auth.cjs `open`.
pub fn open(acct: &Account, req: &Request, now: i64) -> Result<Option<Outcome>, Fault> {
    let p = req.path.as_str();
    let is_device_path = is_device_api(p) || p == "/device" || p == "/device/";
    if !acct.native_client_auth()? {
        return Ok(is_device_path.then(|| Reply::error(404, "not found").into()));
    }
    if p.starts_with("/api/")
        && !server_auth::request::bearer_token(&req.creds).is_empty()
        && server_auth::request::has_session_cookie(&req.creds)
    {
        return Ok(Some(
            Reply::json(
                400,
                &JValue::obj([
                    (
                        "error",
                        JValue::from("Send either a session cookie or a bearer token, not both."),
                    ),
                    ("code", JValue::from("ambiguous_credentials")),
                ]),
            )
            .into(),
        ));
    }
    if p != "/api/auth/device/code" && p != "/api/auth/device/token" {
        return Ok(None);
    }
    if req.method != "POST" {
        return Ok(Some(Reply::error(405, "method not allowed").into()));
    }
    if !acct
        .writer
        .read(|r| acct.auth.origin_valid(r, &req.creds))?
    {
        return Ok(Some(Reply::error(403, "origin not allowed").into()));
    }
    let Some(body) = token_body(req)? else {
        return Ok(Some(oauth(
            400,
            "invalid_request",
            "The body must be JSON or form-encoded.",
        )));
    };
    Ok(Some(if p == "/api/auth/device/code" {
        start(acct, req, &body, now)?
    } else {
        token(acct, req, &body, now)?
    }))
}

/// routes/device-auth.cjs `readTokenBody`: form or JSON, 16 KiB.
fn token_body(req: &Request) -> Result<Option<JValue>, Fault> {
    if req.body.over || req.body.bytes.len() > 16 * 1024 {
        return Err(Fault::Status(413, "Request exceeds size limit".into()));
    }
    let raw = String::from_utf8_lossy(&req.body.bytes);
    if req
        .content_type
        .to_lowercase()
        .starts_with("application/x-www-form-urlencoded")
    {
        // Object.fromEntries(new URLSearchParams(raw)): a repeated name keeps its first place
        // and its last value.
        let mut out: Vec<(String, JValue)> = Vec::new();
        for (k, v) in url::form_urlencoded::parse(raw.as_bytes()) {
            let v = JValue::from(v.into_owned());
            match out.iter_mut().find(|(key, _)| *key == k) {
                Some(slot) => slot.1 = v,
                None => out.push((k.into_owned(), v)),
            }
        }
        return Ok(Some(JValue::Obj(out)));
    }
    if raw.is_empty() {
        return Ok(Some(JValue::Obj(Vec::new())));
    }
    Ok(js_json::parse(&raw).ok().filter(JValue::is_object))
}

/// device-auth.cjs `start(req, body)`: POST /api/auth/device/code.
fn start(acct: &Account, req: &Request, body: &JValue, now: i64) -> Result<Outcome, Fault> {
    let mut name = util::clean_client_name(body.get("client_name"));
    if name.is_empty() {
        name = util::clean_client_name(body.get("client_id"));
    }
    if name.is_empty() {
        return Ok(oauth(400, "invalid_request", "client_name is required."));
    }
    let address = req.client_ip.as_str();
    if limited(acct, "device-code:global", 200, now)?
        || limited(
            acct,
            &format!("device-code:name:{}", name.to_lowercase()),
            10,
            now,
        )?
        || (acct.settings.trust_proxy
            && limited(acct, &format!("device-code:address:{address}"), 10, now)?)
    {
        return Ok(too_many("sign-in requests"));
    }
    let device_code = util::random_token(32);
    let created = acct.writer.write(|tx| {
        tx.execute("DELETE FROM device_authorizations WHERE expires_at<=?1", [now])?;
        let pending: i64 = tx
            .reader()
            .row("SELECT count(*) FROM device_authorizations WHERE status='pending'", [], |r| r.get(0))?
            .unwrap_or(0);
        if pending >= MAX_PENDING {
            return Ok(None);
        }
        let mut code = None;
        for _ in 0..5 {
            let candidate = util::random_user_code();
            let taken = tx
                .reader()
                .row(
                    "SELECT 1 FROM device_authorizations WHERE user_code_hash=?1",
                    [util::digest(&candidate)],
                    |r| r.get::<_, i64>(0),
                )?
                .is_some();
            if !taken {
                code = Some(candidate);
                break;
            }
        }
        let Some(code) = code else { return Ok(None) };
        tx.execute(
            "INSERT INTO device_authorizations(device_code_hash,user_code_hash,client_name,ip,user_agent,created_at,expires_at,interval_ms,status) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,'pending')",
            (
                util::digest(&device_code),
                util::digest(&code),
                &name,
                address,
                util::slice16(&req.user_agent, 300),
                now,
                now + DEVICE_CODE_TTL_MS,
                POLL_INTERVAL_MS,
            ),
        )?;
        Ok(Some(code))
    })?;
    let Some(code) = created else {
        return Ok(oauth(
            503,
            "temporarily_unavailable",
            "Device sign-in is busy. Try again in a few minutes.",
        ));
    };
    let origin = acct.writer.read(|r| Ok(acct.origin(r)))??;
    let base = if origin.is_empty() {
        acct.settings.public_origin.clone()
    } else {
        origin
    };
    let uri = format!("{}/device", base.trim_end_matches('/'));
    let formatted = util::format_user_code(&code);
    Ok(Reply::json(
        200,
        &JValue::obj([
            ("device_code", JValue::from(device_code)),
            ("user_code", JValue::from(formatted.as_str())),
            ("verification_uri", JValue::from(uri.as_str())),
            (
                "verification_uri_complete",
                JValue::from(format!("{uri}?code={formatted}")),
            ),
            (
                "expires_in",
                JValue::Num((DEVICE_CODE_TTL_MS / 1000) as f64),
            ),
            ("interval", JValue::Num((POLL_INTERVAL_MS / 1000) as f64)),
        ]),
    )
    .into())
}

/// device-auth.cjs `issuePair`: an access and a refresh token for `grant`, inside a transaction.
fn issue_pair(
    tx: &WriteTx<'_>,
    grant: &str,
    grant_expires: i64,
    at: i64,
) -> Result<(String, JValue), StoreError> {
    let access = format!("{ACCESS_PREFIX}{}", util::random_token(32));
    let refresh = format!("{REFRESH_PREFIX}{}", util::random_token(32));
    let access_expires = (at + ACCESS_TTL_MS).min(grant_expires);
    let insert = "INSERT INTO device_tokens(token_hash,grant_id,kind,created_at,expires_at) VALUES(?1,?2,?3,?4,?5)";
    tx.execute(
        insert,
        (util::digest(&access), grant, "access", at, access_expires),
    )?;
    tx.execute(
        insert,
        (
            util::digest(&refresh),
            grant,
            "refresh",
            at,
            (at + REFRESH_IDLE_MS).min(grant_expires),
        ),
    )?;
    let expires_in = ((access_expires - at) as f64 / 1000.0).floor().max(1.0);
    Ok((
        util::digest(&refresh),
        JValue::obj([
            ("access_token", JValue::from(access)),
            ("token_type", JValue::from("Bearer")),
            ("expires_in", JValue::Num(expires_in)),
            ("refresh_token", JValue::from(refresh)),
            ("scope", JValue::from("api")),
        ]),
    ))
}

fn num(v: &Value) -> Option<i64> {
    match v {
        Value::Integer(i) => Some(*i),
        Value::Real(f) => Some(*f as i64),
        _ => None,
    }
}

/// device-auth.cjs `token(req, body)`: POST /api/auth/device/token.
fn token(acct: &Account, req: &Request, body: &JValue, now: i64) -> Result<Outcome, Fault> {
    let grant = body.get("grant_type").as_str();
    let device = grant == Some(DEVICE_GRANT_TYPE);
    if !device && grant != Some("refresh_token") {
        return Ok(oauth(
            400,
            "unsupported_grant_type",
            "Use the device_code or refresh_token grant.",
        ));
    }
    let credential = body.get(if device {
        "device_code"
    } else {
        "refresh_token"
    });
    let Some(credential) = credential
        .as_str()
        .filter(|c| !c.is_empty() && js_json::js_len(c) <= 256)
    else {
        return Ok(oauth(
            400,
            "invalid_request",
            if device {
                "device_code is required."
            } else {
                "refresh_token is required."
            },
        ));
    };
    let hash = util::digest(credential);
    let known = acct.writer.read(|r| {
        if device {
            Ok(r.row(
                "SELECT 1 FROM device_authorizations WHERE device_code_hash=?1",
                [&hash],
                |x| x.get::<_, i64>(0),
            )?
            .is_some())
        } else {
            Ok(credential.starts_with(REFRESH_PREFIX)
                && r.row(
                    "SELECT 1 FROM device_tokens WHERE token_hash=?1",
                    [&hash],
                    |x| x.get::<_, i64>(0),
                )?
                .is_some())
        }
    })?;
    if !known {
        let bucket = if acct.settings.trust_proxy {
            format!("device-token:unknown:{}", req.client_ip)
        } else {
            "device-token:unknown".to_string()
        };
        if limited(acct, &bucket, 300, now)? {
            return Ok(too_many("token requests with unknown credentials"));
        }
        return Ok(oauth(
            400,
            "invalid_grant",
            if device {
                "Unknown device code."
            } else {
                "Unknown refresh token."
            },
        ));
    }
    if limited(acct, &format!("device-token:credential:{hash}"), 150, now)? {
        return Ok(too_many("token requests"));
    }
    if device
        && !limited(acct, &format!("device-token:global-share:{hash}"), 20, now)?
        && limited(acct, "device-token:global", 5000, now)?
    {
        return Ok(too_many("token requests"));
    }
    if device {
        exchange_device_code(acct, &hash, now)
    } else {
        exchange_refresh(acct, credential, now, false)
    }
}

fn exchange_device_code(acct: &Account, hash: &str, at: i64) -> Result<Outcome, Fault> {
    let row = acct.writer.read(|r| {
        r.row(
            "SELECT device_code_hash, expires_at, status, last_poll_at, interval_ms, user_id, client_name, ip, user_agent FROM device_authorizations WHERE device_code_hash=?1",
            [hash],
            |x| {
                Ok((
                    x.get::<_, String>(0)?,
                    x.get::<_, i64>(1)?,
                    x.get::<_, String>(2)?,
                    x.get::<_, Option<i64>>(3)?,
                    x.get::<_, i64>(4)?,
                    x.get::<_, Option<String>>(5)?,
                    x.get::<_, String>(6)?,
                    x.get::<_, String>(7)?,
                    x.get::<_, String>(8)?,
                ))
            },
        )
    })?;
    let Some((dch, expires, status, last_poll, interval, user_id, client_name, ip, ua)) = row
    else {
        return Ok(oauth(400, "invalid_grant", "Unknown device code."));
    };
    let delete = |acct: &Account| {
        acct.writer.write(|tx| {
            tx.execute(
                "DELETE FROM device_authorizations WHERE device_code_hash=?1",
                [&dch],
            )
        })
    };
    if expires <= at {
        delete(acct)?;
        return Ok(oauth(
            400,
            "expired_token",
            "The sign-in request expired. Start again.",
        ));
    }
    if status == "denied" {
        delete(acct)?;
        return Ok(oauth(
            400,
            "access_denied",
            "The sign-in request was denied.",
        ));
    }
    if status == "pending" {
        // RFC 8628 §3.5: polling faster than the interval earns slow_down and a longer interval.
        let fast = last_poll.is_some_and(|l| l != 0 && at - l < interval);
        let next = if fast {
            interval + SLOW_DOWN_STEP_MS
        } else {
            interval
        };
        acct.writer.write(|tx| {
            tx.execute(
                "UPDATE device_authorizations SET last_poll_at=?1, interval_ms=?2 WHERE device_code_hash=?3",
                (at, next, &dch),
            )
        })?;
        return Ok(if fast {
            oauth(400, "slow_down", "Polling too fast.")
        } else {
            oauth(
                400,
                "authorization_pending",
                "Waiting for approval in the browser.",
            )
        });
    }
    // Approved: the device code is single use; claim it and mint the grant atomically.
    let issued = acct.writer.write(|tx| {
        if tx.execute("DELETE FROM device_authorizations WHERE device_code_hash=?1", [&dch])? != 1 {
            return Ok(None);
        }
        let Some(uid) = user_id.as_deref() else { return Ok(None) };
        let user = tx.reader().row("SELECT id, disabled_at FROM users WHERE id=?1", [uid], |x| {
            Ok((x.get::<_, String>(0)?, x.get::<_, Value>(1)?))
        })?;
        let Some((uid, disabled)) = user.filter(|(_, d)| !server_auth::js::truthy(d)) else {
            return Ok(None);
        };
        let _ = disabled;
        let grant = util::random_hex(16);
        let expires_at = at + GRANT_ABSOLUTE_MS;
        tx.execute(
            "INSERT INTO device_grants(id,user_id,client_name,created_at,last_used_at,expires_at,ip,user_agent) VALUES(?1,?2,?3,?4,?4,?5,?6,?7)",
            (&grant, &uid, &client_name, at, expires_at, &ip, &ua),
        )?;
        let (_, tokens) = issue_pair(tx, &grant, expires_at, at)?;
        audit(
            tx,
            "device.token",
            Some(&uid),
            Some(&uid),
            &JValue::obj([("grantId", JValue::from(grant.as_str())), ("clientName", JValue::from(client_name.as_str()))]),
            at,
        )?;
        Ok(Some(tokens))
    })?;
    Ok(match issued {
        Some(tokens) => Reply::json(200, &tokens).into(),
        None => oauth(
            400,
            "invalid_grant",
            "The sign-in request is no longer valid.",
        ),
    })
}

/// device-auth.cjs `exchangeRefreshToken` (single use, reuse revokes the grant, a 60 s grace
/// while the successor is unused).
fn exchange_refresh(
    acct: &Account,
    token: &str,
    at: i64,
    retrying: bool,
) -> Result<Outcome, Fault> {
    let hash = util::digest(token);
    let row = if token.starts_with(REFRESH_PREFIX) {
        acct.writer.read(|r| {
            r.row(
                "SELECT t.token_hash, t.kind, t.expires_at, t.used_at, t.replaced_by, g.id, g.user_id, g.client_name, g.expires_at, g.last_used_at
                 FROM device_tokens t JOIN device_grants g ON g.id=t.grant_id WHERE t.token_hash=?1",
                [&hash],
                |x| {
                    Ok((
                        x.get::<_, String>(0)?,
                        x.get::<_, String>(1)?,
                        x.get::<_, Value>(2)?,
                        x.get::<_, Option<i64>>(3)?,
                        x.get::<_, Option<String>>(4)?,
                        x.get::<_, String>(5)?,
                        x.get::<_, String>(6)?,
                        x.get::<_, String>(7)?,
                        x.get::<_, Value>(8)?,
                        x.get::<_, Value>(9)?,
                    ))
                },
            )
        })?
    } else {
        None
    };
    let Some((
        token_hash,
        kind,
        token_expires,
        used_at,
        replaced_by,
        grant,
        user_id,
        client_name,
        grant_expires,
        last_used,
    )) = row.filter(|r| r.1 == "refresh")
    else {
        return Ok(oauth(400, "invalid_grant", "Unknown refresh token."));
    };
    let _ = kind;
    let detail = || {
        JValue::obj([
            ("grantId", JValue::from(grant.as_str())),
            ("clientName", JValue::from(client_name.as_str())),
        ])
    };
    let revoke_for_reuse = |acct: &Account| -> Result<Outcome, Fault> {
        acct.writer.write(|tx| {
            tx.execute("DELETE FROM device_grants WHERE id=?1", [&grant])?;
            audit(
                tx,
                "device.refresh_reuse",
                None,
                Some(&user_id),
                &detail(),
                at,
            )
        })?;
        Ok(oauth(
            400,
            "invalid_grant",
            "This refresh token was already used. The device was signed out.",
        ))
    };
    let used = used_at.is_some_and(|u| u != 0);
    let in_grace = used
        && replaced_by.as_deref().is_some_and(|s| !s.is_empty())
        && used_at.is_some_and(|u| at - u <= REFRESH_GRACE_MS)
        && acct
            .writer
            .read(|r| {
                r.row(
                    "SELECT used_at FROM device_tokens WHERE token_hash=?1",
                    [replaced_by.as_deref().unwrap_or("")],
                    |x| x.get::<_, Option<i64>>(0),
                )
            })?
            .is_some_and(|u| u.is_none());
    if used && !in_grace {
        return revoke_for_reuse(acct);
    }
    let (ge, te, lu) = (
        num(&grant_expires).unwrap_or(0),
        num(&token_expires).unwrap_or(0),
        num(&last_used).unwrap_or(0),
    );
    if ge <= at || te <= at || lu + REFRESH_IDLE_MS <= at {
        acct.writer
            .write(|tx| tx.execute("DELETE FROM device_grants WHERE id=?1", [&grant]))?;
        return Ok(oauth(
            400,
            "invalid_grant",
            "The device sign-in expired. Sign in again.",
        ));
    }
    let active = acct.writer.read(|r| {
        r.row(
            "SELECT disabled_at FROM users WHERE id=?1",
            [&user_id],
            |x| x.get::<_, Value>(0),
        )
    })?;
    if !matches!(&active, Some(d) if !server_auth::js::truthy(d)) {
        return Ok(oauth(400, "invalid_grant", "The account is not available."));
    }
    // R1: refused before rotating, so a refused refresh leaves the current pair working.
    if !retrying && limited(acct, &format!("device-token:grant:{grant}"), 30, at)? {
        return Ok(too_many("refreshes for this device"));
    }
    let rotated = acct.writer.write(|tx| {
        if in_grace {
            let successor = replaced_by.as_deref().unwrap_or("");
            if tx.execute(
                "UPDATE device_tokens SET used_at=?1, replaced_by=NULL WHERE token_hash=?2 AND used_at IS NULL",
                (at, successor),
            )? != 1
            {
                return Ok(None);
            }
        } else if tx.execute(
            "UPDATE device_tokens SET used_at=?1, replaced_by=?2 WHERE token_hash=?3 AND used_at IS NULL",
            (at, Option::<String>::None, &token_hash),
        )? != 1
        {
            return Ok(None);
        }
        tx.execute("DELETE FROM device_tokens WHERE grant_id=?1 AND kind='access'", [&grant])?;
        tx.execute("UPDATE device_grants SET last_used_at=?1 WHERE id=?2", (at, &grant))?;
        let (refresh_hash, body) = issue_pair(tx, &grant, ge, at)?;
        tx.execute("UPDATE device_tokens SET replaced_by=?1 WHERE token_hash=?2", (&refresh_hash, &token_hash))?;
        tx.execute(
            "DELETE FROM device_tokens WHERE grant_id=?1 AND kind='refresh' AND used_at IS NOT NULL AND replaced_by IS NOT NULL AND used_at<=?2 AND token_hash<>?3",
            (&grant, at - REFRESH_GRACE_MS, &token_hash),
        )?;
        if in_grace {
            audit(tx, "device.refresh_grace", None, Some(&user_id), &detail(), at)?;
        }
        Ok(Some(body))
    })?;
    if let Some(body) = rotated {
        return Ok(Reply::json(200, &body).into());
    }
    // Lost a race: look again once, so a concurrent first use is judged by the same grace rule.
    if retrying {
        revoke_for_reuse(acct)
    } else {
        exchange_refresh(acct, token, at, true)
    }
}

/// routes/device-auth.cjs `account`.
pub fn account(
    acct: &Account,
    req: &Request,
    authn: &Identity,
    now: i64,
) -> Result<Option<Outcome>, Fault> {
    if !acct.native_client_auth()? {
        return Ok(None);
    }
    let (p, m) = (req.path.as_str(), req.method.as_str());
    if let Credential::Device {
        grant_id,
        client_name,
        expires_at,
    } = &authn.credential
    {
        if p == "/api/auth/session" && m == "GET" {
            return Ok(Some(
                Reply::json(
                    200,
                    &JValue::obj([
                        ("user", users::public_user(&authn.user)),
                        ("csrfToken", JValue::Null),
                        ("legacy", JValue::from(false)),
                        ("accountRole", JValue::from(authn.account_role.as_str())),
                        (
                            "device",
                            JValue::obj([
                                ("id", JValue::from(grant_id.as_str())),
                                ("clientName", JValue::from(client_name.as_str())),
                                ("expiresAt", JValue::from(*expires_at)),
                            ]),
                        ),
                    ]),
                )
                .into(),
            ));
        }
        if p == "/api/auth/logout" && m == "POST" {
            revoke(
                acct,
                &authn.user.id,
                grant_id,
                &authn.user.id,
                "sign-out",
                now,
            )?;
            return Ok(Some(
                Reply::json(200, &JValue::obj([("ok", JValue::from(true))])).into(),
            ));
        }
        return Ok(None);
    }
    if !is_device_api(p) {
        return Ok(None);
    }
    let Credential::Session {
        credential_epoch, ..
    } = &authn.credential
    else {
        return Ok(Some(
            Reply::json(
                403,
                &JValue::obj([
                    (
                        "error",
                        JValue::from("This needs a signed-in browser session."),
                    ),
                    ("code", JValue::from("browser_session_required")),
                ]),
            )
            .into(),
        ));
    };
    let uid = authn.user.id.as_str();
    let trusted = acct.settings.trust_proxy;
    if p == "/api/auth/device/lookup" && m == "POST" {
        let body = read_json(req, crate::BODY_LIMIT)?;
        if verify_limited(acct, uid, now)? {
            return Ok(Some(
                Reply::error(429, "Too many attempts. Wait a few minutes and try again.").into(),
            ));
        }
        let Some(row) = pending(acct, body.opt("user_code"), now)? else {
            return Ok(Some(
                Reply::error(404, "That code is not valid or has expired.").into(),
            ));
        };
        return Ok(Some(
            Reply::json(
                200,
                &JValue::obj([
                    ("clientName", JValue::from(row.client_name.as_str())),
                    ("userCode", JValue::from(util::format_user_code(&row.code))),
                    ("requestedAt", JValue::from(row.created_at)),
                    ("expiresAt", JValue::from(row.expires_at)),
                    (
                        "ip",
                        if trusted {
                            JValue::from(row.ip.as_str())
                        } else {
                            JValue::Null
                        },
                    ),
                    ("userAgent", JValue::from(row.user_agent.as_str())),
                ]),
            )
            .into(),
        ));
    }
    if p == "/api/auth/device/approve" && m == "POST" {
        let body = read_json(req, crate::BODY_LIMIT)?;
        let Some(approve) = body.opt("approve").as_bool() else {
            return Ok(Some(
                Reply::error(400, "approve must be true or false").into(),
            ));
        };
        if verify_limited(acct, uid, now)? {
            return Ok(Some(
                Reply::error(429, "Too many attempts. Wait a few minutes and try again.").into(),
            ));
        }
        let Some(row) = pending(acct, body.get("user_code"), now)? else {
            return Ok(Some(
                Reply::error(404, "That code is not valid or has expired.").into(),
            ));
        };
        let decided = acct.writer.write(|tx| {
            if approve {
                // Approvals fail closed: the epoch read with the session must still be the account's.
                let Some(epoch) = credential_epoch else { return Ok(false) };
                let current = tx.reader().row(
                    "SELECT 1 FROM users WHERE id=?1 AND credential_epoch=?2 AND disabled_at IS NULL",
                    (uid, epoch),
                    |x| x.get::<_, i64>(0),
                )?;
                if current.is_none() {
                    return Ok(false);
                }
            }
            let n = tx.execute(
                "UPDATE device_authorizations SET status=?1, user_id=?2, decided_at=?3 WHERE device_code_hash=?4 AND status='pending' AND expires_at>?3",
                (if approve { "approved" } else { "denied" }, approve.then_some(uid), now, &row.device_code_hash),
            )?;
            if n != 1 {
                return Ok(false);
            }
            audit(
                tx,
                if approve { "device.approve" } else { "device.deny" },
                Some(uid),
                Some(uid),
                &JValue::obj([("clientName", JValue::from(row.client_name.as_str())), ("ip", JValue::from(row.ip.as_str()))]),
                now,
            )?;
            Ok(true)
        })?;
        if !decided {
            return Ok(Some(
                Reply::error(404, "That code is not valid or has expired.").into(),
            ));
        }
        return Ok(Some(
            Reply::json(
                200,
                &JValue::obj([
                    ("ok", JValue::from(true)),
                    ("approved", JValue::from(approve)),
                    ("clientName", JValue::from(row.client_name.as_str())),
                ]),
            )
            .into(),
        ));
    }
    if p == "/api/auth/devices" && m == "GET" {
        let list = acct.writer.write(|tx| {
            tx.execute(
                "DELETE FROM device_grants WHERE expires_at<=?1 OR last_used_at<=?2",
                (now, now - REFRESH_IDLE_MS),
            )?;
            rows_js(&tx.reader(), "SELECT id,client_name AS clientName,created_at AS createdAt,last_used_at AS lastUsedAt,expires_at AS expiresAt,ip,user_agent AS userAgent FROM device_grants WHERE user_id=?1 AND expires_at>?2 ORDER BY last_used_at DESC", (uid, now))
        })?;
        let list = list
            .into_iter()
            .map(|d| match d {
                JValue::Obj(items) if !trusted => JValue::Obj(
                    items
                        .into_iter()
                        .map(|(k, v)| if k == "ip" { (k, JValue::Null) } else { (k, v) })
                        .collect(),
                ),
                other => other,
            })
            .collect();
        return Ok(Some(
            Reply::json(200, &JValue::obj([("devices", JValue::Arr(list))])).into(),
        ));
    }
    if let Some(id) = p.strip_prefix("/api/auth/devices/") {
        if m == "DELETE" {
            let hex32 =
                id.len() == 32 && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
            let done = hex32 && revoke(acct, uid, id, uid, "settings", now)?;
            return Ok(Some(if done {
                Reply::json(200, &JValue::obj([("ok", JValue::from(true))])).into()
            } else {
                Reply::error(404, "not found").into()
            }));
        }
    }
    Ok(Some(Reply::error(405, "method not allowed").into()))
}

fn verify_limited(acct: &Account, uid: &str, now: i64) -> Result<bool, Fault> {
    limited(acct, &format!("device-verify:{uid}"), 20, now)
}

struct Pending {
    device_code_hash: String,
    code: String,
    client_name: String,
    created_at: i64,
    expires_at: i64,
    ip: String,
    user_agent: String,
}

/// device-auth.cjs `pendingByUserCode`.
fn pending(acct: &Account, user_code: &JValue, now: i64) -> Result<Option<Pending>, Fault> {
    let code = util::normalize_user_code(user_code)?;
    if code.is_empty() {
        return Ok(None);
    }
    let row = acct.writer.read(|r| {
        r.row(
            "SELECT device_code_hash, status, client_name, created_at, expires_at, ip, user_agent FROM device_authorizations WHERE user_code_hash=?1",
            [util::digest(&code)],
            |x| {
                Ok((
                    x.get::<_, String>(0)?,
                    x.get::<_, String>(1)?,
                    x.get::<_, String>(2)?,
                    x.get::<_, i64>(3)?,
                    x.get::<_, i64>(4)?,
                    x.get::<_, String>(5)?,
                    x.get::<_, String>(6)?,
                ))
            },
        )
    })?;
    Ok(row.filter(|r| r.1 == "pending" && r.4 > now).map(
        |(device_code_hash, _, client_name, created_at, expires_at, ip, user_agent)| Pending {
            device_code_hash,
            code,
            client_name,
            created_at,
            expires_at,
            ip,
            user_agent,
        },
    ))
}

/// device-auth.cjs `revoke(userId, grantId, actorId, reason)`.
fn revoke(
    acct: &Account,
    uid: &str,
    grant: &str,
    actor: &str,
    reason: &str,
    now: i64,
) -> Result<bool, Fault> {
    Ok(acct.writer.write(|tx| {
        let row = tx.reader().row(
            "SELECT id, client_name FROM device_grants WHERE id=?1 AND user_id=?2",
            (grant, uid),
            |x| Ok((x.get::<_, String>(0)?, x.get::<_, String>(1)?)),
        )?;
        let Some((id, client_name)) = row else {
            return Ok(false);
        };
        tx.execute(
            "DELETE FROM device_grants WHERE id=?1 AND user_id=?2",
            (&id, uid),
        )?;
        audit(
            tx,
            "device.revoke",
            Some(actor),
            Some(uid),
            &JValue::obj([
                ("grantId", JValue::from(id.as_str())),
                ("clientName", JValue::from(client_name.as_str())),
                ("reason", JValue::from(reason)),
            ]),
            now,
        )?;
        Ok(true)
    })?)
}
