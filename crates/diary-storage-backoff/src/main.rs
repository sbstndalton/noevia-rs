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
fn dispatch(v: &Value) -> R<Value> {
    let _ = s(v, "op")?;
    let wall = v["wall"].as_f64().ok_or("wall")?;
    let default = v["default"].as_f64().ok_or("default")?;
    // Local/naive HTTP dates are resolved by the Python host; explicit-zone dates and deltas stay native.
    let local = v["local_timestamp"].as_f64();
    let out = diary_storage_backoff::parse_retry_after_with_local_date(
        v["header"].as_str(),
        default,
        wall,
        |_| local,
    );
    Ok(json!(out.seconds))
}
