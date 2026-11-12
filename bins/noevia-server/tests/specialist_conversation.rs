//! A coworker's conversation through the real front (sbstndalton/noevia#2446, slice 4a): the hidden
//! backing project and chat are made, mirrored and removed with the coworker, are in no list, search,
//! export or move, and serve history GET/POST like any chat; the chat id is exclusive (#765). The legacy
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
    text: String,
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
    let raw = String::from_utf8_lossy(&bytes).into_owned();
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
        text: b.to_owned(),
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

impl Rig {
    async fn coworker(&self, name: &str, brief: &str) -> Value {
        let r = self
            .a(
                "POST",
                "/api/specialists",
                Some(json!({"name": name, "brief": brief})),
            )
            .await;
        assert_eq!(r.status, 201, "{}", r.json);
        r.json["specialist"].clone()
    }
    fn projects_file(&self, user: &str) -> Value {
        let text = std::fs::read_to_string(self.user_dir(user).join("projects.json")).unwrap();
        serde_json::from_str(&text).unwrap()
    }
    fn project(&self, user: &str, id: &str) -> Option<Value> {
        self.projects_file(user)["projects"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["id"] == id)
            .cloned()
    }
    /// An ordinary project the account owns, written the way the store does.
    fn seed_project(&self, user: &str) {
        let text = json!({"projects": [
            {"id": "proj-1-aaaaaa", "name": "QA ordinary", "chats": [{"id": "qa-ordinary", "title": "QA ordinary chat", "updatedAt": 5}]}
        ]});
        std::fs::write(self.user_dir(user).join("projects.json"), text.to_string()).unwrap();
        std::fs::write(
            self.user_dir(user).join("free-chats.json"),
            json!([{"id": "qa-free", "title": "QA free chat", "updatedAt": 6}]).to_string(),
        )
        .unwrap();
    }
}

fn ids_in(v: &Value) -> Vec<String> {
    fn walk(v: &Value, out: &mut Vec<String>) {
        match v {
            Value::String(s) => out.push(s.clone()),
            Value::Array(a) => a.iter().for_each(|x| walk(x, out)),
            Value::Object(o) => o.values().for_each(|x| walk(x, out)),
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk(v, &mut out);
    out
}

fn mentions(v: &Value, needle: &str) -> bool {
    ids_in(v).iter().any(|s| s.contains(needle))
}

#[tokio::test]
async fn create_patch_delete_keep_the_hidden_project_and_chat_in_step() {
    let r = rig().await;
    r.seed_project(A);
    let s = r.coworker("QA Scribe", "Write short notes.").await;
    let id = s["id"].as_str().unwrap();
    let (pid, cid) = (
        s["projectId"].as_str().unwrap().to_owned(),
        s["chatId"].as_str().unwrap().to_owned(),
    );
    assert_eq!(pid, format!("specialist-{id}"));
    assert_eq!(cid, format!("specialist-chat-{id}"));
    assert_eq!(s["toolboxes"], json!([]));
    let p = r.project(A, &pid).expect("hidden project in the store");
    assert_eq!(p["instructions"], "Write short notes.");
    assert_eq!(p["toolboxes"], json!([]));
    assert_eq!(p["chats"].as_array().unwrap().len(), 1);
    assert_eq!(p["chats"][0]["id"], cid.as_str());
    // The other account has none of it.
    assert!(r.projects_file_missing_or_without(B, &pid));

    // Patching the brief mirrors it to the project.
    let got = r.a("GET", &format!("/api/specialists/{id}"), None).await;
    let rev = got.json["revision"].as_u64().unwrap();
    let up = r
        .a(
            "PATCH",
            &format!("/api/specialists/{id}"),
            Some(json!({"revision": rev, "brief": "Write long notes."})),
        )
        .await;
    assert_eq!(up.status, 200, "{}", up.json);
    assert_eq!(r.project(A, &pid).unwrap()["instructions"], "Write long notes.");

    // A toolbox the catalogue cannot vouch for is refused, never accepted unchecked (no live
    // discovery here, so the answer is the retryable 503), and changes nothing.
    let rev = up.json["revision"].as_u64().unwrap();
    let bad = r
        .a(
            "PATCH",
            &format!("/api/specialists/{id}"),
            Some(json!({"revision": rev, "toolboxes": ["core"], "brief": "nope"})),
        )
        .await;
    assert_eq!(bad.status, 503, "{}", bad.json);
    assert_eq!(code(&bad), "tools_unavailable");
    assert_eq!(r.project(A, &pid).unwrap()["instructions"], "Write long notes.");
    let again = r.a("GET", &format!("/api/specialists/{id}"), None).await;
    assert_eq!(again.json["specialist"]["brief"], "Write long notes.");
    assert_eq!(again.json["specialist"]["toolboxes"], json!([]));

    // Delete: the record, the project and the chat (tombstoned) all go; the rest stays.
    let rev = again.json["revision"].as_u64().unwrap();
    let del = r
        .a("DELETE", &format!("/api/specialists/{id}?revision={rev}"), None)
        .await;
    assert_eq!(del.status, 200, "{}", del.json);
    assert!(r.project(A, &pid).is_none());
    assert!(r.project(A, "proj-1-aaaaaa").is_some());
    let tomb = std::fs::read_to_string(r.user_dir(A).join("deleted-chats.json")).unwrap();
    assert!(tomb.contains(&cid), "{tomb}");
    let hist = r.a("GET", &format!("/api/chats/{cid}/history"), None).await;
    assert_eq!(hist.json["history"], json!([]));
    let post = r
        .a(
            "POST",
            &format!("/api/chats/{cid}/history"),
            Some(json!({"history": [{"role": "user", "content": "late"}]})),
        )
        .await;
    assert_eq!(post.status, 410, "a deleted conversation never comes back");
}

impl Rig {
    fn projects_file_missing_or_without(&self, user: &str, id: &str) -> bool {
        match std::fs::read_to_string(self.user_dir(user).join("projects.json")) {
            Err(_) => true,
            Ok(t) => !t.contains(id),
        }
    }
}

#[tokio::test]
async fn history_works_for_the_coworkers_chat_id() {
    let r = rig().await;
    let s = r.coworker("QA Scribe", "b").await;
    let cid = s["chatId"].as_str().unwrap();
    let path = format!("/api/chats/{cid}/history");
    let empty = r.a("GET", &path, None).await;
    assert_eq!(empty.status, 200);
    assert_eq!(empty.json["history"], json!([]));
    let first = r
        .a(
            "POST",
            &path,
            Some(json!({"history": [
                {"role": "user", "content": "QA-needle hello"},
                {"role": "assistant", "content": "hi"}
            ]})),
        )
        .await;
    assert_eq!(first.status, 200, "{}", first.json);
    let rev = first.json["revision"].as_str().unwrap().to_owned();
    let got = r.a("GET", &path, None).await;
    assert_eq!(got.json["history"].as_array().unwrap().len(), 2);
    assert_eq!(got.json["revision"], rev.as_str());
    // The revision rule holds: a stale base is a 409 with the current copy.
    let stale = r
        .a(
            "POST",
            &path,
            Some(json!({"baseRevision": "0".repeat(64), "history": []})),
        )
        .await;
    assert_eq!(stale.status, 409);
    // Another account sees an empty chat of that id and cannot touch this one.
    let other = r.go("GET", &path, COOKIE_B, None).await;
    assert_eq!(other.json["history"], json!([]));
    let theirs = r
        .go(
            "POST",
            &path,
            COOKIE_B,
            Some(json!({"history": [{"role": "user", "content": "QA-b"}]})),
        )
        .await;
    assert_eq!(theirs.status, 200);
    assert_eq!(
        r.a("GET", &path, None).await.json["history"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[tokio::test]
async fn the_coworkers_project_and_chat_are_in_no_list_search_or_export() {
    let r = rig().await;
    r.seed_project(A);
    let s = r.coworker("QA Hidden Scribe", "b").await;
    let (pid, cid) = (
        s["projectId"].as_str().unwrap().to_owned(),
        s["chatId"].as_str().unwrap().to_owned(),
    );
    let name_needle = "QA Hidden Scribe";
    let post = r
        .a(
            "POST",
            &format!("/api/chats/{cid}/history"),
            Some(json!({"history": [{"role": "user", "content": "QA-zebra-needle"}]})),
        )
        .await;
    assert_eq!(post.status, 200);

    // The sidebar's workspace: the ordinary project and chats stay, the coworker's are not there.
    let ws = r.a("GET", "/api/workspace", None).await;
    assert_eq!(ws.status, 200, "{}", ws.json);
    assert!(mentions(&ws.json, "proj-1-aaaaaa") && mentions(&ws.json, "qa-free"));
    for needle in [pid.as_str(), cid.as_str(), "specialist-", name_needle] {
        assert!(!mentions(&ws.json, needle), "workspace shows {needle}: {}", ws.json);
    }
    // Search finds an ordinary chat with the same words (the control) but not the coworker's.
    let ord = r
        .a(
            "POST",
            "/api/chats/qa-ordinary/history",
            Some(json!({"history": [{"role": "user", "content": "QA-zebra-needle"}]})),
        )
        .await;
    assert_eq!(ord.status, 200);
    let s1 = r.a("GET", "/api/chats/search?q=QA-zebra-needle", None).await;
    assert_eq!(s1.status, 200, "{}", s1.json);
    assert!(mentions(&s1.json, "qa-ordinary"), "control: {}", s1.json);
    assert!(!mentions(&s1.json, &cid), "{}", s1.json);
    // Export: the ordinary chat downloads (the control), the coworker's does not.
    let control = r.a("GET", "/api/chats/qa-ordinary/export", None).await;
    assert_eq!(control.status, 200);
    let ex = r.a("GET", &format!("/api/chats/{cid}/export"), None).await;
    assert_eq!(ex.status, 404, "{:?}", ex.json);
    // An ordinary project's chat list and the free list hold none of it.
    let chats = r.a("GET", "/api/projects/proj-1-aaaaaa/chats", None).await;
    assert!(!mentions(&chats.json, &cid), "{}", chats.json);
    let free = r.a("GET", "/api/freechats", None).await;
    assert!(!mentions(&free.json, &cid), "{}", free.json);
    // The account-wide export of conversations holds the ordinary chat and none of the coworker's.
    let all = r.a("GET", "/api/export/conversations", None).await;
    assert_eq!(all.status, 200, "{}", all.text);
    assert!(all.text.contains("QA ordinary"), "control");
    assert!(!all.text.contains("QA Hidden Scribe") && !all.text.contains("specialist-"));
}

#[tokio::test]
async fn the_chat_id_is_exclusive_to_the_coworkers_project() {
    let r = rig().await;
    r.seed_project(A);
    let s = r.coworker("QA Scribe", "b").await;
    let cid = s["chatId"].as_str().unwrap().to_owned();
    // #765: another list that tries to take the id is refused and told so, and the project's own
    // list keeps it.
    let into_project = r
        .a(
            "POST",
            "/api/projects/proj-1-aaaaaa/chats",
            Some(json!({"chats": [{"id": cid, "title": "stolen", "updatedAt": 9}]})),
        )
        .await;
    assert_eq!(into_project.status, 200, "{}", into_project.json);
    let listed = r.a("GET", "/api/projects/proj-1-aaaaaa/chats", None).await;
    assert!(!mentions(&listed.json, &cid), "{}", listed.json);
    assert!(mentions(&into_project.json, &cid), "reported as skipped: {}", into_project.json);
    let into_free = r
        .a(
            "POST",
            "/api/freechats",
            Some(json!({"chats": [{"id": cid, "title": "stolen", "updatedAt": 9}]})),
        )
        .await;
    assert_eq!(into_free.status, 200, "{}", into_free.json);
    let free = r.a("GET", "/api/freechats", None).await;
    assert!(!mentions(&free.json, &cid), "{}", free.json);
    let hidden = r
        .a("GET", &format!("/api/projects/{}/chats", s["projectId"].as_str().unwrap()), None)
        .await;
    assert!(mentions(&hidden.json, &cid), "{}", hidden.json);
}

#[tokio::test]
async fn the_conversation_can_only_be_deleted_with_the_coworker() {
    let r = rig().await;
    let s = r.coworker("QA Scribe", "b").await;
    let (pid, cid) = (
        s["projectId"].as_str().unwrap().to_owned(),
        s["chatId"].as_str().unwrap().to_owned(),
    );
    let chat = r
        .a("DELETE", &format!("/api/projects/{pid}/chats/{cid}"), None)
        .await;
    assert_eq!(chat.status, 404, "{}", chat.json);
    let project = r.a("DELETE", &format!("/api/projects/{pid}"), None).await;
    assert_eq!(project.status, 404, "{}", project.json);
    let p = r.project(A, &pid).unwrap();
    assert_eq!(p["chats"][0]["id"], cid.as_str());
    assert!(!std::fs::read_to_string(r.user_dir(A).join("deleted-chats.json"))
        .unwrap_or_default()
        .contains(&cid));
}

#[tokio::test]
async fn an_older_coworker_gets_its_conversation_on_the_first_read() {
    let r = rig().await;
    let id = "dddddddd-dddd-4ddd-8ddd-dddddddddddd";
    std::fs::write(
        r.user_dir(A).join("specialists.json"),
        json!({"revision": 2, "specialists": [{
            "id": id, "name": "QA Old", "glyph": "", "tint": "", "brief": "old brief",
            "projectId": null, "chatId": null, "spaces": [], "defaultSpace": null,
            "createdAt": 1, "updatedAt": 1
        }]})
        .to_string(),
    )
    .unwrap();
    let got = r.a("GET", &format!("/api/specialists/{id}"), None).await;
    assert_eq!(got.status, 200, "{}", got.json);
    assert_eq!(got.json["specialist"]["projectId"], format!("specialist-{id}"));
    assert_eq!(got.json["specialist"]["chatId"], format!("specialist-chat-{id}"));
    assert_eq!(
        r.project(A, &format!("specialist-{id}")).unwrap()["instructions"],
        "old brief"
    );
    let hist = r
        .a("GET", &format!("/api/chats/specialist-chat-{id}/history"), None)
        .await;
    assert_eq!(hist.status, 200);
}

#[tokio::test]
async fn two_coworkers_and_two_accounts_stay_apart() {
    let r = rig().await;
    let a1 = r.coworker("QA One", "one").await;
    let a2 = r.coworker("QA Two", "two").await;
    assert_ne!(a1["projectId"], a2["projectId"]);
    assert_ne!(a1["chatId"], a2["chatId"]);
    let b1 = r
        .go("POST", "/api/specialists", COOKIE_B, Some(json!({"name": "QA Bee", "brief": "bee"})))
        .await;
    assert_eq!(b1.status, 201);
    let theirs = b1.json["specialist"]["projectId"].as_str().unwrap().to_owned();
    assert!(r.project(A, &theirs).is_none());
    assert!(r.project(B, a1["projectId"].as_str().unwrap()).is_none());
    // Account A's list never names B's coworker.
    let list = r.a("GET", "/api/specialists", None).await;
    assert_eq!(list.json["specialists"].as_array().unwrap().len(), 2);
    // B cannot read or delete A's coworker (404, not 403).
    let id = a1["id"].as_str().unwrap();
    assert_eq!(r.go("GET", &format!("/api/specialists/{id}"), COOKIE_B, None).await.status, 404);
    assert_eq!(
        r.go("DELETE", &format!("/api/specialists/{id}?revision=1"), COOKIE_B, None)
            .await
            .status,
        404
    );
    assert!(r.project(A, a1["projectId"].as_str().unwrap()).is_some());
}
