//! Lossless legacy summary data on the actual CLI wire and receipt replay.
use diary_backup_format::{Limits, PythonJson};
use diary_workspace_transaction::{execute, write_reply, Request};
use rusqlite::{params, Connection};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    error::Error,
    io::Write,
    process::{Command, Stdio},
};
mod common;
fn limits() -> Limits {
    Limits {
        manifest_bytes: 1024 * 1024,
        files: 0,
        directories: 0,
        json_depth: 32,
        json_nodes: 10000,
        string_bytes: 1024 * 1024,
        payload_bytes: 1024 * 1024,
    }
}
fn summary_time(value: PythonJson, op: &str) -> Result<PythonJson, Box<dyn Error>> {
    let PythonJson::Object(mut wire) = value else {
        return Err("wire object".into());
    };
    if wire.remove("status") != Some(PythonJson::Integer("200".into())) {
        return Err("status".into());
    }
    let PythonJson::Object(mut body) = wire.remove("body").ok_or("body")? else {
        return Err("body object".into());
    };
    if op == "trashList" {
        let PythonJson::Array(records) = body.remove("records").ok_or("records")? else {
            return Err("array".into());
        };
        let PythonJson::Object(row) = records.into_iter().next().ok_or("empty records")? else {
            return Err("record".into());
        };
        body = row;
    }
    Ok(body.remove("trashedAt").ok_or("timestamp")?)
}
#[test]
fn legacy_summary_numbers_and_nested_values_survive_wire_and_receipt_replay(
) -> Result<(), Box<dyn Error>> {
    let giant = "9".repeat(100);
    let profiles = [
        giant.clone(),
        format!("{{\"historical\":[{giant}]}}"),
        "9".repeat(1000),
        "9".repeat(4300),
        "{\"$serde_json::private::Number\":\"123\"}".into(),
        "null".into(),
        "true".into(),
        "-0.0".into(),
        "1e-05".into(),
        "1e+20".into(),
        "[]".into(),
        format!("\"{}\"", "9".repeat(5000)),
    ];
    let mut mismatches = Vec::new();
    for timestamp in profiles {
        for op in ["trashList", "trash", "restore"] {
            let (_dir, path) = common::fixture()?;
            let db = Connection::open(&path)?;
            db.execute("DELETE FROM files WHERE path='Notes/one.md'", [])?;
            let version = format!("{:x}", Sha256::digest(b"synthetic note"));
            let encoded=format!("{{\"format\":\"noevia-trash-v1\",\"id\":\"{}\",\"path\":\"Notes/one.md\",\"version\":\"{version}\",\"data\":\"c3ludGhldGljIG5vdGU=\",\"state\":\"trashed\",\"trashedAt\":{timestamp},\"unknown\":{{\"counter\":{},\"notFinite\":NaN}}}}",common::CAPSULE,"9".repeat(1000)).into_bytes();
            let capsule = format!(".noevia-trash/{}.json", common::CAPSULE);
            db.execute(
                "INSERT INTO files VALUES(?,?,?,1.0)",
                params![capsule, encoded, format!("{:x}", Sha256::digest(&encoded))],
            )?;
            let mut input = json!({"version":1,"dbPath":path,"expectedTenant":common::TENANT,"operationNamespace":common::NAMESPACE,"operationId":common::OP_ID,"capsuleId":common::CAPSULE,"op":op,"now":2.0,"debounce":0.0,"config":common::config()});
            if op == "trash" {
                let fields = input.as_object_mut().ok_or("input object")?;
                fields.insert("source".into(), json!("Notes/one.md"));
                fields.insert("sourceVersion".into(), json!(version));
            }
            let raw = serde_json::to_vec(&input)?;
            let mut child = Command::new(env!("CARGO_BIN_EXE_diary-workspace-transaction"))
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()?;
            let mut pipe = child.stdin.take().ok_or("stdin")?;
            pipe.write_all(b"DWT1")?;
            pipe.write_all(&(raw.len() as u32).to_le_bytes())?;
            pipe.write_all(&raw)?;
            drop(pipe);
            let output = child.wait_with_output()?;
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let end = output.stdout.len().checked_sub(4).ok_or("short frame")?;
            assert_eq!(output.stdout.get(..4), Some(b"DWR1".as_slice()));
            assert_eq!(output.stdout.get(end..), Some(b"END1".as_slice()));
            let size: u32 =
                u32::from_le_bytes(output.stdout.get(4..8).ok_or("frame size")?.try_into()?);
            assert_eq!(size as usize, end.checked_sub(8).ok_or("short payload")?);
            let expected =
                PythonJson::parse(timestamp.as_bytes(), limits()).map_err(|_| "timestamp JSON")?;
            let actual = summary_time(
                PythonJson::parse(output.stdout.get(8..end).ok_or("payload")?, limits())
                    .map_err(|_| "wire JSON")?,
                op,
            )?;
            if actual != expected {
                mismatches.push((op, timestamp.clone()));
            }
            if op != "trashList" {
                let mut req: Request = serde_json::from_value(input)?;
                req.now = None;
                req.config = None;
                req.debounce = None;
                let replay = execute(&req)?;
                if timestamp == giant
                    || timestamp.contains("historical")
                    || (timestamp.len() >= 1000 && !timestamp.starts_with('"'))
                {
                    assert!(
                        replay.body().to_value().is_err(),
                        "a checked Value view must not round"
                    );
                }
                let mut wire = Vec::new();
                write_reply(&mut wire, &replay)?;
                assert_eq!(wire, output.stdout);
            }
            let stored: Vec<u8> =
                db.query_row("SELECT data FROM files WHERE path=?", [capsule], |r| {
                    r.get(0)
                })?;
            let PythonJson::Object(values) =
                PythonJson::parse(&stored, limits()).map_err(|_| "capsule JSON")?
            else {
                return Err("capsule object".into());
            };
            assert_eq!(values.get("trashedAt"), Some(&expected));
        }
    }
    assert!(
        mismatches.is_empty(),
        "legacy timestamp rounded: {mismatches:?}"
    );
    Ok(())
}

#[test]
fn python_integer_conversion_limit_applies_to_opaque_capsule_fields() -> Result<(), Box<dyn Error>>
{
    let mut accepted = Vec::new();
    let over = "9".repeat(4301);
    for extra in [
        format!("\"unknown\":{over}"),
        format!("\"unknown\":{over},\"unknown\":0"),
        format!("\"nested\":{{\"x\":-{over}}},\"nested\":null"),
        format!("\"unknown\":{over},\"unk\\u006eown\":0"),
    ] {
        for op in ["trashList", "trash", "restore"] {
            let (_dir, path) = common::fixture()?;
            let db = Connection::open(&path)?;
            db.execute("DELETE FROM files WHERE path='Notes/one.md'", [])?;
            let version = format!("{:x}", Sha256::digest(b"synthetic note"));
            let raw=format!("{{\"format\":\"noevia-trash-v1\",\"id\":\"{}\",\"path\":\"Notes/one.md\",\"version\":\"{version}\",\"data\":\"c3ludGhldGljIG5vdGU=\",\"state\":\"trashed\",\"trashedAt\":1,{extra}}}",common::CAPSULE).into_bytes();
            let key = format!(".noevia-trash/{}.json", common::CAPSULE);
            db.execute(
                "INSERT INTO files VALUES(?,?,?,1.0)",
                params![key, raw, format!("{:x}", Sha256::digest(&raw))],
            )?;
            let mut input = json!({"version":1,"dbPath":path,"expectedTenant":common::TENANT,"operationNamespace":common::NAMESPACE,"operationId":common::OP_ID,"capsuleId":common::CAPSULE,"op":op,"now":2.0,"debounce":0.0,"config":common::config()});
            if op == "trash" {
                let fields = input.as_object_mut().ok_or("object")?;
                fields.insert("source".into(), json!("Notes/one.md"));
                fields.insert("sourceVersion".into(), json!(version));
            }
            let req: Request = serde_json::from_value(input)?;
            let result = execute(&req)?;
            let rejected = if op == "trashList" {
                result
                    .body()
                    .to_value()?
                    .get("records")
                    .and_then(serde_json::Value::as_array)
                    .and_then(|v| v.first())
                    .and_then(|v| v.get("invalid"))
                    .and_then(serde_json::Value::as_bool)
                    == Some(true)
            } else {
                result.status() == 409
            };
            if !rejected {
                accepted.push(op);
            }
            assert!(
                db.query_row::<Vec<u8>, _, _>("SELECT data FROM files WHERE path=?", [key], |r| r
                    .get(0))?
                    == raw,
                "capsule changed"
            );
            assert_eq!(
                db.query_row::<String, _, _>(
                    "SELECT value FROM meta WHERE key='generation'",
                    [],
                    |r| r.get(0)
                )?,
                "0"
            );
        }
    }
    assert!(
        accepted.is_empty(),
        "invalid opaque integer accepted: {accepted:?}"
    );
    Ok(())
}
