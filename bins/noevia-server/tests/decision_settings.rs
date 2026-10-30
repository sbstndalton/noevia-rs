//! /api/admin/decision-settings and /test through the real front (sbstndalton/noevia#2190). The
//! legacy upstream is dead, so a request that fell through to Node would fail; the health
//! endpoints are synthetic loopback servers.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
use noevia_server::{
    config::Config,
    serve::{serve, Limits},
    App,
};
use server_store::rusqlite::Connection;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot,
};
const A: &str = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
const B: &str = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";
const P: &str = "/api/admin/decision-settings";
const T: &str = "/api/admin/decision-settings/test";

fn hex(s: &str) -> String {
    Sha256::digest(s.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

async fn call(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    cookie: &str,
    extra: &str,
    body: &[u8],
) -> (u16, String) {
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nCookie: {cookie}\r\nContent-Length: {}\r\n{extra}\r\n", body.len()).as_bytes()).await.unwrap();
    let _ = s.write_all(body).await;
    let mut bytes = Vec::new();
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        s.read_to_end(&mut bytes),
    )
    .await
    .unwrap();
    let text = String::from_utf8(bytes).unwrap();
    let (h, b) = text.split_once("\r\n\r\n").unwrap();
    let lower = h.to_ascii_lowercase();
    assert!(lower.contains("content-type: application/json"), "{h}");
    assert!(lower.contains("cache-control: no-store"), "{h}");
    (
        h.split_whitespace().nth(1).unwrap().parse().unwrap(),
        b.to_string(),
    )
}

/// A one-route loopback server answering every request with `reply` (status line onwards).
async fn upstream(reply: String) -> std::net::SocketAddr {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = l.accept().await else {
                return;
            };
            let reply = reply.clone();
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf).await;
                let _ = s.write_all(reply.as_bytes()).await;
            });
        }
    });
    addr
}

fn ok_json(body: &str) -> String {
    format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
}

struct Front {
    addr: std::net::SocketAddr,
    stop: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<()>,
}

async fn front(dir: &std::path::Path) -> Front {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let env = HashMap::from([
        ("NOEVIA_LEGACY_UPSTREAM", "http://127.0.0.1:9".to_string()),
        ("UI_DATA_DIR", dir.display().to_string()),
        ("PUBLIC_ORIGIN", format!("http://{addr}")),
        ("COWORK_DECISION_URL", "http://laya:8040".to_string()),
    ]);
    let app = App::new(Config::from_lookup(|k| env.get(k).cloned()).unwrap());
    let (tx, rx) = oneshot::channel();
    let task = tokio::spawn(serve(listener, app, Limits::default(), async {
        let _ = rx.await;
    }));
    Front {
        addr,
        stop: Some(tx),
        task,
    }
}

impl Front {
    async fn stop(mut self) {
        let _ = self.stop.take().unwrap().send(());
        let _ = self.task.await;
    }
}

fn seed(dir: &std::path::Path) -> Connection {
    let db = Connection::open(dir.join("cowork.db")).unwrap();
    db.execute_batch(include_str!(
        "../../../crates/server-store/tests/fixtures/node-schema.sql"
    ))
    .unwrap();
    let now = noevia_server::identity::now_ms();
    for (uid, role, t) in [(A, "admin", "admin-token"), (B, "member", "member-token")] {
        db.execute("INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,created_at,updated_at) VALUES(?1,?1,?1,'Synthetic',?2,'x',?1,1,1)", (uid, role)).unwrap();
        db.execute(
            "INSERT INTO sessions VALUES(?1,?2,?3,?4,?4,?5,'ua','127.0.0.1')",
            (hex(t), uid, hex("csrf"), now, now + 86400000),
        )
        .unwrap();
    }
    db
}

fn stored(db: &Connection) -> Option<String> {
    db.query_row(
        "SELECT value FROM settings WHERE key='decision:configuration'",
        [],
        |r| r.get(0),
    )
    .ok()
}

fn audits(db: &Connection) -> Vec<(String, String, String, String)> {
    let mut st = db.prepare("SELECT actor_user_id,target_user_id,action,detail FROM audit_events WHERE action LIKE 'decision.%' ORDER BY id").unwrap();
    st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

const ADMIN: &str = "cowork_session=admin-token; cowork_csrf=csrf";
const MEMBER: &str = "cowork_session=member-token; cowork_csrf=csrf";

#[tokio::test]
async fn decision_settings_gate_save_reset_machine_and_restart() {
    let d = tempfile::tempdir().unwrap();
    let db = seed(d.path());
    let f = front(d.path()).await;
    let addr = f.addr;
    let csrf = format!("Origin: http://{addr}\r\nX-CSRF-Token: csrf\r\n");

    // Gate: session, admin, CSRF, then the 405s.
    assert_eq!(call(addr, "GET", P, "", "", b"").await.0, 401);
    assert_eq!(
        call(addr, "GET", P, MEMBER, "", b"").await,
        (403, r#"{"error":"Administrator required"}"#.into())
    );
    assert_eq!(call(addr, "PUT", P, ADMIN, "", b"{}").await.0, 403);
    for (m, p) in [
        ("POST", P),
        ("HEAD", P),
        ("PATCH", P),
        ("GET", T),
        ("PUT", T),
        ("DELETE", T),
    ] {
        let (s, b) = call(addr, m, p, ADMIN, &csrf, b"").await;
        assert_eq!(s, 405, "{m} {p}");
        if m != "HEAD" {
            assert_eq!(b, r#"{"error":"method not allowed"}"#);
        }
    }

    // The deployment's URL until an administrator saves one.
    assert_eq!(
        call(addr, "GET", P, ADMIN, "", b"").await,
        (
            200,
            r#"{"url":"http://laya:8040","timeoutMs":1500,"source":"deployment"}"#.into()
        )
    );

    // Body and validation refusals change nothing.
    for (raw, want) in [
        (&b"{"[..], (400, r#"{"error":"invalid JSON"}"#)),
        (
            b"",
            (
                400,
                r#"{"error":"Enter a private decision-service URL and a deadline from 100 to 2000 ms.","errorId":"invalidInput"}"#,
            ),
        ),
        (
            br#"{"reset":"yes"}"#,
            (
                400,
                r#"{"error":"Enter a private decision-service URL and a deadline from 100 to 2000 ms.","errorId":"invalidInput"}"#,
            ),
        ),
        (
            b"null",
            (
                400,
                r#"{"error":"Enter a private decision-service URL and a deadline from 100 to 2000 ms.","errorId":"invalidInput"}"#,
            ),
        ),
        (
            br#"{"url":"http://8.8.8.8:8040","timeoutMs":1500}"#,
            (
                400,
                r#"{"error":"Use a private HTTP address such as http://192.168.1.20:8040, with no path, login or query string.","errorId":"invalidUrl"}"#,
            ),
        ),
        (
            br#"{"url":"http://laya:8040/x","timeoutMs":1500}"#,
            (
                400,
                r#"{"error":"Use a private HTTP address such as http://192.168.1.20:8040, with no path, login or query string.","errorId":"invalidUrl"}"#,
            ),
        ),
        (
            br#"{"url":"http://laya:8040","timeoutMs":1500,"model":"gpt-9"}"#,
            (
                400,
                r#"{"error":"Choose a decision model from the list, or automatic.","errorId":"invalidModel"}"#,
            ),
        ),
    ] {
        let (s, b) = call(addr, "PUT", P, ADMIN, &csrf, raw).await;
        assert_eq!((s, b.as_str()), want, "{}", String::from_utf8_lossy(raw));
    }
    let big = vec![b' '; 1024 * 1024 + 1];
    assert_eq!(
        call(addr, "PUT", P, ADMIN, &csrf, &big).await,
        (413, r#"{"error":"Request exceeds size limit"}"#.into())
    );
    assert_eq!(stored(&db), None);
    assert!(audits(&db).is_empty());

    // Save: Node's stored bytes, view and audit row.
    let (s, b) = call(
        addr,
        "PUT",
        P,
        ADMIN,
        &csrf,
        br#"{"url":" http://192.168.1.20:8040/ ","timeoutMs":900.0,"model":" KEV-0.8B "}"#,
    )
    .await;
    assert_eq!(
        (s, b.as_str()),
        (
            200,
            r#"{"url":"http://192.168.1.20:8040","timeoutMs":900,"model":"kev-0.8b","source":"admin"}"#
        )
    );
    assert_eq!(
        stored(&db).unwrap(),
        r#"{"url":"http://192.168.1.20:8040","timeoutMs":900,"model":"kev-0.8b"}"#
    );
    // A PUT without `model` keeps the saved one (#1816).
    let (s, b) = call(
        addr,
        "PUT",
        P,
        ADMIN,
        &csrf,
        br#"{"url":"http://laya:8041","timeoutMs":1000}"#,
    )
    .await;
    assert_eq!(
        (s, b.as_str()),
        (
            200,
            r#"{"url":"http://laya:8041","timeoutMs":1000,"model":"kev-0.8b","source":"admin"}"#
        )
    );
    assert_eq!(
        audits(&db),
        vec![
            (
                A.into(),
                A.into(),
                "decision.configure".into(),
                r#"{"url":"http://192.168.1.20:8040","timeoutMs":900,"model":"kev-0.8b"}"#.into()
            ),
            (
                A.into(),
                A.into(),
                "decision.configure".into(),
                r#"{"url":"http://laya:8041","timeoutMs":1000,"model":"kev-0.8b"}"#.into()
            ),
        ]
    );

    // An Inference machine for the decision service wins over the saved URL.
    db.execute("INSERT OR REPLACE INTO settings(key,value) VALUES('inference:machines',?1)", [r#"{"version":1,"machines":[{"id":"m1","name":"Pi","address":"http://10.0.0.7"}],"assignments":{"decision":{"machine":"m1","port":8040}}}"#]).unwrap();
    assert_eq!(
        call(addr, "GET", P, ADMIN, "", b"").await,
        (200, r#"{"url":"http://10.0.0.7:8040","timeoutMs":1000,"model":"kev-0.8b","source":"machine"}"#.into())
    );
    db.execute("DELETE FROM settings WHERE key='inference:machines'", [])
        .unwrap();

    // A restarted front reads what the last one wrote.
    f.stop().await;
    let f = front(d.path()).await;
    let addr = f.addr;
    let csrf = format!("Origin: http://{addr}\r\nX-CSRF-Token: csrf\r\n");
    assert_eq!(
        call(addr, "GET", P, ADMIN, "", b"").await,
        (
            200,
            r#"{"url":"http://laya:8041","timeoutMs":1000,"model":"kev-0.8b","source":"admin"}"#
                .into()
        )
    );

    // Reset (PUT {reset:true} or DELETE), audited also when nothing was saved.
    let deployment = r#"{"url":"http://laya:8040","timeoutMs":1500,"source":"deployment"}"#;
    assert_eq!(
        call(addr, "PUT", P, ADMIN, &csrf, br#"{"reset":true}"#).await,
        (200, deployment.into())
    );
    assert_eq!(stored(&db), None);
    assert_eq!(
        call(addr, "DELETE", P, ADMIN, &csrf, b"").await,
        (200, deployment.into())
    );
    let tail: Vec<_> = audits(&db).into_iter().skip(2).collect();
    assert_eq!(
        tail,
        vec![
            (
                A.into(),
                A.into(),
                "decision.reset".into(),
                r#"{"hadSaved":true}"#.into()
            ),
            (
                A.into(),
                A.into(),
                "decision.reset".into(),
                r#"{"hadSaved":false}"#.into()
            ),
        ]
    );

    // Node's fallbacks for rows it cannot parse.
    db.execute(
        "INSERT OR REPLACE INTO settings(key,value) VALUES('decision:configuration','{')",
        [],
    )
    .unwrap();
    assert_eq!(
        call(addr, "GET", P, ADMIN, "", b"").await,
        (
            200,
            r#"{"url":"","timeoutMs":1500,"source":"admin"}"#.into()
        )
    );
    db.execute(
        "INSERT OR REPLACE INTO settings(key,value) VALUES('decision:configuration',0)",
        [],
    )
    .unwrap();
    assert_eq!(
        call(addr, "GET", P, ADMIN, "", b"").await,
        (200, deployment.into())
    );
    f.stop().await;
}

#[tokio::test]
async fn decision_connection_test_policy() {
    let d = tempfile::tempdir().unwrap();
    let db = seed(d.path());
    let f = front(d.path()).await;
    let addr = f.addr;
    let csrf = format!("Origin: http://{addr}\r\nX-CSRF-Token: csrf\r\n");
    let ready = r#"{"ok":true,"message":"Decision service is ready. No inference was run.","messageId":"ready"}"#;
    let not_ready = r#"{"error":"The decision service is not ready or could not be reached. Check the URL and service status.","errorId":"notReady"}"#;
    let body = |a: std::net::SocketAddr| format!(r#"{{"url":"http://{a}","timeoutMs":1500}}"#);

    let up = upstream(ok_json(r#"{"ready":true}"#)).await;
    assert_eq!(
        call(addr, "POST", T, ADMIN, &csrf, body(up).as_bytes()).await,
        (200, ready.into())
    );
    // A name is resolved and judged: localhost is loopback.
    let named = format!(
        r#"{{"url":"http://localhost:{}","timeoutMs":1500}}"#,
        up.port()
    );
    assert_eq!(
        call(addr, "POST", T, ADMIN, &csrf, named.as_bytes()).await,
        (200, ready.into())
    );
    for reply in [
        ok_json(r#"{"ready":false}"#),
        ok_json("not json"),
        ok_json(&format!(r#"{{"ready":true,"pad":"{}"}}"#, "x".repeat(5000))),
        "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 14\r\nConnection: close\r\n\r\n{\"ready\":true}".into(),
        format!("HTTP/1.1 302 Found\r\nLocation: http://{up}/health\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"),
    ] {
        let bad = upstream(reply.clone()).await;
        assert_eq!(call(addr, "POST", T, ADMIN, &csrf, body(bad).as_bytes()).await, (502, not_ready.into()), "{reply}");
    }
    // A service that never answers: the 3 s deadline.
    let silent = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let silent_addr = silent.local_addr().unwrap();
    let started = std::time::Instant::now();
    assert_eq!(
        call(addr, "POST", T, ADMIN, &csrf, body(silent_addr).as_bytes()).await,
        (502, not_ready.into())
    );
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
    drop(silent);
    // Validation first; a public address is never contacted.
    assert_eq!(
        call(
            addr,
            "POST",
            T,
            ADMIN,
            &csrf,
            br#"{"url":"http://1.1.1.1","timeoutMs":1500}"#
        )
        .await
        .0,
        400
    );
    assert_eq!(
        call(addr, "POST", T, ADMIN, &csrf, b"{").await,
        (400, r#"{"error":"invalid JSON"}"#.into())
    );
    assert_eq!(call(addr, "POST", T, MEMBER, &csrf, b"{}").await.0, 403);
    // The test writes nothing.
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM settings WHERE key='decision:configuration'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    f.stop().await;
}
