use serde_json::{json, Value};
use std::io::{Read, Write};
type R<T> = std::result::Result<T, Box<dyn std::error::Error>>;
fn s<'a>(v: &'a Value, k: &str) -> R<&'a str> {
    v.get(k)
        .and_then(Value::as_str)
        .ok_or_else(|| "string field".into())
}
fn run() -> R<()> {
    if std::env::args().nth(1).as_deref() != Some("json-v1") {
        return Err("protocol".into());
    }
    let mut raw = Vec::new();
    std::io::stdin()
        .take(4 * 1024 * 1024 + 1)
        .read_to_end(&mut raw)?;
    if raw.len() > 4 * 1024 * 1024 {
        return Err("capacity".into());
    }
    let v: Value = serde_json::from_slice(&raw)?;
    let out = dispatch(&v)?;
    let body = serde_json::to_vec(&out)?;
    if body.len() > 8 * 1024 * 1024 {
        return Err("capacity".into());
    }
    std::io::stdout().write_all(&body)?;
    Ok(())
}
fn main() {
    if run().is_err() {
        eprintln!("unavailable");
        std::process::exit(2);
    }
}
use diary_backup_format::*;
fn dispatch(v: &Value) -> R<Value> {
    let limits = Limits {
        manifest_bytes: 4 * 1024 * 1024,
        files: 50000,
        directories: 50000,
        json_depth: 64,
        json_nodes: 500000,
        string_bytes: 1024 * 1024,
        payload_bytes: 1024 * 1024 * 1024,
    };
    let raw = s(v, "body")?.as_bytes();
    let body = PythonJson::parse(raw, limits)?.canonical_bytes(limits)?;
    Ok(json!({"body":String::from_utf8(body)?}))
}
