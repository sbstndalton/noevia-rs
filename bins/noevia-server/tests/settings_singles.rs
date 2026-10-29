//! Real HTTP through the native front for the settings singles of sbstndalton/noevia#2190: the
//! public pages (`/about`, `/privacy`), the device approval page (`/device`) and the plugin
//! directory (`/api/plugins/directory`). The legacy upstream is dead (any proxied request is the
//! proxy's own 502), and the directory reads a synthetic registry on loopback that counts its hits;
//! nothing reaches the Internet. Synthetic accounts only.
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
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Duration;
use std::{collections::HashMap, path::Path, sync::Arc};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot,
};

const A: &str = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
const B: &str = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";
const ADMIN: &str = "cowork_session=admin-token; cowork_csrf=csrf";
const MEMBER: &str = "cowork_session=member-token; cowork_csrf=csrf";
const DIRECTORY: &str = "/api/plugins/directory";

fn hex(s: &str) -> String {
    Sha256::digest(s.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

async fn call(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    headers: &str,
) -> (u16, String, String) {
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(
        format!(
            "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n{headers}Content-Length: 0\r\n\r\n"
        )
        .as_bytes(),
    )
    .await
    .unwrap();
    let mut bytes = Vec::new();
    tokio::time::timeout(Duration::from_secs(30), s.read_to_end(&mut bytes))
        .await
        .unwrap()
        .unwrap();
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let (h, b) = text.split_once("\r\n\r\n").unwrap();
    (
        h.split_whitespace().nth(1).unwrap().parse().unwrap(),
        h.to_ascii_lowercase(),
        b.to_string(),
    )
}

fn cookie(c: &str) -> String {
    format!("Cookie: {c}\r\n")
}

fn seed_database(dir: &Path, native_client_auth: Option<bool>) -> Connection {
    let db = Connection::open(dir.join("cowork.db")).unwrap();
    db.execute_batch(include_str!(
        "../../../crates/server-store/tests/fixtures/node-schema.sql"
    ))
    .unwrap();
    let now = noevia_server::identity::now_ms();
    for (uid, role, token) in [(A, "admin", "admin-token"), (B, "member", "member-token")] {
        db.execute("INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,created_at,updated_at) VALUES(?1,?1,?1,'Synthetic',?2,'x',?1,1,1)", (uid, role)).unwrap();
        db.execute(
            "INSERT INTO sessions VALUES(?1,?2,?3,?4,?4,?5,'ua','127.0.0.1')",
            (hex(token), uid, hex("csrf"), now, now + 86_400_000),
        )
        .unwrap();
    }
    if let Some(on) = native_client_auth {
        db.execute(
            "INSERT INTO settings(key,value) VALUES('feature:nativeClientAuth',?1)",
            [if on { "true" } else { "false" }],
        )
        .unwrap();
    }
    db
}

async fn start(
    dir: &Path,
    extra: &[(&str, String)],
) -> (std::net::SocketAddr, oneshot::Sender<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut env: HashMap<&str, String> = HashMap::from([
        ("NOEVIA_LEGACY_UPSTREAM", "http://127.0.0.1:9".into()),
        ("UI_DATA_DIR", dir.display().to_string()),
        ("TRUST_PROXY", "true".into()),
        ("PUBLIC_ORIGIN", format!("http://{addr}")),
        // No test may reach the real directory hosts.
        ("NOEVIA_QA_MCP_REGISTRY", "http://127.0.0.1:9".into()),
        (
            "NOEVIA_QA_SKILLS_DIRECTORY",
            "http://127.0.0.1:9/skills".into(),
        ),
    ]);
    for (k, v) in extra {
        env.insert(k, v.clone());
    }
    let app = App::new(Config::from_lookup(|k| env.get(k).cloned()).unwrap());
    let (stop, rx) = oneshot::channel();
    tokio::spawn(serve(
        listener,
        Arc::clone(&app),
        Limits::default(),
        async {
            let _ = rx.await;
        },
    ));
    (addr, stop)
}

/// A synthetic directory host: answers each request with `respond(path_and_query)` and records
/// what it was asked.
struct Upstream {
    addr: std::net::SocketAddr,
    hits: Arc<Mutex<Vec<String>>>,
    count: Arc<AtomicUsize>,
}

type Respond = fn(&str) -> (u16, Vec<(&'static str, String)>, Vec<u8>);

async fn upstream(respond: Respond) -> Upstream {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let hits = Arc::new(Mutex::new(Vec::new()));
    let count = Arc::new(AtomicUsize::new(0));
    let (h, c) = (Arc::clone(&hits), Arc::clone(&count));
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = listener.accept().await else {
                return;
            };
            let (h, c) = (Arc::clone(&h), Arc::clone(&c));
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    match s.read(&mut chunk).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    }
                }
                let head = String::from_utf8_lossy(&buf).into_owned();
                let target = head.split_whitespace().nth(1).unwrap_or("").to_owned();
                h.lock().unwrap().push(head.to_ascii_lowercase());
                c.fetch_add(1, Ordering::SeqCst);
                let (status, headers, body) = respond(&target);
                let mut out = format!(
                    "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n",
                    body.len()
                );
                for (k, v) in headers {
                    out.push_str(&format!("{k}: {v}\r\n"));
                }
                out.push_str("\r\n");
                let _ = s.write_all(out.as_bytes()).await;
                let _ = s.write_all(&body).await;
            });
        }
    });
    Upstream { addr, hits, count }
}

fn registry(target: &str) -> (u16, Vec<(&'static str, String)>, Vec<u8>) {
    if target.starts_with("/skills") {
        return (200, vec![], br#"[{"type":"dir","name":"pdf","html_url":"https://github.com/anthropics/skills/tree/main/skills/pdf"},{"type":"dir","name":"skill-creator"},{"type":"file","name":"README.md"}]"#.to_vec());
    }
    if target.contains("search=boom") {
        return (500, vec![], b"{}".to_vec());
    }
    if target.contains("search=moved") {
        return (
            302,
            vec![("Location", "https://example.invalid/".into())],
            Vec::new(),
        );
    }
    if target.contains("search=huge") {
        let mut body = b"{\"servers\":[\"".to_vec();
        body.resize(4 * 1024 * 1024 + 16, b'a');
        body.extend_from_slice(b"\"]}");
        return (200, vec![], body);
    }
    if target.contains("search=ai.exa%2Fexa") {
        return (200, vec![], br#"{"servers":[{"server":{"name":"ai.exa/exa","remotes":[{"type":"streamable-http","url":"https://mcp.exa.ai/mcp"}]}}]}"#.to_vec());
    }
    (200, vec![], br#"{"servers":[{"server":{"name":"com.notion/mcp","description":"Synthetic","remotes":[{"type":"streamable-http","url":"https://mcp.notion.example/mcp"}]}},{"server":{"name":"io.pkg/local"}}]}"#.to_vec())
}

#[tokio::test]
async fn public_pages_are_nodes_bytes_and_need_no_session() {
    let d = tempfile::tempdir().unwrap();
    let _db = seed_database(d.path(), None);
    let (addr, stop) = start(d.path(), &[]).await;
    for (path, title) in [("/about", "About"), ("/privacy", "Privacy policy")] {
        let (s, h, b) = call(addr, "GET", path, "").await;
        assert_eq!(s, 200, "{path}");
        assert!(h.contains("content-type: text/html; charset=utf-8"));
        assert!(h.contains("cache-control: public, max-age=300"));
        assert!(h.contains("strict-transport-security: max-age=15552000"));
        assert!(!h.contains("x-noevia-api"));
        assert!(
            b.contains(&format!("<title>{title} · noevia</title>")),
            "{path}"
        );
        let (s, _, b) = call(addr, "HEAD", path, "").await;
        assert_eq!((s, b.as_str()), (200, ""), "HEAD {path}");
        // Other methods and the trailing-slash form were never these pages: Node's (dead here).
        let (s, _, _) = call(addr, "POST", path, "").await;
        assert_eq!(s, 502, "POST {path}");
        let (s, _, _) = call(addr, "GET", &format!("{path}/"), "").await;
        assert_eq!(s, 502, "{path}/");
    }
    let _ = stop.send(());
}

#[tokio::test]
async fn the_device_page_is_a_404_while_native_sign_in_is_off_and_the_bundle_when_on() {
    let d = tempfile::tempdir().unwrap();
    let db = seed_database(d.path(), Some(false));
    let dist = d.path().join("dist");
    std::fs::create_dir_all(&dist).unwrap();
    std::fs::write(dist.join("index.html"), "<!doctype html><title>spa</title>").unwrap();
    let (addr, stop) = start(d.path(), &[("NOEVIA_WEB_DIST", dist.display().to_string())]).await;
    for p in ["/device", "/device/"] {
        for m in ["GET", "POST"] {
            let (s, h, b) = call(addr, m, p, "Accept: text/html\r\n").await;
            assert_eq!(
                (s, b.as_str()),
                (404, r#"{"error":"not found"}"#),
                "{m} {p}"
            );
            assert!(h.contains("x-frame-options: deny"));
        }
    }
    // Switched on: the bundle's approval page, read per request (no restart).
    db.execute(
        "UPDATE settings SET value='true' WHERE key='feature:nativeClientAuth'",
        [],
    )
    .unwrap();
    let (s, _, b) = call(addr, "GET", "/device", "Accept: text/html\r\n").await;
    assert_eq!(s, 200, "{b}");
    assert!(b.contains("<title>spa</title>"));
    // The WHATWG pathname decides, as in Node: a dot-segment spelling is the same page.
    let (s, _, b) = call(addr, "GET", "/x/../device", "Accept: text/html\r\n").await;
    assert_eq!(s, 200, "{b}");
    assert!(b.contains("<title>spa</title>"));
    let _ = stop.send(());
}

#[tokio::test]
async fn the_device_page_fails_closed_without_the_database() {
    let d = tempfile::tempdir().unwrap();
    let (addr, stop) = start(d.path(), &[]).await;
    let (s, _, b) = call(addr, "GET", "/device", "").await;
    assert_eq!(s, 503, "{b}");
    let _ = stop.send(());
}

#[tokio::test]
async fn the_directory_gate_and_methods_follow_core() {
    let d = tempfile::tempdir().unwrap();
    let _db = seed_database(d.path(), None);
    let up = upstream(registry).await;
    let base = format!("http://{}", up.addr);
    let (addr, stop) = start(d.path(), &[("NOEVIA_QA_MCP_REGISTRY", base)]).await;
    let (s, h, _) = call(addr, "GET", DIRECTORY, "").await;
    assert_eq!(s, 401);
    assert!(h.contains("www-authenticate"));
    // A write without the CSRF pair is Node's 403 before the 405.
    let (s, _, b) = call(addr, "POST", DIRECTORY, &cookie(MEMBER)).await;
    assert_eq!((s, b.as_str()), (403, r#"{"error":"invalid CSRF token"}"#));
    let csrf = format!(
        "{}X-CSRF-Token: csrf\r\nOrigin: http://{addr}\r\n",
        cookie(MEMBER)
    );
    for m in ["POST", "PUT", "DELETE"] {
        let (s, _, b) = call(addr, m, DIRECTORY, &csrf).await;
        assert_eq!(
            (s, b.as_str()),
            (405, r#"{"error":"method not allowed"}"#),
            "{m}"
        );
    }
    let (s, _, _) = call(addr, "HEAD", DIRECTORY, &cookie(MEMBER)).await;
    assert_eq!(s, 405);
    assert_eq!(
        up.count.load(Ordering::SeqCst),
        0,
        "no refused request reached the directory"
    );
    let (s, _, _) = call(addr, "GET", "/api/plugins/directory/", &cookie(MEMBER)).await;
    assert_eq!(s, 502, "the trailing-slash form is Node's (dead here)");
    let _ = stop.send(());
}

#[tokio::test]
async fn the_directory_lists_caches_and_fails_like_core() {
    let d = tempfile::tempdir().unwrap();
    let _db = seed_database(d.path(), None);
    let up = upstream(registry).await;
    let base = format!("http://{}", up.addr);
    let (addr, stop) = start(
        d.path(),
        &[
            ("NOEVIA_QA_MCP_REGISTRY", base.clone()),
            ("NOEVIA_QA_SKILLS_DIRECTORY", format!("{base}/skills")),
        ],
    )
    .await;
    let (s, h, b) = call(addr, "GET", DIRECTORY, &cookie(MEMBER)).await;
    assert_eq!(s, 200, "{b}");
    assert!(h.contains("x-noevia-api: 1") && h.contains("cache-control: no-store"));
    assert_eq!(
        b,
        r#"{"items":[{"id":"com.notion/mcp","name":"Notion","publisher":"com.notion","description":"Synthetic","version":"","url":"","remote":true,"remoteUrl":"https://mcp.notion.example/mcp","installable":true},{"id":"io.pkg/local","name":"local","publisher":"io.pkg","description":"","version":"","url":"","remote":false,"installable":false,"notInstallable":"Runs as a program on the server; not supported"}],"source":{"label":"MCP registry","home":"https://registry.modelcontextprotocol.io"}}"#
    );
    {
        let hits = up.hits.lock().unwrap();
        assert!(
            hits[0].starts_with("get /v0/servers?limit=40 http/1.1"),
            "{}",
            hits[0]
        );
        assert!(hits[0].contains("user-agent: noevia"));
        assert!(hits[0].contains("accept: application/json"));
        assert!(
            !hits[0].contains("cookie"),
            "no credential leaves the server"
        );
    }
    // An hour's cache: the same answer, no second fetch.
    let (_, _, again) = call(addr, "GET", DIRECTORY, &cookie(ADMIN)).await;
    assert_eq!(again, b);
    assert_eq!(up.count.load(Ordering::SeqCst), 1);
    // The query: trimmed, encoded like encodeURIComponent.
    let (s, _, _) = call(
        addr,
        "GET",
        &format!("{DIRECTORY}?q=%20a%2Fb%20c%20"),
        &cookie(MEMBER),
    )
    .await;
    assert_eq!(s, 200);
    assert!(up.hits.lock().unwrap()[1]
        .starts_with("get /v0/servers?limit=40&search=a%2fb%20c http/1.1"));
    // Skills: the listing, filtered by name.
    let (s, _, b) = call(
        addr,
        "GET",
        &format!("{DIRECTORY}?kind=skills&q=CREATOR"),
        &cookie(MEMBER),
    )
    .await;
    assert_eq!(s, 200, "{b}");
    assert_eq!(
        b,
        r#"{"items":[{"id":"skill-creator","name":"Skill creator","publisher":"Anthropic","description":"","version":"","url":"","remote":false}],"source":{"label":"anthropics/skills on GitHub","home":"https://github.com/anthropics/skills"}}"#
    );
    // Starters: each pick resolved against the directory; the ones it lacks drop out.
    let (s, _, b) = call(
        addr,
        "GET",
        &format!("{DIRECTORY}?starters=1"),
        &cookie(MEMBER),
    )
    .await;
    assert_eq!(s, 200, "{b}");
    assert_eq!(
        b,
        r#"{"items":[{"id":"ai.exa/exa","name":"exa","publisher":"ai.exa","description":"","version":"","url":"","remote":true,"remoteUrl":"https://mcp.exa.ai/mcp","installable":true,"why":"Web search and page reading, with no key to manage."},{"id":"com.notion/mcp","name":"Notion","publisher":"com.notion","description":"Synthetic","version":"","url":"","remote":true,"remoteUrl":"https://mcp.notion.example/mcp","installable":true,"why":"Read and write your Notion pages; sign in with Notion."}],"source":{"label":"MCP registry","home":"https://registry.modelcontextprotocol.io"}}"#
    );
    let (s, _, b) = call(
        addr,
        "GET",
        &format!("{DIRECTORY}?kind=skills&starters=1"),
        &cookie(MEMBER),
    )
    .await;
    assert_eq!(s, 200);
    assert!(b.starts_with(r#"{"items":[{"id":"pdf","name":"Pdf","publisher":"Anthropic","description":"","version":"","url":"https://github.com/anthropics/skills/tree/main/skills/pdf","remote":false,"why":"Fill, split"#), "{b}");
    assert!(b.contains(r#""id":"skill-creator""#));
    // A failing, redirecting or oversized directory is the one 502 core answers; a redirect is
    // never followed.
    for q in ["boom", "moved", "huge"] {
        let (s, _, b) = call(addr, "GET", &format!("{DIRECTORY}?q={q}"), &cookie(MEMBER)).await;
        assert_eq!(
            (s, b.as_str()),
            (
                502,
                r#"{"error":"The directory could not be reached right now.","source":{"label":"MCP registry","home":"https://registry.modelcontextprotocol.io"}}"#
            ),
            "{q}"
        );
    }
    assert!(up
        .hits
        .lock()
        .unwrap()
        .iter()
        .all(|h| !h.contains("example.invalid")));
    let _ = stop.send(());
}

#[tokio::test]
async fn an_unreachable_directory_is_the_corpus_502() {
    // contracts/http/corpus 000141: the recorded answer when the registry cannot be reached.
    let d = tempfile::tempdir().unwrap();
    let _db = seed_database(d.path(), None);
    let (addr, stop) = start(d.path(), &[]).await;
    let (s, _, b) = call(addr, "GET", DIRECTORY, &cookie(MEMBER)).await;
    assert_eq!(
        (s, b.as_str()),
        (
            502,
            r#"{"error":"The directory could not be reached right now.","source":{"label":"MCP registry","home":"https://registry.modelcontextprotocol.io"}}"#
        )
    );
    let (s, _, b) = call(
        addr,
        "GET",
        &format!("{DIRECTORY}?kind=skills"),
        &cookie(MEMBER),
    )
    .await;
    assert_eq!(s, 502);
    assert!(b.contains("anthropics/skills on GitHub"));
    let _ = stop.send(());
}

#[test]
fn production_reads_only_the_two_fixed_hosts() {
    let s = noevia_server::plugin_directory_routes::Sources::from_lookup(&|_: &str| None);
    assert_eq!(s.registry, "https://registry.modelcontextprotocol.io");
    assert_eq!(
        s.skills,
        "https://api.github.com/repos/anthropics/skills/contents/skills"
    );
    assert!(!s.loopback_mcp);
}
