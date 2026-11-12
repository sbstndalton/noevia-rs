#![allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::expect_used
)]
use noevia_server::inference_machines::*;
use serde_json::{json, Value};
#[test]
fn node_registry_sequence() {
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/machines-registry-node.json")).unwrap();
    let mut s = empty();
    let bodies = [
        json!({}),
        json!({"name":"box","address":"8.8.8.8"}),
        json!({"name":"box","address":"192.168.1.2:8080"}),
        json!({"name":"box","address":"192.168.1.2"}),
        json!({"name":"BOX","address":"192.168.1.3"}),
        json!({"name":"other","address":"192.168.1.2"}),
        json!({"action":"assign","service":"decision","machine":"<machine>","port":8040}),
        json!({"action":"remove","id":"<machine>"}),
        json!({"action":"assign","service":"decision","machine":"local"}),
        json!({"action":"remove","id":"<machine>"}),
    ];
    assert_eq!(view(&s), fixture["cases"][0]["body"]);
    for (i, mut b) in bodies.into_iter().enumerate() {
        if b.get("action").is_none() {
            b["action"] = "add".into()
        }
        let actual = mutate(&mut s, &b, "<machine>", false);
        let expected = &fixture["cases"][i + 1];
        match actual {
            Ok(v) => {
                assert_eq!(expected["status"], 200);
                assert_eq!(v, expected["body"], "case {i}")
            }
            Err(e) => {
                assert_eq!(e.0 as u64, expected["status"].as_u64().unwrap(), "case {i}");
                assert_eq!(e.1["code"], expected["body"]["code"], "case {i}");
            }
        }
    }
    assert_eq!(
        s,
        serde_json::from_str::<Value>(fixture["state"].as_str().unwrap()).unwrap()
    );
}
#[path = "mcp_support/mod.rs"]
mod support;
use support::*;
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn route_method_role_and_invalid_body_oracle() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path());
    let owner = upstream().await;
    owner.set(|r| {
        assert_eq!(r.path, "/internal/inference-machines/probes");
        let request = j(&r.body);
        let mut view = request["view"].clone();
        if request["health"] == true {
            let cases = j(include_str!("fixtures/machines-routes-node.json"));
            view["health"] = cases
                .as_array()
                .unwrap()
                .iter()
                .find(|case| {
                    case["role"] == "admin"
                        && case["method"] == "GET"
                        && case["query"] == "?health=1"
                })
                .unwrap()["output"]["health"]
                .clone();
        }
        reply("200 OK", &view.to_string())
    });
    let front = front_env(
        dir.path(),
        true,
        &[
            ("NOEVIA_LEGACY_UPSTREAM", owner.base()),
            ("NOEVIA_PROJECT_STORE_TOKEN", STORE_TOKEN.into()),
        ],
    )
    .await;
    let cases: Value =
        serde_json::from_str(include_str!("fixtures/machines-routes-node.json")).unwrap();
    for case in cases.as_array().unwrap() {
        let method = case["method"].as_str().unwrap();
        let path = format!("{}{}", PATH, case["query"].as_str().unwrap());
        let cookie = match case["role"].as_str() {
            Some("admin") => ADMIN,
            Some("member") => MEMBER,
            _ => "",
        };
        let body = if case["invalid"] == true {
            "{".into()
        } else {
            case["body"].to_string()
        };
        let (status, output) = write(front.addr, method, &path, cookie, &body).await;
        assert_eq!(status, case["status"].as_u64().unwrap() as u16, "{case}");
        if method != "HEAD" {
            assert_eq!(output, case["output"], "{case}");
        }
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mutation_csrf_concurrency_and_state_compatibility() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path());
    let owner = upstream().await;
    owner.set(|r| reply("200 OK", &j(&r.body)["view"].to_string()));
    let front = front_env(
        dir.path(),
        true,
        &[
            ("NOEVIA_LEGACY_UPSTREAM", owner.base()),
            ("NOEVIA_PROJECT_STORE_TOKEN", STORE_TOKEN.into()),
        ],
    )
    .await;
    assert_eq!(
        raw(
            front.addr,
            "POST",
            PATH,
            ADMIN,
            "",
            br#"{"action":"add","name":"box","address":"192.168.1.2"}"#
        )
        .await
        .0,
        403
    );
    let body = r#"{"action":"add","name":"box","address":"192.168.1.2"}"#;
    let (a, b) = tokio::join!(
        write(front.addr, "POST", PATH, ADMIN, body),
        write(front.addr, "POST", PATH, ADMIN, body)
    );
    let mut statuses = [a.0, b.0];
    statuses.sort();
    assert_eq!(statuses, [201, 409]);
    let added = if a.0 == 201 { a.1 } else { b.1 };
    let id = added["machine"]["id"].as_str().unwrap();
    assert_eq!(
        write(
            front.addr,
            "POST",
            PATH,
            ADMIN,
            &json!({"action":"assign","service":"decision","machine":id,"port":8040}).to_string()
        )
        .await
        .0,
        200
    );
    let (status, body) = write(
        front.addr,
        "POST",
        PATH,
        ADMIN,
        &json!({"action":"remove","id":id}).to_string(),
    )
    .await;
    assert_eq!(status, 409);
    assert_eq!(body["code"], "inUse");
    let db = server_store::rusqlite::Connection::open(dir.path().join("cowork.db")).unwrap();
    let raw: String = db
        .query_row(
            "SELECT value FROM settings WHERE key='inference:machines'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let saved = state(Some(&raw));
    assert_eq!(saved["version"], 1);
    assert_eq!(saved["assignments"]["decision"]["port"], 8040);
    db.execute("UPDATE settings SET value=?1 WHERE key='inference:machines'",[r#"{"version":1,"machines":[{"id":"old","name":"Node","address":"http://192.168.1.3"}],"assignments":{"decision":{"machine":"old","port":8041}}}"#]).unwrap();
    let (status, body) = get(front.addr, PATH, ADMIN).await;
    assert_eq!(status, 200);
    assert!(body["machines"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["id"] == "old"));
}
async fn private(front: &Front, body: Value) -> (u16, Value) {
    raw(
        front.store,
        "POST",
        "/v1/inference-machines",
        "",
        &format!("X-Noevia-Store-Token: {STORE_TOKEN}\r\n"),
        body.to_string().as_bytes(),
    )
    .await
}
fn tune_registry(dir: &std::path::Path, port: u16) {
    let db = server_store::rusqlite::Connection::open(dir.join("cowork.db")).unwrap();
    db.execute("INSERT OR REPLACE INTO settings(key,value) VALUES('inference:machines',?1)",[json!({"version":1,"machines":[{"id":"synthetic-machine","name":"Synthetic","address":"http://127.0.0.1"}],"assignments":{"chatFast":{"machine":"synthetic-machine","port":port,"model":"synthetic-model"}}}).to_string()]).unwrap();
}
async fn terminal(front: &Front) -> Value {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let (status, value) = get(front.addr, &format!("{PATH}?tune=1"), ADMIN).await;
        assert_eq!(status, 200);
        if value["tune"]["finishedAt"].is_string() {
            return value["tune"].clone();
        }
        assert!(
            std::time::Instant::now() < deadline,
            "measurement did not settle: {value}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_tune_matches_node_script_and_result_using_synthetic_http_only() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path());
    let db = server_store::rusqlite::Connection::open(dir.path().join("cowork.db")).unwrap();
    db.execute("INSERT INTO settings(key,value) VALUES('inference:machineTune',?1)", [r#"{"results":[null,{}, {"machineId":"older-machine","model":"older-model","synthetic":true}]}"#]).unwrap();
    let engine = upstream().await;
    engine.set(|r|if r.path.starts_with("/props"){reply("200 OK",r#"{"default_generation_settings":{"n_ctx":8192}}"#)}else{assert_eq!(r.path,"/v1/chat/completions");reply("200 OK",r#"{"timings":{"predicted_per_second":40,"prompt_per_second":20,"prompt_n":4096,"draft_n":10,"draft_n_accepted":4},"usage":{"completion_tokens":10}}"#)});
    tune_registry(dir.path(), engine.addr.port());
    let owner = upstream().await;
    owner.set(|r| {
        if r.path.ends_with("/ready") {
            reply("200 OK", r#"{"ok":true}"#)
        } else {
            reply("200 OK", &j(&r.body)["view"].to_string())
        }
    });
    let front = front_env(
        dir.path(),
        true,
        &[
            ("NOEVIA_LEGACY_UPSTREAM", owner.base()),
            ("NOEVIA_PROJECT_STORE_TOKEN", STORE_TOKEN.into()),
        ],
    )
    .await;
    assert_eq!(private(&front, json!({"op":"session"})).await.0, 200);
    let (status, started) = write(
        front.addr,
        "POST",
        PATH,
        ADMIN,
        r#"{"action":"tune","service":"chatFast","confirm":true}"#,
    )
    .await;
    assert_eq!(status, 202);
    assert_eq!(terminal(&front).await["state"], "done");
    let (status, mut actual) = private(
        &front,
        json!({"op":"tuneResult","machineId":"synthetic-machine","model":"synthetic-model"}),
    )
    .await;
    assert_eq!(status, 200);
    actual.as_object_mut().unwrap().remove("measuredAt");
    let saved: String = db
        .query_row(
            "SELECT value FROM settings WHERE key='inference:machineTune'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let saved = j(&saved);
    assert_eq!(saved["results"].as_array().unwrap().len(), 2);
    assert_eq!(
        saved["results"][0],
        json!({"machineId":"older-machine","model":"older-model","synthetic":true})
    );
    let fixture = j(include_str!("fixtures/machines-tune-node.json"));
    let mut started = started["tune"].clone();
    started["id"] = "<run>".into();
    started["startedAt"] = "<time>".into();
    assert_eq!(started, fixture["start"]);
    assert_eq!(
        js_json::parse(&actual.to_string()).unwrap(),
        js_json::parse(&fixture["result"].to_string()).unwrap()
    );
    let requests: Vec<Value> = engine
        .seen()
        .into_iter()
        .map(|r| {
            let mut body = serde_json::from_str::<Value>(&r.body).unwrap_or(Value::Null);
            if let Some(content) = body["messages"][0]["content"].as_str() {
                if content.starts_with("Measurement ") {
                    let suffix = content.split_once('.').unwrap().1;
                    body["messages"][0]["content"] = format!("Measurement <run>.{suffix}").into();
                }
            }
            json!({"path":r.path.split('?').next().unwrap(),"method":r.method,"body":body})
        })
        .collect();
    assert_eq!(json!(requests), fixture["requests"]);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_chat_interrupts_tune_and_explicit_abort_closes_the_run() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path());
    let engine = upstream().await;
    engine.set(|_| (std::time::Duration::from_secs(10), String::new()));
    tune_registry(dir.path(), engine.addr.port());
    let owner = upstream().await;
    owner.set(|_| reply("200 OK", r#"{"ok":true}"#));
    let front = front_env(
        dir.path(),
        true,
        &[
            ("NOEVIA_LEGACY_UPSTREAM", owner.base()),
            ("NOEVIA_PROJECT_STORE_TOKEN", STORE_TOKEN.into()),
        ],
    )
    .await;
    let (_, session) = private(&front, json!({"op":"session"})).await;
    let epoch = session["epoch"].clone();
    for (token, abort) in [("turn-one", false), ("turn-two", true)] {
        assert_eq!(
            write(
                front.addr,
                "POST",
                PATH,
                ADMIN,
                r#"{"action":"tune","service":"chatFast","confirm":true}"#
            )
            .await
            .0,
            202
        );
        assert_eq!(
            write(
                front.addr,
                "POST",
                PATH,
                ADMIN,
                r#"{"action":"tune","service":"chatFast","confirm":true}"#
            )
            .await
            .0,
            409
        );
        if abort {
            assert_eq!(
                write(front.addr, "POST", PATH, ADMIN, r#"{"action":"tuneAbort"}"#)
                    .await
                    .0,
                200
            );
        } else {
            assert_eq!(private(&front,json!({"op":"turnBegin","epoch":epoch,"sequence":1,"token":token,"machine":"synthetic-machine"})).await.0,200);
        }
        let status = terminal(&front).await;
        assert_eq!(
            status["state"],
            if abort { "aborted" } else { "interrupted" }
        );
        if !abort {
            let (status, body) = write(
                front.addr,
                "POST",
                PATH,
                ADMIN,
                r#"{"action":"tune","service":"chatFast","confirm":true}"#,
            )
            .await;
            assert_eq!(status, 409);
            assert_eq!(body["code"], "machineBusy");
            assert_eq!(
                private(
                    &front,
                    json!({"op":"turnEnd","epoch":epoch,"sequence":1,"token":token})
                )
                .await
                .0,
                200
            );
        }
    }
    let db = server_store::rusqlite::Connection::open(dir.path().join("cowork.db")).unwrap();
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM settings WHERE key='inference:machineTune'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unreachable_machine_cannot_start_a_measurement() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path());
    tune_registry(dir.path(), 1);
    let owner = upstream().await;
    owner.set(|_| {
        reply(
            "200 OK",
            r#"{"ok":false,"code":"machineUnreachable","error":"synthetic machine unavailable"}"#,
        )
    });
    let front = front_env(
        dir.path(),
        true,
        &[
            ("NOEVIA_LEGACY_UPSTREAM", owner.base()),
            ("NOEVIA_PROJECT_STORE_TOKEN", STORE_TOKEN.into()),
        ],
    )
    .await;
    assert_eq!(private(&front, json!({"op":"session"})).await.0, 200);
    let (status, body) = write(
        front.addr,
        "POST",
        PATH,
        ADMIN,
        r#"{"action":"tune","service":"chatFast","confirm":true}"#,
    )
    .await;
    assert_eq!(status, 503);
    assert_eq!(body["code"], "machineUnreachable");
    assert_eq!(
        get(front.addr, &format!("{PATH}?tune=1"), ADMIN).await.1["tune"],
        Value::Null
    );
}
#[test]
fn node_address_validation_oracle() {
    let cases = j(include_str!("fixtures/machines-address-node.json"));
    for case in cases.as_array().unwrap() {
        match address(&case["raw"]) {
            Ok(a) => {
                assert_eq!(case["status"], 200, "{case}");
                assert_eq!(a, case["address"], "{case}")
            }
            Err(e) => {
                assert_eq!(e.0 as u64, case["status"].as_u64().unwrap(), "{case}");
                assert_eq!(e.1["error"], case["error"], "{case}");
                assert_eq!(e.1["code"], case["code"], "{case}")
            }
        }
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn abort_drops_the_http_connection_at_every_script_await() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    for pause in 0..6 {
        let dir = tempfile::tempdir().unwrap();
        seed(dir.path());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let entered = std::sync::Arc::new(tokio::sync::Notify::new());
        let closed = std::sync::Arc::new(tokio::sync::Notify::new());
        let entered_task = entered.clone();
        let closed_task = closed.clone();
        let engine = tokio::spawn(async move {
            for index in 0..=pause {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                let header_end = loop {
                    let mut chunk = [0u8; 4096];
                    let n = socket.read(&mut chunk).await.unwrap();
                    assert!(n > 0);
                    bytes.extend_from_slice(&chunk[..n]);
                    if let Some(pos) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                        break pos + 4;
                    }
                };
                let headers = String::from_utf8_lossy(&bytes[..header_end]);
                let length = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .and_then(|s| s.trim().parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                while bytes.len() < header_end + length {
                    let mut chunk = [0u8; 4096];
                    let n = socket.read(&mut chunk).await.unwrap();
                    assert!(n > 0);
                    bytes.extend_from_slice(&chunk[..n]);
                }
                if index == pause {
                    entered_task.notify_one();
                    let mut byte = [0u8; 1];
                    assert_eq!(socket.read(&mut byte).await.unwrap(), 0);
                    closed_task.notify_one();
                    return;
                }
                let body = if index == 0 {
                    r#"{"default_generation_settings":{"n_ctx":8192}}"#
                } else {
                    r#"{"timings":{"predicted_per_second":40,"prompt_per_second":20,"prompt_n":4096}}"#
                };
                socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
            }
        });
        tune_registry(dir.path(), port);
        let owner = upstream().await;
        owner.set(|_| reply("200 OK", r#"{"ok":true}"#));
        let front = front_env(
            dir.path(),
            true,
            &[
                ("NOEVIA_LEGACY_UPSTREAM", owner.base()),
                ("NOEVIA_PROJECT_STORE_TOKEN", STORE_TOKEN.into()),
            ],
        )
        .await;
        assert_eq!(private(&front, json!({"op":"session"})).await.0, 200);
        assert_eq!(
            write(
                front.addr,
                "POST",
                PATH,
                ADMIN,
                r#"{"action":"tune","service":"chatFast","confirm":true}"#
            )
            .await
            .0,
            202
        );
        tokio::time::timeout(std::time::Duration::from_secs(5), entered.notified())
            .await
            .unwrap();
        assert_eq!(
            write(front.addr, "POST", PATH, ADMIN, r#"{"action":"tuneAbort"}"#)
                .await
                .0,
            200
        );
        tokio::time::timeout(std::time::Duration::from_secs(5), closed.notified())
            .await
            .unwrap();
        engine.await.unwrap();
        assert_eq!(terminal(&front).await["state"], "aborted");
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancellation_during_readiness_leaves_no_run_reservation() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path());
    tune_registry(dir.path(), 1);
    let owner = upstream().await;
    owner.set(|_| (std::time::Duration::from_secs(10), String::new()));
    let front = front_env(
        dir.path(),
        true,
        &[
            ("NOEVIA_LEGACY_UPSTREAM", owner.base()),
            ("NOEVIA_PROJECT_STORE_TOKEN", STORE_TOKEN.into()),
        ],
    )
    .await;
    private(&front, json!({"op":"session"})).await;
    let app = front.app.clone();
    let launch = tokio::spawn(async move {
        noevia_server::machine_service::start(
            &app,
            &json!({"service":"chatFast","confirm":true,"actor":A}),
        )
        .await
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while owner.count("/internal/inference-machines/ready") == 0 {
        assert!(std::time::Instant::now() < deadline);
        tokio::task::yield_now().await;
    }
    launch.abort();
    assert!(launch.await.unwrap_err().is_cancelled());
    assert_eq!(
        get(front.addr, &format!("{PATH}?tune=1"), ADMIN).await.1["tune"],
        Value::Null
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn registry_audit_failure_preserves_nodes_already_committed_state() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path());
    let db = server_store::rusqlite::Connection::open(dir.path().join("cowork.db")).unwrap();
    db.execute_batch("CREATE TRIGGER synthetic_audit_failure BEFORE INSERT ON audit_events WHEN NEW.action='machines.add' BEGIN SELECT RAISE(FAIL,'synthetic audit failure'); END;").unwrap();
    let front = front_env(
        dir.path(),
        true,
        &[("NOEVIA_PROJECT_STORE_TOKEN", STORE_TOKEN.into())],
    )
    .await;
    let (status, body) = write(
        front.addr,
        "POST",
        PATH,
        ADMIN,
        r#"{"action":"add","name":"box","address":"192.168.1.2"}"#,
    )
    .await;
    assert_eq!(status, 500);
    assert_eq!(
        body,
        json!({"error":"The inference machines could not be changed."})
    );
    let raw: String = db
        .query_row(
            "SELECT value FROM settings WHERE key='inference:machines'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(state(Some(&raw))["machines"][0]["name"], "box");
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_assignment_checks_served_models_and_machine_moves_require_confirmation() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path());
    let engine = upstream().await;
    engine.set(|r| {
        assert_eq!(r.path, "/v1/models");
        reply(
            "200 OK",
            r#"{"data":[{"id":null,"model":"synthetic-model"},{"id":false,"model":"wrong"},{"id":"","model":"wrong"}]}"#,
        )
    });
    let owner = upstream().await;
    owner.set(|r| reply("200 OK", &j(&r.body)["view"].to_string()));
    let front = front_env(
        dir.path(),
        true,
        &[
            ("NOEVIA_LEGACY_UPSTREAM", owner.base()),
            ("NOEVIA_PROJECT_STORE_TOKEN", STORE_TOKEN.into()),
        ],
    )
    .await;
    let (status, added) = write(
        front.addr,
        "POST",
        PATH,
        ADMIN,
        r#"{"action":"add","name":"Synthetic","address":"127.0.0.1"}"#,
    )
    .await;
    assert_eq!(status, 201);
    let id = added["machine"]["id"].as_str().unwrap();
    let (status, listed) = get(
        front.addr,
        &format!("{PATH}?models={id}&port={}", engine.addr.port()),
        ADMIN,
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(listed["models"], json!(["synthetic-model"]));
    let mut body = json!({"action":"assign","service":"chatFast","machine":id,"port":engine.addr.port(),"model":"not-served"});
    let (status, error) = write(front.addr, "POST", PATH, ADMIN, &body.to_string()).await;
    assert_eq!(status, 400);
    assert_eq!(error["code"], "unknownModel");
    assert_eq!(error["models"], json!(["synthetic-model"]));
    body["model"] = " synthetic-model ".into();
    assert_eq!(
        write(front.addr, "POST", PATH, ADMIN, &body.to_string())
            .await
            .0,
        200
    );
    assert_eq!(
        write(
            front.addr,
            "POST",
            PATH,
            ADMIN,
            &json!({"action":"remove","id":id}).to_string()
        )
        .await
        .1["services"],
        json!(["chatFast"])
    );
    engine.set(|_| {
        reply(
            "200 OK",
            r#"{"models":[{"model":null,"name":"synthetic-model"}]}"#,
        )
    });
    let (_, moved) = write(
        front.addr,
        "POST",
        PATH,
        ADMIN,
        &json!({"action":"update","id":id,"address":"localhost"}).to_string(),
    )
    .await;
    assert!(moved["services"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == "chatFast")
        .unwrap()
        .get("needsConfirm")
        .is_none());
    engine.set(|_| reply("200 OK", r#"{"data":[{"id":"other-model"}]}"#));
    let (status, moved) = write(
        front.addr,
        "POST",
        PATH,
        ADMIN,
        &json!({"action":"update","id":id,"address":"127.0.0.1"}).to_string(),
    )
    .await;
    assert_eq!(status, 200);
    let role = moved["services"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == "chatFast")
        .unwrap();
    assert_eq!(role["needsConfirm"], true);
    body["model"] = "other-model".into();
    let (status, confirmed) = write(front.addr, "POST", PATH, ADMIN, &body.to_string()).await;
    assert_eq!(status, 200);
    let role = confirmed["services"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == "chatFast")
        .unwrap();
    assert!(role.get("needsConfirm").is_none());
    assert_eq!(
        write(
            front.addr,
            "POST",
            PATH,
            ADMIN,
            &json!({"action":"assign","service":"chatFast","machine":"local"}).to_string()
        )
        .await
        .0,
        200
    );
    assert_eq!(
        write(
            front.addr,
            "POST",
            PATH,
            ADMIN,
            &json!({"action":"remove","id":id}).to_string()
        )
        .await
        .0,
        200
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_late_begin_and_an_old_session_cannot_recreate_or_erase_chat_coverage() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path());
    tune_registry(dir.path(), 1);
    let owner = upstream().await;
    owner.set(|_| {
        reply(
            "200 OK",
            r#"{"ok":false,"code":"machineUnreachable","error":"synthetic unavailable"}"#,
        )
    });
    let front = front_env(
        dir.path(),
        true,
        &[
            ("NOEVIA_LEGACY_UPSTREAM", owner.base()),
            ("NOEVIA_PROJECT_STORE_TOKEN", STORE_TOKEN.into()),
        ],
    )
    .await;
    let (_, first) = private(&front, json!({"op":"session"})).await;
    let old = first["epoch"].clone();
    assert_eq!(
        private(
            &front,
            json!({"op":"turnEnd","epoch":old,"sequence":7,"token":"late"})
        )
        .await
        .0,
        200
    );
    assert_eq!(private(&front,json!({"op":"turnBegin","epoch":old,"sequence":7,"token":"late","machine":"synthetic-machine"})).await.0,503);
    let (_, second) = private(&front, json!({"op":"session"})).await;
    let current = second["epoch"].clone();
    assert_ne!(old, current);
    assert_eq!(private(&front,json!({"op":"turnBegin","epoch":current,"sequence":1,"token":"same-token","machine":"synthetic-machine"})).await.0,200);
    assert_eq!(
        private(
            &front,
            json!({"op":"turnEnd","epoch":old,"sequence":8,"token":"same-token"})
        )
        .await
        .0,
        200
    );
    let (status, body) = write(
        front.addr,
        "POST",
        PATH,
        ADMIN,
        r#"{"action":"tune","service":"chatFast","confirm":true}"#,
    )
    .await;
    assert_eq!(status, 409);
    assert_eq!(body["code"], "machineBusy");
    assert_eq!(owner.count("/internal/inference-machines/ready"), 0);
    assert_eq!(
        private(
            &front,
            json!({"op":"turnEnd","epoch":current,"sequence":1,"token":"same-token"})
        )
        .await
        .0,
        200
    );
    assert_eq!(
        write(
            front.addr,
            "POST",
            PATH,
            ADMIN,
            r#"{"action":"tune","service":"chatFast","confirm":true}"#
        )
        .await
        .0,
        503
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn existing_private_use_names_remain_readable_and_mutable() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path());
    let db = server_store::rusqlite::Connection::open(dir.path().join("cowork.db")).unwrap();
    let raw = json!({"version":1,"machines":[{"id":"box","name":"\u{10ffff}","address":"http://192.168.1.2"}],"assignments":{}}).to_string();
    db.execute(
        "INSERT INTO settings(key,value) VALUES('inference:machines',?1)",
        [&raw],
    )
    .unwrap();
    let owner = upstream().await;
    owner.set(|r| reply("200 OK", &j(&r.body)["view"].to_string()));
    let front = front_env(
        dir.path(),
        true,
        &[
            ("NOEVIA_PROJECT_STORE_TOKEN", STORE_TOKEN.into()),
            ("NOEVIA_LEGACY_UPSTREAM", owner.base()),
        ],
    )
    .await;
    let (status, view) = get(front.addr, PATH, ADMIN).await;
    assert_eq!(status, 200);
    assert_eq!(view["machines"][1]["name"], "\u{10ffff}");
    let (status, body) = write(front.addr, "POST", PATH, ADMIN, "1e400").await;
    assert_eq!(status, 400);
    assert_eq!(body["error"], "A JSON object is required");
    drop(front);
    let front = front_env(
        dir.path(),
        true,
        &[
            ("NOEVIA_PROJECT_STORE_TOKEN", STORE_TOKEN.into()),
            ("NOEVIA_LEGACY_UPSTREAM", owner.base()),
        ],
    )
    .await;
    assert_eq!(
        write(
            front.addr,
            "POST",
            PATH,
            ADMIN,
            r#"{"action":"update","id":"box","name":"renamed"}"#
        )
        .await
        .0,
        200
    );
    let saved: String = db
        .query_row(
            "SELECT value FROM settings WHERE key='inference:machines'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(state(Some(&saved))["machines"][0]["name"], "renamed");
}

#[test]
fn accepted_json_classes_round_trip_and_invalid_json_is_refused() {
    assert_eq!(
        name(&parse_json(&format!("\"{}\"", "\u{10f800}".repeat(30))).unwrap())
            .unwrap()
            .chars()
            .count(),
        30
    );
    assert!(name(&parse_json(&format!("\"{}\"", "\u{10f800}".repeat(31))).unwrap()).is_err());
    assert_ne!(parse_json(r#""\ud800""#), parse_json("\"\u{10f800}\""));
    assert_eq!(
        encode_json(&parse_json(r#""\ud800""#).unwrap()),
        r#""\ud800""#
    );
    assert_eq!(
        encode_json(&parse_json("\"\u{10f800}\"").unwrap()),
        "\"\u{10f800}\""
    );
    assert_eq!(
        encode_json(&parse_json(r#"{"\ue000N":true}"#).unwrap()),
        "{\"\u{e000}N\":true}"
    );
    assert_eq!(name(&parse_json("1e400").unwrap()).unwrap(), "Infinity");
    assert!(port(&parse_json("1e400").unwrap()).is_err());
    assert_eq!(encode_json(&parse_json("1e400").unwrap()), "null");
    for raw in [
        r#"{"action":"add","name":"\udbfe\udc00","address":"192.168.1.2","ignored":1e400}"#
            .to_string(),
        format!(
            "{{\"action\":\"add\",\"name\":\"box\",\"address\":\"192.168.1.2\",\"ignored\":{}{}}}",
            "[".repeat(300),
            "]".repeat(300)
        ),
    ] {
        let body = parse_json(&raw).unwrap();
        let mut s = empty();
        mutate(&mut s, &body, "box", false).unwrap();
        assert_eq!(state(Some(&encode_json(&s))), s);
    }
    for raw in ["{\"ignored\":1e400,}", "[1,]", "{\"x\":[}"] {
        assert!(parse_json(raw).is_none());
    }
    for raw in [
        r#""\ud800""#,
        "\"\u{10f800}\"",
        "\"\u{e000}\"",
        r#""\ue000\ud800""#,
    ] {
        let value = parse_json(raw).unwrap();
        assert_eq!(parse_json(&encode_json(&value)).unwrap(), value);
    }
}

// ---- resource safety (review r2): every case runs in its own process on a small stack, so a
// stack overflow, abort or runaway parse fails this test instead of taking the harness down.
const ISOLATED: &str = "NOEVIA_MACHINES_ISOLATED_CASE";
fn isolated(case: &str, body: impl FnOnce() + Send + 'static) {
    if std::env::var_os(ISOLATED).is_some() {
        std::thread::Builder::new()
            .stack_size(512 * 1024)
            .spawn(body)
            .unwrap()
            .join()
            .unwrap_or_else(|e| std::panic::resume_unwind(e));
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let (out, err) = (dir.path().join("out"), dir.path().join("err"));
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([case, "--exact", "--test-threads=1", "--nocapture"])
        .env(ISOLATED, "1")
        .stdout(std::fs::File::create(&out).unwrap())
        .stderr(std::fs::File::create(&err).unwrap())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if std::time::Instant::now() > deadline {
            child.kill().unwrap();
            let _ = child.wait();
            panic!("{case}: still running after 90 s (runaway parse or drop)");
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    let stdout = std::fs::read_to_string(&out).unwrap_or_default();
    let stderr = std::fs::read_to_string(&err).unwrap_or_default();
    assert!(
        status.success() && stdout.contains("1 passed"),
        "{case}: {status:?}\n{stdout}\n{}",
        stderr.chars().take(2000).collect::<String>()
    );
}
fn run(f: impl std::future::Future<Output = ()>) {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_stack_size(1 << 20)
        .enable_all()
        .build()
        .unwrap()
        .block_on(f)
}
fn nested(n: usize, leaf: &str) -> String {
    format!("{}{leaf}{}", "[".repeat(n), "]".repeat(n))
}
fn within(what: &str, secs: u64, f: impl FnOnce()) {
    let start = std::time::Instant::now();
    f();
    assert!(
        start.elapsed() < std::time::Duration::from_secs(secs),
        "{what} took {:?}",
        start.elapsed()
    );
}
const BODY_CAP: usize = 1024 * 1024;

#[test]
fn near_cap_deep_and_wide_json_is_parsed_and_dropped_in_bounded_time() {
    isolated(
        "near_cap_deep_and_wide_json_is_parsed_and_dropped_in_bounded_time",
        || {
            let head = r#"{"action":"invalid","ignored":"#;
            let arrays = (BODY_CAP - head.len() - 8) / 2;
            let objects = (BODY_CAP - head.len() - 8) / 6;
            let mixed = (BODY_CAP - head.len() - 8) / 8;
            let bodies = [
                format!("{head}{}0{}}}", "[".repeat(arrays), "]".repeat(arrays)),
                format!(
                    "{head}{}1{}}}",
                    "{\"a\":".repeat(objects),
                    "}".repeat(objects)
                ),
                format!(
                    "{head}{}0{}}}",
                    "[{\"a\":".repeat(mixed),
                    "}]".repeat(mixed)
                ),
                format!(
                    "{head}[{}0]}}",
                    "0,".repeat((BODY_CAP - head.len() - 8) / 2)
                ),
                format!(
                    "{head}{{{}\"z\":0}}}}",
                    (0..45_000)
                        .map(|i| format!("\"k{i}\":0,"))
                        .collect::<String>()
                ),
            ];
            for (i, body) in bodies.iter().enumerate() {
                assert!(body.len() <= BODY_CAP, "case {i}");
                within(&format!("parse and drop {i}"), 5, || {
                    let value = parse_json(body).unwrap_or_else(|| panic!("case {i} refused"));
                    assert_eq!(value["action"], "invalid");
                    assert!(value["ignored"].is_array() || value["ignored"].is_object());
                });
            }
            // Malformed deep documents are refused promptly and never half-built.
            let open = format!("{head}{}", "[".repeat(arrays));
            for bad in [
                open.clone(),
                format!("{open}0{}", "]".repeat(arrays - 1)),
                format!("{head}{}0{}}} x", "[".repeat(1000), "]".repeat(1000)),
            ] {
                within("refuse malformed deep document", 5, || {
                    assert!(parse_json(&bad).is_none())
                });
            }
        },
    )
}

#[test]
fn deep_values_that_are_read_keep_nodes_coercion_results_in_bounded_time() {
    isolated(
        "deep_values_that_are_read_keep_nodes_coercion_results_in_bounded_time",
        || {
            within("deep coercions", 10, || {
                for (depth, expected) in [(1000, Some("x")), (2500, Some("x")), (5000, None)] {
                    let value = parse_json(&nested(depth, "\"x\"")).unwrap();
                    assert_eq!(coerce(&value).ok().as_deref(), expected, "depth {depth}");
                    assert_eq!(name(&value).ok().as_deref(), expected, "name {depth}");
                }
                for depth in [200_000, BODY_CAP / 2 - 8] {
                    let value = parse_json(&nested(depth, "\"x\"")).unwrap();
                    assert_eq!(coerce(&value).unwrap_err().0, 500);
                    assert_eq!(name(&value).unwrap_err().0, 500);
                    assert_eq!(text(&value).len(), 1);
                    let _ = encode_json(&value);
                }
                // An object ends String()'s recursion: arrays deep inside it are never walked.
                let inner =
                    parse_json(&format!(r#"["a",{{"k":{}}}]"#, nested(300_000, "1"))).unwrap();
                assert_eq!(coerce(&inner).unwrap(), "a,[object Object]");
                let leaf = parse_json(&nested(300_000, r#"{"toString":1}"#)).unwrap();
                assert_eq!(coerce(&leaf).unwrap_err().0, 500);
            });
        },
    )
}

fn node_case_text(input: &Value) -> String {
    if let Some(raw) = input["raw"].as_str() {
        return raw.into();
    }
    if let Some(n) = input["nest"].as_array() {
        return nested(n[0].as_u64().unwrap() as usize, n[1].as_str().unwrap());
    }
    if let Some(n) = input["comma"].as_u64() {
        return (0..n).fold("\"x\"".to_string(), |s, _| format!("[\"p\",{s},\"q\"]"));
    }
    if let Some(n) = input["inObject"].as_u64() {
        return format!(r#"["a",{{"k":{}}}]"#, nested(n as usize, "1"));
    }
    if let Some(n) = input["wide"].as_u64() {
        return format!("[{}]", vec!["0"; n as usize].join(","));
    }
    panic!("unknown template {input}")
}

#[test]
fn replacement_coercion_matches_the_node_oracle() {
    isolated("replacement_coercion_matches_the_node_oracle", || {
        let fixture: Value =
            serde_json::from_str(include_str!("fixtures/machines-coerce-node.json")).unwrap();
        let cases = fixture["cases"].as_array().unwrap();
        assert!(cases.len() >= 64);
        within("oracle replay", 60, || {
            for case in cases {
                let text = node_case_text(&case["input"]);
                let label = format!("{:.80}", case["input"].to_string());
                let value = parse_json(&text).unwrap_or_else(|| panic!("refused {label}"));
                let coerced = coerce(&value);
                let expected = &case["coerce"];
                if text != "null" {
                    if expected.get("throws").is_some() {
                        let error = coerced.expect_err(&label);
                        assert_eq!(error.0, 500, "{label}");
                    } else if let Some(want) = expected["text"].as_str() {
                        assert_eq!(
                            coerced.as_ref().map(String::as_str).ok(),
                            Some(want),
                            "{label}"
                        );
                    } else {
                        let got = coerced.expect(&label);
                        assert_eq!(got.len() as u64, expected["len"], "{label}");
                        assert!(
                            got.starts_with(expected["head"].as_str().unwrap()),
                            "{label}"
                        );
                        assert!(got.ends_with(expected["tail"].as_str().unwrap()), "{label}");
                    }
                }
                let added = &case["add"];
                match (name(&value), added) {
                    (Ok(got), a) if a.get("ok").is_some() => assert_eq!(got, a["ok"], "{label}"),
                    (Err(e), a) if a.get("throws").is_some() => assert_eq!(e.0, 500, "{label}"),
                    (Err(e), a) if a.get("status").is_some() => {
                        assert_eq!(
                            (e.0 as u64, &e.1["code"]),
                            (a["status"].as_u64().unwrap(), &a["code"]),
                            "{label}"
                        )
                    }
                    (got, a) => panic!("{label}: {got:?} vs {a}"),
                }
            }
        });
    })
}

#[test]
fn hostile_to_string_names_are_refused_before_anything_is_saved() {
    isolated(
        "hostile_to_string_names_are_refused_before_anything_is_saved",
        || {
            run(async {
                let dir = tempfile::tempdir().unwrap();
                seed(dir.path());
                let owner = upstream().await;
                owner.set(|r| reply("200 OK", &j(&r.body)["view"].to_string()));
                let front = front_env(
                    dir.path(),
                    true,
                    &[
                        ("NOEVIA_LEGACY_UPSTREAM", owner.base()),
                        ("NOEVIA_PROJECT_STORE_TOKEN", STORE_TOKEN.into()),
                    ],
                )
                .await;
                let names = [
                    r#"{"toString":1}"#.to_string(),
                    r#"[{"toString":null}]"#.to_string(),
                    r#"["a",["b",{"toStrin\u0067":"x"}]]"#.to_string(),
                    nested(100_000, "{}"),
                    nested(5000, "\"x\""),
                ];
                for name in &names {
                    let body =
                        format!(r#"{{"action":"add","name":{name},"address":"192.168.1.2"}}"#);
                    let (status, error) = write(front.addr, "POST", PATH, ADMIN, &body).await;
                    assert_eq!(status, 500, "{:.60}", name);
                    assert_eq!(
                        error["error"],
                        "The inference machines could not be changed."
                    );
                }
                let db =
                    server_store::rusqlite::Connection::open(dir.path().join("cowork.db")).unwrap();
                let saved: Option<String> = db
                    .query_row(
                        "SELECT value FROM settings WHERE key='inference:machines'",
                        [],
                        |r| r.get(0),
                    )
                    .ok();
                assert!(saved.is_none(), "{saved:?}");
                let audits: i64 = db
                    .query_row(
                        "SELECT COUNT(*) FROM audit_events WHERE action LIKE 'machines.%'",
                        [],
                        |r| r.get(0),
                    )
                    .unwrap();
                assert_eq!(audits, 0);
                // Objects without their own toString, and arrays holding them, are accepted as before.
                for (name, address, want) in [
                    (r#"{"valueOf":1}"#, "192.168.1.2", "[object Object]"),
                    (
                        r#"[{"a":{"toString":1}},1]"#,
                        "192.168.1.3",
                        "[object Object],1",
                    ),
                ] {
                    let body = format!(r#"{{"action":"add","name":{name},"address":"{address}"}}"#);
                    let (status, added) = write(front.addr, "POST", PATH, ADMIN, &body).await;
                    assert_eq!(status, 201, "{added}");
                    assert_eq!(added["machine"]["name"], want);
                }
            })
        },
    )
}

#[test]
fn admin_posts_with_deep_ignored_fields_refuse_and_accept_without_overflow() {
    isolated(
        "admin_posts_with_deep_ignored_fields_refuse_and_accept_without_overflow",
        || {
            run(async {
                let dir = tempfile::tempdir().unwrap();
                seed(dir.path());
                let owner = upstream().await;
                owner.set(|r| reply("200 OK", &j(&r.body)["view"].to_string()));
                let front = front_env(
                    dir.path(),
                    true,
                    &[
                        ("NOEVIA_LEGACY_UPSTREAM", owner.base()),
                        ("NOEVIA_PROJECT_STORE_TOKEN", STORE_TOKEN.into()),
                    ],
                )
                .await;
                let deep = nested(520_000, "0");
                assert!(deep.len() < BODY_CAP - 200);
                for action in ["invalid", "tuneAbort"] {
                    let body = format!(r#"{{"action":"{action}","ignored":{deep}}}"#);
                    let start = std::time::Instant::now();
                    let (status, answer) = write(front.addr, "POST", PATH, ADMIN, &body).await;
                    assert!(start.elapsed() < std::time::Duration::from_secs(20));
                    if action == "invalid" {
                        assert_eq!((status, &answer["code"]), (400, &json!("invalidAction")));
                    } else {
                        assert_eq!(status, 200);
                    }
                }
                let body = format!(
                    r#"{{"action":"add","name":"deep","address":"192.168.1.2","ignored":{deep}}}"#
                );
                let (status, added) = write(front.addr, "POST", PATH, ADMIN, &body).await;
                assert_eq!(status, 201, "{added}");
                assert_eq!(added["machine"]["name"], "deep");
                let (status, view) = get(front.addr, PATH, ADMIN).await;
                assert_eq!(status, 200);
                assert_eq!(view["machines"][1]["name"], "deep");
            })
        },
    )
}

#[test]
fn remote_model_lists_and_saved_rows_with_deep_ignored_fields_stay_usable() {
    isolated(
        "remote_model_lists_and_saved_rows_with_deep_ignored_fields_stay_usable",
        || {
            run(async {
                let dir = tempfile::tempdir().unwrap();
                seed(dir.path());
                let db =
                    server_store::rusqlite::Connection::open(dir.path().join("cowork.db")).unwrap();
                let saved = format!(
                    r#"{{"version":1,"machines":[{{"id":"box","name":"Stored","address":"http://127.0.0.1","ignored":{}}}],"assignments":{{}},"also":{}}}"#,
                    nested(150_000, "0"),
                    nested(100_000, "\"x\"")
                );
                db.execute(
                    "INSERT INTO settings(key,value) VALUES('inference:machines',?1)",
                    [&saved],
                )
                .unwrap();
                let engine = upstream().await;
                let answer = format!(
                    r#"{{"data":[{{"id":"served-model","ignored":{}}}]}}"#,
                    nested(120_000, "0")
                );
                assert!(answer.len() < 256 * 1024, "{}", answer.len());
                let answer = std::sync::Arc::new(answer);
                let a = answer.clone();
                engine.set(move |_| reply("200 OK", &a));
                let owner = upstream().await;
                owner.set(|r| reply("200 OK", &j(&r.body)["view"].to_string()));
                let front = front_env(
                    dir.path(),
                    true,
                    &[
                        ("NOEVIA_LEGACY_UPSTREAM", owner.base()),
                        ("NOEVIA_PROJECT_STORE_TOKEN", STORE_TOKEN.into()),
                    ],
                )
                .await;
                let (status, view) = get(front.addr, PATH, ADMIN).await;
                assert_eq!(status, 200);
                assert_eq!(view["machines"][1]["name"], "Stored");
                let (status, listed) = get(
                    front.addr,
                    &format!("{PATH}?models=box&port={}", engine.addr.port()),
                    ADMIN,
                )
                .await;
                assert_eq!((status, &listed["models"]), (200, &json!(["served-model"])));
                // An answer a few bytes over the 256 KiB remote cap is still refused as too large.
                let big = format!(
                    r#"{{"data":[{{"id":"a","ignored":{}}}]}}"#,
                    nested(131_072, "0")
                );
                engine.set(move |_| reply("200 OK", &big));
                let (status, error) = get(
                    front.addr,
                    &format!("{PATH}?models=box&port={}", engine.addr.port()),
                    ADMIN,
                )
                .await;
                assert_eq!(
                    (status, &error["code"]),
                    (502, &json!("machineUnreachable"))
                );
                assert!(error["error"]
                    .as_str()
                    .unwrap()
                    .contains("did not list its models"));
                let (status, _) = write(
                    front.addr,
                    "POST",
                    PATH,
                    ADMIN,
                    r#"{"action":"update","id":"box","name":"Renamed"}"#,
                )
                .await;
                assert_eq!(status, 200);
                let row: String = db
                    .query_row(
                        "SELECT value FROM settings WHERE key='inference:machines'",
                        [],
                        |r| r.get(0),
                    )
                    .unwrap();
                assert_eq!(state(Some(&row))["machines"][0]["name"], "Renamed");
                assert!(row.len() < 1000, "ignored fields are not written back");
            })
        },
    )
}

#[test]
fn json_grammar_matches_json_parse_and_serde_on_every_single_edit() {
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/machines-coerce-node.json")).unwrap();
    for case in fixture["grammar"].as_array().unwrap() {
        let text = case["text"].as_str().unwrap();
        assert_eq!(
            parse_json(text).is_some(),
            case["accept"].as_bool().unwrap(),
            "{text:?}"
        );
    }
    let docs = [
        r#"{"a":[1,-2.5e+3,"xé😀\ud800",true,false,null,0.5],"b":{"c":{},"d":[]}}"#,
        r#"[0,[],{},"","\"\\\/\b\f\n\r\t",-0,1E5,{"k":[[1],[2]]}]"#,
        " \t\r\n{ \"a\" : [ 1 , 2 ] }\n",
    ];
    let junk = [
        "", ",", ":", "[", "]", "{", "}", "\"", "\\", "0", "-", ".", "e", "+", " ", "\n", "\u{1}",
        "t", "n", "\u{b}", "\u{feff}", "/",
    ];
    let mut checked = 0;
    // The same edits inside a document nested below the build depth, where the collapsing
    // reader (not the building one) validates them.
    let wrappers = [
        (String::new(), String::new()),
        ("[".repeat(33), "]".repeat(33)),
        ("{\"k\":".repeat(33), "}".repeat(33)),
    ];
    for doc in docs {
        assert!(parse_json(doc).is_some(), "{doc}");
        let cuts: Vec<usize> = doc
            .char_indices()
            .map(|(i, _)| i)
            .chain([doc.len()])
            .collect();
        for &at in &cuts {
            for piece in junk {
                let mut inserted = doc.to_string();
                inserted.insert_str(at, piece);
                let mut replaced = doc.to_string();
                if let Some(c) = doc[at..].chars().next() {
                    replaced.replace_range(at..at + c.len_utf8(), piece);
                }
                for edited in [inserted, replaced] {
                    for (open, close) in &wrappers {
                        let edited = format!("{open}{edited}{close}");
                        let want =
                            serde_json::from_str::<Box<serde_json::value::RawValue>>(&edited)
                                .is_ok();
                        assert_eq!(parse_json(&edited).is_some(), want, "{edited:?}");
                        checked += 1;
                    }
                }
            }
        }
    }
    assert!(checked > 3000);
}

#[test]
fn collapsed_subtrees_coerce_exactly_like_built_ones() {
    let docs = [
        r#""x""#,
        "null",
        "[]",
        "{}",
        r#"[null,[],[null],"a",1,-0,1e21,1e400,true,false]"#,
        r#"["a",{"b":[{"toString":1}]}]"#,
        r#"["a",[{"toString":1}]]"#,
        r#"{"toString":null}"#,
        r#"{"valueOf":1,"x":{"toString":1}}"#,
        r#"[["\ud800",["\ue000","\ud83d\ude00\u00e9"]],{"a":[1,2]}]"#,
        "[[],[[]],[[],[]]]",
        r#"[{"k":[[[["deep"]]]]},[["tail"]]]"#,
    ];
    for doc in docs {
        let built = coerce(&parse_json(&format!("[{doc}]")).unwrap());
        for depth in [2, 5, 31, 32, 33, 34, 40, 100, 1000] {
            let wrapped = parse_json(&nested(depth, doc)).unwrap();
            match (&built, coerce(&wrapped)) {
                (Ok(want), Ok(got)) => assert_eq!(*want, got, "{doc} at {depth}"),
                (Err(_), Err(got)) => assert_eq!(got.0, 500, "{doc} at {depth}"),
                (want, got) => panic!("{doc} at {depth}: {want:?} vs {got:?}"),
            }
        }
    }
}

#[test]
fn array_coercion_exception_refuses_at_the_qualified_http_boundary() {
    isolated(
        "array_coercion_exception_refuses_at_the_qualified_http_boundary",
        || {
            // Explicit HTTP compatibility exception qualified on pinned Node 22.23.3 /
            // V8 12.4.254.21-node.57. This is Rust's contract, not a floating V8 assertion.
            let boundary = 3086;
            let saved = [1, 128, boundary - 1];
            let refused = [boundary, boundary + 1, 5000, 100_000];
            for depth in saved {
                let value = parse_json(&nested(depth, "\"x\"")).unwrap();
                assert_eq!(coerce(&value).unwrap(), "x", "coerce {depth}");
                assert_eq!(name(&value).unwrap(), "x", "name {depth}");
            }
            for depth in refused {
                let value = parse_json(&nested(depth, "\"x\"")).unwrap();
                assert_eq!(coerce(&value).unwrap_err().0, 500, "coerce {depth}");
                let value = parse_json(&nested(depth, "{\"toString\":1}")).unwrap();
                assert_eq!(coerce(&value).unwrap_err().0, 500, "object leaf {depth}");
            }
            run(async move {
                let dir = tempfile::tempdir().unwrap();
                seed(dir.path());
                let owner = upstream().await;
                owner.set(|r| reply("200 OK", &j(&r.body)["view"].to_string()));
                let front = front_env(
                    dir.path(),
                    true,
                    &[
                        ("NOEVIA_LEGACY_UPSTREAM", owner.base()),
                        ("NOEVIA_PROJECT_STORE_TOKEN", STORE_TOKEN.into()),
                    ],
                )
                .await;
                let mut count = 0;
                for (i, depth) in saved.into_iter().enumerate() {
                    let body = format!(
                        r#"{{"action":"add","name":{},"address":"192.168.1.{}"}}"#,
                        nested(depth, &format!("\"m{i}\"")),
                        i + 2
                    );
                    let (status, added) = write(front.addr, "POST", PATH, ADMIN, &body).await;
                    assert_eq!(status, 201, "depth {depth}: {added}");
                    assert_eq!(added["machine"]["name"], format!("m{i}"));
                    count += 1;
                }
                for depth in refused {
                    let body = format!(
                        r#"{{"action":"add","name":{},"address":"192.168.1.200"}}"#,
                        nested(depth, "\"over\"")
                    );
                    let db = server_store::rusqlite::Connection::open(dir.path().join("cowork.db"))
                        .unwrap();
                    let before: String = db
                        .query_row(
                            "SELECT value FROM settings WHERE key='inference:machines'",
                            [],
                            |r| r.get(0),
                        )
                        .unwrap();
                    let audits: i64 = db
                        .query_row("SELECT count(*) FROM audit_events", [], |r| r.get(0))
                        .unwrap();
                    let (status, error) = write(front.addr, "POST", PATH, ADMIN, &body).await;
                    assert_eq!(status, 500, "depth {depth}");
                    assert_eq!(
                        error,
                        json!({"error":"The inference machines could not be changed."})
                    );
                    let after: String = db
                        .query_row(
                            "SELECT value FROM settings WHERE key='inference:machines'",
                            [],
                            |r| r.get(0),
                        )
                        .unwrap();
                    assert_eq!(after, before, "no registry write at {depth}");
                    assert_eq!(
                        db.query_row("SELECT count(*) FROM audit_events", [], |r| r
                            .get::<_, i64>(0))
                            .unwrap(),
                        audits,
                        "no audit write at {depth}"
                    );
                }
                let (_, view) = get(front.addr, PATH, ADMIN).await;
                assert_eq!(view["machines"].as_array().unwrap().len(), count + 1);
            })
        },
    )
}
