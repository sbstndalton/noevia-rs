#![allow(dead_code)]
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{path::Path, process::Command, time::Duration};
#[path = "../../../../tools/qualification/process.rs"]
pub mod process;
pub const NOW: i64 = 1800000000000;
pub const ACTOR: &str = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
pub fn unhex(s: &str) -> Result<Vec<u8>, String> {
    if !s.len().is_multiple_of(2) {
        return Err("odd hex".into());
    }
    s.as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|s| {
            std::str::from_utf8(s)
                .map_err(|e| e.to_string())
                .and_then(|s| u8::from_str_radix(s, 16).map_err(|e| e.to_string()))
        })
        .collect()
}
pub fn at<'a>(v: &'a Value, key: &str) -> &'a Value {
    v.get(key).unwrap_or(&Value::Null)
}
pub fn put(v: &mut Value, key: &str, value: Value) -> Result<(), String> {
    v.as_object_mut()
        .ok_or("case object absent")?
        .insert(key.into(), value);
    Ok(())
}
fn base() -> Value {
    json!({"name":"base","file":null,"allowed":null,"feature":"true","env":{},"routeEnv":{},"featureUpdate":null,"directFeature":null,"auditAbort":false,"readError":false,"method":"PUT","path":"/api/routing-mode","role":"member","body":hex(br#"{"mode":"local"}"#)})
}
pub fn cases() -> Result<Vec<Value>, String> {
    let mut cases = Vec::new();
    let mut bodies: Vec<Vec<u8>> = [
        "",
        "{",
        "null",
        "[]",
        "[1]",
        "true",
        "1e999",
        "\"x\"",
        "{}",
        r#"{"mode":null}"#,
        r#"{"mode":"LOCAL"}"#,
        r#"{"mode":"cloud"}"#,
        r#"{"mode":"hybrid"}"#,
        r#"{"mode":"local","whenSensitive":null}"#,
        r#"{"mode":"local","whenSensitive":"local"}"#,
        r#"{"mode":"local","cloud":null}"#,
        r#"{"mode":"local","cloud":[]}"#,
        r#"{"mode":"local","cloud":{"providerId":1}}"#,
        r#"{"mode":"local","cloud":{"fast":false}}"#,
        r#"{"mode":"local","cloud":{"smart":" \t "}}"#,
        r#"{"mode":"local","unknown":1e999}"#,
        r#"{"mode":"local","\ud800":"\udfff"}"#,
        r#"{"mode":"local","unknown":{"mode":false}}"#,
        r#"{"mode":"hybrid","mode":null}"#,
        r#"{"mode":"hybrid","\u006dode":"local"}"#,
        r#"{"mode":"local","mode":false}"#,
        r#"{"mode":"local","cloud":{"fast":"x","fast":false}}"#,
        r#"{"mode":"cloud","cloud":{"providerId":"p","code":"a/b:@+-"}}"#,
        r#"{"mode":"cloud","cloud":{"providerId":"p","fast":"m"}}"#,
        r#"{"mode":"cloud","cloud":{"providerId":"p","smart":"m"}}"#,
        r#"{"mode":"local","cloud":{"providerId":"p\n"}}"#,
        r#"{"mode":"local","cloud":{"smart":"\ud800"}}"#,
        r#"{"mode":"local","__proto__":{"cloud":null}}"#,
    ]
    .into_iter()
    .map(|s| s.as_bytes().to_vec())
    .collect();
    for ch in [
        '\u{9}', '\u{b}', '\u{c}', '\u{d}', ' ', '\u{a0}', '\u{1680}', '\u{2000}', '\u{2007}',
        '\u{2028}', '\u{2029}', '\u{202f}', '\u{205f}', '\u{3000}', '\u{feff}', '\u{85}',
        '\u{180e}',
    ] {
        bodies.push(
            json!({"mode":"cloud","cloud":{"providerId":"p","fast":format!("{ch}m{ch}")}})
                .to_string()
                .into_bytes(),
        );
    }
    for len in [0, 1, 79, 80, 81, 199, 200, 201] {
        bodies.push(
            json!({"mode":"hybrid","cloud":{"providerId":"p".repeat(len),"smart":"m".repeat(len)}})
                .to_string()
                .into_bytes(),
        );
    }
    for role in ["fast", "smart", "code"] {
        for len in [199, 200, 201] {
            let mut cloud = json!({"providerId":"p"});
            put(&mut cloud, role, json!("m".repeat(len)))?;
            bodies.push(
                json!({"mode":"cloud","cloud":cloud})
                    .to_string()
                    .into_bytes(),
            );
        }
    }
    bodies.push(b"{\"mode\":\"local\",\"extra\":\"\xff\"}".to_vec());
    for (i, body) in bodies.into_iter().enumerate() {
        for allowed in [None, Some("[]"), Some("[\"hybrid\",\"local\"]")] {
            let mut c = base();
            put(&mut c, "name", json!(format!("account/{i}/{allowed:?}")))?;
            put(&mut c, "body", json!(hex(&body)))?;
            put(&mut c, "allowed", json!(allowed))?;
            cases.push(c);
        }
    }
    for list in [
        "null",
        "[]",
        r#"["hybrid","local","local"]"#,
        r#"["unknown"]"#,
        r#"[true]"#,
        "{}",
        "1e999",
    ] {
        for role in ["admin", "member"] {
            let mut c = base();
            put(&mut c, "name", json!(format!("allowed/{list}/{role}")))?;
            put(&mut c, "path", json!("/api/routing-mode/allowed"))?;
            put(&mut c, "role", json!(role))?;
            put(
                &mut c,
                "body",
                json!(hex(format!("{{\"allowed\":{list}}}").as_bytes())),
            )?;
            cases.push(c);
        }
    }
    for file in [
        None,
        Some("{"),
        Some("null"),
        Some("[]"),
        Some(r#"{"mode":"hybrid","whenSensitive":"bad","cloud":null}"#),
        Some(r#"{"mode":"cloud","cloud":{"providerId":"p","fast":" m "}}"#),
        Some(r#"{"mode":"bad","cloud":{"providerId":false,"smart":1e999}}"#),
        Some(r#"{"mode":"local","unknown":"\ud800"}"#),
    ] {
        for allowed in [
            None,
            Some("bad"),
            Some("null"),
            Some("[]"),
            Some("[\"local\"]"),
            Some("[1,\"hybrid\",\"unknown\",\"hybrid\"]"),
        ] {
            let mut c = base();
            put(&mut c, "name", json!(format!("get/{file:?}/{allowed:?}")))?;
            put(&mut c, "method", json!("GET"))?;
            put(&mut c, "file", json!(file.map(|s| hex(s.as_bytes()))))?;
            put(&mut c, "allowed", json!(allowed))?;
            cases.push(c);
        }
    }
    for path in ["/api/routing-mode", "/api/routing-mode/allowed"] {
        for method in ["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE", "OPTIONS"] {
            for feature in ["true", "false"] {
                for role in ["admin", "member"] {
                    let mut c = base();
                    put(
                        &mut c,
                        "name",
                        json!(format!("method/{path}/{method}/{feature}/{role}")),
                    )?;
                    put(&mut c, "path", json!(path))?;
                    put(&mut c, "method", json!(method))?;
                    put(&mut c, "feature", json!(feature))?;
                    put(&mut c, "role", json!(role))?;
                    put(&mut c, "body", json!(hex(b"{")))?;
                    cases.push(c);
                }
            }
        }
    }
    for abort in [false, true] {
        let mut c = base();
        put(&mut c, "name", json!(format!("audit/{abort}")))?;
        put(&mut c, "path", json!("/api/routing-mode/allowed"))?;
        put(&mut c, "role", json!("admin"))?;
        put(&mut c, "auditAbort", json!(abort))?;
        put(
            &mut c,
            "body",
            json!(hex(br#"{"allowed":["cloud","local"]}"#)),
        )?;
        cases.push(c);
    }
    let mut c = base();
    put(&mut c, "name", json!("read-error"))?;
    put(&mut c, "readError", json!(true))?;
    cases.push(c);
    for extra in [super_limit() - 35, super_limit()] {
        let mut c = base();
        put(&mut c, "name", json!(format!("body-size/{extra}")))?;
        put(
            &mut c,
            "body",
            json!(hex(json!({"mode":"local","extra":"x".repeat(extra)})
                .to_string()
                .as_bytes())),
        )?;
        cases.push(c);
    }
    Ok(cases)
}
fn super_limit() -> usize {
    1024 * 1024
}
pub fn source(core: &Path, expected: &str) -> Result<Value, String> {
    let git = |args: &[&str]| -> Result<String, String> {
        let mut command = Command::new("git");
        command.arg("-C").arg(core).args(args);
        let o = process::bounded(command, Vec::new(), Duration::from_secs(10))?;
        if !o.status.success() {
            return Err("source git check failed".into());
        }
        Ok(String::from_utf8_lossy(&o.stdout).trim().into())
    };
    if expected.len() != 40 || git(&["rev-parse", "HEAD"])? != expected {
        return Err("actual Node source SHA mismatch".into());
    }
    if !git(&["status", "--porcelain", "--untracked-files=no"])?.is_empty() {
        return Err("actual Node source must be clean".into());
    }
    let mut hashes = serde_json::Map::new();
    for file in [
        "server/routing-modes.cjs",
        "server/routes/routing-mode.cjs",
        "server/features.cjs",
        "tools/contract-corpus/serve.cjs",
    ] {
        hashes.insert(
            file.into(),
            json!(format!(
                "{:x}",
                Sha256::digest(std::fs::read(core.join(file)).map_err(|e| e.to_string())?)
            )),
        );
    }
    Ok(
        json!({"core":expected,"archiveSha256":include_str!("../../../../contracts/http/core.sha256").trim(),"sources":hashes}),
    )
}
pub fn oracle(core: &Path, cases: &[Value]) -> Result<Value, String> {
    let node = std::env::var("NOEVIA_ORACLE_NODE").unwrap_or_else(|_| "node".into());
    let mut command = Command::new(&node);
    command.arg("--version");
    let version = process::bounded(command, Vec::new(), Duration::from_secs(10))?;
    if !version.status.success() || String::from_utf8_lossy(&version.stdout).trim() != "v22.23.3" {
        return Err("oracle requires Node22.23.3".into());
    }
    let mut command = Command::new(node);
    command
        .arg(core.join("tools/contract-corpus/serve.cjs"))
        .arg("--routing-mode-oracle");
    let o = process::bounded(
        command,
        process::json_input(&json!({"cases":cases}))?,
        Duration::from_secs(10),
    )?;
    if !o.status.success() {
        return Err(format!(
            "actual Node oracle failed: {}",
            String::from_utf8_lossy(&o.stderr)
        ));
    }
    serde_json::from_slice(&o.stdout).map_err(|e| e.to_string())
}
