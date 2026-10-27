//! Metadata-only workspace reads. SQLite checks stored scalar sizes before any
//! path/version is decoded into Rust; stat streams its sorted identity lines and
//! list returns at most 501 visible immediate children to the Rust planner.
use super::{config, reply, Fault, Reply, Request};
use rusqlite::{params, Transaction};
use serde_json::Value;
use sha2::{Digest, Sha256};
// Backend keys include the configured corpus root; response paths remain relative.
fn read_path(r: &Request, directory: bool) -> Result<(String, String), Fault> {
    let raw = r.source.as_deref().unwrap_or("");
    let relative = if directory {
        if raw.chars().count() > 500
            || raw.contains(['\\', '\0'])
            || (!raw.is_empty()
                && raw
                    .split('/')
                    .any(|p| p.is_empty() || matches!(p, "." | "..") || p.starts_with('.')))
        {
            return Err(Fault::Invalid);
        }
        raw.to_owned()
    } else {
        diary_workspace_rules::rel_path(raw, false).map_err(|_| Fault::Invalid)?
    };
    let root = config(r)?.remote_root;
    let root = diary_workspace_rules::rel_path(&root, true).map_err(|_| Fault::Invalid)?;
    Ok((
        relative.clone(),
        diary_workspace_rules::join(&root, &[&relative]),
    ))
}

// A backend key is at most a 500-codepoint configured root plus a slash and a
// 500-codepoint relative path. Byte bounds also cover embedded-NUL storage.
pub(super) const MAX_KEY_BYTES: i64 = 4004;
pub(super) fn admit(tx: &Transaction<'_>, full: &str, timestamps: bool) -> Result<(), Fault> {
    let prefix = if full.is_empty() {
        String::new()
    } else {
        format!("{full}/")
    };
    let n = i64::try_from(prefix.chars().count()).map_err(|_| Fault::Capacity)?;
    // CASE makes NULL/wrong-type fields explicit refusals. Only booleans cross
    // this boundary, including when a corrupt stored scalar is very large.
    let invalid: bool = tx.query_row(r#"
        SELECT EXISTS(SELECT 1 FROM files WHERE (typeof(path)!='text' OR path=?1 OR substr(path,1,?2)=?3)
          AND CASE WHEN typeof(path)='text' AND length(CAST(path AS BLOB)) BETWEEN 1 AND ?4
          AND substr(path,1,1)!='/' AND substr(path,-1)!='/' AND instr(path,char(0))=0 AND instr(path,char(92))=0
          AND instr('/'||path||'/','/./')=0 AND instr('/'||path||'/','/../')=0 AND instr(path,'//')=0
          AND typeof(version)='text' AND length(CAST(version AS BLOB))=64 AND instr(version,char(0))=0 AND version NOT GLOB '*[^0-9a-f]*'
          AND (?5=0 OR (typeof(updated)='real' AND abs(updated)<=1.7976931348623157e308))
          THEN 0 ELSE 1 END)
        OR EXISTS(SELECT 1 FROM directories WHERE (typeof(path)!='text' OR path=?1 OR substr(path,1,?2)=?3)
          AND CASE WHEN typeof(path)='text' AND length(CAST(path AS BLOB)) BETWEEN 1 AND ?4
          AND substr(path,1,1)!='/' AND substr(path,-1)!='/' AND instr(path,char(0))=0 AND instr(path,char(92))=0
          AND instr('/'||path||'/','/./')=0 AND instr('/'||path||'/','/../')=0 AND instr(path,'//')=0
          THEN 0 ELSE 1 END)
    "#, params![full,n,prefix,MAX_KEY_BYTES,timestamps], |row|row.get(0))?;
    if invalid {
        return Err(Fault::Schema);
    }
    hierarchy(tx, full)
}

/// Metadata-only namespace consistency for this subtree and its ancestor line.
/// Prefix ranges use the validated BINARY primary-key order; no body is read.
pub(super) fn hierarchy(tx: &Transaction<'_>, full: &str) -> Result<(), Fault> {
    let prefix = if full.is_empty() {
        String::new()
    } else {
        format!("{full}/")
    };
    let n = i64::try_from(prefix.chars().count()).map_err(|_| Fault::Capacity)?;
    let corrupt:bool=tx.query_row(r#"
        SELECT EXISTS(SELECT 1 FROM files f
          WHERE (f.path=?1 OR substr(f.path,1,?2)=?3) AND (
            EXISTS(SELECT 1 FROM directories d WHERE d.path=f.path)
            OR EXISTS(SELECT 1 FROM files c WHERE c.path>=f.path||'/' AND c.path<f.path||'0')
            OR EXISTS(SELECT 1 FROM directories c WHERE c.path>=f.path||'/' AND c.path<f.path||'0')))
    "#,params![full,n,prefix],|r|r.get(0))?;
    if corrupt {
        return Err(Fault::Schema);
    }
    let occupied:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM files WHERE path=?1 OR substr(path,1,?2)=?3 UNION ALL SELECT 1 FROM directories WHERE path=?1 OR substr(path,1,?2)=?3)",params![full,n,prefix],|r|r.get(0))?;
    for (offset, c) in full.char_indices() {
        if c != '/' {
            continue;
        }
        let ancestor = full.get(..offset).ok_or(Fault::Internal)?;
        let file: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM files WHERE path=?)",
            [ancestor],
            |r| r.get(0),
        )?;
        if file {
            // A missing target below an otherwise valid file is a normal
            // missing/parent-conflict policy case, not corrupt existing data.
            let ambiguous:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM directories WHERE path=?1) OR EXISTS(SELECT 1 FROM files WHERE path>=?1||'/' AND path<?1||'0') OR EXISTS(SELECT 1 FROM directories WHERE path>=?1||'/' AND path<?1||'0')",[ancestor],|r|r.get(0))?;
            if occupied || ambiguous {
                return Err(Fault::Schema);
            }
        }
    }
    Ok(())
}

pub(super) fn stat(tx: &Transaction<'_>, r: &Request) -> Result<Reply, Fault> {
    let (relative, full) = read_path(r, false)?;
    admit(tx, &full, false)?;
    let prefix = format!("{full}/");
    let n = i64::try_from(prefix.chars().count()).map_err(|_| Fault::Capacity)?;
    let offset = i64::try_from(full.chars().count()).map_err(|_| Fault::Capacity)? + 1;
    // Each line is bounded by admitted metadata. SQLite sorts the same complete
    // UTF-8 strings as Python; no subtree-sized vector or BLOB is held in Rust.
    let mut query = tx.prepare(
        r#"
        SELECT line FROM (
          SELECT 'f'||char(9)||substr(path,?4)||char(9)||version AS line
            FROM files WHERE path=?1 OR substr(path,1,?2)=?3
          UNION ALL
          SELECT 'd'||char(9)||substr(path,?4) AS line
            FROM directories WHERE path=?1 OR substr(path,1,?2)=?3
        ) ORDER BY line COLLATE BINARY
    "#,
    )?;
    let mut hash = Sha256::new();
    let mut entries = 0u64;
    for row in query.query_map(params![full, n, prefix, offset], |row| {
        row.get::<_, String>(0)
    })? {
        let line = row?;
        if entries != 0 {
            hash.update(b"\n");
        }
        hash.update(line.as_bytes());
        entries = entries.checked_add(1).ok_or(Fault::Capacity)?;
    }
    if entries == 0 {
        return Err(Fault::Missing);
    }
    use rusqlite::OptionalExtension;
    let exact: Option<String> = tx
        .query_row("SELECT version FROM files WHERE path=?", [&full], |row| {
            row.get(0)
        })
        .optional()?;
    let is_dir = exact.is_none();
    let version = if let Some(version) = exact {
        entries = 1;
        version
    } else {
        format!("{:x}", hash.finalize())
    };
    Ok(reply(
        200,
        serde_json::json!({"path":relative,"isDir":is_dir,"version":version,"entries":entries}),
    ))
}

pub(super) fn list(tx: &Transaction<'_>, r: &Request) -> Result<Reply, Fault> {
    let (relative, full) = read_path(r, true)?;
    admit(tx, &full, true)?;
    let children = list_rows(tx, &full)?;
    if children.len() > 500 {
        return Ok(reply(413, serde_json::json!({"error":"capacity"})));
    }
    // Reuse the pinned Unicode15/Final_Sigma implementation. Newer host
    // Unicode can change both keys and case-tie order for valid stored names.
    let mut keyed = children
        .into_iter()
        .map(|row| {
            Ok((
                diary_entity_focus::python_lower(&row.0).map_err(|_| Fault::Capacity)?,
                row,
            ))
        })
        .collect::<Result<Vec<_>, Fault>>()?;
    keyed.sort_by(|a, b| a.0.cmp(&b.0));
    let mut items = Vec::with_capacity(keyed.len());
    for (_, (name, is_dir, modified)) in keyed {
        let path = if relative.is_empty() {
            name.clone()
        } else {
            format!("{relative}/{name}")
        };
        let mut item = serde_json::json!({"path":path,"name":name,"isDir":is_dir});
        if let Some(modified) = modified.filter(|v| *v > 0.0) {
            item.as_object_mut()
                .ok_or(Fault::Internal)?
                .insert("modified".into(), serde_json::json!(modified));
        }
        items.push(item);
    }
    Ok(reply(200, Value::Array(items)))
}

type Child = (String, bool, Option<f64>);
fn list_rows(tx: &Transaction<'_>, full: &str) -> Result<Vec<Child>, Fault> {
    let prefix = if full.is_empty() {
        String::new()
    } else {
        format!("{full}/")
    };
    let n = i64::try_from(prefix.chars().count()).map_err(|_| Fault::Capacity)?;
    // The frozen backend visits file rows then directory rows. Last occurrence
    // replaces metadata without changing a name's first insertion position.
    // Grouping and the visible-result bound happen inside SQLite, before Rust
    // allocates child entries. NOT INDEXED fixes the rowid-table scan observed
    // for the frozen CPython 3.12 / SQLite 3.53.1 oracle. Other source planner
    // profiles require qualification; this is not a universal tie-order claim.
    let mut query = tx.prepare(
        r#"
      WITH raw AS (
        SELECT substr(path,?1+1) tail,version IS NULL directory,updated,0 kind,rowid seq
          FROM files NOT INDEXED WHERE substr(path,1,?1)=?2
        UNION ALL
        SELECT substr(path,?1+1),1,NULL,1,rowid
          FROM directories NOT INDEXED WHERE substr(path,1,?1)=?2
      ), children AS (
        SELECT CASE WHEN instr(tail,'/')=0 THEN tail ELSE substr(tail,1,instr(tail,'/')-1) END name,
          (directory OR instr(tail,'/')>0) directory,updated,kind,seq FROM raw WHERE tail!=''
      ), ranked AS (
        SELECT name,directory,updated,
          row_number() OVER (PARTITION BY name ORDER BY kind DESC,seq DESC) last_row,
          first_value(kind) OVER (PARTITION BY name ORDER BY kind,seq) first_kind,
          first_value(seq) OVER (PARTITION BY name ORDER BY kind,seq) first_seq
        FROM children
      )
      SELECT name,directory,updated FROM ranked
      WHERE last_row=1 AND substr(name,1,1)!='.' AND (directory OR lower(substr(name,-3))='.md')
      ORDER BY first_kind,first_seq LIMIT 501
    "#,
    )?;
    let mut children = Vec::new();
    for row in query.query_map(params![n, prefix], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, bool>(1)?,
            row.get::<_, Option<f64>>(2)?,
        ))
    })? {
        children.push(row?);
    }
    Ok(children)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_fixture::{config, fixture, TENANT};
    use rusqlite::Connection;
    fn request(path: &str, op: &str, source: &str) -> Result<Request, Box<dyn std::error::Error>> {
        let mut cfg = config();
        *cfg.get_mut("remoteRoot").ok_or("root")? = serde_json::json!("");
        Ok(serde_json::from_value(
            serde_json::json!({"version":1,"dbPath":path,"expectedTenant":TENANT,"op":op,"source":source,"config":cfg}),
        )?)
    }
    #[test]
    fn corrupt_read_scalars_refuse_without_repair() -> Result<(), Box<dyn std::error::Error>> {
        for field in ["path", "version"] {
            let (_dir, path) = fixture()?;
            let db = Connection::open(&path)?;
            let value = if field == "path" {
                format!("Notes/bad\0{}", "x".repeat(100_000))
            } else {
                format!("{}\0{}", "0".repeat(64), "x".repeat(100_000))
            };
            db.execute(&format!("UPDATE files SET {field}=?"), [&value])?;
            for op in ["stat", "list"] {
                assert_eq!(
                    crate::execute(&request(&path, op, "Notes")?).err(),
                    Some(Fault::Schema),
                    "{op}/{field}"
                );
            }
            assert_eq!(
                db.query_row::<i64, _, _>(
                    &format!("SELECT length(CAST({field} AS BLOB)) FROM files"),
                    [],
                    |r| r.get(0)
                )?,
                i64::try_from(value.len())?
            );
        }
        Ok(())
    }
    #[test]
    fn ambiguous_null_or_blob_identity_is_visible() -> Result<(), Box<dyn std::error::Error>> {
        for sql in [
            "INSERT INTO files VALUES(NULL,x'78',?,0)",
            "INSERT INTO files VALUES(x'4e6f7465732f6261642e6d64',x'78',?,0)",
        ] {
            let (_dir, path) = fixture()?;
            let db = Connection::open(&path)?;
            db.execute(sql, ["0".repeat(64)])?;
            for op in ["stat", "list"] {
                assert_eq!(
                    crate::execute(&request(&path, op, "Notes")?).err(),
                    Some(Fault::Schema)
                );
            }
        }
        Ok(())
    }
    #[test]
    fn trailing_slash_keys_refuse_before_projection() -> Result<(), Box<dyn std::error::Error>> {
        let mut accepted = Vec::new();
        for table in ["files", "directories"] {
            for key in ["Notes/", "Notes/child/"] {
                let (_dir, path) = fixture()?;
                let db = Connection::open(&path)?;
                if table == "files" {
                    db.execute(
                        "INSERT INTO files VALUES(?,x'78',?,0.0)",
                        params![key, "0".repeat(64)],
                    )?;
                } else {
                    db.execute("INSERT INTO directories VALUES(?)", [key])?;
                }
                for op in ["stat", "list"] {
                    if crate::execute(&request(&path, op, "Notes")?).err() != Some(Fault::Schema) {
                        accepted.push(format!("{table}/{key}/{op}"));
                    }
                }
                assert_eq!(
                    db.query_row::<i64, _, _>(
                        &format!("SELECT COUNT(*) FROM {table} WHERE path=?"),
                        [key],
                        |row| row.get(0)
                    )?,
                    1
                );
            }
        }
        assert!(
            accepted.is_empty(),
            "admission accepted trailing-slash keys: {accepted:?}"
        );
        Ok(())
    }
    #[test]
    fn unrelated_corrupt_metadata_and_blobs_are_not_read() -> Result<(), Box<dyn std::error::Error>>
    {
        let (_dir, path) = fixture()?;
        let db = Connection::open(&path)?;
        db.execute(
            "INSERT INTO files VALUES('Elsewhere/bad.md',zeroblob(2000000),?,0)",
            [format!("{}\0{}", "0".repeat(64), "x".repeat(100_000))],
        )?;
        for op in ["stat", "list"] {
            assert_eq!(crate::execute(&request(&path, op, "Notes")?)?.status(), 200);
        }
        Ok(())
    }
    #[test]
    fn visible_projection_is_bounded_and_stat_streams_large_folder(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let (_dir, path) = fixture()?;
        let mut db = Connection::open(&path)?;
        let tx = db.transaction()?;
        let version = "0".repeat(64);
        for i in 0..1000 {
            tx.execute(
                "INSERT INTO files VALUES(?,x'78',?,1.0)",
                params![format!("Notes/.hidden-{i}.md"), version],
            )?;
            tx.execute(
                "INSERT INTO files VALUES(?,x'78',?,1.0)",
                params![format!("Notes/ignored-{i}.txt"), version],
            )?;
        }
        assert_eq!(list_rows(&tx, "Notes")?.len(), 1);
        for i in 0..499 {
            tx.execute(
                "INSERT INTO files VALUES(?,x'78',?,1.0)",
                params![format!("Notes/file-{i}.md"), version],
            )?;
        }
        assert_eq!(list_rows(&tx, "Notes")?.len(), 500);
        assert_eq!(
            list(&tx, &request(&path, "list", "Notes")?)?
                .body()
                .to_value()?
                .as_array()
                .ok_or("array")?
                .len(),
            500
        );
        for i in 499..700 {
            tx.execute(
                "INSERT INTO files VALUES(?,x'78',?,1.0)",
                params![format!("Notes/file-{i}.md"), version],
            )?;
        }
        assert_eq!(list_rows(&tx, "Notes")?.len(), 501);
        assert_eq!(list(&tx, &request(&path, "list", "Notes")?)?.status(), 413);
        let result = stat(&tx, &request(&path, "stat", "Notes")?)?;
        assert_eq!(
            result.body().to_value()?.get("entries"),
            Some(&serde_json::json!(2702))
        );
        tx.rollback()?;
        Ok(())
    }
    #[test]
    fn invalid_lastmod_refuses_and_stat_identity_still_uses_only_path_version(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let (_dir, path) = fixture()?;
        let db = Connection::open(&path)?;
        db.execute("UPDATE files SET updated=?", [f64::INFINITY])?;
        assert_eq!(
            crate::execute(&request(&path, "list", "Notes")?).err(),
            Some(Fault::Schema)
        );
        assert_eq!(
            crate::execute(&request(&path, "stat", "Notes")?)?.status(),
            200
        );
        Ok(())
    }
}
