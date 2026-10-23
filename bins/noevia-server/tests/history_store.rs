//! sbstndalton/noevia#1071 slice 4: the transcript ops of the internal project store
//! (src/project_store.rs `/v1/history/*`, crates/server-projects `transcript`) over real HTTP: the
//! per-start secret (a previous start's is refused), tenant binding, the compare-and-swap and its
//! conflict shape, tombstones, the interrupted-reply append racing client saves, and a Node
//! process saving through the store while Rust saves too, with a blind-save control that loses a
//! write. Synthetic, QA-named data only.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use js_json::{wtf16, JValue};
use noevia_server::project_store::{serve, Store, TOKEN_HEADER};
use serde_json::{json, Value};
use server_projects::history::{history_file, revision_of};
use server_projects::transcript::{self, Base, Saved};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const USER: &str = "11111111-2222-4333-8444-555555555555";
const OTHER: &str = "99999999-2222-4333-8444-555555555555";
const TOKEN: &str = "synthetic-store-token-0123456789abcdef";
const START: &str = "0123456789abcdef0123456789abcdef";

fn user_dir(root: &Path, user: &str) -> PathBuf {
    root.join("users").join(user)
}

async fn serve_on(root: &Path, token: &str) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let store = Store {
        data_dir: root.to_path_buf(),
        token: token.into(),
        switch: server_store::RustProjects::owned(),
        start_id: START.into(),
    };
    tokio::spawn(serve(listener, store, std::future::pending()));
    addr
}

async fn start() -> (tempfile::TempDir, SocketAddr) {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(user_dir(root.path(), USER)).unwrap();
    let addr = serve_on(root.path(), TOKEN).await;
    (root, addr)
}

async fn call(addr: SocketAddr, path: &str, token: Option<&str>, body: &str) -> (u16, Value) {
    let mut s = TcpStream::connect(addr).await.unwrap();
    let mut req = format!(
        "POST {path} HTTP/1.1\r\nhost: store\r\nconnection: close\r\ncontent-type: application/json\r\ncontent-length: {}\r\n",
        body.len()
    );
    if let Some(t) = token {
        req.push_str(&format!("{TOKEN_HEADER}: {t}\r\n"));
    }
    req.push_str("\r\n");
    s.write_all(req.as_bytes()).await.unwrap();
    s.write_all(body.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    s.read_to_end(&mut out).await.unwrap();
    let text = String::from_utf8_lossy(&out).into_owned();
    let status = text[9..12].parse().unwrap();
    let body = text.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("");
    (status, serde_json::from_str(body).unwrap_or(Value::Null))
}

async fn op(addr: SocketAddr, name: &str, body: Value) -> (u16, Value) {
    call(
        addr,
        &format!("/v1/history/{name}"),
        Some(TOKEN),
        &body.to_string(),
    )
    .await
}

fn rev(v: &Value) -> String {
    revision_of(&wtf16::parse(&v.to_string()).unwrap()).unwrap()
}

#[tokio::test]
async fn every_op_needs_this_starts_secret_and_touches_nothing_without_it() {
    let (root, addr) = start().await;
    let dir = user_dir(root.path(), USER);
    std::fs::write(history_file(&dir, "c1"), r#"{"history":["qa kept"]}"#).unwrap();
    let ops = [
        ("read", json!({"user": USER, "chat": "c1"})),
        (
            "write",
            json!({"user": USER, "chat": "c1", "history": ["qa x"]}),
        ),
        (
            "append-if-last",
            json!({"user": USER, "chat": "c1", "message": "q", "entry": {"role": "assistant"}}),
        ),
        (
            "remove-if",
            json!({"user": USER, "chat": "c1", "history": ["qa kept"]}),
        ),
    ];
    for (name, body) in &ops {
        for token in [
            None,
            Some("wrong"),
            Some(&TOKEN[..TOKEN.len() - 1]),
            Some(""),
        ] {
            let (status, answer) = call(
                addr,
                &format!("/v1/history/{name}"),
                token,
                &body.to_string(),
            )
            .await;
            assert_eq!(status, 401, "{name} {token:?}");
            assert_eq!(answer, json!({"error": "unauthorised"}));
        }
    }
    assert_eq!(
        std::fs::read_to_string(history_file(&dir, "c1")).unwrap(),
        r#"{"history":["qa kept"]}"#
    );
}

#[tokio::test]
async fn a_previous_starts_secret_is_refused_after_a_restart() {
    // The web supervisor makes a new secret for every container start; a Node left over from the
    // previous start (or anything that learned its secret) cannot write through the new front.
    let (root, old) = start().await;
    let (status, _) = op(
        old,
        "write",
        json!({"user": USER, "chat": "c1", "history": ["qa 1"]}),
    )
    .await;
    assert_eq!(status, 200);
    let fresh = serve_on(root.path(), "synthetic-store-token-new-start-0123456789").await;
    let body = json!({"user": USER, "chat": "c1", "history": ["qa stale"]}).to_string();
    let (status, _) = call(fresh, "/v1/history/write", Some(TOKEN), &body).await;
    assert_eq!(status, 401);
    let (status, now) = call(
        fresh,
        "/v1/history/read",
        Some("synthetic-store-token-new-start-0123456789"),
        &json!({"user": USER, "chat": "c1"}).to_string(),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(now["history"], json!(["qa 1"]));
}

#[tokio::test]
async fn a_request_is_bound_to_the_user_it_names() {
    let (root, addr) = start().await;
    std::fs::create_dir_all(user_dir(root.path(), OTHER)).unwrap();
    std::fs::write(
        history_file(&user_dir(root.path(), OTHER), "c1"),
        r#"{"history":["qa theirs"]}"#,
    )
    .unwrap();
    let (status, _) = op(
        addr,
        "write",
        json!({"user": USER, "chat": "c1", "history": ["qa mine"]}),
    )
    .await;
    assert_eq!(status, 200);
    let (_, mine) = op(addr, "read", json!({"user": USER, "chat": "c1"})).await;
    assert_eq!(mine["history"], json!(["qa mine"]));
    let (_, theirs) = op(addr, "read", json!({"user": OTHER, "chat": "c1"})).await;
    assert_eq!(theirs["history"], json!(["qa theirs"]));
    // A chat id cannot reach another user's directory.
    let sneaky = format!("../{OTHER}/c1");
    let (status, _) = op(
        addr,
        "write",
        json!({"user": USER, "chat": sneaky, "history": ["qa x"]}),
    )
    .await;
    assert_eq!(status, 200);
    let (_, theirs) = op(addr, "read", json!({"user": OTHER, "chat": "c1"})).await;
    assert_eq!(theirs["history"], json!(["qa theirs"]));
    // Not a user id: refused; a removed account (no directory): 410, and nothing recreated.
    for user in ["../x", "", "admin"] {
        let (status, _) = op(
            addr,
            "write",
            json!({"user": user, "chat": "c1", "history": []}),
        )
        .await;
        assert_eq!(status, 400, "{user}");
    }
    let gone = "22222222-2222-4333-8444-555555555555";
    let (status, answer) = op(
        addr,
        "write",
        json!({"user": gone, "chat": "c1", "history": []}),
    )
    .await;
    assert_eq!(
        (status, answer),
        (410, json!({"error": "account no longer exists"}))
    );
    assert!(!user_dir(root.path(), gone).exists());
    // A missing user or chat is a bad request, never a default.
    for body in [
        json!({"chat": "c1", "history": []}),
        json!({"user": USER, "history": []}),
        json!({"user": USER, "chat": "..", "history": []}),
    ] {
        let (status, _) = op(addr, "write", body.clone()).await;
        assert_eq!(status, 400, "{body}");
    }
}

#[tokio::test]
async fn the_compare_and_swap_answers_the_current_copy_on_conflict() {
    let (_root, addr) = start().await;
    let (_, empty) = op(addr, "read", json!({"user": USER, "chat": "c1"})).await;
    assert_eq!(
        empty,
        json!({"history": [], "revision": rev(&json!([])), "exists": false})
    );
    let one = json!([{"role": "user", "content": "qa one"}]);
    let (status, saved) = op(
        addr,
        "write",
        json!({"user": USER, "chat": "c1", "history": one, "baseRevision": rev(&json!([]))}),
    )
    .await;
    assert_eq!((status, saved), (200, json!({"revision": rev(&one)})));
    let two = json!([{"role": "user", "content": "qa two"}]);
    let (status, conflict) = op(
        addr,
        "write",
        json!({"user": USER, "chat": "c1", "history": two, "baseRevision": rev(&json!([]))}),
    )
    .await;
    assert_eq!(status, 409);
    assert_eq!(
        conflict,
        json!({"history": one, "revision": rev(&one), "exists": true, "proposedRevision": rev(&two)})
    );
    // ifAbsent never overwrites.
    let (status, _) = op(
        addr,
        "write",
        json!({"user": USER, "chat": "c1", "history": two, "ifAbsent": true}),
    )
    .await;
    assert_eq!(status, 409);
    let (status, _) = op(
        addr,
        "write",
        json!({"user": USER, "chat": "c1", "history": two, "ifAbsent": true, "baseRevision": "x"}),
    )
    .await;
    assert_eq!(status, 400);
    let (status, _) = op(
        addr,
        "write",
        json!({"user": USER, "chat": "c1", "history": {"a": 1}}),
    )
    .await;
    assert_eq!(status, 400);
}

#[tokio::test]
async fn lone_surrogates_round_trip_and_hash_as_node_does() {
    let (_root, addr) = start().await;
    // Node's JSON.stringify writes a lone surrogate as an escape; it must survive and hash alike.
    let body = format!(r#"{{"user":"{USER}","chat":"c1","history":["qa \ud83d"]}}"#);
    let (status, saved) = call(addr, "/v1/history/write", Some(TOKEN), &body).await;
    assert_eq!(status, 200);
    let want = revision_of(&wtf16::parse(r#"["qa \ud83d"]"#).unwrap()).unwrap();
    assert_eq!(saved["revision"], json!(want));
    let mut s = TcpStream::connect(addr).await.unwrap();
    let read = format!(r#"{{"user":"{USER}","chat":"c1"}}"#);
    s.write_all(format!("POST /v1/history/read HTTP/1.1\r\nhost: s\r\nconnection: close\r\ncontent-length: {}\r\n{TOKEN_HEADER}: {TOKEN}\r\n\r\n{read}", read.len()).as_bytes()).await.unwrap();
    let mut out = Vec::new();
    s.read_to_end(&mut out).await.unwrap();
    assert!(String::from_utf8_lossy(&out).contains(r#""history":["qa \ud83d"]"#));
}

#[tokio::test]
async fn a_deleted_chat_is_never_written_back_by_any_op() {
    let (root, addr) = start().await;
    let dir = user_dir(root.path(), USER);
    std::fs::write(dir.join("deleted-chats.json"), r#"["c-gone"]"#).unwrap();
    let (status, answer) = op(
        addr,
        "write",
        json!({"user": USER, "chat": "c-gone", "history": ["qa late"]}),
    )
    .await;
    assert_eq!(
        (status, answer),
        (
            410,
            json!({"error": "This chat was deleted.", "code": "CHAT_DELETED"})
        )
    );
    let (status, answer) = op(addr, "append-if-last", json!({"user": USER, "chat": "c-gone", "message": "q", "entry": {"role": "assistant", "content": "qa"}})).await;
    assert_eq!((status, answer), (200, json!({"outcome": "deleted"})));
    assert!(!history_file(&dir, "c-gone").exists());
}

#[tokio::test]
async fn a_client_save_leaves_a_running_replys_partial_and_supersedes_a_leftover() {
    let (root, addr) = start().await;
    let dir = user_dir(root.path(), USER);
    let partial = server_projects::partial::file_for(&dir, "c1");
    std::fs::create_dir_all(partial.parent().unwrap()).unwrap();
    std::fs::write(
        &partial,
        format!(r#"{{"content":"qa","running":{{"start":"{START}"}}}}"#),
    )
    .unwrap();
    let save = json!({"user": USER, "chat": "c1", "history": [], "supersedePartial": true});
    assert_eq!(op(addr, "write", save.clone()).await.0, 200);
    assert!(partial.exists(), "the reply still streams in this start");
    std::fs::write(
        &partial,
        r#"{"content":"qa","running":{"start":"ffffffffffffffffffffffffffffffff"}}"#,
    )
    .unwrap();
    assert_eq!(op(addr, "write", save).await.0, 200);
    assert!(
        !partial.exists(),
        "the previous start's partial is superseded"
    );
}

#[tokio::test]
async fn the_interrupted_reply_append_and_a_client_save_never_both_lose() {
    // Many rounds of the race Node's chat turn had with the history save: the transcript ends
    // with the user message; the client saves its own copy of the turn (a CAS from that base) while
    // the turn's interrupted-reply append lands. Exactly one of them wins each round, and the
    // loser learns it (a 409, or `not-last`): the transcript is never a mix and never loses the
    // winner's write.
    let (root, addr) = start().await;
    let data = root.path().to_path_buf();
    for round in 0..40 {
        let chat = format!("c-race-{round}");
        let base = json!([{"role": "user", "content": "q"}]);
        std::fs::write(
            history_file(&user_dir(&data, USER), &chat),
            json!({"history": base}).to_string(),
        )
        .unwrap();
        let client = json!([{"role": "user", "content": "q"}, {"role": "assistant", "content": "qa client copy"}]);
        let (a, b) = tokio::join!(
            op(
                addr,
                "append-if-last",
                json!({"user": USER, "chat": chat, "message": "q", "entry": {"role": "assistant", "content": "qa kept partial", "interrupted": true}})
            ),
            op(
                addr,
                "write",
                json!({"user": USER, "chat": chat, "history": client, "baseRevision": rev(&base)})
            )
        );
        let (_, now) = op(addr, "read", json!({"user": USER, "chat": chat})).await;
        match (a.1["outcome"].as_str().unwrap(), b.0) {
            ("saved", 409) => {
                assert_eq!(now["history"][1]["content"], "qa kept partial", "{round}")
            }
            ("not-last", 200) => assert_eq!(now["history"], client, "{round}"),
            other => panic!("round {round}: {other:?}"),
        }
        assert_eq!(now["history"].as_array().unwrap().len(), 2);
    }
}

#[tokio::test]
async fn concurrent_client_saves_from_many_tabs_lose_nothing() {
    let (_root, addr) = start().await;
    let mut tasks = Vec::new();
    for tab in 0..6 {
        tasks.push(tokio::spawn(async move {
            for i in 0..10 {
                let mut done = false;
                for _ in 0..200 {
                    let (_, now) = op(addr, "read", json!({"user": USER, "chat": "c1"})).await;
                    let mut h = now["history"].as_array().unwrap().clone();
                    h.push(json!(format!("qa tab{tab}-{i}")));
                    let (status, _) = op(addr, "write", json!({"user": USER, "chat": "c1", "history": h, "baseRevision": now["revision"]})).await;
                    if status == 200 {
                        done = true;
                        break;
                    }
                    assert_eq!(status, 409);
                }
                assert!(done, "tab{tab}-{i} gave up");
            }
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    let (_, now) = op(addr, "read", json!({"user": USER, "chat": "c1"})).await;
    assert_eq!(now["history"].as_array().unwrap().len(), 60);
}

const ROUNDS: usize = 60;

/// Node saving the transcript while Rust saves it too. `blind`: Node writes as it did before
/// slice 4 (read once, then `writeFileSync(<file>.tmp)` + rename, no lock, no revision), the
/// negative control. Returns the transcript's entries afterwards.
fn node_race(blind: bool) -> Option<Vec<String>> {
    if std::process::Command::new("node")
        .arg("--version")
        .output()
        .is_err()
    {
        assert!(
            std::env::var("NOEVIA_CORE_CHECKOUT").is_err(),
            "node is required in CI"
        );
        eprintln!("node not on PATH: skipped");
        return None;
    }
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (root, addr) = rt.block_on(start());
    let dir = user_dir(root.path(), USER);
    let file = history_file(&dir, "c1");
    // Node: a view loaded before Rust's first save (Rust waits for the marker), so its first save
    // is from a stale copy; then ROUNDS saves, each adding one entry.
    let script = r#"
      const fs = require('fs');
      const [port, token, user, file, rounds, mode] = process.argv.slice(1);
      const call = async (op, body) => {
        const res = await fetch(`http://127.0.0.1:${port}/v1/history/${op}`, { method: 'POST',
          headers: { 'content-type': 'application/json', 'x-noevia-store-token': token }, body: JSON.stringify(body) });
        return { status: res.status, body: await res.json() };
      };
      (async () => {
        let view, revision;
        if (mode === 'blind') view = (() => { try { return JSON.parse(fs.readFileSync(file, 'utf8')).history; } catch { return []; } })();
        else { const r = await call('read', { user, chat: 'c1' }); view = r.body.history; revision = r.body.revision; }
        fs.writeFileSync(file + '.loaded', '');
        const until = Date.now() + 20000;
        while (!(fs.existsSync(file) && fs.readFileSync(file, 'utf8').includes('"rust-0"'))) {
          if (Date.now() > until) throw new Error('Rust never wrote');
          await new Promise((r) => setTimeout(r, 1));
        }
        for (let i = 0; i < Number(rounds); i++) {
          if (mode === 'blind') {
            view = [...view, 'node-' + i];
            fs.writeFileSync(file + '.tmp', JSON.stringify({ history: view }, null, 2));
            fs.renameSync(file + '.tmp', file);
            continue;
          }
          for (let attempt = 0; ; attempt++) {
            if (attempt > 500) throw new Error('gave up');
            const r = await call('write', { user, chat: 'c1', history: [...view, 'node-' + i], baseRevision: revision });
            if (r.status === 200) { view = [...view, 'node-' + i]; revision = r.body.revision; break; }
            if (r.status !== 409) throw new Error('store answered ' + r.status);
            view = r.body.history; revision = r.body.revision;
          }
        }
      })().catch((e) => { console.error(e); process.exit(1); });
    "#;
    let mut node = std::process::Command::new("node")
        .args([
            "-e",
            script,
            &addr.port().to_string(),
            TOKEN,
            USER,
            file.to_str().unwrap(),
            &ROUNDS.to_string(),
            if blind { "blind" } else { "cas" },
        ])
        .spawn()
        .expect("node");
    let loaded = dir.join("history-c1.json.loaded");
    let until = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while !loaded.exists() {
        assert!(std::time::Instant::now() < until, "node never loaded");
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    // Rust: the history save route's own path, a CAS from what it reads, retried on conflict.
    for i in 0..ROUNDS {
        let mut done = false;
        for _ in 0..500 {
            let now = transcript::read(root.path(), USER, "c1").unwrap();
            let mut h = match now.history {
                JValue::Arr(items) => items,
                _ => Vec::new(),
            };
            h.push(JValue::Str(format!("rust-{i}")));
            if let Saved::Saved { .. } = transcript::write(
                root.path(),
                USER,
                "c1",
                &h,
                &Base::Revision(now.revision),
                None,
            )
            .unwrap()
            {
                done = true;
                break;
            }
        }
        assert!(done, "rust-{i} gave up");
    }
    assert!(node.wait().unwrap().success(), "node side failed");
    let now = transcript::read(root.path(), USER, "c1").unwrap();
    let JValue::Arr(items) = now.history else {
        panic!()
    };
    Some(
        items
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect(),
    )
}

#[test]
fn node_and_rust_saving_through_the_store_lose_nothing() {
    let Some(entries) = node_race(false) else {
        return;
    };
    for who in ["node", "rust"] {
        for i in 0..ROUNDS {
            assert!(
                entries.contains(&format!("{who}-{i}")),
                "{who}-{i} lost: {entries:?}"
            );
        }
    }
    assert_eq!(entries.len(), 2 * ROUNDS);
}

/// The control: Node writing the file itself, as before slice 4, drops what Rust saved, so the
/// test above would catch a store that stopped serialising.
#[test]
fn without_the_store_nodes_saves_drop_rusts() {
    let Some(entries) = node_race(true) else {
        return;
    };
    // Node's view never had rust-0 and it saved after rust-0 was written: rust-0 is gone.
    assert!(!entries.contains(&"rust-0".to_string()), "{entries:?}");
}
