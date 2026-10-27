//! Scoped SQLite mutations. The outer transaction/savepoint and receipt owner
//! commits these effects atomically; no migration, caller, or runtime activation.
use super::{
    canonical_uuid, config, derived_uuid, metadata, need_version, protected, reads, reply, sha256,
    Entry, Fault, Json, Reply, Request,
};
use diary_workspace_rules::{self as rules, ProtectionConfig};
use rusqlite::{params, OptionalExtension, Transaction};
use serde_json::Value;

const MAX_CAPSULE_BYTES: i64 = (rules::MAX_FILE * 2) as i64;
struct Tree {
    files: Vec<(String, Entry)>,
    dirs: Vec<String>,
}
fn scope(full: &str) -> Result<(String, i64), Fault> {
    let prefix = format!("{full}/");
    let n = i64::try_from(prefix.chars().count()).map_err(|_| Fault::Capacity)?;
    Ok((prefix, n))
}
fn identity(tx: &Transaction<'_>, r: &Request) -> Result<(bool, String), Fault> {
    let result = reads::stat(tx, r)?;
    Ok((
        result
            .body()
            .get("isDir")
            .and_then(Json::as_bool)
            .ok_or(Fault::Internal)?,
        result
            .body()
            .get("version")
            .and_then(Json::as_str)
            .ok_or(Fault::Internal)?
            .to_owned(),
    ))
}
fn guard_paths(
    tx: &Transaction<'_>,
    full: &str,
    mut guard: impl FnMut(&str) -> Result<(), Fault>,
) -> Result<(), Fault> {
    reads::admit(tx, full, true)?;
    guard(full)?;
    let (prefix, n) = scope(full)?;
    let mut query=tx.prepare("SELECT path FROM files WHERE path=?1 OR substr(path,1,?2)=?3 UNION ALL SELECT path FROM directories WHERE path=?1 OR substr(path,1,?2)=?3")?;
    for row in query.query_map(params![full, n, prefix], |row| row.get::<_, String>(0))? {
        guard(&row?)?;
    }
    Ok(())
}
fn file(tx: &Transaction<'_>, path: &str) -> Result<Option<Entry>, Fault> {
    reads::admit(tx, path, true)?;
    let meta: Option<(i64, String, String)> = tx
        .query_row(
            "SELECT length(data),version,typeof(data) FROM files WHERE path=?",
            [path],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let Some((size, version, storage)) = meta else {
        return Ok(None);
    };
    if storage != "blob" || size < 0 {
        return Err(Fault::Schema);
    }
    if size > rules::MAX_BYTES as i64 {
        return Err(Fault::Capacity);
    }
    let data: Vec<u8> =
        tx.query_row("SELECT data FROM files WHERE path=?", [path], |r| r.get(0))?;
    if sha256(&data) != version {
        return Err(Fault::Schema);
    }
    Ok(Some(Entry { data, version }))
}
fn tree(
    tx: &Transaction<'_>,
    full: &str,
    mut guard: impl FnMut(&str) -> Result<(), Fault>,
) -> Result<Tree, Fault> {
    reads::admit(tx, full, true)?;
    let (prefix, n) = scope(full)?;
    let mut paths = Vec::new();
    let mut dirs = Vec::new();
    let mut count = 0usize;
    let mut bytes = 0i64;
    let mut query = tx.prepare(
        "SELECT path,length(data),typeof(data) FROM files WHERE path=?1 OR substr(path,1,?2)=?3",
    )?;
    for row in query.query_map(params![full, n, prefix], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, String>(2)?,
        ))
    })? {
        let (path, size, storage) = row?;
        guard(&path)?;
        if storage != "blob" || size < 0 {
            return Err(Fault::Schema);
        }
        count = count.checked_add(1).ok_or(Fault::Capacity)?;
        bytes = bytes.checked_add(size).ok_or(Fault::Capacity)?;
        if count <= rules::MAX_ENTRIES {
            paths.push(path);
        }
    }
    let mut query =
        tx.prepare("SELECT path FROM directories WHERE path=?1 OR substr(path,1,?2)=?3")?;
    for row in query.query_map(params![full, n, prefix], |r| r.get::<_, String>(0))? {
        let path = row?;
        guard(&path)?;
        count = count.checked_add(1).ok_or(Fault::Capacity)?;
        if count <= rules::MAX_ENTRIES {
            dirs.push(path);
        }
    }
    if count > rules::MAX_ENTRIES || bytes > rules::MAX_BYTES as i64 {
        return Err(Fault::Capacity);
    }
    let mut files = Vec::with_capacity(paths.len());
    for path in paths {
        let entry = file(tx, &path)?.ok_or(Fault::Schema)?;
        files.push((path, entry));
    }
    Ok(Tree { files, dirs })
}
fn kind(tx: &Transaction<'_>, full: &str) -> Result<Option<bool>, Fault> {
    let (prefix, n) = scope(full)?;
    if tx.query_row::<bool, _, _>(
        "SELECT EXISTS(SELECT 1 FROM files WHERE path=?)",
        [full],
        |r| r.get(0),
    )? {
        return Ok(Some(false));
    }
    let exists:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM files WHERE substr(path,1,?1)=?2 UNION ALL SELECT 1 FROM directories WHERE path=?3 OR substr(path,1,?1)=?2)",params![n,prefix,full],|r|r.get(0))?;
    Ok(exists.then_some(true))
}
fn parent_exists(tx: &Transaction<'_>, full: &str) -> Result<bool, Fault> {
    let parent = full.rsplit_once('/').map(|(p, _)| p).unwrap_or("");
    if parent.is_empty() {
        return Ok(true);
    }
    if tx.query_row::<bool, _, _>(
        "SELECT EXISTS(SELECT 1 FROM files WHERE path=?)",
        [parent],
        |r| r.get(0),
    )? {
        return Ok(false);
    }
    Ok(kind(tx, parent)?.is_some())
}
fn keep_parent(tx: &Transaction<'_>, full: &str) -> Result<(), Fault> {
    let Some((parent, _)) = full.rsplit_once('/') else {
        return Ok(());
    };
    if parent.is_empty() {
        return Ok(());
    }
    let (prefix, n) = scope(parent)?;
    let exists:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM files WHERE substr(path,1,?1)=?2 UNION ALL SELECT 1 FROM directories WHERE path=?3 OR substr(path,1,?1)=?2)",params![n,prefix,parent],|r|r.get(0))?;
    if !exists {
        tx.execute("INSERT OR IGNORE INTO directories VALUES(?)", [parent])?;
    }
    Ok(())
}
fn delete_tree(tx: &Transaction<'_>, full: &str) -> Result<(), Fault> {
    let (prefix, n) = scope(full)?;
    tx.execute(
        "DELETE FROM files WHERE path=?1 OR substr(path,1,?2)=?3",
        params![full, n, prefix],
    )?;
    tx.execute(
        "DELETE FROM directories WHERE path=?1 OR substr(path,1,?2)=?3",
        params![full, n, prefix],
    )?;
    Ok(())
}
fn touch(tx: &Transaction<'_>, path: &str) -> Result<(), Fault> {
    tx.execute(
        "INSERT OR IGNORE INTO workspace_index_outbox VALUES(?)",
        [path],
    )?;
    Ok(())
}
fn no_protected(c: &ProtectionConfig, path: &str) -> Result<(), Fault> {
    if protected(c, path)? {
        Err(Fault::Protected)
    } else {
        Ok(())
    }
}
fn capsule_id(r: &Request) -> Result<&str, Fault> {
    r.capsule_id
        .as_deref()
        .filter(|s| canonical_uuid(s))
        .ok_or(Fault::Invalid)
}
fn capsule_key(id: &str) -> String {
    format!("{}{id}.json", rules::TRASH_PREFIX)
}
fn capsule_namespace(tx: &Transaction<'_>, key: &str) -> Result<(), Fault> {
    if kind(tx, key)?.is_some() {
        return Err(Fault::Conflict);
    }
    let root = rules::TRASH_PREFIX.trim_end_matches('/');
    if tx.query_row::<bool, _, _>(
        "SELECT EXISTS(SELECT 1 FROM files WHERE path=?)",
        [root],
        |r| r.get(0),
    )? {
        return Err(Fault::Conflict);
    }
    Ok(())
}
fn write_capsule(
    tx: &Transaction<'_>,
    id: &str,
    path: &str,
    entry: &Entry,
    now: f64,
) -> Result<(), Fault> {
    let key = capsule_key(id);
    capsule_namespace(tx, &key)?;
    let raw = rules::encode_writer(&rules::NewCapsule {
        id,
        path,
        raw: &entry.data,
        state: rules::TrashState::Trashed,
        trashed_at: now,
        restored_at: None,
    })
    .map_err(|_| Fault::Protected)?;
    tx.execute(
        "INSERT INTO files VALUES(?,?,?,?)",
        params![key, raw, sha256(&raw), now],
    )?;
    Ok(())
}
fn read_capsule(
    tx: &Transaction<'_>,
    key: &str,
    id: &str,
) -> Result<Option<rules::LegacyCapsule>, Fault> {
    let meta: Option<(String, i64)> = tx
        .query_row(
            "SELECT typeof(data),length(data) FROM files WHERE path=?",
            [key],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((storage, size)) = meta else {
        return Ok(None);
    };
    if storage != "blob" || !(0..=MAX_CAPSULE_BYTES).contains(&size) {
        return Err(Fault::Conflict);
    }
    let raw: Vec<u8> = tx.query_row("SELECT data FROM files WHERE path=?", [key], |r| r.get(0))?;
    decode_capsule(&raw, id).map(Some)
}
fn decode_capsule(raw: &[u8], id: &str) -> Result<rules::LegacyCapsule, Fault> {
    use diary_backup_format::PythonJson;
    Json::integer_tokens(raw).map_err(|_| Fault::Conflict)?;
    let PythonJson::Object(record) =
        PythonJson::parse(raw, json_limits()).map_err(|_| Fault::Conflict)?
    else {
        return Err(Fault::Conflict);
    };
    fn integer_profile(value: &PythonJson) -> bool {
        match value {
            PythonJson::Integer(v) => v.strip_prefix('-').unwrap_or(v).len() <= 4300,
            PythonJson::Array(v) => v.iter().all(integer_profile),
            PythonJson::Object(v) => v.values().all(integer_profile),
            _ => true,
        }
    }
    // json.loads enforces the pinned CPython decimal-conversion limit even on
    // opaque fields. Unknown nonfinite legacy metadata is retained, not emitted.
    if !record.values().all(integer_profile) {
        return Err(Fault::Conflict);
    }
    // The pure legacy decoder checks these six string fields, including raw
    // bytes/digest/path/state. Do not send unknown numeric data through Value.
    let mut required = serde_json::Map::new();
    for key in ["format", "id", "path", "version", "data", "state"] {
        let Some(PythonJson::String(value)) = record.get(key) else {
            return Err(Fault::Conflict);
        };
        required.insert(key.into(), Value::String(value.clone()));
    }
    let fields = serde_json::to_vec(&required).map_err(|_| Fault::Internal)?;
    let mut capsule = rules::decode_legacy(&fields, id).map_err(|_| Fault::Conflict)?;
    capsule.encoded = raw.to_vec();
    Ok(capsule)
}
fn summary(encoded: &[u8], already: Option<bool>) -> Result<Json, Fault> {
    let diary_backup_format::PythonJson::Object(mut record) =
        diary_backup_format::PythonJson::parse(encoded, json_limits())
            .map_err(|_| Fault::Conflict)?
    else {
        return Err(Fault::Conflict);
    };
    let mut out = std::collections::BTreeMap::new();
    for key in ["id", "path", "version", "state", "trashedAt"] {
        out.insert(
            key.into(),
            Json::from_python(record.remove(key).ok_or(Fault::Conflict)?)
                .map_err(|_| Fault::Conflict)?,
        );
    }
    if let Some(already) = already {
        out.insert("alreadyApplied".into(), Json::Bool(already));
    }
    Ok(Json::Object(out))
}
fn json_limits() -> diary_backup_format::Limits {
    diary_backup_format::Limits {
        manifest_bytes: MAX_CAPSULE_BYTES as usize,
        files: 0,
        directories: 0,
        json_depth: 32,
        json_nodes: 10000,
        string_bytes: MAX_CAPSULE_BYTES as usize,
        payload_bytes: MAX_CAPSULE_BYTES as usize,
    }
}
fn restored_bytes(original: &[u8], now: f64) -> Result<Vec<u8>, Fault> {
    use diary_backup_format::PythonJson;
    let parsed = PythonJson::parse(original, json_limits()).map_err(|_| Fault::Conflict)?;
    let PythonJson::Object(mut values) = parsed else {
        return Err(Fault::Conflict);
    };
    values.insert("state".into(), PythonJson::String("restored".into()));
    values.insert("restoredAt".into(), PythonJson::Float(now));
    PythonJson::Object(values)
        .canonical_bytes(json_limits())
        .map_err(|_| Fault::Conflict)
}
fn safe_source(r: &Request) -> Result<String, Fault> {
    rules::rel_path(r.source.as_deref().unwrap_or(""), false).map_err(|_| Fault::Invalid)
}
fn target(
    c: &ProtectionConfig,
    source: &str,
    destination: &str,
    path: &str,
) -> Result<String, Fault> {
    let suffix = path.strip_prefix(source).ok_or(Fault::Schema)?;
    let relative = format!("{destination}{suffix}");
    let full = rules::join(&c.remote_root, &[&relative]);
    // A transfer must not create children outside either client path admission
    // or the stored/read key domain. Refuse the complete plan, never truncate.
    if relative.chars().count() > 500
        || i64::try_from(full.len()).map_err(|_| Fault::Capacity)? > reads::MAX_KEY_BYTES
    {
        return Err(Fault::Capacity);
    }
    Ok(full)
}
fn transfer(
    tx: &Transaction<'_>,
    r: &Request,
    c: &ProtectionConfig,
    now: f64,
) -> Result<Value, Fault> {
    let relative = safe_source(r)?;
    let destination = rules::rel_path(r.destination.as_deref().unwrap_or(""), false)
        .map_err(|_| Fault::Invalid)?;
    let full = rules::join(&c.remote_root, &[&relative]);
    let dest = rules::join(&c.remote_root, &[&destination]);
    if dest == full || dest.starts_with(&format!("{full}/")) {
        return Err(Fault::Protected);
    }
    let moving = r.op == "move";
    let (is_dir, version) = identity(tx, r)?;
    guard_paths(tx, &full, |path| {
        if moving {
            no_protected(c, path)?;
        }
        let suffix = path.strip_prefix(&full).ok_or(Fault::Schema)?;
        no_protected(c, &format!("{dest}{suffix}"))
    })?;
    if moving || r.source_version.is_present() {
        need_version(&version, r.source_version.value().map(String::as_str))?;
    }
    let source = tree(tx, &full, |_| Ok(()))?;
    if !is_dir && !dest.to_ascii_lowercase().ends_with(".md") {
        return Err(Fault::Protected);
    }
    if !parent_exists(tx, &dest)? {
        return Err(Fault::Conflict);
    }
    // Validate every derived output before any replacement capsule, deletion,
    // update, or insertion is issued, including explicit empty directories.
    for path in source
        .files
        .iter()
        .map(|(path, _)| path)
        .chain(source.dirs.iter())
    {
        target(c, &full, &destination, path)?;
    }
    let dest_kind = kind(tx, &dest)?;
    let replaced = dest_kind.is_some();
    if let Some(dest_dir) = dest_kind {
        if r.overwrite.value() != Some(&true) {
            return Err(Fault::Precondition);
        }
        if is_dir || dest_dir {
            return Err(Fault::Conflict);
        }
        let old = file(tx, &dest)?.ok_or(Fault::Schema)?;
        need_version(
            &old.version,
            r.destination_version.value().map(String::as_str),
        )?;
        write_capsule(tx, capsule_id(r)?, &destination, &old, now)?;
        tx.execute("DELETE FROM files WHERE path=?", [&dest])?;
    }
    for (path, entry) in &source.files {
        let target = target(c, &full, &destination, path)?;
        if moving {
            tx.execute(
                "UPDATE files SET path=?,updated=? WHERE path=?",
                params![target, now, path],
            )?;
            touch(tx, path)?;
        } else {
            tx.execute(
                "INSERT INTO files VALUES(?,?,?,?)",
                params![target, entry.data, entry.version, now],
            )?;
        }
        touch(tx, &target)?;
    }
    for path in &source.dirs {
        let target = target(c, &full, &destination, path)?;
        if moving {
            tx.execute(
                "UPDATE directories SET path=? WHERE path=?",
                params![target, path],
            )?;
        } else {
            tx.execute("INSERT OR IGNORE INTO directories VALUES(?)", [target])?;
        }
    }
    if moving {
        keep_parent(tx, &full)?;
    }
    Ok(serde_json::json!({"path":relative,"destination":destination,"replaced":replaced}))
}
fn delete(
    tx: &Transaction<'_>,
    r: &Request,
    c: &ProtectionConfig,
    now: f64,
) -> Result<Value, Fault> {
    let relative = safe_source(r)?;
    let full = rules::join(&c.remote_root, &[&relative]);
    let (_, version) = identity(tx, r)?;
    guard_paths(tx, &full, |path| no_protected(c, path))?;
    need_version(&version, r.source_version.value().map(String::as_str))?;
    let mut source = tree(tx, &full, |_| Ok(()))?;
    if source
        .files
        .iter()
        .any(|(p, _)| !p.to_ascii_lowercase().ends_with(".md"))
    {
        return Err(Fault::Conflict);
    }
    for (_, entry) in &source.files {
        rules::trashable(&entry.data).map_err(|_| Fault::Protected)?;
    }
    source.files.sort_by(|(a, _), (b, _)| a.cmp(b));
    let mut ids = Vec::new();
    let root = rules::join(&c.remote_root, &[]);
    for (path, entry) in &source.files {
        let rel = if root.is_empty() {
            path.as_str()
        } else {
            path.strip_prefix(&format!("{root}/"))
                .ok_or(Fault::Schema)?
        };
        let id = derived_uuid(r.operation_id.as_deref().ok_or(Fault::Invalid)?, rel);
        write_capsule(tx, &id, rel, entry, now)?;
        ids.push(id);
    }
    delete_tree(tx, &full)?;
    keep_parent(tx, &full)?;
    for (path, _) in &source.files {
        touch(tx, path)?;
    }
    Ok(serde_json::json!({"deleted":relative,"trash":ids}))
}
fn preserve(
    tx: &Transaction<'_>,
    r: &Request,
    c: &ProtectionConfig,
    now: f64,
) -> Result<Value, Fault> {
    let relative = safe_source(r)?;
    let full = rules::join(&c.remote_root, &[&relative]);
    let old = file(tx, &full)?.ok_or(Fault::Missing)?;
    if protected(c, &full)? {
        return Err(Fault::Required);
    }
    need_version(&old.version, r.source_version.value().map(String::as_str))?;
    let path = rules::replaced_name(&relative, now).map_err(|_| Fault::Protected)?;
    let id = capsule_id(r)?;
    write_capsule(tx, id, &path, &old, now)?;
    Ok(serde_json::json!({"path":relative,"trash":id}))
}
fn trash_change(
    tx: &Transaction<'_>,
    r: &Request,
    c: &ProtectionConfig,
    now: f64,
) -> Result<(Json, bool), Fault> {
    let id = capsule_id(r)?;
    let key = capsule_key(id);
    let existing = read_capsule(tx, &key, id)?;
    let (record, changed) = if r.op == "trash" {
        let relative = r.source.as_deref().ok_or(Fault::Invalid)?;
        rules::safe_file_path(relative).map_err(|_| Fault::Invalid)?;
        let full = rules::join(&c.remote_root, &[relative]);
        if protected(c, &full)? {
            return Err(Fault::Conflict);
        }
        if let Some(existing) = existing {
            if existing.record.get("path").and_then(Value::as_str) != Some(relative)
                || existing.record.get("version").and_then(Value::as_str)
                    != r.source_version.value().map(String::as_str)
            {
                return Err(Fault::Conflict);
            }
            (existing.encoded, false)
        } else {
            let old = file(tx, &full)?.ok_or(Fault::Conflict)?;
            if r.source_version.value().map(String::as_str) != Some(old.version.as_str()) {
                return Err(Fault::Conflict);
            }
            rules::trashable(&old.data).map_err(|_| Fault::Conflict)?;
            capsule_namespace(tx, &key)?;
            let raw = rules::encode_writer(&rules::NewCapsule {
                id,
                path: relative,
                raw: &old.data,
                state: rules::TrashState::Trashed,
                trashed_at: now,
                restored_at: None,
            })
            .map_err(|_| Fault::Conflict)?;
            tx.execute("DELETE FROM files WHERE path=?", [&full])?;
            tx.execute(
                "INSERT INTO files VALUES(?,?,?,?)",
                params![key, raw, sha256(&raw), now],
            )?;
            touch(tx, &full)?;
            (
                {
                    decode_capsule(&raw, id).map_err(|_| Fault::Internal)?;
                    raw
                },
                true,
            )
        }
    } else {
        let existing = existing.ok_or(Fault::Conflict)?;
        let relative = existing
            .record
            .get("path")
            .and_then(Value::as_str)
            .ok_or(Fault::Conflict)?;
        let full = rules::join(&c.remote_root, &[relative]);
        if protected(c, &full)? {
            return Err(Fault::Conflict);
        }
        if existing.record.get("state").and_then(Value::as_str) == Some("restored") {
            (existing.encoded, false)
        } else {
            if kind(tx, &full)?.is_some() {
                return Err(Fault::Conflict);
            }
            let parts = full.split('/').collect::<Vec<_>>();
            for i in 1..parts.len() {
                let ancestor = parts.get(..i).ok_or(Fault::Internal)?.join("/");
                if tx.query_row::<bool, _, _>(
                    "SELECT EXISTS(SELECT 1 FROM files WHERE path=?)",
                    [ancestor],
                    |row| row.get(0),
                )? {
                    return Err(Fault::Conflict);
                }
            }
            let raw = restored_bytes(&existing.encoded, now)?;
            tx.execute(
                "INSERT INTO files VALUES(?,?,?,?)",
                params![full, existing.raw, sha256(&existing.raw), now],
            )?;
            tx.execute(
                "UPDATE files SET data=?,version=?,updated=? WHERE path=?",
                params![raw, sha256(&raw), now, key],
            )?;
            touch(tx, &full)?;
            (
                {
                    decode_capsule(&raw, id).map_err(|_| Fault::Conflict)?;
                    raw
                },
                true,
            )
        }
    };
    Ok((summary(&record, Some(!changed))?, changed))
}
pub(super) fn apply(tx: &Transaction<'_>, r: &Request) -> Result<Reply, Fault> {
    let mut c = config(r)?;
    c.remote_root = rules::rel_path(&c.remote_root, true).map_err(|_| Fault::Invalid)?;
    let now = r.now.filter(|v| v.is_finite()).ok_or(Fault::Invalid)?;
    let debounce = r
        .debounce
        .filter(|v| v.is_finite() && *v >= 0.0)
        .ok_or(Fault::Invalid)?;
    let (body, changed) = match r.op.as_str() {
        "delete" => (Json::from(delete(tx, r, &c, now)?), true),
        "move" | "copy" => (Json::from(transfer(tx, r, &c, now)?), true),
        "preserve" => (Json::from(preserve(tx, r, &c, now)?), true),
        "trash" | "restore" => trash_change(tx, r, &c, now)?,
        _ => return Err(Fault::Invalid),
    };
    if changed {
        metadata::advance(tx, now, debounce)?;
    }
    Ok(reply(200, body))
}
pub(super) fn trash_list(tx: &Transaction<'_>, r: &Request) -> Result<Reply, Fault> {
    let limit = r.limit.unwrap_or(100);
    if limit == 0 || limit > 100 {
        return Err(Fault::Invalid);
    }
    let cursor = r.cursor.as_deref().unwrap_or("");
    let after = if cursor.is_empty() {
        rules::TRASH_PREFIX.to_owned()
    } else {
        format!(
            "{}{}.json",
            rules::TRASH_PREFIX,
            rules::normalize_uuid(cursor).map_err(|_| Fault::Conflict)?
        )
    };
    let mut query=tx.prepare("SELECT CASE WHEN typeof(path)='text' AND length(CAST(path AS BLOB))<=4096 THEN path END,typeof(data),length(data) FROM files WHERE substr(path,1,?)=? AND path>? ORDER BY path LIMIT ?")?;
    let mut records = Vec::new();
    let mut more = false;
    for row in query.query_map(
        params![
            rules::TRASH_PREFIX.len() as i64,
            rules::TRASH_PREFIX,
            after,
            limit + 1
        ],
        |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
            ))
        },
    )? {
        let (path, storage, size) = row?;
        if records.len() >= limit as usize {
            more = true;
            break;
        }
        let path = path.ok_or(Fault::Conflict)?;
        let id = path
            .strip_prefix(rules::TRASH_PREFIX)
            .and_then(|s| s.strip_suffix(".json"))
            .ok_or(Fault::Conflict)?;
        let id = rules::normalize_uuid(id).map_err(|_| Fault::Conflict)?;
        let decoded = if storage == "blob" && (0..=MAX_CAPSULE_BYTES).contains(&size) {
            read_capsule(tx, &path, &id)
        } else {
            Err(Fault::Conflict)
        };
        let item = match decoded {
            Ok(Some(capsule)) => summary(&capsule.encoded, None)?,
            Ok(None) => return Err(Fault::Schema),
            Err(Fault::Conflict) => Json::from(
                serde_json::json!({"id":id,"path":null,"version":null,"state":"invalid","trashedAt":null,"invalid":true}),
            ),
            Err(fault) => return Err(fault),
        };
        records.push(item);
    }
    let next = if more {
        records
            .last()
            .and_then(|v| v.get("id"))
            .cloned()
            .unwrap_or(Json::Null)
    } else {
        Json::Null
    };
    Ok(reply(
        200,
        Json::Object(std::collections::BTreeMap::from([
            ("records".into(), Json::Array(records)),
            ("next".into(), next),
        ])),
    ))
}
