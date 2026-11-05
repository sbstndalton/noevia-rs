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
use diary_index_transform::*;
fn state(v: &Value) -> R<IndexFile> {
    Ok(IndexFile {
        preamble: serde_json::from_value(v["preamble"].clone())?,
        month_links: serde_json::from_value(v["month_links"].clone())?,
        sections: serde_json::from_value(v["sections"].clone())?,
    })
}
fn value(i: &IndexFile) -> Value {
    json!({"preamble":i.preamble,"month_links":i.month_links,"sections":i.sections})
}
fn ops(v: &Value, timeline: bool) -> R<Vec<EditOp>> {
    let mut out = Vec::new();
    for x in v.as_array().ok_or("ops")? {
        // Ignore unknown/missing actions before inspecting fields Python never uses.
        let Some(action) = x.get("action").and_then(Value::as_str) else {
            continue;
        };
        if !matches!(action, "add" | "edit") && (timeline || action != "resolve") {
            continue;
        }
        let text = match x.get("text") {
            None | Some(Value::Null) | Some(Value::Bool(false)) => "",
            Some(Value::String(text)) => text,
            _ => return Err("text".into()),
        };
        if text
            .trim_matches(|c: char| c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c))
            .is_empty()
        {
            continue;
        }
        let replacement = if action == "edit" {
            x.get("replacement")
                .map(|r| -> R<Option<String>> {
                    if r.is_null() {
                        Ok(None)
                    } else {
                        Ok(Some(r.as_str().ok_or("replacement")?.into()))
                    }
                })
                .transpose()?
        } else {
            None
        };
        // Timeline edits with no replacement are also no-ops, regardless of date.
        if timeline
            && action == "edit"
            && !replacement
                .as_ref()
                .and_then(|r| r.as_deref())
                .is_some_and(|r| !r.is_empty())
        {
            continue;
        }
        let date = if timeline {
            match x.get("date") {
                None | Some(Value::Null) => None,
                Some(Value::String(d)) if d.is_empty() => None,
                Some(Value::String(d)) => Some(d.clone()),
                _ => return Err("date".into()),
            }
        } else {
            None
        };
        out.push(EditOp {
            action: action.into(),
            text: text.into(),
            replacement,
            date,
        });
    }
    Ok(out)
}
fn dispatch(v: &Value) -> R<Value> {
    Ok(match s(v, "op")? {
        "parse" => value(&parse_index(v["text"].as_str())?),
        "render" => json!(state(&v["state"])?.render()?),
        "register" => {
            let mut i = state(&v["state"])?;
            let changed = register_month(&mut i, s(v, "label")?, s(v, "filename")?)?;
            json!({"state":value(&i),"changed":changed})
        }
        "apply" => {
            let mut i = state(&v["state"])?;
            let changed = apply_index_edits(
                &mut i,
                &ops(&v["questions"], false)?,
                &ops(&v["timeline"], true)?,
                s(v, "today")?,
            )?;
            json!({"state":value(&i),"changed":changed})
        }
        _ => return Err("op".into()),
    })
}
