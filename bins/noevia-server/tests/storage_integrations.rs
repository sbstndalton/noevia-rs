//! POST /api/integrations/storage/{folder,test,nextcloud/start,poll,cancel} through the real front
//! (sbstndalton/noevia#2190). The legacy upstream is dead, so a request that fell through to Node
//! would fail. Nextcloud is a synthetic https server on 127.0.0.1 (a throwaway CA trusted through
//! NOEVIA_QA_STORAGE_CA, tests/fixtures/synthetic-tls); WebDAV and S3 are synthetic http servers.
//! No real account or credential is used. MEMBER_OUTBOUND_ORIGINS is never set in this binary, so a
//! member is refused every target.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
use noevia_server::{
    config::Config,
    serve::{serve, Limits},
    App,
};
use server_store::rusqlite::Connection;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot,
};

const A: &str = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
const B: &str = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";
const ADMIN: &str = "cowork_session=admin-token; cowork_csrf=csrf";
const MEMBER: &str = "cowork_session=member-token; cowork_csrf=csrf";
const KEY: [u8; 32] = [7; 32];
const START: &str = "/api/integrations/storage/nextcloud/start";
const POLL: &str = "/api/integrations/storage/nextcloud/poll";
const CANCEL: &str = "/api/integrations/storage/nextcloud/cancel";
const TEST: &str = "/api/integrations/storage/test";
const FOLDER: &str = "/api/integrations/storage/folder";
const PRIVATE: &str = "An http(s) server URL is required. This server is not approved for member connections. Ask an administrator to add its origin to MEMBER_OUTBOUND_ORIGINS.";

/// Golden vector shared with core tests (server/rust-storage-integrations.test.cjs): Node's
/// `secrets.encrypt('synthetic-app-password', A)` with key [7;32] and nonce 0x01..0x0c (also
/// computed with Node's own `crypto.createCipheriv('aes-256-gcm')`, independent of the wasm port).
const GOLDEN: &str = "enc:v2:AQIDBAUGBwgJCgsMSgG3ElISmXU7G_CAvinHk8gFhuoCj5ZFhhNjbuWK0yGnliZfwC8";

fn hex(s: &str) -> String {
    Sha256::digest(s.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn j(s: &str) -> serde_json::Value {
    serde_json::from_str(s).unwrap_or_else(|_| panic!("not JSON: {s}"))
}

async fn call(
    addr: std::net::SocketAddr,
    path: &str,
    cookie: &str,
    body: &str,
) -> (u16, serde_json::Value) {
    let csrf = format!("Origin: http://{addr}\r\nX-CSRF-Token: csrf\r\n");
    raw(addr, "POST", path, cookie, &csrf, body.as_bytes()).await
}

async fn raw(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    cookie: &str,
    extra: &str,
    body: &[u8],
) -> (u16, serde_json::Value) {
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nCookie: {cookie}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{extra}\r\n", body.len()).as_bytes()).await.unwrap();
    let _ = s.write_all(body).await;
    let mut bytes = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(40), s.read_to_end(&mut bytes))
        .await
        .unwrap();
    let text = String::from_utf8(bytes).unwrap();
    let (h, b) = text.split_once("\r\n\r\n").unwrap();
    let lower = h.to_ascii_lowercase();
    assert!(lower.contains("content-type: application/json"), "{h}");
    assert!(lower.contains("cache-control: no-store"), "{h}");
    let status = h.split_whitespace().nth(1).unwrap().parse().unwrap();
    (status, j(b))
}

// ---------------------------------------------------------------------------------------------
// Synthetic upstreams.

#[derive(Clone, Debug)]
struct Seen {
    method: String,
    path: String,
    headers: String,
    body: String,
}

type Handler = Arc<dyn Fn(&Seen) -> (Duration, String) + Send + Sync>;

#[derive(Clone)]
struct Upstream {
    addr: std::net::SocketAddr,
    seen: Arc<Mutex<Vec<Seen>>>,
    handler: Arc<Mutex<Handler>>,
}

impl Upstream {
    fn set(&self, f: impl Fn(&Seen) -> (Duration, String) + Send + Sync + 'static) {
        *self.handler.lock().unwrap() = Arc::new(f);
    }
    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
    fn clear(&self) {
        self.seen.lock().unwrap().clear();
    }
    fn base(&self, scheme: &str) -> String {
        format!("{scheme}://{}", self.addr)
    }
}

fn reply(status: &str, headers: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n{body}",
        body.len()
    )
}

fn now(text: String) -> (Duration, String) {
    (Duration::ZERO, text)
}

async fn exchange<S: AsyncRead + AsyncWrite + Unpin>(
    mut s: S,
    seen: Arc<Mutex<Vec<Seen>>>,
    handler: Arc<Mutex<Handler>>,
) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        let Ok(n) = s.read(&mut chunk).await else {
            return;
        };
        if n == 0 {
            return;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let len = head
        .lines()
        .find_map(|l| {
            l.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .map(|v| v.trim().parse::<usize>().unwrap_or(0))
        })
        .unwrap_or(0);
    while buf.len() < head_end + len {
        let Ok(n) = s.read(&mut chunk).await else {
            return;
        };
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let mut first = head.lines().next().unwrap_or("").split_whitespace();
    let request = Seen {
        method: first.next().unwrap_or("").into(),
        path: first.next().unwrap_or("").into(),
        headers: head.to_ascii_lowercase(),
        body: String::from_utf8_lossy(&buf[head_end..]).to_string(),
    };
    seen.lock().unwrap().push(request.clone());
    let f = handler.lock().unwrap().clone();
    let (delay, text) = f(&request);
    tokio::time::sleep(delay).await;
    let _ = s.write_all(text.as_bytes()).await;
    let _ = s.shutdown().await;
}

async fn upstream(tls: bool) -> Upstream {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let up = Upstream {
        addr: l.local_addr().unwrap(),
        seen: Arc::new(Mutex::new(Vec::new())),
        handler: Arc::new(Mutex::new(Arc::new(|_: &Seen| {
            now(reply("500 Oops", "", ""))
        }))),
    };
    let acceptor = tls.then(|| {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/synthetic-tls");
        let cert = std::fs::read(format!("{dir}/leaf.der")).unwrap();
        let key = std::fs::read(format!("{dir}/leaf.key.der")).unwrap();
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![rustls::pki_types::CertificateDer::from(cert)],
            rustls::pki_types::PrivateKeyDer::Pkcs8(key.into()),
        )
        .unwrap();
        tokio_rustls::TlsAcceptor::from(Arc::new(config))
    });
    let (seen, handler) = (Arc::clone(&up.seen), Arc::clone(&up.handler));
    tokio::spawn(async move {
        loop {
            let Ok((s, _)) = l.accept().await else {
                return;
            };
            let (seen, handler, acceptor) =
                (Arc::clone(&seen), Arc::clone(&handler), acceptor.clone());
            tokio::spawn(async move {
                match acceptor {
                    Some(a) => {
                        if let Ok(s) = a.accept(s).await {
                            exchange(s, seen, handler).await;
                        }
                    }
                    None => exchange(s, seen, handler).await,
                }
            });
        }
    });
    up
}

// ---------------------------------------------------------------------------------------------
// The front and its data directory.

struct Front {
    addr: std::net::SocketAddr,
    stop: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<()>,
}

async fn front(dir: &std::path::Path) -> Front {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let env = HashMap::from([
        ("NOEVIA_LEGACY_UPSTREAM", "http://127.0.0.1:9".to_string()),
        ("UI_DATA_DIR", dir.display().to_string()),
        ("PUBLIC_ORIGIN", format!("http://{addr}")),
        (
            "NOEVIA_QA_STORAGE_CA",
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/synthetic-tls/ca.der"
            )
            .into(),
        ),
    ]);
    let app = App::new(Config::from_lookup(|k| env.get(k).cloned()).unwrap());
    let (tx, rx) = oneshot::channel();
    let task = tokio::spawn(serve(listener, app, Limits::default(), async {
        let _ = rx.await;
    }));
    Front {
        addr,
        stop: Some(tx),
        task,
    }
}

impl Front {
    async fn stop(mut self) {
        let _ = self.stop.take().unwrap().send(());
        let _ = self.task.await;
    }
}

fn seed(dir: &std::path::Path) -> Connection {
    std::fs::write(dir.join("secrets.key"), KEY).unwrap();
    let db = Connection::open(dir.join("cowork.db")).unwrap();
    db.execute_batch(include_str!(
        "../../../crates/server-store/tests/fixtures/node-schema.sql"
    ))
    .unwrap();
    let now = noevia_server::identity::now_ms();
    for (uid, role, t) in [(A, "admin", "admin-token"), (B, "member", "member-token")] {
        db.execute("INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,created_at,updated_at) VALUES(?1,?1,?1,'Synthetic',?2,'x',?1,1,1)", (uid, role)).unwrap();
        db.execute(
            "INSERT INTO sessions VALUES(?1,?2,?3,?4,?4,?5,'ua','127.0.0.1')",
            (hex(t), uid, hex("csrf"), now, now + 86400000),
        )
        .unwrap();
    }
    db
}

fn seal(plain: &str, user: &str) -> String {
    let mut nonce = [0u8; 12];
    nonce[0] = plain.len() as u8;
    nonce[1] = user.len() as u8;
    secret_envelope::seal(&KEY, &nonce, plain.as_bytes(), Some(user.as_bytes())).unwrap()
}

fn open(value: &str, user: &str) -> String {
    let units: Vec<u16> = value.encode_utf16().collect();
    let (_, plain) = secret_envelope::open(&KEY, None, &units, Some(user.as_bytes())).unwrap();
    String::from_utf8(plain.to_vec()).unwrap()
}

fn save_row(db: &Connection, user: &str, kind: &str, base: &str, bucket: &str, secret: &str) {
    db.execute(
        "INSERT OR REPLACE INTO storage_connections(user_id,kind,base_url,bucket,region,username,secret,corpus_root,updated_at) VALUES(?1,?2,?3,?4,'us-east-1','alice',?5,'Notes',1)",
        (user, kind, base, bucket, seal(secret, user)),
    )
    .unwrap();
}

fn row(db: &Connection, user: &str) -> Option<(String, String, String, String, String, String)> {
    db.query_row(
        "SELECT kind,base_url,username,secret,corpus_root,region FROM storage_connections WHERE user_id=?1",
        [user],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
    )
    .ok()
}

fn audits(db: &Connection) -> Vec<(String, String, String, String)> {
    let mut st = db.prepare("SELECT actor_user_id,target_user_id,action,detail FROM audit_events WHERE action='storage.update' ORDER BY id").unwrap();
    st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

fn flows(dir: &std::path::Path) -> serde_json::Value {
    j(&std::fs::read_to_string(dir.join("nextcloud-login-flows.json")).unwrap())
}

/// A Nextcloud that offers a login flow at `nc` and answers polls with `poll`.
fn nextcloud(nc: &Upstream, poll_endpoint: String, poll: Arc<Mutex<(Duration, String)>>) {
    let base = nc.base("https");
    nc.set(move |r: &Seen| match (r.method.as_str(), r.path.as_str()) {
        ("POST", "/index.php/login/v2") => now(reply(
            "200 OK",
            "Content-Type: application/json\r\n",
            &format!(
                r#"{{"poll":{{"token":"synthetic-poll-token","endpoint":"{poll_endpoint}"}},"login":"{base}/login/v2/flow/synthetic"}}"#
            ),
        )),
        ("POST", "/login/v2/poll") => poll.lock().unwrap().clone(),
        ("GET", "/ocs/v2.php/cloud/user?format=json") => now(reply(
            "200 OK",
            "",
            r#"{"ocs":{"data":{"id":"alice"}}}"#,
        )),
        ("PROPFIND", "/remote.php/dav/files/alice/") => now(reply("207 Multi-Status", "", "")),
        ("DELETE", "/ocs/v2.php/core/apppassword") => now(reply("200 OK", "", "")),
        _ => now(reply("404 Not Found", "", "")),
    });
}

fn credentials(base: &str) -> String {
    reply(
        "200 OK",
        "",
        &format!(
            r#"{{"server":"{base}","loginName":"alice@example.test","appPassword":"synthetic-app-password"}}"#
        ),
    )
}

#[test]
fn golden_vector_matches_node_sealing() {
    let nonce: [u8; 12] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12];
    let keys = server_auth::secrets::SecretKeys::from_existing_bytes(
        zeroize::Zeroizing::new(KEY.to_vec()),
        None,
    )
    .unwrap();
    assert_eq!(
        keys.seal("synthetic-app-password", Some(A), &nonce)
            .unwrap(),
        GOLDEN
    );
    assert_eq!(keys.seal("", Some(A), &nonce).unwrap(), "");
    assert!(keys.seal("x", None, &nonce).unwrap().starts_with("enc:v1:"));
    let (plain, _) = keys.open(GOLDEN, Some(A)).unwrap();
    assert_eq!(plain.as_str(), "synthetic-app-password");
    assert!(keys.open(GOLDEN, Some(B)).is_err());
}

#[tokio::test]
async fn nextcloud_sign_in_lifecycle_limits_and_restart() {
    let d = tempfile::tempdir().unwrap();
    let db = seed(d.path());
    let nc = upstream(true).await;
    let base = nc.base("https");
    let poll = Arc::new(Mutex::new(now(reply("404 Not Found", "", ""))));
    nextcloud(&nc, format!("{base}/login/v2/poll"), Arc::clone(&poll));
    let f = front(d.path()).await;
    let addr = f.addr;

    // The gate, then the body.
    assert_eq!(raw(addr, "POST", START, "", "", b"{}").await.0, 401);
    assert_eq!(raw(addr, "POST", START, ADMIN, "", b"{}").await.0, 403);
    assert_eq!(
        call(addr, START, ADMIN, "[]").await,
        (400, j(r#"{"error":"request body must be a JSON object"}"#))
    );
    assert_eq!(
        call(addr, START, ADMIN, "{").await,
        (400, j(r#"{"error":"invalid JSON"}"#))
    );
    let big = format!(r#"{{"x":"{}"}}"#, "a".repeat(1024 * 1024));
    assert_eq!(
        call(addr, START, ADMIN, &big).await,
        (413, j(r#"{"error":"Request exceeds size limit"}"#))
    );
    // https only, then the member-origin rule, before anything is contacted.
    let (s, b) = call(addr, START, ADMIN, r#"{"baseUrl":"http://127.0.0.1:1"}"#).await;
    assert_eq!(
        (s, b["code"].as_str()),
        (400, Some("nextcloudHttpsRequired"))
    );
    assert_eq!(
        call(addr, START, MEMBER, &format!(r#"{{"baseUrl":"{base}"}}"#)).await,
        (400, serde_json::json!({ "error": PRIVATE }))
    );
    assert!(nc.seen().is_empty());

    // Start: the Files URL is reduced to the server root; the flow is sealed for its owner.
    let (s, b) = call(
        addr,
        START,
        ADMIN,
        &format!(r#"{{"baseUrl":" {base}/index.php/apps/files/ "}}"#),
    )
    .await;
    assert_eq!(s, 200, "{b}");
    let flow_id = b["flowId"].as_str().unwrap().to_string();
    assert_eq!(flow_id.len(), 36);
    assert_eq!(b["loginUrl"], format!("{base}/login/v2/flow/synthetic"));
    assert!(b["expiresAt"].as_f64().unwrap() > noevia_server::identity::now_ms() as f64);
    let seen = nc.seen();
    assert_eq!(seen[0].method, "POST");
    assert_eq!(seen[0].path, "/index.php/login/v2");
    assert!(
        seen[0].headers.contains("user-agent: noevia"),
        "{}",
        seen[0].headers
    );
    let file = std::fs::read_to_string(d.path().join("nextcloud-login-flows.json")).unwrap();
    assert!(!file.contains("synthetic-poll-token") && !file.contains("/login/v2/poll"));
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(d.path().join("nextcloud-login-flows.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    let stored = flows(d.path());
    assert_eq!(stored["version"], 1);
    let record = &stored["flows"][&flow_id];
    assert_eq!(record["userId"], A);
    let sealed = record["sealed"].as_str().unwrap();
    assert!(sealed.starts_with("enc:v2:"));
    let fields = j(&open(sealed, A));
    assert_eq!(fields["endpoint"], format!("{base}/login/v2/poll"));
    assert_eq!(fields["token"], "synthetic-poll-token");
    assert_eq!(fields["origin"], base);
    assert_eq!(fields["nonce"].as_str().unwrap().len(), 32);

    // Pending, then another account's poll and cancel see nothing.
    let body = format!(r#"{{"flowId":"{flow_id}"}}"#);
    assert_eq!(
        call(addr, POLL, ADMIN, &body).await,
        (202, j(r#"{"pending":true}"#))
    );
    let poll_req = nc.seen().last().unwrap().clone();
    assert_eq!(poll_req.body, "token=synthetic-poll-token");
    assert!(poll_req
        .headers
        .contains("content-type: application/x-www-form-urlencoded"));
    let (s, b) = call(addr, POLL, MEMBER, &body).await;
    assert_eq!((s, b["code"].as_str()), (400, Some("nextcloudFlowExpired")));
    assert_eq!(
        call(addr, CANCEL, MEMBER, &body).await,
        (200, j(r#"{"cancelled":false}"#))
    );

    // Restart: the pending sign-in survives in the file.
    f.stop().await;
    let f = front(d.path()).await;
    let addr = f.addr;
    assert_eq!(call(addr, POLL, ADMIN, &body).await.0, 202);

    // Approved: the who-am-I id makes the WebDAV home, the login is checked, the row is saved.
    *poll.lock().unwrap() = now(credentials(&base));
    nc.clear();
    let (s, b) = call(addr, POLL, ADMIN, &body).await;
    assert_eq!(s, 200, "{b}");
    assert_eq!(
        b,
        serde_json::json!({
            "kind": "nextcloud",
            "baseUrl": format!("{base}/remote.php/dav/files/alice"),
            "bucket": "",
            "region": "us-east-1",
            "username": "alice@example.test",
            "corpusRoot": "Cowork/Diary",
            "secretConfigured": true,
            "secretNeedsReauth": false,
        })
    );
    let paths: Vec<(String, String)> = nc
        .seen()
        .iter()
        .map(|r| (r.method.clone(), r.path.clone()))
        .collect();
    assert_eq!(
        paths,
        [
            ("POST".to_string(), "/login/v2/poll".to_string()),
            ("GET".into(), "/ocs/v2.php/cloud/user?format=json".into()),
            ("PROPFIND".into(), "/remote.php/dav/files/alice/".into()),
        ]
    );
    let (kind, url, user, secret, root, region) = row(&db, A).unwrap();
    assert_eq!(
        (
            kind.as_str(),
            url.as_str(),
            user.as_str(),
            root.as_str(),
            region.as_str()
        ),
        (
            "nextcloud",
            format!("{base}/remote.php/dav/files/alice").as_str(),
            "alice@example.test",
            "Cowork/Diary",
            "us-east-1"
        )
    );
    assert!(secret.starts_with("enc:v2:"));
    assert_eq!(open(&secret, A), "synthetic-app-password");
    assert_eq!(
        audits(&db),
        [(
            A.into(),
            A.into(),
            "storage.update".into(),
            r#"{"kind":"nextcloud"}"#.into()
        )]
    );
    assert_eq!(flows(d.path())["flows"], serde_json::json!({}));
    // The flow is spent.
    let (s, b) = call(addr, POLL, ADMIN, &body).await;
    assert_eq!((s, b["code"].as_str()), (400, Some("nextcloudFlowExpired")));

    // A sign-in again keeps the folder already used; a refused WebDAV login revokes and keeps
    // the row.
    let (_, b) = call(addr, START, ADMIN, &format!(r#"{{"baseUrl":"{base}"}}"#)).await;
    let id2 = b["flowId"].as_str().unwrap().to_string();
    let base2 = base.clone();
    let poll2 = Arc::clone(&poll);
    nc.set(move |r: &Seen| match (r.method.as_str(), r.path.as_str()) {
        ("POST", "/login/v2/poll") => poll2.lock().unwrap().clone(),
        ("GET", _) => now(reply("200 OK", "", r#"{"ocs":{"data":{"id":"alice"}}}"#)),
        ("PROPFIND", _) => now(reply("401 Unauthorized", "", "")),
        ("DELETE", _) => now(reply("200 OK", "", "")),
        _ => now(reply("404 Not Found", "", &base2)),
    });
    nc.clear();
    let (s, b) = call(addr, POLL, ADMIN, &format!(r#"{{"flowId":"{id2}"}}"#)).await;
    assert_eq!(
        (s, b),
        (
            502,
            j(
                r#"{"error":"Nextcloud refused the new app password for WebDAV. Sign in again.","code":"storageLoginRejected","status":401}"#
            )
        )
    );
    let revoke = nc
        .seen()
        .into_iter()
        .find(|r| r.method == "DELETE")
        .unwrap();
    assert_eq!(revoke.path, "/ocs/v2.php/core/apppassword");
    assert!(revoke.headers.contains("ocs-apirequest: true"));
    assert_eq!(audits(&db).len(), 1, "a refused login saves nothing");

    // Cancel by the owner; a poll already in flight then revokes instead of saving (#1433).
    nextcloud(&nc, format!("{base}/login/v2/poll"), Arc::clone(&poll));
    let (_, b) = call(addr, START, ADMIN, &format!(r#"{{"baseUrl":"{base}"}}"#)).await;
    let id3 = b["flowId"].as_str().unwrap().to_string();
    *poll.lock().unwrap() = (Duration::from_millis(1500), credentials(&base));
    nc.clear();
    let id3b = id3.clone();
    let in_flight =
        tokio::spawn(
            async move { call(addr, POLL, ADMIN, &format!(r#"{{"flowId":"{id3}"}}"#)).await },
        );
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        call(addr, CANCEL, ADMIN, &format!(r#"{{"flowId":"{id3b}"}}"#)).await,
        (200, j(r#"{"cancelled":true}"#))
    );
    let (s, b) = in_flight.await.unwrap();
    assert_eq!(
        (s, b["code"].as_str()),
        (409, Some("nextcloudFlowCancelled"))
    );
    assert!(nc.seen().iter().any(|r| r.method == "DELETE"));
    assert_eq!(audits(&db).len(), 1);

    // Nextcloud's own failures, worded as Node does.
    nc.set(|_: &Seen| now(reply("429 Too Many Requests", "Retry-After: 30\r\n", "")));
    let (s, b) = call(addr, START, ADMIN, &format!(r#"{{"baseUrl":"{base}"}}"#)).await;
    assert_eq!(
        (s, b["code"].as_str(), b["retryAfter"].as_f64()),
        (429, Some("nextcloudThrottled"), Some(30.0))
    );
    nc.set(|_: &Seen| {
        now(reply(
            "302 Found",
            "Location: https://elsewhere.invalid/\r\n",
            "",
        ))
    });
    let (s, b) = call(addr, START, ADMIN, &format!(r#"{{"baseUrl":"{base}"}}"#)).await;
    assert_eq!((s, b["reason"].as_str()), (502, Some("redirect")));
    nc.set(|_: &Seen| now(reply("200 OK", "", "<html>not nextcloud</html>")));
    let (s, b) = call(addr, START, ADMIN, &format!(r#"{{"baseUrl":"{base}"}}"#)).await;
    assert_eq!((s, b["code"].as_str()), (502, Some("nextcloudNotFound")));
    // A poll endpoint on another origin (localhost is not 127.0.0.1) is refused (#1432).
    let other = format!("https://localhost:{}/login/v2/poll", nc.addr.port());
    nextcloud(&nc, other, Arc::clone(&poll));
    let (s, b) = call(addr, START, ADMIN, &format!(r#"{{"baseUrl":"{base}"}}"#)).await;
    assert_eq!(
        (s, b["code"].as_str()),
        (400, Some("nextcloudOriginMismatch"))
    );
    f.stop().await;

    // The pending limit counts live flows only: 100 other live flows refuse a new one; expired
    // ones are dropped on load.
    let mut seeded = serde_json::Map::new();
    let later = noevia_server::identity::now_ms() + 300_000;
    for i in 0..100 {
        let fields = format!(
            r#"{{"endpoint":"{base}/p","token":"t{i}","origin":"{base}","nonce":"{i:032}"}}"#
        );
        let user = format!("synthetic-user-{i}");
        seeded.insert(
            format!("flow-{i}"),
            serde_json::json!({"userId": user, "expires": later, "sealed": seal(&fields, &user)}),
        );
    }
    let fields = format!(
        r#"{{"endpoint":"{base}/login/v2/poll","token":"t","origin":"{base}","nonce":"{:032}"}}"#,
        0
    );
    seeded.insert(
        "old".into(),
        serde_json::json!({"userId": A, "expires": 1, "sealed": seal(&fields, A)}),
    );
    std::fs::write(
        d.path().join("nextcloud-login-flows.json"),
        serde_json::json!({"version":1,"flows":seeded}).to_string(),
    )
    .unwrap();
    nextcloud(&nc, format!("{base}/login/v2/poll"), Arc::clone(&poll));
    let f = front(d.path()).await;
    let addr = f.addr;
    let (s, b) = call(addr, POLL, ADMIN, r#"{"flowId":"old"}"#).await;
    assert_eq!((s, b["code"].as_str()), (400, Some("nextcloudFlowExpired")));
    assert_eq!(
        call(addr, START, ADMIN, &format!(r#"{{"baseUrl":"{base}"}}"#)).await,
        (429, j(r#"{"error":"Too many pending connections"}"#))
    );
    f.stop().await;
}

#[tokio::test]
async fn storage_test_folder_and_login_proofs() {
    let d = tempfile::tempdir().unwrap();
    let db = seed(d.path());
    let dav = upstream(false).await;
    let base = dav.base("http");
    let f = front(d.path()).await;
    let addr = f.addr;

    assert_eq!(
        call(addr, TEST, ADMIN, r#"{"kind":"local"}"#).await,
        (200, j(r#"{"ok":true}"#))
    );
    // A connection being tried (not saved): PROPFIND Depth 0 on the corpus root.
    dav.set(|_: &Seen| now(reply("207 Multi-Status", "", "")));
    let try_body = format!(
        r#"{{"kind":"webdav","baseUrl":"{base}/dav/","username":"alice","secret":"synthetic-secret","corpusRoot":"My Notes/x"}}"#
    );
    assert_eq!(
        call(addr, TEST, ADMIN, &try_body).await,
        (200, j(r#"{"ok":true}"#))
    );
    let r = dav.seen().last().unwrap().clone();
    assert_eq!(
        (r.method.as_str(), r.path.as_str()),
        ("PROPFIND", "/dav/My%20Notes/x")
    );
    assert!(r.headers.contains("depth: 0"));
    assert!(
        r.headers
            .contains("authorization: basic ywxpy2u6c3ludghldgljlxnly3jlda=="),
        "{}",
        r.headers
    );
    // Nothing is saved, and nothing proves a login that is not the saved one.
    assert!(row(&db, A).is_none());
    assert!(!d.path().join("storage-login-proofs.json").exists());
    dav.set(|_: &Seen| now(reply("401 Unauthorized", "", "")));
    assert_eq!(
        call(addr, TEST, ADMIN, &try_body).await,
        (502, j(r#"{"error":"WebDAV returned 401"}"#))
    );
    dav.set(|_: &Seen| now(reply("301 Moved", "Location: /x\r\n", "")));
    assert_eq!(
        call(addr, TEST, ADMIN, &try_body).await,
        (502, j(r#"{"error":"fetch failed"}"#))
    );
    // A member may not reach an origin that is not approved.
    assert_eq!(
        call(addr, TEST, MEMBER, &try_body).await,
        (400, serde_json::json!({ "error": PRIVATE }))
    );
    assert_eq!(
        call(addr, FOLDER, MEMBER, r#"{"path":"x"}"#).await,
        (400, j(r#"{"error":"no browsable storage connected"}"#))
    );

    // The saved secret is only reused for the same origin and kind.
    save_row(&db, A, "webdav", &format!("{base}/dav"), "", "saved-secret");
    let reuse = |url: &str, kind: &str| {
        format!(r#"{{"kind":"{kind}","baseUrl":"{url}","username":"alice","useSavedSecret":true}}"#)
    };
    assert_eq!(
        call(
            addr,
            TEST,
            ADMIN,
            &reuse("http://other.invalid/dav", "webdav")
        )
        .await,
        (
            400,
            j(r#"{"error":"Enter a secret to test a different server or storage type."}"#)
        )
    );
    assert_eq!(
        call(
            addr,
            TEST,
            ADMIN,
            &reuse(&format!("{base}/dav"), "nextcloud")
        )
        .await
        .0,
        400
    );
    dav.set(|_: &Seen| now(reply("207 Multi-Status", "", "")));
    dav.clear();
    let (s, _) = call(addr, TEST, ADMIN, &reuse(&format!("{base}/dav"), "webdav")).await;
    assert_eq!(s, 200);
    assert!(
        dav.seen()[0]
            .headers
            .contains("authorization: basic ywxpy2u6c2f2zwqtc2vjcmv0"),
        "{}",
        dav.seen()[0].headers
    );
    // The saved credentials worked: the proof Node's storage latch clears its hold by.
    assert!(d.path().join("storage-login-proofs.json").exists());
    // A login one character away from the saved one proves nothing new.
    std::fs::remove_file(d.path().join("storage-login-proofs.json")).unwrap();
    let near = format!(
        r#"{{"kind":"webdav","baseUrl":"{base}/dav/","username":"alice","secret":"saved-secret"}}"#
    );
    assert_eq!(call(addr, TEST, ADMIN, &near).await.0, 200);
    assert!(!d.path().join("storage-login-proofs.json").exists());

    // useSaved proves the saved login.
    let (s, _) = call(addr, TEST, ADMIN, r#"{"useSaved":true,"kind":"webdav"}"#).await;
    assert_eq!(s, 200);
    let proofs = j(&std::fs::read_to_string(d.path().join("storage-login-proofs.json")).unwrap());
    assert_eq!(proofs["version"], 1);
    let first = proofs["proofs"][A].as_f64().unwrap();
    assert!(first > 0.0);

    // Folder: MKCOL only, the path made safe and encoded.
    dav.set(|r: &Seen| {
        now(match r.path.as_str() {
            "/dav/a/b%20c/" => reply("201 Created", "", ""),
            "/dav/there/" => reply("405 Method Not Allowed", "", ""),
            "/dav/conflict/x/" => reply("409 Conflict", "", ""),
            _ => reply("500 Oops", "", ""),
        })
    });
    dav.clear();
    tokio::time::sleep(Duration::from_millis(5)).await;
    assert_eq!(
        call(addr, FOLDER, ADMIN, r#"{"path":" a\\b c/ "}"#).await,
        (200, j(r#"{"path":"a/b c","existed":false}"#))
    );
    assert_eq!(dav.seen()[0].method, "MKCOL");
    let second = j(&std::fs::read_to_string(d.path().join("storage-login-proofs.json")).unwrap())
        ["proofs"][A]
        .as_f64()
        .unwrap();
    assert!(second >= first);
    assert_eq!(
        call(addr, FOLDER, ADMIN, r#"{"path":"there"}"#).await,
        (200, j(r#"{"path":"there","existed":true}"#))
    );
    assert_eq!(
        call(addr, FOLDER, ADMIN, r#"{"path":"conflict/x"}"#).await,
        (
            400,
            j(r#"{"error":"could not create \"conflict/x\" (409)"}"#)
        )
    );
    assert_eq!(
        call(addr, FOLDER, ADMIN, r#"{"path":"boom"}"#).await,
        (502, j(r#"{"error":"could not create \"boom\" (500)"}"#))
    );
    for bad in [r#"{"path":"../x"}"#, r#"{"path":"/abs"}"#, "{}"] {
        assert_eq!(
            call(addr, FOLDER, ADMIN, bad).await,
            (400, j(r#"{"error":"invalid folder path"}"#)),
            "{bad}"
        );
    }

    // S3: a signed bucket listing; S3 has no folders.
    save_row(&db, A, "s3", &base, "bucket-1", "s3-secret");
    assert_eq!(
        call(addr, FOLDER, ADMIN, r#"{"path":"x"}"#).await,
        (
            400,
            j(r#"{"error":"S3 has no folders — a prefix appears once a file is stored under it"}"#)
        )
    );
    dav.set(|_: &Seen| now(reply("403 Forbidden", "", "")));
    dav.clear();
    assert_eq!(
        call(addr, TEST, ADMIN, r#"{"useSaved":true}"#).await,
        (
            502,
            j(r#"{"error":"S3 returned 403 — check the access key and secret"}"#)
        )
    );
    let r = dav.seen()[0].clone();
    assert_eq!(
        (r.method.as_str(), r.path.as_str()),
        ("GET", "/bucket-1?list-type=2&max-keys=1")
    );
    assert!(
        r.headers
            .contains("authorization: aws4-hmac-sha256 credential=alice/"),
        "{}",
        r.headers
    );
    assert!(r.headers.contains("/us-east-1/s3/aws4_request"));
    assert_eq!(
        call(
            addr,
            TEST,
            ADMIN,
            &format!(r#"{{"kind":"s3","baseUrl":"{base}","bucket":"  "}}"#)
        )
        .await,
        (400, j(r#"{"error":"Bucket is required"}"#))
    );
    f.stop().await;
}

/// A Nextcloud whose answers are a table of `METHOD path` -> reply (404 otherwise).
fn table(nc: &Upstream, routes: Vec<(&'static str, String)>) {
    let routes: HashMap<String, String> = routes
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
    nc.set(move |r: &Seen| {
        now(routes
            .get(&format!("{} {}", r.method, r.path))
            .cloned()
            .unwrap_or_else(|| reply("404 Not Found", "", "")))
    });
}

async fn begin(addr: std::net::SocketAddr, base: &str) -> String {
    let (s, b) = call(addr, START, ADMIN, &format!(r#"{{"baseUrl":"{base}"}}"#)).await;
    assert_eq!(s, 200, "{b}");
    b["flowId"].as_str().unwrap().to_string()
}

fn login_v2(_base: &str, poll: &str, login: &str) -> String {
    reply(
        "200 OK",
        "",
        &format!(
            r#"{{"poll":{{"token":"synthetic-poll-token","endpoint":"{poll}"}},"login":"{login}"}}"#
        ),
    )
}

#[tokio::test]
async fn nextcloud_replies_caps_folders_and_revocation() {
    let d = tempfile::tempdir().unwrap();
    let db = seed(d.path());
    let nc = upstream(true).await;
    let base = nc.base("https");
    let f = front(d.path()).await;
    let addr = f.addr;
    let start = |nc: &Upstream, extra: Vec<(&'static str, String)>| {
        let mut routes = vec![(
            "POST /index.php/login/v2",
            login_v2(
                &base,
                &format!("{base}/login/v2/poll"),
                &format!("{base}/login/v2/flow/x"),
            ),
        )];
        routes.extend(extra);
        table(nc, routes);
    };
    let deletes = |nc: &Upstream| nc.seen().iter().filter(|r| r.method == "DELETE").count();

    // #902: a reply over 64 KiB is refused, at start and at poll.
    let huge = format!(r#"{{"login":"{}"}}"#, "x".repeat(70 * 1024));
    table(
        &nc,
        vec![("POST /index.php/login/v2", reply("200 OK", "", &huge))],
    );
    let (s, b) = call(addr, START, ADMIN, &format!(r#"{{"baseUrl":"{base}"}}"#)).await;
    assert_eq!((s, b["code"].as_str()), (502, Some("nextcloudNotFound")));
    start(
        &nc,
        vec![("POST /login/v2/poll", reply("200 OK", "", &huge))],
    );
    let id = begin(addr, &base).await;
    assert_eq!(
        call(addr, POLL, ADMIN, &format!(r#"{{"flowId":"{id}"}}"#)).await,
        (502, j(r#"{"error":"Nextcloud reply too large"}"#))
    );

    // #1432: a plain-http login page or poll endpoint is refused, and the endpoint never contacted.
    table(
        &nc,
        vec![(
            "POST /index.php/login/v2",
            login_v2(
                &base,
                &format!("{base}/login/v2/poll"),
                "http://127.0.0.1/login",
            ),
        )],
    );
    assert_eq!(
        call(addr, START, ADMIN, &format!(r#"{{"baseUrl":"{base}"}}"#)).await,
        (400, j(r#"{"error":"Invalid connection URLs"}"#))
    );
    table(
        &nc,
        vec![(
            "POST /index.php/login/v2",
            login_v2(
                &base,
                &format!("http://{}/login/v2/poll", nc.addr),
                &format!("{base}/x"),
            ),
        )],
    );
    let (s, b) = call(addr, START, ADMIN, &format!(r#"{{"baseUrl":"{base}"}}"#)).await;
    assert_eq!(
        (s, b["code"].as_str()),
        (400, Some("nextcloudHttpsRequired"))
    );

    // #1432: an app password for another origin or plain http is refused before it is sent anywhere.
    let other = format!(
        r#"{{"server":"https://localhost:{}","loginName":"ada","appPassword":"synthetic-app-password"}}"#,
        nc.addr.port()
    );
    start(
        &nc,
        vec![("POST /login/v2/poll", reply("200 OK", "", &other))],
    );
    let id = begin(addr, &base).await;
    nc.clear();
    let (s, b) = call(addr, POLL, ADMIN, &format!(r#"{{"flowId":"{id}"}}"#)).await;
    assert_eq!(
        (s, b["code"].as_str()),
        (502, Some("nextcloudOriginMismatch"))
    );
    assert_eq!(nc.seen().len(), 1, "only the poll itself");
    let plain = format!(
        r#"{{"server":"http://{}","loginName":"ada","appPassword":"synthetic-app-password"}}"#,
        nc.addr
    );
    start(
        &nc,
        vec![("POST /login/v2/poll", reply("200 OK", "", &plain))],
    );
    let id = begin(addr, &base).await;
    let (s, b) = call(addr, POLL, ADMIN, &format!(r#"{{"flowId":"{id}"}}"#)).await;
    assert_eq!(
        (s, b["code"].as_str()),
        (502, Some("nextcloudHttpsRequired"))
    );
    assert_eq!(deletes(&nc), 0);

    // #1433: every failure once the trusted server issued the password revokes it.
    let creds = format!(
        r#"{{"server":"{base}","loginName":"ada@example.test","appPassword":"synthetic-app-password"}}"#
    );
    for (who, propfind, status, code) in [
        (
            reply("401 Unauthorized", "", ""),
            reply("207 Multi-Status", "", ""),
            502,
            "storageLoginRejected",
        ),
        (
            reply("429 Too Many Requests", "Retry-After: 12\r\n", ""),
            reply("207 Multi-Status", "", ""),
            429,
            "nextcloudThrottled",
        ),
        (
            reply("200 OK", "", r#"{"ocs":{"data":{"id":"ada"}}}"#),
            reply("429 Too Many Requests", "", ""),
            429,
            "nextcloudThrottled",
        ),
    ] {
        start(
            &nc,
            vec![
                ("POST /login/v2/poll", reply("200 OK", "", &creds)),
                ("GET /ocs/v2.php/cloud/user?format=json", who),
                ("PROPFIND /remote.php/dav/files/ada/", propfind),
                (
                    "DELETE /ocs/v2.php/core/apppassword",
                    reply("500 Oops", "", ""),
                ),
            ],
        );
        let id = begin(addr, &base).await;
        nc.clear();
        let (s, b) = call(addr, POLL, ADMIN, &format!(r#"{{"flowId":"{id}"}}"#)).await;
        assert_eq!((s, b["code"].as_str()), (status, Some(code)), "{b}");
        assert_eq!(
            deletes(&nc),
            1,
            "revoked once (a failed revoke never changes the answer)"
        );
    }
    assert!(row(&db, A).is_none());

    // #1427: no user id from Nextcloud: the login name is the WebDAV home; the folder sent is kept
    // (trimmed); an unreachable check still saves, with a warning.
    start(
        &nc,
        vec![
            ("POST /login/v2/poll", reply("200 OK", "", &creds)),
            (
                "GET /ocs/v2.php/cloud/user?format=json",
                reply("500 Oops", "", ""),
            ),
            (
                "PROPFIND /remote.php/dav/files/ada%40example.test/",
                reply("503 Unavailable", "", ""),
            ),
        ],
    );
    let id = begin(addr, &base).await;
    let (s, b) = call(
        addr,
        POLL,
        ADMIN,
        &format!(r#"{{"flowId":"{id}","corpusRoot":"  Documents/Diary "}}"#),
    )
    .await;
    assert_eq!(s, 200, "{b}");
    assert_eq!(
        b["baseUrl"],
        format!("{base}/remote.php/dav/files/ada%40example.test")
    );
    assert_eq!(b["corpusRoot"], "Documents/Diary");
    assert_eq!(b["warningCode"], "storageUnverified");
    assert_eq!(b["status"], 503);
    assert_eq!(
        b["warning"],
        "Saved, but the login could not be checked (the storage server answered 503)."
    );
    assert_eq!(audits(&db).len(), 1);
    // The next sign-in sends no folder: the one in use is kept.
    start(
        &nc,
        vec![
            ("POST /login/v2/poll", reply("200 OK", "", &creds)),
            (
                "GET /ocs/v2.php/cloud/user?format=json",
                reply("200 OK", "", r#"{"ocs":{"data":{"id":"ada"}}}"#),
            ),
            (
                "PROPFIND /remote.php/dav/files/ada/",
                reply("207 Multi-Status", "", ""),
            ),
        ],
    );
    let id = begin(addr, &base).await;
    let (s, b) = call(addr, POLL, ADMIN, &format!(r#"{{"flowId":"{id}"}}"#)).await;
    assert_eq!(
        (s, b["corpusRoot"].as_str(), b.get("warning")),
        (200, Some("Documents/Diary"), None),
        "{b}"
    );
    assert_eq!(
        row(&db, A).unwrap().1,
        format!("{base}/remote.php/dav/files/ada")
    );
    assert_eq!(audits(&db).len(), 2);

    // S3: an unreachable endpoint is the one 502; the saved secret signs an edited bucket.
    let s3 = upstream(false).await;
    save_row(&db, A, "s3", &s3.base("http"), "bucket-1", "s3-secret");
    s3.set(|_: &Seen| now(reply("200 OK", "", "<ListBucketResult/>")));
    let edited = format!(
        r#"{{"kind":"s3","baseUrl":"{}","bucket":"edited","username":"edited-key","useSavedSecret":true}}"#,
        s3.base("http")
    );
    assert_eq!(
        call(addr, TEST, ADMIN, &edited).await,
        (200, j(r#"{"ok":true}"#))
    );
    let r = s3.seen()[0].clone();
    assert_eq!(r.path, "/edited?list-type=2&max-keys=1");
    assert!(
        r.headers.contains("credential=edited-key/"),
        "{}",
        r.headers
    );
    assert_eq!(
        call(addr, TEST, ADMIN, r#"{"kind":"s3","baseUrl":"http://127.0.0.1:1","bucket":"b","username":"ak","secret":"sk"}"#).await,
        (502, j(r#"{"error":"fetch failed"}"#))
    );
    f.stop().await;
}

// ---------------------------------------------------------------------------------------------
// sbstndalton/noevia#2190 round 4: PUT /api/integrations/storage and the legacy WebDAV store op.

const CONNECTION: &str = "/api/integrations/storage";

async fn put(addr: std::net::SocketAddr, cookie: &str, body: &str) -> (u16, serde_json::Value) {
    let csrf = format!("Origin: http://{addr}\r\nX-CSRF-Token: csrf\r\n");
    raw(addr, "PUT", CONNECTION, cookie, &csrf, body.as_bytes()).await
}

/// The reply text as sent (key order included).
async fn put_text(addr: std::net::SocketAddr, cookie: &str, body: &str) -> String {
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(format!("PUT {CONNECTION} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nCookie: {cookie}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nOrigin: http://{addr}\r\nX-CSRF-Token: csrf\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
    let mut out = Vec::new();
    let _ = s.read_to_end(&mut out).await;
    let text = String::from_utf8(out).unwrap();
    text.split_once("\r\n\r\n").unwrap().1.to_string()
}

fn proofs(dir: &std::path::Path) -> Option<serde_json::Value> {
    std::fs::read_to_string(dir.join("storage-login-proofs.json"))
        .ok()
        .map(|t| j(&t))
}

#[tokio::test]
async fn put_connection_validates_checks_and_saves() {
    let d = tempfile::tempdir().unwrap();
    let db = seed(d.path());
    let dav = upstream(false).await;
    let base = dav.base("http");
    let f = front(d.path()).await;
    let addr = f.addr;
    let required = r#"{"error":"server URL, username, and app password are required"}"#;

    // The gate, then the body (core routes/storage.cjs order).
    assert_eq!(raw(addr, "PUT", CONNECTION, "", "", b"{}").await.0, 401);
    assert_eq!(raw(addr, "PUT", CONNECTION, ADMIN, "", b"{}").await.0, 403);
    assert_eq!(
        put(addr, ADMIN, "[]").await,
        (400, j(r#"{"error":"request body must be a JSON object"}"#))
    );
    assert_eq!(
        put(addr, ADMIN, "null").await,
        (400, j(r#"{"error":"request body must be a JSON object"}"#))
    );
    assert_eq!(put(addr, ADMIN, "{}").await, (400, j(required)));
    for body in [
        r#"{"kind":"webdav","baseUrl":"ftp://x","username":"u","secret":"s"}"#,
        r#"{"kind":"webdav","baseUrl":"HTTP://x","username":"u","secret":"s"}"#,
        r#"{"kind":"webdav","baseUrl":"http://x","username":"","secret":"s"}"#,
        r#"{"kind":"s3","baseUrl":"http://x","username":"u","secret":0}"#,
        r#"{"kind":"bogus","baseUrl":"http://x","username":"u"}"#,
    ] {
        assert_eq!(put(addr, ADMIN, body).await, (400, j(required)), "{body}");
    }
    let webdav = |user: &str, secret: &str| {
        format!(
            r#"{{"kind":"webdav","baseUrl":"{base}/dav","username":"{user}","secret":"{secret}","corpusRoot":"Notes"}}"#
        )
    };
    // A member may not save an origin that is not approved; nothing is contacted.
    dav.clear();
    assert_eq!(
        put(addr, MEMBER, &webdav("alice", "s")).await,
        (400, serde_json::json!({ "error": PRIVATE }))
    );
    assert!(dav.seen().is_empty());
    let region = r#"{"error":"Region must be 1-32 lowercase letters, digits, or dashes (for example eu-west-1)"}"#;
    for r in [r#""EU""#, r#""a_b""#, "12345678901234567890123456789012345"] {
        let body = format!(r#"{{"kind":"local","region":{r}}}"#);
        assert_eq!(put(addr, ADMIN, &body).await, (400, j(region)), "{r}");
    }
    assert!(row(&db, A).is_none());

    // A rejected login keeps the old row and saves nothing.
    save_row(&db, A, "webdav", "http://old.invalid", "", "old-secret");
    dav.set(|_: &Seen| now(reply("401 Unauthorized", "", "")));
    assert_eq!(
        put(addr, ADMIN, &webdav("alice", "wrong")).await,
        (
            400,
            j(
                r#"{"error":"The server rejected this username or app password.","code":"storageLoginRejected","status":401}"#
            )
        )
    );
    assert_eq!(row(&db, A).unwrap().1, "http://old.invalid");
    let r = dav.seen().last().unwrap().clone();
    assert_eq!((r.method.as_str(), r.path.as_str()), ("PROPFIND", "/dav/"));
    assert!(r.headers.contains("depth: 0"));
    // basic base64("alice:wrong")
    assert!(
        r.headers.contains("authorization: basic ywxpy2u6d3jvbmc="),
        "{}",
        r.headers
    );
    assert!(audits(&db).is_empty());
    assert!(proofs(d.path()).is_none());

    // An accepted login saves the row (sealed to the account), its audit row and the proof.
    dav.set(|_: &Seen| now(reply("207 Multi-Status", "", "")));
    let (status, body) = put(addr, ADMIN, &webdav("alice", "synthetic-secret")).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body,
        serde_json::json!({"kind":"webdav","baseUrl":format!("{base}/dav"),"bucket":"","region":"us-east-1","username":"alice","corpusRoot":"Notes","secretConfigured":true,"secretNeedsReauth":false})
    );
    let saved = row(&db, A).unwrap();
    assert_eq!(
        (
            saved.0.as_str(),
            saved.2.as_str(),
            saved.4.as_str(),
            saved.5.as_str()
        ),
        ("webdav", "alice", "Notes", "us-east-1")
    );
    assert!(saved.3.starts_with("enc:v2:"), "{}", saved.3);
    assert_eq!(open(&saved.3, A), "synthetic-secret");
    assert_eq!(
        audits(&db),
        vec![(
            A.into(),
            A.into(),
            "storage.update".into(),
            r#"{"kind":"webdav"}"#.into()
        )]
    );
    assert!(proofs(d.path()).unwrap()["proofs"][A].is_number());

    // Not checked: saved anyway, with Node's warning fields after the connection; no new proof.
    std::fs::remove_file(d.path().join("storage-login-proofs.json")).unwrap();
    let cases: Vec<(&str, serde_json::Value)> = vec![
        (
            "403 Forbidden",
            serde_json::json!({"warning":"Saved, but the login could not be checked (the storage server answered 403).","warningCode":"storageUnverified","status":403}),
        ),
        (
            "503 Busy",
            serde_json::json!({"warning":"Saved, but the login could not be checked (the storage server answered 503).","warningCode":"storageUnverified","status":503}),
        ),
        (
            "302 Found",
            serde_json::json!({"warning":"Saved, but the server redirected the login check. Check the URL (https, trailing slash).","warningCode":"storageUnverified","reason":"redirect"}),
        ),
        (
            "429 Too Many",
            serde_json::json!({"warning":"Saved, but the storage server is throttling logins from noevia after too many failed attempts, so the login was not checked. Wait, then press Test.","warningCode":"storageThrottled","status":429,"retryAfter":30}),
        ),
    ];
    for (line, want) in cases {
        let line = line.to_string();
        dav.set(move |_: &Seen| now(reply(&line, "Retry-After: 29.5\r\nLocation: /x\r\n", "")));
        let (status, body) = put(addr, ADMIN, &webdav("bob", "s2")).await;
        assert_eq!(status, 200, "{body}");
        let obj = body.as_object().unwrap();
        for (k, v) in want.as_object().unwrap() {
            assert_eq!(&obj[k], v, "{k}");
        }
        assert_eq!(obj.len(), 8 + want.as_object().unwrap().len());
        assert!(
            !body.to_string().contains("s2\""),
            "the secret is never answered"
        );
        assert_eq!(row(&db, A).unwrap().2, "bob");
    }
    assert!(proofs(d.path()).is_none());
    // Node's `{ ...saved, ...warning }`: the connection's keys first, then the warning's, in order.
    let text = put_text(addr, ADMIN, &webdav("bob", "s2")).await;
    let order: Vec<usize> = [
        "\"kind\"",
        "\"baseUrl\"",
        "\"bucket\"",
        "\"region\"",
        "\"username\"",
        "\"corpusRoot\"",
        "\"secretConfigured\"",
        "\"secretNeedsReauth\"",
        "\"warning\"",
        "\"warningCode\"",
        "\"status\"",
        "\"retryAfter\"",
    ]
    .iter()
    .map(|k| text.find(k).unwrap_or_else(|| panic!("{k} in {text}")))
    .collect();
    assert!(order.windows(2).all(|w| w[0] < w[1]), "{text}");

    // A trailing slash is stripped from the save, so the proof (core fingerprintOf the body)
    // is not written: Node's loginProven compared the saved row with the body as sent.
    dav.set(|_: &Seen| now(reply("200 OK", "", "")));
    let slashed = format!(
        r#"{{"kind":"nextcloud","baseUrl":"{base}/dav//","username":"alice","secret":"s3"}}"#
    );
    let (status, body) = put(addr, ADMIN, &slashed).await;
    assert_eq!(
        (status, body["baseUrl"].clone()),
        (200, serde_json::json!(format!("{base}/dav")))
    );
    assert_eq!(body["corpusRoot"], "");
    assert!(proofs(d.path()).is_none());

    // S3 and local are not login-checked; bucket slashes and the region are normalized.
    dav.clear();
    let s3 = format!(
        r#"{{"kind":"s3","baseUrl":"{base}/","bucket":"notes//","region":"eu-west-1","username":"AKIA","secret":"k"}}"#
    );
    let (status, body) = put(addr, ADMIN, &s3).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        (body["bucket"].clone(), body["region"].clone()),
        (serde_json::json!("notes"), serde_json::json!("eu-west-1"))
    );
    assert!(dav.seen().is_empty());
    // The body's userId is ignored: the save is the session account's alone.
    let (status, body) = put(
        addr,
        ADMIN,
        &format!(r#"{{"kind":"local","region":"","userId":"{B}"}}"#),
    )
    .await;
    assert_eq!(
        (status, body),
        (
            200,
            j(
                r#"{"kind":"local","baseUrl":"","bucket":"","region":"us-east-1","username":"","corpusRoot":"","secretConfigured":false,"secretNeedsReauth":false}"#
            )
        )
    );
    assert_eq!(row(&db, A).unwrap().3, "");
    // The other account's row is untouched by A's saves.
    assert!(row(&db, B).is_none());
    // Another method on the path is Node's (its upstream is dead here).
    let csrf = format!("Origin: http://{addr}\r\nX-CSRF-Token: csrf\r\n");
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(format!("DELETE {CONNECTION} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nCookie: {ADMIN}\r\n{csrf}Content-Length: 0\r\n\r\n").as_bytes()).await.unwrap();
    let mut out = Vec::new();
    let _ = s.read_to_end(&mut out).await;
    let text = String::from_utf8_lossy(&out);
    assert!(!text.starts_with("HTTP/1.1 200"), "{text}");
    f.stop().await;
}

#[tokio::test]
async fn legacy_webdav_store_op_saves_only_an_unconfigured_account() {
    use noevia_server::project_store::{serve_with_context, Store, TOKEN_HEADER};
    const TOKEN: &str = "synthetic-store-token-0123456789abcdef";
    let d = tempfile::tempdir().unwrap();
    let db = seed(d.path());
    let env = HashMap::from([
        ("NOEVIA_LEGACY_UPSTREAM", "http://127.0.0.1:9".to_string()),
        ("UI_DATA_DIR", d.path().display().to_string()),
    ]);
    let app = App::new(Config::from_lookup(|k| env.get(k).cloned()).unwrap());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let store = Store {
        data_dir: d.path().to_path_buf(),
        token: TOKEN.into(),
        switch: server_store::RustProjects::owned(),
        start_id: "".into(),
    };
    tokio::spawn(serve_with_context(
        listener,
        store,
        None,
        Some(app),
        std::future::pending(),
    ));
    let op = |token: Option<&'static str>, body: String| async move {
        let mut s = TcpStream::connect(addr).await.unwrap();
        let mut req = format!("POST /v1/storage/legacy-webdav HTTP/1.1\r\nhost: store\r\nconnection: close\r\ncontent-type: application/json\r\ncontent-length: {}\r\n", body.len());
        if let Some(t) = token {
            req.push_str(&format!("{TOKEN_HEADER}: {t}\r\n"));
        }
        req.push_str("\r\n");
        s.write_all(req.as_bytes()).await.unwrap();
        s.write_all(body.as_bytes()).await.unwrap();
        let mut out = Vec::new();
        s.read_to_end(&mut out).await.unwrap();
        let text = String::from_utf8_lossy(&out).into_owned();
        let status: u16 = text[9..12].parse().unwrap();
        (status, j(text.split_once("\r\n\r\n").unwrap().1))
    };
    let legacy = |user: &str| {
        format!(
            r#"{{"user":"{user}","baseUrl":"https://dav.example.test/remote/","username":"legacy","secret":"legacy-secret","corpusRoot":"Cowork/Diary"}}"#
        )
    };
    assert_eq!(op(None, legacy(A)).await.0, 401);
    assert_eq!(
        op(Some("wrong-token-wrong-token-wrong-token!"), legacy(A))
            .await
            .0,
        401
    );
    assert!(row(&db, A).is_none());
    assert_eq!(op(Some(TOKEN), "[]".into()).await.0, 400);
    assert_eq!(op(Some(TOKEN), r#"{"user":""}"#.into()).await.0, 400);

    // No row: saved as WebDAV, sealed to the account, with its audit row.
    assert_eq!(
        op(Some(TOKEN), legacy(A)).await,
        (200, j(r#"{"saved":true}"#))
    );
    let saved = row(&db, A).unwrap();
    assert_eq!(
        (
            saved.0.as_str(),
            saved.1.as_str(),
            saved.2.as_str(),
            saved.4.as_str()
        ),
        (
            "webdav",
            "https://dav.example.test/remote",
            "legacy",
            "Cowork/Diary"
        )
    );
    assert_eq!(open(&saved.3, A), "legacy-secret");
    assert_eq!(audits(&db).len(), 1);
    // A configured row is never replaced.
    save_row(
        &db,
        A,
        "nextcloud",
        "https://cloud.example.test",
        "",
        "mine",
    );
    assert_eq!(
        op(Some(TOKEN), legacy(A)).await,
        (200, j(r#"{"saved":false}"#))
    );
    assert_eq!(row(&db, A).unwrap().0, "nextcloud");
    assert_eq!(audits(&db).len(), 1);
    // A local row is.
    save_row(&db, B, "local", "", "", "");
    assert_eq!(
        op(Some(TOKEN), legacy(B)).await,
        (200, j(r#"{"saved":true}"#))
    );
    assert_eq!(row(&db, B).unwrap().0, "webdav");
}
