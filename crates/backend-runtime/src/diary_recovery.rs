//! Recovery records in the original diary-conversations/day/id.json layout.
//! The candidate does not replay orphaned captures. Acknowledgment changes only the record.
use rustix::fs::{open, openat, renameat, unlinkat, AtFlags, Mode, OFlags, RawDir};
use serde_json::{json, Value};
use server_projects::{Ctx, Reply};
use std::{
    fs::File,
    io::{Read, Write},
    mem::MaybeUninit,
    path::Path,
};
fn valid_day(day: &str) -> bool {
    day.len() == 10
        && chrono::NaiveDate::parse_from_str(day, "%Y-%m-%d")
            .is_ok_and(|v| v.format("%Y-%m-%d").to_string() == day)
}
fn valid_id(id: &str) -> bool {
    (16..=80).contains(&id.len()) && id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
}
fn directory(data: &Path, user: &str, day: &str) -> std::io::Result<Option<File>> {
    let user = server_projects::store::user_dir(data, user)
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::PermissionDenied))?;
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let root = open(&user, flags, Mode::empty())?;
    let parent = match openat(&root, "diary-conversations", flags, Mode::empty()) {
        Ok(v) => v,
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    match openat(&parent, day, flags, Mode::empty()) {
        Ok(v) => Ok(Some(File::from(v))),
        Err(rustix::io::Errno::NOENT) => Ok(None),
        Err(e) => Err(e.into()),
    }
}
fn read(dir: &File, name: &str) -> std::io::Result<Value> {
    let file = File::from(openat(
        dir,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?);
    let stat = file.metadata()?;
    if !stat.is_file() || stat.len() > 1024 * 1024 {
        return Err(std::io::ErrorKind::InvalidData.into());
    }
    let mut bytes = Vec::new();
    file.take(1024 * 1024 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > 1024 * 1024 {
        return Err(std::io::ErrorKind::InvalidData.into());
    }
    serde_json::from_slice(&bytes).map_err(|_| std::io::ErrorKind::InvalidData.into())
}
fn write(dir: &File, name: &str, row: &Value) -> std::io::Result<()> {
    let temporary = format!(".recovery-{}.tmp", uuid::Uuid::new_v4());
    let result = (|| {
        let mut file = File::from(openat(
            dir,
            temporary.as_str(),
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )?);
        file.write_all(serde_json::to_string_pretty(row)?.as_bytes())?;
        file.sync_all()?;
        renameat(dir, temporary.as_str(), dir, name)?;
        dir.sync_all()
    })();
    if result.is_err() {
        let _ = unlinkat(dir, temporary.as_str(), AtFlags::empty());
    }
    result
}
pub fn handle(ctx: Ctx<'_>, method: &str, query: &str, body: &[u8]) -> Reply {
    if method == "GET" {
        let day = url::form_urlencoded::parse(query.as_bytes())
            .find(|(k, _)| k == "day")
            .map(|(_, v)| v.into_owned())
            .unwrap_or_default();
        if !valid_day(&day) {
            return Reply::error(400, "Valid diary day required.");
        }
        let dir = match directory(ctx.data_dir, ctx.user_id, &day) {
            Ok(Some(v)) => v,
            Ok(None) => return Reply::json(200, json!({"exchanges":[]}).to_string()),
            Err(_) => return Reply::error(500, "Could not read recovery records"),
        };
        let mut buf = [MaybeUninit::<u8>::uninit(); 8192];
        let fd = match openat(
            &dir,
            ".",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(v) => v,
            Err(_) => return Reply::internal(),
        };
        let mut entries = RawDir::new(fd, &mut buf);
        let mut rows = Vec::new();
        let mut count = 0usize;
        let mut total = 0usize;
        while let Some(entry) = entries.next() {
            let entry = match entry {
                Ok(v) => v,
                Err(_) => return Reply::error(500, "Could not read recovery records"),
            };
            let Ok(name) = entry.file_name().to_str() else {
                continue;
            };
            let Some(id) = name.strip_suffix(".json").filter(|id| valid_id(id)) else {
                continue;
            };
            let _ = id;
            count += 1;
            if count > 10000 {
                return Reply::error(503, "Recovery record scan limit reached");
            }
            if let Ok(mut row) = read(&dir, name) {
                if !row.is_object() {
                    continue;
                }
                if row.get("state").unwrap_or(&Value::Null) == "running" {
                    if let Some(object) = row.as_object_mut() {
                        object.insert("state".into(), json!("uncertain"));
                    }
                }
                total += row.to_string().len();
                if total > 128 * 1024 * 1024 {
                    return Reply::error(503, "Recovery record scan limit reached");
                }
                rows.push(row);
            }
        }
        rows.sort_by(|a, b| {
            a.get("startedAt")
                .and_then(Value::as_f64)
                .unwrap_or(0.0)
                .total_cmp(&b.get("startedAt").and_then(Value::as_f64).unwrap_or(0.0))
        });
        if rows.len() > 100 {
            rows.drain(..rows.len() - 100);
        }
        return Reply::json(200, json!({"exchanges":rows}).to_string());
    }
    if method != "POST" {
        return Reply::error(405, "Method not allowed");
    }
    if body.len() > 4096 {
        return Reply::error(413, "Request body too large");
    }
    let body: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => return Reply::error(400, "invalid JSON"),
    };
    if body.get("action").and_then(Value::as_str) != Some("dismiss") {
        return Reply::error(400, "Unknown action");
    }
    let day = body.get("day").and_then(Value::as_str).unwrap_or("");
    let id = body.get("id").and_then(Value::as_str).unwrap_or("");
    if !valid_day(day) || !valid_id(id) {
        return Reply::error(400, "Valid diary day and exchange ID required.");
    }
    let dir = match directory(ctx.data_dir, ctx.user_id, day) {
        Ok(Some(v)) => v,
        Ok(None) => return Reply::error(404, "No such exchange."),
        Err(_) => return Reply::error(500, "This exchange record cannot be read."),
    };
    let name = format!("{id}.json");
    let lock = match openat(
        &dir,
        format!("{id}.lock"),
        OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o600),
    ) {
        Ok(v) => File::from(v),
        Err(_) => return Reply::internal(),
    };
    if lock.try_lock().is_err() {
        return Reply::error(409, "This exchange is still running.");
    }
    let mut row = match read(&dir, &name) {
        Ok(v) if v.is_object() => v,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Reply::error(404, "No such exchange.")
        }
        _ => return Reply::error(500, "This exchange record cannot be read."),
    };
    if row.get("state").unwrap_or(&Value::Null) == "running" {
        if let Some(object) = row.as_object_mut() {
            object.insert("state".into(), json!("uncertain"));
        }
    }
    if !["uncertain", "failed"].contains(
        &row.get("state")
            .unwrap_or(&Value::Null)
            .as_str()
            .unwrap_or(""),
    ) {
        return Reply::error(409, "This exchange has nothing to resolve.");
    }
    if row
        .get("acknowledgedAt")
        .is_none_or(|v| v.is_null() || v == &json!(0) || v == &json!(false) || v == &json!(""))
    {
        if let Some(object) = row.as_object_mut() {
            object.insert("acknowledgedAt".into(), json!(ctx.now_ms));
        }
        if let Some(object) = row.as_object_mut() {
            object.insert("updatedAt".into(), json!(ctx.now_ms));
        }
        if write(&dir, &name, &row).is_err() {
            return Reply::error(500, "Could not update the recovery record");
        }
    }
    Reply::json(200,json!({"exchange":{"id":row.get("id").unwrap_or(&Value::Null),"day":row.get("day").unwrap_or(&Value::Null),"state":row.get("state").unwrap_or(&Value::Null),"acknowledgedAt":row.get("acknowledgedAt").unwrap_or(&Value::Null)}}).to_string())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stale_records_are_never_replayed_and_dismiss_is_idempotent(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        let user = "11111111-2222-4333-8444-555555555555";
        let day = "2026-10-28";
        let id = "exchange-00000001";
        let dir = root
            .path()
            .join("users")
            .join(user)
            .join("diary-conversations")
            .join(day);
        std::fs::create_dir_all(&dir)?;
        let file = dir.join(format!("{id}.json"));
        std::fs::write(&file,json!({"id":id,"day":day,"state":"running","message":"synthetic","startedAt":1,"unknown":{"retain":true}}).to_string())?;
        let ctx = Ctx {
            data_dir: root.path(),
            user_id: user,
            switch: server_store::RustProjects::owned(),
            now_ms: 42,
        };
        let before = std::fs::read(&file)?;
        let r = handle(ctx, "GET", "day=2026-10-28", b"");
        assert!(matches!(r.body,server_projects::Body::Json(ref s)if s.contains("uncertain")));
        assert_eq!(std::fs::read(&file)?, before);
        let body = json!({"action":"dismiss","day":day,"id":id}).to_string();
        assert_eq!(handle(ctx, "POST", "", body.as_bytes()).status, 200);
        let saved = std::fs::read(&file)?;
        let row: Value = serde_json::from_slice(&saved)?;
        assert_eq!(row.pointer("/unknown/retain").unwrap_or(&Value::Null), true);
        assert_eq!(row.get("acknowledgedAt").unwrap_or(&Value::Null), 42);
        assert_eq!(row.get("message").unwrap_or(&Value::Null), "synthetic");
        assert_eq!(
            handle(Ctx { now_ms: 99, ..ctx }, "POST", "", body.as_bytes()).status,
            200
        );
        assert_eq!(std::fs::read(&file)?, saved);
        assert_eq!(handle(ctx, "GET", "day=2026-02-31", b"").status, 400);
        let target = dir.join("symlink-000000001.json");
        std::os::unix::fs::symlink(&file, &target)?;
        let body = json!({"action":"dismiss","day":day,"id":"symlink-000000001"}).to_string();
        assert_eq!(handle(ctx, "POST", "", body.as_bytes()).status, 500);
        assert_eq!(std::fs::read(&file)?, saved);
        Ok(())
    }
}
