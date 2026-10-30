//! The Diary connectors (sbstndalton/noevia#2206): the credentials in `diary_connectors` and the file
//! bridge they open, against a synthetic sidecar and a synthetic tenant database. Synthetic data only.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
mod support;

use bytes::Bytes;
use http_body_util::Full;
use js_json::{wtf16, JValue};
use server_diary::connector::{self, Fault, Refusal, Verified};
use server_diary::{Diary, Reply, Request, Route};
use server_store::rusqlite::Connection;
use sha2::{Digest, Sha256};
use std::sync::atomic::Ordering;
use std::time::Duration;
use support::*;

fn s(t: &str) -> JValue {
    JValue::Str(t.to_owned())
}

fn db(w: &World) -> Connection {
    Connection::open(w.dir.path().join("cowork.db")).unwrap()
}

fn make(w: &World, user: &str, name: &str) -> connector::Created {
    connector::create(&w.writer(), user, &s(name), 1_000)
        .unwrap()
        .unwrap()
}

fn sha(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn who(w: &World, token: &str) -> Verified {
    connector::verify(&w.store, token).unwrap().unwrap()
}

fn json(text: &str) -> JValue {
    wtf16::parse(text).unwrap()
}

// ── the credentials ────────────────────────────────────────────────────────────

#[test]
fn a_connector_is_made_shown_once_and_stored_only_as_a_digest() {
    let w = World::new(&[]);
    let made = make(&w, MEMBER, "  Phone  ");
    assert!(connector::token_shaped(&made.token), "{}", made.id);
    assert!(made.token.starts_with(&format!("nv_diary_{}.", made.id)));
    assert!(connector::id_shaped(&made.id));
    let rows: Vec<(String, String, String, String, i64)> = {
        let c = db(&w);
        let mut st = c
            .prepare("SELECT id,user_id,name,token_hash,created_at FROM diary_connectors")
            .unwrap();
        st.query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })
        .unwrap()
        .map(Result::unwrap)
        .collect()
    };
    assert_eq!(
        rows,
        vec![(
            made.id.clone(),
            MEMBER.to_owned(),
            "Phone".to_owned(),
            sha(&made.token),
            1_000
        )]
    );
    // Neither the credential nor a piece of it is anywhere in the database, audit included.
    let c = db(&w);
    let tail = made.token.rsplit('.').next().unwrap().to_owned();
    for table in ["diary_connectors", "audit_events"] {
        let mut st = c.prepare(&format!("SELECT * FROM {table}")).unwrap();
        let cols = st.column_count();
        let mut rows = st.query([]).unwrap();
        while let Some(row) = rows.next().unwrap() {
            for i in 0..cols {
                let text = format!("{:?}", row.get_ref(i).unwrap());
                assert!(!text.contains(&tail), "{table}");
            }
        }
    }
    let audit: Vec<(String, String, String, String)> = {
        let mut st = c
            .prepare("SELECT actor_user_id,target_user_id,action,detail FROM audit_events")
            .unwrap();
        st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    };
    assert_eq!(
        audit,
        vec![(
            MEMBER.to_owned(),
            MEMBER.to_owned(),
            "diary-connector.create".to_owned(),
            format!(r#"{{"id":"{}","name":"Phone"}}"#, made.id)
        )]
    );
}

#[test]
fn the_list_is_the_accounts_own_in_creation_order() {
    let w = World::new(&[]);
    let a = make(&w, MEMBER, "First");
    make(&w, OTHER, "Not mine");
    let b = make(&w, MEMBER, "Second");
    let listed = connector::list(&w.store, MEMBER).unwrap();
    assert_eq!(
        listed.iter().map(|l| l.id.clone()).collect::<Vec<_>>(),
        [a.id, b.id]
    );
    assert_eq!(
        wtf16::stringify(&listed[0].json()).unwrap(),
        r#"{"id":"__ID__","name":"First","createdAt":1000}"#.replace("__ID__", &listed[0].id)
    );
    assert!(connector::list(&w.store, ADMIN).unwrap().is_empty());
}

#[test]
fn a_name_is_trimmed_and_at_most_80_characters_long_before_trimming() {
    let w = World::new(&[]);
    let writer = w.writer();
    let refused = |name: JValue| connector::create(&writer, MEMBER, &name, 1).unwrap().err();
    assert_eq!(refused(s("")), Some(Refusal::Name));
    assert_eq!(refused(s(" \t\n ")), Some(Refusal::Name));
    assert_eq!(refused(JValue::Null), Some(Refusal::Name));
    assert_eq!(refused(JValue::Num(5.0)), Some(Refusal::Name));
    assert_eq!(refused(JValue::Undefined), Some(Refusal::Name));
    assert_eq!(refused(s(&"a".repeat(81))), Some(Refusal::Name));
    // core measured `name.length` before `trim()`: 80 letters and a space is 81.
    assert_eq!(
        refused(s(&format!("{} ", "a".repeat(80)))),
        Some(Refusal::Name)
    );
    // UTF-16 units: forty emoji are eighty.
    assert!(connector::create(&writer, MEMBER, &s(&"😀".repeat(40)), 1)
        .unwrap()
        .is_ok());
    assert_eq!(refused(s(&"😀".repeat(41))), Some(Refusal::Name));
    assert!(connector::create(&writer, MEMBER, &s(&"a".repeat(80)), 1)
        .unwrap()
        .is_ok());
    // Each refusal says what to change, with a 4xx, where core threw a bare Error (a generic 500).
    assert_eq!(Refusal::Name.fault().status, 400);
    assert_eq!(Refusal::Unavailable.fault().status, 403);
    assert_eq!(Refusal::TooMany.fault().status, 409);
    assert_eq!(
        Refusal::Name.fault().reply().1,
        r#"{"error":"Connector name required (maximum 80 characters)"}"#
    );
}

#[test]
fn an_account_holds_ten_connectors_and_a_revoke_makes_room() {
    let w = World::new(&[]);
    let mut first = None;
    for n in 0..10 {
        let made = make(&w, MEMBER, &format!("c{n}"));
        first.get_or_insert(made.id);
    }
    let writer = w.writer();
    let over = connector::create(&writer, MEMBER, &s("eleventh"), 2).unwrap();
    assert_eq!(over.err(), Some(Refusal::TooMany));
    // Another account has its own ten.
    make(&w, OTHER, "mine");
    assert!(connector::revoke(&writer, MEMBER, &first.unwrap(), 3).unwrap());
    assert!(connector::create(&writer, MEMBER, &s("eleventh"), 4)
        .unwrap()
        .is_ok());
}

#[test]
fn only_an_enabled_account_with_the_addon_can_make_one() {
    let w = World::new(&[]);
    let writer = w.writer();
    db(&w)
        .execute(
            "UPDATE user_features SET diary_enabled=0 WHERE user_id=?1",
            [MEMBER],
        )
        .unwrap();
    assert_eq!(
        connector::create(&writer, MEMBER, &s("x"), 1)
            .unwrap()
            .err(),
        Some(Refusal::Unavailable)
    );
    db(&w)
        .execute("UPDATE users SET disabled_at=5 WHERE id=?1", [OTHER])
        .unwrap();
    assert_eq!(
        connector::create(&writer, OTHER, &s("x"), 1).unwrap().err(),
        Some(Refusal::Unavailable)
    );
    assert_eq!(
        connector::create(&writer, "nobody", &s("x"), 1)
            .unwrap()
            .err(),
        Some(Refusal::Unavailable)
    );
    let rows: i64 = db(&w)
        .query_row("SELECT count(*) FROM diary_connectors", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        rows, 0,
        "a refused create leaves nothing, not even an audit event"
    );
    let events: i64 = db(&w)
        .query_row("SELECT count(*) FROM audit_events", [], |r| r.get(0))
        .unwrap();
    assert_eq!(events, 0);
}

#[test]
fn a_failed_audit_leaves_no_connector_behind() {
    let w = World::new(&[]);
    db(&w)
        .execute_batch("CREATE TRIGGER no_audit BEFORE INSERT ON audit_events BEGIN SELECT RAISE(ABORT,'synthetic'); END;")
        .unwrap();
    let r = connector::create(&w.writer(), MEMBER, &s("x"), 1);
    assert!(r.is_err());
    let rows: i64 = db(&w)
        .query_row("SELECT count(*) FROM diary_connectors", [], |r| r.get(0))
        .unwrap();
    assert_eq!(rows, 0, "the row and its audit event are one transaction");
}

#[test]
fn a_token_is_verified_by_its_digest_while_the_account_is_enabled_and_has_the_addon() {
    let w = World::new(&[]);
    let made = make(&w, MEMBER, "Phone");
    let ok = who(&w, &made.token);
    assert_eq!(
        (ok.id.as_str(), ok.user_id.as_str()),
        (made.id.as_str(), MEMBER)
    );
    // A token of the right shape that is not stored, one with a changed character, a truncated one.
    let mut other = made.token.to_string();
    other.replace_range(
        other.len() - 1..,
        if other.ends_with('A') { "B" } else { "A" },
    );
    assert!(connector::verify(&w.store, &other).unwrap().is_none());
    assert!(
        connector::verify(&w.store, &made.token[..made.token.len() - 1])
            .unwrap()
            .is_none()
    );
    assert!(connector::verify(&w.store, "").unwrap().is_none());
    // The add-on switched off, or the account disabled, ends it; switching back restores it.
    db(&w)
        .execute(
            "UPDATE user_features SET diary_enabled=0 WHERE user_id=?1",
            [MEMBER],
        )
        .unwrap();
    assert!(connector::verify(&w.store, &made.token).unwrap().is_none());
    db(&w)
        .execute(
            "UPDATE user_features SET diary_enabled=1 WHERE user_id=?1",
            [MEMBER],
        )
        .unwrap();
    db(&w)
        .execute("UPDATE users SET disabled_at=7 WHERE id=?1", [MEMBER])
        .unwrap();
    assert!(connector::verify(&w.store, &made.token).unwrap().is_none());
    db(&w)
        .execute("UPDATE users SET disabled_at=NULL WHERE id=?1", [MEMBER])
        .unwrap();
    assert!(connector::verify(&w.store, &made.token).unwrap().is_some());
}

#[test]
fn a_revoke_removes_only_the_accounts_own_connector_and_is_audited_once() {
    let w = World::new(&[]);
    let mine = make(&w, MEMBER, "Phone");
    let theirs = make(&w, OTHER, "Laptop");
    let writer = w.writer();
    assert!(
        !connector::revoke(&writer, MEMBER, &theirs.id, 5).unwrap(),
        "not mine"
    );
    assert!(!connector::revoke(&writer, MEMBER, &"0".repeat(32), 5).unwrap());
    assert!(connector::revoke(&writer, MEMBER, &mine.id, 6).unwrap());
    assert!(
        !connector::revoke(&writer, MEMBER, &mine.id, 7).unwrap(),
        "gone"
    );
    assert!(connector::verify(&w.store, &mine.token).unwrap().is_none());
    assert!(connector::verify(&w.store, &theirs.token)
        .unwrap()
        .is_some());
    let events: Vec<(String, String)> = {
        let c = db(&w);
        let mut st = c
            .prepare("SELECT action,detail FROM audit_events ORDER BY id")
            .unwrap();
        st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    };
    assert_eq!(
        events,
        vec![
            (
                "diary-connector.create".to_owned(),
                format!(r#"{{"id":"{}","name":"Phone"}}"#, mine.id)
            ),
            (
                "diary-connector.create".to_owned(),
                format!(r#"{{"id":"{}","name":"Laptop"}}"#, theirs.id)
            ),
            (
                "diary-connector.revoke".to_owned(),
                format!(r#"{{"id":"{}"}}"#, mine.id)
            ),
        ]
    );
}

#[test]
fn an_accounts_connectors_go_with_the_account() {
    let w = World::new(&[]);
    let made = make(&w, OTHER, "Laptop");
    let c = db(&w);
    c.execute_batch("PRAGMA foreign_keys=ON").unwrap();
    c.execute("DELETE FROM users WHERE id=?1", [OTHER]).unwrap();
    assert!(connector::verify(&w.store, &made.token).unwrap().is_none());
    let rows: i64 = c
        .query_row("SELECT count(*) FROM diary_connectors", [], |r| r.get(0))
        .unwrap();
    assert_eq!(rows, 0);
}

// ── the file bridge ────────────────────────────────────────────────────────────

fn webdav() -> Conn {
    Conn {
        kind: "webdav",
        base_url: "https://cloud.example/dav",
        secret: SECRET.into(),
    }
}

async fn operate(d: &Diary, w: &World, token: &str, body: &str) -> (u16, JValue) {
    let who = connector::verify(&w.store, token).unwrap();
    let Some(who) = who else {
        panic!("not a connector");
    };
    match d
        .operate_connector(
            w.store.clone(),
            w.dir.path().to_owned(),
            token.to_owned(),
            who,
            json(body),
        )
        .await
    {
        Ok(v) => (
            200,
            json(&wtf16::stringify(&v).unwrap_or_else(|| "null".into())),
        ),
        Err(fault) => {
            let (status, text) = fault.reply();
            (status, json(&text))
        }
    }
}

#[tokio::test]
async fn list_read_and_write_speak_to_the_sidecar_as_the_connectors_account() {
    let stub = Stub::start(|seen, _| match (seen.method.as_str(), seen.path()) {
        ("GET", "/api/files") => Out::json(200, r#"{"files":[{"path":"Notes/a.md"}],"extra":1}"#),
        ("POST", "/api/file") => Out::json(200, r#"{"content":"hello \ud83d","version":"v"}"#),
        ("PUT", "/api/file") => Out::json(200, r#"{"written":true,"version":"w"}"#),
        _ => Out::json(404, "{}"),
    })
    .await;
    let w = World::new(&[(MEMBER, webdav())]);
    let d = diary(&stub, TENANT_KEY);
    let made = make(&w, MEMBER, "Phone");

    let (status, v) = operate(
        &d,
        &w,
        &made.token,
        r#"{"action":"list","path":"Notes & more"}"#,
    )
    .await;
    assert_eq!(status, 200);
    // Only the listing is returned, not what else the sidecar said.
    assert_eq!(
        wtf16::stringify(&v).unwrap(),
        r#"{"files":[{"path":"Notes/a.md"}]}"#
    );
    let seen = stub.requests()[0].clone();
    assert_eq!(seen.target, "/api/files?path=Notes%20%26%20more");
    assert_eq!(seen.header("x-cowork-user-id"), Some(MEMBER));
    assert_eq!(
        seen.header("authorization"),
        Some(format!("Bearer {BEARER}").as_str())
    );
    // Signed for this tenant, this method, this exact request.
    let assertion = seen.header("x-cowork-tenant-assertion").unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64();
    tenant_assertion::verify(&tenant_assertion::Request {
        key: TENANT_KEY,
        user_id: MEMBER,
        assertion,
        method: "GET",
        path: "/api/files",
        query: b"path=Notes%20%26%20more",
        body_hash: &sha(""),
        storage: seen.header("x-cowork-storage").unwrap(),
        legacy_owner: "",
        blocked: "",
        now,
    })
    .unwrap();

    // The root folder: an empty path.
    let (status, _) = operate(&d, &w, &made.token, r#"{"action":"list"}"#).await;
    assert_eq!(status, 200);
    assert_eq!(stub.requests()[1].target, "/api/files?path=");

    let (status, v) = operate(
        &d,
        &w,
        &made.token,
        r#"{"action":"read","path":"Notes/a.md"}"#,
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(
        wtf16::stringify(&v).unwrap(),
        r#"{"content":"hello \ud83d","version":"v"}"#
    );
    let read = stub.requests()[2].clone();
    assert_eq!(read.body, br#"{"path":"Notes/a.md"}"#);

    let version = "a".repeat(64);
    let body = format!(
        r#"{{"action":"write","path":"Notes/a.md","content":"new é","version":"{version}"}}"#
    );
    let (status, v) = operate(&d, &w, &made.token, &body).await;
    assert_eq!(status, 200);
    assert_eq!(
        wtf16::stringify(&v).unwrap(),
        r#"{"written":true,"version":"w"}"#
    );
    let put = stub.requests()[3].clone();
    assert_eq!(put.method, "PUT");
    assert_eq!(
        put.body,
        format!(r#"{{"path":"Notes/a.md","content":"new é","version":"{version}"}}"#).as_bytes()
    );
    // A first write has no version yet.
    let (status, _) = operate(
        &d,
        &w,
        &made.token,
        r#"{"action":"write","path":"Notes/b.md","content":"x","version":null}"#,
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(
        stub.requests()[4].body,
        br#"{"path":"Notes/b.md","content":"x","version":null}"#
    );
}

#[tokio::test]
async fn bad_requests_are_refused_in_cores_order_before_the_sidecar_is_called() {
    let stub = Stub::start(|_, _| Out::json(200, "{}")).await;
    let w = World::new(&[]);
    let d = diary(&stub, TENANT_KEY);
    let made = make(&w, MEMBER, "Phone");
    let version = "b".repeat(64);
    let big = "x".repeat(512 * 1024 + 1);
    let at_cap = "x".repeat(512 * 1024);
    for (body, status, error) in [
        (
            r#"{"action":"read"}"#.to_owned(),
            400,
            "Choose a Markdown path inside the Diary",
        ),
        (
            r#"{"action":"read","path":"a.txt"}"#.into(),
            400,
            "Choose a Markdown path inside the Diary",
        ),
        (
            r#"{"action":"read","path":".hidden/a.md"}"#.into(),
            400,
            "Choose a Markdown path inside the Diary",
        ),
        (
            r#"{"action":"list","path":"a/../b"}"#.into(),
            400,
            "Choose a Markdown path inside the Diary",
        ),
        (
            r#"{"action":"list","path":5}"#.into(),
            400,
            "Choose a Markdown path inside the Diary",
        ),
        (
            r#"{"action":"nope","path":"a.md"}"#.into(),
            400,
            "Unknown Diary action",
        ),
        (r#"{"path":"a.md"}"#.into(), 400, "Unknown Diary action"),
        // The path is checked before the action is known.
        (
            r#"{"action":"nope","path":"a.txt"}"#.into(),
            400,
            "Choose a Markdown path inside the Diary",
        ),
        (
            format!(r#"{{"action":"write","path":"a.md","content":"{big}","version":null}}"#),
            400,
            "Content (maximum 512 KiB) and the exact read version are required",
        ),
        (
            r#"{"action":"write","path":"a.md","content":5,"version":null}"#.into(),
            400,
            "Content (maximum 512 KiB) and the exact read version are required",
        ),
        (
            r#"{"action":"write","path":"a.md","content":"x"}"#.into(),
            400,
            "Content (maximum 512 KiB) and the exact read version are required",
        ),
        (
            r#"{"action":"write","path":"a.md","content":"x","version":"short"}"#.into(),
            400,
            "Content (maximum 512 KiB) and the exact read version are required",
        ),
        (
            format!(
                r#"{{"action":"write","path":"a.md","content":"x","version":"{}"}}"#,
                version.to_uppercase()
            ),
            400,
            "Content (maximum 512 KiB) and the exact read version are required",
        ),
        (
            r#"{"action":"write","path":"a.md","content":"x","version":5}"#.into(),
            400,
            "Content (maximum 512 KiB) and the exact read version are required",
        ),
    ] {
        let (got, v) = operate(&d, &w, &made.token, &body).await;
        assert_eq!(
            (got, v.get("error").as_str()),
            (status, Some(error)),
            "{}",
            &body[..body.len().min(80)]
        );
    }
    assert!(
        stub.requests().iter().all(|r| r.path() == "/api/files"),
        "only the valid list calls went out"
    );
    // A 512 KiB body is the largest accepted; the count is in UTF-8 bytes, not characters.
    let ok = format!(r#"{{"action":"write","path":"a.md","content":"{at_cap}","version":null}}"#);
    assert_eq!(operate(&d, &w, &made.token, &ok).await.0, 200);
    let multibyte = "é".repeat(256 * 1024 + 1);
    let over =
        format!(r#"{{"action":"write","path":"a.md","content":"{multibyte}","version":null}}"#);
    assert_eq!(operate(&d, &w, &made.token, &over).await.0, 400);
}

#[tokio::test]
async fn a_revoked_or_disabled_connector_is_refused_before_anything_is_sent() {
    let stub = Stub::start(|_, _| Out::json(200, "{}")).await;
    let w = World::new(&[]);
    let d = diary(&stub, TENANT_KEY);
    let made = make(&w, MEMBER, "Phone");
    let user = who(&w, &made.token);
    assert!(connector::revoke(&w.writer(), MEMBER, &made.id, 9).unwrap());
    let res = d
        .operate_connector(
            w.store.clone(),
            w.dir.path().to_owned(),
            made.token.to_string(),
            user,
            json(r#"{"action":"read","path":"a.md"}"#),
        )
        .await;
    let fault: Fault = res.unwrap_err();
    assert_eq!(
        (fault.status, fault.message.as_str()),
        (403, "Connector was revoked or Diary disabled")
    );
    assert!(stub.requests().is_empty());
}

#[tokio::test]
async fn the_sidecars_answers_map_as_core_mapped_them() {
    let w = World::new(&[(MEMBER, webdav())]);
    let made = make(&w, MEMBER, "Phone");
    for (reply, want) in [
        (
            Out::json(404, r#"{"detail":"No such file"}"#),
            (404, Some("No such file")),
        ),
        (
            Out::json(409, r#"{"detail":"The file changed; read it again"}"#),
            (409, Some("The file changed; read it again")),
        ),
        (
            Out::json(424, r#"{"detail":{"code":1}}"#),
            (424, Some("[object Object]")),
        ),
        (
            Out::json(400, r#"{"detail":["a","b"]}"#),
            (400, Some("a,b")),
        ),
        (
            Out::json(409, "{}"),
            (
                409,
                Some("Diary request interrupted; read the current version before retrying a write"),
            ),
        ),
        (
            Out::text(404, "gateway"),
            (
                404,
                Some("Diary request interrupted; read the current version before retrying a write"),
            ),
        ),
        // A server error keeps its status and loses its detail.
        (
            Out::json(
                503,
                r#"{"detail":"storage login refused at https://cloud.example/dav"}"#,
            ),
            (503, Some("Internal error")),
        ),
        (Out::json(502, "{}"), (502, Some("Internal error"))),
        // Not an error status at all: a generic 500.
        (Out::json(302, "{}"), (500, Some("Internal error"))),
    ] {
        let reply = std::sync::Mutex::new(Some(reply));
        let stub = Stub::start(move |_, _| reply.lock().unwrap().take().unwrap()).await;
        let d = diary(&stub, TENANT_KEY);
        let (status, v) = operate(&d, &w, &made.token, r#"{"action":"read","path":"a.md"}"#).await;
        assert_eq!((status, v.get("error").as_str()), want);
    }
    // The sidecar gone is a generic 500.
    let stub = Stub::start(|_, _| Out::Hangup).await;
    let d = diary(&stub, TENANT_KEY);
    let (status, v) = operate(&d, &w, &made.token, r#"{"action":"read","path":"a.md"}"#).await;
    assert_eq!(
        (status, v.get("error").as_str()),
        (500, Some("Internal error"))
    );
}

#[tokio::test]
async fn a_428_repeats_the_call_once_with_the_secret_and_a_member_origin_is_blocked() {
    let stub = Stub::start(|seen, n| {
        let has_secret = !matches!(seen.storage().get("secret"), JValue::Undefined);
        if n == 0 {
            assert!(!has_secret);
            Out::json(428, "{}")
        } else {
            assert!(has_secret);
            Out::json(200, r#"{"files":[]}"#)
        }
    })
    .await;
    let w = World::new(&[(MEMBER, webdav())]);
    let d = diary(&stub, TENANT_KEY);
    let made = make(&w, MEMBER, "Phone");
    let (status, _) = operate(&d, &w, &made.token, r#"{"action":"list"}"#).await;
    assert_eq!(status, 200);
    assert_eq!(stub.requests().len(), 2);

    // A member whose storage is on an origin the deployment does not list speaks with a blocked
    // descriptor; the credential never leaves.
    let stub = Stub::start(|_, _| Out::json(200, r#"{"files":[]}"#)).await;
    let w = World::new(&[(
        MEMBER,
        Conn {
            kind: "webdav",
            base_url: "https://elsewhere.example/dav",
            secret: SECRET.into(),
        },
    )]);
    let d = diary(&stub, TENANT_KEY);
    let made = make(&w, MEMBER, "Phone");
    assert_eq!(
        operate(&d, &w, &made.token, r#"{"action":"list"}"#).await.0,
        200
    );
    let seen = &stub.requests()[0];
    assert_eq!(seen.header("x-cowork-storage-blocked"), Some("1"));
    assert!(!seen.header("x-cowork-storage").unwrap().is_empty());
    assert!(!format!("{:?}", seen.headers).contains(SECRET));
}

#[tokio::test]
async fn the_call_runs_to_its_end_when_the_client_goes_away() {
    let stub =
        Stub::start(|_, _| Out::json(200, r#"{"written":true}"#).slow(Duration::from_millis(400)))
            .await;
    let w = World::new(&[]);
    let d = diary(&stub, TENANT_KEY);
    let made = make(&w, MEMBER, "Phone");
    let user = who(&w, &made.token);
    let call = d.operate_connector(
        w.store.clone(),
        w.dir.path().to_owned(),
        made.token.to_string(),
        user,
        json(r#"{"action":"write","path":"a.md","content":"x","version":null}"#),
    );
    // The client leaves while the sidecar is still working.
    assert!(tokio::time::timeout(Duration::from_millis(100), call)
        .await
        .is_err());
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(stub.requests().len(), 1, "the write reached the sidecar");
    assert_eq!(
        stub.cancelled.load(Ordering::SeqCst),
        0,
        "and was answered, not abandoned half-sent"
    );
}

#[tokio::test]
async fn a_storage_import_also_runs_to_its_end_when_the_client_goes_away() {
    let stub =
        Stub::start(|_, _| Out::json(200, r#"{"imported":true}"#).slow(Duration::from_millis(400)))
            .await;
    let w = World::new(&[]);
    let d = diary(&stub, TENANT_KEY);
    let serve: std::pin::Pin<Box<dyn std::future::Future<Output = Reply> + Send>> = {
        let d = d.clone();
        let ctx = w.ctx(MEMBER, true);
        Box::pin(async move {
            d.serve(
                Route::StorageImport,
                ctx,
                Request {
                    method: "POST".into(),
                    target: "/api/diary/storage-import".into(),
                    content_length: None,
                },
                Full::new(Bytes::from_static(br#"{"fingerprint":"f"}"#)),
            )
            .await
        })
    };
    assert!(tokio::time::timeout(Duration::from_millis(100), serve)
        .await
        .is_err());
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(stub.requests().len(), 1);
    assert_eq!(stub.cancelled.load(Ordering::SeqCst), 0);
}
