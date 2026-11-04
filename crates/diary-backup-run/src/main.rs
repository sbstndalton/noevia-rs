//! Backup orchestration. The caller retains the existing gated remote backend and flock.
use base64::{engine::general_purpose::STANDARD, Engine};
use diary_backup_format::{plan, sha256, Limits, PlanInput, PythonJson, SourceFile};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use serde_json::{json, Value};
use std::io::{BufRead, Read, Write};
type R<T> = Result<T, Box<dyn std::error::Error>>;
const FAILURE: &str = "Backup could not be verified. Check the connection or remote conflict; app saves are retained.";
fn string<'a>(v: &'a Value, k: &str) -> R<&'a str> {
    v.get(k)
        .and_then(Value::as_str)
        .ok_or_else(|| "field".into())
}
fn read(input: &mut impl BufRead) -> R<Value> {
    let mut line = String::new();
    // Only bounded control replies enter this process; file payloads travel outward.
    input.take(4 * 1024 * 1024 + 1).read_line(&mut line)?;
    if line.len() > 4 * 1024 * 1024 || !line.ends_with('\n') {
        return Err("frame".into());
    }
    Ok(serde_json::from_str(&line)?)
}
fn emit(v: Value) -> R<()> {
    let mut out = std::io::stdout().lock();
    serde_json::to_writer(&mut out, &v)?;
    out.write_all(b"\n")?;
    out.flush()?;
    Ok(())
}
fn meta(db: &Connection, key: &str, default: &str) -> R<String> {
    Ok(db
        .query_row("SELECT value FROM meta WHERE key=?", [key], |r| r.get(0))
        .optional()?
        .unwrap_or_else(|| default.into()))
}
fn wall() -> R<f64> {
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs_f64())
}
fn verify(input: &mut impl BufRead, path: &str, data: &[u8]) -> R<bool> {
    emit(json!({"type":"verify","path":path,"data":STANDARD.encode(data)}))?;
    let reply = read(input)?;
    match string(&reply, "result")? {
        "ok" => Ok(true),
        "loginRefused" => Err("loginRefused".into()),
        _ => Err("remote".into()),
    }
}
fn backup(input: &mut impl BufRead, v: &Value) -> R<Value> {
    let now = v["now"].as_f64().filter(|x| x.is_finite()).ok_or("clock")?;
    let db = Connection::open_with_flags(string(v, "db")?, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    db.busy_timeout(std::time::Duration::from_secs(30))?;
    db.execute_batch("PRAGMA synchronous=FULL; BEGIN")?;
    if meta(&db, "tenant", "")? != string(v, "tenant")? || meta(&db, "active", "")? != "1" {
        return Err("identity".into());
    }
    let destination = string(v, "destination")?;
    let generation = meta(&db, "generation", "0")?.parse::<i64>()?;
    let previous: Option<(i64, f64)> = db
        .query_row(
            "SELECT generation,retry_at FROM backups WHERE destination=?",
            [destination],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if meta(&db, "due", "0")?.parse::<f64>()? > now
        || previous.is_some_and(|(g, retry)| g == generation || retry > now)
    {
        db.execute_batch("COMMIT")?;
        return Ok(json!({"result":"idle"}));
    }
    let files = db
        .prepare("SELECT path,version FROM files ORDER BY path")?
        .query_map([], |r| {
            Ok(SourceFile {
                path: r.get(0)?,
                sha256: r.get(1)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let directories = db
        .prepare("SELECT path FROM directories ORDER BY path")?
        .query_map([], |r| r.get(0))?
        .collect::<Result<Vec<String>, _>>()?;
    let settings = meta(&db, "settings", "{}")?;
    let saved = meta(&db, "snapshot_time", "0")?.parse::<f64>()?;
    db.execute_batch("COMMIT")?;
    let mut abandoned = false;
    let result = (|| -> R<()> {
        let limits = Limits {
            manifest_bytes: 32 * 1024 * 1024,
            files: 50000,
            directories: 50000,
            json_depth: 64,
            json_nodes: 1000000,
            string_bytes: 1024 * 1024,
            payload_bytes: 1024 * 1024 * 1024,
        };
        let p = plan(
            PlanInput {
                corpus_root: string(v, "corpusRoot")?.into(),
                tenant: string(v, "tenant")?.into(),
                generation: generation.to_string(),
                saved_at: saved,
                settings: PythonJson::parse(settings.as_bytes(), limits)?,
                directories,
                files,
            },
            limits,
        )?;
        for file in &p.objects {
            let data: Option<Vec<u8>> = db
                .query_row(
                    "SELECT data FROM files WHERE path=? AND version=?",
                    params![file.path, file.sha256],
                    |r| r.get(0),
                )
                .optional()?;
            let Some(data) = data else {
                abandoned = true;
                return Ok(());
            };
            if sha256(&data) != file.sha256 {
                return Err("checksum".into());
            }
            verify(input, &file.object, &data)?;
        }
        verify(input, &p.manifest_key, &p.bytes)?;
        db.execute(
            "INSERT OR REPLACE INTO backups VALUES (?,?,?,0,0,NULL)",
            params![destination, generation, wall()?],
        )?;
        Ok(())
    })();
    match result {
        Ok(()) => Ok(json!({"result":if abandoned {"abandoned"}else{"complete"}})),
        Err(e) => {
            let previous: Option<(i64, Option<f64>, i64)> = db
                .query_row(
                    "SELECT generation,last_success,failures FROM backups WHERE destination=?",
                    [destination],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()?;
            let (g, last, n) = previous.unwrap_or((-1, None, 0));
            let failures = n.checked_add(1).ok_or("failures")?;
            let delay = (5_i64 * 2_i64.pow((failures - 1).clamp(0, 6) as u32)).min(300) as f64;
            db.execute(
                "INSERT OR REPLACE INTO backups VALUES (?,?,?,?,?,?)",
                params![destination, g, last, now + delay, failures, FAILURE],
            )?;
            Ok(json!({"result":"failed","loginRefused":e.to_string()=="loginRefused"}))
        }
    }
}
fn run() -> R<()> {
    if std::env::args().nth(1).as_deref() != Some("run-v1") {
        return Err("protocol".into());
    }
    let mut input = std::io::stdin().lock();
    let v = read(&mut input)?;
    let result = backup(&mut input, &v)?;
    emit(json!({"type":"done","outcome":result}))
}
fn main() {
    if run().is_err() {
        eprintln!("unavailable");
        std::process::exit(2);
    }
}
