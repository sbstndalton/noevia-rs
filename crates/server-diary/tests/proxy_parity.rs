#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
mod support;
use bytes::Bytes;
use http_body_util::Full;
use js_json::{wtf16, JValue};
use server_diary::{Limits, ReplyBody, Request, Route};
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use support::*;
fn text(v: &JValue) -> String {
    wtf16::stringify(v).unwrap()
}

#[tokio::test]
async fn actual_core_proxy_oracle() {
    let doc = wtf16::parse(include_str!("fixtures/diary-proxy-node.json")).unwrap();
    let JValue::Arr(cases) = doc.get("cases") else {
        panic!()
    };
    for (index, c) in cases.iter().enumerate() {
        let c = c.clone();
        let upstream = c.clone();
        let stub = Stub::start(move |_, _| {
            if upstream.get("fault").as_str().is_some() {
                return Out::Hangup;
            }
            Out::json(
                upstream.get("status").as_num().unwrap() as u16,
                &text(upstream.get("reply")),
            )
        })
        .await;
        let world = World::new(&[]);
        let d = diary(&stub, TENANT_KEY);
        let route =
            Route::from_path(&format!("/api/diary/{}", c.get("route").as_str().unwrap())).unwrap();
        // A generic injected fault is checked separately; a socket hangup models only network faults.
        if c.get("fault").as_str() == Some("internal") {
            continue;
        }
        let raw = c
            .get("size")
            .as_num()
            .map(|n| "x".repeat(n as usize))
            .unwrap_or_else(|| c.get("raw").as_str().unwrap().to_owned());
        let id = if c.get("admin") == &JValue::Bool(true) {
            ADMIN
        } else {
            MEMBER
        };
        let ctx = world.ctx(id, c.get("enabled") == &JValue::Bool(true));
        let reply = d
            .serve_proxy(
                route,
                ctx,
                Request {
                    method: c.get("method").as_str().unwrap().into(),
                    target: c.get("target").as_str().unwrap().into(),
                    content_length: None,
                },
                Full::new(Bytes::from(raw)),
                false,
            )
            .await;
        assert_eq!(
            reply.status,
            c.get("expected").get("status").as_num().unwrap() as u16,
            "case {index}: {}",
            text(&c)
        );
        let expected_wait = c
            .get("expected")
            .get("headers")
            .get("Retry-After")
            .as_str()
            .map(|s| s.parse::<u64>().unwrap());
        assert_eq!(reply.retry_after, expected_wait, "Retry-After case {index}");
        let ReplyBody::Json(body) = reply.body else {
            panic!()
        };
        assert_eq!(
            wtf16::parse(&body).unwrap(),
            c.get("expected").get("body").clone(),
            "case {index}"
        );
        let seen = stub.seen.lock().unwrap();
        if c.get("fault").as_str().is_none() {
            let JValue::Arr(calls) = c.get("calls") else {
                panic!()
            };
            assert_eq!(seen.len(), calls.len(), "calls {index}");
            for (got, want) in seen.iter().zip(calls) {
                assert_eq!(
                    got.target,
                    want.get("path").as_str().unwrap(),
                    "path {index}"
                );
                assert_eq!(
                    got.method,
                    want.get("method").as_str().unwrap(),
                    "method {index}"
                );
                if let Some(body) = want.get("body").as_str() {
                    assert_eq!(String::from_utf8_lossy(&got.body), body, "body {index}");
                }
            }
        }
    }
}

struct Clear(AtomicUsize);
impl server_diary::StorageLatch for Clear {
    fn held(&self, _: &str, _: &server_diary::StorageIdentity<'_>) -> Option<server_diary::Held> {
        None
    }
    fn clear(&self, _: &str) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
#[tokio::test]
async fn explicit_retry_clears_only_after_legacy_proof_and_timeouts_are_bounded() {
    for (retry, mode, expected) in [
        (false, "legacy", 0),
        (true, "managed", 0),
        (true, "legacy", 1),
    ] {
        let stub = Stub::start(move |seen, _| {
            if seen.path() == "/api/storage-status" {
                Out::json(200, &format!("{{\"mode\":\"{mode}\"}}"))
            } else {
                Out::json(200, "{}")
            }
        })
        .await;
        let world = World::new(&[]);
        let latch = Arc::new(Clear(AtomicUsize::new(0)));
        let ctx = world.ctx_with_latch(ADMIN, latch.clone());
        diary(&stub, TENANT_KEY)
            .serve_proxy(
                Route::Files,
                ctx,
                Request {
                    method: "GET".into(),
                    target: "/api/diary/files".into(),
                    content_length: None,
                },
                Full::new(Bytes::new()),
                retry,
            )
            .await;
        assert_eq!(latch.0.load(Ordering::SeqCst), expected);
        assert_eq!(
            stub.seen.lock().unwrap()[0].header("x-cowork-storage-retry"),
            retry.then_some("1")
        );
        for call in stub.seen.lock().unwrap().iter().skip(1) {
            assert_eq!(call.header("x-cowork-storage-retry"), None);
        }
    }
    for route in [Route::Files, Route::ExternalSources, Route::Edit] {
        let stub = Stub::start(|_, _| Out::json(200, "{}").slow(Duration::from_millis(200))).await;
        let d = diary_with(
            &stub,
            TENANT_KEY,
            Limits {
                proxy_timeout: Duration::from_millis(10),
                external_timeout: Duration::from_millis(10),
                ..Limits::default()
            },
        );
        let world = World::new(&[]);
        let raw = if route == Route::Edit {
            r#"{"xid":"x","me":"m"}"#
        } else {
            ""
        };
        let reply = d
            .serve_proxy(
                route,
                world.ctx(ADMIN, true),
                Request {
                    method: if route == Route::Edit { "POST" } else { "GET" }.into(),
                    target: "/api/diary/files".into(),
                    content_length: None,
                },
                Full::new(Bytes::from(raw)),
                false,
            )
            .await;
        assert_eq!(reply.status, 503);
    }
}

#[tokio::test]
async fn actual_core_worker_schedule_oracle() {
    use server_diary::backup::Schedule;
    use std::sync::Mutex;
    let doc = wtf16::parse(include_str!("fixtures/diary-proxy-node.json")).unwrap();
    let JValue::Arr(scripts) = doc.get("worker") else {
        panic!()
    };
    for script in scripts {
        let mut schedule = Schedule::default();
        let JValue::Arr(steps) = script else { panic!() };
        for step in steps {
            let JValue::Arr(users) = step.get("users") else {
                panic!()
            };
            let users = users
                .iter()
                .map(|v| v.as_str().unwrap().to_owned())
                .collect();
            let calls = Arc::new(Mutex::new(Vec::new()));
            let reply = step.get("reply").clone();
            let step = step.clone();
            schedule
                .tick(
                    users,
                    step.get("now").as_num().unwrap() as u64,
                    30_000,
                    |id| {
                        let calls = calls.clone();
                        let step = step.clone();
                        let reply = reply.clone();
                        async move {
                            if step.get("suspended") == &JValue::Bool(true) {
                                return None;
                            }
                            calls.lock().unwrap().push(JValue::Str(id));
                            Some(if step.get("fail") == &JValue::Bool(true) {
                                None
                            } else {
                                Some(reply)
                            })
                        }
                    },
                )
                .await;
            assert_eq!(
                JValue::Arr(calls.lock().unwrap().clone()),
                step.get("calls").clone()
            );
        }
    }
}

#[tokio::test]
async fn redirect_and_configuration_failures_remain_internal_errors() {
    use server_diary::{Diary, DiaryConfig};
    let stub = Stub::start(|_, _| Out::json(302, "{}")).await;
    let world = World::new(&[]);
    let bad = Diary::new(DiaryConfig::new(
        "invalid-base",
        BEARER,
        TENANT_KEY,
        String::new,
    ))
    .unwrap();
    for route in [
        Route::Files,
        Route::WorkspaceTrash,
        Route::History,
        Route::ExternalSources,
        Route::Edit,
    ] {
        for d in [diary(&stub, TENANT_KEY), bad.clone()] {
            let reply = d
                .serve_proxy(
                    route,
                    world.ctx(ADMIN, true),
                    Request {
                        method: if route == Route::Edit { "POST" } else { "GET" }.into(),
                        target: "/api/diary/files".into(),
                        content_length: None,
                    },
                    Full::new(Bytes::from_static(br#"{"xid":"x","me":"m"}"#)),
                    false,
                )
                .await;
            assert_eq!(reply.status, 500);
        }
    }
    assert_eq!(Limits::default().external_timeout, Duration::from_secs(30));
    assert_eq!(Limits::default().proxy_timeout, Duration::from_secs(60));
    assert_eq!(Limits::default().read_timeout, Duration::from_secs(15));
}

#[tokio::test]
async fn valid_js_json_and_conflict_text_match_the_actual_core_handlers() {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/diary-proxy-node-raw.json")).unwrap();
    for c in fixture["cases"].as_array().unwrap() {
        let status = c["status"].as_u64().unwrap() as u16;
        let reply_raw = c["replyRaw"].as_str().unwrap().to_owned();
        let stub = Stub::start(move |_, _| Out::json(status, &reply_raw)).await;
        let w = World::new(&[]);
        let route =
            Route::from_path(&format!("/api/diary/{}", c["route"].as_str().unwrap())).unwrap();
        let r = diary(&stub, TENANT_KEY)
            .serve_proxy(
                route,
                w.ctx(ADMIN, true),
                Request {
                    method: c["method"].as_str().unwrap().into(),
                    target: "/api/diary/test?month=2026-09&path=a%20b%2F%C3%A9&after=mark%2Fone"
                        .into(),
                    content_length: None,
                },
                Full::new(Bytes::from(c["raw"].as_str().unwrap().to_owned())),
                false,
            )
            .await;
        assert_eq!(
            r.status,
            c["expectedStatus"].as_u64().unwrap() as u16,
            "{c}"
        );
        let ReplyBody::Json(body) = r.body else {
            panic!()
        };
        assert_eq!(body, c["expectedBody"].as_str().unwrap(), "{c}");
        let seen = stub.seen.lock().unwrap();
        assert_eq!(seen.len(), c["calls"].as_array().unwrap().len());
        for (got, expected) in seen.iter().zip(c["calls"].as_array().unwrap()) {
            if let Some(body) = expected["body"].as_str() {
                assert_eq!(String::from_utf8_lossy(&got.body), body);
            }
        }
    }
}

#[tokio::test]
async fn actual_core_deep_detail_coercion_at_join_boundary_and_reply_cap() {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/diary-proxy-node-depth.json")).unwrap();
    assert_eq!(
        fixture["replyCap"].as_u64().unwrap() as usize,
        server_diary::client::JSON_REPLY_CAP
    );
    let mut failures = Vec::new();
    for c in fixture["cases"].as_array().unwrap() {
        let depth = c["depth"].as_u64().unwrap() as usize;
        let raw_reply = format!("{{\"detail\":{}1{}}}", "[".repeat(depth), "]".repeat(depth));
        assert_eq!(raw_reply.len(), c["replyBytes"].as_u64().unwrap() as usize);
        let upstream = c["status"].as_u64().unwrap() as u16;
        let stub = Stub::start(move |_, _| Out::json(upstream, &raw_reply)).await;
        let w = World::new(&[]);
        let name = c["route"].as_str().unwrap();
        let route = Route::from_path(&format!("/api/diary/{name}")).unwrap();
        let r = diary(&stub, TENANT_KEY)
            .serve_proxy(
                route,
                w.ctx(ADMIN, true),
                Request {
                    method: "POST".into(),
                    target: format!("/api/diary/{name}"),
                    content_length: None,
                },
                Full::new(Bytes::from(c["raw"].as_str().unwrap().to_owned())),
                false,
            )
            .await;
        let ReplyBody::Json(body) = r.body else {
            panic!()
        };
        let pass = r.status == c["expectedStatus"].as_u64().unwrap() as u16
            && body == c["expectedBody"].as_str().unwrap();
        eprintln!(
            "test result: Core differential depth {depth} {name}: expected {} {}, actual {} {}; {}",
            c["expectedStatus"],
            c["expectedBody"],
            r.status,
            body,
            if pass { "PASS" } else { "FAIL" }
        );
        if !pass {
            failures.push((name.to_owned(), depth, r.status, body));
        }
    }
    assert!(failures.is_empty(), "Core differentials: {failures:?}");
}

// Exercise each shared-client projection, including routes whose historic contract is 502.
#[tokio::test]
async fn tls_protocol_failure_uses_each_routes_internal_response() {
    use server_diary::{Diary, DiaryConfig};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut hello = [0; 4096];
            let _ = socket.read(&mut hello).await;
            let _ = socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                .await;
        }
    });
    let d = Diary::new(DiaryConfig::new(
        format!("https://{address}"),
        BEARER,
        TENANT_KEY,
        String::new,
    ))
    .unwrap();
    let world = World::new(&[]);
    for path in Route::PATHS {
        let route = Route::from_path(path).unwrap();
        let (method, raw) = match route {
            Route::File => ("PUT", "{}"),
            Route::ExternalImport => ("POST", r#"{"sourcePath":"/a","relPath":"b"}"#),
            Route::Edit => ("POST", r#"{"xid":"x","me":"m"}"#),
            Route::WorkspaceImport | Route::StorageImport => ("POST", "{}"),
            _ => ("GET", ""),
        };
        let reply = d
            .serve(
                route,
                world.ctx(ADMIN, true),
                Request {
                    method: method.into(),
                    target: path.into(),
                    content_length: None,
                },
                Full::new(Bytes::from_static(raw.as_bytes())),
            )
            .await;
        let expected = match route {
            Route::WorkspaceExport => (502, "Workspace export could not finish. Retry when storage is available."),
            Route::WorkspaceImport => (502, "Import response unavailable. Retry with the same file and folder to safely check whether it completed."),
            _ => (500, "Internal error"),
        };
        assert_eq!(reply.status, expected.0, "{path}");
        let ReplyBody::Json(body) = reply.body else {
            panic!("expected JSON")
        };
        assert_eq!(body, format!(r#"{{"error":"{}"}}"#, expected.1), "{path}");
        assert!(
            !body.contains(&address.to_string())
                && !body.contains(BEARER)
                && !body.contains(SECRET),
            "{path}"
        );
    }
    task.abort();
    let _ = task.await;
}
