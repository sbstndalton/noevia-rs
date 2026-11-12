//! Coworkers and Spaces slice 1 through the real front (sbstndalton/noevia#2446, capability
//! `rust-specialists`): every route and method, the gate and CSRF, cross-account 404s, the limits
//! and their codes, the revision rule (409 with the current revision or version), path safety and
//! concurrent writers. The legacy upstream is dead, so a request that fell through to Node would
//! answer 502. Synthetic accounts and QA-named data only.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    dead_code
)]
use noevia_server::{
    config::Config,
    serve::{serve, Limits},
    App,
};
use serde_json::{json, Value};
use server_store::rusqlite::Connection;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const SCHEMA: &str = include_str!("../../../crates/server-store/tests/fixtures/node-schema.sql");
const A: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
const B: &str = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
const COOKIE_A: &str = "cowork_session=session-a; cowork_csrf=csrf";
const COOKIE_B: &str = "cowork_session=session-b; cowork_csrf=csrf";
const MISSING: &str = "cccccccc-cccc-4ccc-8ccc-cccccccccccc";

fn hex(s: &str) -> String {
    Sha256::digest(s.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn seed(dir: &std::path::Path) {
    let db = Connection::open(server_store::db_path(dir)).unwrap();
    db.execute_batch(SCHEMA).unwrap();
    let now = noevia_server::identity::now_ms();
    for (uid, name, token) in [(A, "ada", "session-a"), (B, "bob", "session-b")] {
        db.execute("INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,created_at,updated_at) VALUES(?1,?2,?2,'Synthetic','member','x',?1,1,1)", (uid, name)).unwrap();
        db.execute(
            "INSERT INTO sessions VALUES(?1,?2,?3,?4,?4,?5,'ua','127.0.0.1')",
            (hex(token), uid, hex("csrf"), now, now + 86_400_000),
        )
        .unwrap();
        std::fs::create_dir_all(dir.join("users").join(uid)).unwrap();
    }
}

struct Rig {
    dir: tempfile::TempDir,
    addr: std::net::SocketAddr,
}

async fn rig() -> Rig {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let env = HashMap::from([
        ("NOEVIA_LEGACY_UPSTREAM", "http://127.0.0.1:9".to_string()),
        ("UI_DATA_DIR", dir.path().display().to_string()),
        ("PUBLIC_ORIGIN", format!("http://{addr}")),
    ]);
    let app = App::new(Config::from_lookup(|k| env.get(k).cloned()).unwrap());
    tokio::spawn(serve(
        listener,
        app,
        Limits::default(),
        std::future::pending(),
    ));
    Rig { dir, addr }
}

struct Answer {
    status: u16,
    headers: HashMap<String, String>,
    json: Value,
}

#[derive(Clone, Copy, PartialEq)]
enum Csrf {
    Valid,
    /// A browser page on another site.
    ForeignOrigin,
    WrongToken,
    NoToken,
}

async fn send(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    cookie: &str,
    body: Option<&Value>,
    csrf: Csrf,
) -> Answer {
    let text = body.map(Value::to_string).unwrap_or_default();
    let guard = if method == "GET" {
        String::new()
    } else {
        match csrf {
            Csrf::Valid => format!("Origin: http://{addr}\r\nX-CSRF-Token: csrf\r\n"),
            Csrf::ForeignOrigin => {
                "Origin: http://evil.example\r\nX-CSRF-Token: csrf\r\n".to_owned()
            }
            Csrf::NoToken => format!("Origin: http://{addr}\r\n"),
            Csrf::WrongToken => format!("Origin: http://{addr}\r\nX-CSRF-Token: nope\r\n"),
        }
    };
    let cookie = if cookie.is_empty() {
        String::new()
    } else {
        format!("Cookie: {cookie}\r\n")
    };
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(
        format!(
            "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n{cookie}Content-Type: application/json\r\nContent-Length: {}\r\n{guard}\r\n{text}",
            text.len()
        )
        .as_bytes(),
    )
    .await
    .unwrap();
    let mut bytes = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(40), s.read_to_end(&mut bytes)).await;
    let raw = String::from_utf8(bytes).unwrap();
    let (h, b) = raw.split_once("\r\n\r\n").unwrap();
    let mut lines = h.lines();
    let status = lines
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    let headers = lines
        .filter_map(|l| l.split_once(": "))
        .map(|(k, v)| (k.to_lowercase(), v.to_owned()))
        .collect();
    Answer {
        status,
        headers,
        json: serde_json::from_str(b).unwrap_or(Value::Null),
    }
}

impl Rig {
    async fn go(&self, method: &str, path: &str, cookie: &str, body: Option<Value>) -> Answer {
        send(self.addr, method, path, cookie, body.as_ref(), Csrf::Valid).await
    }
    async fn a(&self, method: &str, path: &str, body: Option<Value>) -> Answer {
        self.go(method, path, COOKIE_A, body).await
    }
    async fn space(&self, name: &str) -> String {
        let r = self
            .a("POST", "/api/spaces", Some(json!({"name": name})))
            .await;
        assert_eq!(r.status, 201, "{}", r.json);
        r.json["space"]["id"].as_str().unwrap().to_owned()
    }
    fn user_dir(&self, user: &str) -> std::path::PathBuf {
        self.dir.path().join("users").join(user)
    }
}

fn code(a: &Answer) -> &str {
    a.json["code"].as_str().unwrap_or("")
}

const ALL: &[(&str, &[&str])] = &[
    ("/api/specialists", &["GET", "POST"]),
    ("/api/specialists/aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa", &["GET", "PATCH", "DELETE"]),
    ("/api/spaces", &["GET", "POST"]),
    ("/api/spaces/aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa", &["GET", "PATCH", "DELETE"]),
    ("/api/spaces/aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa/pages", &["GET", "POST"]),
    (
        "/api/spaces/aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa/pages/bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
        &["GET", "PUT", "DELETE"],
    ),
];

#[tokio::test]
async fn signed_out_is_401_on_every_route_and_method() {
    let r = rig().await;
    for (path, methods) in ALL {
        for m in *methods {
            let a = r.go(m, path, "", Some(json!({}))).await;
            assert_eq!(a.status, 401, "{m} {path}");
        }
    }
    assert!(!r.user_dir(A).join("specialists.json").exists());
}

#[tokio::test]
async fn writes_without_a_valid_csrf_token_are_refused_and_change_nothing() {
    let r = rig().await;
    for csrf in [Csrf::ForeignOrigin, Csrf::WrongToken, Csrf::NoToken] {
        for (m, path) in [
            ("POST", "/api/specialists"),
            ("POST", "/api/spaces"),
            ("PATCH", &format!("/api/spaces/{A}")),
            ("DELETE", &format!("/api/specialists/{A}")),
            ("POST", &format!("/api/spaces/{A}/pages")),
            ("PUT", &format!("/api/spaces/{A}/pages/{B}")),
            ("DELETE", &format!("/api/spaces/{A}/pages/{B}")),
        ] {
            let a = send(
                r.addr,
                m,
                path,
                COOKIE_A,
                Some(&json!({"name": "QA", "revision": 0})),
                csrf,
            )
            .await;
            assert_eq!(a.status, 403, "{m} {path}");
        }
    }
    for f in ["specialists.json", "spaces.json", "spaces"] {
        assert!(!r.user_dir(A).join(f).exists(), "{f}");
    }
}

#[tokio::test]
async fn unknown_methods_are_405_with_allow() {
    let r = rig().await;
    for (path, methods) in ALL {
        for m in ["PUT", "PATCH", "DELETE", "POST", "GET"] {
            if methods.contains(&m) {
                continue;
            }
            let a = r.a(m, path, Some(json!({}))).await;
            assert_eq!(a.status, 405, "{m} {path}");
            assert_eq!(code(&a), "method_not_allowed");
            let mut allow: Vec<&str> = a.headers["allow"].split(", ").collect();
            allow.sort_unstable();
            let mut want = methods.to_vec();
            want.sort_unstable();
            assert_eq!(allow, want, "{path}");
        }
    }
}

#[tokio::test]
async fn specialists_crud_with_revisions() {
    let r = rig().await;
    let a = r.a("GET", "/api/specialists", None).await;
    assert_eq!(
        (a.status, a.json.clone()),
        (200, json!({"revision": 0, "specialists": []}))
    );

    let space = r.space("QA Space").await;
    let a = r
        .a(
            "POST",
            "/api/specialists",
            Some(
                json!({"name": "QA Ada", "glyph": "A", "tint": "sky", "brief": "Be brief.",
                "spaces": [space], "defaultSpace": space}),
            ),
        )
        .await;
    assert_eq!(a.status, 201, "{}", a.json);
    let s = &a.json["specialist"];
    assert_eq!(a.json["revision"], 1);
    // Slice 4a: every coworker has its hidden project and conversation, and no toolboxes by default.
    let sid = s["id"].as_str().unwrap();
    assert_eq!(s["projectId"], format!("specialist-{sid}"));
    assert_eq!(s["chatId"], format!("specialist-chat-{sid}"));
    assert_eq!(s["toolboxes"], json!([]));
    assert_eq!(
        (s["name"].as_str(), s["glyph"].as_str(), s["tint"].as_str()),
        (Some("QA Ada"), Some("A"), Some("sky"))
    );
    assert_eq!(s["spaces"], json!([space]));
    let id = s["id"].as_str().unwrap().to_owned();
    assert!(r.user_dir(A).join("specialists.json").exists());

    let a = r.a("GET", &format!("/api/specialists/{id}"), None).await;
    assert_eq!((a.status, a.json["revision"].clone()), (200, json!(1)));

    // PATCH with the current revision works and bumps it; the old one is then stale.
    let a = r
        .a(
            "PATCH",
            &format!("/api/specialists/{id}"),
            Some(json!({"revision": 1, "name": "QA Bea", "defaultSpace": null})),
        )
        .await;
    assert_eq!(a.status, 200, "{}", a.json);
    assert_eq!(
        (
            a.json["revision"].clone(),
            a.json["specialist"]["name"].clone()
        ),
        (json!(2), json!("QA Bea"))
    );
    assert!(a.json["specialist"]["defaultSpace"].is_null());
    let a = r
        .a(
            "PATCH",
            &format!("/api/specialists/{id}"),
            Some(json!({"revision": 1, "name": "late"})),
        )
        .await;
    assert_eq!((a.status, code(&a)), (409, "stale_revision"));
    assert_eq!(a.json["revision"], 2);
    let a = r.a("GET", &format!("/api/specialists/{id}"), None).await;
    assert_eq!(a.json["specialist"]["name"], "QA Bea");

    // A body that names projectId, or has no revision, is refused.
    let a = r
        .a(
            "PATCH",
            &format!("/api/specialists/{id}"),
            Some(json!({"revision": 2, "projectId": "x"})),
        )
        .await;
    assert_eq!((a.status, code(&a)), (400, "invalid_request"));
    let a = r
        .a(
            "PATCH",
            &format!("/api/specialists/{id}"),
            Some(json!({"name": "x"})),
        )
        .await;
    assert_eq!(a.status, 400);

    // DELETE takes the revision (body or query) and is checked.
    let a = r
        .a(
            "DELETE",
            &format!("/api/specialists/{id}"),
            Some(json!({"revision": 1})),
        )
        .await;
    assert_eq!((a.status, code(&a)), (409, "stale_revision"));
    let a = r.a("DELETE", &format!("/api/specialists/{id}"), None).await;
    assert_eq!(a.status, 400);
    let a = r
        .a("DELETE", &format!("/api/specialists/{id}?revision=2"), None)
        .await;
    assert_eq!(
        (a.status, a.json),
        (200, json!({"ok": true, "revision": 3}))
    );
    let a = r.a("GET", &format!("/api/specialists/{id}"), None).await;
    assert_eq!((a.status, code(&a)), (404, "not_found"));
}

#[tokio::test]
async fn specialist_limits_and_validation() {
    let r = rig().await;
    for bad in [
        json!({}),
        json!({"name": ""}),
        json!({"name": "x".repeat(61)}),
        json!({"name": "QA", "brief": "b".repeat(8001)}),
        json!({"name": "QA", "spaces": [MISSING]}),
        json!({"name": "QA", "projectId": "p"}),
        json!({"name": "QA", "tint": "Not A Token"}),
    ] {
        let a = r.a("POST", "/api/specialists", Some(bad.clone())).await;
        assert_eq!((a.status, code(&a)), (400, "invalid_request"), "{bad}");
    }
    let a = r
        .a(
            "POST",
            "/api/specialists",
            Some(json!({"name": "QA", "brief": "b".repeat(8000)})),
        )
        .await;
    assert_eq!(a.status, 201);
    for i in 1..24 {
        let a = r
            .a(
                "POST",
                "/api/specialists",
                Some(json!({"name": format!("QA {i}")})),
            )
            .await;
        assert_eq!(a.status, 201, "{i}");
    }
    let a = r
        .a("POST", "/api/specialists", Some(json!({"name": "QA 25"})))
        .await;
    assert_eq!((a.status, code(&a)), (409, "specialist_limit"));
    // Not valid JSON at all.
    let mut s = TcpStream::connect(r.addr).await.unwrap();
    let body = "{nope";
    s.write_all(format!("POST /api/specialists HTTP/1.1\r\nHost: {0}\r\nConnection: close\r\nCookie: {COOKIE_A}\r\nOrigin: http://{0}\r\nX-CSRF-Token: csrf\r\nContent-Length: {1}\r\n\r\n{body}", r.addr, body.len()).as_bytes()).await.unwrap();
    let mut out = String::new();
    let _ = tokio::time::timeout(Duration::from_secs(20), s.read_to_string(&mut out)).await;
    assert!(out.starts_with("HTTP/1.1 400"), "{out}");
}

#[tokio::test]
async fn spaces_crud_limits_and_with_pages() {
    let r = rig().await;
    let a = r.a("GET", "/api/spaces", None).await;
    assert_eq!(a.json, json!({"revision": 0, "spaces": []}));
    for bad in [
        json!({}),
        json!({"name": " "}),
        json!({"name": "x".repeat(81)}),
    ] {
        let a = r.a("POST", "/api/spaces", Some(bad)).await;
        assert_eq!(a.status, 400);
    }
    let a = r
        .a("POST", "/api/spaces", Some(json!({"name": "QA One"})))
        .await;
    assert_eq!((a.status, a.json["revision"].clone()), (201, json!(1)));
    let id = a.json["space"]["id"].as_str().unwrap().to_owned();

    let a = r
        .a(
            "PATCH",
            &format!("/api/spaces/{id}"),
            Some(json!({"revision": 1, "name": "QA Two"})),
        )
        .await;
    assert_eq!(
        (
            a.status,
            a.json["space"]["name"].clone(),
            a.json["revision"].clone()
        ),
        (200, json!("QA Two"), json!(2))
    );
    let a = r
        .a(
            "PATCH",
            &format!("/api/spaces/{id}"),
            Some(json!({"revision": 1, "name": "late"})),
        )
        .await;
    assert_eq!(
        (a.status, code(&a), a.json["revision"].clone()),
        (409, "stale_revision", json!(2))
    );

    // A page makes the Space non-empty.
    let p = r
        .a(
            "POST",
            &format!("/api/spaces/{id}/pages"),
            Some(json!({"title": "QA p", "content": "x"})),
        )
        .await;
    assert_eq!(p.status, 201);
    let pid = p.json["page"]["id"].as_str().unwrap().to_owned();
    let file = r
        .user_dir(A)
        .join("spaces")
        .join(&id)
        .join(format!("{pid}.md"));
    assert!(file.exists());
    let a = r
        .a(
            "DELETE",
            &format!("/api/spaces/{id}"),
            Some(json!({"revision": 2})),
        )
        .await;
    assert_eq!((a.status, code(&a)), (409, "space_not_empty"));
    assert!(file.exists());
    let a = r
        .a(
            "DELETE",
            &format!("/api/spaces/{id}"),
            Some(json!({"revision": 1, "withPages": true})),
        )
        .await;
    assert_eq!((a.status, code(&a)), (409, "stale_revision"));
    assert!(file.exists());
    let a = r
        .a(
            "DELETE",
            &format!("/api/spaces/{id}"),
            Some(json!({"revision": 2, "withPages": true})),
        )
        .await;
    assert_eq!(
        (a.status, a.json),
        (200, json!({"ok": true, "revision": 3}))
    );
    assert!(!r.user_dir(A).join("spaces").join(&id).exists());
    assert_eq!(
        r.a("GET", &format!("/api/spaces/{id}"), None).await.status,
        404
    );

    // An empty Space deletes without withPages; the limit is 50.
    let e = r.space("QA empty").await;
    let a = r
        .a("DELETE", &format!("/api/spaces/{e}?revision=4"), None)
        .await;
    assert_eq!(a.status, 200, "{}", a.json);
    for i in 0..50 {
        r.space(&format!("QA {i}")).await;
    }
    let a = r
        .a("POST", "/api/spaces", Some(json!({"name": "QA 51"})))
        .await;
    assert_eq!((a.status, code(&a)), (409, "space_limit"));
}

#[tokio::test]
async fn deleting_a_space_removes_the_grant_from_specialists() {
    let r = rig().await;
    let s = r.space("QA").await;
    let w = r
        .a(
            "POST",
            "/api/specialists",
            Some(json!({"name": "QA", "spaces": [s], "defaultSpace": s})),
        )
        .await;
    let wid = w.json["specialist"]["id"].as_str().unwrap().to_owned();
    r.a("DELETE", &format!("/api/spaces/{s}?revision=1"), None)
        .await;
    let a = r.a("GET", &format!("/api/specialists/{wid}"), None).await;
    assert_eq!(a.json["specialist"]["spaces"], json!([]));
    assert!(a.json["specialist"]["defaultSpace"].is_null());
}

#[tokio::test]
async fn pages_create_get_list_search_delete() {
    let r = rig().await;
    let s = r.space("QA").await;
    let base = format!("/api/spaces/{s}/pages");
    let a = r.a("GET", &base, None).await;
    assert_eq!(a.json, json!({"pages": [], "truncated": false, "total": 0}));
    for (t, c) in [
        ("Alpha", "first body"),
        ("Beta", "the QUICK fox"),
        ("Gamma", "quick again"),
    ] {
        let a = r
            .a("POST", &base, Some(json!({"title": t, "content": c})))
            .await;
        assert_eq!(a.status, 201, "{}", a.json);
        let p = &a.json["page"];
        assert_eq!(p["savedBy"], "you");
        assert_eq!(p["bytes"], c.len());
        assert_eq!(p["version"].as_str().unwrap().len(), 64);
    }
    let a = r.a("GET", &base, None).await;
    assert_eq!(
        (
            a.json["pages"].as_array().unwrap().len(),
            a.json["total"].clone()
        ),
        (3, json!(3))
    );
    let a = r.a("GET", &format!("{base}?q=alpha"), None).await;
    assert_eq!(a.json["pages"].as_array().unwrap().len(), 1);
    let a = r.a("GET", &format!("{base}?q=QuIcK"), None).await;
    assert_eq!(a.json["pages"].as_array().unwrap().len(), 2);
    let a = r.a("GET", &format!("{base}?q=quick&limit=1"), None).await;
    assert_eq!(
        (
            a.json["pages"].as_array().unwrap().len(),
            a.json["truncated"].clone()
        ),
        (1, json!(true))
    );
    let a = r.a("GET", &format!("{base}?q=quick&limit=2"), None).await;
    assert_eq!(a.json["truncated"], false);
    for bad in [
        "limit=101",
        "limit=0",
        "limit=x",
        "offset=-1",
        &format!("q={}", "a".repeat(201)),
    ] {
        let a = r.a("GET", &format!("{base}?{bad}"), None).await;
        assert_eq!((a.status, code(&a)), (400, "invalid_request"), "{bad}");
    }

    let id = a_first(&r, &base).await;
    let a = r.a("GET", &format!("{base}/{id}"), None).await;
    assert_eq!(a.status, 200);
    assert!(a.json["content"].is_string() && a.json["page"]["id"] == id);
    let a = r.a("DELETE", &format!("{base}/{id}"), None).await;
    assert_eq!((a.status, a.json), (200, json!({"ok": true})));
    assert_eq!(r.a("GET", &format!("{base}/{id}"), None).await.status, 404);
    assert_eq!(
        r.a("DELETE", &format!("{base}/{id}"), None).await.status,
        404
    );
    assert!(!r
        .user_dir(A)
        .join("spaces")
        .join(&s)
        .join(format!("{id}.md"))
        .exists());
}

async fn a_first(r: &Rig, base: &str) -> String {
    let a = r.a("GET", base, None).await;
    a.json["pages"][0]["id"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn page_put_follows_the_revision_rule() {
    let r = rig().await;
    let s = r.space("QA").await;
    let page = "dddddddd-dddd-4ddd-8ddd-dddddddddddd";
    let url = format!("/api/spaces/{s}/pages/{page}");
    // Updating a page that does not exist is 404; version null creates it.
    let a = r
        .a("PUT", &url, Some(json!({"content": "x", "version": "abc"})))
        .await;
    assert_eq!((a.status, code(&a)), (404, "not_found"));
    let a = r
        .a(
            "PUT",
            &url,
            Some(json!({"title": "QA doc", "content": "one", "version": null})),
        )
        .await;
    assert_eq!((a.status, a.json["changed"].clone()), (201, json!(true)));
    let v1 = a.json["page"]["version"].as_str().unwrap().to_owned();
    let want: String = Sha256::digest(b"one")
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_eq!(v1, want);
    let updated = a.json["page"]["updatedAt"].clone();

    // Create again with null: 409 carrying the current version.
    let a = r
        .a(
            "PUT",
            &url,
            Some(json!({"title": "QA doc", "content": "two", "version": null})),
        )
        .await;
    assert_eq!(
        (a.status, code(&a), a.json["version"].clone()),
        (409, "stale_revision", json!(v1))
    );

    // Identical content is a no-op: same version, same updatedAt, nothing rewritten.
    let file = r
        .user_dir(A)
        .join("spaces")
        .join(&s)
        .join(format!("{page}.md"));
    let before = std::fs::metadata(&file).unwrap().modified().unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    let a = r
        .a("PUT", &url, Some(json!({"content": "one", "version": v1})))
        .await;
    assert_eq!(
        (
            a.status,
            a.json["changed"].clone(),
            a.json["page"]["version"].clone(),
            a.json["page"]["updatedAt"].clone()
        ),
        (200, json!(false), json!(v1), updated)
    );
    assert_eq!(
        std::fs::metadata(&file).unwrap().modified().unwrap(),
        before
    );

    // A change from the current version wins; a second write from the old one is stale.
    let a = r
        .a("PUT", &url, Some(json!({"content": "two", "version": v1})))
        .await;
    assert_eq!((a.status, a.json["changed"].clone()), (200, json!(true)));
    let v2 = a.json["page"]["version"].as_str().unwrap().to_owned();
    assert_ne!(v1, v2);
    let a = r
        .a(
            "PUT",
            &url,
            Some(json!({"content": "three", "version": v1})),
        )
        .await;
    assert_eq!(
        (a.status, code(&a), a.json["version"].clone()),
        (409, "stale_revision", json!(v2))
    );
    let a = r.a("GET", &url, None).await;
    assert_eq!(
        (
            a.json["content"].clone(),
            a.json["page"]["version"].clone(),
            a.json["page"]["title"].clone()
        ),
        (json!("two"), json!(v2), json!("QA doc"))
    );
    // Bodies with unknown fields are refused.
    let a = r
        .a(
            "PUT",
            &url,
            Some(json!({"content": "x", "version": v2, "savedBy": "you"})),
        )
        .await;
    assert_eq!(a.status, 400);
}

#[tokio::test]
async fn page_limits_and_codes() {
    let r = rig().await;
    let s = r.space("QA").await;
    let base = format!("/api/spaces/{s}/pages");
    let max = "a".repeat(512 * 1024);
    let a = r
        .a("POST", &base, Some(json!({"title": "max", "content": max})))
        .await;
    assert_eq!(
        (a.status, a.json["page"]["bytes"].clone()),
        (201, json!(512 * 1024))
    );
    let a = r
        .a(
            "POST",
            &base,
            Some(json!({"title": "over", "content": format!("{max}a")})),
        )
        .await;
    assert_eq!((a.status, code(&a)), (413, "page_too_large"));
    let a = r
        .a(
            "PUT",
            &format!("{base}/{MISSING}"),
            Some(json!({"title": "over", "content": format!("{max}a"), "version": null})),
        )
        .await;
    assert_eq!((a.status, code(&a)), (413, "page_too_large"));
    let a = r
        .a(
            "POST",
            &base,
            Some(json!({"title": "x".repeat(201), "content": "c"})),
        )
        .await;
    assert_eq!((a.status, code(&a)), (400, "invalid_request"));
    let a = r.a("POST", &base, Some(json!({"content": "c"}))).await;
    assert_eq!(a.status, 400);
    assert_eq!(r.a("GET", &base, None).await.json["total"], 1);

    // 2000 pages: fill the index directly (2000 requests would only test the loop), then one more.
    let rows: Vec<Value> = (0..2000)
        .map(|i| json!({"id": format!("00000000-0000-4000-8000-{i:012x}"), "title": format!("p{i}"), "version": "", "bytes": 0, "createdAt": 1, "updatedAt": 1, "savedBy": "you"}))
        .collect();
    let dir = r.user_dir(A).join("spaces").join(&s);
    std::fs::write(dir.join("index.json"), json!({"pages": rows}).to_string()).unwrap();
    let a = r
        .a(
            "POST",
            &base,
            Some(json!({"title": "one more", "content": "c"})),
        )
        .await;
    assert_eq!((a.status, code(&a)), (409, "page_limit"));
    let a = r.a("GET", &base, None).await;
    assert_eq!(
        (
            a.json["pages"].as_array().unwrap().len(),
            a.json["total"].clone(),
            a.json["truncated"].clone()
        ),
        (100, json!(2000), json!(true))
    );
}

#[tokio::test]
async fn another_accounts_ids_are_404_never_403() {
    let r = rig().await;
    let space = r.space("QA mine").await;
    let page = r
        .a(
            "POST",
            &format!("/api/spaces/{space}/pages"),
            Some(json!({"title": "QA", "content": "secret"})),
        )
        .await;
    let pid = page.json["page"]["id"].as_str().unwrap().to_owned();
    let who = r
        .a("POST", "/api/specialists", Some(json!({"name": "QA mine"})))
        .await;
    let wid = who.json["specialist"]["id"].as_str().unwrap().to_owned();
    for (m, path, body) in [
        ("GET", format!("/api/specialists/{wid}"), None),
        (
            "PATCH",
            format!("/api/specialists/{wid}"),
            Some(json!({"revision": 1, "name": "x"})),
        ),
        ("DELETE", format!("/api/specialists/{wid}?revision=1"), None),
        ("GET", format!("/api/spaces/{space}"), None),
        (
            "PATCH",
            format!("/api/spaces/{space}"),
            Some(json!({"revision": 1, "name": "x"})),
        ),
        (
            "DELETE",
            format!("/api/spaces/{space}?revision=1&withPages=true"),
            None,
        ),
        ("GET", format!("/api/spaces/{space}/pages"), None),
        (
            "POST",
            format!("/api/spaces/{space}/pages"),
            Some(json!({"title": "x", "content": "y"})),
        ),
        ("GET", format!("/api/spaces/{space}/pages/{pid}"), None),
        (
            "PUT",
            format!("/api/spaces/{space}/pages/{pid}"),
            Some(json!({"content": "y", "version": null})),
        ),
        ("DELETE", format!("/api/spaces/{space}/pages/{pid}"), None),
    ] {
        let a = r.go(m, &path, COOKIE_B, body).await;
        assert_eq!((a.status, code(&a)), (404, "not_found"), "{m} {path}");
    }
    // Their own lists are empty, and mine are untouched.
    for list in ["/api/specialists", "/api/spaces"] {
        let a = r.go("GET", list, COOKIE_B, None).await;
        assert_eq!(
            (a.status, a.json[list.rsplit('/').next().unwrap()].clone()),
            (200, json!([]))
        );
    }
    assert!(!r.user_dir(B).join("spaces").exists());
    let a = r
        .a("GET", &format!("/api/spaces/{space}/pages/{pid}"), None)
        .await;
    assert_eq!(a.json["content"], "secret");
}

#[tokio::test]
async fn a_specialist_cannot_be_granted_another_accounts_space() {
    let r = rig().await;
    let theirs = r
        .go(
            "POST",
            "/api/spaces",
            COOKIE_B,
            Some(json!({"name": "QA bob"})),
        )
        .await;
    let sid = theirs.json["space"]["id"].as_str().unwrap().to_owned();
    let a = r
        .a(
            "POST",
            "/api/specialists",
            Some(json!({"name": "QA", "spaces": [sid]})),
        )
        .await;
    assert_eq!((a.status, code(&a)), (400, "invalid_request"));
}

#[tokio::test]
async fn ids_that_are_not_uuids_are_404_without_touching_the_disk() {
    let r = rig().await;
    let uuid = A;
    for path in [
        "/api/specialists/not-a-uuid".to_owned(),
        "/api/specialists/..".to_owned(),
        "/api/spaces/..%2F..%2Fetc".to_owned(),
        "/api/spaces/%2e%2e".to_owned(),
        "/api/spaces/AAAAAAAA-AAAA-4AAA-8AAA-AAAAAAAAAAAA".to_owned(),
        format!("/api/spaces/{uuid}x"),
        "/api/spaces/x/pages".to_owned(),
        format!("/api/spaces/{uuid}/pages/..%2Findex"),
        format!("/api/spaces/{uuid}/pages/index.json"),
        format!("/api/spaces/{uuid}/pages/{uuid}.md"),
    ] {
        for m in ["GET", "DELETE"] {
            // The pages collection has no DELETE: its 405 comes before the id check.
            if m == "DELETE" && path.ends_with("/pages") {
                continue;
            }
            let a = r.a(m, &path, None).await;
            assert_eq!((a.status, code(&a)), (404, "not_found"), "{m} {path}");
        }
        let a = r
            .a("PUT", &path, Some(json!({"content": "x", "version": null})))
            .await;
        assert!(
            a.status == 404 || a.status == 405,
            "PUT {path}: {}",
            a.status
        );
    }
    // Nothing was made: no store files, no spaces directory.
    let names: Vec<_> = std::fs::read_dir(r.user_dir(A))
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert!(names.is_empty(), "{names:?}");
    // A trailing slash or an extra segment names nothing either.
    for path in [
        "/api/spaces/",
        "/api/specialists/",
        &format!("/api/spaces/{uuid}/pages/"),
    ] {
        let a = r.a("GET", path, None).await;
        assert_eq!(a.status, 404, "{path}");
    }
}

#[tokio::test]
async fn two_concurrent_puts_from_one_version_have_one_winner() {
    let r = rig().await;
    let s = r.space("QA").await;
    let base = format!("/api/spaces/{s}/pages");
    let made = r
        .a(
            "POST",
            &base,
            Some(json!({"title": "QA race", "content": "base"})),
        )
        .await;
    let id = made.json["page"]["id"].as_str().unwrap().to_owned();
    let v = made.json["page"]["version"].as_str().unwrap().to_owned();
    let url = format!("{base}/{id}");
    for round in 0..5 {
        let cur = r.a("GET", &url, None).await.json["page"]["version"]
            .as_str()
            .unwrap()
            .to_owned();
        let (x, y) = tokio::join!(
            r.a(
                "PUT",
                &url,
                Some(json!({"content": format!("left {round}"), "version": cur}))
            ),
            r.a(
                "PUT",
                &url,
                Some(json!({"content": format!("right {round}"), "version": cur}))
            ),
        );
        let mut got = [x.status, y.status];
        got.sort_unstable();
        assert_eq!(got, [200, 409], "round {round}");
        let lose = if x.status == 409 { &x } else { &y };
        let win = if x.status == 200 { &x } else { &y };
        assert_eq!(code(lose), "stale_revision");
        assert_eq!(lose.json["version"], win.json["page"]["version"]);
        let now = r.a("GET", &url, None).await;
        assert_eq!(now.json["page"]["version"], win.json["page"]["version"]);
    }
    assert_ne!(v, "");
    let index: Value = serde_json::from_str(
        &std::fs::read_to_string(r.user_dir(A).join("spaces").join(&s).join("index.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(index["pages"].as_array().unwrap().len(), 1);
    let leftovers: Vec<_> = std::fs::read_dir(r.user_dir(A).join("spaces").join(&s))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

#[tokio::test]
async fn two_concurrent_creates_of_one_id_have_one_winner() {
    let r = rig().await;
    let s = r.space("QA").await;
    let url = format!("/api/spaces/{s}/pages/{MISSING}");
    let body = json!({"title": "QA", "content": "same", "version": null});
    let (x, y) = tokio::join!(
        r.a("PUT", &url, Some(body.clone())),
        r.a("PUT", &url, Some(body))
    );
    let mut got = [x.status, y.status];
    got.sort_unstable();
    assert_eq!(got, [201, 409]);
}
