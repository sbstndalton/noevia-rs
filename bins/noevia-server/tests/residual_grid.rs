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
                    let facts = r.method() == "POST"
                        && r.uri().path() == "/internal/model-downloads-read/facts"
                        && r.headers()
                            .get("x-noevia-store-token")
                            .is_some_and(|h| h == "synthetic-residual-grid-private-token");
                    let _ = r.into_body().collect().await;
                    let answer = if facts {
                        Response::builder()
                            .status(200)
                            .body(Full::new(Bytes::from_static(br#"{"kind":"disabled"}"#)))
                            .unwrap()
                    } else {
                        Response::builder()
                            .status(599)
                            .header("x-residual-sentinel", "1")
                            .body(Full::new(Bytes::new()))
                            .unwrap()
                    };
                    Ok::<_, std::convert::Infallible>(answer)
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
    env.insert(
        "NOEVIA_PROJECT_STORE_TOKEN",
        "synthetic-residual-grid-private-token".into(),
    );
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
    headers: BTreeMap<String, String>,
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
        let headers = res
            .headers()
            .iter()
            .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap().to_string()))
            .collect();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        Answer {
            status,
            proxied,
            cookies,
            text: String::from_utf8(bytes.to_vec()).unwrap(),
            headers,
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

/// Routes whose 200 body carries a rolling window (`json.days`, one entry per civil day) that ends
/// on the server's current date. The stack recorded in the fixture ran on one day and the replay
/// runs on another, so the window cannot be compared as literal dates (the replay broke at UTC
/// midnight after the recording). Everything else in these bodies is still compared exactly.
const DATED_ROUTES: [&str; 2] = ["/api/usage", "/api/usage/aggregate"];
const MS_PER_DAY: i64 = 86_400_000;

/// Days since 1970-01-01 of a proleptic Gregorian civil date.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The civil date `YYYY-MM-DD` of a day number from `days_from_civil`.
fn civil_from_days(z: i64) -> String {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

fn parse_day(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let (y, m, d) = (
        s[..4].parse::<i64>().ok()?,
        s[5..7].parse::<i64>().ok()?,
        s[8..].parse::<i64>().ok()?,
    );
    let day = days_from_civil(y, m, d);
    // Rejects 2026-02-30 and the like: the date must survive the round trip.
    (civil_from_days(day) == s).then_some(day)
}

/// Rewrites `json.days[*].day` of a dated-route body to its offset from the window's last day
/// (`<day:-364>` ... `<day:0>`), after checking the window is consecutive ascending civil days.
/// Returns the body and the window's last day number. A malformed window is an error, never
/// passed through.
fn rebase_days(body: &Value) -> Result<(Value, i64), String> {
    let mut out = body.clone();
    let days = out
        .pointer_mut("/json/days")
        .and_then(Value::as_array_mut)
        .ok_or("dated route body has no json.days array")?;
    let mut nums = Vec::with_capacity(days.len());
    for (i, entry) in days.iter().enumerate() {
        let n = entry["day"]
            .as_str()
            .and_then(parse_day)
            .ok_or_else(|| format!("days[{i}].day is not a civil date: {}", entry["day"]))?;
        if let Some(prev) = nums.last() {
            if n != prev + 1 {
                return Err(format!(
                    "days[{i}].day is not the day after days[{}]",
                    i - 1
                ));
            }
        }
        nums.push(n);
    }
    let last = *nums.last().ok_or("empty days window")?;
    for (entry, n) in days.iter_mut().zip(&nums) {
        entry["day"] = json!(format!("<day:{}>", n - last));
    }
    Ok((out, last))
}

/// UTC day number of "now".
fn utc_today() -> i64 {
    noevia_server::identity::now_ms().div_euclid(MS_PER_DAY)
}

/// A dated-route comparison: both bodies rebased onto their own last day, and the live window must
/// end on today (UTC; the day before and after the request are both accepted, so a request that
/// straddles midnight does not flake). The recorded body's window end is its recording date and
/// needs no check beyond being a valid consecutive window.
fn compare_dated(
    actual: &Value,
    recorded: &Value,
    before: i64,
    after: i64,
) -> Result<bool, String> {
    let (rebased_actual, last) = rebase_days(actual).map_err(|e| format!("front body: {e}"))?;
    let (rebased_recorded, _) = rebase_days(recorded).map_err(|e| format!("fixture body: {e}"))?;
    if last < before || last > after {
        return Err(format!(
            "front window ends {} but today (UTC) is {}..{}",
            civil_from_days(last),
            civil_from_days(before),
            civil_from_days(after)
        ));
    }
    Ok(canon(&rebased_actual) == canon(&rebased_recorded))
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
        let today_before = utc_today();
        let got = call(target, id, method, path, &h, unsafe_method.then_some("{}")).await;
        let today_after = utc_today();
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
        let dated = got.status == 200
            && c["status"] == 200
            && method == "GET"
            && DATED_ROUTES.contains(&c["route"].as_str().unwrap());
        let body_same = if dated {
            match compare_dated(&body, &c["body"], today_before, today_after) {
                Ok(same) => same,
                Err(e) => {
                    out.failures.push(format!("{id}: {e}"));
                    false
                }
            }
        } else {
            canon(&body) == canon(&c["body"])
        };
        let same = u64::from(got.status) == c["status"].as_u64().unwrap() && body_same;
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

/// Scoped Node origin/main fallback oracle, replayed through the same real identity layer and
/// sentinel as the full grid. Compare stable header values and literal body bytes as well.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn residual_fallback_wire_answers_equal_node() {
    let wire: Value =
        serde_json::from_str(include_str!("fixtures/residual-fallback-node.json")).unwrap();
    let (addr, _up, accts, _data) = booted().await;
    let cases = wire["cases"].as_array().unwrap();
    assert!(cases.len() > 1000, "fallback oracle unexpectedly empty");
    let baseline = fixture();
    let out = replay(addr, cases, &baseline["policy"], &accts, None, false).await;
    assert!(out.failures.is_empty(), "{}", out.failures.join("\n"));
    for c in cases.iter().filter(|c| c["answeredBy"] == "front") {
        let who = c["who"].as_str().unwrap();
        let acct = if who.starts_with("admin") {
            Some(&accts.0)
        } else if who.starts_with("member") {
            Some(&accts.1)
        } else {
            None
        };
        let mut h = vec![("origin", ORIGIN)];
        if let Some(a) = acct {
            h.push(("cookie", a.cookie.as_str()));
            if !who.ends_with("-nocsrf") {
                h.push(("x-csrf-token", a.csrf.as_str()));
            }
        }
        let method = c["method"].as_str().unwrap();
        let got = call(
            addr,
            c["id"].as_str().unwrap(),
            method,
            c["path"].as_str().unwrap(),
            &h,
            UNSAFE.contains(&method).then_some("{}"),
        )
        .await;
        assert_eq!(got.text, c["wireBody"].as_str().unwrap(), "{}", c["id"]);
        for (k, v) in c["headers"].as_object().unwrap() {
            assert_eq!(
                got.headers.get(k).map(String::as_str),
                v.as_str(),
                "{} header {k}",
                c["id"]
            );
        }
    }
}

fn dated_fixture_cases() -> Vec<Value> {
    fixture()["cases"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| {
            c["status"] == 200
                && c["method"] == "GET"
                && DATED_ROUTES.contains(&c["route"].as_str().unwrap())
        })
        .cloned()
        .collect()
}

/// The recorded body with its whole window moved `by` days (what the same stack would answer on
/// another date: only the day labels move).
fn shifted(body: &Value, by: i64) -> Value {
    let mut out = body.clone();
    for entry in out
        .pointer_mut("/json/days")
        .unwrap()
        .as_array_mut()
        .unwrap()
    {
        let n = parse_day(entry["day"].as_str().unwrap()).unwrap();
        entry["day"] = json!(civil_from_days(n + by));
    }
    out
}

#[test]
fn dated_window_civil_dates_round_trip() {
    assert_eq!(days_from_civil(1970, 1, 1), 0);
    assert_eq!(civil_from_days(0), "1970-01-01");
    assert_eq!(civil_from_days(days_from_civil(2026, 11, 5)), "2026-11-05");
    assert_eq!(
        days_from_civil(2026, 11, 6) - days_from_civil(2026, 11, 5),
        1
    );
    assert_eq!(
        days_from_civil(2028, 3, 1) - days_from_civil(2028, 2, 28),
        2
    );
    assert_eq!(parse_day("2026-02-30"), None);
    assert_eq!(parse_day("2026-1-05"), None);
}

#[test]
fn dated_routes_compare_by_offset_not_by_the_day_they_were_recorded() {
    let cases = dated_fixture_cases();
    // /api/usage member and admin, /api/usage/aggregate admin: if one is dropped the date
    // handling is no longer exercised.
    assert_eq!(
        cases.len(),
        3,
        "{:?}",
        cases.iter().map(|c| &c["id"]).collect::<Vec<_>>()
    );
    for c in &cases {
        let recorded = &c["body"];
        let (_, recorded_last) = rebase_days(recorded).unwrap();
        // The recording's own day, the day after it (the 2026-11-06 failure), and far in the
        // future (a leap day inside the window and a year boundary included).
        for by in [0, 1, 400, 5_000, 365_000] {
            let live = shifted(recorded, by);
            assert_eq!(
                compare_dated(&live, recorded, recorded_last + by, recorded_last + by),
                Ok(true),
                "{} shifted {by}",
                c["id"]
            );
            // The window must end today (UTC): the recorded date is stale when by > 0.
            if by > 0 {
                assert!(
                    compare_dated(&live, recorded, recorded_last, recorded_last).is_err(),
                    "{} shifted {by} accepted as an old window",
                    c["id"]
                );
            }
        }
    }
}

#[test]
fn dated_routes_still_compare_everything_else_exactly() {
    let c = &dated_fixture_cases()[0];
    let recorded = &c["body"];
    let (_, last) = rebase_days(recorded).unwrap();
    // A changed total, a changed per-day count and a changed offset all still differ.
    let mut total = recorded.clone();
    total["json"]["allTime"]["replies"] = json!(1);
    assert_eq!(compare_dated(&total, recorded, last, last), Ok(false));
    let mut per_day = recorded.clone();
    per_day["json"]["days"][10]["replies"] = json!(1);
    assert_eq!(compare_dated(&per_day, recorded, last, last), Ok(false));
    let mut short = recorded.clone();
    short["json"]["days"].as_array_mut().unwrap().remove(0);
    assert_eq!(compare_dated(&short, recorded, last, last), Ok(false));
}

#[test]
fn dated_routes_reject_a_malformed_window() {
    let c = &dated_fixture_cases()[0];
    let recorded = &c["body"];
    let (_, last) = rebase_days(recorded).unwrap();
    let mut gap = recorded.clone();
    gap["json"]["days"].as_array_mut().unwrap().remove(100);
    assert!(compare_dated(&gap, recorded, last, last).is_err());
    let mut unordered = recorded.clone();
    unordered["json"]["days"].as_array_mut().unwrap().swap(3, 4);
    assert!(compare_dated(&unordered, recorded, last, last).is_err());
    let mut text = recorded.clone();
    text["json"]["days"][0]["day"] = json!("yesterday");
    assert!(compare_dated(&text, recorded, last, last).is_err());
    assert!(compare_dated(&json!({"kind": "empty"}), recorded, last, last).is_err());
    // A live window ending a day off today fails in either direction.
    assert!(compare_dated(recorded, recorded, last + 1, last + 1).is_err());
    assert!(compare_dated(recorded, recorded, last - 1, last - 1).is_err());
    // A request that straddles midnight may end on either day.
    assert_eq!(compare_dated(recorded, recorded, last - 1, last), Ok(true));
}
