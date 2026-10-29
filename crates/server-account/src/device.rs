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

/// One request's earlier coherent generation. OFF/re-enable, release, expiry or
/// process replacement invalidates it; never recapture a new admission at commit.
struct Issuance<'a> {
    acct: &'a Account,
    req: &'a Request,
    admission: crate::feature_context::Admission,
    clock: &'a dyn Fn() -> i64,
    browser: Option<&'a Identity>,
    #[cfg(test)]
    before_write: Option<&'a dyn Fn()>,
}
impl<'a> Issuance<'a> {
    fn capture(
        acct: &'a Account,
        req: &'a Request,
        clock: &'a dyn Fn() -> i64,
        browser: Option<&'a Identity>,
    ) -> Result<Self, Fault> {
        let controller = acct.feature_context.as_ref().ok_or(Fault::Unavailable)?;
        let admission = acct.writer.read(|reader| {
            let view = controller.view(reader)?;
            if !crate::features::evaluate(
                reader,
                &view,
                server_features::Feature::NativeClientAuth,
            )?
            .enabled
            {
                return Err(context_unavailable());
            }
            Ok(view.admission)
        })?;
        Ok(Self {
            acct,
            req,
            admission,
            clock,
            browser,
            #[cfg(test)]
            before_write: None,
        })
    }
    fn check(&self, tx: &WriteTx<'_>, now: i64, deadline: Option<i64>) -> Result<(), StoreError> {
        let reader = tx.reader();
        let controller = self
            .acct
            .feature_context
            .as_ref()
            .ok_or_else(context_unavailable)?;
        let view = controller.view(&reader)?;
        if view.admission != self.admission
            || !crate::features::evaluate(
                &reader,
                &view,
                server_features::Feature::NativeClientAuth,
            )?
            .enabled
            || deadline.is_some_and(|at| now >= at)
            || !self.acct.auth.origin_valid(&reader, &self.req.creds)?
        {
            return Err(context_unavailable());
        }
        if let Some(original) = self.browser {
            match self
                .acct
                .auth
                .gate_in(&reader, &self.req.creds, &self.req.path, now)?
            {
                Ok(current)
                    if current.user.id == original.user.id
                        && current.credential == original.credential => {}
                _ => return Err(context_unavailable()),
            }
        }
        Ok(())
    }
    fn write<T>(
        &self,
        deadline: Option<i64>,
        action: impl FnOnce(&WriteTx<'_>, i64) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        let controller = self
            .acct
            .feature_context
            .as_ref()
            .ok_or_else(context_unavailable)?;
        #[cfg(test)]
        if let Some(probe) = self.before_write {
            probe();
        }
        controller.mutate(&self.acct.writer, self.admission, false, |tx| {
            let now = (self.clock)();
            self.check(tx, now, deadline)?;
            let result = action(tx, now)?;
            self.check(tx, (self.clock)(), deadline)?;
            Ok(result)
        })
    }
}
fn active_user(tx: &WriteTx<'_>, uid: &str) -> Result<bool, StoreError> {
    Ok(tx
        .reader()
        .row("SELECT disabled_at FROM users WHERE id=?1", [uid], |r| {
            r.get::<_, Value>(0)
        })?
        .is_some_and(|v| !server_auth::js::truthy(&v)))
}
fn context_unavailable() -> StoreError {
    StoreError::NotReady("device feature context is unavailable".into())
}

/// routes/device-auth.cjs `open`.
pub fn open(
    acct: &Account,
    req: &Request,
    now: i64,
    current: &dyn Fn() -> i64,
) -> Result<Option<Outcome>, Fault> {
    let p = req.path.as_str();
    let is_device_path = is_device_api(p) || p == "/device" || p == "/device/";
    if !acct.native_client_auth()? {
        return Ok(is_device_path.then(|| Reply::error(404, "not found").into()));
    }
    // Do not put unrelated browser sign-in/setup behind a producer outage.
    // Device paths, however, cannot run ahead of committed startup cleanup.
    let issuance = if is_device_path {
        Some(Issuance::capture(acct, req, current, None)?)
    } else {
        None
    };
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
    let issuance = issuance.as_ref().ok_or(Fault::Unavailable)?;
    Ok(Some(if p == "/api/auth/device/code" {
        start(acct, req, &body, now, issuance)?
    } else {
        token(acct, req, &body, now, issuance)?
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
fn start(
    acct: &Account,
    req: &Request,
    body: &JValue,
    now: i64,
    issuance: &Issuance<'_>,
) -> Result<Outcome, Fault> {
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
    let created = issuance.write(None, |tx, now| {
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
fn token(
    acct: &Account,
    req: &Request,
    body: &JValue,
    now: i64,
    issuance: &Issuance<'_>,
) -> Result<Outcome, Fault> {
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
        exchange_device_code(acct, &hash, now, issuance)
    } else {
        exchange_refresh(acct, credential, now, false, issuance)
    }
}

fn exchange_device_code(
    acct: &Account,
    hash: &str,
    at: i64,
    issuance: &Issuance<'_>,
) -> Result<Outcome, Fault> {
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
    let issued = issuance.write(Some(expires), |tx, at| {
        // Re-read the actual approved row after Writer admission. A previous
        // snapshot is not permission to resurrect a consumed/revoked code.
        let live=tx.reader().row("SELECT user_id,client_name,ip,user_agent FROM device_authorizations WHERE device_code_hash=?1 AND status='approved' AND expires_at>?2",(&dch,at),|r|Ok((r.get::<_,Option<String>>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?)))?;
        if live!=Some((user_id.clone(),client_name.clone(),ip.clone(),ua.clone())) {return Ok(None);}
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
        if !active_user(tx,&uid)? {return Err(context_unavailable());}
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
    issuance: &Issuance<'_>,
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
    let rotated = issuance.write(Some(ge.min(te).min(lu.saturating_add(REFRESH_IDLE_MS))), |tx, at| {
        let live=tx.reader().row(
            "SELECT t.expires_at,t.used_at,t.replaced_by,g.expires_at,g.last_used_at,u.disabled_at FROM device_tokens t JOIN device_grants g ON g.id=t.grant_id JOIN users u ON u.id=g.user_id WHERE t.token_hash=?1 AND t.kind='refresh' AND g.id=?2 AND g.user_id=?3",
            (&token_hash,&grant,&user_id),|r|Ok((r.get::<_,Value>(0)?,r.get::<_,Option<i64>>(1)?,r.get::<_,Option<String>>(2)?,r.get::<_,Value>(3)?,r.get::<_,Value>(4)?,r.get::<_,Value>(5)?)))?;
        if !matches!(live,Some((ref expiry,used,ref replaced,ref grant_expiry,ref last,ref disabled)) if expiry==&token_expires && used==used_at && replaced==&replaced_by && grant_expiry==&grant_expires && last==&last_used && !server_auth::js::truthy(disabled)) {return Ok(None);}
        if in_grace && used_at.is_none_or(|used|at.saturating_sub(used)>REFRESH_GRACE_MS) {return Ok(None);}

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
        if !active_user(tx,&user_id)? {return Err(context_unavailable());}
        Ok(Some(body))
    })?;
    if let Some(body) = rotated {
        return Ok(Reply::json(200, &body).into());
    }
    // Lost a race: look again once, so a concurrent first use is judged by the same grace rule.
    if retrying {
        revoke_for_reuse(acct)
    } else {
        exchange_refresh(acct, token, (issuance.clock)(), true, issuance)
    }
}

/// routes/device-auth.cjs `account`.
pub fn account(
    acct: &Account,
    req: &Request,
    authn: &Identity,
    now: i64,
    current: &dyn Fn() -> i64,
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
        let issuance = Issuance::capture(acct, req, current, Some(authn))?;
        let decided = issuance.write(Some(row.expires_at), |tx, now| {
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

#[cfg(test)]
mod issuance_tests {
    use super::*;
    use crate::{
        feature_context::{
            self, source_fingerprint, BootFacts, BrowserFact, Cancellation, ContextSource, Control,
            Controller, FeatureRuntime, Registration, Snapshot,
        },
        Body, Settings,
    };
    use server_auth::{AuthConfig, Authenticator, Creds};
    use server_store::{db_path, rusqlite::Connection, RustAuth, RustFeatureSettings, Writer};
    use std::{
        sync::{
            atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering},
            Arc, Barrier,
        },
        time::{Duration, Instant},
    };
    use uuid::Uuid;
    type Error = Box<dyn std::error::Error>;
    const T0: i64 = 1_800_000_000_000;
    const UID: &str = "00000000-0000-4000-8000-000000000001";
    const ORIGIN: &str = "https://noevia.example.test";
    struct Fixture {
        _dir: tempfile::TempDir,
        node: Connection,
        acct: Account,
        req: Request,
        controller: Arc<Controller>,
        boot: Uuid,
        session: Uuid,
    }
    fn fixture() -> Result<Fixture, Error> {
        let dir = tempfile::tempdir()?;
        let node = Connection::open(db_path(dir.path()))?;
        node.execute_batch(include_str!(
            "../../server-store/tests/fixtures/node-schema.sql"
        ))?;
        node.execute("INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,created_at,updated_at) VALUES(?1,'Synthetic','synthetic','Synthetic','admin','unused','wa',?2,?2)",(UID,T0))?;
        node.execute("INSERT INTO sessions(id_hash,user_id,csrf_hash,created_at,last_seen_at,expires_at,user_agent,ip) VALUES(?1,?2,?3,?4,?4,?5,'Synthetic','127.0.0.1')",(util::digest("synthetic-session"),UID,util::digest("synthetic-csrf"),T0,T0+60_000))?;
        node.execute(
            "INSERT INTO settings(key,value) VALUES('feature:nativeClientAuth','true')",
            [],
        )?;
        let writer = Arc::new(Writer::open(dir.path(), RustAuth::owned())?);
        let controller = Arc::new(Controller::new(
            Arc::new(FeatureRuntime::new()?),
            [None; 23],
        ));
        let snapshot = writer.read(|r| {
            Ok(Snapshot {
                decision: source_fingerprint(r, ContextSource::Decision)?,
                machines: source_fingerprint(r, ContextSource::InferenceMachines)?,
                choice: None,
                system_one_unconfigured: true,
            })
        })?;
        let facts = BootFacts {
            env: [None; 23],
            restart: [false, false],
            browser: BrowserFact::Available,
            code: false,
            trust_proxy: true,
        };
        let boot = Uuid::new_v4();
        let session = Uuid::new_v4();
        let cancel = Cancellation::default();
        let control = Control {
            request: Uuid::new_v4(),
            deadline: Instant::now() + Duration::from_secs(30),
            cancelled: cancel.signal(),
        };
        assert!(matches!(
            controller.register(
                &writer,
                &control,
                Registration {
                    boot,
                    session,
                    facts: facts.clone(),
                    snapshot
                },
                |tx| feature_context::startup::initialize(tx, &facts, T0)
            )?,
            feature_context::Reply::Ready(_)
        ));
        let auth = Arc::new(Authenticator::new(AuthConfig {
            public_origin: ORIGIN.into(),
            additional_origins: vec![],
            legacy_token: String::new(),
            legacy_compat: false,
            trust_proxy: true,
            native_client_auth_env: None,
            origin_from_settings: false,
        }));
        let settings = Settings {
            public_origin: ORIGIN.into(),
            webauthn_rp_id: String::new(),
            trust_proxy: true,
            data_dir: dir.path().to_owned(),
            dav_available: false,
            dav: crate::DavEnv::default(),
        };
        let acct = Account::new(writer, auth, settings, RustAuth::owned(), T0)
            .with_feature_context(Arc::clone(&controller));
        let req = Request {
            method: "POST".into(),
            path: "/api/auth/device/code".into(),
            creds: Creds {
                method: "POST".into(),
                origin: Some(ORIGIN.into()),
                cookie: None,
                csrf: None,
                authorization: None,
            },
            user_agent: "Synthetic".into(),
            client_ip: "127.0.0.1".into(),
            content_type: "application/json".into(),
            body: Body {
                bytes: br#"{"client_name":"Synthetic"}"#.to_vec(),
                over: false,
            },
        };
        Ok(Fixture {
            _dir: dir,
            node,
            acct,
            req,
            controller,
            boot,
            session,
        })
    }
    fn count(f: &Fixture, table: &str) -> Result<i64, Error> {
        // All callers below use fixed test-owned table names.
        Ok(f.node
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))?)
    }
    fn status(result: Result<Outcome, Fault>) -> u16 {
        match result {
            Ok(Outcome::Reply(reply)) => reply.status,
            Err(Fault::Unavailable) => 503,
            Err(Fault::Status(n, _)) => n,
            _ => 500,
        }
    }
    fn pending(f: &Fixture, approved: bool) -> Result<(), Error> {
        f.node.execute("INSERT INTO device_authorizations(device_code_hash,user_code_hash,client_name,ip,user_agent,created_at,expires_at,interval_ms,status,user_id) VALUES(?1,?2,'Synthetic','127.0.0.1','Synthetic',?3,?4,5000,?5,?6)",(util::digest("synthetic-code"),util::digest("BCDFGHJK"),T0,T0+60_000,if approved {"approved"} else {"pending"},approved.then_some(UID)))?;
        Ok(())
    }
    fn refresh(f: &Fixture) -> Result<(), Error> {
        f.node.execute("INSERT INTO device_grants(id,user_id,client_name,created_at,last_used_at,expires_at,ip,user_agent) VALUES('synthetic-grant',?1,'Synthetic',?2,?2,?3,'127.0.0.1','Synthetic')",(UID,T0,T0+60_000))?;
        f.node.execute("INSERT INTO device_tokens(token_hash,grant_id,kind,created_at,expires_at) VALUES(?1,'synthetic-grant','refresh',?2,?3)",(util::digest("nvr_synthetic"),T0,T0+60_000))?;
        Ok(())
    }
    #[test]
    fn off_reenable_invalidates_earlier_work_and_cannot_restore_pending_codes() -> Result<(), Error>
    {
        let f = fixture()?;
        pending(&f, false)?;
        let fence =
            Issuance::capture(&f.acct, &f.req, &|| T0, None).map_err(|e| format!("{e:?}"))?;
        let earlier = fence.admission;
        f.controller.mutate(&f.acct.writer, earlier, true, |tx| {
            tx.set_feature_enabled(
                RustFeatureSettings::owned(),
                server_features::Feature::NativeClientAuth,
                false,
            )?;
            feature_context::startup::revoke_all(tx, Some(UID), T0)
        })?;
        let next = f.acct.writer.read(|r| f.controller.admission(r))?;
        f.controller.mutate(&f.acct.writer, next, false, |tx| {
            tx.set_feature_enabled(
                RustFeatureSettings::owned(),
                server_features::Feature::NativeClientAuth,
                true,
            )
        })?;
        assert_eq!(
            status(start(
                &f.acct,
                &f.req,
                &JValue::obj([("client_name", JValue::from("Synthetic"))]),
                T0,
                &fence
            )),
            503
        );
        assert_eq!(count(&f, "device_authorizations")?, 0);
        let fresh =
            Issuance::capture(&f.acct, &f.req, &|| T0, None).map_err(|e| format!("{e:?}"))?;
        assert_eq!(
            status(start(
                &f.acct,
                &f.req,
                &JValue::obj([("client_name", JValue::from("Synthetic"))]),
                T0,
                &fresh
            )),
            200
        );
        Ok(())
    }
    #[test]
    fn released_and_replaced_contexts_refuse_without_erasing_existing_credentials(
    ) -> Result<(), Error> {
        for replaced in [false, true] {
            let f = fixture()?;
            refresh(&f)?;
            let fence =
                Issuance::capture(&f.acct, &f.req, &|| T0, None).map_err(|e| format!("{e:?}"))?;
            if replaced {
                FeatureRuntime::new()?.initialize(&f.acct.writer)?;
            } else {
                let cancel = Cancellation::default();
                let control = Control {
                    request: Uuid::new_v4(),
                    deadline: Instant::now() + Duration::from_secs(30),
                    cancelled: cancel.signal(),
                };
                f.controller
                    .release(&f.acct.writer, &control, f.boot, f.session)?;
            }
            assert!(fence
                .write(None, |tx, now| tx
                    .execute("UPDATE device_grants SET last_used_at=?1", [now]))
                .is_err());
            assert_eq!(count(&f, "device_grants")?, 1);
            assert_eq!(count(&f, "device_tokens")?, 1);
        }
        Ok(())
    }
    #[test]
    fn code_and_refresh_recheck_authoritative_rows_after_read_before_write() -> Result<(), Error> {
        for code in [true, false] {
            let f = fixture()?;
            if code {
                pending(&f, true)?;
            } else {
                refresh(&f)?;
            }
            let admission = f.acct.writer.read(|r| f.controller.admission(r))?;
            let arrived = Barrier::new(2);
            let proceed = Barrier::new(2);
            std::thread::scope(|scope| -> Result<(), Error> {
                let acct = &f.acct;
                let req = &f.req;
                let arrived = &arrived;
                let proceed = &proceed;
                let worker = scope.spawn(move || {
                    let first = AtomicBool::new(true);
                    let probe = || {
                        if first.swap(false, Ordering::SeqCst) {
                            arrived.wait();
                            proceed.wait();
                        }
                    };
                    let fence = Issuance {
                        acct,
                        req,
                        admission,
                        clock: &|| T0,
                        browser: None,
                        before_write: Some(&probe),
                    };
                    if code {
                        status(exchange_device_code(
                            acct,
                            &util::digest("synthetic-code"),
                            T0,
                            &fence,
                        ))
                    } else {
                        status(exchange_refresh(acct, "nvr_synthetic", T0, false, &fence))
                    }
                });
                arrived.wait();
                let changed = if code {
                    f.node
                        .execute("UPDATE device_authorizations SET status='denied'", [])
                } else {
                    f.node.execute("UPDATE users SET disabled_at=?1", [T0])
                };
                proceed.wait();
                changed?;
                assert_eq!(worker.join().map_err(|_| "worker panic")?, 400);
                Ok(())
            })?;
            assert_eq!(count(&f, "device_tokens")?, if code { 0 } else { 1 });
            assert_eq!(count(&f, "device_grants")?, if code { 0 } else { 1 });
        }
        Ok(())
    }
    #[test]
    fn actual_writer_queue_and_post_sql_deadline_expiry_both_refuse() -> Result<(), Error> {
        let f = fixture()?;
        let admission = f.acct.writer.read(|r| f.controller.admission(r))?;
        let clock = AtomicI64::new(T0);
        let entered = Barrier::new(2);
        std::thread::scope(|scope| -> Result<(), Error> {
            let mut worker = None;
            f.acct.writer.write(|_|{
                let acct=&f.acct;let req=&f.req;let clock=&clock;let entered=&entered;
                worker=Some(scope.spawn(move||{
                    let clock=||clock.load(Ordering::SeqCst);
                    let fence=Issuance {acct,req,admission,clock:&clock,browser:None,before_write:None};
                    entered.wait();
                    fence.write(Some(T0+100),|tx,now|tx.execute("INSERT INTO device_authorizations(device_code_hash,user_code_hash,client_name,ip,user_agent,created_at,expires_at,interval_ms,status) VALUES('queued','queued','Synthetic','ip','ua',?1,?2,5000,'pending')",(now,now+1000))).is_err()
                }));
                entered.wait();clock.store(T0+101,Ordering::SeqCst);Ok(())
            })?;
            assert!(worker.ok_or("worker")?.join().map_err(|_| "worker panic")?);
            Ok(())
        })?;
        assert_eq!(count(&f, "device_authorizations")?, 0);
        let samples = AtomicUsize::new(0);
        let clock = || {
            if samples.fetch_add(1, Ordering::SeqCst) == 0 {
                T0
            } else {
                T0 + 101
            }
        };
        let fence =
            Issuance::capture(&f.acct, &f.req, &clock, None).map_err(|e| format!("{e:?}"))?;
        assert!(fence.write(Some(T0+100),|tx,_|tx.execute("INSERT INTO device_authorizations(device_code_hash,user_code_hash,client_name,ip,user_agent,created_at,expires_at,interval_ms,status) VALUES('post','post','Synthetic','ip','ua',1,1000,5000,'pending')",[])).is_err());
        assert_eq!(count(&f, "device_authorizations")?, 0);
        Ok(())
    }
    #[test]
    fn approval_rechecks_actual_browser_after_sql_and_rolls_everything_back() -> Result<(), Error> {
        let mut f = fixture()?;
        pending(&f, false)?;
        f.req.path = "/api/auth/device/approve".into();
        f.req.creds.cookie =
            Some("cowork_session=synthetic-session; cowork_csrf=synthetic-csrf".into());
        f.req.creds.csrf = Some("synthetic-csrf".into());
        f.req.body.bytes = br#"{"user_code":"BCDFGHJK","approve":true}"#.to_vec();
        let samples = AtomicUsize::new(0);
        let outcome = f.acct.handle_with_clock(&f.req, || {
            if samples.fetch_add(1, Ordering::SeqCst) >= 2 {
                T0 + 60_001
            } else {
                T0
            }
        });
        assert_eq!(status(Ok(outcome)), 503);
        assert_eq!(
            f.node
                .query_row("SELECT status FROM device_authorizations", [], |r| r
                    .get::<_, String>(0))?,
            "pending"
        );
        assert_eq!(count(&f, "sessions")?, 1);
        assert_eq!(count(&f, "audit_events")?, 0);
        Ok(())
    }
    #[test]
    fn absent_context_does_not_block_browser_signin_or_fabricate_device_off() -> Result<(), Error> {
        let mut f = fixture()?;
        refresh(&f)?;
        f.acct.feature_context = None;
        assert_eq!(status(Ok(f.acct.handle(&f.req, T0))), 503);
        f.req.path = "/api/setup/status".into();
        f.req.method = "GET".into();
        f.req.creds.method = "GET".into();
        assert_eq!(status(Ok(f.acct.handle(&f.req, T0))), 200);
        assert_eq!(count(&f, "device_grants")?, 1);
        assert_eq!(count(&f, "device_tokens")?, 1);
        Ok(())
    }
}
