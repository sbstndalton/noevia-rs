//! Residual method/path/auth/feature grid for the N2/N3/N4 route families (auth, profile, admin;
//! integrations, connectors, MCP, storage; models, providers, settings).
//!
//! tests/fixtures/residual-methods-node.json is what the shipped stack (this front with Node at
//! contracts/http/front-core.ref behind it) answered for every case: every method on every route,
//! trailing slash and malformed-identifier variants, signed out, member and admin with and without
//! the CSRF token, in the fresh-install feature state. tests/residual-grid-oracle.cjs generates it
//! on two fresh synthetic stacks under the operation policy stored in the fixture (`policy`), which
//! leaves out only reviewed operations that start work or succeed as writes.
//!
//! Here the same cases go through the in-process front with a sentinel in Node's place:
//! - the case set must be exactly what the policy gives for the rust routes of routes.toml in
//!   these families, so an added or dropped route fails;
//! - each case's `answeredBy` (from the reviewed baseline, oracle `classify`) is asserted: a case
//!   that should be native and reaches the sentinel fails, and so does a residual Node case the
//!   front starts answering (move it deliberately by re-classifying);
//! - a native case must get exactly the stack's status and body, with the reviewed
//!   `policy.volatile` fields compared by type (non-empty string, positive number) and numbers by
//!   value; `front-node-facts` cases (routes in `policy.nodeFacts`, whose answer needs runtime
//!   facts Node publishes) are asserted native, their bodies compared only in the real stack.
//!
//! A case with an `intentional` field is a deliberate change from the Node-era answer (it names
//! the issue; today only sbstndalton/noevia#2411, GET /api/instance/); the oracle re-applies it on
//! regeneration. Synthetic accounts only.
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
use noevia_server::routes::{Owner, ROUTES};
use noevia_server::serve::{serve, Limits};
use serde_json::{json, Value};
use server_store::rusqlite::Connection;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};

const SCHEMA: &str = include_str!("../../../crates/server-store/tests/fixtures/node-schema.sql");
const FIXTURE: &str = include_str!("fixtures/residual-methods-node.json");
const CODE: &str = "synthetic-residual-grid-setup-code";
const ORIGIN: &str = "http://127.0.0.1:18021";
const HOST: &str = "127.0.0.1:18021";
const UNSAFE: [&str; 4] = ["POST", "PUT", "PATCH", "DELETE"];
/// Per request: connect, write, response head and body.
const DEADLINE: Duration = Duration::from_secs(15);

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

/// Answers like a native handler would (no sentinel header): the reverse negative control.
async fn fake_native() -> SocketAddr {
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
                    Ok::<_, std::convert::Infallible>(Response::new(Full::new(Bytes::from_static(
                        br#"{"error":"not found"}"#,
                    ))))
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
    // Node also writes the credential-encryption key at boot (core server/secrets.cjs:17-18: 32
    // random bytes, mode 0600); the storage-integration flows read it. A fixed synthetic key.
    let key = data.join("secrets.key");
    std::fs::write(&key, [0x5au8; 32]).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
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

/// One request on its own connection, bounded by [`DEADLINE`]; `id` names the case on failure.
async fn call(
    addr: SocketAddr,
    id: &str,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<&str>,
) -> Answer {
    let exchange = async {
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
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        Answer {
            status,
            proxied,
            cookies,
            text: String::from_utf8(bytes.to_vec()).unwrap(),
        }
    };
    match tokio::time::timeout(DEADLINE, exchange).await {
        Ok(answer) => answer,
        Err(_) => panic!("case {id}: no complete answer within {DEADLINE:?}"),
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

/// JSON as the oracle's JavaScript sees it: one number type (`0.0` and `0` are the same value).
fn canon(v: &Value) -> Value {
    match v {
        Value::Number(n) => json!(n.as_f64().unwrap()),
        Value::Array(items) => Value::Array(items.iter().map(canon).collect()),
        Value::Object(map) => {
            Value::Object(map.iter().map(|(k, x)| (k.clone(), canon(x))).collect())
        }
        other => other.clone(),
    }
}

fn matches_pattern(pattern: &str, at: &str) -> bool {
    let (a, b): (Vec<_>, Vec<_>) = (pattern.split('/').collect(), at.split('/').collect());
    a.len() == b.len() && a.iter().zip(&b).all(|(p, s)| *p == "*" || p == s)
}

/// The oracle's `normalize`: a reviewed volatile field becomes "<volatile:string>" or
/// "<volatile:number>" when it holds a non-empty string or a positive number (anything else stays
/// and so fails the comparison).
fn normalize(v: &Value, patterns: &[String], at: &str) -> Value {
    if patterns.iter().any(|p| matches_pattern(p, at)) {
        if v.as_str().is_some_and(|s| !s.is_empty()) {
            return json!("<volatile:string>");
        }
        if v.as_f64().is_some_and(|n| n > 0.0) {
            return json!("<volatile:number>");
        }
        return v.clone();
    }
    match v {
        Value::Array(items) => Value::Array(
            items
                .iter()
                .enumerate()
                .map(|(i, x)| normalize(x, patterns, &format!("{at}/{i}")))
                .collect(),
        ),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, x)| (k.clone(), normalize(x, patterns, &format!("{at}/{k}"))))
                .collect(),
        ),
        other => other.clone(),
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
        "setup",
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
        "invite",
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
        "accept",
        "POST",
        "/api/auth/invitations/accept",
        &json_post,
        Some(&accept),
    )
    .await;
    assert_eq!(m.status, 201, "{}", m.text);
    (admin, account(&m))
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap().to_string())
        .collect()
}

/// The oracle's `variants`: (shape, concrete path) for one route pattern.
fn variants(pattern: &str) -> Vec<(&'static str, String)> {
    let base = pattern.strip_suffix("{*?}").unwrap_or(pattern);
    let fill = |v: &str| {
        let mut out = String::new();
        let mut rest = base;
        while let Some(start) = rest.find('{') {
            let end = rest[start..].find('}').unwrap() + start;
            out.push_str(&rest[..start]);
            out.push_str(v);
            rest = &rest[end + 1..];
        }
        out.push_str(rest);
        out
    };
    let mut out = vec![
        ("plain", fill("grid-x")),
        ("slash", format!("{}/", fill("grid-x"))),
    ];
    if base.contains('{') {
        out.extend([
            ("malformed", fill("%zz")),
            ("dotted", fill("..")),
            ("encoded-slash", fill("a%2Fb")),
        ]);
    }
    if base.len() != pattern.len() {
        out.push(("suffix", format!("{}/grid-x", fill("grid-x"))));
    }
    for (_, p) in &out {
        assert!(!p.contains(['{', '}']), "template placeholder left in {p}");
    }
    out
}

/// The case ids the fixture's operation policy gives for the rust routes of routes.toml.
fn expected_ids(policy: &Value) -> (BTreeSet<String>, usize) {
    let families = policy["families"].as_object().unwrap();
    let (methods, unsafe_methods, roles) = (
        strings(&policy["methods"]),
        strings(&policy["unsafe"]),
        strings(&policy["roles"]),
    );
    // Each exclusion names its method, route, roles and shapes, and cites where the work starts.
    let start_work: Vec<(String, String, Vec<String>, Vec<String>)> = policy["startWork"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            assert!(
                e["starts"].as_str().is_some_and(|s| !s.is_empty()),
                "startWork entry without a source citation: {e}"
            );
            (
                e["method"].as_str().unwrap().into(),
                e["route"].as_str().unwrap().into(),
                strings(&e["roles"]),
                strings(&e["shapes"]),
            )
        })
        .collect();
    let writes: BTreeSet<String> = strings(&policy["successfulWrites"]).into_iter().collect();
    let in_family = |path: &str| {
        families.values().any(|prefixes| {
            strings(prefixes).iter().any(|x| {
                path == x
                    || path.starts_with(&format!("{x}/"))
                    || path.starts_with(&format!("{x}{{"))
            })
        })
    };
    let mut ids = BTreeSet::new();
    let mut routes = 0;
    for r in ROUTES
        .iter()
        .filter(|r| r.owner == Owner::Rust && !r.catch_all && in_family(r.path))
    {
        routes += 1;
        for (shape, p) in variants(r.path) {
            for m in &methods {
                for who in &roles {
                    if who.ends_with("-nocsrf") && !unsafe_methods.contains(m) {
                        continue;
                    }
                    if start_work.iter().any(|(sm, sr, sroles, sshapes)| {
                        sm == m
                            && sr == r.path
                            && sroles.contains(who)
                            && sshapes.iter().any(|x| x == shape)
                    }) {
                        continue;
                    }
                    let id = format!("{m} {p} {who}");
                    if !writes.contains(&id) {
                        ids.insert(id);
                    }
                }
            }
        }
    }
    (ids, routes)
}

#[derive(Default)]
struct Outcome {
    failures: Vec<String>,
    by_family: BTreeMap<String, (usize, usize)>,
    proxied_answers: BTreeMap<String, usize>,
    report: Vec<Value>,
}

/// Where one case is sent instead of the front (negative controls only).
struct Divert<'a> {
    id: &'a str,
    to: SocketAddr,
}

async fn replay(
    addr: SocketAddr,
    cases: &[Value],
    policy: &Value,
    accts: &(Account, Account),
    divert: Option<Divert<'_>>,
    classify: bool,
) -> Outcome {
    let volatile = policy["volatile"].as_object().unwrap();
    let mut out = Outcome::default();
    for c in cases {
        let (id, method, path, who) = (
            c["id"].as_str().unwrap(),
            c["method"].as_str().unwrap(),
            c["path"].as_str().unwrap(),
            c["who"].as_str().unwrap(),
        );
        let acct = if who.starts_with("admin") {
            Some(&accts.0)
        } else if who.starts_with("member") {
            Some(&accts.1)
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
        let target = match &divert {
            Some(d) if d.id == id => d.to,
            _ => addr,
        };
        let got = call(target, id, method, path, &h, unsafe_method.then_some("{}")).await;
        let expected = c["answeredBy"].as_str();
        // front-node-facts: native, but the answer needs facts Node publishes (policy.nodeFacts);
        // asserted native here, its body compared only in the real stack.
        let facts = expected == Some("front-node-facts");
        let answered_by = match (got.proxied, facts) {
            (true, _) => "node",
            (false, true) => "front-node-facts",
            (false, false) => "front",
        };
        let entry = out
            .by_family
            .entry(c["family"].as_str().unwrap().to_string())
            .or_default();
        if got.proxied {
            entry.1 += 1;
        } else {
            entry.0 += 1;
        }
        if !classify && expected != Some(answered_by) {
            out.failures.push(format!(
                "{id}: expected answeredBy {expected:?}, got {answered_by} ({})",
                if got.proxied {
                    "unexpected proxy to Node"
                } else {
                    "unexpected native answer"
                }
            ));
        }
        if got.proxied {
            *out.proxied_answers
                .entry(format!("{} {}", c["status"], c["body"]))
                .or_default() += 1;
            out.report.push(json!({"id": id, "answeredBy": "node"}));
            continue;
        }
        if facts {
            out.report
                .push(json!({"id": id, "answeredBy": answered_by, "front": got.status}));
            continue;
        }
        let key = c["volatileKey"]
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| format!("{method} {}", c["route"].as_str().unwrap()));
        let patterns: Vec<String> = volatile.get(&key).map(strings).unwrap_or_default();
        let body = normalize(&shape(&got.text), &patterns, "");
        let same = u64::from(got.status) == c["status"].as_u64().unwrap()
            && canon(&body) == canon(&c["body"]);
        out.report
            .push(json!({"id": id, "answeredBy": "front", "front": got.status, "same": same}));
        if !same {
            out.failures.push(format!(
                "{id}: node-stack {} {} / front {} {body}",
                c["status"], c["body"], got.status
            ));
        }
    }
    out
}

fn fixture() -> Value {
    serde_json::from_str(FIXTURE).unwrap()
}

async fn booted() -> (
    SocketAddr,
    SocketAddr,
    (Account, Account),
    tempfile::TempDir,
) {
    let data = tempfile::tempdir().unwrap();
    let up = sentinel().await;
    let addr = front(up, data.path()).await;
    let accts = accounts(addr).await;
    (addr, up, accts, data)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn residual_grid_native_answers_equal_the_node_stack() {
    let fixture = fixture();
    let cases = fixture["cases"].as_array().unwrap();
    let policy = &fixture["policy"];
    let (expected, routes) = expected_ids(policy);
    let actual: BTreeSet<String> = cases
        .iter()
        .map(|c| c["id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(actual.len(), cases.len(), "duplicate case ids");
    let missing: Vec<_> = expected.difference(&actual).take(20).collect();
    let extra: Vec<_> = actual.difference(&expected).take(20).collect();
    assert!(
        missing.is_empty() && extra.is_empty(),
        "fixture case set differs from routes.toml under the policy ({routes} routes): \
         missing {missing:?}, extra {extra:?}; regenerate with residual-grid-oracle.cjs"
    );
    let classify = std::env::var("RESIDUAL_GRID_CLASSIFY").is_ok_and(|v| v == "1");
    let (addr, _up, accts, _data) = booted().await;
    let out = replay(addr, cases, policy, &accts, None, classify).await;
    if let Ok(path) = std::env::var("RESIDUAL_GRID_REPORT") {
        std::fs::write(
            &path,
            serde_json::to_string_pretty(&json!({"families": out.by_family,
                "proxiedAnswers": out.proxied_answers, "failures": out.failures, "cases": out.report}))
            .unwrap(),
        )
        .unwrap();
    }
    eprintln!(
        "residual grid: {} cases over {routes} routes; per family (front, node) {:?}; {} failures",
        cases.len(),
        out.by_family,
        out.failures.len()
    );
    assert!(
        out.failures.is_empty(),
        "{} grid failures:\n{}",
        out.failures.len(),
        out.failures.join("\n")
    );
}

/// The grid catches a native case that regresses to Node and a Node case answered natively.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn negative_controls_fail_the_grid() {
    if std::env::var("RESIDUAL_GRID_CLASSIFY").is_ok_and(|v| v == "1") {
        return;
    }
    let fixture = fixture();
    let cases = fixture["cases"].as_array().unwrap();
    let native = "GET /api/instance anon";
    let pick = |id: &str| cases.iter().find(|c| c["id"] == id).unwrap().clone();
    assert_eq!(pick(native)["answeredBy"], "front");
    let node = cases
        .iter()
        .find(|c| c["answeredBy"] == "node" && c["who"] == "anon")
        .unwrap()
        .clone();
    let node_id = node["id"].as_str().unwrap().to_string();
    let subset = vec![pick(native), node];
    let (addr, up, accts, _data) = booted().await;

    let clean = replay(addr, &subset, &fixture["policy"], &accts, None, false).await;
    assert!(clean.failures.is_empty(), "{:?}", clean.failures);

    let forced = replay(
        addr,
        &subset,
        &fixture["policy"],
        &accts,
        Some(Divert { id: native, to: up }),
        false,
    )
    .await;
    assert_eq!(forced.failures.len(), 1, "{:?}", forced.failures);
    assert!(
        forced.failures[0].starts_with(native)
            && forced.failures[0].contains("unexpected proxy to Node"),
        "{:?}",
        forced.failures
    );

    let fake = fake_native().await;
    let moved = replay(
        addr,
        &subset,
        &fixture["policy"],
        &accts,
        Some(Divert {
            id: &node_id,
            to: fake,
        }),
        false,
    )
    .await;
    assert!(
        moved
            .failures
            .iter()
            .any(|f| f.starts_with(&node_id) && f.contains("unexpected native answer")),
        "{:?}",
        moved.failures
    );
}
