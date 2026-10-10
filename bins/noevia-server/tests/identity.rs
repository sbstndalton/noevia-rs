//! The validated-identity extractor (M2) on a test-only route: Node's gate verdicts and refusal
//! responses, failing closed while the store is unusable. The front's own router has no route
//! that uses it (front.rs checks every route still reaches its owner). Synthetic data only.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::get;
use axum::Router;
use http_body_util::BodyExt;
use noevia_server::config::Config;
use noevia_server::identity::{Status, ValidatedIdentity};
use noevia_server::App;
use server_store::rusqlite::Connection;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Arc;
use tower::ServiceExt;

const SCHEMA: &str = include_str!("../../../crates/server-store/tests/fixtures/node-schema.sql");
const SESSION: &str = "synthetic-session-token-0001";
const CSRF: &str = "synthetic-csrf-token-0001";
const DEVICE: &str = "nva_synthetic-device-token-0001";

fn hex(s: &str) -> String {
    Sha256::digest(s.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn now() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap()
}

/// A Node-shaped cowork.db with one member, one session and one device token.
fn seed(dir: &std::path::Path) -> Connection {
    let c = Connection::open(server_store::db_path(dir)).unwrap();
    c.execute_batch(SCHEMA).unwrap();
    let n = now();
    c.execute("INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,created_at,updated_at) VALUES('u-1','Member','member','Synthetic','admin','x','w',1,1)", []).unwrap();
    c.execute(
        "INSERT INTO sessions VALUES(?1,'u-1',?2,?3,?3,?4,'ua','127.0.0.1')",
        (hex(SESSION), hex(CSRF), n, n + 86_400_000),
    )
    .unwrap();
    c.execute(
        "INSERT INTO device_grants VALUES('g-1','u-1','Mac',?1,?1,?2,'ip','ua')",
        (n, n + 86_400_000),
    )
    .unwrap();
    c.execute(
        "INSERT INTO device_tokens VALUES(?1,'g-1','access',?2,?3,NULL,NULL)",
        (hex(DEVICE), n, n + 3_600_000),
    )
    .unwrap();
    c
}

fn app(env: &[(&str, &str)]) -> Arc<App> {
    let mut m: HashMap<String, String> = HashMap::new();
    m.insert("NOEVIA_LEGACY_UPSTREAM".into(), "http://127.0.0.1:9".into());
    for (k, v) in env {
        m.insert((*k).into(), (*v).into());
    }
    App::new(Config::from_lookup(|k| m.get(k).cloned()).unwrap())
}

async fn who(ValidatedIdentity(id): ValidatedIdentity) -> String {
    format!("{} {}", id.user.id, id.user.role.as_str())
}

fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/api/test/who", get(who).post(who))
        .route("/api/admin/who", get(who))
        .route("/api/{*rest}", get(who))
        .with_state(app)
}

async fn call(
    app: &Arc<App>,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
) -> (StatusCode, HashMap<String, String>, String) {
    let mut b = Request::builder().method(method).uri(path);
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    let res = router(Arc::clone(app))
        .oneshot(b.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let h = res
        .headers()
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_str().unwrap().to_string()))
        .collect();
    let body =
        String::from_utf8(res.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap();
    (status, h, body)
}

#[tokio::test]
async fn the_gate_answers_like_node() {
    let dir = tempfile::tempdir().unwrap();
    let _node = seed(dir.path());
    let d = dir.path().display().to_string();
    let app = app(&[
        ("UI_DATA_DIR", &d),
        ("PUBLIC_ORIGIN", "https://noevia.example.test"),
    ]);
    assert_eq!(app.identity.status(), Status::Ready);

    let (s, h, body) = call(&app, "GET", "/api/test/who", &[]).await;
    assert_eq!(
        (s, body.as_str()),
        (StatusCode::UNAUTHORIZED, r#"{"error":"unauthorized"}"#)
    );
    assert_eq!(h["www-authenticate"], "Bearer realm=\"cowork\"");
    assert_eq!(h["cache-control"], "no-store");
    assert_eq!(h["content-type"], "application/json");
    assert_eq!(h["x-content-type-options"], "nosniff");

    let cookie = format!("theme=dark; cowork_session={SESSION}; cowork_csrf={CSRF}");
    let (s, _, body) = call(&app, "GET", "/api/test/who", &[("cookie", &cookie)]).await;
    assert_eq!((s, body.as_str()), (StatusCode::OK, "u-1 admin"));

    // A write needs the CSRF header and a trusted origin.
    let (s, _, body) = call(&app, "POST", "/api/test/who", &[("cookie", &cookie)]).await;
    assert_eq!(
        (s, body.as_str()),
        (StatusCode::FORBIDDEN, r#"{"error":"invalid CSRF token"}"#)
    );
    let (s, _, _) = call(
        &app,
        "POST",
        "/api/test/who",
        &[
            ("cookie", &cookie),
            ("x-csrf-token", CSRF),
            ("origin", "https://evil.example.test"),
        ],
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _, body) = call(
        &app,
        "POST",
        "/api/test/who",
        &[
            ("cookie", &cookie),
            ("x-csrf-token", CSRF),
            ("origin", "https://noevia.example.test"),
        ],
    )
    .await;
    assert_eq!((s, body.as_str()), (StatusCode::OK, "u-1 admin"));

    // Device tokens only with nativeClientAuth (off here): the bearer is not a credential.
    let bearer = format!("Bearer {DEVICE}");
    let (s, _, _) = call(&app, "GET", "/api/test/who", &[("authorization", &bearer)]).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);

    // Rust reads only: the session row is exactly as Node left it.
    let conn = Connection::open(server_store::db_path(dir.path())).unwrap();
    let rows: i64 = conn
        .query_row("SELECT count(*) FROM sessions", [], |r| r.get(0))
        .unwrap();
    assert_eq!(rows, 1);
}

#[tokio::test]
async fn device_tokens_act_as_members_and_never_on_browser_only_paths() {
    let dir = tempfile::tempdir().unwrap();
    let _node = seed(dir.path());
    let d = dir.path().display().to_string();
    let app = app(&[
        ("UI_DATA_DIR", &d),
        ("TRUST_PROXY", "true"),
        ("NOEVIA_FEATURE_NATIVE_CLIENT_AUTH", "true"),
    ]);
    let bearer = format!("Bearer {DEVICE}");
    let (s, _, body) = call(&app, "GET", "/api/test/who", &[("authorization", &bearer)]).await;
    assert_eq!((s, body.as_str()), (StatusCode::OK, "u-1 member"));
    let (s, _, body) = call(&app, "GET", "/api/admin/who", &[("authorization", &bearer)]).await;
    assert_eq!(
        (s, body.as_str()),
        (
            StatusCode::FORBIDDEN,
            r#"{"error":"This needs a signed-in browser session.","code":"browser_session_required"}"#
        )
    );
    // A session cookie riding along voids the device token.
    let (s, _, _) = call(
        &app,
        "GET",
        "/api/test/who",
        &[("authorization", &bearer), ("cookie", "cowork_csrf=x")],
    )
    .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn fails_closed_until_the_store_is_usable() {
    let unavailable = |s: (StatusCode, HashMap<String, String>, String)| {
        assert_eq!(s.0, StatusCode::SERVICE_UNAVAILABLE);
        assert!(s.2.contains("unavailable"), "{}", s.2);
    };
    let cookie = format!("cowork_session={SESSION}");
    // No UI_DATA_DIR.
    let app0 = app(&[]);
    assert!(matches!(app0.identity.status(), Status::Unavailable(_)));
    unavailable(call(&app0, "GET", "/api/test/who", &[("cookie", &cookie)]).await);
    // No database yet; then Node creates it and the next request works without a restart.
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path().display().to_string();
    let app1 = app(&[("UI_DATA_DIR", &d)]);
    unavailable(call(&app1, "GET", "/api/test/who", &[("cookie", &cookie)]).await);
    assert!(
        !server_store::db_path(dir.path()).exists(),
        "the front never creates cowork.db"
    );
    let node = seed(dir.path());
    let (s, _, _) = call(&app1, "GET", "/api/test/who", &[("cookie", &cookie)]).await;
    assert_eq!(s, StatusCode::OK);
    // Node migrates past what this build knows: refused at once, still never "signed in".
    node.execute("INSERT INTO schema_migrations VALUES(6,0)", [])
        .unwrap();
    unavailable(call(&app1, "GET", "/api/test/who", &[("cookie", &cookie)]).await);
    assert!(
        matches!(app1.identity.status(), Status::Ready),
        "the opened store stays; each read rechecks"
    );
    let fresh = app(&[("UI_DATA_DIR", &d)]);
    assert!(matches!(fresh.identity.status(), Status::Refused(_)));
    // An invalid feature value (Node refuses to start on it): 503, and the front still builds.
    let bad = app(&[
        ("UI_DATA_DIR", &d),
        ("NOEVIA_FEATURE_NATIVE_CLIENT_AUTH", "maybe"),
    ]);
    assert!(matches!(bad.identity.status(), Status::Refused(_)));
}

/// Review F2: Node gates on the WHATWG-parsed pathname. A path parsing would change is refused
/// with Node's badRequestUrl 400 before the gate, so a device token can never reach a browser-only
/// path ("/api/admin/...") by spelling it "/api/test/../admin/...".
#[tokio::test]
async fn paths_whatwg_parsing_changes_are_refused_before_the_gate() {
    let dir = tempfile::tempdir().unwrap();
    let _node = seed(dir.path());
    let d = dir.path().display().to_string();
    let app = app(&[
        ("UI_DATA_DIR", &d),
        ("TRUST_PROXY", "true"),
        ("NOEVIA_FEATURE_NATIVE_CLIENT_AUTH", "true"),
    ]);
    let bearer = format!("Bearer {DEVICE}");
    for path in [
        "/api/test/../admin/who",
        "/api/test/%2e%2e/admin/who",
        "/api/test/%2E%2E/admin/who",
        "/api/test/.%2e/admin/who",
        "/api/test/./who",
        "/api/test/%2e/who",
        "/api/test/x/..",
    ] {
        for headers in [vec![("authorization", bearer.as_str())], vec![]] {
            let (s, h, body) = call(&app, "GET", path, &headers).await;
            assert_eq!(
                (s, body.as_str()),
                (StatusCode::BAD_REQUEST, r#"{"error":"invalid URL"}"#),
                "{path}"
            );
            assert_eq!(h["cache-control"], "no-store");
        }
    }
    // Backslashes: http::Uri accepts them and WHATWG turns them into "/", so the extractor refuses.
    for path in ["/api/test\\..\\admin\\who", "/api/test\\admin\\who"] {
        let uri: axum::http::Uri = path.parse().unwrap();
        let (s, _, _) = call(&app, "GET", &uri.to_string(), &[("authorization", &bearer)]).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{path}");
    }
    // Paths parsing leaves alone still reach the gate.
    let (s, _, body) = call(
        &app,
        "GET",
        "/api/test/a%2Fb..c",
        &[("authorization", &bearer)],
    )
    .await;
    assert_eq!((s, body.as_str()), (StatusCode::OK, "u-1 member"));
    let (s, _, body) = call(
        &app,
        "GET",
        "/api/test/who?x=/../admin",
        &[("authorization", &bearer)],
    )
    .await;
    assert_eq!((s, body.as_str()), (StatusCode::OK, "u-1 member"));
    let (s, _, _) = call(&app, "GET", "/api/admin/who", &[("authorization", &bearer)]).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
}
