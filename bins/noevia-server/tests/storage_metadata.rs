//! Source controls for the dark metadata slice; execution requires a separate grant.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
use noevia_server::{
    config::Config,
    routes::{dispatch_under, Native, Switches},
};
#[test]
fn exact_get_is_the_fronts_whenever_native_auth_is_on() {
    // sbstndalton/noevia#1071: no switch of its own; NOEVIA_RUST_AUTH=1 alone moves it.
    let on = Switches {
        rust_auth: true,
        ..Default::default()
    };
    for method in ["GET", "HEAD", "POST", "PUT", "DELETE", "OPTIONS"] {
        assert_eq!(
            dispatch_under(method, "/api/integrations/storage", Switches::default()),
            None
        );
        assert_eq!(
            dispatch_under(method, "/api/integrations/storage", on),
            // Only a build with the rust-storage-reads feature answers it (routes.rs compiled()).
            if method == "GET" && cfg!(feature = "rust-storage-reads") {
                Some(Native::StorageMetadata)
            } else {
                None
            }
        );
    }
    for path in [
        "/api/integrations/storage/",
        "/api/integrations/storage/folder",
    ] {
        assert_eq!(dispatch_under("GET", path, on), None);
    }
    // A leftover old switch value changes nothing, on or off.
    for value in [None, Some("0"), Some("1")] {
        let c = Config::from_lookup(|k| match k {
            "NOEVIA_LEGACY_UPSTREAM" => Some("http://127.0.0.1:9".into()),
            "NOEVIA_RUST_STORAGE_READS" => value.map(str::to_owned),
            _ => None,
        })
        .unwrap();
        assert!(!c.rust_storage_reads);
    }
    // Native auth without a data directory refuses to start rather than leave it unanswered.
    assert!(Config::from_lookup(|k| match k {
        "NOEVIA_LEGACY_UPSTREAM" => Some("http://127.0.0.1:9".into()),
        "NOEVIA_RUST_AUTH" => Some("1".into()),
        _ => None,
    })
    .is_err());
}

#[cfg(feature = "rust-storage-reads")]
#[tokio::test]
async fn real_headers_owner_projection_omission_and_gate_before_keys() {
    use axum::{body::Body, http::Request};
    use http_body_util::BodyExt;
    use noevia_server::{serve::Conn, storage_read_routes, App};
    use server_store::rusqlite::Connection;
    use sha2::{Digest, Sha256};
    let d = tempfile::tempdir().unwrap();
    let db = Connection::open(d.path().join("cowork.db")).unwrap();
    db.execute_batch(include_str!(
        "../../../crates/server-store/tests/fixtures/node-schema.sql"
    ))
    .unwrap();
    let id = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
    db.execute("INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,created_at,updated_at) VALUES(?1,?1,?1,'Synthetic','member','x',?1,1,1)",[id]).unwrap();
    let hash = |v: &str| {
        Sha256::digest(v.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    };
    let now = noevia_server::identity::now_ms();
    db.execute(
        "INSERT INTO sessions VALUES(?1,?2,?3,?4,?4,?5,'ua','127.0.0.1')",
        (
            hash("synthetic-session"),
            id,
            hash("csrf"),
            now,
            now + 86400000,
        ),
    )
    .unwrap();
    let config = Config::from_lookup(|k| match k {
        "NOEVIA_LEGACY_UPSTREAM" => Some("http://127.0.0.1:9".into()),
        "NOEVIA_RUST_AUTH" => Some("1".into()),
        "UI_DATA_DIR" => Some(d.path().display().to_string()),
        "PUBLIC_ORIGIN" => Some("http://127.0.0.1:8021".into()),
        _ => None,
    })
    .unwrap();
    let app = App::new(config);
    let conn = Conn {
        peer: "127.0.0.1:1".parse().unwrap(),
        local: Some("127.0.0.1:8021".parse().unwrap()),
    };
    let path = "/api/integrations/storage";
    let res = storage_read_routes::serve(
        &app,
        conn,
        Request::builder()
            .uri(format!("{path}?user_id={id}"))
            .body(Body::from(format!("{{\"user_id\":\"{id}\"}}")))
            .unwrap(),
        path.into(),
    )
    .await;
    assert_eq!(res.status(), 401); // no key file: gate wins, body/query identity cannot authenticate
    assert_eq!(res.headers()["www-authenticate"], "Bearer realm=\"cowork\"");
    std::fs::write(d.path().join("secrets.key"), vec![7; 32]).unwrap();
    let res = storage_read_routes::serve(
        &app,
        conn,
        Request::builder()
            .uri(path)
            .header("cookie", "cowork_session=synthetic-session")
            .body(Body::empty())
            .unwrap(),
        path.into(),
    )
    .await;
    assert_eq!(res.status(), 200);
    assert_eq!(res.headers()["content-type"], "application/json");
    assert_eq!(res.headers()["cache-control"], "no-store");
    assert_eq!(res.headers()["x-noevia-api"], "1");
    assert_eq!(res.headers()["x-content-type-options"], "nosniff");
    assert_eq!(res.headers()["x-frame-options"], "DENY");
    let body = res.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(body.as_ref(),b"{\"kind\":\"local\",\"baseUrl\":\"\",\"bucket\":\"\",\"region\":\"us-east-1\",\"username\":\"\",\"corpusRoot\":\"\"}");
    let foreign = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";
    db.execute("INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,created_at,updated_at) VALUES(?1,?1,?1,'Synthetic','admin','x',?1,1,1)",[foreign]).unwrap();
    db.execute("INSERT INTO storage_connections(user_id,kind,base_url,username,secret,corpus_root,updated_at) VALUES(?1,'webdav','https://synthetic.invalid','synthetic','synthetic-private','/owner',1)",[foreign]).unwrap();
    db.execute("INSERT INTO storage_connections(user_id,kind,base_url,username,secret,corpus_root,updated_at) VALUES(?1,'unknown-text','','','','',1)",[id]).unwrap();
    let res = storage_read_routes::serve(
        &app,
        conn,
        Request::builder()
            .uri(path)
            .header("cookie", "cowork_session=synthetic-session")
            .body(Body::empty())
            .unwrap(),
        path.into(),
    )
    .await;
    assert_eq!(res.status(), 200);
    let body = res.into_body().collect().await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["kind"], "unknown-text");
    assert_eq!(v["baseUrl"], "");
    assert_eq!(v["secretConfigured"], false);
    assert_eq!(v["secretNeedsReauth"], false);
    assert!(v.get("secret").is_none());
    assert_eq!(
        db.query_row(
            "SELECT secret FROM storage_connections WHERE user_id=?1",
            [foreign],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "synthetic-private"
    );
    assert_eq!(
        std::fs::read(d.path().join("secrets.key")).unwrap(),
        vec![7; 32]
    );
    app.storage_metadata.shutdown().await.unwrap();
}

#[cfg(feature = "rust-storage-reads")]
mod pool {
    //! noevia#1827: authentication before a slot, a small pool, and keys only for stored rows.
    use axum::{body::Body, http::Request};
    use noevia_server::{config::Config, serve::Conn, storage_read_routes, App};
    use server_store::rusqlite::Connection;
    use sha2::{Digest, Sha256};
    use std::sync::Arc;

    const PATH: &str = "/api/integrations/storage";
    const ID: &str = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";

    fn hash(v: &str) -> String {
        Sha256::digest(v.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    /// A synthetic data dir with one signed-in member and no storage connection or key file.
    fn app(d: &tempfile::TempDir) -> Arc<App> {
        let db = Connection::open(d.path().join("cowork.db")).unwrap();
        db.execute_batch(include_str!(
            "../../../crates/server-store/tests/fixtures/node-schema.sql"
        ))
        .unwrap();
        db.execute("INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,created_at,updated_at) VALUES(?1,?1,?1,'Synthetic','member','x',?1,1,1)",[ID]).unwrap();
        let now = noevia_server::identity::now_ms();
        db.execute(
            "INSERT INTO sessions VALUES(?1,?2,?3,?4,?4,?5,'ua','127.0.0.1')",
            (
                hash("synthetic-session"),
                ID,
                hash("csrf"),
                now,
                now + 86400000,
            ),
        )
        .unwrap();
        let dir = d.path().display().to_string();
        App::new(
            Config::from_lookup(|k| match k {
                "NOEVIA_LEGACY_UPSTREAM" => Some("http://127.0.0.1:9".into()),
                "NOEVIA_RUST_AUTH" => Some("1".into()),
                "UI_DATA_DIR" => Some(dir.clone()),
                "PUBLIC_ORIGIN" => Some("http://127.0.0.1:8021".into()),
                _ => None,
            })
            .unwrap(),
        )
    }

    async fn get(app: &Arc<App>, cookie: Option<&str>) -> u16 {
        let conn = Conn {
            peer: "127.0.0.1:1".parse().unwrap(),
            local: Some("127.0.0.1:8021".parse().unwrap()),
        };
        let mut req = Request::builder().uri(PATH);
        if let Some(cookie) = cookie {
            req = req.header("cookie", cookie);
        }
        storage_read_routes::serve(app, conn, req.body(Body::empty()).unwrap(), PATH.into())
            .await
            .status()
            .as_u16()
    }

    const SIGNED_IN: Option<&str> = Some("cowork_session=synthetic-session");

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn two_concurrent_signed_in_requests_both_succeed() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("secrets.key"), vec![7; 32]).unwrap();
        let app = app(&d);
        let (a, b) = tokio::join!(get(&app, SIGNED_IN), get(&app, SIGNED_IN));
        assert_eq!((a, b), (200, 200));
        app.storage_metadata.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn an_unauthenticated_flood_cannot_starve_a_signed_in_request() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("secrets.key"), vec![7; 32]).unwrap();
        let app = app(&d);
        let mut flood = Vec::new();
        for i in 0..64 {
            let app = Arc::clone(&app);
            flood.push(tokio::spawn(async move {
                let cookie = format!("cowork_session=forged-{i}");
                get(&app, if i % 2 == 0 { None } else { Some(&cookie) }).await
            }));
        }
        let signed_in = get(&app, SIGNED_IN).await;
        for rejected in flood {
            assert_eq!(rejected.await.unwrap(), 401);
        }
        assert_eq!(signed_in, 200);
        app.storage_metadata.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn missing_or_invalid_keys_refuse_only_owners_with_a_stored_connection() {
        let d = tempfile::tempdir().unwrap();
        let app = app(&d);
        // No key file: an owner without a connection still gets the no-connection answer.
        assert_eq!(get(&app, SIGNED_IN).await, 200);
        // An invalid key file: the same.
        std::fs::write(d.path().join("secrets.key"), vec![7; 31]).unwrap();
        assert_eq!(get(&app, SIGNED_IN).await, 200);
        // With a stored connection the credential cannot be opened: refused.
        let db = Connection::open(d.path().join("cowork.db")).unwrap();
        db.execute("INSERT INTO storage_connections(user_id,kind,base_url,username,secret,corpus_root,updated_at) VALUES(?1,'webdav','https://synthetic.invalid','synthetic','enc:v2:synthetic','/owner',1)",[ID]).unwrap();
        assert_eq!(get(&app, SIGNED_IN).await, 503);
        std::fs::remove_file(d.path().join("secrets.key")).unwrap();
        assert_eq!(get(&app, SIGNED_IN).await, 503);
        // Valid keys again: the row is projected.
        std::fs::write(d.path().join("secrets.key"), vec![7; 32]).unwrap();
        assert_eq!(get(&app, SIGNED_IN).await, 200);
        app.storage_metadata.shutdown().await.unwrap();
    }
}
