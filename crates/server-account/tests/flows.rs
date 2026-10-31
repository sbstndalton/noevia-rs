//! End-to-end flows through [`server_account::Account::handle`] on a Node-shaped cowork.db
//! (synthetic accounts only): first-run setup, the session it opens, password sign-in and its
//! limits, invitations, disabling, recovery revoking every credential, app passwords, a passkey
//! registered and used, device sign-in with refresh rotation and reuse detection, and the
//! account records. Node's behaviour is checked by the corpus replay (CI); these pin the Rust
//! side's own invariants.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use server_account::{Account, Body, Outcome, Reply, Request, Settings};
use server_auth::{AuthConfig, Authenticator, Creds};
use server_store::rusqlite::Connection;
use server_store::{RustAuth, Writer};
use std::sync::Arc;

const SCHEMA: &str = include_str!("../../server-store/tests/fixtures/node-schema.sql");
const ORIGIN: &str = "https://noevia.example.test";
const SETUP_CODE: &str = "synthetic-setup-code-0001";
const PASSWORD: &str = "synthetic password 0001";
const T0: i64 = 1_800_000_000_000;

struct World {
    dir: tempfile::TempDir,
    node: Connection,
    acct: Account,
}

fn world(native: bool) -> World {
    world_with(native, server_account::DavEnv::default())
}

fn world_with(native: bool, dav: server_account::DavEnv) -> World {
    let dir = tempfile::tempdir().unwrap();
    let node = Connection::open(server_store::db_path(dir.path())).unwrap();
    node.execute_batch(SCHEMA).unwrap();
    node.execute(
        "INSERT INTO settings VALUES('setup_code_hash', ?1)",
        [server_auth::js::digest(SETUP_CODE)],
    )
    .unwrap();
    std::fs::write(
        dir.path().join("first-run-setup-code"),
        format!("{SETUP_CODE}\n"),
    )
    .unwrap();
    let switch = RustAuth::owned();
    let writer = Arc::new(Writer::open(dir.path(), switch).unwrap());
    let auth = Arc::new(Authenticator::new(AuthConfig {
        public_origin: ORIGIN.into(),
        additional_origins: vec![],
        legacy_token: String::new(),
        legacy_compat: false,
        trust_proxy: true,
        native_client_auth_env: Some(native),
        origin_from_settings: true,
    }));
    let settings = Settings {
        public_origin: ORIGIN.into(),
        webauthn_rp_id: String::new(),
        trust_proxy: true,
        data_dir: dir.path().to_path_buf(),
        dav_available: false,
        dav,
    };
    // Real producer registration is now a prerequisite for device issuance.
    // Keep the original flow expectations; do not bypass the final context gate.
    use server_account::feature_context::{
        source_fingerprint, startup, BootFacts, BrowserFact, Cancellation, ContextSource, Control,
        Controller, FeatureRuntime, Registration, Snapshot,
    };
    let mut env = [None; 23];
    let native_index = server_features::Feature::ALL
        .iter()
        .position(|f| *f == server_features::Feature::NativeClientAuth)
        .unwrap();
    env[native_index] = Some(native);
    let controller = Arc::new(Controller::new(
        Arc::new(FeatureRuntime::new().unwrap()),
        env,
    ));
    let facts = BootFacts {
        env,
        restart: [false, false],
        browser: BrowserFact::Available,
        code: false,
        trust_proxy: true,
    };
    let snapshot = writer
        .read(|r| {
            Ok(Snapshot {
                decision: source_fingerprint(r, ContextSource::Decision)?,
                machines: source_fingerprint(r, ContextSource::InferenceMachines)?,
                choice: None,
                system_one_unconfigured: true,
            })
        })
        .unwrap();
    let cancel = Cancellation::default();
    let control = Control {
        request: uuid::Uuid::new_v4(),
        deadline: std::time::Instant::now() + std::time::Duration::from_secs(30),
        cancelled: cancel.signal(),
    };
    let registered = controller
        .register(
            &writer,
            &control,
            Registration {
                boot: uuid::Uuid::new_v4(),
                session: uuid::Uuid::new_v4(),
                facts: facts.clone(),
                snapshot,
            },
            |tx| startup::initialize(tx, &facts, T0),
        )
        .unwrap();
    assert!(matches!(
        registered,
        server_account::feature_context::Reply::Ready(_)
    ));
    let acct = Account::new(writer, auth, settings, switch, T0).with_feature_context(controller);
    World { dir, node, acct }
}

/// A browser: its cookie jar and CSRF token.
#[derive(Default, Clone)]
struct Browser {
    session: Option<String>,
    csrf: Option<String>,
    ip: String,
}

impl Browser {
    fn new(ip: &str) -> Self {
        Browser {
            ip: ip.into(),
            ..Self::default()
        }
    }
}

fn call(
    w: &World,
    b: &mut Browser,
    method: &str,
    path: &str,
    body: &str,
    now: i64,
) -> (u16, serde_json::Value, Reply) {
    let mut cookie = Vec::new();
    if let Some(s) = &b.session {
        cookie.push(format!("cowork_session={s}"));
    }
    if let Some(c) = &b.csrf {
        cookie.push(format!("cowork_csrf={c}"));
    }
    let req = Request {
        method: method.into(),
        path: path.into(),
        creds: Creds {
            cookie: (!cookie.is_empty()).then(|| cookie.join("; ")),
            authorization: None,
            origin: Some(ORIGIN.into()),
            csrf: b.csrf.clone(),
            method: method.into(),
        },
        user_agent: "synthetic-test".into(),
        content_type: "application/json".into(),
        client_ip: b.ip.clone(),
        body: Body {
            bytes: body.as_bytes().to_vec(),
            over: false,
        },
    };
    let Outcome::Reply(r) = w.acct.handle(&req, now) else {
        panic!("{method} {path} passed to Node");
    };
    for c in &r.cookies {
        let (pair, _) = c.split_once(';').unwrap();
        let (name, value) = pair.split_once('=').unwrap();
        let v = (!value.is_empty()).then(|| value.to_string());
        match name {
            "cowork_session" => b.session = v,
            "cowork_csrf" => b.csrf = v,
            _ => {}
        }
    }
    let v = serde_json::from_str(&r.body).unwrap_or(serde_json::Value::Null);
    (r.status, v, r)
}

fn count(w: &World, sql: &str) -> i64 {
    w.node.query_row(sql, [], |r| r.get(0)).unwrap()
}

fn setup(w: &World, b: &mut Browser) -> String {
    let body = format!(
        r#"{{"setupCode":"{SETUP_CODE}","publicOrigin":"{ORIGIN}","username":"Owner","password":"{PASSWORD}","diaryEnabled":true}}"#
    );
    let (s, v, r) = call(w, b, "POST", "/api/setup/complete", &body, T0);
    assert_eq!(s, 201, "{}", r.body);
    assert_eq!(v["migrationRequired"], true);
    assert_eq!(v["user"]["role"], "admin");
    assert_eq!(v["user"]["onboarded"], false);
    assert_eq!(v["user"]["diaryEnabled"], true);
    assert_eq!(r.cookies.len(), 2);
    assert!(
        r.cookies[0].ends_with("Max-Age=2592000; Secure"),
        "{}",
        r.cookies[0]
    );
    v["user"]["id"].as_str().unwrap().to_string()
}

#[test]
fn first_run_setup_signs_the_owner_in_once() {
    let w = world(false);
    let mut owner = Browser::new("10.0.0.1");
    let (s, v, _) = call(&w, &mut owner, "GET", "/api/setup/status", "", T0);
    assert_eq!(
        (s, v["configured"].clone(), v["publicOrigin"].clone()),
        (200, false.into(), ORIGIN.into())
    );
    // A wrong code, then the right one; the code is single use and its file goes.
    let (s, _, _) = call(
        &w,
        &mut owner,
        "POST",
        "/api/setup/complete",
        r#"{"setupCode":"nope"}"#,
        T0,
    );
    assert_eq!(s, 401);
    let id = setup(&w, &mut owner);
    assert!(!w.dir.path().join("first-run-setup-code").exists());
    assert_eq!(
        count(
            &w,
            "SELECT count(*) FROM settings WHERE key='setup_code_hash'"
        ),
        0
    );
    assert_eq!(
        count(
            &w,
            "SELECT count(*) FROM audit_events WHERE action='setup.complete'"
        ),
        1
    );
    let (s, _, _) = call(
        &w,
        &mut owner.clone(),
        "POST",
        "/api/setup/complete",
        "{}",
        T0,
    );
    assert_eq!(s, 409);
    let (s, v, _) = call(&w, &mut owner, "GET", "/api/auth/session", "", T0 + 1);
    assert_eq!(s, 200);
    assert_eq!(v["user"]["id"], id.as_str());
    assert_eq!(v["csrfToken"], owner.csrf.clone().unwrap().as_str());
    // Writes need the CSRF token.
    let mut no_csrf = Browser {
        csrf: None,
        ..owner.clone()
    };
    let (s, v, _) = call(
        &w,
        &mut no_csrf,
        "PATCH",
        "/api/profile",
        r#"{"displayName":"x"}"#,
        T0 + 2,
    );
    assert_eq!((s, v["error"].clone()), (403, "invalid CSRF token".into()));
    let (s, _, _) = call(
        &w,
        &mut owner,
        "PATCH",
        "/api/profile",
        r#"{"displayName":"  Renamed  "}"#,
        T0 + 2,
    );
    assert_eq!(s, 200);
    let (_, v, _) = call(&w, &mut owner, "GET", "/api/profile", "", T0 + 3);
    assert_eq!(v["user"]["displayName"], "Renamed");
    assert_eq!(v["sessions"][0]["current"], true);
    let (s, _, r) = call(&w, &mut owner, "POST", "/api/auth/logout", "{}", T0 + 4);
    assert_eq!(s, 200);
    assert!(r.cookies[0].contains("Max-Age=0"));
    let (s, _, r) = call(&w, &mut owner, "GET", "/api/auth/session", "", T0 + 5);
    assert_eq!(s, 401);
    assert_eq!(r.headers[0].1, "Bearer realm=\"cowork\"");
}

/// #1483: the profile's session list shows only sessions that can still sign in (the same rule
/// authenticate applies), removes the dead rows of this account, shows when each live one ends
/// (the earlier of its absolute and idle limits), and never touches another account's rows.
#[test]
fn profile_lists_only_sessions_that_can_still_sign_in() {
    const DAY: i64 = 24 * 60 * 60 * 1000;
    let w = world(false);
    let mut owner = Browser::new("198.51.100.7");
    let id = setup(&w, &mut owner);
    let live_hash = server_auth::js::digest(owner.session.as_deref().unwrap());
    // Synthetic rows: one dead for five weeks (absolute limit passed), one idle past the idle limit.
    let insert = |hash: &str, user: &str, created: i64, seen: i64, expires: i64| {
        w.node
            .execute(
                "INSERT INTO sessions(id_hash,user_id,csrf_hash,created_at,last_seen_at,expires_at,user_agent,ip) VALUES(?1,?2,'csrf',?3,?4,?5,'Synthetic Browser','127.0.0.1')",
                (hash, user, created, seen, expires),
            )
            .unwrap();
    };
    insert(
        "stale-five-weeks-1483",
        &id,
        T0 - 35 * DAY,
        T0 - 35 * DAY,
        T0 - 5 * DAY,
    );
    insert(
        "idle-eight-days-1483",
        &id,
        T0 - 10 * DAY,
        T0 - 8 * DAY,
        T0 + 20 * DAY,
    );
    // Another account's dead row: this account's listing must not remove it.
    w.node
        .execute(
            "INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,created_at,updated_at) VALUES('other-1483','Other','other-1483','Other','member','synthetic','wa-other-1483',?1,?1)",
            [T0],
        )
        .unwrap();
    insert(
        "other-stale-1483",
        "other-1483",
        T0 - 35 * DAY,
        T0 - 35 * DAY,
        T0 - 5 * DAY,
    );

    let (s, v, _) = call(&w, &mut owner, "GET", "/api/profile", "", T0 + DAY);
    assert_eq!(s, 200);
    let listed: Vec<&str> = v["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        listed,
        vec![live_hash.as_str()],
        "only the live session is listed"
    );
    assert_eq!(v["sessions"][0]["current"], true);
    // The live session ends at the earlier of its absolute limit and its idle limit.
    let ends: i64 = w
        .node
        .query_row(
            "SELECT MIN(expires_at,last_seen_at+604800000) FROM sessions WHERE id_hash=?1",
            [&live_hash],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(v["sessions"][0]["expiresAt"].as_i64(), Some(ends));
    assert!(ends > T0 + DAY, "a live session ends in the future");
    // The dead rows of this account are removed, not just hidden; the other account's row is kept.
    assert_eq!(
        count(
            &w,
            "SELECT count(*) FROM sessions WHERE id_hash='stale-five-weeks-1483'"
        ),
        0
    );
    assert_eq!(
        count(
            &w,
            "SELECT count(*) FROM sessions WHERE id_hash='idle-eight-days-1483'"
        ),
        0
    );
    assert_eq!(
        count(
            &w,
            "SELECT count(*) FROM sessions WHERE id_hash='other-stale-1483'"
        ),
        1
    );
}

#[test]
fn password_sign_in_and_its_limits() {
    let w = world(false);
    let mut owner = Browser::new("10.0.0.1");
    setup(&w, &mut owner);
    let mut b = Browser::new("10.0.0.9");
    let wrong = r#"{"username":"OWNER","password":"not the password at all"}"#;
    for _ in 0..5 {
        let (s, _, _) = call(&w, &mut b, "POST", "/api/auth/login/password", wrong, T0);
        assert_eq!(s, 401);
    }
    // The sixth attempt for this address and username is refused even with the right password.
    let right = format!(r#"{{"username":"owner","password":"{PASSWORD}"}}"#);
    let (s, _, _) = call(&w, &mut b, "POST", "/api/auth/login/password", &right, T0);
    assert_eq!(s, 429);
    // Another client address has its own bucket (the front sees real peers: noevia#1245).
    let mut c = Browser::new("10.0.0.10");
    let (s, v, _) = call(&w, &mut c, "POST", "/api/auth/login/password", &right, T0);
    assert_eq!(s, 200);
    assert_eq!(v["user"]["username"], "Owner");
    // After the window the first address signs in again.
    let (s, _, _) = call(
        &w,
        &mut b,
        "POST",
        "/api/auth/login/password",
        &right,
        T0 + 15 * 60 * 1000 + 1,
    );
    assert_eq!(s, 200);
    assert_eq!(
        count(
            &w,
            "SELECT count(*) FROM audit_events WHERE action='auth.password'"
        ),
        2
    );
}

#[test]
fn invitations_disable_and_recovery() {
    let w = world(false);
    let mut owner = Browser::new("10.0.0.1");
    let owner_id = setup(&w, &mut owner);
    let (s, v, _) = call(
        &w,
        &mut owner,
        "POST",
        "/api/admin/invitations",
        r#"{"role":"member"}"#,
        T0,
    );
    assert_eq!(s, 201);
    let token = v["token"].as_str().unwrap().to_string();
    let mut member = Browser::new("10.0.0.2");
    let body = format!(r#"{{"token":"{token}","username":"Member","password":"{PASSWORD}"}}"#);
    let (s, v, _) = call(
        &w,
        &mut member,
        "POST",
        "/api/auth/invitations/accept",
        &body,
        T0 + 1,
    );
    assert_eq!(s, 201);
    let member_id = v["user"]["id"].as_str().unwrap().to_string();
    assert_eq!(v["user"]["role"], "member");
    // Single use.
    let (s, _, _) = call(
        &w,
        &mut Browser::new("10.0.0.3"),
        "POST",
        "/api/auth/invitations/accept",
        &body,
        T0 + 2,
    );
    assert_eq!(s, 400);
    // Members are not administrators.
    let (s, v, _) = call(&w, &mut member, "GET", "/api/admin/users", "", T0 + 3);
    assert_eq!(
        (s, v["error"].clone()),
        (403, "administrator required".into())
    );
    let (_, v, _) = call(&w, &mut owner, "GET", "/api/admin/users", "", T0 + 3);
    assert_eq!(v["users"].as_array().unwrap().len(), 2);
    // The last administrator cannot be disabled; the member can, and is signed out.
    let (s, v, _) = call(
        &w,
        &mut owner,
        "PUT",
        &format!("/api/admin/users/{owner_id}/disabled"),
        r#"{"disabled":true}"#,
        T0 + 4,
    );
    assert_eq!(
        (s, v["error"].clone()),
        (400, "cannot disable the last administrator".into())
    );
    let (s, _, _) = call(
        &w,
        &mut owner,
        "PUT",
        &format!("/api/admin/users/{member_id}/disabled"),
        r#"{"disabled":true}"#,
        T0 + 4,
    );
    assert_eq!(s, 200);
    let (s, _, _) = call(&w, &mut member, "GET", "/api/profile", "", T0 + 5);
    assert_eq!(s, 401);
    let (s, _, _) = call(
        &w,
        &mut owner,
        "PUT",
        &format!("/api/admin/users/{member_id}/disabled"),
        r#"{"disabled":false}"#,
        T0 + 6,
    );
    assert_eq!(s, 200);
    // A recovery link replaces the password and revokes every other credential.
    let mut again = Browser::new("10.0.0.2");
    let login = format!(r#"{{"username":"member","password":"{PASSWORD}"}}"#);
    assert_eq!(
        call(
            &w,
            &mut again,
            "POST",
            "/api/auth/login/password",
            &login,
            T0 + 7
        )
        .0,
        200
    );
    let (s, _, _) = call(
        &w,
        &mut again,
        "POST",
        "/api/profile/app-passwords",
        r#"{"name":"Phone","scope":"lan"}"#,
        T0 + 8,
    );
    assert_eq!(s, 201);
    let (s, v, _) = call(
        &w,
        &mut owner,
        "POST",
        &format!("/api/admin/users/{member_id}/recovery"),
        "",
        T0 + 9,
    );
    assert_eq!(s, 201);
    let rec = v["token"].as_str().unwrap().to_string();
    let (s, v, _) = call(
        &w,
        &mut Browser::new("10.0.0.4"),
        "POST",
        "/api/auth/recovery/complete",
        &format!(r#"{{"token":"{rec}","password":"short"}}"#),
        T0 + 10,
    );
    assert_eq!(
        (s, v["error"].clone()),
        (400, "password must be 12-128 characters".into())
    );
    let new_pw = "a brand new synthetic password";
    let (s, _, _) = call(
        &w,
        &mut Browser::new("10.0.0.4"),
        "POST",
        "/api/auth/recovery/complete",
        &format!(r#"{{"token":"{rec}","password":"{new_pw}"}}"#),
        T0 + 11,
    );
    assert_eq!(s, 200);
    assert_eq!(
        count(
            &w,
            &format!("SELECT count(*) FROM sessions WHERE user_id='{member_id}'")
        ),
        0
    );
    assert_eq!(
        count(
            &w,
            &format!("SELECT count(*) FROM app_passwords WHERE user_id='{member_id}'")
        ),
        0
    );
    assert_eq!(
        count(
            &w,
            &format!("SELECT credential_epoch FROM users WHERE id='{member_id}'")
        ),
        1
    );
    let detail: String = w
        .node
        .query_row(
            "SELECT detail FROM audit_events WHERE action='recovery.complete'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        detail,
        r#"{"revoked":{"recoveries":0,"passkeys":0,"appPasswords":1,"diaryConnectors":0}}"#
    );
    let (s, _, _) = call(&w, &mut again, "GET", "/api/profile", "", T0 + 12);
    assert_eq!(s, 401);
    let login = format!(r#"{{"username":"member","password":"{new_pw}"}}"#);
    assert_eq!(
        call(
            &w,
            &mut Browser::new("10.0.0.5"),
            "POST",
            "/api/auth/login/password",
            &login,
            T0 + 13
        )
        .0,
        200
    );
}

#[test]
fn app_passwords_appearance_and_records() {
    let w = world(false);
    let mut owner = Browser::new("10.0.0.1");
    let id = setup(&w, &mut owner);
    let (s, v, _) = call(
        &w,
        &mut owner,
        "POST",
        "/api/profile/app-passwords",
        r#"{"name":" Phone ","scope":"lan"}"#,
        T0,
    );
    assert_eq!(s, 201);
    assert_eq!(v["name"], "Phone");
    let pw = v["password"].as_str().unwrap().to_string();
    let app_id = v["id"].as_str().unwrap().to_string();
    assert!(pw.starts_with(&format!("nv_dav_{app_id}.")));
    // The stored hash verifies the password (what the DAV listener checks).
    let hash: String = w
        .node
        .query_row("SELECT password_hash FROM app_passwords", [], |r| r.get(0))
        .unwrap();
    assert!(hash.starts_with("$argon2id$v=19$m=19456,t=2,p=1$"));
    assert!(server_auth::password::verify(&hash, &pw));
    let (s, v, _) = call(
        &w,
        &mut owner,
        "POST",
        "/api/profile/app-passwords",
        r#"{"name":"","scope":"lan"}"#,
        T0,
    );
    assert_eq!(
        (s, v["error"].clone()),
        (400, "Use a device name of 1–80 characters.".into())
    );
    let (_, v, _) = call(&w, &mut owner, "GET", "/api/profile/app-passwords", "", T0);
    assert_eq!(v["appPasswords"][0]["id"], app_id.as_str());
    assert_eq!(v["sharingAvailable"], false);
    let (s, _, _) = call(
        &w,
        &mut owner,
        "DELETE",
        &format!("/api/profile/app-passwords/{app_id}"),
        "",
        T0,
    );
    assert_eq!(s, 200);
    let (s, _, _) = call(
        &w,
        &mut owner,
        "DELETE",
        &format!("/api/profile/app-passwords/{app_id}"),
        "",
        T0,
    );
    assert_eq!(s, 404);
    // An id the front never issues is the generic 404 (contract corpus exchange 186), not Node's.
    let (s, v, _) = call(
        &w,
        &mut owner,
        "DELETE",
        "/api/profile/app-passwords/missing-app-password",
        "",
        T0,
    );
    assert_eq!((s, v["error"].clone()), (404, "not found".into()));
    // Appearance.
    let (_, v, _) = call(&w, &mut owner, "GET", "/api/profile/appearance", "", T0);
    assert!(v.is_null());
    let (s, _, r) = call(
        &w,
        &mut owner,
        "PUT",
        "/api/profile/appearance",
        r#"{"dark":"iris","theme":"dark","light":"warm","x":1}"#,
        T0,
    );
    assert_eq!(
        (s, r.body.as_str()),
        (200, r#"{"theme":"dark","light":"warm","dark":"iris"}"#)
    );
    assert_eq!(
        call(&w, &mut owner, "DELETE", "/api/profile/appearance", "", T0).0,
        405
    );
    // Account records in users/<id>/, with the JS modules' key order.
    let (s, _, r) = call(
        &w,
        &mut owner,
        "PUT",
        "/api/account/preferences",
        r#"{"sendKey":"mod-enter"}"#,
        T0 + 1,
    );
    assert_eq!(s, 200, "{}", r.body);
    let file = std::fs::read_to_string(
        w.dir
            .path()
            .join("users")
            .join(&id)
            .join("account-preferences.json"),
    )
    .unwrap();
    assert_eq!(
        file,
        format!(
            r#"{{"notifications":{{"replyFinished":true,"approvalNeeded":true}},"sendKey":"mod-enter","locale":"system","updatedAt":{}}}"#,
            T0 + 1
        )
    );
    let (s, v, _) = call(
        &w,
        &mut owner,
        "PUT",
        "/api/account/preferences",
        r#"{"locale":"xx"}"#,
        T0 + 2,
    );
    assert_eq!(
        (s, v["error"].clone()),
        (400, "That format is not available.".into())
    );
    let (s, v, _) = call(
        &w,
        &mut owner,
        "PUT",
        "/api/account/memory",
        r#"{"memories":["  likes   tea ","likes tea",""]}"#,
        T0 + 3,
    );
    assert_eq!(
        (s, v["memories"].clone()),
        (200, serde_json::json!(["likes tea"]))
    );
    let (s, _, _) = call(
        &w,
        &mut owner,
        "PUT",
        "/api/account/memory",
        r#"{"memories":[]}"#,
        T0 + 4,
    );
    assert_eq!(s, 200);
    assert!(!w
        .dir
        .path()
        .join("users")
        .join(&id)
        .join("account-memory.json")
        .exists());
    let (s, v, _) = call(
        &w,
        &mut owner,
        "PUT",
        "/api/account/instructions",
        r#"{"text":" Be brief. ","style":"concise","language":"Norwegian"}"#,
        T0 + 5,
    );
    assert_eq!(s, 200);
    assert_eq!(v["text"], "Be brief.");
    assert_eq!(v["advanced"]["tone"], "auto");
    assert_eq!(v["maxChars"], 4000);
    let (s, v, _) = call(
        &w,
        &mut owner,
        "PUT",
        "/api/account/instructions",
        "not json",
        T0 + 6,
    );
    assert_eq!((s, v["error"].clone()), (400, "invalid JSON".into()));
    let (s, _, _) = call(
        &w,
        &mut owner,
        "PUT",
        "/api/profile/features",
        r#"{"diaryEnabled":0}"#,
        T0 + 7,
    );
    assert_eq!(s, 200);
    assert_eq!(
        count(
            &w,
            &format!("SELECT diary_enabled FROM user_features WHERE user_id='{id}'")
        ),
        0
    );
}

#[test]
fn a_passkey_registers_and_signs_in() {
    use p256::ecdsa::signature::Signer;
    use passkey::cbor::{encode, Cbor};
    use sha2::Digest;
    let w = world(false);
    let mut owner = Browser::new("10.0.0.1");
    setup(&w, &mut owner);
    let (s, v, _) = call(
        &w,
        &mut owner,
        "POST",
        "/api/auth/passkeys/register/options",
        "{}",
        T0,
    );
    assert_eq!(s, 200);
    let rp = v["options"]["rp"]["id"].as_str().unwrap().to_string();
    assert_eq!(rp, "noevia.example.test");
    assert_eq!(
        v["options"]["authenticatorSelection"]["requireResidentKey"],
        false
    );
    let challenge = v["options"]["challenge"].as_str().unwrap().to_string();
    let token = v["challengeToken"].as_str().unwrap().to_string();
    let sk = p256::ecdsa::SigningKey::random(&mut rand_core::OsRng);
    let pt = sk.verifying_key().to_encoded_point(false);
    let key = encode(&Cbor::Map(vec![
        (Cbor::Num(1.0), Cbor::Num(2.0)),
        (Cbor::Num(3.0), Cbor::Num(-7.0)),
        (Cbor::Num(-1.0), Cbor::Num(1.0)),
        (Cbor::Num(-2.0), Cbor::Bytes(pt.x().unwrap().to_vec())),
        (Cbor::Num(-3.0), Cbor::Bytes(pt.y().unwrap().to_vec())),
    ]));
    let cred_id = [9u8; 16];
    let mut ad = sha2::Sha256::digest(rp.as_bytes()).to_vec();
    ad.push(0x45);
    ad.extend([0, 0, 0, 0]);
    ad.extend([0u8; 16]);
    ad.extend([0, 16]);
    ad.extend(cred_id);
    ad.extend(&key);
    let ao = encode(&Cbor::Map(vec![
        (Cbor::Text("fmt".into()), Cbor::Text("none".into())),
        (Cbor::Text("attStmt".into()), Cbor::Map(vec![])),
        (Cbor::Text("authData".into()), Cbor::Bytes(ad)),
    ]));
    let b64 = passkey::b64::from_buffer;
    let cdj = b64(format!(
        r#"{{"type":"webauthn.create","challenge":"{challenge}","origin":"{ORIGIN}"}}"#
    )
    .as_bytes());
    let id = b64(&cred_id);
    let body = serde_json::json!({"challengeToken": token, "name": "Laptop", "response": {"id": id, "rawId": id, "type": "public-key",
        "response": {"clientDataJSON": cdj, "attestationObject": b64(&ao), "transports": ["internal"]}}});
    let (s, v, r) = call(
        &w,
        &mut owner,
        "POST",
        "/api/auth/passkeys/register/verify",
        &body.to_string(),
        T0 + 1,
    );
    assert_eq!(s, 200, "{}", r.body);
    assert_eq!(v["verified"], true);
    // The same challenge token is single use.
    let (s, v, _) = call(
        &w,
        &mut owner,
        "POST",
        "/api/auth/passkeys/register/verify",
        &body.to_string(),
        T0 + 2,
    );
    assert_eq!(
        (s, v["error"].clone()),
        (400, "registration challenge expired".into())
    );
    let stored: (String, Vec<u8>, String) = w
        .node
        .query_row(
            "SELECT rp_id, public_key, transports FROM passkeys",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        (stored.0.as_str(), stored.1, stored.2.as_str()),
        ("noevia.example.test", key, r#"["internal"]"#)
    );
    // Sign in with it, from another browser.
    let mut anon = Browser::new("10.0.0.7");
    let (s, v, _) = call(
        &w,
        &mut anon,
        "POST",
        "/api/auth/login/passkey/options",
        r#"{"username":"owner"}"#,
        T0 + 3,
    );
    assert_eq!(s, 200);
    let list = v["options"]["allowCredentials"].as_array().unwrap();
    assert_eq!(list.len(), 5, "padded with decoys");
    assert!(list.iter().any(|c| c["id"] == id.as_str()));
    let challenge = v["options"]["challenge"].as_str().unwrap().to_string();
    let token = v["challengeToken"].as_str().unwrap().to_string();
    let mut ad = sha2::Sha256::digest(rp.as_bytes()).to_vec();
    ad.push(0x05);
    ad.extend([0, 0, 0, 1]);
    let cdj = b64(format!(
        r#"{{"type":"webauthn.get","challenge":"{challenge}","origin":"{ORIGIN}"}}"#
    )
    .as_bytes());
    let mut base = ad.clone();
    base.extend(sha2::Sha256::digest(passkey::b64::to_buffer(&cdj)));
    let sig: p256::ecdsa::Signature = sk.sign(&base);
    let body = serde_json::json!({"challengeToken": token, "response": {"id": id, "rawId": id, "type": "public-key",
        "response": {"clientDataJSON": cdj, "authenticatorData": b64(&ad), "signature": b64(sig.to_der().as_bytes())}}});
    let (s, v, r) = call(
        &w,
        &mut anon,
        "POST",
        "/api/auth/login/passkey/verify",
        &body.to_string(),
        T0 + 4,
    );
    assert_eq!(s, 200, "{}", r.body);
    assert_eq!(v["user"]["username"], "Owner");
    assert_eq!(count(&w, "SELECT counter FROM passkeys"), 1);
    // A replay of the same assertion is refused (the challenge is gone).
    let (s, _, _) = call(
        &w,
        &mut Browser::new("10.0.0.8"),
        "POST",
        "/api/auth/login/passkey/verify",
        &body.to_string(),
        T0 + 5,
    );
    assert_eq!(s, 401);
    // The only remaining sign-in methods: the password stays, so the passkey can go.
    let (s, _, _) = call(
        &w,
        &mut owner,
        "DELETE",
        &format!("/api/auth/passkeys/{id}"),
        "",
        T0 + 6,
    );
    assert_eq!(s, 200);
}

#[test]
fn device_sign_in_rotates_and_detects_reuse() {
    let w = world(true);
    let mut owner = Browser::new("10.0.0.1");
    setup(&w, &mut owner);
    let mut app = Browser::new("10.0.0.20");
    let (s, v, _) = call(
        &w,
        &mut app,
        "POST",
        "/api/auth/device/code",
        r#"{"client_name":"Synthetic\u0000Mac"}"#,
        T0,
    );
    assert_eq!(s, 200);
    assert_eq!(v["verification_uri"], format!("{ORIGIN}/device"));
    let device_code = v["device_code"].as_str().unwrap().to_string();
    let user_code = v["user_code"].as_str().unwrap().to_string();
    let poll = format!(
        r#"{{"grant_type":"urn:ietf:params:oauth:grant-type:device_code","device_code":"{device_code}"}}"#
    );
    let (s, v, _) = call(
        &w,
        &mut app,
        "POST",
        "/api/auth/device/token",
        &poll,
        T0 + 1,
    );
    assert_eq!(
        (s, v["error"].clone()),
        (400, "authorization_pending".into())
    );
    let (s, v, _) = call(
        &w,
        &mut app,
        "POST",
        "/api/auth/device/token",
        &poll,
        T0 + 2,
    );
    assert_eq!((s, v["error"].clone()), (400, "slow_down".into()));
    let (s, v, _) = call(
        &w,
        &mut owner,
        "POST",
        "/api/auth/device/lookup",
        &format!(r#"{{"user_code":"{}"}}"#, user_code.to_lowercase()),
        T0 + 3,
    );
    assert_eq!(s, 200);
    assert_eq!(v["clientName"], "Synthetic Mac");
    let (s, _, _) = call(
        &w,
        &mut owner,
        "POST",
        "/api/auth/device/approve",
        &format!(r#"{{"user_code":"{user_code}","approve":true}}"#),
        T0 + 4,
    );
    assert_eq!(s, 200);
    let (s, v, _) = call(
        &w,
        &mut app,
        "POST",
        "/api/auth/device/token",
        &poll,
        T0 + 20_000,
    );
    assert_eq!(s, 200);
    let refresh = v["refresh_token"].as_str().unwrap().to_string();
    assert!(v["access_token"].as_str().unwrap().starts_with("nva_"));
    // The device code is single use.
    let (s, _, _) = call(
        &w,
        &mut app,
        "POST",
        "/api/auth/device/token",
        &poll,
        T0 + 30_000,
    );
    assert_eq!(s, 400);
    let rot = |t: &str| format!(r#"{{"grant_type":"refresh_token","refresh_token":"{t}"}}"#);
    let (s, v, _) = call(
        &w,
        &mut app,
        "POST",
        "/api/auth/device/token",
        &rot(&refresh),
        T0 + 40_000,
    );
    assert_eq!(s, 200);
    let next = v["refresh_token"].as_str().unwrap().to_string();
    // A retry with the old token inside the grace window, while its successor is unused, works once.
    let (s, v, _) = call(
        &w,
        &mut app,
        "POST",
        "/api/auth/device/token",
        &rot(&refresh),
        T0 + 50_000,
    );
    assert_eq!(s, 200);
    let after_grace = v["refresh_token"].as_str().unwrap().to_string();
    assert_eq!(
        count(
            &w,
            "SELECT count(*) FROM audit_events WHERE action='device.refresh_grace'"
        ),
        1
    );
    // The discarded successor presented later is reuse: the grant is revoked.
    let (s, v, _) = call(
        &w,
        &mut app,
        "POST",
        "/api/auth/device/token",
        &rot(&next),
        T0 + 60_000,
    );
    assert_eq!((s, v["error"].clone()), (400, "invalid_grant".into()));
    assert_eq!(count(&w, "SELECT count(*) FROM device_grants"), 0);
    let (s, _, _) = call(
        &w,
        &mut app,
        "POST",
        "/api/auth/device/token",
        &rot(&after_grace),
        T0 + 70_000,
    );
    assert_eq!(s, 400);
}

#[test]
fn feature_off_hides_device_paths() {
    let w = world(false);
    let mut b = Browser::new("10.0.0.1");
    let (s, v, _) = call(&w, &mut b, "POST", "/api/auth/device/code", "{}", T0);
    assert_eq!((s, v["error"].clone()), (404, "not found".into()));
}

/// A P-256 passkey made outside the routes (as a row Node stored), and an assertion for it.
struct Key {
    sk: p256::ecdsa::SigningKey,
    id: String,
}

fn stored_key(w: &World, user_id: &str, rp_id: Option<&str>) -> Key {
    use passkey::cbor::{encode, Cbor};
    let sk = p256::ecdsa::SigningKey::random(&mut rand_core::OsRng);
    let pt = sk.verifying_key().to_encoded_point(false);
    let cose = encode(&Cbor::Map(vec![
        (Cbor::Num(1.0), Cbor::Num(2.0)),
        (Cbor::Num(3.0), Cbor::Num(-7.0)),
        (Cbor::Num(-1.0), Cbor::Num(1.0)),
        (Cbor::Num(-2.0), Cbor::Bytes(pt.x().unwrap().to_vec())),
        (Cbor::Num(-3.0), Cbor::Bytes(pt.y().unwrap().to_vec())),
    ]));
    let id = passkey::b64::from_buffer(&server_account::util::random_bytes(16));
    w.node
        .execute(
            "INSERT INTO passkeys(id,user_id,name,public_key,webauthn_user_id,counter,device_type,backed_up,transports,created_at,rp_id) VALUES(?1,?2,'Old',?3,'w',0,'multiDevice',1,'[\"internal\"]',1,?4)",
            (&id, user_id, &cose, rp_id),
        )
        .unwrap();
    Key { sk, id }
}

fn sign_in(w: &World, key: &Key, rp: &str, origin: &str, now: i64) -> (u16, String) {
    use p256::ecdsa::signature::Signer;
    use sha2::Digest;
    let mut anon = Browser::new("10.0.0.30");
    let (s, v, _) = call(
        w,
        &mut anon,
        "POST",
        "/api/auth/login/passkey/options",
        r#"{"username":"owner"}"#,
        now,
    );
    assert_eq!(s, 200);
    assert_eq!(v["options"]["rpId"], rp, "the ceremony's RP ID");
    let challenge = v["options"]["challenge"].as_str().unwrap().to_string();
    let token = v["challengeToken"].as_str().unwrap().to_string();
    let b64 = passkey::b64::from_buffer;
    let mut ad = sha2::Sha256::digest(rp.as_bytes()).to_vec();
    ad.push(0x05);
    ad.extend([0, 0, 0, 0]);
    let cdj = b64(format!(
        r#"{{"type":"webauthn.get","challenge":"{challenge}","origin":"{origin}"}}"#
    )
    .as_bytes());
    let mut base = ad.clone();
    base.extend(sha2::Sha256::digest(passkey::b64::to_buffer(&cdj)));
    let sig: p256::ecdsa::Signature = key.sk.sign(&base);
    let body = serde_json::json!({"challengeToken": token, "response": {"id": key.id, "rawId": key.id, "type": "public-key",
        "response": {"clientDataJSON": cdj, "authenticatorData": b64(&ad), "signature": b64(sig.to_der().as_bytes())}}});
    let (s, _, r) = call(
        w,
        &mut anon,
        "POST",
        "/api/auth/login/passkey/verify",
        &body.to_string(),
        now + 1,
    );
    (s, r.body)
}

#[test]
fn legacy_and_renamed_passkeys_keep_signing_in() {
    let w = world(false);
    let mut owner = Browser::new("10.0.0.1");
    let uid = setup(&w, &mut owner);
    // rp_id NULL (a row from before passkeys remembered their RP): the current RP ID.
    let legacy = stored_key(&w, &uid, None);
    assert_eq!(
        sign_in(&w, &legacy, "noevia.example.test", ORIGIN, T0).0,
        200
    );
    // Renamed site: the only passkey was made under the old name. The ceremony uses that RP ID and
    // the old address is still an accepted origin (previous_origins).
    w.node.execute("DELETE FROM passkeys", []).unwrap();
    w.node
        .execute(
            "INSERT INTO settings VALUES('previous_origins','[\"https://old.example.test\"]')",
            [],
        )
        .unwrap();
    let old = stored_key(&w, &uid, Some("old.example.test"));
    let (s, body) = sign_in(
        &w,
        &old,
        "old.example.test",
        "https://old.example.test",
        T0 + 10,
    );
    assert_eq!(s, 200, "{body}");
    // The wrong RP for that key is refused.
    let (s, _) = {
        let mut anon = Browser::new("10.0.0.31");
        let (_, v, _) = call(
            &w,
            &mut anon,
            "POST",
            "/api/auth/login/passkey/options",
            r#"{"username":"owner"}"#,
            T0 + 20,
        );
        assert_eq!(v["options"]["rpId"], "old.example.test");
        sign_in_with_rp(&w, &old, "noevia.example.test", ORIGIN, T0 + 30)
    };
    assert_eq!(s, 401);
}

fn sign_in_with_rp(w: &World, key: &Key, signed_rp: &str, origin: &str, now: i64) -> (u16, String) {
    use p256::ecdsa::signature::Signer;
    use sha2::Digest;
    let mut anon = Browser::new("10.0.0.32");
    let (_, v, _) = call(
        w,
        &mut anon,
        "POST",
        "/api/auth/login/passkey/options",
        r#"{"username":"owner"}"#,
        now,
    );
    let challenge = v["options"]["challenge"].as_str().unwrap().to_string();
    let token = v["challengeToken"].as_str().unwrap().to_string();
    let b64 = passkey::b64::from_buffer;
    let mut ad = sha2::Sha256::digest(signed_rp.as_bytes()).to_vec();
    ad.push(0x05);
    ad.extend([0, 0, 0, 0]);
    let cdj = b64(format!(
        r#"{{"type":"webauthn.get","challenge":"{challenge}","origin":"{origin}"}}"#
    )
    .as_bytes());
    let mut base = ad.clone();
    base.extend(sha2::Sha256::digest(passkey::b64::to_buffer(&cdj)));
    let sig: p256::ecdsa::Signature = key.sk.sign(&base);
    let body = serde_json::json!({"challengeToken": token, "response": {"id": key.id, "rawId": key.id, "type": "public-key",
        "response": {"clientDataJSON": cdj, "authenticatorData": b64(&ad), "signature": b64(sig.to_der().as_bytes())}}});
    let (s, _, r) = call(
        w,
        &mut anon,
        "POST",
        "/api/auth/login/passkey/verify",
        &body.to_string(),
        now + 1,
    );
    (s, r.body)
}

#[test]
fn a_cookie_and_a_device_token_together_are_refused() {
    let w = world(true);
    let mut owner = Browser::new("10.0.0.1");
    setup(&w, &mut owner);
    let req = Request {
        method: "GET".into(),
        path: "/api/auth/session".into(),
        creds: Creds {
            cookie: Some(format!("cowork_session={}", owner.session.clone().unwrap())),
            authorization: Some("Bearer nva_synthetic".into()),
            origin: None,
            csrf: None,
            method: "GET".into(),
        },
        user_agent: String::new(),
        content_type: String::new(),
        client_ip: "10.0.0.1".into(),
        body: Body::default(),
    };
    let Outcome::Reply(r) = w.acct.handle(&req, T0) else {
        panic!("passed")
    };
    assert_eq!(
        (r.status, r.body.as_str()),
        (
            400,
            r#"{"error":"Send either a session cookie or a bearer token, not both.","code":"ambiguous_credentials"}"#
        )
    );
}

/// core dav-settings.cjs's table, as core creates it at boot.
const DAV_SETTINGS: &str = "CREATE TABLE IF NOT EXISTS dav_settings(user_id TEXT PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    scope TEXT NOT NULL CHECK(scope IN ('off','lan','public')), acknowledged_at INTEGER, updated_at INTEGER NOT NULL);";

fn lan_cleartext() -> server_account::DavEnv {
    server_account::DavEnv {
        port: 8090.0,
        ui_port: 8021.0,
        scope: Some("lan".into()),
        origin: Some("http://10.0.0.5:8090".into()),
        proxy_token_ok: false,
        host: None,
    }
}

// #1071: Settings -> Sharing is the front's, answer for answer with dav-settings.cjs.
#[test]
fn sharing_reports_and_saves_the_opt_in() {
    let w = world_with(false, lan_cleartext());
    w.node.execute_batch(DAV_SETTINGS).unwrap();
    let mut owner = Browser::new("10.0.0.1");
    let uid = setup(&w, &mut owner);
    let (s, _, r) = call(&w, &mut owner, "GET", "/api/profile/sharing", "", T0);
    assert_eq!(
        (s, r.body.as_str()),
        (
            200,
            r#"{"available":true,"scope":"off","endpointScope":"lan","cleartext":true,"port":8090,"url":"http://10.0.0.5:8090/dav/Owner/","eligible":true}"#
        )
    );
    for (body, msg) in [
        (r#"{"scope":"everyone"}"#, "Choose a sharing scope."),
        (
            r#"{"scope":"public"}"#,
            "This sharing scope is not configured by the operator.",
        ),
        (
            r#"{"scope":"lan"}"#,
            "Acknowledge that plain HTTP sends device credentials unencrypted.",
        ),
        (
            r#"{"scope":"lan","acknowledgeCleartext":"yes"}"#,
            "Acknowledge that plain HTTP sends device credentials unencrypted.",
        ),
        ("[]", "request body must be a JSON object"),
        ("{", "invalid JSON"),
    ] {
        let (s, v, _) = call(&w, &mut owner, "PUT", "/api/profile/sharing", body, T0 + 1);
        assert_eq!((s, v["error"].as_str()), (400, Some(msg)), "{body}");
    }
    assert_eq!(count(&w, "SELECT count(*) FROM dav_settings"), 0);
    let (s, v, _) = call(
        &w,
        &mut owner,
        "PUT",
        "/api/profile/sharing",
        r#"{"scope":"lan","acknowledgeCleartext":true}"#,
        T0 + 2,
    );
    assert_eq!((s, v["scope"].as_str()), (200, Some("lan")));
    assert_eq!(
        count(&w, &format!("SELECT count(*) FROM dav_settings WHERE user_id='{uid}' AND scope='lan' AND acknowledged_at={} AND updated_at={}", T0 + 2, T0 + 2)),
        1
    );
    assert_eq!(
        count(&w, &format!("SELECT count(*) FROM audit_events WHERE action='dav.sharing' AND actor_user_id='{uid}' AND target_user_id='{uid}' AND detail='{{\"scope\":\"lan\"}}'")),
        1
    );
    // Off never needs eligibility or an acknowledgement, and clears it.
    w.node
        .execute(
            "INSERT INTO storage_connections(user_id,kind,updated_at) VALUES(?1,'webdav',1)",
            [&uid],
        )
        .unwrap();
    let (_, v, _) = call(&w, &mut owner, "GET", "/api/profile/sharing", "", T0 + 3);
    assert_eq!(
        (v["scope"].as_str(), v["eligible"].as_bool()),
        (Some("lan"), Some(false))
    );
    let (s, v, _) = call(
        &w,
        &mut owner,
        "PUT",
        "/api/profile/sharing",
        r#"{"scope":"lan","acknowledgeCleartext":true}"#,
        T0 + 4,
    );
    assert_eq!(
        (s, v["error"].as_str()),
        (
            400,
            Some("Sharing requires enabled Diary with server-held local storage.")
        )
    );
    let (s, v, _) = call(
        &w,
        &mut owner,
        "PUT",
        "/api/profile/sharing",
        r#"{"scope":"off"}"#,
        T0 + 5,
    );
    assert_eq!((s, v["scope"].as_str()), (200, Some("off")));
    assert_eq!(
        count(&w, &format!("SELECT count(*) FROM dav_settings WHERE user_id='{uid}' AND scope='off' AND acknowledged_at IS NULL")),
        1
    );
}

// noevia#2142: only validation answers 400. A store failure is a 500 that does not show the
// database's own message, an oversized body keeps its 413, and the row and its audit event are one
// transaction (a failed audit leaves no opt-in behind).
#[test]
fn sharing_save_failures_are_500_without_the_driver_message_and_all_or_nothing() {
    let w = world_with(false, lan_cleartext());
    w.node.execute_batch(DAV_SETTINGS).unwrap();
    let mut owner = Browser::new("10.0.0.1");
    setup(&w, &mut owner);
    let save = r#"{"scope":"lan","acknowledgeCleartext":true}"#;

    // The audit insert fails: no sharing row either.
    w.node
        .execute_batch(
            "CREATE TRIGGER fail_audit BEFORE INSERT ON audit_events WHEN NEW.action='dav.sharing'
             BEGIN SELECT RAISE(ABORT, 'synthetic driver detail audit'); END;",
        )
        .unwrap();
    let (s, v, r) = call(&w, &mut owner, "PUT", "/api/profile/sharing", save, T0 + 1);
    assert_eq!(
        (s, v["error"].as_str()),
        (500, Some("Internal error")),
        "{}",
        r.body
    );
    assert!(!r.body.contains("synthetic driver detail"), "{}", r.body);
    assert_eq!(count(&w, "SELECT count(*) FROM dav_settings"), 0);
    assert_eq!(
        count(
            &w,
            "SELECT count(*) FROM audit_events WHERE action='dav.sharing'"
        ),
        0
    );
    w.node.execute_batch("DROP TRIGGER fail_audit").unwrap();

    // The row write itself fails (a constraint, as a busy or read-only database would).
    w.node
        .execute_batch(
            "CREATE TRIGGER fail_row BEFORE INSERT ON dav_settings
             BEGIN SELECT RAISE(ABORT, 'synthetic driver detail row'); END;",
        )
        .unwrap();
    let (s, v, r) = call(&w, &mut owner, "PUT", "/api/profile/sharing", save, T0 + 2);
    assert_eq!(
        (s, v["error"].as_str()),
        (500, Some("Internal error")),
        "{}",
        r.body
    );
    assert!(!r.body.contains("synthetic driver detail"), "{}", r.body);
    assert_eq!(
        count(
            &w,
            "SELECT count(*) FROM audit_events WHERE action='dav.sharing'"
        ),
        0
    );
    w.node.execute_batch("DROP TRIGGER fail_row").unwrap();

    // The size refusal is 413, not 400.
    let big = format!(r#"{{"scope":"off","pad":"{}"}}"#, "a".repeat(1024 * 1024));
    let (s, v, _) = call(&w, &mut owner, "PUT", "/api/profile/sharing", &big, T0 + 3);
    assert_eq!(
        (s, v["error"].as_str()),
        (413, Some("Request exceeds size limit"))
    );

    // With the failures gone the same save works and writes both rows.
    let (s, v, _) = call(&w, &mut owner, "PUT", "/api/profile/sharing", save, T0 + 4);
    assert_eq!((s, v["scope"].as_str()), (200, Some("lan")));
    assert_eq!(count(&w, "SELECT count(*) FROM dav_settings"), 1);
    assert_eq!(
        count(
            &w,
            "SELECT count(*) FROM audit_events WHERE action='dav.sharing'"
        ),
        1
    );
}

#[test]
fn sharing_without_an_endpoint_or_a_table() {
    let w = world(false);
    let mut owner = Browser::new("10.0.0.1");
    setup(&w, &mut owner);
    // Before core created dav_settings: off, and nothing can be saved.
    let (s, _, r) = call(&w, &mut owner, "GET", "/api/profile/sharing", "", T0);
    assert_eq!(
        (s, r.body.as_str()),
        (
            200,
            r#"{"available":false,"reasonId":"not-configured","reason":"The operator has not configured a file-sharing endpoint.","scope":"off","cleartext":false,"url":"","eligible":true}"#
        )
    );
    let (s, _, _) = call(
        &w,
        &mut owner,
        "PUT",
        "/api/profile/sharing",
        r#"{"scope":"off"}"#,
        T0 + 1,
    );
    assert_eq!(s, 503);
    w.node.execute_batch(DAV_SETTINGS).unwrap();
    let (s, v, _) = call(
        &w,
        &mut owner,
        "PUT",
        "/api/profile/sharing",
        r#"{"scope":"lan"}"#,
        T0 + 2,
    );
    assert_eq!(
        (s, v["error"].as_str()),
        (
            400,
            Some("This sharing scope is not configured by the operator.")
        )
    );
    let (s, v, _) = call(
        &w,
        &mut owner,
        "PUT",
        "/api/profile/sharing",
        r#"{"scope":"off"}"#,
        T0 + 3,
    );
    assert_eq!((s, v["available"].as_bool()), (200, Some(false)));
    // Signed out: the gate's 401.
    let mut stranger = Browser::new("10.0.0.2");
    let (s, _, _) = call(&w, &mut stranger, "GET", "/api/profile/sharing", "", T0 + 4);
    assert_eq!(s, 401);
}

#[test]
fn sharing_with_an_invalid_environment_is_an_internal_error() {
    let mut dav = lan_cleartext();
    dav.scope = Some("everywhere".into());
    let w = world_with(false, dav);
    let mut owner = Browser::new("10.0.0.1");
    setup(&w, &mut owner);
    let (s, _, r) = call(&w, &mut owner, "GET", "/api/profile/sharing", "", T0);
    assert_eq!((s, r.body.as_str()), (500, r#"{"error":"Internal error"}"#));
    // An invalid scope is the caller's 400 even when the environment is broken.
    let (s, v, _) = call(
        &w,
        &mut owner,
        "PUT",
        "/api/profile/sharing",
        r#"{"scope":"nope"}"#,
        T0 + 1,
    );
    assert_eq!(
        (s, v["error"].as_str()),
        (400, Some("Choose a sharing scope."))
    );
}

/// A device (bearer) token never reaches account-security routes: Settings -> Sharing is
/// browser-only, so both verbs get the shared 403 and nothing is written (core#243 drops Node's
/// copy of this test).
#[test]
fn a_device_token_is_refused_on_sharing_and_writes_nothing() {
    let w = world_with(true, lan_cleartext());
    w.node.execute_batch(DAV_SETTINGS).unwrap();
    let mut owner = Browser::new("10.0.0.1");
    setup(&w, &mut owner);
    let mut app = Browser::new("10.0.0.20");
    let (_, v, _) = call(
        &w,
        &mut app,
        "POST",
        "/api/auth/device/code",
        r#"{"client_name":"Synthetic Mac"}"#,
        T0,
    );
    let device_code = v["device_code"].as_str().unwrap().to_string();
    let user_code = v["user_code"].as_str().unwrap().to_string();
    let (s, _, _) = call(
        &w,
        &mut owner,
        "POST",
        "/api/auth/device/approve",
        &format!(r#"{{"user_code":"{user_code}","approve":true}}"#),
        T0 + 4,
    );
    assert_eq!(s, 200);
    let poll = format!(
        r#"{{"grant_type":"urn:ietf:params:oauth:grant-type:device_code","device_code":"{device_code}"}}"#
    );
    let (s, v, _) = call(
        &w,
        &mut app,
        "POST",
        "/api/auth/device/token",
        &poll,
        T0 + 20_000,
    );
    assert_eq!(s, 200);
    let token = v["access_token"].as_str().unwrap().to_string();
    let audit = "SELECT count(*) FROM audit_events";
    let audits_before = count(&w, audit);
    for (method, body) in [("GET", ""), ("PUT", r#"{"scope":"off"}"#)] {
        let req = Request {
            method: method.into(),
            path: "/api/profile/sharing".into(),
            creds: Creds {
                cookie: None,
                authorization: Some(format!("Bearer {token}")),
                origin: None,
                csrf: None,
                method: method.into(),
            },
            user_agent: "synthetic-test".into(),
            content_type: "application/json".into(),
            client_ip: "10.0.0.20".into(),
            body: Body {
                bytes: body.as_bytes().to_vec(),
                over: false,
            },
        };
        let Outcome::Reply(r) = w.acct.handle(&req, T0 + 30_000) else {
            panic!("{method} passed to Node");
        };
        assert_eq!(
            (r.status, r.body.as_str()),
            (
                403,
                r#"{"error":"This needs a signed-in browser session.","code":"browser_session_required"}"#
            ),
            "{method}"
        );
    }
    assert_eq!(count(&w, "SELECT count(*) FROM dav_settings"), 0);
    assert_eq!(count(&w, audit), audits_before);
}

#[test]
fn framing_preferences_real_device_token_reads_writes_and_respects_disablement() {
    let w = world(true);
    let mut owner = Browser::new("10.0.0.1");
    let id = setup(&w, &mut owner);
    // Portable tests initialize the workspace; real Linux first-use/legacy behavior
    // is a separate mandatory native test.
    std::fs::create_dir_all(w.dir.path().join("users").join(&id)).unwrap();
    let mut app = Browser::new("10.0.0.20");
    let (_, code, _) = call(
        &w,
        &mut app,
        "POST",
        "/api/auth/device/code",
        r#"{"client_name":"Synthetic client"}"#,
        T0,
    );
    let device = code["device_code"].as_str().unwrap();
    let user = code["user_code"].as_str().unwrap();
    assert_eq!(
        call(
            &w,
            &mut owner,
            "POST",
            "/api/auth/device/approve",
            &format!(r#"{{"user_code":"{user}","approve":true}}"#),
            T0 + 4
        )
        .0,
        200
    );
    let (status, token, _) = call(
        &w,
        &mut app,
        "POST",
        "/api/auth/device/token",
        &format!(
            r#"{{"grant_type":"urn:ietf:params:oauth:grant-type:device_code","device_code":"{device}"}}"#
        ),
        T0 + 20_000,
    );
    assert_eq!(status, 200);
    let token = token["access_token"].as_str().unwrap();
    let request = |method: &str, body: &str| Request {
        method: method.into(),
        path: "/api/chat-framing/preferences".into(),
        creds: Creds {
            cookie: None,
            authorization: Some(format!("Bearer {token}")),
            origin: None,
            csrf: None,
            method: method.into(),
        },
        user_agent: "Synthetic client".into(),
        content_type: "application/json".into(),
        client_ip: "10.0.0.20".into(),
        body: Body {
            bytes: body.as_bytes().to_vec(),
            over: false,
        },
    };
    for (method, body) in [("GET", ""), ("PUT", r#"{"autoAccept":true}"#)] {
        let Outcome::Reply(r) = w.acct.handle(&request(method, body), T0 + 20_001) else {
            panic!("native route fell through")
        };
        assert_eq!(r.status, 200, "{}", r.body);
    }
    let before = std::fs::read(
        w.dir
            .path()
            .join("users")
            .join(&id)
            .join("chat-framing.json"),
    )
    .unwrap();
    w.node
        .execute(
            "UPDATE users SET disabled_at=?1 WHERE id=?2",
            (T0 + 20_002, &id),
        )
        .unwrap();
    let Outcome::Reply(r) = w
        .acct
        .handle(&request("PUT", r#"{"autoAccept":false}"#), T0 + 20_003)
    else {
        panic!("native route fell through")
    };
    assert_eq!(r.status, 401);
    assert_eq!(
        std::fs::read(
            w.dir
                .path()
                .join("users")
                .join(&id)
                .join("chat-framing.json")
        )
        .unwrap(),
        before
    );
}
