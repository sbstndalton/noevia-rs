//! M3 session upkeep in the front (NOEVIA_RUST_AUTH=1): every request the front proxies to Node
//! moves a live session's `last_seen_at` and deletes a rejected session row, as Node's gate did;
//! the bundle served by the front is left alone; with the switch off nothing is written.
//! Synthetic data only.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::{Request, Response};
use noevia_server::config::Config;
use noevia_server::serve::{serve, Limits};
use server_store::rusqlite::Connection;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;
use tokio::net::{TcpListener, TcpStream};

const SCHEMA: &str = include_str!("../../../crates/server-store/tests/fixtures/node-schema.sql");

fn hex(s: &str) -> String {
    Sha256::digest(s.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn now() -> i64 {
    noevia_server::identity::now_ms()
}

fn seed(dir: &Path) -> Connection {
    let c = Connection::open(server_store::db_path(dir)).unwrap();
    c.execute_batch(SCHEMA).unwrap();
    c.execute("INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,created_at,updated_at) VALUES('u-1','A','a','A','member','x','w',1,1)", []).unwrap();
    let n = now();
    for (raw, last_seen) in [("live", n - 60_000), ("idle", n - 8 * 86_400_000)] {
        c.execute(
            "INSERT INTO sessions VALUES(?1,'u-1','c',?2,?2,?3,'ua','127.0.0.1')",
            (hex(raw), last_seen, n + 86_400_000),
        )
        .unwrap();
    }
    c
}

fn last_seen(c: &Connection, raw: &str) -> Option<i64> {
    c.query_row(
        "SELECT last_seen_at FROM sessions WHERE id_hash=?1",
        [hex(raw)],
        |r| r.get(0),
    )
    .ok()
}

async fn upstream() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let svc = hyper::service::service_fn(|_r: Request<Incoming>| async {
                    Ok::<_, std::convert::Infallible>(
                        Response::builder()
                            .header("x-upstream", "1")
                            .body(Full::new(Bytes::from_static(b"{}")))
                            .unwrap(),
                    )
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(hyper_util::rt::TokioIo::new(stream), svc)
                    .await;
            });
        }
    });
    addr
}

async fn front(up: SocketAddr, data: &Path, dist: &Path, rust_auth: bool) -> SocketAddr {
    let mut env: HashMap<&str, String> = HashMap::new();
    env.insert("NOEVIA_LEGACY_UPSTREAM", format!("http://{up}"));
    env.insert("UI_PORT", "1".into());
    env.insert("UI_DATA_DIR", data.display().to_string());
    env.insert("NOEVIA_WEB_DIST", dist.display().to_string());
    if rust_auth {
        env.insert("NOEVIA_RUST_AUTH", "1".into());
    }
    let config = Config::from_lookup(|k| env.get(k).cloned()).unwrap();
    let app = noevia_server::App::new(config);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(serve(
        listener,
        app,
        Limits::default(),
        std::future::pending(),
    ));
    addr
}

async fn get(addr: SocketAddr, path: &str, cookie: &str) -> Response<Incoming> {
    let stream = TcpStream::connect(addr).await.unwrap();
    let (mut send, conn) =
        hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(stream))
            .await
            .unwrap();
    tokio::spawn(conn);
    let req = Request::builder()
        .uri(path)
        .header("host", "front.test")
        .header("cookie", cookie)
        .body(Full::new(Bytes::new()).boxed())
        .unwrap();
    let res = send.send_request(req).await.unwrap();
    let _ = res.headers().clone();
    res
}

fn dist(dir: &Path) -> std::path::PathBuf {
    let d = dir.join("dist");
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(d.join("index.html"), "<!doctype html>").unwrap();
    d
}

#[tokio::test]
async fn the_front_keeps_sessions_alive_like_node() {
    let data = tempfile::tempdir().unwrap();
    let node = seed(data.path());
    let before = last_seen(&node, "live").unwrap();
    let addr = front(upstream().await, data.path(), &dist(data.path()), true).await;

    // The bundle is the front's own: no upkeep, as since M1.
    let res = get(addr, "/", "cowork_session=live").await;
    assert!(res.headers().get("x-upstream").is_none());
    let _ = res.into_body().collect().await;
    assert_eq!(last_seen(&node, "live"), Some(before));

    let res = get(addr, "/api/projects", "cowork_session=live").await;
    assert!(res.headers().get("x-upstream").is_some());
    let _ = res.into_body().collect().await;
    let after = last_seen(&node, "live").unwrap();
    assert!(after > before, "last_seen_at did not move");

    // A rejected session's row goes, before Node sees the request.
    let res = get(addr, "/about", "cowork_session=idle").await;
    assert!(res.headers().get("x-upstream").is_some());
    let _ = res.into_body().collect().await;
    assert_eq!(last_seen(&node, "idle"), None);
}

#[tokio::test]
async fn without_the_switch_nothing_is_written() {
    let data = tempfile::tempdir().unwrap();
    let node = seed(data.path());
    let before = last_seen(&node, "live").unwrap();
    let addr = front(upstream().await, data.path(), &dist(data.path()), false).await;
    for (path, cookie) in [
        ("/api/projects", "cowork_session=live"),
        ("/api/projects", "cowork_session=idle"),
    ] {
        let res = get(addr, path, cookie).await;
        assert!(res.headers().get("x-upstream").is_some());
        let _ = res.into_body().collect().await;
    }
    assert_eq!(last_seen(&node, "live"), Some(before));
    assert!(last_seen(&node, "idle").is_some());
}
