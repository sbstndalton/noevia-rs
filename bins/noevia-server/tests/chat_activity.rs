//! Chat port S3: every authenticated /api/ request the front sees records its account in
//! `UI_DATA_DIR/chat-activity.json` (`{"version":1,"seen":{"<user>":<ms>}}`), the file core's change
//! journal reader merges as request starts (chat-activity.cjs). A user who only uses routes the front
//! answers itself is therefore still read. Signed-out requests, refused sessions and paths outside /api/
//! record nothing. Synthetic data only; Node is not running (the proxy answers 502).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use noevia_server::config::Config;
use noevia_server::serve::{serve, Limits};
use server_store::rusqlite::Connection;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const SCHEMA: &str = include_str!("../../../crates/server-store/tests/fixtures/node-schema.sql");
const UID: &str = "00000000-0000-4000-8000-0000000000a1";
const OTHER: &str = "00000000-0000-4000-8000-0000000000b2";
const SESSION: &str = "synthetic-activity-session";
const OTHER_SESSION: &str = "synthetic-activity-other";
const ORIGIN: &str = "http://127.0.0.1:18021";

struct Front {
    _dir: tempfile::TempDir,
    data: PathBuf,
    addr: SocketAddr,
}

async fn front() -> Front {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().to_path_buf();
    let db = Connection::open(server_store::db_path(&data)).unwrap();
    db.execute_batch(SCHEMA).unwrap();
    let now = noevia_server::identity::now_ms();
    for (id, name, session, disabled) in [
        (UID, "synthetic", SESSION, None::<i64>),
        (OTHER, "disabled", OTHER_SESSION, Some(1)),
    ] {
        db.execute("INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,disabled_at,created_at,updated_at) VALUES(?1,?2,?2,?2,'member','unused',?2,?3,?4,?4)", (id, name, disabled, now)).unwrap();
        db.execute(
            "INSERT INTO sessions VALUES(?1,?2,?3,?4,?4,?5,'synthetic','127.0.0.1')",
            (
                server_auth::js::digest(session),
                id,
                server_auth::js::digest("csrf"),
                now,
                now + 86_400_000,
            ),
        )
        .unwrap();
    }
    let mut env: HashMap<&str, String> = HashMap::new();
    env.insert("NOEVIA_LEGACY_UPSTREAM", "http://127.0.0.1:9".into());
    env.insert("UI_PORT", "1".into());
    env.insert("UI_DATA_DIR", data.display().to_string());
    env.insert("NOEVIA_WEB_DIST", data.join("dist").display().to_string());
    env.insert("PUBLIC_ORIGIN", ORIGIN.into());
    let app = noevia_server::App::new(Config::from_lookup(|k| env.get(k).cloned()).unwrap());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(serve(
        listener,
        app,
        Limits::default(),
        std::future::pending(),
    ));
    Front {
        _dir: dir,
        data,
        addr,
    }
}

async fn get(f: &Front, path: &str, session: Option<&str>) -> u16 {
    let mut c = TcpStream::connect(f.addr).await.unwrap();
    let cookie = session
        .map(|s| format!("cookie: cowork_session={s}; cowork_csrf=csrf\r\n"))
        .unwrap_or_default();
    c.write_all(
        format!("GET {path} HTTP/1.1\r\nhost: front.test\r\nconnection: close\r\n{cookie}\r\n")
            .as_bytes(),
    )
    .await
    .unwrap();
    let mut out = Vec::new();
    c.read_to_end(&mut out).await.unwrap();
    String::from_utf8_lossy(&out[9..12]).parse().unwrap()
}

fn activity(f: &Front) -> Option<serde_json::Value> {
    std::fs::read_to_string(f.data.join("chat-activity.json"))
        .ok()
        .map(|t| serde_json::from_str(&t).unwrap())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_request_to_a_route_the_front_answers_records_the_user_for_nodes_reader() {
    let f = front().await;
    // Nothing yet: signed out, a refused session, and a path outside /api/ record nobody.
    assert_eq!(get(&f, "/api/chat-framing/suggest", None).await, 401);
    assert_eq!(
        get(&f, "/api/chat-framing/suggest", Some(OTHER_SESSION)).await,
        401
    );
    get(&f, "/", Some(SESSION)).await;
    assert_eq!(activity(&f), None);
    // A route the front answers itself (Node never sees it): the user is recorded at once.
    let before = noevia_server::identity::now_ms();
    assert_eq!(
        get(&f, "/api/chat-framing/suggest", Some(SESSION)).await,
        405
    );
    let after = noevia_server::identity::now_ms();
    let doc = activity(&f).expect("the activity file");
    assert_eq!(doc["version"], 1);
    let seen = doc["seen"].as_object().unwrap();
    assert_eq!(seen.keys().collect::<Vec<_>>(), vec![UID]);
    let at = seen[UID].as_i64().unwrap();
    assert!(before <= at && at <= after, "{before} {at} {after}");
    // A request proxied to Node is recorded too (Node also touches it itself).
    assert_eq!(get(&f, "/api/unlisted-for-node", Some(SESSION)).await, 502);
    assert_eq!(activity(&f).unwrap()["seen"].as_object().unwrap().len(), 1);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(f.data.join("chat-activity.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    assert!(noevia_server::FEATURES.contains(&"chat-activity"));
}
