//! `POST /v1/specialists/page-save` on the internal store (sbstndalton/noevia#2446, slice 4b): the
//! secret, the account binding, the grants and the answers Node's chat loop turns into a tool
//! result. The grant and same-draft logic is crates/server-specialists `tools`; this is the wire.
//! Synthetic data only.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
use noevia_server::project_store::{self, Store, TOKEN_HEADER};
use serde_json::{json, Value};
use server_specialists::{Backing, NewSpace, NewSpecialist, Store as Specialists};
use std::net::SocketAddr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const A: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
const B: &str = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
const TOKEN: &str = "synthetic-store-token-0123456789abcdef";

async fn post(addr: SocketAddr, token: Option<&str>, body: &Value) -> (u16, Value) {
    let body = body.to_string();
    let mut req = format!(
        "POST /v1/specialists/page-save HTTP/1.1\r\nhost: store.test\r\nconnection: close\r\ncontent-type: application/json\r\ncontent-length: {}\r\n",
        body.len()
    );
    if let Some(t) = token {
        req.push_str(&format!("{TOKEN_HEADER}: {t}\r\n"));
    }
    req.push_str("\r\n");
    req.push_str(&body);
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(req.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    s.read_to_end(&mut out).await.unwrap();
    let text = String::from_utf8(out).unwrap();
    let status = text[9..12].parse().unwrap();
    let body = text.split_once("\r\n\r\n").unwrap().1;
    (status, serde_json::from_str(body).unwrap_or(Value::Null))
}

fn account(root: &std::path::Path, id: &str) -> Specialists {
    let dir = server_projects::store::user_dir(root, id).unwrap();
    std::fs::create_dir_all(&dir).unwrap();
    Specialists::new(dir).with_backing(Backing {
        data_dir: root.to_path_buf(),
        user_id: id.to_owned(),
    })
}

#[tokio::test]
async fn the_loop_saves_a_page_for_the_conversations_coworker_only() {
    let dir = tempfile::tempdir().unwrap();
    let mine = account(dir.path(), A);
    let theirs = account(dir.path(), B);
    let space = mine
        .create_space(NewSpace {
            name: "QA notes".into(),
        })
        .unwrap()
        .1
        .id;
    let secret = mine
        .create_space(NewSpace {
            name: "QA secret".into(),
        })
        .unwrap()
        .1
        .id;
    let who = mine
        .create_specialist(NewSpecialist {
            name: "QA writer".into(),
            spaces: Some(vec![space.clone()]),
            default_space: Some(space.clone()),
            ..Default::default()
        })
        .unwrap()
        .1;
    let (project, chat) = (
        who.project_id.clone().unwrap(),
        who.chat_id.clone().unwrap(),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let store = Store {
        data_dir: dir.path().to_path_buf(),
        token: TOKEN.into(),
        switch: server_store::RustProjects::owned(),
        start_id: "".into(),
    };
    tokio::spawn(project_store::serve_with_context(
        listener,
        store,
        None,
        None,
        std::future::pending(),
    ));
    let call = |user: &str, p: &str, c: &str, extra: Value| {
        let mut body = json!({"user": user, "projectId": p, "chatId": c, "title": "QA plan", "content": "# QA\nbody"});
        for (k, v) in extra.as_object().unwrap() {
            body[k] = v.clone();
        }
        body
    };
    let ok = call(A, &project, &chat, json!({}));

    // The secret first; the body is never looked at without it.
    assert_eq!(post(addr, None, &ok).await.0, 401);
    assert_eq!(
        post(addr, Some("wrong-wrong-wrong-wrong-wrong-wrong"), &ok)
            .await
            .0,
        401
    );
    assert_eq!(post(addr, Some(TOKEN), &json!({"nope": 1})).await.0, 400);
    assert_eq!(
        post(addr, Some(TOKEN), &call("../x", &project, &chat, json!({})))
            .await
            .0,
        400
    );

    let (status, saved) = post(addr, Some(TOKEN), &ok).await;
    assert_eq!(status, 200, "{saved}");
    assert_eq!(saved["spaceId"], space.as_str());
    assert_eq!(saved["title"], "QA plan");
    let page = saved["pageId"].as_str().unwrap().to_owned();
    assert_eq!(saved["link"], format!("/spaces/{space}/{page}"));
    assert_eq!(mine.get_page(&space, &page).unwrap().1, "# QA\nbody");

    // The same draft answers with the same page.
    let (status, again) = post(addr, Some(TOKEN), &ok).await;
    assert_eq!(
        (status, again["pageId"].as_str()),
        (200, Some(page.as_str()))
    );
    assert_eq!(mine.list_pages(&space, None, None, 0).unwrap().total, 1);

    // A Space the coworker was not given, and one that does not exist: refused, nothing written.
    for s in [secret.as_str(), "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"] {
        let (status, body) = post(
            addr,
            Some(TOKEN),
            &call(A, &project, &chat, json!({"spaceId": s})),
        )
        .await;
        assert_eq!(status, 400, "{body}");
        assert!(body["error"]
            .as_str()
            .unwrap()
            .contains("nothing was saved"));
    }
    assert_eq!(mine.list_pages(&secret, None, None, 0).unwrap().total, 0);

    // Another account naming this conversation, or this account naming another's: 404, no page.
    let (status, _) = post(
        addr,
        Some(TOKEN),
        &call(B, &project, &chat, json!({"spaceId": space})),
    )
    .await;
    assert_eq!(status, 404);
    assert!(theirs.list_specialists().unwrap().specialists.is_empty());
    for (p, c) in [
        ("proj-1-aaaaaa", chat.as_str()),
        (project.as_str(), "free-chat-1"),
    ] {
        assert_eq!(
            post(addr, Some(TOKEN), &call(A, p, c, json!({}))).await.0,
            404
        );
    }
    assert_eq!(mine.list_pages(&space, None, None, 0).unwrap().total, 1);

    // A page of control characters is 6 JSON bytes each: 3 MiB on the wire, still a normal save.
    let heavy = "\u{1}".repeat(server_specialists::MAX_PAGE_BYTES);
    let (status, body) = post(
        addr,
        Some(TOKEN),
        &call(A, &project, &chat, json!({"content": heavy})),
    )
    .await;
    assert_eq!(status, 200, "{body}");

    // Limits: an empty title and an oversized page.
    let (status, _) = post(
        addr,
        Some(TOKEN),
        &call(A, &project, &chat, json!({"title": " "})),
    )
    .await;
    assert_eq!(status, 400);
    let big = "x".repeat(server_specialists::MAX_PAGE_BYTES + 1);
    let (status, body) = post(
        addr,
        Some(TOKEN),
        &call(A, &project, &chat, json!({"content": big})),
    )
    .await;
    assert_eq!(
        (status, body["code"].as_str()),
        (413, Some("page_too_large"))
    );
}
