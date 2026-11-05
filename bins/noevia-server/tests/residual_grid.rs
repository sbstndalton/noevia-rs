//! Residual method/path/auth/feature grid for the N2/N3/N4 route families (auth, profile, admin;
//! integrations, connectors, MCP, storage; models, providers, settings).
//!
//! tests/fixtures/residual-methods-node.json is what the shipped stack (this front with Node at
//! contracts/http/front-core.ref behind it) answered for every case: unsupported methods,
//! trailing slashes, malformed identifiers, HEAD/OPTIONS, signed out, signed in without the
//! CSRF token, member against admin routes, and the fresh-install feature state. It is generated
//! by tests/residual-grid-oracle.cjs on two fresh synthetic stacks.
//!
//! Here the same cases go through the in-process front with a sentinel in Node's place. A case the
//! front answers itself must get exactly Node-stack's status and body (volatile bodies: status
//! only). A case that reaches the sentinel is a residual Node fallback: it is counted (and listed in
//! the RESIDUAL_GRID_REPORT file when set), and the fixture holds the exact answer a native
//! replacement must give. Synthetic accounts only.
//!
//! A case with an `intentional` field is a deliberate change from the Node-era answer (it names
//! the issue; today only sbstndalton/noevia#2411, GET /api/instance/). Re-apply those after
//! regenerating the fixture with the oracle.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::{Request, Response};
use noevia_server::config::Config;
use noevia_server::serve::{serve, Limits};
use serde_json::{json, Value};
use server_store::rusqlite::Connection;
use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::path::Path;
use tokio::net::{TcpListener, TcpStream};

const SCHEMA: &str = include_str!("../../../crates/server-store/tests/fixtures/node-schema.sql");
const FIXTURE: &str = include_str!("fixtures/residual-methods-node.json");
const CODE: &str = "synthetic-residual-grid-setup-code";
const ORIGIN: &str = "http://127.0.0.1:18021";
const HOST: &str = "127.0.0.1:18021";
const UNSAFE: [&str; 4] = ["POST", "PUT", "PATCH", "DELETE"];

/// Node's stand-in: any request that reaches it is a residual fallback.
async fn sentinel() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let svc = hyper::service::service_fn(|r: Request<Incoming>| async move {
                    let _ = r.into_body().collect().await;
                    Ok::<_, std::convert::Infallible>(
                        Response::builder()
                            .status(599)
                            .header("x-residual-sentinel", "1")
                            .body(Full::new(Bytes::new()))
                            .unwrap(),
                    )
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(hyper_util::rt::TokioIo::new(stream), svc)
                    .await;
            });
        }
    });
    addr
}

async fn front(up: SocketAddr, data: &Path) -> SocketAddr {
    let c = Connection::open(server_store::db_path(data)).unwrap();
    c.execute_batch(SCHEMA).unwrap();
    c.execute(
        "INSERT INTO settings VALUES('setup_code_hash', ?1)",
        [server_auth::js::digest(CODE)],
    )
    .unwrap();
    // Node writes the instance id at boot (core server/auth.cjs `instance_id`); GET /api/instance
    // is native but reads it, so the sentinel stack needs it seeded the way Node leaves it.
    c.execute(
        "INSERT INTO settings VALUES('instance_id', 'synthetic-residual-grid-instance')",
        [],
    )
    .unwrap();
    drop(c);
    let mut env: HashMap<&str, String> = HashMap::new();
    env.insert("NOEVIA_LEGACY_UPSTREAM", format!("http://{up}"));
    env.insert("UI_PORT", "1".into());
    env.insert("UI_DATA_DIR", data.display().to_string());
    env.insert("NOEVIA_WEB_DIST", data.join("dist").display().to_string());
    env.insert("PUBLIC_ORIGIN", ORIGIN.into());
    // As tools/front/replay-front.sh: the directory sources point at a closed port.
    env.insert("NOEVIA_QA_MCP_REGISTRY", "http://127.0.0.1:9".into());
    env.insert(
        "NOEVIA_QA_SKILLS_DIRECTORY",
        "http://127.0.0.1:9/skills".into(),
    );
    let app = noevia_server::App::new(Config::from_lookup(|k| env.get(k).cloned()).unwrap());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(serve(
        listener,
        app,
        Limits::default(),
        std::future::pending(),
    ));
    addr
}

struct Answer {
    status: u16,
    proxied: bool,
    cookies: Vec<String>,
    text: String,
}

async fn call(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<&str>,
) -> Answer {
    let stream = TcpStream::connect(addr).await.unwrap();
    let (mut send, conn) =
        hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(stream))
            .await
            .unwrap();
    tokio::spawn(conn);
    let mut b = Request::builder()
        .method(method)
        .uri(path)
        .header("host", HOST)
        .header("connection", "close");
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    let payload = body.unwrap_or("").to_string();
    if body.is_some() {
        b = b.header("content-length", payload.len());
    }
    let res = send
        .send_request(b.body(Full::new(Bytes::from(payload)).boxed()).unwrap())
        .await
        .unwrap();
    let status = res.status().as_u16();
    let proxied = res.headers().contains_key("x-residual-sentinel");
    let cookies = res
        .headers()
        .get_all("set-cookie")
        .iter()
        .map(|v| v.to_str().unwrap().split(';').next().unwrap().to_string())
        .collect();
    let text =
        String::from_utf8(res.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap();
    Answer {
        status,
        proxied,
        cookies,
        text,
    }
}

/// The oracle's body shape: empty, JSON, or text.
fn shape(text: &str) -> Value {
    if text.is_empty() {
        return json!({"kind": "empty"});
    }
    match serde_json::from_str::<Value>(text) {
        Ok(v) => json!({"kind": "json", "json": v}),
        Err(_) => json!({"kind": "text", "text": text}),
    }
}

#[derive(Clone)]
struct Account {
    cookie: String,
    csrf: String,
}

fn account(a: &Answer) -> Account {
    assert!(a.cookies.len() >= 2, "{} {}", a.status, a.text);
    let csrf = a
        .cookies
        .iter()
        .find_map(|c| c.strip_prefix("cowork_csrf="))
        .unwrap()
        .to_string();
    Account {
        cookie: a.cookies.join("; "),
        csrf,
    }
}

/// The same steps as residual-grid-oracle.cjs `accounts`.
async fn accounts(addr: SocketAddr) -> (Account, Account) {
    let json_post = [("origin", ORIGIN), ("content-type", "application/json")];
    let setup = json!({"setupCode": CODE, "username": "grid-admin", "displayName": "Grid Admin",
        "password": "residual-grid-Synthetic-pw-1", "diaryEnabled": false})
    .to_string();
    let a = call(
        addr,
        "POST",
        "/api/setup/complete",
        &json_post,
        Some(&setup),
    )
    .await;
    assert_eq!(a.status, 201, "{}", a.text);
    let admin = account(&a);
    let mut h = json_post.to_vec();
    h.push(("cookie", &admin.cookie));
    h.push(("x-csrf-token", &admin.csrf));
    let i = call(
        addr,
        "POST",
        "/api/admin/invitations",
        &h,
        Some(r#"{"role":"member"}"#),
    )
    .await;
    assert_eq!(i.status, 201, "{}", i.text);
    let token = serde_json::from_str::<Value>(&i.text).unwrap()["token"]
        .as_str()
        .unwrap()
        .to_string();
    let accept = json!({"token": token, "username": "grid-member", "displayName": "Grid Member",
        "password": "residual-grid-Synthetic-pw-2", "diaryEnabled": false})
    .to_string();
    let m = call(
        addr,
        "POST",
        "/api/auth/invitations/accept",
        &json_post,
        Some(&accept),
    )
    .await;
    assert_eq!(m.status, 201, "{}", m.text);
    (admin, account(&m))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn residual_grid_native_answers_equal_the_node_stack() {
    let fixture: Value = serde_json::from_str(FIXTURE).unwrap();
    let cases = fixture["cases"].as_array().unwrap();
    assert!(cases.len() > 6000, "grid shrank: {}", cases.len());
    let data = tempfile::tempdir().unwrap();
    let up = sentinel().await;
    let addr = front(up, data.path()).await;
    let (admin, member) = accounts(addr).await;

    let mut mismatches = Vec::new();
    let mut proxied: BTreeMap<String, usize> = BTreeMap::new();
    let mut by_family: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    let mut report = Vec::new();
    for c in cases {
        let (id, method, path, who) = (
            c["id"].as_str().unwrap(),
            c["method"].as_str().unwrap(),
            c["path"].as_str().unwrap(),
            c["who"].as_str().unwrap(),
        );
        let acct = if who.starts_with("admin") {
            Some(&admin)
        } else if who.starts_with("member") {
            Some(&member)
        } else {
            None
        };
        let mut h: Vec<(&str, &str)> = vec![("origin", ORIGIN)];
        if let Some(a) = acct {
            h.push(("cookie", &a.cookie));
            if !who.ends_with("-nocsrf") {
                h.push(("x-csrf-token", &a.csrf));
            }
        }
        let unsafe_method = UNSAFE.contains(&method);
        if unsafe_method {
            h.push(("content-type", "application/json"));
        }
        let got = call(addr, method, path, &h, unsafe_method.then_some("{}")).await;
        let family = c["family"].as_str().unwrap().to_string();
        let entry = by_family.entry(family).or_default();
        if got.proxied {
            entry.1 += 1;
            *proxied
                .entry(format!(
                    "{} {}",
                    c["status"],
                    serde_json::to_string(&c["body"]).unwrap()
                ))
                .or_default() += 1;
            report.push(json!({"id": id, "answeredBy": "node", "node": c["status"]}));
            continue;
        }
        entry.0 += 1;
        let body = shape(&got.text);
        let volatile = c["volatile"].as_bool().unwrap_or(false);
        let same = u64::from(got.status) == c["status"].as_u64().unwrap()
            && (volatile || body == c["body"]);
        report.push(json!({"id": id, "answeredBy": "front", "node": c["status"],
            "front": got.status, "same": same}));
        if !same {
            mismatches.push(format!(
                "{id}: node {} {} / front {} {}",
                c["status"], c["body"], got.status, body
            ));
        }
    }
    if let Ok(out) = std::env::var("RESIDUAL_GRID_REPORT") {
        std::fs::write(
            &out,
            serde_json::to_string_pretty(&json!({"families": by_family, "proxiedAnswers": proxied,
                "mismatches": mismatches, "cases": report}))
            .unwrap(),
        )
        .unwrap();
    }
    eprintln!(
        "residual grid: {} cases; per family (front, node) {by_family:?}; {} native mismatches",
        cases.len(),
        mismatches.len()
    );
    assert!(
        mismatches.is_empty(),
        "{} native answers differ from the Node stack:\n{}",
        mismatches.len(),
        mismatches.join("\n")
    );
}
