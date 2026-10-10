//! auth.cjs's passkey ceremonies: `registrationOptions`, `registrationVerify`,
//! `authenticationOptions`, `authenticationVerify`, with @simplewebauthn v14's options shapes and
//! the passkey crate's verification. A passkey keeps the RP ID it was made under
//! (`passkeys.rp_id`; `NULL` is a legacy row and means the current one, auth.cjs `keyRp`).

use crate::users::{self, audit};
use crate::util;
use crate::{Account, Fault, Handled, Outcome, Reply, Request};
use js_json::JValue;
use server_store::rusqlite::types::Value;
use server_store::Reader;

/// auth.cjs PASSKEY_LIST_SIZE.
const LIST_SIZE: usize = 5;
/// auth.cjs PASSKEY_OPTIONS_PER_ADDRESS (per challenge lifetime, TRUST_PROXY only).
const OPTIONS_PER_ADDRESS: u64 = 30;

/// The current RP ID and every origin a ceremony may come from (`relyingPartyId`,
/// `passkeyOrigins()`).
struct Rp {
    id: String,
    origins: Vec<String>,
}

fn rp(acct: &Account, r: &Reader<'_>) -> Result<Rp, Fault> {
    let origin = acct.origin(r)?;
    let id = util::rp_for(&origin, &acct.settings.webauthn_rp_id)?;
    let setup = r
        .row(
            "SELECT value FROM settings WHERE key='public_origin'",
            [],
            |row| row.get::<_, Value>(0),
        )?
        .and_then(|v| match v {
            Value::Text(t) => Some(t),
            _ => None,
        })
        .unwrap_or_default();
    let current = if origin.is_empty() { setup } else { origin };
    let previous: Vec<String> = r
        .row(
            "SELECT value FROM settings WHERE key='previous_origins'",
            [],
            |row| row.get::<_, Value>(0),
        )?
        .and_then(|v| match v {
            Value::Text(t) if !t.is_empty() => js_json::parse(&t).ok(),
            _ => None,
        })
        .map(|v| match v {
            JValue::Arr(items) => items
                .into_iter()
                .filter_map(|i| match i {
                    JValue::Str(s) => Some(s),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        })
        .unwrap_or_default();
    let mut origins: Vec<String> = Vec::new();
    for o in std::iter::once(current).chain(previous) {
        if !o.is_empty() && !origins.contains(&o) {
            origins.push(o);
        }
    }
    Ok(Rp { id, origins })
}

struct KeyRow {
    id: String,
    user_id: String,
    public_key: Vec<u8>,
    counter: f64,
    transports: String,
    rp_id: Option<String>,
}

fn key_rows(r: &Reader<'_>, sql: &str, param: &str) -> Result<Vec<KeyRow>, Fault> {
    Ok(r.rows(sql, [param], |row| {
        Ok(KeyRow {
            id: row.get(0)?,
            user_id: row.get(1)?,
            public_key: row.get::<_, Vec<u8>>(2)?,
            counter: match row.get::<_, Value>(3)? {
                Value::Integer(i) => i as f64,
                Value::Real(f) => f,
                _ => f64::NAN,
            },
            transports: row.get(4)?,
            rp_id: row.get(5)?,
        })
    })?)
}

const KEY_COLS: &str = "id, user_id, public_key, counter, transports, rp_id";

/// `key.rp_id || relyingPartyId`.
fn key_rp<'a>(k: &'a KeyRow, current: &'a str) -> &'a str {
    k.rp_id
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or(current)
}

/// `{ id, transports: JSON.parse(k.transports) }` as options list it (`{ ...cred, id, type }`).
fn descriptor(id: &str, transports: JValue) -> Result<JValue, Fault> {
    if !passkey::b64::is_base64url(id) {
        return Err(Fault::Internal);
    }
    Ok(JValue::obj([
        ("id", JValue::from(id.replace('=', ""))),
        ("transports", transports),
        ("type", JValue::from("public-key")),
    ]))
}

/// auth.cjs `registrationOptions(userId)` (POST /api/auth/passkeys/register/options).
pub fn registration_options(acct: &Account, user_id: &str, now: i64) -> Handled {
    let (rp, user, keys) = acct.writer.read(|r| {
        let rp = rp(acct, r);
        let user = r.row(
            "SELECT username, display_name, webauthn_user_id FROM users WHERE id=?1",
            [user_id],
            |row| {
                Ok((
                    row.get::<_, Value>(0)?,
                    row.get::<_, Value>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )?;
        let keys = key_rows(
            r,
            &format!("SELECT {KEY_COLS} FROM passkeys WHERE user_id=?1"),
            user_id,
        );
        Ok((rp, user, keys))
    })?;
    let (rp, keys) = (rp?, keys?);
    let (username, display, webauthn_id) = user.ok_or(Fault::Internal)?;
    let mut exclude = Vec::new();
    for k in keys.iter().filter(|k| key_rp(k, &rp.id) == rp.id) {
        let t = js_json::parse(&k.transports).map_err(|_| Fault::Internal)?;
        exclude.push(descriptor(&k.id, t)?);
    }
    let challenge = util::random_token(32);
    let user_handle = passkey::b64::from_buffer(&util::node_base64url_decode(&webauthn_id));
    let alg = |a: f64| {
        JValue::obj([
            ("alg", JValue::Num(a)),
            ("type", JValue::from("public-key")),
        ])
    };
    let options = JValue::obj([
        ("challenge", JValue::from(challenge.as_str())),
        (
            "rp",
            JValue::obj([
                ("name", JValue::from("noevia")),
                ("id", JValue::from(rp.id.as_str())),
            ]),
        ),
        (
            "user",
            JValue::obj([
                ("id", JValue::from(user_handle)),
                ("name", users::sql_js(&username)),
                ("displayName", users::sql_js(&display)),
            ]),
        ),
        (
            "pubKeyCredParams",
            JValue::Arr(passkey::SUPPORTED_ALGS.iter().map(|a| alg(*a)).collect()),
        ),
        ("timeout", JValue::Num(60000.0)),
        ("attestation", JValue::from("none")),
        ("excludeCredentials", JValue::Arr(exclude)),
        (
            "authenticatorSelection",
            JValue::obj([
                ("residentKey", JValue::from("preferred")),
                ("userVerification", JValue::from("required")),
                ("requireResidentKey", JValue::from(false)),
            ]),
        ),
        (
            "extensions",
            JValue::obj([("credProps", JValue::from(true))]),
        ),
        ("hints", JValue::Arr(Vec::new())),
    ]);
    match users::save_challenge(acct, Some(user_id), "register", &challenge, now)? {
        Ok(token) => Ok(Reply::json(
            200,
            &JValue::obj([
                ("options", options),
                ("challengeToken", JValue::from(token)),
            ]),
        )
        .into()),
        Err(()) => Ok(Reply::error(503, "passkey setup is temporarily busy").into()),
    }
}

/// auth.cjs `registrationVerify(userId, body)` (POST /api/auth/passkeys/register/verify): every
/// thrown message is the route's 400.
pub fn registration_verify(acct: &Account, user_id: &str, body: &JValue, now: i64) -> Handled {
    let fail = |m: &str| -> Handled { Ok(Reply::error(400, m).into()) };
    let epoch = acct.writer.read(|r| users::credential_epoch(r, user_id))?;
    let token = util::js_string_or(body.get("challengeToken"), "")?;
    let challenge = users::take_challenge(acct, &token, "register", now)?;
    let Some(challenge) = challenge.filter(|c| c.user_id.as_deref() == Some(user_id)) else {
        return fail("registration challenge expired");
    };
    let rp = acct.writer.read(|r| Ok(rp(acct, r)))??;
    let cred = match passkey::verify_registration(
        body.get("response"),
        passkey::Expected {
            challenge: &challenge.challenge,
            origins: &rp.origins,
            rp_id: &rp.id,
        },
    ) {
        Ok(c) => c,
        Err(e) => return fail(&e.message),
    };
    let name = util::slice16(&util::js_string_or(body.get("name"), "Passkey")?, 80);
    let transports = if cred.transports.truthy() {
        js_json::stringify(&cred.transports).unwrap_or_else(|| "[]".into())
    } else {
        "[]".into()
    };
    enum Saved {
        Ok,
        Expired,
        Failed(String),
    }
    let saved = acct.writer.write(|tx| {
        let r = tx.reader();
        let current = users::credential_epoch(&r, user_id)?;
        if epoch.is_none() || current != epoch {
            return Ok(Saved::Expired);
        }
        let handle = r.row(
            "SELECT webauthn_user_id FROM users WHERE id=?1",
            [user_id],
            |row| row.get::<_, Value>(0),
        )?;
        let inserted = tx.execute(
            "INSERT INTO passkeys(id,user_id,name,public_key,webauthn_user_id,counter,device_type,backed_up,transports,created_at,rp_id) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
            (
                &cred.id,
                user_id,
                &name,
                &cred.public_key,
                handle,
                i64::from(cred.counter),
                cred.device_type,
                i64::from(cred.backed_up),
                &transports,
                now,
                &rp.id,
            ),
        );
        match inserted {
            Ok(_) => Ok(Saved::Ok),
            // A thrown SQLite error inside the transaction: its message is the 400.
            Err(server_store::StoreError::Sqlite(m)) => Ok(Saved::Failed(m)),
            Err(e) => Err(e),
        }
    })?;
    match saved {
        Saved::Ok => {}
        Saved::Expired => return fail("registration challenge expired"),
        Saved::Failed(m) => return fail(&m),
    }
    acct.writer.write(|tx| {
        audit(
            tx,
            "passkey.add",
            Some(user_id),
            Some(user_id),
            &JValue::obj([("credentialId", JValue::from(cred.id.as_str()))]),
            now,
        )
    })?;
    Ok(Reply::json(200, &JValue::obj([("verified", JValue::from(true))])).into())
}

/// The passkey decoy key (`settings.passkey_decoy_key`), made on first use like auth.cjs.
fn decoy_key(acct: &Account) -> Result<Vec<u8>, Fault> {
    let mut slot = acct.decoy_key.lock().map_err(|_| Fault::Internal)?;
    if let Some(k) = slot.as_ref() {
        return Ok(k.clone());
    }
    let value = acct.writer.write(|tx| {
        let read = |tx: &server_store::WriteTx<'_>| {
            tx.reader().row(
                "SELECT value FROM settings WHERE key='passkey_decoy_key'",
                [],
                |row| row.get::<_, Value>(0),
            )
        };
        let mut v = read(tx)?;
        if !matches!(&v, Some(Value::Text(t)) if !t.is_empty()) {
            tx.insert_setting_if_absent("passkey_decoy_key", &util::random_hex(32))?;
            v = read(tx)?;
        }
        Ok(v)
    })?;
    let Some(Value::Text(hex)) = value else {
        return Err(Fault::Internal);
    };
    let key = util::node_hex_decode(&hex);
    *slot = Some(key.clone());
    Ok(key)
}

fn hmac_id(key: &[u8], msg: &str) -> Result<String, Fault> {
    use hmac::{Hmac, Mac};
    let mut m = Hmac::<sha2::Sha256>::new_from_slice(key).map_err(|_| Fault::Internal)?;
    m.update(msg.as_bytes());
    Ok(passkey::b64::from_buffer(&m.finalize().into_bytes()))
}

/// auth.cjs `authenticationOptions(username, req)` (POST /api/auth/login/passkey/options).
pub fn authentication_options(
    acct: &Account,
    req: &Request,
    username: &JValue,
    now: i64,
) -> Handled {
    if acct.settings.trust_proxy {
        let limited = acct.rate.lock().map_err(|_| Fault::Internal)?.limited(
            &format!("passkey-opt:{}", req.client_ip),
            OPTIONS_PER_ADDRESS,
            users::CHALLENGE_MS,
            now,
        );
        if limited {
            return Ok(Reply::error(429, "too many sign-in attempts; try again later").into());
        }
    }
    let norm = util::js_string_or(username, "")?.to_lowercase();
    let (rp, user, keys) = acct.writer.read(|r| {
        let rp = rp(acct, r);
        let user = r.row(
            "SELECT id FROM users WHERE username_norm=?1 AND disabled_at IS NULL",
            [&norm],
            |row| row.get::<_, String>(0),
        )?;
        let keys = match &user {
            Some(id) => key_rows(
                r,
                &format!("SELECT {KEY_COLS} FROM passkeys WHERE user_id=?1"),
                id,
            ),
            None => Ok(Vec::new()),
        };
        Ok((rp, user, keys))
    })?;
    let (rp, keys) = (rp?, keys?);
    // One RP ID per ceremony: the current one if the account has a passkey for it (or none at
    // all), else the one its first passkey was made under.
    let sign_in_rp = if keys.is_empty() || keys.iter().any(|k| key_rp(k, &rp.id) == rp.id) {
        rp.id.clone()
    } else {
        keys.first()
            .map(|k| key_rp(k, &rp.id).to_string())
            .unwrap_or_default()
    };
    let mut list: Vec<(String, JValue)> = Vec::new();
    for k in keys.iter().filter(|k| key_rp(k, &rp.id) == sign_in_rp) {
        let t = js_json::parse(&k.transports).map_err(|_| Fault::Internal)?;
        list.push((k.id.clone(), t));
    }
    if list.len() < LIST_SIZE {
        let key = decoy_key(acct)?;
        let mut i = 0;
        while list.len() < LIST_SIZE {
            list.push((
                hmac_id(&key, &format!("{norm}:{i}"))?,
                JValue::Arr(vec![JValue::from("internal"), JValue::from("hybrid")]),
            ));
            i += 1;
        }
    }
    // `(a, b) => (a.id < b.id ? -1 : 1)`: UTF-16 order.
    list.sort_by(|a, b| a.0.encode_utf16().cmp(b.0.encode_utf16()));
    let mut allow = Vec::new();
    for (id, t) in list {
        allow.push(descriptor(&id, t)?);
    }
    let challenge = util::random_token(32);
    let options = JValue::obj([
        ("rpId", JValue::from(sign_in_rp)),
        ("challenge", JValue::from(challenge.as_str())),
        ("allowCredentials", JValue::Arr(allow)),
        ("timeout", JValue::Num(60000.0)),
        ("userVerification", JValue::from("required")),
    ]);
    match users::save_challenge(acct, user.as_deref(), "authenticate", &challenge, now)? {
        Ok(token) => Ok(Reply::json(
            200,
            &JValue::obj([
                ("options", options),
                ("challengeToken", JValue::from(token)),
            ]),
        )
        .into()),
        Err(()) => Ok(Reply::error(503, "passkey sign-in is temporarily busy").into()),
    }
}

/// auth.cjs `authenticationVerify(req, res, body)`: `Ok(None)` (and any error) is the route's 401.
pub fn authentication_verify(
    acct: &Account,
    req: &Request,
    body: &JValue,
    now: i64,
) -> Result<Option<Outcome>, Fault> {
    let token = util::js_string_or(body.get("challengeToken"), "")?;
    let challenge = users::take_challenge(acct, &token, "authenticate", now)?;
    let id = body.get("response").get("id");
    let key = match id.as_str() {
        Some(id) if !id.is_empty() => acct
            .writer
            .read(|r| {
                Ok(key_rows(
                    r,
                    &format!("SELECT {KEY_COLS} FROM passkeys WHERE id=?1"),
                    id,
                ))
            })??
            .into_iter()
            .next(),
        _ => None,
    };
    let (Some(challenge), Some(key)) = (challenge, key) else {
        return Ok(None);
    };
    if challenge.user_id.as_deref() != Some(key.user_id.as_str()) {
        return Ok(None);
    }
    let (epoch, rp) = acct
        .writer
        .read(|r| Ok((users::credential_epoch(r, &key.user_id)?, rp(acct, r))))?;
    let rp = rp?;
    if js_json::parse(&key.transports).is_err() {
        return Ok(None);
    }
    let Ok(new_counter) = passkey::verify_authentication(
        body.get("response"),
        passkey::Expected {
            challenge: &challenge.challenge,
            origins: &rp.origins,
            rp_id: key_rp(&key, &rp.id),
        },
        passkey::Stored {
            public_key: &key.public_key,
            counter: key.counter,
        },
    ) else {
        return Ok(None);
    };
    let epoch = epoch.unwrap_or(Value::Null);
    // The passkey must still exist (a recovery deletes it) and the account be unchanged since the
    // key was read; the counter moves either way once the signature checked out.
    let signed_in = acct.writer.write(|tx| {
        let moved = tx.execute(
            "UPDATE passkeys SET counter=?1,last_used_at=?2 WHERE id=?3 AND user_id=?4",
            (i64::from(new_counter), now, &key.id, &key.user_id),
        )?;
        if moved != 1 {
            return Ok(None);
        }
        users::issue_session_if_current(acct, tx, req, &key.user_id, &epoch, now)
            .map_err(|_| server_store::StoreError::Sqlite("session".into()))
    })?;
    let Some((user, issued)) = signed_in else {
        return Ok(None);
    };
    acct.writer.write(|tx| {
        audit(
            tx,
            "auth.passkey",
            Some(&key.user_id),
            Some(&key.user_id),
            &JValue::obj([("credentialId", JValue::from(key.id.as_str()))]),
            now,
        )
    })?;
    Ok(Some(
        Reply::json(
            200,
            &JValue::obj([("user", user), ("csrfToken", JValue::from(issued.csrf))]),
        )
        .with_cookies(issued.cookies)
        .into(),
    ))
}
