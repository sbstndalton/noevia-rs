//! `GET|PUT /api/chat-vault-mirror/preferences` on the serve path (sbstndalton/noevia#2190 round 3),
//! with synthetic SQLite only. Production workspace initialization is Linux-only.
#![cfg(target_os = "linux")]
use axum::body::Body;
use axum::http::Request;
use http_body_util::BodyExt;
use noevia_server::{account_routes, config::Config, identity, serve::Conn, App};
use server_store::rusqlite::Connection;
use std::{collections::HashMap, io, sync::Arc};
type Error = Box<dyn std::error::Error>;
const UID: &str = "00000000-0000-4000-8000-000000000001";
const PATH: &str = "/api/chat-vault-mirror/preferences";
const ORIGIN: &str = "https://mirror.example.test";
const SESSION: &str = "synthetic-mirror-session";
const CSRF: &str = "synthetic-mirror-csrf";

struct Harness {
    dir: tempfile::TempDir,
    db: Connection,
    app: Arc<App>,
}
impl Harness {
    fn new() -> Result<Self, Error> {
        let dir = tempfile::tempdir()?;
        let db = Connection::open(server_store::db_path(dir.path()))?;
        db.execute_batch(include_str!(
            "../../../crates/server-store/tests/fixtures/node-schema.sql"
        ))?;
        let now = identity::now_ms();
        db.execute("INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,created_at,updated_at) VALUES(?1,'Synthetic','synthetic','Synthetic','admin','unused','wa',?2,?2)",(UID,now))?;
        db.execute(
            "INSERT INTO sessions VALUES(?1,?2,?3,?4,?4,?5,'synthetic','127.0.0.1')",
            (
                server_auth::js::digest(SESSION),
                UID,
                server_auth::js::digest(CSRF),
                now,
                now + 86_400_000,
            ),
        )?;
        db.execute("INSERT INTO settings VALUES('public_origin',?1)", [ORIGIN])?;
        let vars = HashMap::from([
            ("NOEVIA_LEGACY_UPSTREAM", "http://127.0.0.1:9".to_string()),
            ("UI_DATA_DIR", dir.path().display().to_string()),
            ("PUBLIC_ORIGIN", ORIGIN.to_string()),
            ("WEBAUTHN_RP_ID", "example.test".to_string()),
            (
                "NOEVIA_START_ID",
                "11111111111111111111111111111111".to_string(),
            ),
        ]);
        let config = Config::from_lookup(|key| vars.get(key).cloned()).map_err(io::Error::other)?;
        Ok(Self {
            dir,
            db,
            app: App::new(config),
        })
    }
    fn file(&self) -> std::path::PathBuf {
        self.dir
            .path()
            .join("users")
            .join(UID)
            .join("chat-vault-mirror.json")
    }
    fn nudges(&self) -> Option<String> {
        std::fs::read_to_string(self.dir.path().join("chat-vault-mirror-nudges.json")).ok()
    }
    fn audits(&self) -> Result<Vec<String>, Error> {
        let mut st = self.db.prepare(
            "SELECT detail FROM audit_events WHERE action='chat-vault-mirror.preferences' AND actor_user_id=target_user_id ORDER BY id",
        )?;
        let rows = st.query_map([], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }
    async fn call(&self, method: &str, body: &str) -> Result<(u16, String), Error> {
        let mut request = Request::builder()
            .method(method)
            .uri(PATH)
            .header("content-length", body.len().to_string())
            .header(
                "cookie",
                format!("cowork_session={SESSION}; cowork_csrf={CSRF}"),
            );
        if method != "GET" {
            request = request
                .header("origin", ORIGIN)
                .header("x-csrf-token", CSRF);
        }
        let response = account_routes::serve(
            &self.app,
            Conn {
                peer: "127.0.0.1:41000".parse()?,
                local: Some("127.0.0.1:8010".parse()?),
            },
            request.body(Body::from(body.to_string()))?,
            PATH.into(),
        )
        .await;
        let status = response.status().as_u16();
        let text = String::from_utf8(response.into_body().collect().await?.to_bytes().to_vec())?;
        Ok((status, text))
    }
    fn register(&self) -> Result<(), Error> {
        use server_account::feature_context::{
            source_fingerprint, BootFacts, BrowserFact, Cancellation, ContextSource, Control,
            Registration, Reply, Snapshot,
        };
        let controller = self
            .app
            .feature_context
            .as_ref()
            .ok_or("missing runtime")?
            .controller();
        let writer = self.app.writes.writer()?;
        let snapshot = writer.read(|r| {
            Ok(Snapshot {
                decision: source_fingerprint(r, ContextSource::Decision)?,
                machines: source_fingerprint(r, ContextSource::InferenceMachines)?,
                choice: None,
                system_one_unconfigured: true,
            })
        })?;
        let facts = BootFacts {
            env: [None; 23],
            restart: [false, false],
            browser: BrowserFact::Available,
            code: false,
            trust_proxy: false,
        };
        let cancel = Cancellation::default();
        let control = Control {
            request: "00000000-0000-4000-8000-000000000003".parse()?,
            deadline: std::time::Instant::now() + std::time::Duration::from_secs(30),
            cancelled: cancel.signal(),
        };
        let result = controller.register(
            &writer,
            &control,
            Registration {
                boot: "00000000-0000-4000-8000-000000000001".parse()?,
                session: "00000000-0000-4000-8000-000000000002".parse()?,
                facts: facts.clone(),
                snapshot,
            },
            |tx| {
                server_account::feature_context::startup::initialize(tx, &facts, identity::now_ms())
            },
        )?;
        assert!(matches!(result, Reply::Ready(_)));
        Ok(())
    }
}

#[tokio::test]
async fn validation_and_methods_write_nothing() -> Result<(), Error> {
    let h = Harness::new()?;
    for (method, body, status, text) in [
        ("PUT", "{", 400, r#"{"error":"invalid JSON"}"#),
        (
            "PUT",
            "",
            400,
            r#"{"error":"enabled must be true or false"}"#,
        ),
        (
            "PUT",
            r#"{"enabled":"true"}"#,
            400,
            r#"{"error":"enabled must be true or false"}"#,
        ),
        (
            "PUT",
            r#"{"enabled":1}"#,
            400,
            r#"{"error":"enabled must be true or false"}"#,
        ),
        ("POST", "{}", 405, r#"{"error":"method not allowed"}"#),
        ("DELETE", "", 405, r#"{"error":"method not allowed"}"#),
    ] {
        assert_eq!(
            h.call(method, body).await?,
            (status, text.to_string()),
            "{method} {body}"
        );
    }
    assert!(!h.file().exists());
    assert!(h.audits()?.is_empty());
    assert!(h.nudges().is_none());
    Ok(())
}

/// Without a registered feature runtime, Node's `features.enabled` throws its 503 — after the PUT
/// already saved and audited, so the save stands.
#[tokio::test]
async fn unavailable_feature_runtime_is_node_503_after_the_save() -> Result<(), Error> {
    let h = Harness::new()?;
    let unavailable = (
        503,
        r#"{"error":"Feature runtime context is unavailable."}"#.to_string(),
    );
    assert_eq!(h.call("GET", "").await?, unavailable);
    assert_eq!(h.call("PUT", r#"{"enabled":true}"#).await?, unavailable);
    assert_eq!(std::fs::read_to_string(h.file())?, r#"{"enabled":true}"#);
    assert_eq!(h.audits()?, vec![r#"{"enabled":true}"#.to_string()]);
    assert!(h.nudges().is_none());
    Ok(())
}

#[tokio::test]
async fn availability_follows_chat_framing_and_diary_and_nudges_node() -> Result<(), Error> {
    let h = Harness::new()?;
    h.register()?;
    let off = r#"{"enabled":false,"available":false}"#;
    assert_eq!(h.call("GET", "").await?, (200, off.to_string()));
    // On while unavailable: saved and audited, no nudge.
    assert_eq!(
        h.call("PUT", r#"{"enabled":true}"#).await?,
        (200, r#"{"enabled":true,"available":false}"#.to_string())
    );
    assert!(h.nudges().is_none());
    h.db.execute(
        "INSERT INTO settings VALUES('feature:chatFraming','true')",
        [],
    )?;
    assert_eq!(
        h.call("GET", "").await?,
        (200, r#"{"enabled":true,"available":false}"#.to_string()),
        "chatFraming alone is not enough"
    );
    h.db.execute(
        "INSERT INTO user_features(user_id,diary_enabled,updated_at) VALUES(?1,1,0)",
        [UID],
    )?;
    assert_eq!(
        h.call("GET", "").await?,
        (200, r#"{"enabled":true,"available":true}"#.to_string())
    );
    assert_eq!(
        h.call("PUT", r#"{"enabled":true,"extra":1}"#).await?,
        (200, r#"{"enabled":true,"available":true}"#.to_string())
    );
    let nudges = h.nudges().ok_or("no nudge")?;
    assert!(
        nudges.starts_with(&format!(r#"{{"version":1,"nudges":{{"{UID}":"#)),
        "{nudges}"
    );
    std::fs::remove_file(h.dir.path().join("chat-vault-mirror-nudges.json"))?;
    // Turning it off never nudges.
    assert_eq!(
        h.call("PUT", r#"{"enabled":false}"#).await?,
        (200, r#"{"enabled":false,"available":true}"#.to_string())
    );
    assert!(h.nudges().is_none());
    assert_eq!(std::fs::read_to_string(h.file())?, r#"{"enabled":false}"#);
    assert_eq!(h.audits()?.len(), 3);
    Ok(())
}
