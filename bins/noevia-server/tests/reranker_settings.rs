//! Synthetic HTTP, persistence and authorization checks for the native reranker owner.
#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
#[path = "mcp_support/mod.rs"]
mod support;
use server_store::rusqlite;
use server_store::rusqlite::OptionalExtension;
use support::*;
async fn get(addr: std::net::SocketAddr, path: &str, cookie: &str) -> (u16, String, String) {
    support::raw_text(addr, "GET", path, cookie, "", b"").await
}
async fn raw(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    cookie: &str,
    body: &[u8],
) -> (u16, String, String) {
    support::raw_text(
        addr,
        method,
        path,
        cookie,
        &format!("Origin: http://{addr}\r\nX-CSRF-Token: csrf\r\n"),
        body,
    )
    .await
}

const PATH: &str = "/api/models/reranker";
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auth_validation_reset_and_unreachable_engine() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path());
    let front = front_env(
        dir.path(),
        true,
        &[
            ("RERANK_BACKEND", "tei".into()),
            ("MODEL_MANAGER_KIND", "llamacpp".into()),
            ("INFERENCE_BASE_URL", "http://127.0.0.1:9/v1".into()),
        ],
    )
    .await;
    assert_eq!(get(front.addr, PATH, "").await.0, 401);
    assert_eq!(get(front.addr, PATH, MEMBER).await.0, 403);
    assert_eq!(
        raw_text(front.addr, "PUT", PATH, ADMIN, "", b"{}").await.0,
        403
    );
    let (status, _, body) = get(front.addr, PATH, ADMIN).await;
    assert_eq!(status, 200);
    assert_eq!(j(&body)["source"], "deployment");
    for (input, expected) in [
        ("null", "invalidInput"),
        ("{}", "invalidInput"),
        ("{\"enabled\":true,\"choice\":null}", "noReranker"),
        (
            "{\"enabled\":true,\"choice\":\"unknown\"}",
            "unknownReranker",
        ),
        ("{", "invalidInput"),
    ] {
        let (status, _, body) = raw(front.addr, "PUT", PATH, ADMIN, input.as_bytes()).await;
        assert_eq!(status, 400, "{input} {body}");
        assert_eq!(j(&body)["errorId"], expected);
        if input == "{" {
            assert_eq!(j(&body)["error"], "invalid JSON");
        }
    }
    let (status, _, body) = raw(
        front.addr,
        "PUT",
        PATH,
        ADMIN,
        b"{\"enabled\":true,\"choice\":\"tei\"}",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(j(&body)["source"], "admin");
    assert_eq!(j(&body)["enabled"], true);
    let (status, _, body) = raw(front.addr, "PUT", PATH, ADMIN, b"{\"reset\":true}").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(j(&body)["source"], "deployment");
    assert_eq!(raw(front.addr, "DELETE", PATH, ADMIN, b"").await.0, 200);
    assert_eq!(raw(front.addr, "POST", PATH, ADMIN, b"{}").await.0, 405);
    let db = rusqlite::Connection::open(dir.path().join("cowork.db")).unwrap();
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM settings WHERE key='rerank:configuration'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM audit_events WHERE action IN ('rerank.configure','rerank.reset')",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        3
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_puts_persist_one_complete_configuration() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path());
    let front = front_env(dir.path(), true, &[("RERANK_BACKEND", "tei".into())]).await;
    let (a, b) = tokio::join!(
        raw(
            front.addr,
            "PUT",
            PATH,
            ADMIN,
            b"{\"enabled\":true,\"choice\":\"tei\"}"
        ),
        raw(
            front.addr,
            "PUT",
            PATH,
            ADMIN,
            b"{\"enabled\":false,\"choice\":null}"
        )
    );
    assert_eq!(a.0, 200, "{}", a.2);
    assert_eq!(b.0, 200, "{}", b.2);
    let db = rusqlite::Connection::open(dir.path().join("cowork.db")).unwrap();
    let stored: String = db
        .query_row(
            "SELECT value FROM settings WHERE key='rerank:configuration'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!([
        j("{\"enabled\":true,\"choice\":\"tei\"}"),
        j("{\"enabled\":false,\"choice\":null}")
    ]
    .contains(&j(&stored)));
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM audit_events WHERE action='rerank.configure'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        2
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn existing_settings_row_and_private_client_protocol_are_compatible() {
    let dir = tempfile::tempdir().unwrap();
    let db = seed(dir.path());
    db.execute(
        "INSERT INTO settings(key,value) VALUES('rerank:configuration',?1)",
        ["{\"enabled\":true,\"choice\":\"llamacpp:synthetic-stale\"}"],
    )
    .unwrap();
    let front = front_env(
        dir.path(),
        true,
        &[
            ("RERANK_BACKEND", "tei".into()),
            ("RERANK_API_KEY", "synthetic-key".into()),
        ],
    )
    .await;
    let (status, _, body) = get(front.addr, PATH, ADMIN).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(j(&body)["choice"], "llamacpp:synthetic-stale");
    assert_eq!(j(&body)["enabled"], false);
    assert_eq!(j(&body)["source"], "admin");
    let private = "/v1/reranker/settings";
    assert_eq!(
        support::raw_text(front.store, "POST", private, "", "", b"{}")
            .await
            .0,
        401
    );
    let (status, _, body) = support::raw_text(
        front.store,
        "POST",
        private,
        "",
        &format!("X-Noevia-Store-Token: {STORE_TOKEN}\r\n"),
        b"{\"installed\":[]}",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(j(&body)["env"]["NOEVIA_FEATURE_RAG_RERANK"], "false");
    assert_eq!(j(&body)["view"]["choice"], "llamacpp:synthetic-stale");
    let initial_revision = j(&body)["revision"].as_str().unwrap().to_owned();
    let (boot, sequence) = initial_revision.split_once(':').unwrap();
    assert_eq!(boot.len(), 32);
    assert!(boot
        .bytes()
        .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()));
    assert_eq!(sequence, "0");
    let (status, _, body) = raw(front.addr, "PUT", PATH, ADMIN, b"{\"enabled\":false}").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(j(&body)["choice"], "llamacpp:synthetic-stale");
    assert_eq!(
        db.query_row(
            "SELECT value FROM settings WHERE key='rerank:configuration'",
            [],
            |r| r.get::<_, String>(0)
        )
        .map(|s| j(&s))
        .unwrap(),
        j("{\"enabled\":false,\"choice\":\"llamacpp:synthetic-stale\"}")
    );
    // A second identical save must still invalidate the engine cache, as the old onChange did.
    assert_eq!(
        raw(front.addr, "PUT", PATH, ADMIN, b"{\"enabled\":false}")
            .await
            .0,
        200
    );
    let (status, _, body) = support::raw_text(
        front.store,
        "POST",
        private,
        "",
        &format!("X-Noevia-Store-Token: {STORE_TOKEN}\r\n"),
        b"{}",
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(j(&body)["revision"], format!("{boot}:2"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replay_original_node_http_oracle_with_unreachable_catalogue() {
    let cases: Vec<serde_json::Value> =
        serde_json::from_str(include_str!("fixtures/reranker-http.json")).unwrap();
    for case in cases {
        let dir = tempfile::tempdir().unwrap();
        let db = seed(dir.path());
        if !case["seed"].is_null() {
            db.execute(
                "INSERT INTO settings(key,value) VALUES('rerank:configuration',?1)",
                [format!(
                    "{{\"enabled\":{},\"choice\":{}}}",
                    case["seed"]["enabled"], case["seed"]["choice"]
                )],
            )
            .unwrap();
        }
        let front = front_env(
            dir.path(),
            true,
            &[
                ("RERANK_BACKEND", "tei".into()),
                ("NOEVIA_FEATURE_RAG_RERANK", "false".into()),
            ],
        )
        .await;
        let cookie = match case["role"].as_str() {
            Some("admin") => ADMIN,
            Some("member") => MEMBER,
            _ => "",
        };
        let extra = if case["csrf"] == false {
            String::new()
        } else {
            format!("Origin: http://{}\r\nX-CSRF-Token: csrf\r\n", front.addr)
        };
        let (status, _, body) = support::raw_text(
            front.addr,
            case["method"].as_str().unwrap(),
            PATH,
            cookie,
            &extra,
            case["raw"].as_str().unwrap_or("").as_bytes(),
        )
        .await;
        assert_eq!(
            serde_json::json!(status),
            case["status"],
            "{} {body}",
            case["name"]
        );
        assert_eq!(j(&body), case["body"], "{}", case["name"]);
        let stored: Option<String> = db
            .query_row(
                "SELECT value FROM settings WHERE key='rerank:configuration'",
                [],
                |r| r.get(0),
            )
            .optional()
            .unwrap();
        assert_eq!(
            serde_json::json!(stored),
            case["stored"],
            "{}",
            case["name"]
        );
    }
}

// Each worker is a separate OS process, with its own App, store readers and writer.
#[test]
fn independent_front_worker() {
    let Ok(dir) = std::env::var("RERANK_TEST_DIR") else {
        return;
    };
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let front = front_env(
            std::path::Path::new(&dir),
            true,
            &[("RERANK_BACKEND", "tei".into())],
        )
        .await;
        std::fs::write(
            std::env::var("RERANK_TEST_READY").unwrap(),
            format!("{} {}", front.addr, front.store),
        )
        .unwrap();
        std::future::pending::<()>().await;
    });
}
struct Worker(std::process::Child);
impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
async fn worker(
    dir: &std::path::Path,
    name: &str,
) -> (Worker, std::net::SocketAddr, std::net::SocketAddr) {
    let ready = dir.join(name);
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "independent_front_worker", "--nocapture"])
        .env("RERANK_TEST_DIR", dir)
        .env("RERANK_TEST_READY", &ready)
        .spawn()
        .unwrap();
    let child = Worker(child);
    for _ in 0..500 {
        if let Ok(text) = std::fs::read_to_string(&ready) {
            let mut addresses = text.split_whitespace();
            return (
                child,
                addresses.next().unwrap().parse().unwrap(),
                addresses.next().unwrap().parse().unwrap(),
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("independent front did not start");
}
async fn snapshot(store: std::net::SocketAddr) -> serde_json::Value {
    let (status, _, body) = support::raw_text(
        store,
        "POST",
        "/v1/reranker/settings",
        "",
        &format!("X-Noevia-Store-Token: {STORE_TOKEN}\r\n"),
        b"{}",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    j(&body)
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn independent_process_mutations_invalidate_consumer_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let db = seed(dir.path());
    let (_a, a, store_a) = worker(dir.path(), "a-ready").await;
    let (_b, b, store_b) = worker(dir.path(), "b-ready").await;
    if let Ok(script) = std::env::var("RERANKER_CONSUMER_TEST") {
        let status = std::process::Command::new("node")
            .args(["--test", &script])
            .env("RERANK_TEST_FRONT", format!("http://{b}"))
            .env("RERANK_TEST_STORE_PORT", store_a.port().to_string())
            .status()
            .unwrap();
        assert!(
            status.success(),
            "real Node consumer must invalidate after remote writes"
        );
    }
    let audit_baseline = db
        .query_row(
            "SELECT count(*) FROM audit_events WHERE action LIKE 'rerank.%'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .unwrap();
    let x = br#"{"enabled":true,"choice":"tei"}"#;
    assert_eq!(raw(a, "PUT", PATH, ADMIN, x).await.0, 200);
    let mut cached = snapshot(store_a).await;
    // Identical saves, ABA between reads, repeated resets, and the reverse writer direction.
    for (writer, method, body) in [
        (b, "PUT", x.as_slice()),
        (b, "PUT", x.as_slice()),
        (b, "PUT", br#"{"enabled":false,"choice":"tei"}"#.as_slice()),
        (b, "PUT", x.as_slice()),
        (b, "DELETE", b"".as_slice()),
        (b, "DELETE", b"".as_slice()),
        (a, "PUT", x.as_slice()),
    ] {
        assert_eq!(raw(writer, method, PATH, ADMIN, body).await.0, 200);
        let next = snapshot(store_a).await;
        assert_ne!(
            cached["revision"], next["revision"],
            "successful remote mutation must invalidate the consumer cache"
        );
        assert_eq!(next, snapshot(store_b).await);
        cached = next;
    }
    let before = cached.clone();
    assert_eq!(raw(b, "PUT", PATH, ADMIN, b"{}").await.0, 400);
    assert_eq!(before, snapshot(store_a).await);
    // Successful mutations, generation and audit all advance together.
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM audit_events WHERE action LIKE 'rerank.%'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        audit_baseline + 8
    );
}

// The two original Core public-route tests execute here through the actual
// native HTTP listener. Direct Node retirement tests are a separate boundary.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn original_public_reranker_auth_methods_and_address_redaction() {
    let dir = tempfile::tempdir().unwrap();
    let db = seed(dir.path());
    let engine = upstream().await;
    engine.set(|_| reply("500 Internal Server Error", "{}"));
    let env = [
        ("RERANK_BACKEND", "tei".into()),
        ("NOEVIA_FEATURE_RAG_RERANK", "true".into()),
        ("MODEL_MANAGER_KIND", "llamacpp".into()),
        ("MODEL_MANAGER_BASE_URL", engine.base()),
    ];
    let front = front_env(dir.path(), true, &env).await;
    let before = snapshot(front.store).await;
    let initial = engine.seen().len();
    for method in ["GET", "PUT", "DELETE"] {
        // Invalid input must not outrank authentication/role or touch the catalogue.
        assert_eq!(raw(front.addr, method, PATH, "", b"{").await.0, 401);
        assert_eq!(raw(front.addr, method, PATH, MEMBER, b"{").await.0, 403);
    }
    assert_eq!(engine.seen().len(), initial);
    for method in ["PUT", "DELETE"] {
        assert_eq!(
            support::raw_text(front.addr, method, PATH, ADMIN, "", b"{}")
                .await
                .0,
            403
        );
    }
    assert_eq!(snapshot(front.store).await, before);
    assert!(audits(&db, "rerank.%").is_empty());
    let (status, _, text) = get(front.addr, PATH, ADMIN).await;
    let body = j(&text);
    assert_eq!(
        (status, &body["enabled"], &body["choice"]),
        (200, &serde_json::json!(true), &serde_json::json!("tei"))
    );
    assert_eq!(body["source"], "deployment");
    assert_eq!(
        body["options"],
        serde_json::json!([
            {"id":"tei","backend":"tei","model":"gte-multilingual-reranker-base"}
        ])
    );
    for private in ["http://", "https://", "rerank:8080", "127.0.0.1", "10.0.0."] {
        assert!(!text.contains(private), "{text}");
    }
    assert_eq!(raw(front.addr, "POST", PATH, ADMIN, b"{}").await.0, 405);
    assert_eq!(snapshot(front.store).await, before);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn original_public_reranker_save_errors_put_reset_and_delete() {
    let dir = tempfile::tempdir().unwrap();
    let db = seed(dir.path());
    let env = [
        ("RERANK_BACKEND", "tei".into()),
        ("NOEVIA_FEATURE_RAG_RERANK", "true".into()),
    ];
    let front = front_env(dir.path(), true, &env).await;
    let before = snapshot(front.store).await;
    let (status, _, text) = raw(front.addr, "PUT", PATH, ADMIN, br#"{"enabled":false}"#).await;
    let off = j(&text);
    assert_eq!(
        (status, &off["enabled"], &off["source"]),
        (200, &serde_json::json!(false), &serde_json::json!("admin"))
    );
    let audit: (String, String, String) = db.query_row(
        "SELECT action,actor_user_id,detail FROM audit_events WHERE action='rerank.configure' ORDER BY id DESC LIMIT 1",
        [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap();
    assert_eq!((&*audit.0, &*audit.1), ("rerank.configure", A));
    assert_eq!(
        j(&audit.2),
        serde_json::json!({"enabled":false,"backend":"tei","model":"gte-multilingual-reranker-base"})
    );
    let stored: String = db
        .query_row(
            "SELECT value FROM settings WHERE key='rerank:configuration'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        j(&stored),
        serde_json::json!({"enabled":false,"choice":"tei"})
    );
    let saved = snapshot(front.store).await;
    assert_ne!(saved["revision"], before["revision"]);
    let restarted = front_env(dir.path(), true, &env).await;
    assert_eq!(
        j(&get(restarted.addr, PATH, ADMIN).await.2)["enabled"],
        false
    );
    for (input, error) in [
        (
            br#"{"enabled":true,"choice":"llamacpp:x"}"#.as_slice(),
            "unknownReranker",
        ),
        (b"{".as_slice(), "invalidInput"),
    ] {
        let (status, _, text) = raw(front.addr, "PUT", PATH, ADMIN, input).await;
        assert_eq!(
            (status, j(&text)["errorId"].as_str()),
            (400, Some(error)),
            "{text}"
        );
        assert_eq!(snapshot(front.store).await, saved);
        assert_eq!(audits(&db, "rerank.%"), ["rerank.configure"]);
    }
    let (status, _, text) = raw(front.addr, "PUT", PATH, ADMIN, br#"{"reset":true}"#).await;
    assert_eq!(
        (status, j(&text)["source"].as_str()),
        (200, Some("deployment"))
    );
    let reset = snapshot(front.store).await;
    assert_ne!(reset["revision"], saved["revision"]);
    assert_eq!(
        raw(front.addr, "PUT", PATH, ADMIN, br#"{"enabled":false}"#)
            .await
            .0,
        200
    );
    let second = snapshot(front.store).await;
    assert_ne!(second["revision"], reset["revision"]);
    let (status, _, text) = raw(front.addr, "DELETE", PATH, ADMIN, b"").await;
    let deleted = j(&text);
    assert_eq!(
        (status, &deleted["source"], &deleted["enabled"]),
        (
            200,
            &serde_json::json!("deployment"),
            &serde_json::json!(true)
        )
    );
    assert_ne!(snapshot(front.store).await["revision"], second["revision"]);
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM settings WHERE key='rerank:configuration'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    assert_eq!(
        audits(&db, "rerank.%"),
        [
            "rerank.configure",
            "rerank.reset",
            "rerank.configure",
            "rerank.reset"
        ]
    );
    let actors: i64 = db
        .query_row(
            "SELECT count(*) FROM audit_events WHERE action LIKE 'rerank.%' AND actor_user_id!=?1",
            [A],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        actors, 0,
        "a member or another tenant must never be credited with the admin mutation"
    );
}
