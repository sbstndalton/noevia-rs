//! End to end: a synthetic corpus in the recorder's format replayed against a fake server.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use replay::bind::Bindings;
use replay::{corpus, http, Options};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// A server with one session cookie, a CSRF check and a project store; `drift` changes one field.
fn fake_server(drift: bool) -> (u16, Arc<AtomicU32>) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let counter = Arc::new(AtomicU32::new(0));
    let c = counter.clone();
    std::thread::spawn(move || {
        let session = format!("Sess{}xYz0123456789abcdefGHIJ", port);
        let csrf = format!("Csrf{}aBc9876543210zyxwvuQRST", port);
        let project = format!("{port:08x}-1111-4222-8333-444455556666");
        for stream in l.incoming() {
            let Ok(mut s) = stream else { continue };
            c.fetch_add(1, Ordering::SeqCst);
            let mut buf = vec![0u8; 65536];
            let mut n = 0;
            loop {
                let m = s.read(&mut buf[n..]).unwrap();
                n += m;
                let text = String::from_utf8_lossy(&buf[..n]).to_string();
                if let Some(h) = text.find("\r\n\r\n") {
                    let len = text
                        .lines()
                        .find_map(|l| l.strip_prefix("Content-Length: "))
                        .map(|v| v.trim().parse::<usize>().unwrap())
                        .unwrap_or(0);
                    if n >= h + 4 + len || m == 0 {
                        break;
                    }
                }
                if m == 0 {
                    break;
                }
            }
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            let line = req.lines().next().unwrap_or("").to_string();
            let body = req.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
            let has = |h: &str| req.lines().any(|l| l.eq_ignore_ascii_case(h));
            let signed_in = req.lines().any(|l| {
                l.starts_with("cookie: ") && l.contains(&format!("cowork_session={session}"))
            });
            let (status, extra, out) = if line.starts_with("POST /api/auth/login/password ") {
                if body.contains("\"password\":\"synthetic-pw\"") {
                    (
                        200,
                        format!("Set-Cookie: cowork_session={session}; Path=/; HttpOnly\r\nSet-Cookie: cowork_csrf={csrf}; Path=/\r\n"),
                        format!("{{\"user\":{{\"username\":\"synthetic\"}},\"csrfToken\":\"{csrf}\"}}"),
                    )
                } else {
                    (
                        401,
                        String::new(),
                        "{\"error\":\"invalid credentials\"}".to_string(),
                    )
                }
            } else if line.starts_with("POST /api/projects ") {
                if !signed_in || !has(&format!("x-csrf-token: {csrf}")) {
                    (403, String::new(), "{\"error\":\"csrf\"}".to_string())
                } else {
                    (201, String::new(), format!("{{\"id\":\"{project}\",\"name\":\"Synthetic\",\"createdAt\":1760000000000}}"))
                }
            } else if line.starts_with(&format!("GET /api/projects/{project} ")) && signed_in {
                let name = if drift { "Renamed" } else { "Synthetic" };
                (200, String::new(), format!("{{\"id\":\"{project}\",\"name\":\"{name}\",\"updatedAt\":\"2026-10-09T12:00:00Z\"}}"))
            } else if line.starts_with("POST /api/chat ") && signed_in {
                let resp = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n";
                let ev = format!(": ping\n\ndata: {{\"type\":\"start\",\"chatId\":\"chat-{port}-aaaa1111\"}}\n\ndata: [DONE]\n\n");
                let _ =
                    s.write_all(format!("{resp}{:x}\r\n{ev}\r\n0\r\n\r\n", ev.len()).as_bytes());
                continue;
            } else {
                (404, String::new(), "{\"error\":\"not found\"}".to_string())
            };
            let _ = s.write_all(format!("HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nX-Noevia-API: 1\r\n{extra}Content-Length: {}\r\n\r\n{out}", out.len()).as_bytes());
        }
    });
    (port, counter)
}

fn write_corpus() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "replay-e2e-{}-{}",
        std::process::id(),
        rand_suffix()
    ));
    let ex = dir.join("exchanges");
    std::fs::create_dir_all(&ex).unwrap();
    let files = [
        (
            "000001-POST-api-auth-login-password.json",
            r#"{"v":1,"seq":1,"request":{"method":"POST","path":"/api/auth/login/password","query":[],"headers":{"content-type":"application/json","origin":"<origin>"},"body":{"kind":"json","json":{"username":"synthetic","password":"<secret:1>"}}},
               "response":{"status":200,"headers":{"content-type":"application/json","x-noevia-api":"1","set-cookie":["cowork_session=<secret:2>; Path=/; HttpOnly","cowork_csrf=<secret:3>; Path=/"]},"body":{"kind":"json","json":{"user":{"username":"synthetic"},"csrfToken":"<secret:3>"}}}}"#,
        ),
        (
            "000002-POST-api-auth-login-password.json",
            r#"{"v":1,"seq":2,"request":{"method":"POST","path":"/api/auth/login/password","query":[],"headers":{"content-type":"application/json"},"body":{"kind":"json","json":{"username":"synthetic","password":"<secret:4>"}}},
               "response":{"status":401,"headers":{"content-type":"application/json","x-noevia-api":"1"},"body":{"kind":"json","json":{"error":"invalid credentials"}}}}"#,
        ),
        (
            "000003-POST-api-projects.json",
            r#"{"v":1,"seq":3,"request":{"method":"POST","path":"/api/projects","query":[],"headers":{"content-type":"application/json","cookie":"cowork_session=<secret:2>; cowork_csrf=<secret:3>","x-csrf-token":"<secret:3>"},"body":{"kind":"json","json":{"name":"Synthetic"}}},
               "response":{"status":201,"headers":{"content-type":"application/json","x-noevia-api":"1"},"body":{"kind":"json","json":{"id":"<id:1>","name":"Synthetic","createdAt":"<ts>"}}}}"#,
        ),
        (
            "000004-GET-api-projects-id.json",
            r#"{"v":1,"seq":4,"request":{"method":"GET","path":"/api/projects/<id:1>","query":[],"headers":{"cookie":"cowork_session=<secret:2>; cowork_csrf=<secret:3>"},"body":{"kind":"empty"}},
               "response":{"status":200,"headers":{"content-type":"application/json","x-noevia-api":"1"},"body":{"kind":"json","json":{"id":"<id:1>","name":"Synthetic","updatedAt":"<ts>"}}}}"#,
        ),
        (
            "000005-POST-api-chat.json",
            r#"{"v":1,"seq":5,"request":{"method":"POST","path":"/api/chat","query":[],"headers":{"cookie":"cowork_session=<secret:2>; cowork_csrf=<secret:3>","content-type":"application/json"},"body":{"kind":"json","json":{"projectId":"<id:1>","message":"hi"}}},
               "response":{"status":200,"headers":{"content-type":"text/event-stream"},"body":{"kind":"sse","events":[{"data":{"type":"start","chatId":"<id:2>"},"json":true},{"data":"[DONE]"}]}}}"#,
        ),
    ];
    for (name, text) in files {
        std::fs::write(ex.join(name), text).unwrap();
    }
    std::fs::write(ex.join("000006-GET-api-leaky.dropped"), "{}").unwrap();
    std::fs::write(
        dir.join("manifest.json"),
        r#"{"v":1,"inputs":{"<secret:1>":{"value":"synthetic-pw"}}}"#,
    )
    .unwrap();
    dir
}

fn rand_suffix() -> u32 {
    static N: AtomicU32 = AtomicU32::new(0);
    N.fetch_add(1, Ordering::SeqCst)
}

fn opts(port: u16) -> Options {
    Options {
        base: http::Base::parse(&format!("http://127.0.0.1:{port}")).unwrap(),
        timeout: Duration::from_secs(5),
        ignore_headers: Vec::new(),
        data_dir: None,
        now: "2026-01-01T00:00:00.000Z".into(),
    }
}

#[test]
fn a_faithful_server_replays_clean_and_binds_every_placeholder() {
    let dir = write_corpus();
    let c = corpus::load(&dir).unwrap();
    assert_eq!((c.exchanges.len(), c.dropped), (5, 1));
    let (port, hits) = fake_server(false);
    let o = opts(port);
    let mut b = Bindings::new(&o.base.origin());
    let out = replay::replay(&c, &o, &mut b, false);
    for x in &out {
        assert!(x.clean(), "{} differs: {:?} {:?}", x.file, x.error, x.diffs);
    }
    assert_eq!(hits.load(Ordering::SeqCst), 5);
    assert!(b.get("<secret:2>").unwrap().starts_with("Sess"));
    assert!(b.get("<id:1>").unwrap().ends_with("444455556666"));
    assert_eq!(b.get("<id:2>").unwrap(), format!("chat-{port}-aaaa1111"));
    // The deliberately wrong password had no recorded value: it was generated, and still refused.
    assert_eq!(out[1].generated, vec!["<secret:4>".to_string()]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_drifted_server_is_reported_at_the_field_that_changed() {
    let dir = write_corpus();
    let c = corpus::load(&dir).unwrap();
    let (port, _) = fake_server(true);
    let o = opts(port);
    let mut b = Bindings::new(&o.base.origin());
    let out = replay::replay(&c, &o, &mut b, false);
    let bad: Vec<_> = out.iter().filter(|x| !x.clean()).collect();
    assert_eq!(bad.len(), 1);
    assert_eq!(bad[0].path, "/api/projects/<id:1>");
    assert_eq!(bad[0].diffs.len(), 1);
    assert_eq!(bad[0].diffs[0].at, "body/name");
    assert_eq!(bad[0].diffs[0].actual, "\"Renamed\"");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_missing_input_fails_sign_in_and_everything_after_it_shows() {
    let dir = write_corpus();
    std::fs::write(dir.join("manifest.json"), r#"{"v":1,"inputs":{}}"#).unwrap();
    let c = corpus::load(&dir).unwrap();
    let (port, _) = fake_server(false);
    let o = opts(port);
    let mut b = Bindings::new(&o.base.origin());
    let out = replay::replay(&c, &o, &mut b, true);
    assert_eq!(out.len(), 1, "--stop-on-first stops at the failed sign-in");
    assert!(out[0]
        .diffs
        .iter()
        .any(|d| d.at == "status" && d.actual == "401"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_manifest_input_outside_the_data_dir_is_refused() {
    let dir = write_corpus();
    std::fs::write(
        dir.join("manifest.json"),
        r#"{"v":1,"inputs":{"<secret:1>":{"file":"../etc/passwd"}}}"#,
    )
    .unwrap();
    assert!(corpus::load(&dir)
        .unwrap_err()
        .contains("inside the data dir"));
    let _ = std::fs::remove_dir_all(&dir);
}
