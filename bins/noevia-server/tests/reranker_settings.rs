//! Synthetic HTTP, persistence and authorization checks for the native reranker owner.
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
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
