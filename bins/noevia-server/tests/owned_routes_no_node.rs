//! The deletion train's proof that the front's route manifest (`noevia-server --owned-routes`) is safe: signed in as
//! the owner, with every deployment switch off, every listed (method, pattern) pair is sent under both fillings, and
//! none may come back from Node. The manifest is route-table ownership; this test checks the handler as well, so a
//! native handler that still hands a method or a spelling on to Node is caught here, not by the text scan in
//! owned_routes_tests (sbstndalton/noevia#2725 item 4).
//!
//! Opt-in: it runs only with `ROUTE_PINS_STRICT=1`, the deletion train's mode, because it sends about 730
//! requests to every native handler (some do real work against the synthetic data directory) and a newly added
//! route must not make every track's CI slower or flakier. Synthetic data only.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
#[path = "support/approval_front.rs"]
mod fixture;
use fixture::*;
use noevia_server::owned_routes::{fill, manifest, FILLS};
use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// How long one request may take to send back its head. A forward to the fake Node answers at once, so a slower
/// answer is native work (a stream, a timeout against an absent service); it is counted and printed, not failed.
const HEAD_WITHIN: Duration = Duration::from_secs(3);
/// The whole sweep's deadline (every wait in this file is bounded).
const SWEEP_WITHIN: Duration = Duration::from_secs(900);
/// The pairs that end the owner's session on purpose. They run last, so the rest of the sweep stays signed in.
const SESSION_ENDERS: &[(&str, &str)] = &[("POST", "/api/auth/logout")];
const PASSWORD: &str = "synthetic approval password";
/// Pattern prefixes left out of the sweep while their owner holds them: every Diary route (the cloud move holds all
/// Diary work, and the deletion train deletes no Diary stub until it ends). Remove the hold with the move.
const HELD: &[&str] = &["/api/diary", "/api/profile/diary-connectors"];

/// The pairs a signed-in owner can use to start a sign-in with an outside host without any body (a provider's
/// device login or model list, any `…/connect`). Left out so the sweep never dials out; the deletion train checks
/// them by hand or with egress blocked.
fn outbound(pattern: &str) -> bool {
    // An optional tail (`{*?}`) is filled with nothing, so the pattern and its filled path agree.
    let p = pattern.trim_end_matches("{*?}");
    p.ends_with("/connect")
        || (p.starts_with("/api/providers/") && (p.ends_with("/device") || p.ends_with("/models")))
}

fn held(pattern: &str) -> bool {
    HELD.iter().any(|h| pattern.starts_with(h)) || outbound(pattern)
}

/// Native handlers that call Node themselves on the same public path (not a forward): the readiness read asks Node's
/// own readiness. A Node hit on one of these is not a leak.
const NODE_PROBES: &[&str] = &["GET /api/ready"];
/// The harness trusts the proxy: the sweep and the re-sign-in come from different documentation addresses, so the
/// sweep's own failed sign-ins (empty bodies on the login routes) never rate-limit the re-sign-in.
const SWEEP_FROM: &str = "192.0.2.10";
const SIGN_IN_FROM: &str = "192.0.2.20";

fn strict() -> bool {
    std::env::var("ROUTE_PINS_STRICT").as_deref() == Ok("1")
}

/// A fake Node that answers everything with `x-upstream: 1` and records what it saw.
async fn recording_node() -> (SocketAddr, Arc<Mutex<Vec<String>>>) {
    let hits = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&hits);
    let router = axum::Router::new().fallback(move |req: axum::extract::Request| {
        let seen = Arc::clone(&seen);
        async move {
            seen.lock()
                .unwrap()
                .push(format!("{} {}", req.method(), req.uri().path()));
            (
                [("x-upstream", "1"), ("content-type", "application/json")],
                "{}",
            )
        }
    });
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (addr, hits)
}

/// The response head of one request: (status, whether it came from Node), or None when no head arrived in time.
async fn head_of(
    addr: SocketAddr,
    method: &str,
    path: &str,
    jar: &str,
    csrf: &str,
) -> Option<(u16, bool)> {
    let body = if matches!(method, "POST" | "PUT" | "PATCH" | "DELETE") {
        "{}"
    } else {
        ""
    };
    let req = format!(
        "{method} {path} HTTP/1.1\r\nhost: front.test\r\norigin: {ORIGIN}\r\ncookie: {jar}\r\nx-csrf-token: {csrf}\r\n\
         x-forwarded-for: {SWEEP_FROM}\r\nconnection: close\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
        body.len()
    );
    let read = async {
        let mut c = TcpStream::connect(addr).await.ok()?;
        c.write_all(req.as_bytes()).await.ok()?;
        let mut out = Vec::new();
        let mut buf = [0u8; 4096];
        while !out.windows(4).any(|w| w == b"\r\n\r\n") {
            let n = c.read(&mut buf).await.ok()?;
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }
        Some(out)
    };
    let out = tokio::time::timeout(HEAD_WITHIN, read).await.ok()??;
    let text = String::from_utf8_lossy(&out);
    let head = text.split("\r\n\r\n").next().unwrap_or("");
    let status = head.split(' ').nth(1)?.parse().ok()?;
    let from_node = head
        .lines()
        .any(|l| l.to_ascii_lowercase().starts_with("x-upstream:"));
    Some((status, from_node))
}

/// Whether the jar is still a signed-in session: Some(true) when the session read names the jar's CSRF token (it
/// does only for a signed-in session), Some(false) on any other complete answer, None when no answer came in time.
async fn session_state(addr: SocketAddr, jar: &str, csrf: &str) -> Option<bool> {
    let req = format!(
        "GET /api/auth/session HTTP/1.1\r\nhost: front.test\r\ncookie: {jar}\r\nconnection: close\r\n\r\n"
    );
    let read = async {
        let mut s = TcpStream::connect(addr).await.ok()?;
        s.write_all(req.as_bytes()).await.ok()?;
        let mut out = Vec::new();
        s.read_to_end(&mut out).await.ok()?;
        Some(String::from_utf8_lossy(&out).into_owned())
    };
    let text = tokio::time::timeout(HEAD_WITHIN * 2, read).await.ok()??;
    if !text.starts_with("HTTP/1.1 ") {
        return None;
    }
    Some(text.starts_with("HTTP/1.1 200") && text.contains(&format!(r#""csrfToken":"{csrf}""#)))
}

/// [`session_state`] with one retry when no answer came: Some(false) only on a definite signed-out answer.
async fn signed_in(addr: SocketAddr, jar: &str, csrf: &str) -> Option<bool> {
    match session_state(addr, jar, csrf).await {
        Some(v) => Some(v),
        None => session_state(addr, jar, csrf).await,
    }
}

/// A fresh owner session after a pair ended the old one: (jar, csrf), or None when the owner can no longer sign in.
async fn sign_in_again(addr: SocketAddr) -> Option<(String, String)> {
    let body = format!(r#"{{"username":"owner","password":"{PASSWORD}"}}"#);
    let req = format!(
        "POST /api/auth/login/password HTTP/1.1\r\nhost: front.test\r\norigin: {ORIGIN}\r\nconnection: close\r\n\
         x-forwarded-for: {SIGN_IN_FROM}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
        body.len()
    );
    let read = async {
        let mut s = TcpStream::connect(addr).await.ok()?;
        s.write_all(req.as_bytes()).await.ok()?;
        let mut out = Vec::new();
        s.read_to_end(&mut out).await.ok()?;
        Some(String::from_utf8_lossy(&out).into_owned())
    };
    let text = tokio::time::timeout(HEAD_WITHIN * 2, read).await.ok()??;
    if !text.starts_with("HTTP/1.1 200") && !text.starts_with("HTTP/1.1 201") {
        return None;
    }
    let cookies: Vec<String> = text
        .lines()
        .filter_map(|l| {
            l.strip_prefix("set-cookie: ")
                .and_then(|v| v.split(';').next())
                .map(str::to_string)
        })
        .collect();
    let csrf = cookies.get(1)?.split_once('=')?.1.to_string();
    Some((cookies.join("; "), csrf))
}

/// The sweep order: every pair under both fillings, the session enders last.
fn sweep() -> Vec<(&'static str, String)> {
    let pairs = manifest();
    let mut first = Vec::new();
    let mut last = Vec::new();
    for &(method, pattern) in &pairs {
        if held(pattern) {
            continue;
        }
        let mut paths: Vec<String> = FILLS.iter().map(|id| fill(pattern, id)).collect();
        paths.dedup();
        for path in paths {
            if SESSION_ENDERS.contains(&(method, pattern)) {
                last.push((method, path));
            } else {
                first.push((method, path));
            }
        }
    }
    first.extend(last);
    first
}

#[test]
fn outbound_sign_ins_are_held_by_shape() {
    for yes in [
        "/api/mcp-oauth/{id}/connect",
        "/api/providers/x/device",
        "/api/providers/{providerId}/models{*?}",
        "/api/providers/qa-1/models",
    ] {
        assert!(held(yes), "{yes}");
    }
    for no in [
        "/api/providers",
        "/api/providers/{id}",
        "/api/providers/test",
        "/api/models",
        "/api/connectors",
    ] {
        assert!(!held(no), "{no}");
    }
}

#[test]
fn the_sweep_lists_every_pair_once_per_distinct_filling_and_ends_with_the_session_enders() {
    let s = sweep();
    let pairs = manifest();
    let swept = pairs.iter().filter(|(_, p)| !held(p)).count();
    assert!(swept < pairs.len(), "the Diary hold leaves pairs out");
    assert!(s.len() >= swept);
    assert!(s.len() <= swept * FILLS.len());
    assert!(
        s.iter().all(|(_, path)| !held(path)),
        "a held pair is swept"
    );
    let distinct: BTreeSet<&(&str, String)> = s.iter().collect();
    assert_eq!(distinct.len(), s.len(), "a request is sent twice");
    for &(method, pattern) in SESSION_ENDERS {
        if pairs.contains(&(method, pattern)) {
            assert_eq!(s.last().map(|x| x.0), Some(method));
            assert_eq!(s.last().map(|x| x.1.as_str()), Some(pattern));
        }
    }
}

// Four workers: a native handler that blocks its thread must not stall the client's own deadlines.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_listed_pair_reaches_node_for_a_signed_in_owner() {
    if !strict() {
        eprintln!("owned_routes_no_node: skipped (the deletion train sets ROUTE_PINS_STRICT=1)");
        return;
    }
    let (node_addr, hits) = recording_node().await;
    let (_unused, node) = node().await;
    let f = front_with_upstream(format!("http://{node_addr}"), node).await;
    assert_eq!(
        signed_in(f.addr, &f.jar, &f.csrf).await,
        Some(true),
        "the owner is signed in"
    );
    let order = sweep();
    let total = order.len();
    let run = async {
        let (mut jar, mut csrf) = (f.jar.clone(), f.csrf.clone());
        let mut reached = Vec::new();
        let mut slow = Vec::new();
        let mut enders = Vec::new();
        let mut unsure = Vec::new();
        for (method, path) in order {
            match head_of(f.addr, method, &path, &jar, &csrf).await {
                Some((status, true)) => reached.push(format!("{method} {path} -> {status}")),
                Some(_) => {}
                None => slow.push(format!("{method} {path}")),
            }
            let state = signed_in(f.addr, &jar, &csrf).await;
            if state != Some(true) {
                // No answer twice: not counted as an ender, but sign in again so the rest stays signed in.
                if state.is_none() {
                    unsure.push(format!("{method} {path}"));
                } else {
                    enders.push(format!("{method} {path}"));
                }
                let Some(fresh) = sign_in_again(f.addr).await else {
                    panic!("after {method} {path} the owner can no longer sign in; the rest of the sweep would be signed out");
                };
                (jar, csrf) = fresh;
            }
        }
        (reached, slow, enders, unsure)
    };
    let (reached, slow, enders, unsure) = tokio::time::timeout(SWEEP_WITHIN, run)
        .await
        .expect("the sweep finishes within its deadline");
    eprintln!(
        "owned_routes_no_node: {total} requests; {} slower than {HEAD_WITHIN:?} (native work, not Node): {slow:?}",
        slow.len()
    );
    // A pair that ended the session but is not listed made part of the sweep run on a new session; name it so it
    // moves to SESSION_ENDERS (the result for it is still checked above).
    let unlisted: Vec<&String> = enders
        .iter()
        .filter(|e| {
            !SESSION_ENDERS
                .iter()
                .any(|(m, p)| e.as_str() == format!("{m} {p}"))
        })
        .collect();
    eprintln!("owned_routes_no_node: session ended by {enders:?}; canary silent after {unsure:?}");
    let node_saw = hits.lock().unwrap().clone();
    // A forward that took longer than the head window (or lost its `x-upstream` header) still shows in Node's log
    // under the swept request's own method and path. Native handlers' own Node calls use other paths.
    let swept: BTreeSet<String> = sweep()
        .into_iter()
        .map(|(m, p)| format!("{m} {p}"))
        .collect();
    let leaked: BTreeSet<&String> = node_saw
        .iter()
        .filter(|h| swept.contains(h.as_str()) && !NODE_PROBES.contains(&h.as_str()))
        .collect();
    assert!(
        leaked.is_empty(),
        "Node saw listed pairs (a slow or header-stripped forward): {leaked:?}"
    );
    assert!(
        reached.is_empty(),
        "listed pairs answered by Node: {reached:?} (add each to HANDLER_FORWARDS or answer it natively); Node saw {} requests",
        node_saw.len()
    );
    assert!(
        unlisted.is_empty(),
        "pairs that end the session, not in SESSION_ENDERS: {unlisted:?}"
    );
}
