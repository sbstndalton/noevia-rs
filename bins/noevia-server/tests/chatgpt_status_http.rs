#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#[path = "support/chatgpt_status_http_controls.rs"]
mod controls;
use axum::{
    body::Body,
    extract::ConnectInfo,
    http::{Request, StatusCode},
};
use noevia_server::{
    chatgpt_status_routes,
    config::Config,
    serve::{Arrival, Conn},
    App,
};
use server_store::rusqlite::Connection;
use std::time::{Duration, Instant};
use tower::ServiceExt;
const SCHEMA: &str = include_str!("../../../crates/server-store/tests/fixtures/node-schema.sql");
fn config(directory: &std::path::Path, upstream: std::net::SocketAddr) -> Config {
    Config::from_lookup(|key| match key {
        "NOEVIA_RUST_AUTH" | "NOEVIA_RUST_CHATGPT_STATUS" => Some("1".into()),
        "UI_DATA_DIR" => Some(directory.to_str().unwrap().into()),
        "NOEVIA_LEGACY_UPSTREAM" => Some(format!("http://{upstream}")),
        "COWORK_CODE_NET_ADDR" => Some("127.0.0.2".into()),
        "NOEVIA_FEATURE_NATIVE_CLIENT_AUTH" => Some("true".into()),
        "TRUST_PROXY" => Some("true".into()),
        _ => None,
    })
    .unwrap()
}
fn request(path: &str, local: Option<&str>, arrival: Option<Instant>) -> Request<Body> {
    let mut r = Request::builder().uri(path).body(Body::empty()).unwrap();
    r.extensions_mut().insert(ConnectInfo(Conn {
        peer: "127.0.0.1:1".parse().unwrap(),
        local: local.map(|s| s.parse().unwrap()),
    }));
    if let Some(a) = arrival {
        r.extensions_mut().insert(Arrival(a));
    }
    r
}
#[test]
fn native_status_selector_and_path_controls() {
    assert!(
        Config::from_lookup(|k| (k == "NOEVIA_RUST_CHATGPT_STATUS").then(|| "1".into())).is_err()
    );
    assert!(Config::from_lookup(|k| match k {
        "NOEVIA_RUST_AUTH" | "NOEVIA_RUST_CHATGPT_STATUS" => Some("1".into()),
        "UI_DATA_DIR" => Some("relative".into()),
        _ => None,
    })
    .is_err());
    assert!(
        !Config::from_lookup(
            |k| (k == "NOEVIA_LEGACY_UPSTREAM").then(|| "http://127.0.0.1:1".into())
        )
        .unwrap()
        .rust_chatgpt_status
    );
    for (path, selected) in [
        ("/api/providers/chatgpt", true),
        ("/api/providers/chatgpt?x=1", true),
        ("/api/providers/x/../chatgpt", true),
        ("/api/providers/chatgpt/", false),
        ("/api/providers/chatgpt/x", false),
    ] {
        assert_eq!(
            chatgpt_status_routes::candidate(&request(path, None, None)),
            selected,
            "M6_HTTP_ASSERT/path"
        );
    }
    for method in ["HEAD", "OPTIONS", "POST", "get"] {
        let r = Request::builder()
            .method(method)
            .uri("/api/providers/chatgpt")
            .body(Body::empty())
            .unwrap();
        assert!(
            !chatgpt_status_routes::candidate(&r),
            "M6_HTTP_ASSERT/method"
        );
    }
    let path = format!("/{}", "a".repeat(8192));
    assert!(
        chatgpt_status_routes::candidate(&request(&path, None, None)),
        "M6_HTTP_ASSERT/overcap-path"
    );
}
#[cfg(feature = "rust-chatgpt-status")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_status_closed_http_pipeline() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("secrets.key"), [7; 32]).unwrap();
    let database = Connection::open(server_store::db_path(directory.path())).unwrap();
    database.execute_batch(SCHEMA).unwrap();
    database.execute_batch("CREATE TABLE chatgpt_oauth_tokens(user_id TEXT PRIMARY KEY,data_enc TEXT NOT NULL,state TEXT NOT NULL DEFAULT 'connected',updated_at INTEGER NOT NULL); INSERT INTO settings VALUES('feature:chatgptOAuth','true');").unwrap();
    let now = noevia_server::identity::now_ms();
    database.execute_batch(&format!("INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,created_at,updated_at) VALUES('u','A','a','A','admin','x','w',1,1); INSERT INTO sessions VALUES('{}','u','c',1,{}, {},'ua','ip'); INSERT INTO sessions VALUES('{}','u','c',1,{}, {},'ua','ip');", server_auth::js::digest("live"),now-1000,now+99999,server_auth::js::digest("idle"),now-604800001,now+99999)).unwrap();
    database.execute_batch(&format!("INSERT INTO device_grants VALUES('g','u','Mac',1,{}, {},'ip','ua'); INSERT INTO device_tokens VALUES('{}','g','access',1,{},NULL,NULL);",now-60000,now+99999,server_auth::js::digest("nva_device"),now+99999)).unwrap();
    let upstream = controls::Fixture::upstream().await;
    let cfg = config(directory.path(), upstream.address);
    let app = App::new(cfg);
    let owner = app
        .chatgpt_status
        .as_ref()
        .unwrap()
        .as_ref()
        .unwrap()
        .clone();
    let deadline = Instant::now() + Duration::from_secs(3);
    while !owner.ready() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(owner.ready(), "M6_HTTP_ASSERT/startup");
    let front = controls::Fixture::front(app.clone()).await;
    let (status, headers, body) = controls::request(
        front.address,
        "GET",
        "/api/providers/chatgpt?x=1",
        Some("cowork_session=live"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "M6_HTTP_ASSERT/intercept");
    assert!(
        body.contains("\"state\":\"disconnected\""),
        "M6_HTTP_ASSERT/status"
    );
    assert_eq!(
        headers.get("cache-control").unwrap(),
        "no-store",
        "M6_HTTP_ASSERT/cache"
    );
    assert_eq!(
        headers.get("x-content-type-options").unwrap(),
        "nosniff",
        "M6_HTTP_ASSERT/security"
    );
    let (status, headers, _) = controls::request(
        front.address,
        "GET",
        "/api/providers/chatgpt",
        Some("cowork_session=idle"),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "M6_HTTP_ASSERT/upkeep");
    assert_eq!(
        headers.get("www-authenticate").unwrap(),
        "Bearer realm=\"cowork\"",
        "M6_HTTP_ASSERT/challenge"
    );
    assert_eq!(
        database
            .query_row(
                "SELECT count(*) FROM sessions WHERE id_hash=?1",
                [server_auth::js::digest("idle")],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        0,
        "M6_HTTP_ASSERT/upkeep-delta"
    );
    let (status, _, _) = controls::request_headers(
        front.address,
        "GET",
        "/api/providers/chatgpt",
        &[("authorization", "Bearer nva_device")],
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "M6_HTTP_ASSERT/device-browser"
    );
    assert!(
        database
            .query_row(
                "SELECT last_used_at FROM device_grants WHERE id='g'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap()
            >= now,
        "M6_HTTP_ASSERT/device-upkeep-touch"
    );
    let deadline = Instant::now() + Duration::from_secs(1);
    while owner.active_jobs() != Some(0) && Instant::now() < deadline {
        tokio::task::yield_now().await;
    }
    assert_eq!(owner.active_jobs(), Some(0), "M6_HTTP_ASSERT/device-reaped");
    let (status, headers, _) = controls::request_headers(
        front.address,
        "GET",
        "/api/providers/chatgpt",
        &[
            ("authorization", "Bearer nva_device"),
            ("cookie", "cowork_session=live"),
        ],
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "M6_HTTP_ASSERT/mixed-credential"
    );
    assert_eq!(
        headers.get("www-authenticate").unwrap(),
        "Bearer realm=\"cowork\"",
        "M6_HTTP_ASSERT/mixed-challenge"
    );
    let deadline = Instant::now() + Duration::from_secs(1);
    while owner.active_jobs() != Some(0) && Instant::now() < deadline {
        tokio::task::yield_now().await;
    }
    assert_eq!(owner.active_jobs(), Some(0), "M6_HTTP_ASSERT/mixed-reaped");
    let (status, _, _) = controls::request_headers(
        front.address,
        "GET",
        "/api/providers/chatgpt",
        &[
            ("authorization", ""),
            ("authorization", "Bearer nva_device"),
            ("cookie", "cowork_session=live"),
        ],
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "M6_HTTP_ASSERT/empty-first-authorization"
    );
    for (method, path) in [
        ("GET", "/api/providers/chatgpt/"),
        ("HEAD", "/api/providers/chatgpt"),
        ("OPTIONS", "/api/providers/chatgpt"),
        ("GET", "/api/providers/chatgpt/x"),
    ] {
        assert_eq!(
            controls::request(front.address, method, path, Some("cowork_session=live"))
                .await
                .0,
            StatusCode::IM_A_TEAPOT,
            "M6_HTTP_ASSERT/node-ownership"
        );
    }
    let router = noevia_server::router(app.clone());
    for arrival in [
        None,
        Some(Instant::now() + Duration::from_secs(1)),
        Some(Instant::now() - Duration::from_secs(3)),
    ] {
        assert_eq!(
            router
                .clone()
                .oneshot(request(
                    "/api/providers/chatgpt",
                    Some("127.0.0.1:1"),
                    arrival
                ))
                .await
                .unwrap()
                .status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "M6_HTTP_ASSERT/arrival"
        );
    }
    for local in [None, Some("127.0.0.2:1")] {
        let response = router
            .clone()
            .oneshot(request("/api/providers/chatgpt", local, None))
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::FORBIDDEN,
            "M6_HTTP_ASSERT/prefilter"
        );
        assert_eq!(
            controls::body(response).await,
            "",
            "M6_HTTP_ASSERT/bare-deny"
        );
    }
    let path = format!("/api/providers/chatgpt?{}", "x".repeat(8192));
    assert_eq!(
        router
            .clone()
            .oneshot(request(&path, Some("127.0.0.1:1"), Some(Instant::now())))
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST,
        "M6_HTTP_ASSERT/query-cap"
    );
    let deadline = Instant::now() + Duration::from_secs(1);
    while owner.active_jobs() != Some(0) && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    assert_eq!(
        owner.active_jobs(),
        Some(0),
        "M6_HTTP_ASSERT/maintenance-reap"
    );
    assert_eq!(
        noevia_server::health::ready_with_native(&app.config.upstream, None, false)
            .await
            .status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "M6_HTTP_ASSERT/native-ready-and"
    );
    database.execute_batch("INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,created_at,updated_at) VALUES('other','Other','other','Other','member','x','z',2,2); INSERT INTO chatgpt_oauth_tokens VALUES('u','{\"email\":\"alice@example.invalid\",\"plan\":\"synthetic-alpha\"}','connected',1),('other','{\"email\":\"other@example.invalid\",\"plan\":\"synthetic-other\"}','connected',1);").unwrap();
    let (status, _, body) = controls::request(
        front.address,
        "GET",
        "/api/providers/chatgpt",
        Some("cowork_session=live"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "M6_HTTP_ASSERT/owner-status");
    assert!(
        body.contains("synthetic-alpha") && !body.contains("synthetic-other"),
        "M6_HTTP_ASSERT/owner-isolated"
    );
    let deadline = Instant::now() + Duration::from_secs(1);
    while owner.active_jobs() != Some(0) && Instant::now() < deadline {
        tokio::task::yield_now().await;
    }
    assert_eq!(owner.active_jobs(), Some(0), "M6_HTTP_ASSERT/owner-reaped");
    database.execute_batch("UPDATE settings SET value='false' WHERE key='feature:chatgptOAuth'; DROP TABLE chatgpt_oauth_tokens;").unwrap();
    assert_eq!(
        controls::request(
            front.address,
            "GET",
            "/api/providers/chatgpt",
            Some("cowork_session=live")
        )
        .await
        .0,
        StatusCode::NOT_FOUND,
        "M6_HTTP_ASSERT/product-before-schema"
    );
    assert_eq!(
        controls::request(front.address, "GET", "/api/ready", None)
            .await
            .0,
        StatusCode::OK,
        "M6_HTTP_ASSERT/product-off-ready"
    );
    owner.request_shutdown();
    front.finish().await;
    drop(app);
    // Explicit native controller/thread/job reap happens outside Tokio.
    let joined = std::thread::spawn(chatgpt_status_routes::shutdown_process_owners);
    while !joined.is_finished() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(joined.join().unwrap(), "M6_HTTP_ASSERT/shutdown");
    upstream.finish().await;
}

#[test]
fn native_status_finite_ingress_caps() {
    use noevia_server::chatgpt_worker::JobInput;
    fn input<'a>(headers: impl IntoIterator<Item = (&'a str, &'a [u8])>) -> bool {
        JobInput::from_headers(
            Instant::now(),
            1_800_000_000_000,
            "GET",
            "/api/providers/chatgpt",
            None,
            headers,
        )
        .is_ok()
    }
    for count in 0..=70 {
        assert_eq!(
            input((0..count).map(|_| ("irrelevant", b"".as_slice()))),
            count <= 64,
            "M6_INGRESS_ASSERT/count"
        );
    }
    for size in [0, 1, 127, 128, 129, 256] {
        let name = "x".repeat(size);
        assert_eq!(
            input([(name.as_str(), b"".as_slice())]),
            size <= 128,
            "M6_INGRESS_ASSERT/name"
        );
    }
    for size in [8191, 8192, 8193] {
        let value = vec![b'a'; size];
        assert_eq!(
            input([("cookie", value.as_slice())]),
            size <= 8192,
            "M6_INGRESS_ASSERT/relevant"
        );
    }
    for ascii in [125, 126, 127] {
        let mut values = vec![vec![0xff; 128]; 64];
        for byte in values.iter_mut().flatten().take(ascii) {
            *byte = b'a';
        }
        assert_eq!(
            input(values.iter().map(|v| ("cookie", v.as_slice()))),
            ascii >= 126,
            "M6_INGRESS_ASSERT/latin1-projection"
        );
    }
    // Ignored Authorization duplicates still consume raw-count and relevant caps.
    let duplicate = vec![0xff; 8192];
    assert!(
        input([
            ("authorization", b"".as_slice()),
            ("authorization", duplicate.as_slice())
        ]),
        "M6_INGRESS_ASSERT/ignored-duplicate"
    );
    let duplicate = vec![0xff; 8193];
    assert!(
        !input([
            ("authorization", b"".as_slice()),
            ("authorization", duplicate.as_slice())
        ]),
        "M6_INGRESS_ASSERT/ignored-duplicate-cap"
    );
}
