//! Track R14 step 5: `POST /v1/projects/text-write` on the front's internal project store, end to
//! end (the write itself is server_projects::text_write, recorded against core in its own tests):
//! the store token, the body core sends, the account core names (gone, disabled, a member), and a
//! write on local storage that lands in the account's projects.json and index outbox only.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
use noevia_server::project_store::{serve_with_context, Store, TOKEN_HEADER};
use noevia_server::{config::Config, App};
use server_store::rusqlite::Connection;
use std::collections::HashMap;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const TOKEN: &str = "synthetic-store-token-0123456789abcdef";
const A: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
const B: &str = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
const GONE: &str = "cccccccc-cccc-4ccc-8ccc-cccccccccccc";
const OFF: &str = "dddddddd-dddd-4ddd-8ddd-dddddddddddd";

fn seed(dir: &std::path::Path) {
    std::fs::write(dir.join("secrets.key"), [7u8; 32]).unwrap();
    let db = Connection::open(dir.join("cowork.db")).unwrap();
    db.execute_batch(include_str!(
        "../../../crates/server-store/tests/fixtures/node-schema.sql"
    ))
    .unwrap();
    for (uid, role) in [(A, "admin"), (B, "member"), (OFF, "member")] {
        db.execute("INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,created_at,updated_at) VALUES(?1,?1,?1,'QA synthetic',?2,'x',?1,1,1)", (uid, role)).unwrap();
    }
    db.execute("UPDATE users SET disabled_at=5 WHERE id=?1", [OFF])
        .unwrap();
    for uid in [A, B, OFF] {
        let user = dir.join("users").join(uid);
        std::fs::create_dir_all(&user).unwrap();
        std::fs::write(
            user.join("projects.json"),
            r#"{"projects":[{"id":"qa-p","name":"QA","modes":["chat"],"chats":[],"instructionSkills":{},"files":[]}]}"#,
        )
        .unwrap();
    }
}

fn j(s: &str) -> serde_json::Value {
    serde_json::from_str(s).unwrap()
}

#[tokio::test]
async fn the_op_writes_for_the_account_core_names_and_nobody_else() {
    let d = tempfile::tempdir().unwrap();
    seed(d.path());
    let env = HashMap::from([
        ("NOEVIA_LEGACY_UPSTREAM", "http://127.0.0.1:9".to_string()),
        ("UI_DATA_DIR", d.path().display().to_string()),
    ]);
    let app = App::new(Config::from_lookup(|k| env.get(k).cloned()).unwrap());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let store = Store {
        data_dir: d.path().to_path_buf(),
        token: TOKEN.into(),
        switch: server_store::RustProjects::owned(),
        start_id: "".into(),
    };
    tokio::spawn(serve_with_context(
        listener,
        store,
        None,
        Some(app),
        std::future::pending(),
    ));
    let op = |token: Option<&'static str>, body: String| async move {
        let mut s = TcpStream::connect(addr).await.unwrap();
        let mut req = format!("POST /v1/projects/text-write HTTP/1.1\r\nhost: store\r\nconnection: close\r\ncontent-type: application/json\r\ncontent-length: {}\r\n", body.len());
        if let Some(t) = token {
            req.push_str(&format!("{TOKEN_HEADER}: {t}\r\n"));
        }
        req.push_str("\r\n");
        s.write_all(req.as_bytes()).await.unwrap();
        s.write_all(body.as_bytes()).await.unwrap();
        let mut out = Vec::new();
        s.read_to_end(&mut out).await.unwrap();
        let text = String::from_utf8_lossy(&out).into_owned();
        let status: u16 = text[9..12].parse().unwrap();
        assert!(
            text.to_ascii_lowercase()
                .contains("cache-control: no-store"),
            "{text}"
        );
        (status, j(text.split_once("\r\n\r\n").unwrap().1))
    };
    let create = |user: &str, name: &str| {
        format!(
            r#"{{"user":"{user}","project":"qa-p","name":"{name}","text":"QA synthetic text"}}"#
        )
    };
    // The store token, the body core sends, the account.
    assert_eq!(op(None, create(A, "a.md")).await.0, 401);
    assert_eq!(
        op(
            Some("wrong-token-wrong-token-wrong-token!"),
            create(A, "a.md")
        )
        .await
        .0,
        401
    );
    assert_eq!(op(Some(TOKEN), "[]".into()).await.0, 400);
    assert_eq!(
        op(
            Some(TOKEN),
            r#"{"user":"../x","project":"qa-p","name":"a.md","text":""}"#.into()
        )
        .await
        .0,
        400
    );
    assert_eq!(op(Some(TOKEN), r#"{"user":"aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa","project":"","name":"a.md","text":""}"#.into()).await.0, 400);
    assert_eq!(
        op(Some(TOKEN), create(GONE, "a.md")).await,
        (404, j(r#"{"error":"no such account"}"#))
    );
    assert_eq!(op(Some(TOKEN), create(OFF, "a.md")).await.0, 404);
    // A write on local storage: the record is answered and saved, and only that account changes.
    let (status, reply) = op(Some(TOKEN), create(B, "b.md")).await;
    assert_eq!(status, 200, "{reply}");
    assert_eq!(reply["file"]["name"], "b.md");
    let listed = |uid: &str| {
        let text = std::fs::read_to_string(d.path().join("users").join(uid).join("projects.json"))
            .unwrap();
        j(&text)["projects"][0]["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["name"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>()
    };
    assert_eq!(listed(B), vec!["b.md".to_owned()]);
    assert!(listed(A).is_empty() && listed(OFF).is_empty());
    let jobs = server_projects::outbox::jobs(&d.path().join("users").join(B)).unwrap();
    assert_eq!(
        jobs.iter()
            .map(|r| (r.kind.as_str(), r.file.as_deref()))
            .collect::<Vec<_>>(),
        vec![("index", Some("b.md"))]
    );
    // A refusal is the model's text with a 200, and nothing changes.
    let (status, reply) = op(
        Some(TOKEN),
        r#"{"user":"bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb","project":"qa-p","name":"c.md","text":"QA","expectName":"gone.md"}"#.into(),
    )
    .await;
    assert_eq!(
        (status, reply),
        (
            200,
            j(r#"{"error":"\"gone.md\" is no longer in this project; nothing was saved"}"#)
        )
    );
    assert_eq!(listed(B), vec!["b.md".to_owned()]);
}
