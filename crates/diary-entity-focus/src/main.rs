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
use diary_entity_focus::*;
fn ent(v: &Value) -> R<Entity> {
    Ok(Entity {
        name: s(v, "name")?.into(),
        keys: serde_json::from_value(v["keys"].clone())?,
    })
}
fn dispatch(v: &Value) -> R<Value> {
    Ok(match s(v, "op")? {
        "asked" => json!(asked_entities(s(v, "text")?)?
            .iter()
            .map(|e| json!({"name":e.name,"keys":e.keys}))
            .collect::<Vec<_>>()),
        "classify" => {
            let (k, t) = classify(s(v, "text")?, &ent(&v["entity"])?)?;
            json!([k.map(Kind::as_str), t])
        }
        "people" => {
            let es = v["asked"]
                .as_array()
                .map(|a| a.iter().map(ent).collect::<R<Vec<_>>>())
                .transpose()?;
            json!(identified_people(s(v, "text")?, es.as_deref())?)
        }
        _ => return Err("op".into()),
    })
}
