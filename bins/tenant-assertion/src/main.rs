//! `tenant-assertion check`: one request as a JSON object on stdin, the decision on stdout
//! (noevia-services' diary/agent/tenant_assertion.py, TENANT_ASSERTION_IMPL=rust).
//!
//! Input (stdin only; the key never travels in argv or the environment):
//!   {"op":"verify","key":..,"user_id":..,"assertion":..,"method":..,"path":..,
//!    "query_hex":..,"body_hash":..,"storage":..,"legacy_owner":..,"blocked":..,"now":"<float>"}
//!   {"op":"secret_ref","key":..,"user_id":..,"secret":..,"ref":..}
//! `now` is a decimal string (Python `repr(float)`), parsed with correct rounding.
//!
//! Output: `accept` and exit 0, or `reject: <reason>` and exit 1. A request that is not exactly
//! that shape (unknown or missing field, wrong type, oversized): nothing on stdout,
//! `tenant-assertion: bad request` on stderr, exit 2. I/O failure: exit 3. Nothing derived from
//! the input is ever written, so no key, secret or signature can leak through output.
//!
//! `tenant-assertion self-test`: checks a built-in synthetic request is accepted and a tampered
//! copy refused; prints `ok`. For image smoke tests.

#![forbid(unsafe_code)]

use std::io::{Read, Write};
use std::process::ExitCode;

use serde_json::{Map, Value};
use tenant_assertion::{secret_ref_matches, sign, verify, Reject, Request};
use zeroize::Zeroize;

const USAGE: &str = "usage: tenant-assertion check < request.json | tenant-assertion self-test";
/// Well above any real request (headers are capped far lower by uvicorn).
const MAX_INPUT_BYTES: u64 = 1024 * 1024;

fn fail(msg: &str, code: u8) -> ExitCode {
    let _ = writeln!(std::io::stderr().lock(), "tenant-assertion: {msg}");
    ExitCode::from(code)
}

fn say(line: &str, code: u8) -> ExitCode {
    let mut out = std::io::stdout().lock();
    if writeln!(out, "{line}").and_then(|()| out.flush()).is_err() {
        return fail("could not write to stdout", 3);
    }
    ExitCode::from(code)
}

fn decision(result: Result<(), Reject>) -> ExitCode {
    match result {
        Ok(()) => say("accept", 0),
        Err(r) => say(&format!("reject: {}", r.reason()), 1),
    }
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    let digit = |b: u8| match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        _ => None,
    };
    s.as_bytes()
        .chunks(2)
        .map(|p| Some(digit(*p.first()?)? << 4 | digit(*p.get(1)?)?))
        .collect()
}

/// Exactly `fields`, all strings; anything else is a bad request.
fn strings<'a>(obj: &'a Map<String, Value>, fields: &[&str]) -> Option<Vec<&'a str>> {
    if obj.len() != fields.len() + 1 {
        return None;
    }
    fields.iter().map(|f| obj.get(*f)?.as_str()).collect()
}

fn run(obj: &Map<String, Value>) -> Option<ExitCode> {
    match obj.get("op")?.as_str()? {
        "verify" => {
            let f = strings(
                obj,
                &[
                    "key",
                    "user_id",
                    "assertion",
                    "method",
                    "path",
                    "query_hex",
                    "body_hash",
                    "storage",
                    "legacy_owner",
                    "blocked",
                    "now",
                ],
            )?;
            let [key, user_id, assertion, method, path, query_hex, body_hash, storage, legacy_owner, blocked, now] =
                f.as_slice()
            else {
                return None;
            };
            let query = unhex(query_hex)?;
            let now: f64 = now.parse().ok()?;
            let req = Request {
                key,
                user_id,
                assertion,
                method,
                path,
                query: &query,
                body_hash,
                storage,
                legacy_owner,
                blocked,
                now,
            };
            Some(decision(verify(&req)))
        }
        "secret_ref" => {
            let f = strings(obj, &["key", "user_id", "secret", "ref"])?;
            let [key, user_id, secret, secret_ref] = f.as_slice() else {
                return None;
            };
            Some(decision(
                if secret_ref_matches(key, user_id, secret, secret_ref) {
                    Ok(())
                } else {
                    Err(Reject::BadSignature)
                },
            ))
        }
        _ => None,
    }
}

fn self_test() -> ExitCode {
    let mut req = Request {
        key: "synthetic-self-test-key",
        user_id: "11111111-1111-4111-8111-111111111111",
        assertion: "",
        method: "post",
        path: "/api/diary/entries",
        query: b"a=1",
        body_hash: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        storage: "",
        legacy_owner: "",
        blocked: "",
        now: 1_700_000_000.5,
    };
    let Some(signed) = sign(&req, 1_700_000_000, "0123456789abcdef0123456789abcdef") else {
        return fail("self-test failed", 1);
    };
    req.assertion = &signed;
    let accepted = verify(&req).is_ok();
    let tampered = Request {
        path: "/api/diary/entrieS",
        ..req
    };
    if accepted && verify(&tampered) == Err(Reject::BadSignature) {
        say("ok", 0)
    } else {
        fail("self-test failed", 1)
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("check") if args.len() == 1 => {}
        Some("self-test") if args.len() == 1 => return self_test(),
        _ => return fail(USAGE, 2),
    }
    let mut input = Vec::new();
    if std::io::stdin()
        .lock()
        .take(MAX_INPUT_BYTES + 1)
        .read_to_end(&mut input)
        .is_err()
    {
        input.zeroize();
        return fail("could not read stdin", 3);
    }
    if input.len() as u64 > MAX_INPUT_BYTES {
        input.zeroize();
        return fail("bad request", 2);
    }
    let parsed: Option<Value> = serde_json::from_slice(&input).ok();
    input.zeroize();
    let code = match parsed.as_ref().and_then(Value::as_object) {
        Some(obj) => run(obj).unwrap_or_else(|| fail("bad request", 2)),
        None => fail("bad request", 2),
    };
    // Best effort: serde_json's Value has no zeroize; the process exits right after.
    drop(parsed);
    code
}
