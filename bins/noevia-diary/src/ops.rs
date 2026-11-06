//! Managed workspace mutations. The caller owns the D5 writer until this synchronous
//! operation settles. File effects and recovery outbox are one FULL SQLite transaction;
//! dirty transfer is durable-before-delete, so a crash never loses invalidation.
use crate::{capsule_json::Json, corpus::Layout, trash, workspace::Reply, write, writes};
use diary_workspace_rules as rules;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde_json::{Map, Value};
use std::path::Path;

const PROTECTED: &str = "Diary capture files, month files, the index and AI Memory are protected";
const CAPTURE: &str = "Capture records or files over 512 KiB cannot be moved to Trash";
const INVALID: &str = "Recovery record is invalid; preserved bytes have not been changed";

enum Failure {
    Public(u16, String),
    Decline,
    Infra,
}
type Result<T> = std::result::Result<T, Failure>;
impl From<rusqlite::Error> for Failure {
    fn from(_: rusqlite::Error) -> Self {
        Self::Infra
    }
}
fn fail<T>(status: u16, text: &str) -> Result<T> {
    Err(Failure::Public(status, text.into()))
}
fn string<'a>(b: &'a Map<String, Value>, k: &str) -> Option<&'a str> {
    b.get(k).and_then(Value::as_str)
}
fn rel(b: &Map<String, Value>, k: &str) -> Result<String> {
    let Some(s) = string(b, k) else {
        return fail(400, "Invalid path");
    };
    if s.chars().count() > 500 || s.contains(['\\', '\0']) {
        return fail(400, "Invalid path");
    }
    rules::rel_path(s, false).map_err(|_| {
        Failure::Public(
            403,
            if s.trim_matches('/').is_empty() {
                "The workspace root cannot be changed"
            } else {
                "Choose a path inside the diary folder"
            }
            .into(),
        )
    })
}
fn protection(l: &Layout) -> rules::ProtectionConfig {
    rules::ProtectionConfig {
        remote_root: String::new(),
        entries_prefix: l.entries_prefix.clone(),
        monthly_prefix: l.monthly_prefix.clone(),
        month_file_template: l.month_file_template.clone(),
        index_file: l.index_file.clone(),
        month_names: (1..=12)
            .map(|m| crate::corpus::month_name(m).to_string())
            .collect(),
    }
}
fn protected(l: &Layout, p: &str) -> Result<bool> {
    protection(l).protected(p).map_err(|_| Failure::Decline)
}
#[derive(Clone)]
struct File {
    path: String,
    data: Vec<u8>,
    version: String,
}
struct Tree {
    files: Vec<File>,
    dirs: Vec<String>,
}
impl Tree {
    fn kind(&self, p: &str) -> Option<bool> {
        if self.files.iter().any(|f| f.path == p) {
            Some(false)
        } else if !self.files.is_empty() || !self.dirs.is_empty() {
            Some(true)
        } else {
            None
        }
    }
    fn version(&self, p: &str) -> String {
        if let Some(f) = self.files.iter().find(|f| f.path == p) {
            return f.version.clone();
        }
        rules::folder_version(
            &self
                .files
                .iter()
                .map(|f| rules::FileVersion {
                    path: f.path.clone(),
                    version: f.version.clone(),
                })
                .collect::<Vec<_>>(),
            &self.dirs,
            p,
        )
    }
    fn check(&self, p: &str, expected: Option<&str>, label: &str) -> Result<()> {
        let Some(expected) = expected.filter(|v| !v.is_empty()) else {
            return fail(
                428,
                &format!("Read the {label} ETag and send it with If-Match"),
            );
        };
        if self.version(p) != expected {
            return fail(412, &format!("The {label} changed; read it again"));
        }
        Ok(())
    }
    fn bounds(&self) -> Result<()> {
        rules::check_tree_bounds(
            self.files.len(),
            self.dirs.len(),
            self.files.iter().map(|f| f.data.len()).sum(),
        )
        .map_err(|_| {
            Failure::Public(
                507,
                "Folder operations are limited to 500 entries and 50 MiB".into(),
            )
        })
    }
    fn refuse(&self, l: &Layout, p: &str) -> Result<()> {
        for path in std::iter::once(p)
            .chain(self.files.iter().map(|f| f.path.as_str()))
            .chain(self.dirs.iter().map(String::as_str))
        {
            if protected(l, path)? {
                return fail(403, PROTECTED);
            }
        }
        Ok(())
    }
}
fn tree(db: &Connection, p: &str) -> Result<Tree> {
    let n = p.chars().count() as i64 + 1;
    let below = format!("{p}/");
    let mut st =
        db.prepare("SELECT path,data,version FROM files WHERE path=? OR substr(path,1,?)=?")?;
    let files = st
        .query_map(params![p, n, below], |r| {
            Ok(File {
                path: r.get(0)?,
                data: r.get(1)?,
                version: r.get(2)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut st = db.prepare("SELECT path FROM directories WHERE path=? OR substr(path,1,?)=?")?;
    let dirs = st
        .query_map(params![p, n, below], |r| r.get(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(Tree { files, dirs })
}
fn exists(db: &Connection, sql: &str, p: &str) -> Result<bool> {
    Ok(db.query_row(sql, [p], |_| Ok(())).optional()?.is_some())
}
fn all_paths(db: &Connection) -> Result<Vec<(String, bool)>> {
    let mut s = db.prepare("SELECT path,1 FROM files UNION ALL SELECT path,0 FROM directories")?;
    let rows = s
        .query_map([], |r| Ok((r.get(0)?, r.get::<_, i64>(1)? != 0)))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}
fn parent_exists(db: &Connection, p: &str) -> Result<bool> {
    let parent = p.rsplit_once('/').map(|(p, _)| p).unwrap_or("");
    if parent.is_empty() {
        return Ok(true);
    }
    let paths = all_paths(db)?;
    Ok(!paths.iter().any(|(p, f)| p == parent && *f)
        && paths
            .iter()
            .any(|(p, _)| p == parent || p.starts_with(&format!("{parent}/"))))
}
fn keep_parent(db: &Connection, p: &str) -> Result<()> {
    if let Some((parent, _)) = p.rsplit_once('/') {
        if !all_paths(db)?
            .iter()
            .any(|(p, f)| (!*f && p == parent) || p.starts_with(&format!("{parent}/")))
        {
            db.execute("INSERT OR IGNORE INTO directories VALUES (?)", [parent])?;
        }
    }
    Ok(())
}
fn put(db: &Connection, p: &str, data: &[u8], version: &str, now: f64) -> Result<()> {
    db.execute(
        "INSERT OR REPLACE INTO files VALUES (?,?,?,?)",
        params![p, data, version, now],
    )?;
    Ok(())
}
fn commit(db: &Connection, paths: &[String], now: f64) -> Result<()> {
    for p in paths {
        db.execute(
            "INSERT OR IGNORE INTO workspace_index_outbox VALUES (?)",
            [p],
        )?;
    }
    write::bump(db, now).map_err(|_| Failure::Infra)
}
fn capsule(db: &Connection, f: &File, now: f64, restore: Option<&str>) -> Result<String> {
    rules::trashable(&f.data).map_err(|_| Failure::Public(403, CAPTURE.into()))?;
    let id = write::uuid4().map_err(|_| Failure::Infra)?;
    // Unlike the checked new-writer API, Python preserve accepts non-Markdown source names.
    let record = serde_json::json!({"format":"noevia-trash-v1","id":id,"path":restore.unwrap_or(&f.path),"version":f.version,"data":base64::Engine::encode(&base64::engine::general_purpose::STANDARD,&f.data),"state":"trashed","trashedAt":now});
    let encoded = encode(&record)?;
    put(
        db,
        &format!("{}{id}.json", rules::TRASH_PREFIX),
        &encoded,
        &rules::sha256(&encoded),
        now,
    )?;
    Ok(id)
}
// Python json.dumps(sort_keys=True,ensure_ascii=False), retaining unknown fields on restore.
fn encode(v: &Value) -> Result<Vec<u8>> {
    fn render(v: &Value) -> Result<String> {
        Ok(match v {
            Value::Array(a) => format!(
                "[{}]",
                a.iter().map(render).collect::<Result<Vec<_>>>()?.join(", ")
            ),
            Value::Object(o) => format!(
                "{{{}}}",
                o.iter()
                    .map(|(k, v)| Ok(format!(
                        "{}: {}",
                        serde_json::to_string(k).map_err(|_| Failure::Infra)?,
                        render(v)?
                    )))
                    .collect::<Result<Vec<_>>>()?
                    .join(", ")
            ),
            Value::Number(n) if n.is_f64() => {
                write::py_float(n.as_f64().ok_or(Failure::Decline)?).ok_or(Failure::Decline)?
            }
            _ => serde_json::to_string(v).map_err(|_| Failure::Infra)?,
        })
    }
    Ok(render(v)?.into_bytes())
}
// Ordered response fields: Starlette emits compact JSON in insertion order.
fn reply(fields: &[(&str, Value)]) -> Reply {
    let parts = fields
        .iter()
        .map(|(k, v)| {
            format!(
                "{}:{}",
                serde_json::to_string(k).unwrap_or_default(),
                serde_json::to_string(v).unwrap_or_default()
            )
        })
        .collect::<Vec<_>>();
    Reply::Json(200, format!("{{{}}}", parts.join(",")).into_bytes())
}
fn operate(db: &Connection, l: &Layout, b: &Map<String, Value>, now: f64) -> Result<Reply> {
    if matches!(b.get("op"), Some(Value::Array(_) | Value::Object(_))) {
        return Err(Failure::Decline); // Python dict lookup raises TypeError.
    }
    let op = string(b, "op").unwrap_or("");
    if !["stat", "delete", "move", "copy", "preserve"].contains(&op) {
        return fail(400, "Choose stat, delete, move, copy or preserve");
    }
    let source = rel(b, "path")?;
    let dest = if matches!(op, "move" | "copy") {
        let d = rel(b, "destination")?;
        if d == source || d.starts_with(&format!("{source}/")) {
            return fail(403, "A folder cannot be moved or copied into itself");
        }
        Some(d)
    } else {
        None
    };
    let t = tree(db, &source)?;
    let Some(dir) = t.kind(&source) else {
        return fail(
            404,
            if op == "preserve" {
                "File not found"
            } else {
                "File or folder not found"
            },
        );
    };
    if op == "stat" {
        return Ok(reply(&[
            ("path", source.clone().into()),
            ("isDir", dir.into()),
            ("version", t.version(&source).into()),
            (
                "entries",
                (if dir { t.files.len() + t.dirs.len() } else { 1 }).into(),
            ),
        ]));
    }
    if op == "preserve" {
        if dir {
            return fail(404, "File not found");
        }
        if protected(l, &source)? {
            return fail(428,"Diary capture files, month files, the index and AI Memory need If-Match to be replaced");
        }
        t.check(&source, string(b, "version"), "source")?;
        let name = rules::replaced_name(&source, now).map_err(|_| Failure::Infra)?;
        let f = t
            .files
            .iter()
            .find(|f| f.path == source)
            .ok_or(Failure::Infra)?;
        let id = capsule(db, f, now, Some(&name))?;
        commit(db, &[], now)?;
        return Ok(reply(&[("path", source.into()), ("trash", id.into())]));
    }
    if op != "copy" {
        t.refuse(l, &source)?;
        t.check(&source, string(b, "version"), "source")?;
    }
    if op == "delete" {
        t.bounds()?;
        if t.files
            .iter()
            .any(|f| f.path.starts_with(rules::TRASH_PREFIX))
        {
            return fail(403, "Trash records cannot be deleted");
        }
        if t.files
            .iter()
            .any(|f| !f.path.to_lowercase().ends_with(".md"))
        {
            return fail(
                409,
                "The folder holds files this endpoint cannot show; remove it in the app",
            );
        }
        let mut files = t.files.clone();
        files.sort_by(|a, b| a.path.cmp(&b.path));
        let ids = files
            .iter()
            .map(|f| capsule(db, f, now, None))
            .collect::<Result<Vec<_>>>()?;
        for table in ["files", "directories"] {
            db.execute(
                &format!("DELETE FROM {table} WHERE path=? OR substr(path,1,?)=?"),
                params![
                    source,
                    source.chars().count() as i64 + 1,
                    format!("{source}/")
                ],
            )?;
        }
        keep_parent(db, &source)?;
        commit(
            db,
            &files.iter().map(|f| f.path.clone()).collect::<Vec<_>>(),
            now,
        )?;
        return Ok(reply(&[("deleted", source.into()), ("trash", ids.into())]));
    }
    let destination = dest.ok_or(Failure::Infra)?;
    if protected(l, &destination)? {
        return fail(403, PROTECTED);
    }
    if op == "copy" || dir {
        for p in t.files.iter().map(|f| &f.path).chain(t.dirs.iter()) {
            let target = format!(
                "{destination}{}",
                p.strip_prefix(&source).ok_or(Failure::Infra)?
            );
            if protected(l, &target)? {
                return fail(403, PROTECTED);
            }
        }
    }
    if op == "copy" && b.contains_key("version") {
        t.check(&source, string(b, "version"), "source")?;
    }
    t.bounds()?;
    if !dir && !destination.to_lowercase().ends_with(".md") {
        return fail(403, "Only Markdown file names are allowed");
    }
    if !parent_exists(db, &destination)? {
        return fail(409, "Destination folder does not exist");
    }
    let dt = tree(db, &destination)?;
    let mut replaced = false;
    if let Some(ddir) = dt.kind(&destination) {
        if b.get("overwrite") != Some(&Value::Bool(true)) {
            return fail(
                412,
                "Destination exists; send Overwrite: T with its ETag to replace it",
            );
        }
        if ddir || dir {
            return fail(
                409,
                "Replacing folders is not supported; delete the destination first",
            );
        }
        dt.check(&destination, string(b, "destinationVersion"), "destination")?;
        capsule(
            db,
            dt.files
                .iter()
                .find(|f| f.path == destination)
                .ok_or(Failure::Infra)?,
            now,
            None,
        )?;
        db.execute("DELETE FROM files WHERE path=?", [&destination])?;
        replaced = true;
    }
    let mut touched = Vec::new();
    for f in &t.files {
        let target = format!(
            "{destination}{}",
            f.path.strip_prefix(&source).ok_or(Failure::Infra)?
        );
        if op == "copy" {
            put(db, &target, &f.data, &rules::sha256(&f.data), now)?;
        } else {
            db.execute(
                "UPDATE files SET path=?,updated=? WHERE path=?",
                params![target, now, f.path],
            )?;
            touched.push(f.path.clone());
        }
        touched.push(target);
    }
    for p in &t.dirs {
        let target = format!(
            "{destination}{}",
            p.strip_prefix(&source).ok_or(Failure::Infra)?
        );
        if op == "copy" {
            db.execute("INSERT OR IGNORE INTO directories VALUES (?)", [target])?;
        } else {
            db.execute(
                "UPDATE directories SET path=? WHERE path=?",
                params![target, p],
            )?;
        }
    }
    if op == "move" {
        keep_parent(db, &source)?;
    }
    commit(db, &touched, now)?;
    Ok(reply(&[
        ("path", source.into()),
        ("destination", destination.into()),
        ("replaced", replaced.into()),
    ]))
}
fn allowed(l: &Layout, p: &str) -> Result<()> {
    crate::workspace::safe_path(p, false).map_err(|s| Failure::Public(409, s.into()))?;
    if protected(l, p)? {
        return fail(
            409,
            "Diary capture files, month files, the index and AI Memory cannot be moved to Trash",
        );
    }
    Ok(())
}
struct Capsule {
    record: Json,
    raw: Vec<u8>,
}
fn decode(data: &[u8], id: &str) -> Result<Capsule> {
    match trash::decode(data, id) {
        trash::Decoded::Decline => return Err(Failure::Decline),
        trash::Decoded::Invalid => return fail(409, INVALID),
        _ => {}
    }
    let decoded = rules::decode_legacy(data, id).map_err(|_| Failure::Decline)?;
    Ok(Capsule {
        record: Json::parse(data).map_err(|_| Failure::Decline)?,
        raw: decoded.raw,
    })
}
fn summary(r: &Json, already: bool) -> Result<Reply> {
    let mut fields = Vec::new();
    for k in ["id", "path", "version", "state", "trashedAt"] {
        fields.push((k, r.get(k).cloned().ok_or(Failure::Decline)?));
    }
    fields.push(("alreadyApplied", Json::bool(already)));
    fields.push(("indexPending", Json::bool(true)));
    Ok(Reply::Json(
        200,
        Json::object(fields)
            .encode(false, false)
            .map_err(|_| Failure::Decline)?,
    ))
}
fn change(db: &Connection, l: &Layout, b: &Map<String, Value>, now: f64) -> Result<Reply> {
    let id = string(b, "id")
        .and_then(|v| rules::normalize_uuid(v).ok())
        .ok_or_else(|| Failure::Public(409, "A valid recovery operation ID is required".into()))?;
    let action = string(b, "action").unwrap_or("");
    if !["trash", "restore"].contains(&action) {
        return fail(409, "Choose trash or restore");
    }
    let capsule = format!("{}{id}.json", rules::TRASH_PREFIX);
    let existing: Option<Vec<u8>> = db
        .query_row("SELECT data FROM files WHERE path=?", [&capsule], |r| {
            r.get(0)
        })
        .optional()?;
    let (mut record, raw, full) = if action == "trash" {
        let Some(p) = string(b, "path") else {
            return Err(Failure::Decline);
        };
        allowed(l, p)?;
        if let Some(data) = existing {
            let rec = decode(&data, &id)?.record;
            if rec.get("path").and_then(Json::as_str) != string(b, "path")
                || rec.get("version").and_then(Json::as_str) != string(b, "version")
            {
                return fail(
                    409,
                    "Operation ID was already used for a different file version",
                );
            }
            return summary(&rec, true);
        }
        let row: Option<(Vec<u8>, String)> = db
            .query_row("SELECT data,version FROM files WHERE path=?", [p], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .optional()?;
        let Some((raw, version)) = row.filter(|(_, v)| Some(v.as_str()) == string(b, "version"))
        else {
            return fail(
                409,
                "File changed or is missing. Reopen it before moving it to Trash.",
            );
        };
        rules::trashable(&raw).map_err(|_| Failure::Public(409, CAPTURE.into()))?;
        if exists(db, "SELECT 1 FROM files WHERE path=?", ".noevia-trash")?
            || exists(db, "SELECT 1 FROM directories WHERE path=?", &capsule)?
            || all_paths(db)?
                .iter()
                .any(|(p, f)| *f && p.starts_with(&format!("{capsule}/")))
        {
            return fail(409, "Recovery namespace conflicts with existing data");
        }
        let rec = serde_json::json!({"format":"noevia-trash-v1","id":id,"path":p,"version":version,"data":base64::Engine::encode(&base64::engine::general_purpose::STANDARD,&raw),"state":"trashed","trashedAt":now});
        (
            Json::from_value(&rec).map_err(|_| Failure::Decline)?,
            raw,
            p.to_string(),
        )
    } else {
        let data =
            existing.ok_or_else(|| Failure::Public(409, "Recovery record was not found".into()))?;
        let decoded = decode(&data, &id)?;
        let p = decoded
            .record
            .get("path")
            .and_then(Json::as_str)
            .ok_or(Failure::Decline)?
            .to_string();
        allowed(l, &p)?;
        if decoded.record.get("state").and_then(Json::as_str) == Some("restored") {
            return summary(&decoded.record, true);
        }
        if all_paths(db)?.iter().any(|(path, file)| {
            path == &p
                || path.starts_with(&format!("{p}/"))
                || (*file && p.starts_with(&format!("{path}/")))
        }) {
            return fail(
                409,
                "The original path is occupied. Restore never overwrites existing data.",
            );
        }
        (decoded.record, decoded.raw, p)
    };
    // Preflight serialization and response before the first effect (decline must be safe).
    if action == "restore" {
        record
            .set("state", Json::String("restored".into()))
            .map_err(|_| Failure::Decline)?;
        record
            .set(
                "restoredAt",
                Json::from_value(&now.into()).map_err(|_| Failure::Decline)?,
            )
            .map_err(|_| Failure::Decline)?;
    }
    let response = summary(&record, false)?;
    let encoded = record.encode(true, true).map_err(|_| Failure::Decline)?;
    if action == "trash" {
        db.execute("DELETE FROM files WHERE path=?", [&full])?;
    } else {
        put(db, &full, &raw, &rules::sha256(&raw), now)?;
    }
    put(db, &capsule, &encoded, &rules::sha256(&encoded), now)?;
    commit(db, &[full], now)?;
    Ok(response)
}
/// Under D5 exclusion, including after caller cancellation (spawn_blocking owns it).
pub fn run(path: &Path, index: &Path, l: &Layout, b: &Map<String, Value>, is_trash: bool) -> Reply {
    let result = (|| -> Result<Reply> {
        let mut db = write::open_rw(path).map_err(|_| Failure::Infra)?;
        db.execute_batch("PRAGMA synchronous=FULL")?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = write::now_epoch();
        let response = if is_trash {
            change(&tx, l, b, now)?
        } else {
            operate(&tx, l, b, now)?
        };
        tx.commit()?;
        // Same crash-safe protocol as drain_index_outbox; a failed transfer retains the outbox.
        let _ = drain(&mut db, index);
        Ok(response)
    })();
    match result {
        Ok(r) => r,
        Err(Failure::Public(s, t)) => Reply::Json(
            s,
            serde_json::to_vec(&serde_json::json!({"detail":t})).unwrap_or_default(),
        ),
        Err(Failure::Decline) => Reply::Decline,
        Err(Failure::Infra) => writes::internal(),
    }
}
fn drain(db: &mut Connection, index: &Path) -> Result<()> {
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let journal = write::Journal::open(index).map_err(|_| Failure::Infra)?;
    let paths = {
        let mut s = tx.prepare("SELECT path FROM workspace_index_outbox")?;
        let rows = s
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        rows
    };
    for path in paths {
        journal.mark_dirty(&path).map_err(|_| Failure::Infra)?;
        tx.execute("DELETE FROM workspace_index_outbox WHERE path=?", [path])?;
    }
    tx.commit()?;
    Ok(())
}
