//! `noevia-web-supervisor --healthcheck` end to end against scripted loopback servers, and the same
//! cases through the image's own Node probe (`NODE_PROBE`, verbatim from noevia build/web.Dockerfile)
//! when `node` is on PATH: exit codes and the requests each sent must agree. Without node the
//! differential is skipped; `WEB_SUPERVISOR_HEALTH_ORACLE_REQUIRED=1` makes a missing node a failure.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The web image's HEALTHCHECK program (noevia build/web.Dockerfile, `node -e` argument).
const NODE_PROBE: &str = "fetch('http://127.0.0.1:'+(process.env.UI_PORT||8021)+'/api/setup/status',{signal:AbortSignal.timeout(5000)}).then(r=>process.exit(r.ok?0:1)).catch(()=>process.exit(1))";

enum Step {
    /// Read the request, write these bytes, close.
    Reply(Vec<u8>),
    /// Read the request, then say nothing for this long.
    Hold(Duration),
}

type Seen = Arc<Mutex<Vec<String>>>;

fn bind() -> TcpListener {
    TcpListener::bind("127.0.0.1:0").unwrap_or_else(|e| unreachable!("{e}"))
}

fn port_of(listener: &TcpListener) -> u16 {
    listener.local_addr().map(|a| a.port()).unwrap_or_else(|e| unreachable!("{e}"))
}

/// Reads one request head; returns (target, host header).
fn read_request(stream: &mut TcpStream) -> (String, String) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") && buf.len() < 65536 {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => buf.extend_from_slice(chunk.get(..n).unwrap_or_default()),
        }
    }
    let text = String::from_utf8_lossy(&buf).into_owned();
    let mut lines = text.split("\r\n");
    let target = lines
        .next()
        .and_then(|l| l.split(' ').nth(1))
        .unwrap_or_default()
        .to_string();
    let host = lines
        .filter_map(|l| l.split_once(':'))
        .find(|(n, _)| n.eq_ignore_ascii_case("host"))
        .map(|(_, v)| v.trim().to_string())
        .unwrap_or_default();
    (target, host)
}

/// Serves `steps` on `listener`, one connection each, recording `target` (plus ` BADHOST` when the
/// Host header is not `127.0.0.1:<port>`). Gives up 20 s after start.
fn serve(listener: TcpListener, steps: Vec<Step>, seen: Seen) {
    let port = port_of(&listener);
    let _ = listener.set_nonblocking(true);
    std::thread::spawn(move || {
        let until = Instant::now() + Duration::from_secs(20);
        for step in steps {
            let mut stream = loop {
                match listener.accept() {
                    Ok((s, _)) => break s,
                    Err(_) if Instant::now() < until => std::thread::sleep(Duration::from_millis(5)),
                    Err(_) => return,
                }
            };
            let _ = stream.set_nonblocking(false);
            let (target, host) = read_request(&mut stream);
            let mark = if host == format!("127.0.0.1:{port}") { "" } else { " BADHOST" };
            if let Ok(mut s) = seen.lock() {
                s.push(format!("{target}{mark}"));
            }
            match step {
                Step::Reply(bytes) => {
                    let _ = stream.write_all(&bytes);
                    let _ = stream.flush();
                }
                Step::Hold(d) => std::thread::sleep(d),
            }
        }
    });
}

fn reply(head: &str) -> Step {
    Step::Reply(format!("{head}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").into_bytes())
}

fn redirect(status: u16, location: &str) -> Step {
    reply(&format!("HTTP/1.1 {status} Moved\r\nLocation: {location}"))
}

/// One run's servers: the UI_PORT to probe with and what the servers saw.
struct Built {
    ui_port: Option<String>,
    seen: Seen,
}

fn single(steps: Vec<Step>) -> Built {
    let listener = bind();
    let port = port_of(&listener);
    let seen = Seen::default();
    serve(listener, steps, seen.clone());
    Built { ui_port: Some(port.to_string()), seen }
}

/// A port nobody listens on.
fn closed_port() -> u16 {
    port_of(&bind())
}

struct Case {
    name: &'static str,
    expect: i32,
    /// Requests the servers must have seen, in order.
    seen: Vec<String>,
    /// Compared with Node as well (false: behaviour not yet confirmed against Node).
    oracle: bool,
    build: Box<dyn Fn() -> Built>,
}

fn case(name: &'static str, expect: i32, seen: &[&str], build: impl Fn() -> Built + 'static) -> Case {
    Case {
        name,
        expect,
        seen: seen.iter().map(|s| s.to_string()).collect(),
        oracle: true,
        build: Box::new(build),
    }
}

const S: &str = "/api/setup/status";

fn cases() -> Vec<Case> {
    let mut cases = vec![
        case("200", 0, &[S], || single(vec![reply("HTTP/1.1 200 OK")])),
        case("204", 0, &[S], || single(vec![reply("HTTP/1.1 204 No Content")])),
        case("299", 0, &[S], || single(vec![reply("HTTP/1.1 299 Odd")])),
        case("http/1.0 200", 0, &[S], || single(vec![reply("HTTP/1.0 200 OK")])),
        case("404", 1, &[S], || single(vec![reply("HTTP/1.1 404 Not Found")])),
        case("500", 1, &[S], || single(vec![reply("HTTP/1.1 500 Internal Server Error")])),
        case("503", 1, &[S], || single(vec![reply("HTTP/1.1 503 Service Unavailable")])),
        case("304", 1, &[S], || single(vec![reply("HTTP/1.1 304 Not Modified")])),
        case("302 without location", 1, &[S], || single(vec![reply("HTTP/1.1 302 Found")])),
        case("302 absolute path", 0, &[S, "/next"], || {
            single(vec![redirect(302, "/next"), reply("HTTP/1.1 200 OK")])
        }),
        case("301 relative", 0, &[S, "/api/setup/next"], || {
            single(vec![redirect(301, "next"), reply("HTTP/1.1 200 OK")])
        }),
        case("303 dot segments", 0, &[S, "/api/x"], || {
            single(vec![redirect(303, "../x"), reply("HTTP/1.1 200 OK")])
        }),
        case("307 query only", 0, &[S, "/api/setup/status?again=1"], || {
            single(vec![redirect(307, "?again=1"), reply("HTTP/1.1 200 OK")])
        }),
        case("308 encoded", 0, &[S, "/a%20b"], || {
            single(vec![redirect(308, "/a b"), reply("HTTP/1.1 200 OK")])
        }),
        case("redirect to a 500", 1, &[S, "/next"], || {
            single(vec![redirect(302, "/next"), reply("HTTP/1.1 500 Nope")])
        }),
        case("redirect to another port", 0, &[S, "/elsewhere?x=1"], || {
            let other = bind();
            let other_port = port_of(&other);
            let seen = Seen::default();
            serve(other, vec![reply("HTTP/1.1 200 OK")], seen.clone());
            let first = bind();
            let port = port_of(&first);
            let location = format!("http://127.0.0.1:{other_port}/elsewhere?x=1#frag");
            serve(first, vec![redirect(302, &location)], seen.clone());
            Built { ui_port: Some(port.to_string()), seen }
        }),
        case("redirect to a closed port", 1, &[S], || {
            let location = format!("http://127.0.0.1:{}/", closed_port());
            single(vec![redirect(302, &location)])
        }),
        case("redirect to https", 1, &[S], || {
            let location = format!("https://127.0.0.1:{}/", closed_port());
            single(vec![redirect(302, &location)])
        }),
        case("redirect to another scheme", 1, &[S], || single(vec![redirect(302, "ftp://127.0.0.1/")])),
        case("garbage answer", 1, &[S], || single(vec![Step::Reply(b"hello\r\n\r\n".to_vec())])),
        case("closed without answer", 1, &[S], || single(vec![Step::Reply(Vec::new())])),
        case("head under the cap", 0, &[S], || {
            single(vec![reply(&format!("HTTP/1.1 200 OK\r\nX-Pad: {}", "a".repeat(8 * 1024)))])
        }),
        case("head over the cap", 1, &[S], || {
            single(vec![reply(&format!("HTTP/1.1 200 OK\r\nX-Pad: {}", "a".repeat(20 * 1024)))])
        }),
        case("refused", 1, &[], || Built { ui_port: Some(closed_port().to_string()), seen: Seen::default() }),
        case("port not digits", 1, &[], || Built { ui_port: Some("abc".to_string()), seen: Seen::default() }),
        case("port too large", 1, &[], || Built { ui_port: Some("65536".to_string()), seen: Seen::default() }),
        case("port with a space", 1, &[], || Built { ui_port: Some(" 1".to_string()), seen: Seen::default() }),
        case("leading zero port", 0, &[S], || {
            let mut built = single(vec![reply("HTTP/1.1 200 OK")]);
            built.ui_port = built.ui_port.map(|p| format!("000{p}"));
            built
        }),
        case("tab inside the port", 0, &[S], || {
            let mut built = single(vec![reply("HTTP/1.1 200 OK")]);
            built.ui_port = built.ui_port.map(|p| {
                let (a, b) = p.split_at(1);
                format!("{a}\t{b}")
            });
            built
        }),
    ];
    // Fetch follows 20 redirects and fails on the 21st.
    let mut twenty = vec![S.to_string()];
    twenty.extend(std::iter::repeat_n("/again".to_string(), 20));
    cases.push(Case {
        name: "20 redirects",
        expect: 0,
        seen: twenty.clone(),
        oracle: true,
        build: Box::new(|| {
            let mut steps: Vec<Step> = (0..20).map(|_| redirect(302, "/again")).collect();
            steps.push(reply("HTTP/1.1 200 OK"));
            single(steps)
        }),
    });
    cases.push(Case {
        name: "21 redirects",
        expect: 1,
        seen: twenty,
        oracle: true,
        build: Box::new(|| single((0..21).map(|_| redirect(302, "/again")).collect())),
    });
    // Not yet confirmed against Node's HTTP parser: checked for this binary only.
    for (name, bytes) in [
        ("100 then 200", b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n".to_vec()),
        ("bare LF head", b"HTTP/1.1 200 OK\nContent-Length: 0\n\n".to_vec()),
    ] {
        cases.push(Case {
            name,
            expect: 0,
            seen: vec![S.to_string()],
            oracle: false,
            build: Box::new(move || single(vec![Step::Reply(bytes.clone())])),
        });
    }
    cases.push(Case {
        name: "101",
        expect: 1,
        seen: vec![S.to_string()],
        oracle: false,
        build: Box::new(|| single(vec![Step::Reply(b"HTTP/1.1 101 Switching\r\n\r\n".to_vec())])),
    });
    cases
}

/// Runs `cmd` with only `UI_PORT` (and PATH) set; returns the exit code and the time it took.
fn run(mut cmd: Command, ui_port: Option<&str>) -> (i32, Duration) {
    cmd.env_clear().stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    if let Some(path) = std::env::var_os("PATH") {
        cmd.env("PATH", path);
    }
    if let Some(p) = ui_port {
        cmd.env("UI_PORT", p);
    }
    let started = Instant::now();
    let mut child = cmd.spawn().unwrap_or_else(|e| unreachable!("spawn: {e}"));
    let until = started + Duration::from_secs(15);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return (status.code().unwrap_or(-1), started.elapsed()),
            Ok(None) if Instant::now() < until => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return (-2, started.elapsed());
            }
        }
    }
}

fn ours(ui_port: Option<&str>) -> (i32, Duration) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_noevia-web-supervisor"));
    cmd.arg("--healthcheck");
    run(cmd, ui_port)
}

fn node_probe(ui_port: Option<&str>) -> (i32, Duration) {
    let mut cmd = Command::new("node");
    cmd.arg("-e").arg(NODE_PROBE);
    run(cmd, ui_port)
}

fn node_available() -> bool {
    Command::new("node")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn seen_of(built: &Built) -> Vec<String> {
    // The server records before it answers, so the list is complete once the probe has exited.
    built.seen.lock().map(|s| s.clone()).unwrap_or_default()
}

#[test]
fn healthcheck_cases() {
    let mut failures = Vec::new();
    for c in cases() {
        let built = (c.build)();
        let (code, took) = ours(built.ui_port.as_deref());
        let seen = seen_of(&built);
        if code != c.expect || seen != c.seen {
            failures.push(format!("{}: exit {code} (want {}), seen {seen:?} (want {:?})", c.name, c.expect, c.seen));
        }
        if took > Duration::from_secs(3) {
            failures.push(format!("{}: took {took:?}", c.name));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn healthcheck_gives_up_after_five_seconds() {
    let built = single(vec![Step::Hold(Duration::from_secs(8))]);
    let (code, took) = ours(built.ui_port.as_deref());
    assert_eq!(code, 1);
    assert!(took >= Duration::from_millis(4900), "{took:?}");
    assert!(took < Duration::from_millis(6500), "{took:?}");
    assert_eq!(seen_of(&built), vec![S.to_string()]);
}

#[test]
fn deadline_covers_the_whole_redirect_chain() {
    // A redirect answered at once, then a hold: one deadline for both, not five seconds each.
    let built = single(vec![redirect(302, "/next"), Step::Hold(Duration::from_secs(8))]);
    let (code, took) = ours(built.ui_port.as_deref());
    assert_eq!(code, 1);
    assert!(took < Duration::from_millis(6500), "{took:?}");
}

#[test]
fn healthcheck_ignores_the_supervisor_environment() {
    // `--healthcheck` never pairs, plans or refuses: no NOEVIA_FRONT, no Core, any directory.
    let built = single(vec![reply("HTTP/1.1 200 OK")]);
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_noevia-web-supervisor"));
    cmd.arg("--healthcheck").current_dir(std::env::temp_dir());
    assert_eq!(run(cmd, built.ui_port.as_deref()).0, 0);
}

#[test]
fn node_probe_agrees() {
    if !node_available() {
        assert!(
            std::env::var("WEB_SUPERVISOR_HEALTH_ORACLE_REQUIRED").as_deref() != Ok("1"),
            "node is not on PATH and WEB_SUPERVISOR_HEALTH_ORACLE_REQUIRED=1"
        );
        eprintln!("node_probe_agrees: skipped (no node on PATH)");
        return;
    }
    let mut failures = Vec::new();
    let mut compared = 0;
    for c in cases().into_iter().filter(|c| c.oracle) {
        let built = (c.build)();
        let (node, _) = node_probe(built.ui_port.as_deref());
        let node_seen = seen_of(&built);
        let built = (c.build)();
        let (rust, _) = ours(built.ui_port.as_deref());
        let rust_seen = seen_of(&built);
        compared += 1;
        if node != rust || node_seen != rust_seen {
            failures.push(format!(
                "{}: node exit {node} seen {node_seen:?}; rust exit {rust} seen {rust_seen:?}",
                c.name
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    assert!(compared >= 30, "only {compared} cases compared");
    // The five-second deadline, both sides.
    let built = single(vec![Step::Hold(Duration::from_secs(8))]);
    let (node, took) = node_probe(built.ui_port.as_deref());
    assert_eq!(node, 1);
    assert!(took >= Duration::from_millis(4900) && took < Duration::from_millis(7000), "{took:?}");
}
