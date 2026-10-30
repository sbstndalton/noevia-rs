//! `GET /api/workspace` through the real front (sbstndalton/noevia#2191 close-out, capability
//! `rust-workspace`): the projects and free chats as core's route served them, the account's load view
//! (modes, the retired provider id, damaged file entries), the delete-old-chats sweep and the orphan
//! chat-context cleanup the list runs, and the text-missing flag of a source (#1815/#1818). The legacy
//! upstream is dead, so a request that fell through to Node would answer 502. Synthetic data only.
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
    for (uid, name, token, diary) in [
        (A, "ada", "session-a", true),
        (B, "bob", "session-b", false),
    ] {
        db.execute("INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,created_at,updated_at) VALUES(?1,?2,?2,'Synthetic','member','x',?1,1,1)", (uid, name)).unwrap();
        if diary {
            db.execute(
                "INSERT INTO user_features(user_id,diary_enabled,updated_at) VALUES(?1,1,1)",
                [uid],
            )
            .unwrap();
        }
        db.execute(
            "INSERT INTO sessions VALUES(?1,?2,?3,?4,?4,?5,'ua','127.0.0.1')",
            (hex(token), uid, hex("csrf"), now, now + 86_400_000),
        )
        .unwrap();
        std::fs::create_dir_all(dir.join("users").join(uid)).unwrap();
    }
}

async fn front(dir: &std::path::Path) -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let env = HashMap::from([
        ("NOEVIA_LEGACY_UPSTREAM", "http://127.0.0.1:9".to_string()),
        ("UI_DATA_DIR", dir.display().to_string()),
        ("PUBLIC_ORIGIN", format!("http://{addr}")),
    ]);
    let app = App::new(Config::from_lookup(|k| env.get(k).cloned()).unwrap());
    tokio::spawn(serve(
        listener,
        app,
        Limits::default(),
        std::future::pending(),
    ));
    addr
}

struct Answer {
    status: u16,
    headers: HashMap<String, String>,
    text: String,
}

async fn call(addr: std::net::SocketAddr, method: &str, path: &str, cookie: &str) -> Answer {
    let csrf = if method == "GET" {
        String::new()
    } else {
        format!("Origin: http://{addr}\r\nX-CSRF-Token: csrf\r\n")
    };
    let cookie = if cookie.is_empty() {
        String::new()
    } else {
        format!("Cookie: {cookie}\r\n")
    };
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(
        format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n{cookie}Content-Length: 0\r\n{csrf}\r\n")
            .as_bytes(),
    )
    .await
    .unwrap();
    let mut bytes = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(40), s.read_to_end(&mut bytes)).await;
    let text = String::from_utf8(bytes).unwrap();
    let (h, b) = text.split_once("\r\n\r\n").unwrap();
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
        text: b.to_owned(),
    }
}

const REF1: &str = "1111111111111111111111111111111111111111111111111111111111111111";
const REF2: &str = "2222222222222222222222222222222222222222222222222222222222222222";
const CTX: &str = "cowork-chat-context-";
const DAY: i64 = 86_400_000;

fn user(dir: &std::path::Path, id: &str) -> std::path::PathBuf {
    dir.join("users").join(id)
}

fn put(dir: &std::path::Path, id: &str, name: &str, text: &str) {
    std::fs::write(user(dir, id).join(name), text).unwrap();
}

fn data() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path());
    dir
}

async fn list(addr: std::net::SocketAddr, cookie: &str) -> serde_json::Value {
    let r = call(addr, "GET", "/api/workspace", cookie).await;
    assert_eq!(r.status, 200, "{}", r.text);
    serde_json::from_str(&r.text).unwrap()
}

fn ids(v: &serde_json::Value) -> Vec<String> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|p| p["id"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn the_gate_is_nodes_and_the_answer_is_the_lists_core_served() {
    let dir = data();
    put(
        dir.path(),
        A,
        "projects.json",
        &format!(
            r#"{{"projects":[
              {{"id":"p1","name":"Plan","chats":[{{"id":"c1","title":"T","updatedAt":5,"junk":"x"}},null,{{"id":"c2"}}],
                "files":[{{"name":"inline.md","content":"hello"}},{{"name":"ext.md","textRef":"{REF1}","chars":9,"content":"stale"}}]}},
              {{"id":"cowork-diary-extras","name":"internal","chats":[]}},
              {{"id":"{CTX}c1","name":"context","chats":[]}}
            ]}}"#
        ),
    );
    put(
        dir.path(),
        A,
        "free-chats.json",
        r#"[{"id":"f1","title":"Free","updatedAt":3}]"#,
    );
    let addr = front(dir.path()).await;

    let r = call(addr, "GET", "/api/workspace", "").await;
    assert_eq!(
        (r.status, r.text.as_str()),
        (401, r#"{"error":"unauthorized"}"#)
    );
    let r = call(addr, "GET", "/api/workspace", COOKIE_A).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.headers["content-type"], "application/json");
    assert_eq!(r.headers["cache-control"], "no-store");
    assert_eq!(r.headers["x-noevia-api"], "1");
    let v: serde_json::Value = serde_json::from_str(&r.text).unwrap();
    // The internal projects stay out; the other account's workspace is its own (empty).
    assert_eq!(ids(&v["projects"]), ["p1"]);
    assert_eq!(
        list(addr, COOKIE_B).await,
        serde_json::json!({"projects":[],"freeChats":[]})
    );
    // A source is metadata only: an inline text becomes `chars`, an external one keeps its pointer, never its text.
    let files = v["projects"][0]["files"].as_array().unwrap();
    assert_eq!(files[0], serde_json::json!({"name":"inline.md","chars":5}));
    assert_eq!(
        files[1],
        serde_json::json!({"name":"ext.md","textRef":REF1,"chars":9})
    );
    assert!(!r.text.contains("hello") && !r.text.contains("stale"));
    // The chat metas are sanitized as core's `sanitizeChats` does: an entry that is no object with a string id is dropped, a meta is kept whole.
    let chats = v["projects"][0]["chats"].as_array().unwrap();
    assert_eq!(
        chats
            .iter()
            .map(|c| c["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["c1", "c2"]
    );
    assert_eq!(chats[0]["junk"], "x");
    assert_eq!(v["freeChats"][0]["id"], "f1");
    // The top-level keys are core's, in core's order.
    assert!(r.text.starts_with(r#"{"projects":["#) && r.text.contains(r#""freeChats":["#));
}

#[tokio::test]
async fn the_list_is_the_view_core_loaded_not_the_raw_file() {
    std::env::set_var("DEFAULT_PROVIDER_ID", "synthetic-default");
    let dir = data();
    put(
        dir.path(),
        A,
        "projects.json",
        r#"{"projects":[
          {"id":"a","provider":"lemonade","files":[null,3,{"name":"kept.md","content":"x"},[]],"modes":["cowork"]},
          {"id":"b","modes":["code","cowork","chat"]},
          {"id":"c","modes":["chat","cowork"],"provider":"other"},
          {"id":"d","modes":"chat"},
          {"id":"e","files":"not a list"}
        ]}"#,
    );
    let addr = front(dir.path()).await;
    let v = list(addr, COOKIE_A).await;
    let p = |i: usize| &v["projects"][i];
    // #1576: Chat first, the other recorded values kept in core's order; an unchanged list is left as it was.
    assert_eq!(p(0)["modes"], serde_json::json!(["chat", "cowork"]));
    assert_eq!(p(1)["modes"], serde_json::json!(["chat", "cowork", "code"]));
    assert_eq!(p(2)["modes"], serde_json::json!(["chat", "cowork"]));
    assert_eq!(p(3)["modes"], serde_json::json!(["chat"]));
    assert_eq!(p(4)["modes"], serde_json::json!(["chat"]));
    // The retired `lemonade` provider id reads as the default's; another stays.
    assert_eq!(p(0)["provider"], "synthetic-default");
    assert_eq!(p(2)["provider"], "other");
    // #1236: a null or primitive entry of `files` is dropped before anything sees it; an array entry is an object in JS.
    assert_eq!(
        p(0)["files"],
        serde_json::json!([{"name":"kept.md","chars":1}, {}])
    );
    assert_eq!(p(4)["files"], "not a list");
    // The file on disk is not rewritten by a read.
    assert!(
        std::fs::read_to_string(user(dir.path(), A).join("projects.json"))
            .unwrap()
            .contains("\"lemonade\"")
    );
}

#[tokio::test]
async fn the_hourly_sweep_runs_with_the_list_and_never_fails_it() {
    let dir = data();
    let now = noevia_server::identity::now_ms();
    put(dir.path(), A, "projects.json", r#"{"projects":[]}"#);
    put(
        dir.path(),
        A,
        "free-chats.json",
        &format!(
            r#"[{{"id":"stale","updatedAt":{old}}},{{"id":"pinned","updatedAt":{old},"pinned":true}},{{"id":"recent","updatedAt":{now}}}]"#,
            old = now - 100 * DAY
        ),
    );
    for id in ["stale", "pinned", "recent"] {
        put(
            dir.path(),
            A,
            &format!("history-{id}.json"),
            r#"{"history":[{"role":"user","content":"synthetic"}]}"#,
        );
    }
    put(dir.path(), A, "chat-retention.json", r#"{"days":90}"#);
    let addr = front(dir.path()).await;
    let v = list(addr, COOKIE_A).await;
    // The sweep deleted the expired chat before the list was built; a pinned and a recent chat stay.
    assert_eq!(ids(&v["freeChats"]), ["pinned", "recent"]);
    assert!(!user(dir.path(), A).join("history-stale.json").exists());
    assert!(
        std::fs::read_to_string(user(dir.path(), A).join("deleted-chats.json"))
            .unwrap()
            .contains("stale")
    );
    let marked = std::fs::read_to_string(user(dir.path(), A).join("chat-retention.json")).unwrap();
    assert!(marked.contains("lastSweep"), "{marked}");
    // At most once an hour: a chat that expires now is kept by the next list.
    put(
        dir.path(),
        A,
        "free-chats.json",
        &format!(r#"[{{"id":"late","updatedAt":{}}}]"#, now - 200 * DAY),
    );
    assert_eq!(ids(&list(addr, COOKIE_A).await["freeChats"]), ["late"]);
    // The other account has no retention setting: nothing of it is swept.
    assert_eq!(
        list(addr, COOKIE_B).await["freeChats"],
        serde_json::json!([])
    );
    // A sweep that cannot run (a damaged setting) is logged, never the list's failure (core caught it).
    put(dir.path(), A, "chat-retention.json", "{damaged");
    let r = call(addr, "GET", "/api/workspace", COOKIE_A).await;
    assert_eq!(r.status, 200, "{}", r.text);
}

#[tokio::test]
async fn an_attachments_project_goes_only_when_its_chats_deletion_is_proven() {
    let dir = data();
    let tomb = |id: &str| format!("\"{id}\"");
    // Three left behind by deletes before #788 (tombstoned, in no list); one opened in a tab (in no list, not tombstoned);
    // one listed and tombstoned (the list wins); a regular project; one for a chat that sits in a project's list.
    let mut projects = vec![
        format!(r#"{{"id":"{CTX}gone-1"}}"#),
        format!(r#"{{"id":"{CTX}gone-2"}}"#),
        format!(r#"{{"id":"{CTX}gone-3"}}"#),
        format!(r#"{{"id":"{CTX}fresh"}}"#),
        format!(r#"{{"id":"{CTX}listed"}}"#),
        format!(r#"{{"id":"{CTX}inproj"}}"#),
        format!(r#"{{"id":"{CTX}bad id"}}"#),
        r#"{"id":"regular","name":"R","chats":[{"id":"inproj"}]}"#.to_owned(),
    ];
    put(
        dir.path(),
        A,
        "projects.json",
        &format!(r#"{{"projects":[{}]}}"#, projects.join(",")),
    );
    put(dir.path(), A, "free-chats.json", r#"[{"id":"listed"}]"#);
    put(
        dir.path(),
        A,
        "deleted-chats.json",
        &format!(
            "[{}]",
            ["gone-1", "gone-2", "gone-3", "listed", "inproj", "bad id"]
                .map(tomb)
                .join(",")
        ),
    );
    // Bob's orphan with the same id is his workspace's business.
    put(
        dir.path(),
        B,
        "projects.json",
        &format!(r#"{{"projects":[{{"id":"{CTX}gone-1"}}]}}"#),
    );
    put(dir.path(), B, "deleted-chats.json", r#"["gone-1"]"#);
    let addr = front(dir.path()).await;

    let _ = list(addr, COOKIE_A).await;
    let left = |who: &str| -> Vec<String> {
        let v: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(user(dir.path(), who).join("projects.json")).unwrap(),
        )
        .unwrap();
        ids(&v["projects"])
    };
    assert_eq!(
        left(A),
        [
            format!("{CTX}fresh"),
            format!("{CTX}listed"),
            format!("{CTX}inproj"),
            format!("{CTX}bad id"),
            "regular".to_owned()
        ]
    );
    assert_eq!(
        left(B),
        [format!("{CTX}gone-1")],
        "another account's context is not this sweep's"
    );
    // Idempotent: nothing more goes on the next list.
    let _ = list(addr, COOKIE_A).await;
    assert_eq!(left(A).len(), 5);
    projects.clear();
}

#[tokio::test]
async fn the_cleanup_takes_ten_contexts_per_list_and_drains_a_backlog_over_several() {
    let dir = data();
    let n = 13;
    let contexts: Vec<String> = (0..n)
        .map(|i| format!(r#"{{"id":"{CTX}backlog-{i}"}}"#))
        .collect();
    put(
        dir.path(),
        A,
        "projects.json",
        &format!(r#"{{"projects":[{}]}}"#, contexts.join(",")),
    );
    let dead: Vec<String> = (0..n).map(|i| format!("\"backlog-{i}\"")).collect();
    put(
        dir.path(),
        A,
        "deleted-chats.json",
        &format!("[{}]", dead.join(",")),
    );
    let addr = front(dir.path()).await;
    let count = || -> usize {
        let v: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(user(dir.path(), A).join("projects.json")).unwrap(),
        )
        .unwrap();
        v["projects"].as_array().unwrap().len()
    };
    let _ = list(addr, COOKIE_A).await;
    assert_eq!(count(), 3, "ten went");
    let _ = list(addr, COOKIE_A).await;
    assert_eq!(count(), 0);
    let _ = list(addr, COOKIE_A).await;
    assert_eq!(count(), 0);
}

#[tokio::test]
async fn a_source_whose_text_is_gone_is_flagged_once_the_scan_has_looked() {
    let dir = data();
    let sources = user(dir.path(), A).join("project-sources").join("p1");
    std::fs::create_dir_all(&sources).unwrap();
    std::fs::write(sources.join(format!("{REF1}.txt")), "present").unwrap();
    put(
        dir.path(),
        A,
        "projects.json",
        &format!(
            r#"{{"projects":[{{"id":"p1","files":[{{"name":"here.md","textRef":"{REF1}"}},{{"name":"gone.md","textRef":"{REF2}"}},{{"name":"inline.md","content":"x"}},{{"name":"odd.md","textRef":"not-a-ref"}}]}}]}}"#
        ),
    );
    let addr = front(dir.path()).await;
    let flags = |v: &serde_json::Value| -> Vec<bool> {
        v["projects"][0]["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f.get("textMissing").is_some())
            .collect()
    };
    // The first list shows nothing flagged (unknown is not flagged) and starts the scan; a list never touches the disk per source.
    assert_eq!(
        flags(&list(addr, COOKIE_A).await),
        [false, false, false, false]
    );
    server_projects::workspace_scan::settled(&user(dir.path(), A));
    let v = list(addr, COOKIE_A).await;
    assert_eq!(flags(&v), [false, true, false, false]);
    assert_eq!(
        v["projects"][0]["files"][1],
        serde_json::json!({"name":"gone.md","textRef":REF2,"textMissing":true})
    );
    // The text written back clears the flag at the next list; removed again, it is flagged again after the next scan.
    std::fs::write(sources.join(format!("{REF2}.txt")), "back").unwrap();
    assert_eq!(
        flags(&list(addr, COOKIE_A).await),
        [false, false, false, false]
    );
    std::fs::remove_file(sources.join(format!("{REF2}.txt"))).unwrap();
    server_projects::workspace_scan::due_again(&user(dir.path(), A));
    let _ = list(addr, COOKIE_A).await;
    server_projects::workspace_scan::settled(&user(dir.path(), A));
    assert_eq!(
        flags(&list(addr, COOKIE_A).await),
        [false, true, false, false]
    );
    // A symlink where the text should be is not followed: it counts as gone.
    std::fs::write(user(dir.path(), A).join("elsewhere.txt"), "outside").unwrap();
    std::os::unix::fs::symlink(
        user(dir.path(), A).join("elsewhere.txt"),
        sources.join(format!("{REF2}.txt")),
    )
    .unwrap();
    assert_eq!(
        flags(&list(addr, COOKIE_A).await),
        [false, true, false, false]
    );
}

#[tokio::test]
async fn only_the_get_is_the_fronts() {
    let dir = data();
    let addr = front(dir.path()).await;
    for method in ["POST", "PUT", "DELETE"] {
        assert_eq!(
            call(addr, method, "/api/workspace", COOKIE_A).await.status,
            502,
            "{method}"
        );
    }
    assert_eq!(
        call(addr, "GET", "/api/workspace/", COOKIE_A).await.status,
        502
    );
    assert_eq!(
        call(addr, "GET", "/api/workspace/model", COOKIE_A)
            .await
            .status,
        502
    );
}
