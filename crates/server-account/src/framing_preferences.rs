//! Complete chat-framing preferences route. No inference or suggestion execution.
//! The legacy read is intentionally forgiving; only literal booleans enable a preference.
//! PUT holds the shared SQLite IMMEDIATE writer fence through final authentication and
//! the file read/merge/replace, so partial updates cannot overwrite each other.
use crate::{files, Account, Fault, Outcome, Reply, Request, BODY_LIMIT};
use js_json::JValue;
use serde::Deserialize;
use server_auth::Identity;
use server_store::{json, RustAuth};
use std::path::Path;

const FILE: &str = "chat-framing.json";
const KEYS: [&str; 2] = ["autoAccept", "keepReasoningTraces"];
const INVALID: &str = "autoAccept and keepReasoningTraces must be true or false";

fn json_space(text: &str) -> &str {
    text.trim_matches(|c| matches!(c, ' ' | '\t' | '\r' | '\n'))
}

/// Validate the complete JSON lexically, then project only the two ASCII keys.
/// IgnoredAny does not convert ignored numbers/strings, so valid Node JSON with
/// lone surrogates, private-use scalars, overflow numbers or deep ignored data
/// cannot reset an unrelated saved boolean. This is not a general JSON parser:
/// serde validates grammar; this scan only splits validated top-level members.
fn project(text: &str) -> Result<JValue, ()> {
    let mut decoder = serde_json::Deserializer::from_str(text);
    serde::de::IgnoredAny::deserialize(&mut decoder).map_err(|_| ())?;
    decoder.end().map_err(|_| ())?;
    let Some(inner) = json_space(text)
        .strip_prefix('{')
        .and_then(|s| s.strip_suffix('}'))
    else {
        return Ok(JValue::Null);
    };
    let mut fields: Vec<(String, JValue)> = Vec::new();
    let mut member = |entry: &str| {
        let Some((key, value)) = entry.split_once(':') else {
            return;
        };
        // A literal colon inside an unknown key may split early; such a key
        // cannot be either recognized ASCII key and is safely ignored.
        let Ok(JValue::Str(key)) = js_json::parse(json_space(key)) else {
            return;
        };
        if !KEYS.contains(&key.as_str()) {
            return;
        }
        let value = match json_space(value) {
            "true" => JValue::Bool(true),
            "false" => JValue::Bool(false),
            _ => JValue::Null,
        };
        if let Some((_, old)) = fields.iter_mut().find(|(name, _)| name == &key) {
            *old = value;
        } else {
            fields.push((key, value));
        }
    };
    let (mut depth, mut quoted, mut escaped, mut start) = (0usize, false, false, 0usize);
    for (offset, byte) in inner.bytes().enumerate() {
        if quoted {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                quoted = false;
            }
        } else {
            match byte {
                b'"' => quoted = true,
                b'{' | b'[' => depth += 1,
                b'}' | b']' => depth = depth.saturating_sub(1),
                b',' if depth == 0 => {
                    member(inner.get(start..offset).ok_or(())?);
                    start = offset + 1;
                }
                _ => {}
            }
        }
    }
    member(inner.get(start..).ok_or(())?);
    Ok(JValue::Obj(fields))
}

fn preference_body(req: &Request) -> Result<JValue, Fault> {
    if req.body.over || req.body.bytes.len() > BODY_LIMIT {
        return Err(Fault::Status(413, "Request exceeds size limit".into()));
    }
    let raw = String::from_utf8_lossy(&req.body.bytes);
    if raw.is_empty() {
        return Ok(JValue::Obj(Vec::new()));
    }
    project(&raw).map_err(|()| Fault::Status(400, "invalid JSON".into()))
}

fn read(dir: &Path) -> JValue {
    let data = std::fs::read(dir.join(FILE))
        .ok()
        .and_then(|bytes| project(&String::from_utf8_lossy(&bytes)).ok())
        .unwrap_or(JValue::Null);
    JValue::obj(KEYS.map(|key| (key, JValue::Bool(data.get(key) == &JValue::Bool(true)))))
}

fn update(dir: &Path, body: &JValue, switch: RustAuth) -> Result<JValue, Fault> {
    let JValue::Obj(fields) = body else {
        return Err(Fault::Status(400, INVALID.into()));
    };
    let given: Vec<_> = KEYS
        .iter()
        .copied()
        .filter(|key| fields.iter().any(|(name, _)| name == key))
        .collect();
    if given.is_empty()
        || given
            .iter()
            .any(|key| !matches!(body.get(key), JValue::Bool(_)))
    {
        return Err(Fault::Status(400, INVALID.into()));
    }
    let previous = read(dir);
    let record = JValue::obj(KEYS.map(|key| {
        (
            key,
            if given.contains(&key) {
                body.get(key).clone()
            } else {
                previous.get(key).clone()
            },
        )
    }));
    let text = js_json::stringify(&record).ok_or(Fault::Internal)?;
    json::write_owned_text(dir, FILE, &text, switch)
        .map_err(|_| Fault::Status(400, "Invalid framing preferences".into()))?;
    Ok(record)
}

fn refusal(why: server_auth::identity::Refusal) -> Outcome {
    use server_auth::identity::Refusal;
    match why {
        Refusal::Unauthorized => {
            let mut reply = Reply::error(401, "unauthorized");
            reply
                .headers
                .push(("WWW-Authenticate".into(), "Bearer realm=\"cowork\"".into()));
            reply.into()
        }
        Refusal::Csrf => Reply::error(403, "invalid CSRF token").into(),
        Refusal::BrowserOnly => Reply::json(
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
    }
}

#[cfg(target_os = "linux")]
fn ensure_workspace(acct: &Account, current: &Identity, dir: &Path) -> Result<(), Fault> {
    use server_projects::store::{ensure_user_dir, Ensured};
    if matches!(
        ensure_user_dir(&acct.settings.data_dir, &current.user.id)
            .map_err(|_| Fault::Unavailable)?,
        Ensured::LegacyPending
    ) {
        if current.account_role == server_auth::Role::Admin {
            server_projects::legacy_claim::claim_root(&acct.settings.data_dir, &current.user.id)
                .map_err(|_| Fault::Unavailable)?;
            if matches!(
                ensure_user_dir(&acct.settings.data_dir, &current.user.id)
                    .map_err(|_| Fault::Unavailable)?,
                Ensured::LegacyPending
            ) {
                return Err(Fault::Unavailable);
            }
        } else {
            // Node's workspace.get(claim:false) creates a member's own directory
            // without claiming an administrator's legacy root.
            let mut make = std::fs::DirBuilder::new();
            make.recursive(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                make.mode(0o700);
            }
            make.create(dir).map_err(|_| Fault::Unavailable)?;
        }
    }
    Ok(())
}

// The production server/legacy claim uses Linux descriptor APIs. Portable
// Account tests may exercise an already-initialized synthetic workspace only;
// no non-Linux substitute claims or creates a legacy workspace.
#[cfg(not(target_os = "linux"))]
fn ensure_workspace(_acct: &Account, _current: &Identity, dir: &Path) -> Result<(), Fault> {
    if dir.is_dir() {
        Ok(())
    } else {
        Err(Fault::Unavailable)
    }
}

pub(crate) fn handle(
    acct: &Account,
    req: &Request,
    authn: &Identity,
    now: i64,
) -> Result<Option<Outcome>, Fault> {
    if req.path != "/api/chat-framing/preferences" {
        return Ok(None);
    }
    if !matches!(req.method.as_str(), "GET" | "PUT") {
        return Ok(Some(Reply::error(405, "method not allowed").into()));
    }
    let dir = files::user_dir(acct, &authn.user.id)?;
    let body = if req.method == "PUT" {
        Some(preference_body(req)?)
    } else {
        None
    };
    let waiting = std::time::Instant::now();
    let current_time =
        || now.saturating_add(i64::try_from(waiting.elapsed().as_millis()).unwrap_or(i64::MAX));
    // Same IMMEDIATE writer fence as credential/user changes, including Node
    // writers. No nested Writer::read, and no DB+file crash-atomicity claim.
    let result = acct.writer.write(|tx| {
        let reader = tx.reader();
        let current = match acct
            .auth
            .gate_in(&reader, &req.creds, &req.path, current_time())?
        {
            Ok(current) if current.user.id == authn.user.id => current,
            Ok(_) => return Ok(Ok(refusal(server_auth::identity::Refusal::Unauthorized))),
            Err(why) => return Ok(Ok(refusal(why))),
        };
        if let Err(error) = ensure_workspace(acct, &current, &dir) {
            return Ok(Err(error));
        }
        // Legacy initialization is bounded but can consume time. Expiry must be
        // checked again immediately before the preference operation.
        if let Err(why) = acct
            .auth
            .gate_in(&reader, &req.creds, &req.path, current_time())?
        {
            return Ok(Ok(refusal(why)));
        }
        Ok(match body.as_ref() {
            Some(body) => {
                update(&dir, body, acct.switch).map(|record| Reply::json(200, &record).into())
            }
            None => Ok(Reply::json(200, &read(&dir)).into()),
        })
    })?;
    result.map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Body, Settings};
    use server_auth::{AuthConfig, Authenticator, Creds};
    use server_store::{rusqlite::Connection, Writer};
    use std::sync::{Arc, Barrier};
    type Error = Box<dyn std::error::Error>;
    const T0: i64 = 1_800_000_000_000;
    const UID: &str = "00000000-0000-4000-8000-000000000001";
    const ORIGIN: &str = "https://noevia.example.test";

    fn bytes(value: &serde_json::Value) -> Result<Option<Vec<u8>>, Error> {
        if value.is_null() {
            return Ok(None);
        }
        let hex = value.as_str().ok_or("not hexadecimal text")?;
        if hex.len() % 2 != 0 {
            return Err("odd hexadecimal length".into());
        }
        hex.as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|part| Ok(u8::from_str_radix(std::str::from_utf8(part)?, 16)?))
            .collect::<Result<Vec<_>, Error>>()
            .map(Some)
    }

    #[test]
    fn frozen_node_differentials_preserve_values_bytes_and_errors() -> Result<(), Error> {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/framing-preferences.json"))?;
        for case in fixture
            .get("cases")
            .and_then(serde_json::Value::as_array)
            .ok_or("missing cases")?
        {
            let dir = tempfile::tempdir()?;
            let file = dir.path().join(FILE);
            if let Some(stored) = bytes(case.get("storedBytes").ok_or("storedBytes")?)? {
                std::fs::write(&file, stored)?;
            }
            let result = if let Some(body) = case.get("body").and_then(serde_json::Value::as_str) {
                let req = Request {
                    method: "PUT".into(),
                    path: "/api/chat-framing/preferences".into(),
                    creds: Creds::default(),
                    user_agent: String::new(),
                    content_type: String::new(),
                    client_ip: String::new(),
                    body: Body {
                        bytes: body.as_bytes().to_vec(),
                        over: false,
                    },
                };
                preference_body(&req).and_then(|v| update(dir.path(), &v, RustAuth::owned()))
            } else {
                Ok(read(dir.path()))
            };
            let (status, value) = match result {
                Ok(v) => (
                    200,
                    serde_json::from_str(&js_json::stringify(&v).ok_or("JSON")?)?,
                ),
                Err(Fault::Status(status, message)) => {
                    (status, serde_json::json!({"error":message}))
                }
                Err(other) => return Err(format!("unexpected {other:?}").into()),
            };
            assert_eq!(
                Some(u64::from(status)),
                case.get("status").and_then(serde_json::Value::as_u64)
            );
            assert_eq!(Some(&value), case.get("data"), "case {case}");
            assert_eq!(
                std::fs::read(&file).ok(),
                bytes(case.get("after").ok_or("after")?)?
            );
        }
        Ok(())
    }

    struct Fixture {
        dir: tempfile::TempDir,
        node: Connection,
        acct: Account,
        req: Request,
        initial: Identity,
    }
    fn fixture() -> Result<Fixture, Error> {
        let dir = tempfile::tempdir()?;
        let node = Connection::open(server_store::db_path(dir.path()))?;
        node.execute_batch(include_str!(
            "../../server-store/tests/fixtures/node-schema.sql"
        ))?;
        node.execute("INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,created_at,updated_at) VALUES(?1,'Synthetic','synthetic','Synthetic','admin','unused','synthetic-wa',?2,?2)", (UID,T0))?;
        node.execute("INSERT INTO sessions(id_hash,user_id,csrf_hash,created_at,last_seen_at,expires_at,user_agent,ip) VALUES(?1,?2,?3,?4,?4,?5,'Synthetic','127.0.0.1')",
            (server_auth::js::digest("synthetic-session"),UID,server_auth::js::digest("synthetic-csrf"),T0,T0+86_400_000))?;
        let switch = RustAuth::owned();
        let writer = Arc::new(Writer::open(dir.path(), switch)?);
        let auth = Arc::new(Authenticator::new(AuthConfig {
            public_origin: ORIGIN.into(),
            additional_origins: vec![],
            legacy_token: String::new(),
            legacy_compat: false,
            trust_proxy: false,
            native_client_auth_env: Some(false),
            origin_from_settings: false,
        }));
        let settings = Settings {
            public_origin: ORIGIN.into(),
            webauthn_rp_id: String::new(),
            trust_proxy: false,
            data_dir: dir.path().to_path_buf(),
            dav_available: false,
            dav: crate::DavEnv::default(),
        };
        let acct = Account::new(writer, auth, settings, switch, T0);
        let req = Request {
            method: "PUT".into(),
            path: "/api/chat-framing/preferences".into(),
            creds: Creds {
                cookie: Some("cowork_session=synthetic-session; cowork_csrf=synthetic-csrf".into()),
                csrf: Some("synthetic-csrf".into()),
                origin: Some(ORIGIN.into()),
                method: "PUT".into(),
                authorization: None,
            },
            user_agent: "Synthetic".into(),
            content_type: "application/json".into(),
            client_ip: "127.0.0.1".into(),
            body: Body {
                bytes: br#"{"autoAccept":true}"#.to_vec(),
                over: false,
            },
        };
        std::fs::create_dir_all(dir.path().join("users").join(UID))?;
        let initial = acct
            .writer
            .read(|r| acct.auth.authenticate_in(r, &req.creds, T0))?
            .ok_or("no synthetic identity")?;
        Ok(Fixture {
            dir,
            node,
            acct,
            req,
            initial,
        })
    }
    fn reply(outcome: Outcome) -> Result<Reply, Error> {
        match outcome {
            Outcome::Reply(r) => Ok(r),
            Outcome::Pass => Err("unexpected Node fallback".into()),
        }
    }

    #[test]
    fn actual_account_auth_csrf_method_order_and_read_after_write() -> Result<(), Error> {
        let f = fixture()?;
        for (method, authorized, csrf, expected) in [
            ("DELETE", false, false, 401),
            ("DELETE", true, false, 403),
            ("DELETE", true, true, 405),
            ("HEAD", true, false, 405),
            ("OPTIONS", true, false, 405),
            ("GET", true, false, 200),
        ] {
            let mut req = f.req.clone();
            req.method = method.into();
            req.creds.method = method.into();
            if !authorized {
                req.creds.cookie = None;
            }
            if !csrf {
                req.creds.csrf = None;
            }
            assert_eq!(reply(f.acct.handle(&req, T0))?.status, expected);
        }
        assert_eq!(
            reply(f.acct.handle(&f.req, T0))?.body,
            r#"{"autoAccept":true,"keepReasoningTraces":false}"#
        );
        let mut req = f.req.clone();
        req.method = "GET".into();
        req.creds.method = "GET".into();
        assert_eq!(
            reply(f.acct.handle(&req, T0))?.body,
            r#"{"autoAccept":true,"keepReasoningTraces":false}"#
        );
        req.method = "PUT".into();
        req.creds.method = "PUT".into();
        req.body.bytes = b"{bad".to_vec();
        assert_eq!(
            reply(f.acct.handle(&req, T0))?.body,
            r#"{"error":"invalid JSON"}"#
        );
        req.body.over = true;
        assert_eq!(reply(f.acct.handle(&req, T0))?.status, 413);
        Ok(())
    }

    #[test]
    fn concurrent_partial_updates_keep_both_values() -> Result<(), Error> {
        let f = fixture()?;
        let mut second = f.req.clone();
        second.body.bytes = br#"{"keepReasoningTraces":true}"#.to_vec();
        let barrier = Barrier::new(3);
        std::thread::scope(|scope| -> Result<(), Error> {
            let a = &f.acct;
            let first = &f.req;
            let b = &barrier;
            let x = scope.spawn(move || {
                b.wait();
                a.handle(first, T0)
            });
            let y = scope.spawn(move || {
                b.wait();
                a.handle(&second, T0)
            });
            barrier.wait();
            assert_eq!(
                reply(x.join().map_err(|_| "first thread panicked")?)?.status,
                200
            );
            assert_eq!(
                reply(y.join().map_err(|_| "second thread panicked")?)?.status,
                200
            );
            Ok(())
        })?;
        assert_eq!(
            js_json::stringify(&read(&f.dir.path().join("users").join(UID))).ok_or("JSON")?,
            r#"{"autoAccept":true,"keepReasoningTraces":true}"#
        );
        Ok(())
    }

    #[test]
    fn final_gate_rejects_revoked_disabled_removed_and_expired_initial_identity(
    ) -> Result<(), Error> {
        for sql in [
            "DELETE FROM sessions",
            "UPDATE users SET disabled_at=1800000000001",
            "DELETE FROM users",
        ] {
            let f = fixture()?;
            // A real valid initial authentication is captured before the external
            // lifecycle writer acquires IMMEDIATE. The route must recheck after it.
            if sql == "DELETE FROM users" {
                std::fs::remove_dir(f.dir.path().join("users").join(UID))?;
            }
            f.node.execute_batch("BEGIN IMMEDIATE")?;
            f.node.execute_batch(sql)?;
            let barrier = Barrier::new(2);
            std::thread::scope(|scope| -> Result<(), Error> {
                let a = &f.acct;
                let req = &f.req;
                let initial = &f.initial;
                let b = &barrier;
                let worker = scope.spawn(move || {
                    b.wait();
                    handle(a, req, initial, T0)
                });
                barrier.wait();
                f.node.execute_batch("COMMIT")?;
                let out = worker
                    .join()
                    .map_err(|_| "gate thread panicked")?
                    .map_err(|e| format!("{e:?}"))?
                    .ok_or("no route")?;
                assert_eq!(reply(out)?.status, 401);
                Ok(())
            })?;
            let own = f.dir.path().join("users").join(UID);
            assert!(!own.join(FILE).exists());
            assert_eq!(own.exists(), sql != "DELETE FROM users");
        }
        let f = fixture()?;
        let out = handle(&f.acct, &f.req, &f.initial, T0 + 86_400_001)
            .map_err(|e| format!("{e:?}"))?
            .ok_or("no route")?;
        assert_eq!(reply(out)?.status, 401);
        Ok(())
    }

    #[test]
    fn failures_never_report_success_or_change_other_tenants() -> Result<(), Error> {
        let f = fixture()?;
        let own = f.dir.path().join("users").join(UID);
        let other = f
            .dir
            .path()
            .join("users/00000000-0000-4000-8000-000000000002");
        std::fs::create_dir_all(&other)?;
        std::fs::write(other.join(FILE), b"synthetic-other")?;
        let mut req = f.req.clone();
        req.body.bytes=br#"{"autoAccept":true,"userId":"00000000-0000-4000-8000-000000000002","path":"../other"}"#.to_vec();
        assert_eq!(reply(f.acct.handle(&req, T0))?.status, 200);
        assert_eq!(std::fs::read(other.join(FILE))?, b"synthetic-other");
        let original = std::fs::read(own.join(FILE))?;
        let tmp = own.join(format!("{FILE}.{}.tmp", std::process::id()));
        std::fs::create_dir(&tmp)?;
        let failed = reply(f.acct.handle(&f.req, T0))?;
        assert_eq!(failed.status, 400);
        assert_eq!(failed.body, r#"{"error":"Invalid framing preferences"}"#);
        assert_eq!(std::fs::read(own.join(FILE))?, original);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(own.join(FILE))?.permissions().mode() & 0o777,
                0o600
            );
        }
        Ok(())
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn linux_first_read_claims_legacy_before_creating_the_user_directory() -> Result<(), Error> {
        let f = fixture()?;
        let own = f.dir.path().join("users").join(UID);
        std::fs::remove_dir(&own)?;
        std::fs::write(f.dir.path().join("projects.json"), b"[]")?;
        // Node's legacy claim excludes this root-level preference file too.
        std::fs::write(f.dir.path().join(FILE), br#"{"autoAccept":true}"#)?;
        let mut req = f.req.clone();
        req.method = "GET".into();
        req.creds.method = "GET".into();
        let response = reply(f.acct.handle(&req, T0))?;
        assert_eq!(response.status, 200);
        assert_eq!(
            response.body,
            r#"{"autoAccept":false,"keepReasoningTraces":false}"#
        );
        assert_eq!(std::fs::read(own.join("projects.json"))?, b"[]");
        assert!(f.dir.path().join(FILE).is_file());
        Ok(())
    }
}
