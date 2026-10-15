//! Native connected-storage list/read against the actual Node route (noevia#1368).
//!
//! One synthetic loopback DAV + S3 server answers both sides. Each case runs through
//! `storage_browse_routes::serve` (the real gate, row projection, endpoint policy, engine and owned
//! child transport) and through core `routes/storage.cjs` `createStorageRoutes` with the real
//! `storage-client.cjs`, `http.cjs` and `ssrf.cjs` (a fixed seam supplies only the saved connection
//! and the caller's role, as `authService.getStorage` and the gate would). Response status and
//! body bytes must be equal, and so must the outbound requests the synthetic server saw (method,
//! target and the protocol headers; SigV4's date-bound `authorization`/`x-amz-date` excepted).
//!
//! Needs `NOEVIA_STORAGE_ORACLE_CORE` (a core checkout with `npm ci` and the dav-parse.wasm the
//! pinned lock names) and Linux (the child sets Linux limits before READY). Otherwise the test
//! prints why it is skipped and records it in `M4_ORACLE_SKIP_LOG`.
#![cfg(feature = "rust-storage-browse")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
use axum::{body::Body, http::Request};
use http_body_util::BodyExt;
use noevia_server::{config::Config, serve::Conn, storage_browse_routes, App};
use serde_json::{json, Value};
use server_store::rusqlite::Connection;
use sha2::{Digest, Sha256};
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    time::Duration,
};

const ADMIN: &str = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
const MEMBER: &str = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";
const ORIGIN: &str = "http://127.0.0.1:8021";

fn skipped(why: &str) {
    eprintln!("storage browse differential skipped: {why}");
    if let Some(log) = std::env::var_os("M4_ORACLE_SKIP_LOG") {
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)
        {
            let _ = writeln!(f, "storage_browse::native_matches_node ({why})");
        }
    }
}

/// One request the synthetic server saw: method, target and the compared headers.
type Seen = (String, String, Vec<(String, String)>);

const COMPARED: &[&str] = &[
    "authorization",
    "depth",
    "content-type",
    "x-amz-content-sha256",
    "content-length",
];

fn multistatus(base: &str, entries: &[(&str, Option<&str>)]) -> String {
    let mut out = String::from("<?xml version=\"1.0\"?><d:multistatus xmlns:d=\"DAV:\">");
    out.push_str(&format!("<d:response><d:href>{base}</d:href><d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop></d:propstat></d:response>"));
    for (href, size) in entries {
        match size {
            None => out.push_str(&format!("<d:response><d:href>{href}</d:href><d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop></d:propstat></d:response>")),
            Some(size) => out.push_str(&format!("<d:response><d:href>{href}</d:href><d:propstat><d:prop><d:getcontentlength>{size}</d:getcontentlength><d:resourcetype/></d:prop></d:propstat></d:response>")),
        }
    }
    out.push_str("</d:multistatus>");
    out
}

fn s3_page(prefix: &str, dirs: &[&str], files: &[(&str, &str)], next: Option<&str>) -> String {
    let mut out = String::from("<?xml version=\"1.0\"?><ListBucketResult>");
    out.push_str(&format!("<IsTruncated>{}</IsTruncated>", next.is_some()));
    if let Some(next) = next {
        out.push_str(&format!(
            "<NextContinuationToken>{next}</NextContinuationToken>"
        ));
    }
    for d in dirs {
        out.push_str(&format!(
            "<CommonPrefixes><Prefix>{prefix}{d}/</Prefix></CommonPrefixes>"
        ));
    }
    for (f, size) in files {
        out.push_str(&format!(
            "<Contents><Key>{prefix}{f}</Key><Size>{size}</Size></Contents>"
        ));
    }
    out.push_str("</ListBucketResult>");
    out
}

/// The synthetic storage answer for one request: status, extra headers, body.
fn answer(method: &str, target: &str) -> (u16, String, Vec<u8>) {
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let query: Vec<(String, String)> = url::form_urlencoded::parse(query.as_bytes())
        .into_owned()
        .collect();
    let q = |k: &str| {
        query
            .iter()
            .find(|(key, _)| key == k)
            .map(|(_, v)| v.as_str())
    };
    let ok = |body: String| (207, String::new(), body.into_bytes());
    match (method, path) {
        ("PROPFIND", "/dav/") => ok(multistatus(
            "/dav/",
            &[
                ("/dav/docs/", None),
                ("/dav/Zeta.md", Some("12")),
                ("/dav/%C3%A9t%C3%A9.txt", Some("007")),
                ("/dav/a%20b.json", Some("9007199254740993")),
                ("https://elsewhere.invalid/dav/skip.txt", Some("1")),
            ],
        )),
        ("PROPFIND", "/dav/docs/") => ok(multistatus(
            "/dav/docs/",
            &[("/dav/docs/sub/", None), ("/dav/docs/a.md", Some("10"))],
        )),
        ("PROPFIND", "/dav/docs/sub/") => ok(multistatus("/dav/docs/sub/", &[])),
        ("PROPFIND", "/dav/missing/") => (404, String::new(), Vec::new()),
        ("PROPFIND", "/dav/moved/") => (
            301,
            "Location: http://127.0.0.1:9/elsewhere/\r\n".into(),
            Vec::new(),
        ),
        ("PROPFIND", "/dav/garbage/") => (207, String::new(), b"<not-xml".to_vec()),
        ("PROPFIND", "/dav/huge/") => {
            let mut body =
                multistatus("/dav/huge/", &[("/dav/huge/x.txt", Some("1"))]).into_bytes();
            body.extend(std::iter::repeat_n(b' ', 4 * 1024 * 1024 + 10));
            (207, String::new(), body)
        }
        ("GET", "/dav/docs/a.md") => (200, String::new(), "hello 😀\n".as_bytes().to_vec()),
        ("GET", "/dav/docs/latin.txt") => (200, String::new(), vec![0x63, 0x61, 0x66, 0xe9, 0x0a]),
        ("GET", "/dav/docs/huge.txt") => (200, String::new(), vec![b'a'; 900_000]),
        ("GET", "/dav/docs/err.md") => (500, String::new(), Vec::new()),
        // A server that never sends its headers within the 15 s listing timeout.
        ("PROPFIND", "/dav/stalled/") => {
            std::thread::sleep(Duration::from_secs(17));
            (404, String::new(), Vec::new())
        }
        ("GET", "/s3/bucket") if q("list-type") == Some("2") => {
            let prefix = q("prefix").unwrap_or("");
            match (prefix, q("continuation-token")) {
                ("diary/", None) => (
                    200,
                    String::new(),
                    s3_page("diary/", &["docs"], &[("b.md", "3")], Some("token-2")).into_bytes(),
                ),
                ("diary/", Some("token-2")) => (
                    200,
                    String::new(),
                    s3_page("diary/", &["Alpha"], &[("A.txt", "5")], None).into_bytes(),
                ),
                ("diary/docs/", None) => (
                    200,
                    String::new(),
                    s3_page("diary/docs/", &[], &[("z.txt", "7")], None).into_bytes(),
                ),
                _ => (403, String::new(), Vec::new()),
            }
        }
        ("GET", "/s3/bucket/diary/docs/z.txt") => (200, String::new(), b"s3 text\n".to_vec()),
        _ => (404, String::new(), Vec::new()),
    }
}

fn handle_conn(mut stream: TcpStream, seen: &Mutex<Vec<Seen>>) {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut raw = Vec::new();
    let mut buf = [0u8; 4096];
    let head_end = loop {
        if let Some(at) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
            break at;
        }
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => raw.extend_from_slice(&buf[..n]),
        }
    };
    let head = String::from_utf8_lossy(&raw[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split(' ');
    let method = parts.next().unwrap_or("").to_owned();
    let target = parts.next().unwrap_or("").to_owned();
    let mut headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_owned()))
        .filter(|(k, _)| COMPARED.contains(&k.as_str()))
        .filter(|(k, v)| !(k == "authorization" && v.starts_with("AWS4-HMAC-SHA256")))
        .collect();
    headers.sort();
    seen.lock()
        .unwrap()
        .push((method.clone(), target.clone(), headers));
    if target == "/dav/docs/slow.md" {
        // Headers at once, then the body stalls past the 20 s read timeout.
        let _ = stream.write_all(
            b"HTTP/1.1 200 Synthetic\r\nContent-Length: 10\r\nConnection: close\r\n\r\nhalf",
        );
        let _ = stream.flush();
        std::thread::sleep(Duration::from_secs(23));
        return;
    }
    let (status, extra, body) = answer(&method, &target);
    let mut out = format!(
        "HTTP/1.1 {status} Synthetic\r\nContent-Length: {}\r\nConnection: close\r\n{extra}\r\n",
        body.len()
    )
    .into_bytes();
    out.extend_from_slice(&body);
    let _ = stream.write_all(&out);
    let _ = stream.flush();
}

fn synthetic_storage() -> (u16, Arc<Mutex<Vec<Seen>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let log = Arc::clone(&log);
            std::thread::spawn(move || handle_conn(stream, &log));
        }
    });
    (port, seen)
}

struct Case {
    name: &'static str,
    role: &'static str,
    members: String,
    connection: Value,
    method: &'static str,
    path: String,
    body: Option<String>,
}

fn cases(port: u16) -> Vec<Case> {
    let dav = |kind: &str| json!({"kind":kind,"baseUrl":format!("http://127.0.0.1:{port}/dav"),"bucket":"","region":"us-east-1","username":"synthetic","secret":"synthetic-secret","corpusRoot":"Diary"});
    let s3 = json!({"kind":"s3","baseUrl":format!("http://127.0.0.1:{port}/s3"),"bucket":"bucket","region":"us-east-1","username":"AKIDSYNTHETIC","secret":"synthetic-s3-secret","corpusRoot":"diary"});
    let local = json!({"kind":"local","baseUrl":"","bucket":"","region":"us-east-1","username":"","secret":"","corpusRoot":""});
    let origin = format!("http://127.0.0.1:{port}");
    let list = |name, path: &str, connection: Value| Case {
        name,
        role: "admin",
        members: String::new(),
        connection,
        method: "GET",
        path: format!("/api/integrations/storage/files{path}"),
        body: None,
    };
    let read = |name, body: &str, connection: Value| Case {
        name,
        role: "admin",
        members: String::new(),
        connection,
        method: "POST",
        path: "/api/integrations/storage/file".into(),
        body: Some(body.into()),
    };
    let mut out = vec![
        list("dav root", "", dav("webdav")),
        list("nextcloud root", "", dav("nextcloud")),
        list("dav dir", "/docs", dav("webdav")),
        list("dav encoded slash", "/docs%2Fsub", dav("webdav")),
        list("dav traversal", "/..%2Fetc", dav("webdav")),
        list("dav malformed escape", "/%E0%A4", dav("webdav")),
        list("dav 404", "/missing", dav("webdav")),
        list("dav redirect", "/moved", dav("webdav")),
        list("dav unreadable", "/garbage", dav("webdav")),
        list("dav over cap", "/huge", dav("webdav")),
        list("dav stalled headers", "/stalled", dav("webdav")),
        list(
            "dav unreachable",
            "",
            json!({"kind":"webdav","baseUrl":"http://127.0.0.1:1/dav","bucket":"","region":"us-east-1","username":"","secret":"","corpusRoot":""}),
        ),
        list("s3 paged root", "", s3.clone()),
        list("s3 dir", "/docs", s3.clone()),
        list("local list", "", local.clone()),
        read("dav read", r#"{"path":"docs/a.md"}"#, dav("webdav")),
        read(
            "dav latin1 bytes",
            r#"{"path":"docs/latin.txt"}"#,
            dav("webdav"),
        ),
        read(
            "dav capped preview",
            r#"{"path":"docs/huge.txt"}"#,
            dav("webdav"),
        ),
        read("dav 500", r#"{"path":"docs/err.md"}"#, dav("webdav")),
        read(
            "dav body stall",
            r#"{"path":"docs/slow.md"}"#,
            dav("webdav"),
        ),
        read("unsupported type", r#"{"path":"x.bin"}"#, dav("webdav")),
        read("invalid path", r#"{"path":"../a.md"}"#, dav("webdav")),
        read("empty body", "", dav("webdav")),
        read("invalid json", "{", dav("webdav")),
        read("array body", "[]", dav("webdav")),
        read("s3 read", r#"{"path":"docs/z.txt"}"#, s3.clone()),
        read("local read", r#"{"path":"a.md"}"#, local),
    ];
    let mut member_refused = list("member unapproved origin", "", dav("webdav"));
    member_refused.role = "member";
    let mut member_ok = list("member approved origin", "", dav("webdav"));
    member_ok.role = "member";
    member_ok.members = format!(" {origin} ,http://other.invalid");
    let mut userinfo = list("userinfo endpoint", "", dav("webdav"));
    userinfo.connection["baseUrl"] = json!(format!("http://u:p@127.0.0.1:{port}/dav"));
    out.extend([member_refused, member_ok, userinfo]);
    out
}

const SEAM: &str = r#"'use strict';
const fs=require('node:fs');const core=process.argv[1];const {Readable}=require('node:stream');
const {createStorageRoutes}=require(core+'/server/routes/storage.cjs');
const http=require(core+'/server/http.cjs');
const storageClient=require(core+'/server/storage-client.cjs');
const {createEndpointApproved}=require(core+'/server/ssrf.cjs');
(async()=>{const cases=JSON.parse(fs.readFileSync(0,'utf8'));const out=[];
for(const c of cases){
  const routes=createStorageRoutes({json:http.json,readJson:http.readJson,authService:{getStorage:()=>({...c.connection})},storageClient,endpointApproved:createEndpointApproved({env:{MEMBER_OUTBOUND_ORIGINS:c.members}}),fetch:(...a)=>globalThis.fetch(...a),crypto:require('node:crypto')});
  const req=Readable.from(c.body===null?[]:[Buffer.from(c.body,'utf8')]);req.method=c.method;req.headers={'content-type':'application/json'};
  let status=null,body=null;const res={setHeader(){},getHeader(){},writeHead(s){status=s;},end(b){body=String(b);}};
  try{const handled=await routes(req,res,{path:c.path,authn:{user:{id:'synthetic-owner',role:c.role}}});if(!handled){status=0;body='unhandled';}}
  catch(e){const f=http.errorResponse(e);status=f.status;body=JSON.stringify(f.body);}
  out.push({status,body});}
process.stdout.write(JSON.stringify(out));})().catch(()=>{process.exitCode=1;});
"#;

fn node_answers(core: &str, cases: &[Case]) -> Vec<(u16, String)> {
    let input = json!(cases
        .iter()
        .map(|c| json!({"connection":c.connection,"members":c.members,"method":c.method,"path":c.path,"body":c.body,"role":c.role}))
        .collect::<Vec<_>>());
    let mut child =
        Command::new(std::env::var("NOEVIA_STORAGE_ORACLE_NODE").unwrap_or_else(|_| "node".into()))
            .args(["-e", SEAM, core])
            .env("NODE_PATH", format!("{core}/server/node_modules"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("node");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.to_string().as_bytes())
        .unwrap();
    // Bounded: a stalled oracle fails the test instead of hanging it.
    let (tx, rx) = std::sync::mpsc::channel();
    let mut stdout = child.stdout.take().unwrap();
    std::thread::spawn(move || {
        let mut out = Vec::new();
        let _ = (&mut stdout).take(64 * 1024 * 1024).read_to_end(&mut out);
        let _ = tx.send(out);
    });
    let out = match rx.recv_timeout(Duration::from_secs(240)) {
        Ok(out) => out,
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            panic!("Node oracle timed out");
        }
    };
    assert!(child.wait().unwrap().success(), "Node oracle failed");
    let rows: Vec<Value> = serde_json::from_slice(&out).unwrap();
    rows.iter()
        .map(|r| {
            (
                u16::try_from(r["status"].as_u64().unwrap()).unwrap(),
                r["body"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

fn hash(v: &str) -> String {
    Sha256::digest(v.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn native_app(dir: &std::path::Path) -> Arc<App> {
    let db = Connection::open(dir.join("cowork.db")).unwrap();
    db.execute_batch(include_str!(
        "../../../crates/server-store/tests/fixtures/node-schema.sql"
    ))
    .unwrap();
    let now = noevia_server::identity::now_ms();
    for (id, role) in [(ADMIN, "admin"), (MEMBER, "member")] {
        db.execute("INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,created_at,updated_at) VALUES(?1,?1,?1,'Synthetic',?2,'x',?1,1,1)",[id, role]).unwrap();
        db.execute(
            "INSERT INTO sessions VALUES(?1,?2,?3,?4,?4,?5,'ua','127.0.0.1')",
            (
                hash(&format!("session-{role}")),
                id,
                hash("csrf"),
                now,
                now + 86_400_000,
            ),
        )
        .unwrap();
    }
    std::fs::write(dir.join("secrets.key"), vec![7; 32]).unwrap();
    let data = dir.display().to_string();
    let mut app = App::new(
        Config::from_lookup(|k| match k {
            "NOEVIA_LEGACY_UPSTREAM" => Some("http://127.0.0.1:9".into()),
            "NOEVIA_RUST_AUTH" | "NOEVIA_RUST_STORAGE_BROWSE" => Some("1".into()),
            "UI_DATA_DIR" => Some(data.clone()),
            "PUBLIC_ORIGIN" => Some(ORIGIN.into()),
            _ => None,
        })
        .unwrap(),
    );
    // The test harness is not noevia-server: point the owned child at the real binary.
    Arc::get_mut(&mut app).unwrap().storage_child =
        Some(env!("CARGO_BIN_EXE_noevia-server").into());
    app
}

async fn native_answer(app: &Arc<App>, dir: &std::path::Path, case: &Case) -> (u16, String) {
    let owner = if case.role == "admin" { ADMIN } else { MEMBER };
    let db = Connection::open(dir.join("cowork.db")).unwrap();
    db.execute("DELETE FROM storage_connections", []).unwrap();
    let c = &case.connection;
    let s = |k: &str| c[k].as_str().unwrap().to_owned();
    db.execute(
        "INSERT INTO storage_connections(user_id,kind,base_url,bucket,region,username,secret,corpus_root,updated_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,1)",
        [owner.to_owned(), s("kind"), s("baseUrl"), s("bucket"), s("region"), s("username"), s("secret"), s("corpusRoot")],
    )
    .unwrap();
    std::env::set_var("MEMBER_OUTBOUND_ORIGINS", &case.members);
    let mut req = Request::builder()
        .method(case.method)
        .uri(&case.path)
        .header(
            "cookie",
            format!("cowork_session=session-{}; cowork_csrf=csrf", case.role),
        )
        .header("x-csrf-token", "csrf")
        .header("origin", ORIGIN)
        .header("content-type", "application/json");
    if case.method == "GET" {
        req = req.header("accept", "application/json");
    }
    let body = case
        .body
        .clone()
        .map(Body::from)
        .unwrap_or_else(Body::empty);
    let conn = Conn {
        peer: "127.0.0.1:1".parse().unwrap(),
        local: Some("127.0.0.1:8021".parse().unwrap()),
    };
    let path = case.path.split('?').next().unwrap().to_owned();
    let res = storage_browse_routes::serve(app, conn, req.body(body).unwrap(), path).await;
    let status = res.status().as_u16();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_matches_node() {
    let Ok(core) = std::env::var("NOEVIA_STORAGE_ORACLE_CORE") else {
        skipped("NOEVIA_STORAGE_ORACLE_CORE unset");
        return;
    };
    if !cfg!(target_os = "linux") {
        skipped("the owned child runs only on Linux");
        return;
    }
    let (port, seen) = synthetic_storage();
    let cases = cases(port);
    let node = node_answers(&core, &cases);
    let node_seen: Vec<Seen> = std::mem::take(&mut *seen.lock().unwrap());
    let dir = tempfile::tempdir().unwrap();
    let app = native_app(dir.path());
    let mut native = Vec::new();
    let mut native_seen = Vec::new();
    let mut node_index = 0;
    let mut failures = Vec::new();
    for (case, expected) in cases.iter().zip(&node) {
        let got = native_answer(&app, dir.path(), case).await;
        let requests: Vec<Seen> = std::mem::take(&mut *seen.lock().unwrap());
        native_seen.push(requests);
        if &got != expected {
            failures.push(format!(
                "{}: native {} {} vs node {} {}",
                case.name,
                got.0,
                got.1.chars().take(300).collect::<String>(),
                expected.0,
                expected.1.chars().take(300).collect::<String>()
            ));
        }
        native.push(got);
        node_index += 1;
    }
    assert_eq!(node_index, cases.len());
    // The same outbound requests, in the same order, across the whole matrix.
    let flat: Vec<Seen> = native_seen.into_iter().flatten().collect();
    if flat != node_seen {
        failures.push(format!(
            "outbound requests differ:\nnative {flat:#?}\nnode {node_seen:#?}"
        ));
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    // Sanity: the matrix exercised real outbound reads, listings and both refusals.
    assert!(native
        .iter()
        .any(|(s, b)| *s == 200 && b.starts_with("{\"entries\":[{")));
    assert!(native
        .iter()
        .any(|(s, b)| *s == 200 && b.contains("\"truncated\":true")));
    assert!(node_seen.iter().any(|(m, _, _)| m == "PROPFIND"));
    assert!(node_seen
        .iter()
        .any(|(_, t, _)| t.contains("continuation-token=token-2")));
}

/// One `POST /api/integrations/storage/file` through the real gate with the given `x-csrf-token`
/// (None: no header). The session cookie and its stored CSRF hash are valid.
async fn file_post_with_csrf(app: &Arc<App>, csrf: Option<&str>) -> (u16, String) {
    let mut req = Request::builder()
        .method("POST")
        .uri("/api/integrations/storage/file")
        .header("cookie", "cowork_session=session-admin; cowork_csrf=csrf")
        .header("origin", ORIGIN)
        .header("content-type", "application/json");
    if let Some(token) = csrf {
        req = req.header("x-csrf-token", token);
    }
    let conn = Conn {
        peer: "127.0.0.1:1".parse().unwrap(),
        local: Some("127.0.0.1:8021".parse().unwrap()),
    };
    let res = storage_browse_routes::serve(
        app,
        conn,
        req.body(Body::from(r#"{"path":"docs/a.md"}"#)).unwrap(),
        "/api/integrations/storage/file".into(),
    )
    .await;
    let status = res.status().as_u16();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

/// Core index.cjs answers a state-changing request with a missing or wrong CSRF token with
/// `403 {"error":"invalid CSRF token"}` before the route runs (#1835). The gate refuses before any
/// connection read or outbound work, so this needs neither the oracle nor Linux.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn file_post_with_missing_or_wrong_csrf_is_nodes_403() {
    let (_port, seen) = synthetic_storage();
    let dir = tempfile::tempdir().unwrap();
    let app = native_app(dir.path());
    let expected = (403, r#"{"error":"invalid CSRF token"}"#.to_string());
    assert_eq!(file_post_with_csrf(&app, None).await, expected, "missing");
    assert_eq!(file_post_with_csrf(&app, Some("")).await, expected, "empty");
    assert_eq!(
        file_post_with_csrf(&app, Some("not-the-token")).await,
        expected,
        "wrong"
    );
    assert!(seen.lock().unwrap().is_empty(), "no outbound request");
    // Control: the right token passes the gate (the body is then refused by the route, not by CSRF).
    let (status, body) = file_post_with_csrf(&app, Some("csrf")).await;
    assert_ne!(status, 403, "{body}");
}

/// core routes/storage.cjs with core's own ownership selectors (features.cjs
/// `rustStorageReadsOwned`, `rustStorageBrowseOwned`) over the given environment; each row is one
/// request against a saved local connection, answered `[status, body]`.
/// Method, path, body, Node environment, expected refusal body (None: Node answers).
type RefusalCase<'a> = (
    &'a str,
    &'a str,
    Option<&'a str>,
    &'a Value,
    Option<&'a str>,
);

const REFUSAL_SEAM: &str = r#"'use strict';
const fs=require('node:fs');const core=process.argv[1];const {Readable}=require('node:stream');
const {createStorageRoutes}=require(core+'/server/routes/storage.cjs');
const http=require(core+'/server/http.cjs');
const features=require(core+'/server/features.cjs');
(async()=>{const cases=JSON.parse(fs.readFileSync(0,'utf8'));const out=[];
for(const c of cases){
  const routes=createStorageRoutes({json:http.json,readJson:http.readJson,authService:{getStorage:()=>({kind:'local',baseUrl:'',bucket:'',region:'us-east-1',username:'',corpusRoot:''})},storageClient:require(core+'/server/storage-client.cjs'),endpointApproved:async()=>false,fetch:()=>{throw new Error('no outbound request');},crypto:require('node:crypto'),
    storageReadsOwned:()=>features.rustStorageReadsOwned(c.env),storageBrowseOwned:()=>features.rustStorageBrowseOwned(c.env)});
  const req=Readable.from(c.body===null?[]:[Buffer.from(c.body,'utf8')]);req.method=c.method;req.headers={'content-type':'application/json'};
  let status=null,body=null;const res={setHeader(){},getHeader(){},writeHead(s){status=s;},end(b){body=String(b);}};
  try{const handled=await routes(req,res,{path:c.path,authn:{user:{id:'synthetic-owner',role:'admin'}}});if(!handled){status=0;body='unhandled';}}
  catch(e){const f=http.errorResponse(e);status=f.status;body=JSON.stringify(f.body);}
  out.push([status,body]);}
process.stdout.write(JSON.stringify(out));})().catch((e)=>{console.error(e);process.exitCode=1;});
"#;

/// Negative control for the native list/read and metadata (noevia#1368): once the supervisor has
/// confirmed the front (NOEVIA_FRONT=rust, NOEVIA_RUST_AUTH_CONFIRMED and the slice's
/// *_CONFIRMED), the actual core route must refuse with 503 and its owned code, so only the front
/// can answer them. The read (`POST /api/integrations/storage/file`) has no corpus exchange; this
/// is its refusal proof. An unconfirmed switch must not refuse (control of the control).
#[test]
fn confirmed_node_refuses_the_storage_routes_the_front_owns() {
    let Ok(core) = std::env::var("NOEVIA_STORAGE_ORACLE_CORE") else {
        skipped("NOEVIA_STORAGE_ORACLE_CORE unset (refusal control)");
        return;
    };
    let auth =
        json!({"NOEVIA_FRONT":"rust","NOEVIA_RUST_AUTH":"1","NOEVIA_RUST_AUTH_CONFIRMED":"1"});
    let with = |extra: Value| {
        let mut env = auth.clone();
        env.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        env
    };
    let browse =
        with(json!({"NOEVIA_RUST_STORAGE_BROWSE":"1","NOEVIA_RUST_STORAGE_BROWSE_CONFIRMED":"1"}));
    let reads =
        with(json!({"NOEVIA_RUST_STORAGE_READS":"1","NOEVIA_RUST_STORAGE_READS_CONFIRMED":"1"}));
    let unconfirmed =
        with(json!({"NOEVIA_RUST_STORAGE_BROWSE":"1","NOEVIA_RUST_STORAGE_READS":"1"}));
    let browse_refusal = r#"{"error":"Storage browsing is owned by the Rust front.","code":"RUST_STORAGE_BROWSE_OWNED"}"#;
    let reads_refusal = r#"{"error":"Storage metadata reads are owned by the Rust front.","code":"RUST_STORAGE_READS_OWNED"}"#;
    let read_body = Some(r#"{"path":"docs/a.md"}"#);
    let cases: Vec<RefusalCase> = vec![
        (
            "GET",
            "/api/integrations/storage/files",
            None,
            &browse,
            Some(browse_refusal),
        ),
        (
            "GET",
            "/api/integrations/storage/files/docs/sub",
            None,
            &browse,
            Some(browse_refusal),
        ),
        (
            "POST",
            "/api/integrations/storage/file",
            read_body,
            &browse,
            Some(browse_refusal),
        ),
        (
            "GET",
            "/api/integrations/storage",
            None,
            &reads,
            Some(reads_refusal),
        ),
        // Each slice is confirmed on its own: the other slice's routes stay Node's.
        ("GET", "/api/integrations/storage", None, &browse, None),
        ("GET", "/api/integrations/storage/files", None, &reads, None),
        (
            "POST",
            "/api/integrations/storage/file",
            read_body,
            &reads,
            None,
        ),
        // Switch without the supervisor's confirmation: Node answers.
        (
            "GET",
            "/api/integrations/storage/files",
            None,
            &unconfirmed,
            None,
        ),
        (
            "POST",
            "/api/integrations/storage/file",
            read_body,
            &unconfirmed,
            None,
        ),
        ("GET", "/api/integrations/storage", None, &unconfirmed, None),
    ];
    let input = json!(cases
        .iter()
        .map(|(m, p, b, env, _)| json!({"method":m,"path":p,"body":b,"env":env}))
        .collect::<Vec<_>>());
    let mut child =
        Command::new(std::env::var("NOEVIA_STORAGE_ORACLE_NODE").unwrap_or_else(|_| "node".into()))
            .args(["-e", REFUSAL_SEAM, &core])
            .env("NODE_PATH", format!("{core}/server/node_modules"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("node");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.to_string().as_bytes())
        .unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let mut stdout = child.stdout.take().unwrap();
    std::thread::spawn(move || {
        let mut out = Vec::new();
        let _ = (&mut stdout).take(1024 * 1024).read_to_end(&mut out);
        let _ = tx.send(out);
    });
    let out = rx
        .recv_timeout(Duration::from_secs(120))
        .unwrap_or_else(|_| {
            let _ = child.kill();
            let _ = child.wait();
            panic!("Node refusal oracle timed out");
        });
    assert!(
        child.wait().unwrap().success(),
        "Node refusal oracle failed"
    );
    let rows: Vec<(u16, String)> = serde_json::from_slice(&out).unwrap();
    assert_eq!(rows.len(), cases.len());
    for ((method, path, _, env, refusal), (status, body)) in cases.iter().zip(&rows) {
        match refusal {
            Some(expected) => assert_eq!(
                (*status, body.as_str()),
                (503, *expected),
                "{method} {path} with {env}: Node must refuse"
            ),
            None => {
                assert_ne!(
                    *status, 503,
                    "{method} {path} with {env}: Node must answer: {body}"
                );
                assert_ne!(*status, 0, "{method} {path}: unhandled");
            }
        }
    }
}
