//! Actual serve-path tests with synthetic SQLite and bodies that must not be polled.
//! Production workspace initialization is Linux-only; these are mandatory Linux CI tests.
#![cfg(target_os = "linux")]
use axum::body::Body;
use axum::http::{Request, Response, StatusCode};
use bytes::Bytes;
use http_body::{Body as HttpBody, Frame};
use http_body_util::BodyExt;
use noevia_server::{account_routes, config::Config, identity, serve::Conn, App};
use server_store::rusqlite::Connection;
use std::{
    collections::HashMap,
    io,
    pin::Pin,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    task::{Context, Poll},
    time::Duration,
};
type Error = Box<dyn std::error::Error>;
const UID: &str = "00000000-0000-4000-8000-000000000001";
const PATH: &str = "/api/chat-framing/preferences";
const ORIGIN: &str = "https://framing.example.test";
const SESSION: &str = "synthetic-framing-session";
const CSRF: &str = "synthetic-framing-csrf";
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
            .join("chat-framing.json")
    }
}
fn conn() -> Result<Conn, Error> {
    Ok(Conn {
        peer: "127.0.0.1:41000".parse()?,
        local: Some("127.0.0.1:8010".parse()?),
    })
}
fn request(method: &str, auth: bool, csrf: bool, body: Body) -> Result<Request<Body>, Error> {
    let length = body.size_hint().exact().unwrap_or(1024);
    let mut request = Request::builder()
        .method(method)
        .uri(PATH)
        .header("content-length", length.to_string());
    if auth {
        request = request.header(
            "cookie",
            format!("cowork_session={SESSION}; cowork_csrf={CSRF}"),
        );
    }
    if csrf {
        request = request
            .header("origin", ORIGIN)
            .header("x-csrf-token", CSRF);
    }
    Ok(request.body(body)?)
}
async fn response_text(response: Response<Body>) -> Result<String, Error> {
    Ok(String::from_utf8(
        response.into_body().collect().await?.to_bytes().to_vec(),
    )?)
}
struct ProbeBody {
    polls: Arc<AtomicUsize>,
    error: bool,
}
impl HttpBody for ProbeBody {
    type Data = Bytes;
    type Error = io::Error;
    fn poll_frame(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, io::Error>>> {
        self.polls.fetch_add(1, Ordering::SeqCst);
        if self.error {
            Poll::Ready(Some(Err(io::Error::other("synthetic transport failure"))))
        } else {
            Poll::Pending
        }
    }
}
#[tokio::test]
async fn header_refusals_get_and_methods_never_poll_pending_or_error_bodies() -> Result<(), Error> {
    let h = Harness::new()?;
    for error in [false, true] {
        for (method, auth, csrf, status) in [
            ("PUT", false, false, 401),
            ("PUT", true, false, 403),
            ("DELETE", true, false, 403),
            ("GET", true, false, 200),
            ("DELETE", true, true, 405),
            ("HEAD", true, false, 405),
            ("OPTIONS", true, false, 405),
        ] {
            let polls = Arc::new(AtomicUsize::new(0));
            let body = Body::new(ProbeBody {
                polls: Arc::clone(&polls),
                error,
            });
            let response = tokio::time::timeout(
                Duration::from_secs(2),
                account_routes::serve(
                    &h.app,
                    conn()?,
                    request(method, auth, csrf, body)?,
                    PATH.into(),
                ),
            )
            .await?;
            assert_eq!(response.status().as_u16(), status);
            assert_eq!(
                polls.load(Ordering::SeqCst),
                0,
                "{method} polled an irrelevant/refused body"
            );
        }
    }
    assert!(!h.file().exists());
    Ok(())
}
#[tokio::test]
async fn authenticated_put_transport_failure_has_framing_error_and_no_write() -> Result<(), Error> {
    let h = Harness::new()?;
    let polls = Arc::new(AtomicUsize::new(0));
    let body = Body::new(ProbeBody {
        polls: Arc::clone(&polls),
        error: true,
    });
    let response = account_routes::serve(
        &h.app,
        conn()?,
        request("PUT", true, true, body)?,
        PATH.into(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        response_text(response).await?,
        r#"{"error":"Invalid framing preferences"}"#
    );
    assert_eq!(polls.load(Ordering::SeqCst), 1);
    assert!(!h.file().exists());
    Ok(())
}
struct ControlledBody {
    receiver: tokio::sync::mpsc::Receiver<Bytes>,
    polled: Arc<tokio::sync::Notify>,
}
impl HttpBody for ControlledBody {
    type Data = Bytes;
    type Error = io::Error;
    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, io::Error>>> {
        self.polled.notify_one();
        self.receiver
            .poll_recv(cx)
            .map(|value| value.map(|bytes| Ok(Frame::data(bytes))))
    }
}
#[tokio::test]
async fn credential_revoked_after_header_gate_before_body_finish_never_writes() -> Result<(), Error>
{
    let h = Harness::new()?;
    let (sender, receiver) = tokio::sync::mpsc::channel(1);
    let polled = Arc::new(tokio::sync::Notify::new());
    let body = Body::new(ControlledBody {
        receiver,
        polled: Arc::clone(&polled),
    });
    let app = Arc::clone(&h.app);
    let conn = conn()?;
    let mut req = request("PUT", true, true, body)?;
    req.headers_mut().insert(
        axum::http::header::CONTENT_LENGTH,
        axum::http::HeaderValue::from_static("19"),
    );
    let worker =
        tokio::spawn(async move { account_routes::serve(&app, conn, req, PATH.into()).await });
    tokio::time::timeout(Duration::from_secs(2), polled.notified()).await?;
    h.db.execute("DELETE FROM sessions", [])?;
    sender
        .send(Bytes::from_static(br#"{"autoAccept":true}"#))
        .await?;
    drop(sender);
    let response = tokio::time::timeout(Duration::from_secs(2), worker).await??;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(!h.file().exists());
    Ok(())
}
