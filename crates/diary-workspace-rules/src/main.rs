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
use diary_workspace_rules::*;
fn dispatch(v: &Value) -> R<Value> {
    Ok(match s(v, "op")? {
        "rel_path" => match rel_path(
            s(v, "path")?,
            v["allow_root"].as_bool().ok_or("allow_root")?,
        ) {
            Ok(p) => json!({"path":p}),
            Err(e) => json!({"error":format!("{e:?}")}),
        },
        "replaced_name" => json!(replaced_name(
            s(v, "path")?,
            v["now"].as_f64().ok_or("now")?
        )?),
        "protected" => {
            let c = &v["config"];
            json!(ProtectionConfig {
                remote_root: s(c, "remote_root")?.into(),
                entries_prefix: s(c, "entries_prefix")?.into(),
                monthly_prefix: s(c, "monthly_prefix")?.into(),
                month_file_template: s(c, "month_file_template")?.into(),
                index_file: s(c, "index_file")?.into(),
                month_names: serde_json::from_value(c["month_names"].clone())?
            }
            .protected(s(v, "path")?)?)
        }
        _ => return Err("op".into()),
    })
}
