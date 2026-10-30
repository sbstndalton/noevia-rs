//! Google Drive's token broker and the Connectors writes (sbstndalton/noevia#2190 round 7) through
//! the real front and its real internal store listener. Google is a synthetic server on 127.0.0.1
//! (the loopback overrides GOOGLE_OAUTH_BASE_URL and GOOGLE_DRIVE_*_BASE_URL); the legacy upstream is
//! dead, so a request that fell through to Node would fail. No real account, token or Google endpoint.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
#[path = "gdrive_support/mod.rs"]
mod support;
use support::*;

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

const SESSION: &str = "gdrive/session";
const CONNECT: &str = "/api/connectors/gdrive/connect";
const DISCONNECT: &str = "/api/connectors/gdrive/disconnect";
const POLICY: &str = "/api/connectors/gdrive/policy";
const BACKUP_COPY: &str = "/api/connectors/gdrive/backup-copy";

// The golden vector shared with core (server/rust-google-drive.test.cjs): an account's token file
// as Node's gdrive.cjs sealer writes it, with secrets.cjs derive('google-drive-user') of key [7;32].
const NODE_FILE_JSON: &str = r#"{"refreshToken":"synthetic-refresh","email":"ada@example.test","connectedAt":1700000000000,"owner":"aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"}"#;
const NODE_FILE: &str = "AQIDBAUGBwgJCgsMgIgxgDi+UwWrjb2pgZHTVk5Oe2S+sxp0cQhofG56mOyqJu4MKObbx2YjiOIp95XFlLRpoFsgDgJ1/IeS38BvXz/+MZVPwozLiOOvRCG99kgyZhbaVBN47NmtMEBpqgbKbH3pg0zMV/11xgL0BXiHW9DA/DYPs9ifgguMcyvGCrLfU3M/fg5aQqY2xrOOpQtYrhaJqw43qj4B6g==";

async fn session(
    f: &Front,
    user: &str,
    admin: bool,
    rejected: Option<&str>,
) -> (u16, serde_json::Value) {
    let rej = rejected
        .map(|r| format!(r#","rejected":"{r}""#))
        .unwrap_or_default();
    store(
        f.store,
        SESSION,
        STORE_TOKEN,
        &format!(r#"{{"user":"{user}","admin":{admin}{rej}}}"#),
    )
    .await
}

fn refresh_bodies(up: &Upstream) -> Vec<String> {
    up.seen()
        .into_iter()
        .filter(|s| s.path == "/token" && s.body.contains("grant_type=refresh_token"))
        .map(|s| s.body)
        .collect()
}

#[tokio::test]
async fn a_file_node_sealed_is_read_and_its_refresh_token_never_leaves() {
    let d = tempfile::tempdir().unwrap();
    seed(d.path());
    let up = upstream().await;
    let g = install_oauth(&up, "ada@example.test", Duration::ZERO);
    let f = front(d.path(), &up.base(), &[]).await;
    // Node wrote this file (the vector in core's test decrypts it with secrets.cjs): Rust opens it.
    let dir = d.path().join("google-drive-users");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(format!("{A}.sealed")), NODE_FILE).unwrap();
    assert_eq!(
        std::str::from_utf8(account_sealer().open(NODE_FILE).unwrap().as_slice()).unwrap(),
        NODE_FILE_JSON
    );

    let (s, first) = session(&f, A, true, None).await;
    assert_eq!(s, 200, "{first}");
    assert_eq!(first["accessToken"], "access-1");
    assert_eq!(first["generation"].as_str().unwrap().len(), 64);
    assert!(first["expiresAt"].as_i64().unwrap() > chrono_now() + 3_000_000);
    let text = first.to_string();
    assert!(
        !text.contains("synthetic-refresh"),
        "a refresh token never reaches Node: {text}"
    );
    let bodies = refresh_bodies(&up);
    assert_eq!(bodies.len(), 1);
    assert!(
        bodies[0].contains("refresh_token=synthetic-refresh")
            && bodies[0].contains("client_id=synthetic-client")
            && bodies[0].contains("client_secret=synthetic-secret")
    );
    // Cached while it lasts: no second refresh, same token and generation.
    let (_, again) = session(&f, A, true, None).await;
    assert_eq!(again, first);
    assert_eq!(g.refreshes.load(Ordering::SeqCst), 1);
    // The state file Node re-reads says A is connected, and carries no secret or email.
    let st = state_file(d.path());
    assert_eq!(st["personal"][A], "connected");
    assert!(!st.to_string().contains("ada@example.test") && !st.to_string().contains("refresh"));
    let mode = std::os::unix::fs::PermissionsExt::mode(
        &std::fs::metadata(d.path().join("gdrive-connections.json"))
            .unwrap()
            .permissions(),
    );
    assert_eq!(mode & 0o777, 0o600);
}

fn chrono_now() -> i64 {
    noevia_server::identity::now_ms()
}

#[tokio::test]
async fn concurrent_sessions_refresh_once_and_a_rejected_token_refreshes_again() {
    let d = tempfile::tempdir().unwrap();
    seed(d.path());
    let up = upstream().await;
    let g = install_oauth(&up, "ada@example.test", Duration::from_millis(400));
    let f = front(d.path(), &up.base(), &[]).await;
    plant_account_file(
        d.path(),
        A,
        r#"{"refreshToken":"r-1","email":null,"connectedAt":5}"#,
    );
    let mut tasks = Vec::new();
    for _ in 0..6 {
        let (addr, token) = (f.store, STORE_TOKEN);
        tasks.push(tokio::spawn(async move {
            store(
                addr,
                SESSION,
                token,
                &format!(r#"{{"user":"{A}","admin":false}}"#),
            )
            .await
        }));
    }
    let mut tokens = std::collections::HashSet::new();
    for t in tasks {
        let (s, body) = t.await.unwrap();
        assert_eq!(s, 200, "{body}");
        tokens.insert(body["accessToken"].as_str().unwrap().to_string());
    }
    assert_eq!(
        tokens.len(),
        1,
        "one refresh served every waiter: {tokens:?}"
    );
    assert_eq!(g.refreshes.load(Ordering::SeqCst), 1);
    // A 401 on the cached token asks for another; a stale "rejected" (some other token) does not.
    let (_, other) = session(&f, A, false, Some("some-other-token")).await;
    assert_eq!(g.refreshes.load(Ordering::SeqCst), 1, "{other}");
    let (s, next) = session(&f, A, false, Some("access-1")).await;
    assert_eq!(s, 200);
    assert_eq!(next["accessToken"], "access-2");
    assert_eq!(g.refreshes.load(Ordering::SeqCst), 2);
    // The second 401 on a fresh token drops that sign-in's cache (by generation); another is left alone.
    let generation = next["generation"].as_str().unwrap();
    let (_, dropped) = store(
        f.store,
        "gdrive/reject",
        STORE_TOKEN,
        &format!(
            r#"{{"user":"{A}","admin":false,"generation":"{}"}}"#,
            "0".repeat(64)
        ),
    )
    .await;
    assert_eq!(dropped["dropped"], false);
    let (_, dropped) = store(
        f.store,
        "gdrive/reject",
        STORE_TOKEN,
        &format!(r#"{{"user":"{A}","admin":false,"generation":"{generation}"}}"#),
    )
    .await;
    assert_eq!(dropped["dropped"], true);
    let (_, fresh) = session(&f, A, false, None).await;
    assert_eq!(fresh["accessToken"], "access-3");
}

#[tokio::test]
async fn the_refusals_are_typed() {
    let d = tempfile::tempdir().unwrap();
    seed(d.path());
    let up = upstream().await;
    install_oauth(&up, "ada@example.test", Duration::ZERO);
    let f = front(d.path(), &up.base(), &[]).await;
    // No connection at all.
    let (s, b) = session(&f, A, false, None).await;
    assert_eq!(
        (s, b["code"].as_str(), b["status"].as_u64()),
        (409, Some("disconnected"), Some(409)),
        "{b}"
    );
    assert_eq!(
        up.count("/token"),
        0,
        "nothing is asked of Google for a disconnected account"
    );
    // A revoked grant.
    plant_account_file(d.path(), A, r#"{"refreshToken":"r-1","connectedAt":5}"#);
    up.set(|r: &Seen| {
        if r.path == "/token" {
            reply("400 Bad Request", r#"{"error":"invalid_grant"}"#)
        } else {
            reply("404 Not Found", "")
        }
    });
    let (s, b) = session(&f, A, false, None).await;
    assert_eq!((s, b["code"].as_str()), (409, Some("reconnect")), "{b}");
    assert!(b["error"].as_str().unwrap().contains("Connect again"));
    // Google down.
    up.set(|_: &Seen| reply("503 Unavailable", ""));
    let (s, b) = session(&f, A, false, None).await;
    assert_eq!((s, b["code"].as_str()), (502, Some("transient")), "{b}");
    // A file no key opens reads as an error state, never as a token.
    std::fs::write(
        d.path()
            .join("google-drive-users")
            .join(format!("{B}.sealed")),
        "bm90IHNlYWxlZA==",
    )
    .unwrap();
    let (s, b) = session(&f, B, false, None).await;
    assert_eq!((s, b["code"].as_str()), (409, Some("reconnect")), "{b}");
    // Bad input.
    assert_eq!(store(f.store, SESSION, "wrong-token", "{}").await.0, 401);
    assert_eq!(
        store(f.store, SESSION, STORE_TOKEN, "not json").await.0,
        400
    );
    assert_eq!(
        store(f.store, SESSION, STORE_TOKEN, r#"{"user":""}"#)
            .await
            .0,
        400
    );
    let (s, b) = session(&f, "../../etc/passwd", false, None).await;
    assert_eq!(s, 502, "{b}");
    assert_eq!(
        store(f.store, "gdrive/nope", STORE_TOKEN, r#"{"user":"x"}"#)
            .await
            .0,
        404
    );
}

#[tokio::test]
async fn the_device_sign_in_through_the_connectors_routes() {
    let d = tempfile::tempdir().unwrap();
    let db = seed(d.path());
    let up = upstream().await;
    let g = install_oauth(&up, "bob@example.test", Duration::ZERO);
    let f = front(d.path(), &up.base(), &[]).await;
    let addr = f.addr;
    // The gate comes first; nothing outbound happens before it.
    assert_eq!(http(addr, "POST", CONNECT, "", "", b"{}").await.status, 401);
    assert_eq!(
        http(addr, "POST", CONNECT, MEMBER, "", b"{}").await.status,
        403,
        "no CSRF header"
    );
    assert!(up.seen().is_empty());

    let r = send(addr, "POST", CONNECT, MEMBER, "").await;
    assert_eq!(r.status, 200, "{}", r.text);
    let v = r.json();
    assert_eq!(
        (
            v["id"].as_str(),
            v["name"].as_str(),
            v["configured"].as_bool(),
            v["state"].as_str()
        ),
        (
            Some("gdrive"),
            Some("Google Drive"),
            Some(true),
            Some("pending")
        )
    );
    assert_eq!(v["userCode"], "ABCD-EFGH");
    assert_eq!(v["verificationUrl"], "https://www.google.com/device");
    assert!(v["expiresAt"].as_i64().unwrap() > chrono_now());
    assert!(v["backup"].is_null());
    assert_eq!(v["email"], serde_json::Value::Null);
    let names: Vec<&str> = v["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "drive_search_files",
            "drive_read_file",
            "drive_get_metadata",
            "drive_list_recent",
            "drive_create_file",
            "drive_update_file",
            "drive_trash_file"
        ]
    );
    assert_eq!(v["tools"][0]["label"], "Search files");
    assert_eq!(
        (
            v["tools"][0]["write"].as_bool(),
            v["tools"][0]["mode"].as_str()
        ),
        (Some(false), Some("allow"))
    );
    assert_eq!(
        (
            v["tools"][4]["write"].as_bool(),
            v["tools"][4]["mode"].as_str()
        ),
        (Some(true), Some("ask"))
    );
    // A second connect while it is pending gets the same sign-in, without asking Google again.
    let again = send(addr, "POST", CONNECT, MEMBER, "").await.json();
    assert_eq!(again["userCode"], "ABCD-EFGH");
    assert_eq!(up.count("/device/code"), 1);
    let start = &up.seen()[0];
    assert!(
        start.body.contains("client_id=synthetic-client")
            && start.body.contains(
                "scope=https%3A%2F%2Fwww.googleapis.com%2Fauth%2Fdrive.file+openid+email"
            ),
        "{}",
        start.body
    );
    // Not approved yet: still pending, no file.
    assert!(read_account_file(d.path(), B).is_none());

    g.granted.store(1, Ordering::SeqCst);
    let started = Instant::now();
    while read_account_file(d.path(), B).is_none() {
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "the poll never saved the sign-in"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let saved = read_account_file(d.path(), B).unwrap();
    assert_eq!(saved["refreshToken"], "refresh-0");
    assert_eq!(saved["email"], "bob@example.test");
    assert_eq!(saved["owner"], B);
    assert!(saved["connectedAt"].is_number());
    let plain = std::fs::read_to_string(
        d.path()
            .join("google-drive-users")
            .join(format!("{B}.sealed")),
    )
    .unwrap();
    let plain = account_sealer().open(&plain).unwrap();
    let plain = std::str::from_utf8(plain.as_slice()).unwrap();
    assert!(
        plain.starts_with(
            r#"{"refreshToken":"refresh-0","email":"bob@example.test","connectedAt":"#
        ) && plain.ends_with(&format!(r#","owner":"{B}"}}"#)),
        "core's key order: {plain}"
    );
    let raw = std::fs::read_to_string(
        d.path()
            .join("google-drive-users")
            .join(format!("{B}.sealed")),
    )
    .unwrap();
    assert!(!raw.contains("refresh-0") && !raw.contains("bob@example.test"));
    let mode = std::os::unix::fs::PermissionsExt::mode(
        &std::fs::metadata(
            d.path()
                .join("google-drive-users")
                .join(format!("{B}.sealed")),
        )
        .unwrap()
        .permissions(),
    );
    assert_eq!(mode & 0o777, 0o600);
    assert_eq!(state_file(d.path())["personal"][B], "connected");

    let (_, v) = store(
        f.store,
        "gdrive/view",
        STORE_TOKEN,
        &format!(r#"{{"user":"{B}","admin":false}}"#),
    )
    .await;
    assert_eq!(
        (v["state"].as_str(), v["email"].as_str()),
        (Some("connected"), Some("bob@example.test"))
    );
    assert!(v["connectedAt"].is_number());
    // The first access token came with the grant: the broker serves it without a refresh.
    let (_, s1) = session(&f, B, false, None).await;
    assert_eq!(s1["accessToken"], "access-0");
    assert_eq!(g.refreshes.load(Ordering::SeqCst), 0);

    // Disconnect revokes at Google first, then deletes the file; the account's other state is untouched.
    let r = send(addr, "POST", DISCONNECT, MEMBER, "").await;
    assert_eq!(r.status, 200);
    assert_eq!(r.json()["state"], "disconnected");
    assert!(read_account_file(d.path(), B).is_none());
    let revoke = up.seen().into_iter().find(|s| s.path == "/revoke").unwrap();
    assert!(revoke.body.contains("token=refresh-0"));
    assert_eq!(session(&f, B, false, None).await.0, 409);
    assert!(state_file(d.path())["personal"].get(B).is_none());
    drop(db);
}

#[tokio::test]
async fn a_declined_or_expired_code_says_so_and_a_registered_build_is_required() {
    let d = tempfile::tempdir().unwrap();
    seed(d.path());
    let up = upstream().await;
    install_oauth(&up, "x@example.test", Duration::ZERO);
    let f = front(d.path(), &up.base(), &[]).await;
    // Google refuses the grant: the account sees why on its next look.
    let base = up.clone();
    up.set(move |r: &Seen| match r.path.as_str() {
        "/device/code" => reply("200 OK", r#"{"device_code":"d","user_code":"WXYZ","verification_uri":"https://g.example/device","expires_in":1800,"interval":1}"#),
        "/token" => reply("400 Bad Request", r#"{"error":"access_denied"}"#),
        _ => reply("404 Not Found", ""),
    });
    let _ = base;
    let v = send(f.addr, "POST", CONNECT, MEMBER, "").await.json();
    assert_eq!(v["state"], "pending");
    assert_eq!(
        v["verificationUrl"], "https://g.example/device",
        "verification_uri is read too"
    );
    let started = Instant::now();
    loop {
        let (_, v) = store(
            f.store,
            "gdrive/view",
            STORE_TOKEN,
            &format!(r#"{{"user":"{B}","admin":false}}"#),
        )
        .await;
        if v["state"] == "disconnected" {
            assert_eq!(
                v["message"],
                "Access was declined on Google’s page. Connect again to retry."
            );
            break;
        }
        assert!(started.elapsed() < Duration::from_secs(20), "{v}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    // A build with no Google client registered.
    let d2 = tempfile::tempdir().unwrap();
    seed(d2.path());
    let f2 = front(
        d2.path(),
        &up.base(),
        &[("GOOGLE_OAUTH_CLIENT_ID", String::new())],
    )
    .await;
    let r = send(f2.addr, "POST", CONNECT, MEMBER, "").await;
    assert_eq!(
        (r.status, r.json()["error"].as_str().map(str::to_owned)),
        (
            409,
            Some("This noevia build has no Google sign-in registered.".into())
        )
    );
    let (_, v) = store(
        f2.store,
        "gdrive/view",
        STORE_TOKEN,
        &format!(r#"{{"user":"{B}","admin":false}}"#),
    )
    .await;
    assert_eq!(
        (v["configured"].as_bool(), v["state"].as_str()),
        (Some(false), Some("not-configured"))
    );
}

#[tokio::test]
async fn a_disconnect_never_waits_for_a_refresh_and_the_refresh_is_not_kept() {
    let d = tempfile::tempdir().unwrap();
    seed(d.path());
    let up = upstream().await;
    let g = install_oauth(&up, "ada@example.test", Duration::from_millis(1500));
    let f = front(d.path(), &up.base(), &[]).await;
    plant_account_file(d.path(), A, r#"{"refreshToken":"r-1","connectedAt":5}"#);
    let (addr, token) = (f.store, STORE_TOKEN);
    let slow = tokio::spawn(async move {
        store(
            addr,
            SESSION,
            token,
            &format!(r#"{{"user":"{A}","admin":false}}"#),
        )
        .await
    });
    up.wait_for("/token", 1).await;
    let started = Instant::now();
    let (s, b) = store(
        f.store,
        "gdrive/disconnect",
        STORE_TOKEN,
        &format!(r#"{{"user":"{A}","admin":false}}"#),
    )
    .await;
    assert_eq!(s, 200, "{b}");
    assert!(
        started.elapsed() < Duration::from_millis(1200),
        "the disconnect waited for the refresh: {:?}",
        started.elapsed()
    );
    assert!(read_account_file(d.path(), A).is_none());
    let (s, b) = slow.await.unwrap();
    assert_eq!(
        (s, b["code"].as_str()),
        (409, Some("disconnected")),
        "the refresh's result is not served: {b}"
    );
    assert_eq!(g.refreshes.load(Ordering::SeqCst), 1);
    assert_eq!(
        session(&f, A, false, None).await.0,
        409,
        "and not cached for the next caller"
    );
}

#[tokio::test]
async fn a_newer_sign_in_during_a_revoke_is_left_alone() {
    let d = tempfile::tempdir().unwrap();
    seed(d.path());
    let up = upstream().await;
    install_oauth(&up, "ada@example.test", Duration::ZERO);
    let f = front(d.path(), &up.base(), &[]).await;
    plant_account_file(d.path(), A, r#"{"refreshToken":"old","connectedAt":5}"#);
    // The revoke is slow; while it is in flight the account signs in again (the file is replaced).
    up.set(|r: &Seen| {
        if r.path == "/revoke" {
            (Duration::from_millis(1200), reply("200 OK", "{}").1)
        } else {
            reply("404 Not Found", "")
        }
    });
    let (addr, token) = (f.store, STORE_TOKEN);
    let revoking = tokio::spawn(async move {
        store(
            addr,
            "gdrive/disconnect",
            token,
            &format!(r#"{{"user":"{A}","admin":false}}"#),
        )
        .await
    });
    up.wait_for("/revoke", 1).await;
    plant_account_file(d.path(), A, r#"{"refreshToken":"new","connectedAt":6}"#);
    assert_eq!(revoking.await.unwrap().0, 200);
    assert_eq!(
        read_account_file(d.path(), A).unwrap()["refreshToken"],
        "new",
        "the newer sign-in survived the older disconnect"
    );
}

#[tokio::test]
async fn a_client_that_leaves_mid_request_strands_nothing() {
    let d = tempfile::tempdir().unwrap();
    seed(d.path());
    let up = upstream().await;
    let g = install_oauth(&up, "bob@example.test", Duration::ZERO);
    let f = front(d.path(), &up.base(), &[]).await;
    // Google is slow to start the sign-in; the browser closes the tab right after sending.
    let slow_up = up.clone();
    slow_up.set(|r: &Seen| match r.path.as_str() {
        "/device/code" => (Duration::from_millis(900), reply("200 OK", r#"{"device_code":"d","user_code":"SLOW-CODE","verification_url":"https://g.example/device","expires_in":1800,"interval":1}"#).1),
        "/token" => reply("428 Precondition Required", r#"{"error":"authorization_pending"}"#),
        _ => reply("404 Not Found", ""),
    });
    {
        use tokio::io::AsyncWriteExt;
        let mut s = tokio::net::TcpStream::connect(f.addr).await.unwrap();
        s.write_all(format!("POST {CONNECT} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nCookie: {MEMBER}\r\nOrigin: http://{}\r\nX-CSRF-Token: csrf\r\nContent-Length: 0\r\n\r\n", f.addr, f.addr).as_bytes()).await.unwrap();
        up.wait_for("/device/code", 1).await;
        drop(s);
    }
    let started = Instant::now();
    loop {
        let (_, v) = store(
            f.store,
            "gdrive/view",
            STORE_TOKEN,
            &format!(r#"{{"user":"{B}","admin":false}}"#),
        )
        .await;
        if v["state"] == "pending" {
            assert_eq!(v["userCode"], "SLOW-CODE");
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "the sign-in was stranded: {v}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // ... and the poll that follows still completes the sign-in once Google approves.
    let up2 = up.clone();
    let _ = (up2, &g);
    up.set(|r: &Seen| match r.path.as_str() {
        "/token" => reply(
            "200 OK",
            r#"{"access_token":"a","refresh_token":"rr","expires_in":3600}"#,
        ),
        _ => reply("404 Not Found", ""),
    });
    let started = Instant::now();
    while read_account_file(d.path(), B).is_none() {
        assert!(started.elapsed() < Duration::from_secs(20));
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // A disconnect whose client leaves mid-revoke still deletes the file.
    up.set(|r: &Seen| {
        if r.path == "/revoke" {
            (Duration::from_millis(800), reply("200 OK", "{}").1)
        } else {
            reply("404 Not Found", "")
        }
    });
    {
        use tokio::io::AsyncWriteExt;
        let mut s = tokio::net::TcpStream::connect(f.addr).await.unwrap();
        s.write_all(format!("POST {DISCONNECT} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nCookie: {MEMBER}\r\nOrigin: http://{}\r\nX-CSRF-Token: csrf\r\nContent-Length: 0\r\n\r\n", f.addr, f.addr).as_bytes()).await.unwrap();
        up.wait_for("/revoke", 1).await;
        drop(s);
    }
    let started = Instant::now();
    while read_account_file(d.path(), B).is_some() {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the file outlived the dropped request"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test]
async fn forgetting_a_deleted_account_revokes_and_removes_only_its_file() {
    let d = tempfile::tempdir().unwrap();
    seed(d.path());
    let up = upstream().await;
    install_oauth(&up, "x@example.test", Duration::ZERO);
    let f = front(d.path(), &up.base(), &[]).await;
    plant_account_file(d.path(), A, r#"{"refreshToken":"ra","connectedAt":5}"#);
    plant_account_file(d.path(), B, r#"{"refreshToken":"rb","connectedAt":5}"#);
    let (s, b) = store(
        f.store,
        "gdrive/forget",
        STORE_TOKEN,
        &format!(r#"{{"user":"{A}"}}"#),
    )
    .await;
    assert_eq!((s, b["ok"].as_bool()), (200, Some(true)));
    assert!(read_account_file(d.path(), A).is_none());
    assert_eq!(
        read_account_file(d.path(), B).unwrap()["refreshToken"],
        "rb"
    );
    assert!(up
        .seen()
        .iter()
        .any(|s| s.path == "/revoke" && s.body.contains("token=ra")));
    // A hostile id touches nothing outside the directory and is not an error.
    std::fs::write(d.path().join("keep.sealed"), "x").unwrap();
    let (s, _) = store(
        f.store,
        "gdrive/forget",
        STORE_TOKEN,
        r#"{"user":"../keep"}"#,
    )
    .await;
    assert_eq!(s, 200);
    assert!(d.path().join("keep.sealed").exists());
    // Forgetting an account that never connected is a no-op.
    assert_eq!(
        store(
            f.store,
            "gdrive/forget",
            STORE_TOKEN,
            &format!(r#"{{"user":"{C}"}}"#)
        )
        .await
        .0,
        200
    );
}

#[tokio::test]
async fn tool_permissions_are_the_fronts_single_write() {
    let d = tempfile::tempdir().unwrap();
    let db = seed(d.path());
    let up = upstream().await;
    install_oauth(&up, "x@example.test", Duration::ZERO);
    let f = front(d.path(), &up.base(), &[]).await;
    let put = |body: &str| {
        let body = body.to_string();
        let addr = f.addr;
        async move { send(addr, "PUT", POLICY, MEMBER, &body).await }
    };
    assert_eq!(http(f.addr, "PUT", POLICY, "", "", b"{}").await.status, 401);
    let r = put(r#"{"tools":["drive_search_files","drive_read_file","bogus"],"mode":"ask"}"#).await;
    assert_eq!(r.status, 200, "{}", r.text);
    let v = r.json();
    assert_eq!(v["tools"][0]["mode"], "ask");
    assert_eq!(v["tools"][1]["mode"], "ask");
    assert_eq!(
        v["tools"][2]["mode"], "allow",
        "an untouched read keeps its default"
    );
    let rows: Vec<(String, String)> = db
        .prepare("SELECT tool,mode FROM tool_policies WHERE user_id=?1 ORDER BY tool")
        .unwrap()
        .query_map([B], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(
        rows,
        [
            ("drive_read_file".into(), "ask".into()),
            ("drive_search_files".into(), "ask".into())
        ],
        "a name that is not a Drive tool is dropped"
    );
    let (action, actor, target, detail): (String, String, String, String) = db.query_row("SELECT action,actor_user_id,target_user_id,detail FROM audit_events WHERE action='tool.policy'", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).unwrap();
    assert_eq!(
        (action.as_str(), actor.as_str(), target.as_str()),
        ("tool.policy", B, B)
    );
    assert_eq!(
        detail, r#"{"tools":["drive_search_files","drive_read_file"],"mode":"ask"}"#,
        "core's audit detail, including every tool the account named"
    );
    // Block, then allow on a read.
    assert_eq!(
        put(r#"{"tools":["drive_list_recent"],"mode":"block"}"#)
            .await
            .json()["tools"][3]["mode"],
        "block"
    );
    assert_eq!(
        put(r#"{"tools":["drive_list_recent"],"mode":"allow"}"#)
            .await
            .json()["tools"][3]["mode"],
        "allow"
    );
    // Writes always ask first; the refusal is core's, and nothing is written.
    let before: i64 = db
        .query_row("SELECT count(*) FROM tool_policies", [], |r| r.get(0))
        .unwrap();
    for (body, message) in [
        (
            r#"{"tools":["drive_create_file"],"mode":"allow"}"#,
            "Writes always ask first, so they cannot be set to Always allow.",
        ),
        (
            r#"{"tools":["drive_search_files","drive_trash_file"],"mode":"allow"}"#,
            "Writes always ask first, so they cannot be set to Always allow.",
        ),
        (
            r#"{"tools":["drive_search_files"],"mode":"sometimes"}"#,
            "Choose allow, ask or block.",
        ),
        (
            r#"{"tools":["drive_search_files"]}"#,
            "Choose allow, ask or block.",
        ),
        (r#"{"tools":["bogus"],"mode":"ask"}"#, "Choose a tool."),
        (r#"{"mode":"ask"}"#, "Choose a tool."),
    ] {
        let r = put(body).await;
        assert_eq!(
            (r.status, r.json()["error"].as_str().map(str::to_owned)),
            (400, Some(message.into())),
            "{body}"
        );
    }
    assert_eq!(
        db.query_row::<i64, _, _>("SELECT count(*) FROM tool_policies", [], |r| r.get(0))
            .unwrap(),
        before
    );
    // A write stored as `allow` before reads back as ask (core: never weaker).
    db.execute(
        "INSERT OR REPLACE INTO tool_policies VALUES(?1,'drive_trash_file','allow',1)",
        [B],
    )
    .unwrap();
    let (_, v) = store(
        f.store,
        "gdrive/view",
        STORE_TOKEN,
        &format!(r#"{{"user":"{B}","admin":false}}"#),
    )
    .await;
    assert_eq!(v["tools"][6]["mode"], "ask");
    // Bodies: not JSON, not an object, too large.
    assert_eq!(put("nope").await.status, 400);
    assert_eq!(put("[1]").await.status, 400);
    assert_eq!(
        put(&format!(r#"{{"x":"{}"}}"#, "a".repeat(1_100_000)))
            .await
            .status,
        413
    );
    // The Nextcloud half stays Node's, but its permissions are written here, on Node's behalf, with
    // Node's write flags (it knows the live MCP tools; this process does not).
    let (s, b) = store(f.store, "gdrive/policy", STORE_TOKEN, &format!(r#"{{"user":"{B}","tools":["nc_read","nc_write"],"writes":[false,true],"mode":"block"}}"#)).await;
    assert_eq!((s, b["ok"].as_bool()), (200, Some(true)), "{b}");
    assert_eq!(
        db.query_row::<String, _, _>(
            "SELECT mode FROM tool_policies WHERE user_id=?1 AND tool='nc_write'",
            [B],
            |r| r.get(0)
        )
        .unwrap(),
        "block"
    );
    let (s, b) = store(f.store, "gdrive/policy", STORE_TOKEN, &format!(r#"{{"user":"{B}","tools":["nc_read","nc_write"],"writes":[false,true],"mode":"allow"}}"#)).await;
    assert_eq!(
        (s, b["error"].as_str()),
        (
            400,
            Some("Writes always ask first, so they cannot be set to Always allow.")
        )
    );
    // Mismatched flags fail closed.
    let (s, _) = store(
        f.store,
        "gdrive/policy",
        STORE_TOKEN,
        &format!(r#"{{"user":"{B}","tools":["nc_read"],"writes":[],"mode":"allow"}}"#),
    )
    .await;
    assert_eq!(s, 400);
    // Only an administrator whose Drive holds backups can change the copy setting.
    let r = send(f.addr, "PUT", BACKUP_COPY, MEMBER, r#"{"enabled":false}"#).await;
    assert_eq!(
        (r.status, r.json()["error"].as_str().map(str::to_owned)),
        (
            403,
            Some("Only the administrator whose Drive holds the backups can change this.".into())
        )
    );
    let r = send(f.addr, "PUT", BACKUP_COPY, ADMIN, r#"{"enabled":false}"#).await;
    assert_eq!(
        r.status, 403,
        "the backup folder is not configured, so an administrator's Drive is personal"
    );
}

#[tokio::test]
async fn the_backup_slot_doubles_as_one_administrators_drive() {
    let d = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    seed(d.path());
    let backups = root.path().join("backups");
    std::fs::create_dir_all(&backups).unwrap();
    let key = root.path().join("backup.key");
    std::fs::write(&key, format!("{BACKUP_KEY_HEX}\n")).unwrap();
    let up = upstream().await;
    let g = install_oauth(&up, "admin@example.test", Duration::ZERO);
    let env = [
        ("OFFSITE_BACKUP_DIR", backups.display().to_string()),
        ("OFFSITE_BACKUP_KEY_FILE", key.display().to_string()),
        ("NOEVIA_FEATURE_OFFSITE_BACKUP", "1".to_string()),
    ];
    let f = front(d.path(), &up.base(), &env).await;
    // The first administrator to connect from Connectors fills the backup slot.
    let v = send(f.addr, "POST", CONNECT, ADMIN, "").await.json();
    assert_eq!(v["state"], "pending", "{v}");
    assert!(v["backup"].is_object(), "{v}");
    assert_eq!(v["backup"]["enabled"], true);
    assert_eq!(v["backup"]["copyEnabled"], true);
    // While it is pending, another administrator neither sees its code nor shares the slot: theirs is personal.
    let (_, other) = store(
        f.store,
        "gdrive/view",
        STORE_TOKEN,
        &format!(r#"{{"user":"{C}","admin":true}}"#),
    )
    .await;
    assert_eq!(other["state"], "disconnected", "{other}");
    assert!(other["backup"].is_null());
    assert!(other.get("userCode").is_none());
    g.granted.store(1, Ordering::SeqCst);
    let started = Instant::now();
    while read_backup_file(d.path()).is_none() {
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "no backup slot file"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let saved = read_backup_file(d.path()).unwrap();
    assert_eq!(
        (saved["refreshToken"].as_str(), saved["owner"].as_str()),
        (Some("refresh-0"), Some(A)),
        "sealed under the backup key, owned by the administrator who made it"
    );
    assert!(
        read_account_file(d.path(), A).is_none(),
        "no personal file for A"
    );
    assert_eq!(state_file(d.path())["backup"]["state"], "connected");
    assert_eq!(state_file(d.path())["backup"]["owner"], A);
    // A's chat tools use the backup slot; C's, a second administrator's, do not; a member's neither.
    let (s, tok) = session(&f, A, true, None).await;
    assert_eq!((s, tok["accessToken"].as_str()), (200, Some("access-0")));
    assert_eq!(
        session(&f, C, true, None).await.0,
        409,
        "C has no connection of their own"
    );
    assert_eq!(session(&f, B, false, None).await.0, 409);
    // The connector page shows A the backup block; the backup-copy toggle is A's alone.
    let (_, view) = store(
        f.store,
        "gdrive/view",
        STORE_TOKEN,
        &format!(r#"{{"user":"{A}","admin":true}}"#),
    )
    .await;
    assert_eq!(
        (
            view["state"].as_str(),
            view["backup"]["copyEnabled"].as_bool()
        ),
        (Some("connected"), Some(true))
    );
    let r = send(f.addr, "PUT", BACKUP_COPY, ADMIN, r#"{"enabled":false}"#).await;
    assert_eq!(r.status, 200, "{}", r.text);
    assert_eq!(r.json()["backup"]["copyEnabled"], false);
    assert_eq!(
        send(f.addr, "PUT", BACKUP_COPY, ADMIN2, r#"{"enabled":true}"#)
            .await
            .status,
        403
    );
    // C connecting from Connectors gets a personal connection (the slot is A's).
    let mine = send(f.addr, "POST", CONNECT, ADMIN2, "").await.json();
    assert_eq!(mine["state"], "pending");
    assert!(mine["backup"].is_null());
    // A disconnects from Connectors: the slot file goes (the backup copy to Drive stops).
    let r = send(f.addr, "POST", DISCONNECT, ADMIN, "").await;
    assert_eq!(r.status, 200);
    assert!(read_backup_file(d.path()).is_none());
    assert_eq!(state_file(d.path())["backup"]["state"], "disconnected");
}

#[tokio::test]
async fn a_connection_saved_before_owners_were_recorded_belongs_to_whichever_administrator_opens_it(
) {
    let d = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    seed(d.path());
    let backups = root.path().join("backups");
    std::fs::create_dir_all(&backups).unwrap();
    let key = root.path().join("backup.key");
    std::fs::write(&key, BACKUP_KEY_HEX).unwrap();
    let up = upstream().await;
    install_oauth(&up, "x@example.test", Duration::ZERO);
    let env = [
        ("OFFSITE_BACKUP_DIR", backups.display().to_string()),
        ("OFFSITE_BACKUP_KEY_FILE", key.display().to_string()),
    ];
    let f = front(d.path(), &up.base(), &env).await;
    std::fs::write(
        d.path().join("google-drive.sealed"),
        backup_sealer()
            .seal(
                br#"{"refreshToken":"legacy","email":"old@example.test","connectedAt":5}"#,
                &[4; 12],
            )
            .unwrap(),
    )
    .unwrap();
    for admin in [A, C] {
        let (s, t) = session(&f, admin, true, None).await;
        assert_eq!(s, 200, "{t}");
    }
    assert_eq!(
        session(&f, B, false, None).await.0,
        409,
        "a member never reaches it"
    );
    // A key that does not open the slot reads as an unreadable connection, for its owner too.
    std::fs::write(&key, "01".repeat(32)).unwrap();
    let d2 = tempfile::tempdir().unwrap();
    seed(d2.path());
    let f2 = front(d2.path(), &up.base(), &env).await;
    std::fs::write(
        d2.path().join("google-drive.sealed"),
        backup_sealer()
            .seal(br#"{"refreshToken":"legacy","connectedAt":5}"#, &[4; 12])
            .unwrap(),
    )
    .unwrap();
    let (_, view) = store(
        f2.store,
        "gdrive/view",
        STORE_TOKEN,
        &format!(r#"{{"user":"{A}","admin":true}}"#),
    )
    .await;
    assert_eq!(view["state"], "error", "{view}");
}

#[tokio::test]
async fn the_state_file_exists_only_once_there_is_something_to_say() {
    let d = tempfile::tempdir().unwrap();
    seed(d.path());
    let up = upstream().await;
    install_oauth(&up, "x@example.test", Duration::ZERO);
    let f = front(d.path(), &up.base(), &[]).await;
    let drive = std::sync::Arc::clone(f.app.drive.as_ref().unwrap());
    let state = d.path().join("gdrive-connections.json");
    drive.publish_startup();
    assert!(
        !state.exists(),
        "nobody is connected: no file (Node reads a missing file as nobody)"
    );
    // Asking for an unconnected account's Drive makes core's directory, and no state file.
    let (s, v) = store(
        f.store,
        "gdrive/view",
        STORE_TOKEN,
        &format!(r#"{{"user":"{B}","admin":false}}"#),
    )
    .await;
    assert_eq!((s, v["state"].as_str()), (200, Some("disconnected")));
    assert!(d.path().join("google-drive-users").is_dir() && !state.exists());
    let mode = std::os::unix::fs::PermissionsExt::mode(
        &std::fs::metadata(d.path().join("google-drive-users"))
            .unwrap()
            .permissions(),
    );
    assert_eq!(mode & 0o777, 0o700);
    // A connection Node saved before (or a stale file): the first publish of a start says so.
    plant_account_file(d.path(), B, r#"{"refreshToken":"r","connectedAt":5}"#);
    drive.publish_startup();
    assert_eq!(state_file(d.path())["personal"][B], "connected");
    // After that every committed change publishes, whatever the start did.
    let (s, _) = store(
        f.store,
        "gdrive/disconnect",
        STORE_TOKEN,
        &format!(r#"{{"user":"{B}","admin":false}}"#),
    )
    .await;
    assert_eq!(s, 200);
    assert!(state_file(d.path())["personal"].get(B).is_none());
}
