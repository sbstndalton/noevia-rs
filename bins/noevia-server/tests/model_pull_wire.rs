//! Track R11 (S11): GET /api/models/mtp-artifact through the whole front. With
//! `NOEVIA_RUST_MODEL_WRITES` unset it is relayed to a synthetic Node; with `rust` the front
//! answers administrators from a scripted public hub on 127.0.0.1 (set through `Hub::for_tests`).
//! The hub replays the llama.cpp scenarios core's own code was recorded on
//! (crates/model-pull/tests/fixtures/node-model-pull.v1.json), and the answers must be Node's
//! (all but `checkedAt`, which was the recorder's clock). Also: the cache, the two-at-a-time
//! refusal, a member's refusal, a redirect off the hub, and POST /api/models/pull no longer relayed.
//! QA-named data only; no engine, no model loads, no network beyond 127.0.0.1.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use js_json::{wtf16, JValue};
use noevia_server::{
    config::Config,
    model_pull_routes::Hub,
    serve::{serve, Limits},
    App,
};
use server_store::rusqlite::Connection;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const A: &str = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
const B: &str = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";
const ADMIN: &str = "Cookie: cowork_session=admin-token; cowork_csrf=csrf\r\n";
const MEMBER: &str = "Cookie: cowork_session=member-token; cowork_csrf=csrf\r\n";
const SECRET: &str = "synthetic-store-token-0123456789abcdef";
const REV: &str = "aaaaaaaa0123456789abcdef0123456789abcdef";

fn hex(s: &str) -> String {
    Sha256::digest(s.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn seed(dir: &Path) {
    let db = Connection::open(dir.join("cowork.db")).unwrap();
    db.execute_batch(include_str!(
        "../../../crates/server-store/tests/fixtures/node-schema.sql"
    ))
    .unwrap();
    let now = noevia_server::identity::now_ms();
    for (uid, role, token) in [(A, "admin", "admin-token"), (B, "member", "member-token")] {
        db.execute("INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,created_at,updated_at) VALUES(?1,?1,?1,'QA',?2,'x',?1,1,1)", (uid, role)).unwrap();
        db.execute(
            "INSERT INTO sessions VALUES(?1,?2,?3,?4,?4,?5,'ua','127.0.0.1')",
            (hex(token), uid, hex("csrf"), now, now + 86_400_000),
        )
        .unwrap();
    }
}

async fn call(addr: SocketAddr, method: &str, path: &str, who: &str, body: &str) -> (u16, JValue) {
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(
        format!(
            "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n{who}Origin: http://{addr}\r\nX-CSRF-Token: csrf\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .as_bytes(),
    )
    .await
    .unwrap();
    let mut bytes = Vec::new();
    tokio::time::timeout(Duration::from_secs(60), s.read_to_end(&mut bytes))
        .await
        .unwrap()
        .unwrap();
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let (h, b) = text.split_once("\r\n\r\n").unwrap();
    (
        h.split_whitespace().nth(1).unwrap().parse().unwrap(),
        wtf16::parse(b).unwrap_or(JValue::Null),
    )
}

/// What a stub answers.
#[derive(Clone)]
struct Answer {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    delay_ms: u64,
}

fn json(status: u16, text: &str) -> Answer {
    Answer {
        status,
        headers: vec![("Content-Type".into(), "application/json".into())],
        body: text.as_bytes().to_vec(),
        delay_ms: 0,
    }
}

type Seen = Arc<Mutex<Vec<String>>>;

/// An HTTP/1.1 stub: `route(request line target)` picks the answer; every request head is kept.
async fn stub(route: impl Fn(&str) -> Answer + Send + Sync + 'static) -> (u16, Seen) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen: Seen = Arc::default();
    let (kept, route) = (Arc::clone(&seen), Arc::new(route));
    tokio::spawn(async move {
        while let Ok((mut c, _)) = listener.accept().await {
            let (kept, route) = (Arc::clone(&kept), Arc::clone(&route));
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 8192];
                let head = loop {
                    let Ok(n) = c.read(&mut chunk).await else {
                        return;
                    };
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    let t = String::from_utf8_lossy(&buf).into_owned();
                    if let Some((h, _)) = t.split_once("\r\n\r\n") {
                        break h.to_owned();
                    }
                };
                kept.lock().unwrap().push(head.clone());
                let target = head.split_whitespace().nth(1).unwrap_or("").to_owned();
                let a = route(&target);
                tokio::time::sleep(Duration::from_millis(a.delay_ms)).await;
                let mut out = format!("HTTP/1.1 {} X\r\n", a.status);
                for (k, v) in &a.headers {
                    out.push_str(&format!("{k}: {v}\r\n"));
                }
                out.push_str(&format!(
                    "Content-Length: {}\r\nConnection: close\r\n\r\n",
                    a.body.len()
                ));
                let mut bytes = out.into_bytes();
                bytes.extend_from_slice(&a.body);
                let _ = c.write_all(&bytes).await;
            });
        }
    });
    (port, seen)
}

/// A synthetic Node: every request is relayed here and marked.
async fn node() -> (u16, Seen) {
    stub(|_| json(202, r#"{"relayed":true}"#)).await
}

async fn front(dir: &Path, node: u16, hub: u16, on: bool) -> (SocketAddr, Arc<App>) {
    seed(dir);
    let (router, _) = stub(|_| json(404, "{}")).await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut env: HashMap<String, String> = [
        ("NOEVIA_LEGACY_UPSTREAM", format!("http://127.0.0.1:{node}")),
        ("UI_DATA_DIR", dir.display().to_string()),
        ("PUBLIC_ORIGIN", format!("http://{addr}")),
        ("NOEVIA_PROJECT_STORE_TOKEN", SECRET.to_owned()),
        ("MODEL_MANAGER_KIND", "llamacpp".to_owned()),
        (
            "MODEL_MANAGER_BASE_URL",
            format!("http://127.0.0.1:{router}"),
        ),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v))
    .collect();
    if on {
        env.insert("NOEVIA_RUST_MODEL_WRITES".into(), "rust".into());
    }
    let mut config = Config::from_lookup(|k| env.get(k).cloned()).unwrap();
    config.model_pull_hub = Hub::for_tests(&format!("http://127.0.0.1:{hub}"));
    let app = App::new(config);
    tokio::spawn(serve(
        listener,
        Arc::clone(&app),
        Limits::default(),
        std::future::pending(),
    ));
    (addr, app)
}

// ── GGUF prefixes and the recorded fixture ──────────────────────────────────────────────────

fn gguf(tensors: &[&str]) -> Vec<u8> {
    let mut b = b"GGUF".to_vec();
    b.extend(3u32.to_le_bytes());
    b.extend((tensors.len() as u64).to_le_bytes());
    b.extend(1u64.to_le_bytes());
    let key = b"general.architecture";
    b.extend((key.len() as u64).to_le_bytes());
    b.extend(key);
    b.extend(8u32.to_le_bytes());
    b.extend(7u64.to_le_bytes());
    b.extend(b"qa_arch");
    for t in tensors {
        b.extend((t.len() as u64).to_le_bytes());
        b.extend(t.as_bytes());
        b.extend(1u32.to_le_bytes());
        b.extend(4096u64.to_le_bytes());
        b.extend(1u32.to_le_bytes());
        b.extend(0u64.to_le_bytes());
    }
    b
}

fn ranged(bytes: Vec<u8>) -> Answer {
    Answer {
        status: 206,
        headers: vec![(
            "Content-Range".into(),
            format!("bytes 0-{}/99999999", bytes.len() - 1),
        )],
        body: bytes,
        delay_ms: 0,
    }
}

fn meta() -> Answer {
    json(200, &format!(r#"{{"id":"qa","sha":"{REV}"}}"#))
}

fn unbase64(text: &str) -> Vec<u8> {
    let value = |c: u8| match c {
        b'A'..=b'Z' => c - b'A',
        b'a'..=b'z' => c - b'a' + 26,
        b'0'..=b'9' => c - b'0' + 52,
        b'+' => 62,
        b'/' => 63,
        _ => panic!("base64"),
    };
    let digits: Vec<u8> = text.bytes().filter(|&c| c != b'=').map(value).collect();
    let mut out = Vec::new();
    for chunk in digits.chunks(4) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, d)| n | (u32::from(*d) << (18 - 6 * i)));
        out.extend(n.to_be_bytes().iter().skip(1).take(chunk.len() - 1));
    }
    out
}

/// A recorded hub answer (the recorder's `respond`, without its stream failures).
fn recorded(o: &JValue) -> Answer {
    let status = o.get("status").as_num().unwrap() as u16;
    let body = match (o.get("json"), o.get("text")) {
        (JValue::Undefined, JValue::Str(t)) => t.as_bytes().to_vec(),
        (JValue::Undefined, _) => {
            let mut out = Vec::new();
            if let JValue::Arr(parts) = o.get("parts") {
                for p in parts {
                    match p.get("b64") {
                        JValue::Str(b) => out.extend(unbase64(b)),
                        _ => out.resize(
                            out.len() + p.get("n").as_num().unwrap() as usize,
                            p.get("fill").as_num().unwrap() as u8,
                        ),
                    }
                }
            }
            out
        }
        (json, _) => wtf16::stringify(json).unwrap().into_bytes(),
    };
    let mut headers = Vec::new();
    if let JValue::Str(range) = o.get("contentRange") {
        headers.push(("Content-Range".into(), range.clone()));
    }
    Answer {
        status,
        headers,
        body,
        delay_ms: 0,
    }
}

fn without_time(v: &JValue) -> JValue {
    match v {
        JValue::Obj(items) => JValue::Obj(
            items
                .iter()
                .filter(|(k, _)| k != "checkedAt")
                .cloned()
                .collect(),
        ),
        other => other.clone(),
    }
}

// ── The tests ───────────────────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn switch_unset_the_check_and_the_pull_stay_node_s() {
    let dir = tempfile::tempdir().unwrap();
    let (node_port, relayed) = node().await;
    let (hub, hub_seen) = stub(|_| meta()).await;
    let (addr, _) = front(dir.path(), node_port, hub, false).await;
    let (status, body) = call(
        addr,
        "GET",
        "/api/models/mtp-artifact?repo=qa-org/qa-model&variant=Q8_0",
        ADMIN,
        "",
    )
    .await;
    assert_eq!(
        (status, body),
        (202, JValue::obj([("relayed", JValue::Bool(true))]))
    );
    let (status, _) = call(
        addr,
        "POST",
        "/api/models/pull",
        ADMIN,
        r#"{"checkpoint":"qa-org/qa-model:Q8_0"}"#,
    )
    .await;
    assert_eq!(status, 202);
    assert_eq!(relayed.lock().unwrap().len(), 2);
    assert!(hub_seen.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn switch_on_a_member_is_refused_and_the_pull_is_not_relayed() {
    let dir = tempfile::tempdir().unwrap();
    let (node_port, relayed) = node().await;
    let (hub, hub_seen) = stub(|_| meta()).await;
    let (addr, app) = front(dir.path(), node_port, hub, true).await;
    let (status, body) = call(
        addr,
        "GET",
        "/api/models/mtp-artifact?repo=qa-org/qa-model&variant=Q8_0",
        MEMBER,
        "",
    )
    .await;
    assert_eq!(
        (status, body),
        (
            403,
            JValue::obj([("error", JValue::from("administrator required"))])
        )
    );
    assert!(hub_seen.lock().unwrap().is_empty());
    let (status, _) = call(
        addr,
        "POST",
        "/api/models/pull",
        ADMIN,
        r#"{"checkpoint":"qa-org/qa-model:Q8_0"}"#,
    )
    .await;
    // The pull is the front's under the switch too (model_pull_post.rs): this synthetic Node does
    // not confirm the scan clear, so the front answers its fail-closed 502 and relays nothing.
    assert_eq!(status, 502);
    let heads = relayed.lock().unwrap().clone();
    assert!(
        !heads
            .iter()
            .any(|h| h.starts_with("POST /api/models/pull ")),
        "{heads:?}"
    );
    assert!(!heads.iter().any(|h| h.contains("mtp-artifact")));
    // Other methods on the check's path are not Node's either way (residual_fallback.rs relays
    // only its GET): the front's 404, nothing relayed, nothing fetched.
    let (status, body) = call(addr, "POST", "/api/models/mtp-artifact", ADMIN, "{}").await;
    assert_eq!(
        status,
        404,
        "{}",
        wtf16::stringify(&body).unwrap_or_default()
    );
    assert!(!relayed
        .lock()
        .unwrap()
        .iter()
        .any(|h| h.contains("mtp-artifact")));
    assert!(hub_seen.lock().unwrap().is_empty());
    assert_eq!(app.mtp_artifact.scan_clears(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_front_answers_the_recorded_llamacpp_scenarios_as_node_did() {
    let fixture = wtf16::parse(include_str!(
        "../../../crates/model-pull/tests/fixtures/node-model-pull.v1.json"
    ))
    .unwrap();
    let JValue::Arr(scenarios) = fixture.get("mtp") else {
        panic!()
    };
    let mut statuses = Vec::new();
    for s in scenarios {
        let name = s.get("name").as_str().unwrap();
        if !name.starts_with("llamacpp ") || name.contains("a/..") {
            continue;
        }
        let step = &match s.get("steps") {
            JValue::Arr(steps) => steps.clone(),
            _ => panic!(),
        }[0];
        let r = &match step.get("requests") {
            JValue::Arr(rs) => rs.clone(),
            _ => panic!(),
        }[0];
        let script = match r.get("script") {
            JValue::Arr(items) => items[0].clone(),
            _ => panic!(),
        };
        if !script.get("reply").is_object() {
            continue;
        }
        let reply = script.get("reply").clone();
        let hf = step.get("hf").clone();
        let (hub, _) = stub(move |target| {
            if target == "/api/models/qa-org/qa-model?blobs=true" {
                let body = match reply.get("body") {
                    JValue::Undefined => String::new(),
                    b => wtf16::stringify(b).unwrap(),
                };
                return json(reply.get("status").as_num().unwrap() as u16, &body);
            }
            match hf.get(&format!("https://huggingface.co{target}")) {
                o if o.is_object() => recorded(o),
                _ => json(404, "{}"),
            }
        })
        .await;
        let dir = tempfile::tempdir().unwrap();
        let (node_port, relayed) = node().await;
        let (addr, app) = front(dir.path(), node_port, hub, true).await;
        let query = r.get("query").as_str().unwrap();
        let (status, body) = call(
            addr,
            "GET",
            &format!("/api/models/mtp-artifact?{query}"),
            ADMIN,
            "",
        )
        .await;
        assert_eq!(JValue::Num(f64::from(status)), *r.get("status"), "{name}");
        assert_eq!(
            without_time(&body),
            without_time(r.get("body")),
            "{name}: {}",
            wtf16::stringify(&body).unwrap()
        );
        assert!(relayed
            .lock()
            .unwrap()
            .iter()
            .all(|h| !h.contains("mtp-artifact")));
        assert_eq!(app.mtp_artifact.scan_clears(), 0);
        statuses.push(body.get("status").as_str().unwrap_or("").to_owned());
    }
    for want in ["present", "absent"] {
        assert!(statuses.iter().any(|s| s == want), "{want} in {statuses:?}");
    }
    assert!(statuses.len() >= 10, "{statuses:?}");
}

/// A hub for qa-org/qa-<n>: one Q8_0 file each, with or without an MTP tensor; `delay_ms` on the
/// revision reply; `/redirect/*` for the prefix of qa-org/qa-moved.
async fn hub_for(delay_ms: u64, elsewhere: u16) -> (u16, Seen) {
    stub(move |target| {
        if let Some(rest) = target.strip_prefix("/api/models/qa-org/") {
            if let Some(repo) = rest.strip_suffix("?blobs=true") {
                // qa-list<status>: the file list answered with that 3xx (and a Location).
                if let Some(code) = repo
                    .strip_prefix("qa-list")
                    .and_then(|c| c.parse::<u16>().ok())
                {
                    return Answer {
                        status: code,
                        headers: vec![("Location".into(), "/cdn/qa-list".into())],
                        body: Vec::new(),
                        delay_ms: 0,
                    };
                }
                return json(
                    200,
                    &format!(r#"{{"siblings":[{{"rfilename":"{repo}-Q8_0.gguf"}}]}}"#),
                );
            }
            return Answer { delay_ms, ..meta() };
        }
        // qa-chain<n>: the prefix redirects n times within the hub (/hop/<n-1> .. /hop/0).
        if let Some(n) = target
            .split("qa-chain")
            .nth(1)
            .filter(|_| target.contains("/resolve/"))
            .map(|rest| {
                rest.chars()
                    .take_while(char::is_ascii_digit)
                    .collect::<String>()
            })
            .and_then(|n| n.parse::<u32>().ok())
        {
            return hop(n - 1);
        }
        if let Some(n) = target
            .strip_prefix("/hop/")
            .and_then(|n| n.parse::<u32>().ok())
        {
            if n == 0 {
                return ranged(gguf(&[
                    "blk.0.attn_q.weight",
                    "blk.40.nextn.eh_proj.weight",
                ]));
            }
            return hop(n - 1);
        }
        if target.contains("qa-missing-Q8_0") {
            return json(404, "{}");
        }
        if target.contains("qa-moved-Q8_0") {
            return Answer {
                status: 302,
                headers: vec![(
                    "Location".into(),
                    format!("http://127.0.0.1:{elsewhere}/stolen"),
                )],
                body: Vec::new(),
                delay_ms: 0,
            };
        }
        if target.contains("qa-hop-Q8_0") {
            return Answer {
                status: 302,
                headers: vec![("Location".into(), "/cdn/qa-hop".into())],
                body: Vec::new(),
                delay_ms: 0,
            };
        }
        // A 300 or 304 with a Location: fetch follows neither, so each is the prefix's answer.
        for (repo, status) in [("qa-r300-Q8_0", 300), ("qa-r304-Q8_0", 304)] {
            if target.contains(repo) {
                return Answer {
                    status,
                    headers: vec![("Location".into(), "/cdn/qa-hop".into())],
                    body: Vec::new(),
                    delay_ms: 0,
                };
            }
        }
        // A 302 whose Location does not parse (an unclosed IPv6 host): fetch's network error.
        if target.contains("qa-badloc-Q8_0") {
            return Answer {
                status: 302,
                headers: vec![("Location".into(), "http://[qa/x".into())],
                body: Vec::new(),
                delay_ms: 0,
            };
        }
        // A 302 to a path written in UTF-8: fetch reads it as UTF-8 and the URL parser
        // percent-encodes it, so the hop goes to /cdn/qa-%C3%A9.
        if target.contains("qa-utf8-Q8_0") {
            return Answer {
                status: 302,
                headers: vec![("Location".into(), "/cdn/qa-\u{e9}".into())],
                body: Vec::new(),
                delay_ms: 0,
            };
        }
        if target.contains("qa-present") || target == "/cdn/qa-hop" || target == "/cdn/qa-%C3%A9" {
            return ranged(gguf(&[
                "blk.0.attn_q.weight",
                "blk.40.nextn.eh_proj.weight",
            ]));
        }
        ranged(gguf(&["blk.0.attn_q.weight"]))
    })
    .await
}

fn hop(to: u32) -> Answer {
    Answer {
        status: 302,
        headers: vec![("Location".into(), format!("/hop/{to}"))],
        body: Vec::new(),
        delay_ms: 0,
    }
}

fn query(repo: &str) -> String {
    format!("/api/models/mtp-artifact?repo=qa-org/{repo}&variant=Q8_0")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_cache_answers_a_second_request_without_a_fetch() {
    let dir = tempfile::tempdir().unwrap();
    let (node_port, _) = node().await;
    let (elsewhere, _) = stub(|_| json(200, "{}")).await;
    let (hub, seen) = hub_for(0, elsewhere).await;
    let (addr, _) = front(dir.path(), node_port, hub, true).await;
    let (status, first) = call(addr, "GET", &query("qa-present"), ADMIN, "").await;
    assert_eq!(status, 200);
    assert_eq!(
        first.get("status").as_str(),
        Some("present"),
        "{}",
        wtf16::stringify(&first).unwrap()
    );
    assert_eq!(first.get("revision").as_str(), Some(REV));
    let fetched = seen.lock().unwrap().len();
    assert_eq!(fetched, 3, "the file list, the revision, the prefix");
    let (_, second) = call(addr, "GET", &query("qa-present"), ADMIN, "").await;
    assert_eq!(second, first, "the cached answer, its checkedAt included");
    // The file list is asked again (core asks it on every request); nothing else is.
    assert_eq!(seen.lock().unwrap().len(), fetched + 1);
    let heads = seen.lock().unwrap().clone();
    let prefix = heads
        .iter()
        .find(|h| h.contains("/resolve/"))
        .unwrap()
        .to_ascii_lowercase();
    assert!(prefix.contains("range: bytes=0-16777215"), "{prefix}");
    for h in &heads {
        let h = h.to_ascii_lowercase();
        assert!(
            !h.contains("authorization") && !h.contains("x-noevia-maintenance"),
            "{h}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_third_check_is_refused_while_two_run() {
    let dir = tempfile::tempdir().unwrap();
    let (node_port, _) = node().await;
    let (elsewhere, _) = stub(|_| json(200, "{}")).await;
    let (hub, _) = hub_for(1500, elsewhere).await;
    let (addr, _) = front(dir.path(), node_port, hub, true).await;
    let one = tokio::spawn(async move { call(addr, "GET", &query("qa-one"), ADMIN, "").await });
    let two = tokio::spawn(async move { call(addr, "GET", &query("qa-two"), ADMIN, "").await });
    tokio::time::sleep(Duration::from_millis(500)).await;
    let (status, third) = call(addr, "GET", &query("qa-three"), ADMIN, "").await;
    assert_eq!(status, 200);
    assert_eq!(third.get("status").as_str(), Some("unknown"));
    assert_eq!(
        third.get("reason").as_str(),
        Some("Two artifact checks are already running. Try again shortly.")
    );
    assert_eq!(third.get("files"), &JValue::Arr(Vec::new()));
    for done in [one, two] {
        let (status, body) = done.await.unwrap();
        assert_eq!(status, 200);
        assert_eq!(
            body.get("status").as_str(),
            Some("absent"),
            "{}",
            wtf16::stringify(&body).unwrap()
        );
    }
    // Both finished: the permits are back.
    let (_, again) = call(addr, "GET", &query("qa-three"), ADMIN, "").await;
    assert_eq!(again.get("status").as_str(), Some("absent"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_redirect_off_the_hub_is_refused_and_one_on_it_followed() {
    let dir = tempfile::tempdir().unwrap();
    let (node_port, _) = node().await;
    let (elsewhere, elsewhere_seen) = stub(|_| json(200, "{}")).await;
    let (hub, _) = hub_for(0, elsewhere).await;
    let (addr, _) = front(dir.path(), node_port, hub, true).await;
    let (status, body) = call(addr, "GET", &query("qa-moved"), ADMIN, "").await;
    assert_eq!(status, 200);
    assert_eq!(body.get("status").as_str(), Some("unknown"));
    assert_eq!(body.get("reason").as_str(), Some("fetch failed"));
    assert!(
        elsewhere_seen.lock().unwrap().is_empty(),
        "nothing reached the other host"
    );
    let (_, body) = call(addr, "GET", &query("qa-hop"), ADMIN, "").await;
    assert_eq!(
        body.get("status").as_str(),
        Some("present"),
        "{}",
        wtf16::stringify(&body).unwrap()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_second_request_for_a_running_check_gets_its_answer() {
    let dir = tempfile::tempdir().unwrap();
    let (node_port, _) = node().await;
    let (elsewhere, _) = stub(|_| json(200, "{}")).await;
    let (hub, seen) = hub_for(1500, elsewhere).await;
    let (addr, _) = front(dir.path(), node_port, hub, true).await;
    let first =
        tokio::spawn(async move { call(addr, "GET", &query("qa-missing"), ADMIN, "").await });
    tokio::time::sleep(Duration::from_millis(500)).await;
    let (status, joined) = call(addr, "GET", &query("qa-missing"), ADMIN, "").await;
    let (first_status, first) = first.await.unwrap();
    assert_eq!((status, first_status), (200, 200));
    // An unknown answer is never cached: only the join can give the second request this one.
    assert_eq!(first.get("status").as_str(), Some("unknown"));
    assert_eq!(
        first.get("reason").as_str(),
        Some("Artifact metadata unavailable (404).")
    );
    assert_eq!(
        joined, first,
        "the running check's answer, checkedAt included"
    );
    let heads = seen.lock().unwrap().clone();
    let revisions = heads
        .iter()
        .filter(|h| h.starts_with("GET /api/models/qa-org/qa-missing HTTP"))
        .count();
    assert_eq!(revisions, 1, "one check ran: {heads:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn twenty_redirects_are_followed_and_a_twenty_first_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (node_port, _) = node().await;
    let (elsewhere, _) = stub(|_| json(200, "{}")).await;
    let (hub, seen) = hub_for(0, elsewhere).await;
    let (addr, _) = front(dir.path(), node_port, hub, true).await;
    let (_, twenty) = call(addr, "GET", &query("qa-chain20"), ADMIN, "").await;
    assert_eq!(
        twenty.get("status").as_str(),
        Some("present"),
        "{}",
        wtf16::stringify(&twenty).unwrap()
    );
    let hops = |seen: &Seen| {
        seen.lock()
            .unwrap()
            .iter()
            .filter(|h| h.starts_with("GET /hop/"))
            .count()
    };
    assert_eq!(hops(&seen), 20);
    // The Range header goes with every hop.
    assert!(seen
        .lock()
        .unwrap()
        .iter()
        .filter(|h| h.starts_with("GET /hop/"))
        .all(|h| h.to_ascii_lowercase().contains("range: bytes=0-16777215")));
    let (_, twenty_one) = call(addr, "GET", &query("qa-chain21"), ADMIN, "").await;
    assert_eq!(twenty_one.get("status").as_str(), Some("unknown"));
    assert_eq!(twenty_one.get("reason").as_str(), Some("fetch failed"));
    // 20 more hops reached; the 21st redirect's target (/hop/0) is never asked.
    assert_eq!(hops(&seen), 40);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn only_fetch_s_redirect_statuses_are_followed() {
    let dir = tempfile::tempdir().unwrap();
    let (node_port, _) = node().await;
    let (elsewhere, _) = stub(|_| json(200, "{}")).await;
    let (hub, seen) = hub_for(0, elsewhere).await;
    let (addr, _) = front(dir.path(), node_port, hub, true).await;
    // 300 and 304 are answers, not redirects: core's reason for a prefix that is not ok.
    for (repo, status) in [("qa-r300", 300), ("qa-r304", 304)] {
        let (code, body) = call(addr, "GET", &query(repo), ADMIN, "").await;
        assert_eq!(code, 200);
        let shown = wtf16::stringify(&body).unwrap();
        assert_eq!(body.get("status").as_str(), Some("unknown"), "{shown}");
        let reason = format!("Artifact metadata unavailable ({status}).");
        assert_eq!(
            body.get("reason").as_str(),
            Some(reason.as_str()),
            "{shown}"
        );
    }
    assert!(
        !seen
            .lock()
            .unwrap()
            .iter()
            .any(|h| h.starts_with("GET /cdn/qa-hop ")),
        "neither Location was asked"
    );
    // A followed status whose Location does not parse: fetch failed.
    let (code, body) = call(addr, "GET", &query("qa-badloc"), ADMIN, "").await;
    assert_eq!(code, 200);
    assert_eq!(body.get("status").as_str(), Some("unknown"));
    assert_eq!(body.get("reason").as_str(), Some("fetch failed"));
    // A Location in UTF-8 is followed, percent-encoded.
    let (_, body) = call(addr, "GET", &query("qa-utf8"), ADMIN, "").await;
    assert_eq!(
        body.get("status").as_str(),
        Some("present"),
        "{}",
        wtf16::stringify(&body).unwrap()
    );
    assert!(seen
        .lock()
        .unwrap()
        .iter()
        .any(|h| h.starts_with("GET /cdn/qa-%C3%A9 ")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_file_list_answered_300_or_304_is_not_ok_and_a_redirect_a_throw() {
    // #2661: the variants lookup is fetchJson's (`redirect: 'error'`): only 301, 302, 303, 307 and
    // 308 throw (core's 500 `Internal error`); a 300 or a 304 is a listing that is not ok, which
    // core answers 502 `Could not resolve selected files`. Neither Location is asked.
    let dir = tempfile::tempdir().unwrap();
    let (node_port, _) = node().await;
    let (elsewhere, _) = stub(|_| json(200, "{}")).await;
    let (hub, seen) = hub_for(0, elsewhere).await;
    let (addr, _) = front(dir.path(), node_port, hub, true).await;
    for (repo, status, error) in [
        ("qa-list300", 502, "Could not resolve selected files"),
        ("qa-list304", 502, "Could not resolve selected files"),
        ("qa-list302", 500, "Internal error"),
    ] {
        let (code, body) = call(addr, "GET", &query(repo), ADMIN, "").await;
        assert_eq!(
            (code, body),
            (status, JValue::obj([("error", JValue::from(error))])),
            "{repo}"
        );
    }
    let heads = seen.lock().unwrap().clone();
    assert!(
        !heads.iter().any(|h| h.starts_with("GET /cdn/qa-list ")),
        "{heads:?}"
    );
    // Only the three listings: no check ran.
    assert_eq!(heads.len(), 3, "{heads:?}");
}
