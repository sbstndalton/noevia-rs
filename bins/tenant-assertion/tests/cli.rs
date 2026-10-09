//! The binary's stdin/stdout contract: decisions, bad requests, and no echo of secrets.

use std::io::Write;
use std::process::{Command, Stdio};

const BIN: &str = env!("CARGO_BIN_EXE_tenant-assertion");
const KEY: &str = "synthetic-cli-key-DO-NOT-ECHO";

fn run(args: &[&str], stdin: &[u8]) -> (i32, String, String) {
    let Ok(mut child) = Command::new(BIN)
        .args(args)
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    else {
        return (-1, String::new(), "spawn failed".into());
    };
    if let Some(mut s) = child.stdin.take() {
        let _ = s.write_all(stdin);
    }
    let Ok(out) = child.wait_with_output() else {
        return (-1, String::new(), "wait failed".into());
    };
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn request(assertion: &str, extra: &str) -> String {
    format!(
        r#"{{"op":"verify","key":"{KEY}","user_id":"u","assertion":"{assertion}","method":"GET","path":"/","query_hex":"","body_hash":"stream","storage":"","legacy_owner":"","blocked":"","now":"1.0"{extra}}}"#
    )
}

fn signed() -> String {
    let r = tenant_assertion::Request {
        key: KEY,
        user_id: "u",
        assertion: "",
        method: "GET",
        path: "/",
        query: b"",
        body_hash: "stream",
        storage: "",
        legacy_owner: "",
        blocked: "",
        now: 1.0,
    };
    tenant_assertion::sign(&r, 1, "0123456789abcdef0123456789abcdef").unwrap_or_default()
}

#[test]
fn accepts_and_rejects_with_exit_codes() {
    assert_eq!(
        run(&["check"], request(&signed(), "").as_bytes()),
        (0, "accept\n".into(), String::new())
    );
    let (code, out, _) = run(&["check"], request("v2.1.x", "").as_bytes());
    assert_eq!(
        (code, out.as_str()),
        (1, "reject: missing or malformed assertion\n")
    );
}

#[test]
fn bad_requests_exit_2_with_nothing_on_stdout_and_no_secret_echo() {
    let bad = [
        request(&signed(), r#","extra":"x""#),
        request(&signed(), "").replace(r#","blocked":"""#, ""),
        request(&signed(), "").replace(r#""now":"1.0""#, r#""now":1.0"#),
        request(&signed(), "").replace(r#""query_hex":"""#, r#""query_hex":"0""#),
        request(&signed(), "").replace(r#""op":"verify""#, r#""op":"sign""#),
        format!("[\"{KEY}\"]"),
        format!("{{\"key\":\"{KEY}\""),
        "x".repeat(1024 * 1024 + 2),
    ];
    for input in bad {
        let (code, out, err) = run(&["check"], input.as_bytes());
        assert_eq!(code, 2, "{err}");
        assert!(out.is_empty());
        assert!(!err.contains(KEY) && !out.contains(KEY));
        assert_eq!(err, "tenant-assertion: bad request\n");
    }
}

#[test]
fn secret_ref_op_and_usage() {
    let r = tenant_assertion::storage_secret_ref(KEY, "u", "s").unwrap_or_default();
    let ok =
        format!(r#"{{"op":"secret_ref","key":"{KEY}","user_id":"U","secret":"s","ref":"{r}"}}"#);
    assert_eq!(run(&["check"], ok.as_bytes()).0, 0);
    let no = ok.replace(r#""secret":"s""#, r#""secret":"t""#);
    assert_eq!(run(&["check"], no.as_bytes()).1, "reject: bad signature\n");
    assert_eq!(run(&[], b"").0, 2);
    assert_eq!(run(&["check", KEY], b"").0, 2);
    assert_eq!(run(&["self-test"], b""), (0, "ok\n".into(), String::new()));
}
