//! Differential transcripts against the Node supervisor (noevia-core code-sandbox/supervisor.cjs).
//!
//! `CODE_SANDBOX_SUPERVISOR_ORACLE` names that file; without it this only says SKIP, and with
//! `CODE_SANDBOX_SUPERVISOR_ORACLE_REQUIRED=1` a missing oracle fails. Both supervisors run as
//! processes from the same environment (`CODE_HARNESS_COMMAND`, `WORKSPACE_ROOT`, `PORT`,
//! `CODE_SANDBOX_MAX_CONNECTIONS`, `CODE_SANDBOX_MAX_WALL_MS`) over one synthetic harness script,
//! and every client-visible byte of each scripted session is compared: the start line and its
//! remainder, environment filtering and the HOME fallback, cwd containment, every refusal, the
//! connection cap and its release, the wall limit, exit and large output.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const HARNESS: &str = r#"#!/bin/sh
orig_path=${PATH-unset}
PATH=/usr/bin:/bin
export PATH
case "$LANG" in
echo)
  IFS= read -r line
  printf 'line=%s\n' "$line"
  printf 'pwd=%s\n' "$(pwd)"
  printf 'home=%s\n' "${HOME-unset}"
  printf 'path=%s\n' "$orig_path"
  printf 'tmpdir=%s\n' "${TMPDIR-unset}"
  printf 'proxy=%s\n' "${HTTPS_PROXY-unset}"
  printf 'foo=%s\n' "${FOO-unset}"
  printf 'bridge=%s\n' "${SANDBOX_BRIDGE_IMPL-unset}"
  ;;
two)
  printf 'ready\n'
  IFS= read -r a
  printf 'got=%s\n' "$a"
  IFS= read -r b
  printf '%s|%s\n' "$a" "$b"
  ;;
hold)
  printf 'held\n'
  cat >/dev/null
  ;;
sleep) sleep 60 ;;
big) head -c 300000 /dev/zero | tr '\0' 'x' ;;
exit3) printf 'bye\n'; exit 3 ;;
*) printf 'unknown case %s\n' "${LANG-unset}" ;;
esac
"#;

struct Supervisor {
    child: Child,
    port: u16,
    banner: String,
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn launch(node: Option<&Path>, harness: &Path, root: &Path, extra: &[(&str, &str)]) -> Supervisor {
    let port = free_port();
    let mut command = match node {
        Some(oracle) => {
            let mut c = Command::new("node");
            c.arg(oracle);
            c
        }
        None => Command::new(env!("CARGO_BIN_EXE_code-sandbox-supervisor")),
    };
    command
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", "/sandbox-home")
        .env("CODE_HARNESS_COMMAND", harness)
        .env("WORKSPACE_ROOT", root)
        .env("PORT", port.to_string())
        .envs(extra.iter().copied())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    let mut child = command.spawn().unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut lines = BufReader::new(stdout).lines();
        if let Some(Ok(first)) = lines.next() {
            let _ = tx.send(first);
        }
        // Keep draining the log so the supervisor never blocks on a full pipe.
        for _ in lines {}
    });
    let banner = rx
        .recv_timeout(Duration::from_secs(20))
        .expect("supervisor did not start listening");
    // Both print the banner once the listener is bound. No probe connection: under a cap of one
    // a probe that Node has not yet seen close would take the only slot.
    Supervisor {
        child,
        port,
        banner: banner.replace(&format!(":{port},"), ":PORT,"),
    }
}

fn connect(port: u16) -> TcpStream {
    let s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    // Separate writes stay separate segments, so a split start line really arrives split.
    s.set_nodelay(true).unwrap();
    s
}

/// Reads exactly `want` bytes; the agent's own output is the signal to send the next bytes, so
/// no session depends on a fixed sleep (noevia#2727 item 3).
fn expect(s: &mut TcpStream, want: &str) -> String {
    let mut got = vec![0u8; want.len()];
    s.read_exact(&mut got).unwrap();
    let got = String::from_utf8_lossy(&got).into_owned();
    assert_eq!(got, want, "agent output out of step");
    got
}

/// Everything the client receives until EOF or reset (a reset is part of the transcript).
fn drain(mut s: TcpStream) -> String {
    let mut out = Vec::new();
    let mut buf = [0u8; 65536];
    let end = loop {
        match s.read(&mut buf) {
            Ok(0) => break "EOF",
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => break "RESET",
            Err(e) => {
                break if e.kind() == std::io::ErrorKind::WouldBlock {
                    "TIMEOUT"
                } else {
                    "ERROR"
                }
            }
        }
    };
    let text = String::from_utf8_lossy(&out);
    // Large outputs are summarised so a failure message stays readable.
    if text.len() > 4096 {
        let xs = text.bytes().filter(|b| *b == b'x').count();
        format!("<{} bytes, {xs} x> {end}", text.len())
    } else {
        format!("{text}{end}")
    }
}

fn start(cwd: &Path, env: &str) -> String {
    format!(
        r#"{{"noevia":"start","cwd":{},"env":{env}}}"#,
        json_string(&cwd.display().to_string())
    ) + "\n"
}

fn json_string(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn one(port: u16, payload: &str) -> String {
    let mut s = connect(port);
    s.write_all(payload.as_bytes()).unwrap();
    drain(s)
}

struct Layout {
    _tmp: tempfile::TempDir,
    harness: PathBuf,
    root: PathBuf,
    work: PathBuf,
    outside: PathBuf,
}

fn layout() -> Layout {
    let tmp = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(tmp.path()).unwrap();
    let harness = base.join("harness.sh");
    std::fs::write(&harness, HARNESS).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&harness, std::fs::Permissions::from_mode(0o755)).unwrap();
    let root = base.join("workspaces");
    let work = root.join("task");
    std::fs::create_dir_all(&work).unwrap();
    let outside = base.join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, root.join("escape")).unwrap();
    Layout {
        _tmp: tmp,
        harness,
        root,
        work,
        outside,
    }
}

/// The default-limit sessions, as (name, transcript).
fn default_sessions(sup: &Supervisor, l: &Layout) -> Vec<(String, String)> {
    let p = sup.port;
    let long = "t".repeat(5000);
    let mut out = vec![("banner".to_owned(), sup.banner.clone())];
    let mut add = |name: &str, transcript: String| out.push((name.to_owned(), transcript));
    add(
        "echo-allowlist",
        one(
            p,
            &(start(
                &l.work,
                r#"{"LANG":"echo","HOME":"/h","PATH":"/p","TMPDIR":"/t","HTTPS_PROXY":"http://proxy.invalid:1","FOO":"x","SANDBOX_BRIDGE_IMPL":"rust"}"#,
            ) + "hello\n"),
        ),
    );
    add(
        "echo-home-fallback",
        one(p, &(start(&l.work, r#"{"LANG":"echo"}"#) + "a\n")),
    );
    add(
        "echo-empty-home",
        one(p, &(start(&l.work, r#"{"LANG":"echo","HOME":""}"#) + "a\n")),
    );
    add(
        "echo-long-and-nonstring-values",
        one(
            p,
            &(start(
                &l.work,
                &format!(r#"{{"LANG":"echo","TMPDIR":"{long}","PATH":7,"FOO":null}}"#),
            ) + "a\n"),
        ),
    );
    add(
        "echo-root-itself",
        one(p, &(start(&l.root, r#"{"LANG":"echo"}"#) + "r\n")),
    );
    add(
        "echo-env-missing",
        one(
            p,
            &(format!(
                r#"{{"noevia":"start","cwd":{}}}"#,
                json_string(&l.work.display().to_string())
            ) + "\nx\n"),
        ),
    );
    {
        // ACP bytes sent only after the agent is running, each one after the agent read the last.
        let mut s = connect(p);
        s.write_all(start(&l.work, r#"{"LANG":"two"}"#).as_bytes())
            .unwrap();
        let mut seen = expect(&mut s, "ready\n");
        s.write_all(b"first\n").unwrap();
        seen += &expect(&mut s, "got=first\n");
        s.write_all(b"second\n").unwrap();
        add("later-acp-bytes", seen + &drain(s));
    }
    {
        // The start line itself split over writes. A coalesced segment still yields the same
        // transcript, so this can only lose coverage under load, never fail falsely.
        let line = start(&l.work, r#"{"LANG":"two"}"#) + "one\ntwo\n";
        let (a, b) = line.split_at(10);
        let mut s = connect(p);
        s.write_all(a.as_bytes()).unwrap();
        std::thread::sleep(Duration::from_millis(150));
        s.write_all(b.as_bytes()).unwrap();
        add("split-start-line", drain(s));
    }
    add("not-json", one(p, "not json\n"));
    add("other-message", one(p, "{\"noevia\":\"other\"}\n"));
    add("json-array", one(p, "[1,2]\n"));
    add("json-null", one(p, "null\n"));
    add(
        "outside-root",
        one(p, &start(&l.outside, r#"{"LANG":"echo"}"#)),
    );
    add(
        "symlink-escape",
        one(p, &start(&l.root.join("escape"), r#"{"LANG":"echo"}"#)),
    );
    add(
        "missing-cwd",
        one(p, &start(&l.root.join("nope"), r#"{"LANG":"echo"}"#)),
    );
    add(
        "empty-cwd",
        one(p, "{\"noevia\":\"start\",\"cwd\":\"\",\"env\":{}}\n"),
    );
    add(
        "dotdot-cwd",
        one(
            p,
            &start(&l.work.join("../../outside"), r#"{"LANG":"echo"}"#),
        ),
    );
    add("exit-code", one(p, &start(&l.work, r#"{"LANG":"exit3"}"#)));
    add("large-output", one(p, &start(&l.work, r#"{"LANG":"big"}"#)));
    add("oversize-start", one(p, &"a".repeat(70_000)));
    out
}

fn cap_sessions(sup: &Supervisor, l: &Layout) -> Vec<(String, String)> {
    let p = sup.port;
    let mut held = connect(p);
    held.write_all(start(&l.work, r#"{"LANG":"hold"}"#).as_bytes())
        .unwrap();
    let mut first = [0u8; 5];
    held.read_exact(&mut first).unwrap();
    assert_eq!(&first, b"held\n");
    let refused = one(p, &(start(&l.work, r#"{"LANG":"echo"}"#) + "x\n"));
    held.shutdown(Shutdown::Both).unwrap();
    drop(held);
    // The slot is released on close; retry rather than racing the close event. The deadline is
    // generous for a loaded runner: a release is normally seen on the first or second try.
    let deadline = Instant::now() + Duration::from_secs(30);
    let after = loop {
        let t = one(p, &(start(&l.work, r#"{"LANG":"echo"}"#) + "y\n"));
        if t.starts_with("line=") || Instant::now() > deadline {
            break t;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    vec![
        ("cap-refusal".to_owned(), refused),
        ("cap-released".to_owned(), after),
    ]
}

fn wall_sessions(sup: &Supervisor, l: &Layout) -> Vec<(String, String)> {
    // The agent sleeps 60 s under a 400 ms limit: anything well short of 60 s is the limit at
    // work, so a loaded runner cannot fail this (noevia#2727 item 3).
    let started = Instant::now();
    let t = one(sup.port, &start(&l.work, r#"{"LANG":"sleep"}"#));
    let took = started.elapsed();
    assert!(
        took < Duration::from_secs(30),
        "wall limit did not stop the agent ({took:?})"
    );
    vec![("wall-limit".to_owned(), t)]
}

fn run_all(node: Option<&Path>, l: &Layout) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let sup = launch(node, &l.harness, &l.root, &[]);
    out.extend(default_sessions(&sup, l));
    drop(sup);
    let sup = launch(
        node,
        &l.harness,
        &l.root,
        &[("CODE_SANDBOX_MAX_CONNECTIONS", "1")],
    );
    out.extend(cap_sessions(&sup, l));
    drop(sup);
    let sup = launch(
        node,
        &l.harness,
        &l.root,
        &[("CODE_SANDBOX_MAX_WALL_MS", "400")],
    );
    out.extend(wall_sessions(&sup, l));
    out
}

#[test]
fn transcripts_match_the_node_supervisor() {
    let Some(oracle) = std::env::var_os("CODE_SANDBOX_SUPERVISOR_ORACLE") else {
        assert_ne!(
            std::env::var("CODE_SANDBOX_SUPERVISOR_ORACLE_REQUIRED").as_deref(),
            Ok("1"),
            "CODE_SANDBOX_SUPERVISOR_ORACLE is required here"
        );
        eprintln!(
            "SKIP node parity: set CODE_SANDBOX_SUPERVISOR_ORACLE to code-sandbox/supervisor.cjs"
        );
        return;
    };
    let oracle = PathBuf::from(oracle);
    assert!(oracle.is_file(), "{} is not a file", oracle.display());
    let l = layout();
    let expected = run_all(Some(&oracle), &l);
    let actual = run_all(None, &l);
    assert_eq!(expected.len(), actual.len());
    let mut differ = Vec::new();
    for ((name, node), (_, rust)) in expected.iter().zip(&actual) {
        eprintln!("{name}: {}", node.replace('\n', "\\n"));
        if node != rust {
            differ.push(format!("{name}:\n  node: {node:?}\n  rust: {rust:?}"));
        }
    }
    // The oracle must have exercised what it claims to.
    let get = |name: &str| {
        expected
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, t)| t.as_str())
            .unwrap_or("")
    };
    assert!(get("echo-allowlist").contains("line=hello\n"));
    assert_eq!(
        get("later-acp-bytes"),
        "ready\ngot=first\nfirst|second\nEOF"
    );
    assert_eq!(get("split-start-line"), "ready\ngot=one\none|two\nEOF");
    assert!(get("cap-refusal").contains("already running 1 agents"));
    assert!(get("wall-limit").contains("time limit"));
    assert!(
        differ.is_empty(),
        "{} of {} sessions differ:\n{}",
        differ.len(),
        expected.len(),
        differ.join("\n")
    );
    eprintln!("node parity: {} sessions agree", expected.len());
}
