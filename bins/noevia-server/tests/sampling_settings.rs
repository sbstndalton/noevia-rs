//! Real HTTP through the native front; dead legacy upstream makes fallback fail.
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
    body: &str,
) -> (u16, serde_json::Value) {
    call_bytes(addr, method, path, cookie, extra, body.as_bytes()).await
}
async fn call_bytes(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    cookie: &str,
    extra: &str,
    body: &[u8],
) -> (u16, serde_json::Value) {
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nCookie: {cookie}\r\nContent-Length: {}\r\n{extra}\r\n",body.len()).as_bytes()).await.unwrap();
    // Admin/member refusal may close without consuming a large request body. A write/reset
    // alone never passes: the complete expected HTTP response is still parsed and asserted.
    let write = s.write_all(body).await;
    if let Err(err) = write {
        assert!(matches!(
            err.kind(),
            std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset
        ));
    }
    let mut bytes = Vec::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        s.read_to_end(&mut bytes),
    )
    .await
    .unwrap()
    .unwrap_or_else(|err| {
        assert_eq!(err.kind(), std::io::ErrorKind::ConnectionReset);
        0
    });
    let text = String::from_utf8(bytes).unwrap();
    let (h, b) = text.split_once("\r\n\r\n").unwrap();
    if path.starts_with("/api/sampling-settings") && !h.starts_with("HTTP/1.1 502") {
        let lower = h.to_ascii_lowercase();
        assert!(lower.contains("content-type: application/json"));
        assert!(lower.contains("cache-control: no-store"));
    }
    (
        h.split_whitespace().nth(1).unwrap().parse().unwrap(),
        serde_json::from_str(b).unwrap_or(serde_json::Value::Null),
    )
}
#[tokio::test]
async fn sampling_identity_admin_methods_audit_caps_and_omission() {
    let d = tempfile::tempdir().unwrap();
    let db = Connection::open(d.path().join("cowork.db")).unwrap();
    db.execute_batch(include_str!(
        "../../../crates/server-store/tests/fixtures/node-schema.sql"
    ))
    .unwrap();
    let now = noevia_server::identity::now_ms();
    for (uid, role, t) in [(A, "admin", "admin-token"), (B, "member", "member-token")] {
        db.execute("INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,created_at,updated_at) VALUES(?1,?1,?1,'Synthetic',?2,'x',?1,1,1)",(uid,role)).unwrap();
        db.execute(
            "INSERT INTO sessions VALUES(?1,?2,?3,?4,?4,?5,'ua','127.0.0.1')",
            (hex(t), uid, hex("csrf"), now, now + 86400000),
        )
        .unwrap();
    }
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let origin = format!("http://{addr}");
    let env = HashMap::from([
        ("NOEVIA_LEGACY_UPSTREAM", "http://127.0.0.1:9".into()),
        ("UI_DATA_DIR", d.path().display().to_string()),
        ("PUBLIC_ORIGIN", origin.clone()),
    ]);
    let app = App::new(Config::from_lookup(|k| env.get(k).cloned()).unwrap());
    let (tx, rx) = oneshot::channel();
    let task = tokio::spawn(serve(listener, app, Limits::default(), async {
        let _ = rx.await;
    }));
    let admin = "cowork_session=admin-token; cowork_csrf=csrf";
    let member = "cowork_session=member-token; cowork_csrf=csrf";
    let trusted = format!("Origin: {origin}\r\nX-CSRF-Token: csrf\r\n");
    let p = "/api/sampling-settings";
    assert_eq!(call(addr, "GET", p, "", "", "").await.0, 401);
    assert_eq!(
        call(addr, "GET", p, "cowork_session=wrong", "", "").await.0,
        401
    );
    let (s, v) = call(addr, "GET", p, member, "", "").await;
    assert_eq!(s, 200);
    assert_eq!(v["enabled"], true);
    assert_eq!(v["admin"], false);
    assert_eq!(v["presets"], server_sampling_settings::presets());
    assert_eq!(call(addr, "PUT", p, member, "", "null").await.0, 403); // CSRF gate first
    assert_eq!(
        call(addr, "PUT", p, member, &trusted, "null").await,
        (403, serde_json::json!({"error":"Administrator required"}))
    );
    assert_eq!(call(addr, "HEAD", p, admin, "", "").await.0, 405);
    for raw in ["null", "[]", "0", "false", r#"{"enabled":"false"}"#] {
        assert_eq!(
            call(addr, "PUT", p, admin, &trusted, raw).await,
            (
                400,
                serde_json::json!({"error":"enabled must be true or false"})
            )
        );
    }
    for raw in ["", "{"] {
        assert_eq!(
            call(addr, "PUT", p, admin, &trusted, raw).await,
            (400, serde_json::json!({"error":"invalid JSON"}))
        );
    }
    for enabled in [false, true] {
        let raw = serde_json::json!({"enabled":enabled,"extra":1}).to_string();
        assert_eq!(
            call(addr, "PUT", p, admin, &trusted, &raw).await,
            (200, serde_json::json!({"enabled":enabled}))
        );
        let (s, v) = call(
            addr,
            "GET",
            "/api/sampling-settings?userId=ignored",
            member,
            &format!("X-User-Id: {A}\r\n"),
            "",
        )
        .await;
        assert_eq!(s, 200);
        assert_eq!(v["enabled"], enabled);
        assert_eq!(v["admin"], false);
    }
    let rows:Vec<(String,Option<String>,String,String)>=db.prepare("SELECT actor_user_id,target_user_id,action,detail FROM audit_events WHERE action='sampling.autoPresets' ORDER BY id").unwrap().query_map([],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).unwrap().collect::<Result<_,_>>().unwrap();
    assert_eq!(
        rows,
        vec![
            (
                A.into(),
                None,
                "sampling.autoPresets".into(),
                "{\"enabled\":false}".into()
            ),
            (
                A.into(),
                None,
                "sampling.autoPresets".into(),
                "{\"enabled\":true}".into()
            )
        ]
    );
    db.execute_batch("CREATE TRIGGER sampling_audit_abort BEFORE INSERT ON audit_events BEGIN SELECT RAISE(ABORT,'synthetic audit refusal'); END;").unwrap();
    assert_eq!(
        call(addr, "PUT", p, admin, &trusted, r#"{"enabled":false}"#).await,
        (500, serde_json::json!({"error":"Internal error"}))
    );
    assert_eq!(
        db.query_row(
            "SELECT value FROM settings WHERE key='auto_sampling_presets_enabled'",
            [],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "true"
    );
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM audit_events WHERE action='sampling.autoPresets'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        2
    );
    db.execute_batch("DROP TRIGGER sampling_audit_abort")
        .unwrap();
    for normalized in [
        "/api/x/../sampling-settings",
        "/api/x/%2e%2e/sampling-settings",
    ] {
        assert_eq!(call(addr, "GET", normalized, "", "", "").await.0, 401);
        assert_eq!(
            call(addr, "PUT", normalized, member, &trusted, "null").await,
            (403, serde_json::json!({"error":"Administrator required"}))
        );
        assert_eq!(
            call(addr, "PUT", normalized, admin, "", r#"{"enabled":true}"#)
                .await
                .0,
            403
        );
        let (status, value) = call(addr, "GET", normalized, member, "", "").await;
        assert_eq!(status, 200);
        assert_eq!(value["enabled"], true);
        assert_eq!(
            call(
                addr,
                "PUT",
                normalized,
                admin,
                &trusted,
                r#"{"enabled":true}"#
            )
            .await,
            (200, serde_json::json!({"enabled":true}))
        );
    }
    let normalized_audits:Vec<(String,Option<String>,String)>=db.prepare("SELECT actor_user_id,target_user_id,detail FROM audit_events WHERE action='sampling.autoPresets' ORDER BY id").unwrap().query_map([],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap().collect::<Result<_,_>>().unwrap();
    assert_eq!(normalized_audits.len(), 4);
    assert!(normalized_audits
        .iter()
        .skip(2)
        .all(|row| *row == (A.into(), None, "{\"enabled\":true}".into())));
    let over = format!(
        r#"{{"enabled":false,"extra":"{}"}}"#,
        "a".repeat(server_sampling_settings::BODY_LIMIT)
    );
    assert_eq!(call(addr, "PUT", p, admin, &trusted, &over).await.0, 400);
    db.execute("UPDATE sessions SET expires_at=0 WHERE user_id=?1", [B])
        .unwrap();
    assert_eq!(call(addr, "GET", p, member, "", "").await.0, 401);
    db.execute(
        "INSERT OR REPLACE INTO sessions VALUES(?1,?2,?3,?4,?4,?5,'ua','127.0.0.1')",
        (hex("member-token"), B, hex("csrf"), now, now + 86400000),
    )
    .unwrap();
    assert_eq!(call(addr, "GET", p, member, "", "").await.0, 200);
    tx.send(()).unwrap();
    task.await.unwrap();
    // sbstndalton/noevia#1071: the retired switch set to "0" keeps the route native.
    let mut env = env;
    env.insert("NOEVIA_RUST_SAMPLING_SETTINGS", "0".into());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = oneshot::channel();
    let task = tokio::spawn(serve(
        listener,
        App::new(Config::from_lookup(|k| env.get(k).cloned()).unwrap()),
        Limits::default(),
        async {
            let _ = rx.await;
        },
    ));
    assert_eq!(
        call(addr, "GET", "/api/auth/session", member, "", "")
            .await
            .0,
        200
    );
    for path in [
        p,
        "/api/x/../sampling-settings",
        "/api/x/%2e%2e/sampling-settings",
    ] {
        assert_eq!(
            call(addr, "GET", path, member, "", "").await.0,
            200,
            "{path}"
        );
    }
    tx.send(()).unwrap();
    task.await.unwrap();
}

#[tokio::test]
async fn actual_node_matrix_through_native_http_with_dead_legacy() {
    use server_store::rusqlite::{params, types::Value as SqlValue, OptionalExtension};
    let f: serde_json::Value = serde_json::from_str(include_str!(
        "../../../crates/server-sampling-settings/tests/fixtures/node-sampling.v1.json"
    ))
    .unwrap();
    let d = tempfile::tempdir().unwrap();
    let db = Connection::open(d.path().join("cowork.db")).unwrap();
    db.execute_batch(
        &include_str!("../../../crates/server-store/tests/fixtures/node-schema.sql")
            .replace("value TEXT NOT NULL", "value"),
    )
    .unwrap();
    let now = noevia_server::identity::now_ms();
    for (uid, role, token) in [(A, "admin", "admin-token"), (B, "member", "member-token")] {
        db.execute("INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,created_at,updated_at) VALUES(?1,?1,?1,'Synthetic',?2,'x',?1,1,1)",(uid,role)).unwrap();
        db.execute(
            "INSERT INTO sessions VALUES(?1,?2,?3,?4,?4,?5,'ua','127.0.0.1')",
            (hex(token), uid, hex("csrf"), now, now + 86400000),
        )
        .unwrap();
    }
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let origin = format!("http://{addr}");
    let env = HashMap::from([
        ("UI_DATA_DIR", d.path().display().to_string()),
        ("NOEVIA_LEGACY_UPSTREAM", "http://127.0.0.1:9".into()),
        ("PUBLIC_ORIGIN", origin.clone()),
    ]);
    let (tx, rx) = oneshot::channel();
    let task = tokio::spawn(serve(
        listener,
        App::new(Config::from_lookup(|k| env.get(k).cloned()).unwrap()),
        Limits::default(),
        async {
            let _ = rx.await;
        },
    ));
    let trusted = format!("Origin: {origin}\r\nX-CSRF-Token: csrf\r\n");
    let mut checked = 0;
    for c in f["cases"].as_array().unwrap() {
        // Transport read failure is covered by the actual Node module oracle and bounded-body
        // tests; an HTTP half-close is a different transport contract from Readable throwing.
        if c["input"]["readError"] == true {
            continue;
        }
        db.execute_batch("DROP TRIGGER IF EXISTS sampling_audit_abort; DELETE FROM settings; DELETE FROM audit_events").unwrap();
        let initial = &c["initial"];
        let value = match initial["kind"].as_str().unwrap() {
            "missing" => None,
            "null" => Some(SqlValue::Null),
            "text" => Some(SqlValue::Text(initial["value"].as_str().unwrap().into())),
            "number" => Some(SqlValue::Real(initial["value"].as_f64().unwrap())),
            "blob" => Some(SqlValue::Blob(
                initial["value"]
                    .as_str()
                    .unwrap()
                    .as_bytes()
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|v| u8::from_str_radix(std::str::from_utf8(v).unwrap(), 16).unwrap())
                    .collect(),
            )),
            _ => panic!("fixture kind"),
        };
        if let Some(value) = value {
            db.execute(
                "INSERT INTO settings(key,value) VALUES(?1,?2)",
                params![server_sampling_settings::KEY, value],
            )
            .unwrap();
        }
        if c["input"]["auditAbort"] == true {
            db.execute_batch("CREATE TRIGGER sampling_audit_abort BEFORE INSERT ON audit_events BEGIN SELECT RAISE(ABORT,'synthetic audit refusal'); END;").unwrap();
        }
        let input = &c["input"];
        let bytes = if let Some(n) = input["repeatExtra"].as_u64() {
            format!(
                "{{\"enabled\":true,\"extra\":\"{}\"}}",
                "a".repeat(n as usize)
            )
            .into_bytes()
        } else if input["invalidUtf8"] == true {
            let mut b = b"{\"enabled\":true,\"extra\":\"".to_vec();
            b.push(255);
            b.extend_from_slice(b"\"}");
            b
        } else {
            input["raw"].as_str().unwrap_or("").as_bytes().to_vec()
        };
        let cookie = if c["role"] == "admin" {
            "cowork_session=admin-token; cowork_csrf=csrf"
        } else {
            "cowork_session=member-token; cowork_csrf=csrf"
        };
        let method = c["method"].as_str().unwrap();
        let (status, body) = call_bytes(
            addr,
            method,
            "/api/sampling-settings",
            cookie,
            &trusted,
            &bytes,
        )
        .await;
        assert_eq!(
            status,
            c["expected"]["status"].as_u64().unwrap() as u16,
            "{}",
            c["name"]
        );
        if method != "HEAD" {
            assert_eq!(body, c["expected"]["body"], "{}", c["name"]);
        }
        let setting: Option<(String, SqlValue)> = db
            .query_row(
                "SELECT typeof(value),value FROM settings WHERE key=?1",
                [server_sampling_settings::KEY],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .unwrap();
        let setting = setting.map(|(kind, value)| {
            let value = match value {
                SqlValue::Null => serde_json::Value::Null,
                SqlValue::Text(v) => serde_json::json!(v),
                SqlValue::Integer(v) => serde_json::json!(v),
                SqlValue::Real(v) => {
                    if v.fract() == 0.0 {
                        serde_json::json!(v as i64)
                    } else {
                        serde_json::json!(v)
                    }
                }
                SqlValue::Blob(v) => {
                    serde_json::json!(v.iter().map(|b| format!("{b:02x}")).collect::<String>())
                }
            };
            serde_json::json!({"type":kind,"value":value})
        });
        assert_eq!(
            serde_json::json!(setting),
            c["expected"]["setting"],
            "{}",
            c["name"]
        );
        let rows:Vec<(String,Option<String>,String,String,i64)>=db.prepare("SELECT actor_user_id,target_user_id,action,detail,created_at FROM audit_events ORDER BY id").unwrap().query_map([],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).unwrap().collect::<Result<_,_>>().unwrap();
        let expected = c["expected"]["audit"].as_array().unwrap();
        assert_eq!(rows.len(), expected.len(), "{}", c["name"]);
        for (row, want) in rows.iter().zip(expected) {
            assert_eq!(row.0, A);
            assert_eq!(row.1, None);
            assert_eq!(row.2, want["action"].as_str().unwrap());
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&row.3).unwrap(),
                want["detail"]
            );
            assert!(row.4 >= now && row.4 <= noevia_server::identity::now_ms());
        }
        checked += 1;
    }
    assert_eq!(checked, 74);
    tx.send(()).unwrap();
    task.await.unwrap();
}
