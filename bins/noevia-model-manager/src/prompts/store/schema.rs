//! Full Python database schema bootstrap on the existing secure Store connection.
//! Only the three historical bench_result additions are migrated. Incompatible schemas refuse.
use super::{sql, sql_keyword, Error, Result, Store};
use rusqlite::OptionalExtension;
use serde_json::{json, Value};

const SQL: &str = include_str!("../db_schema.sql");
const EXPECTED: &str = include_str!("../db_schema.json");
const ADDED: [(&str, &str); 3] = [
    ("ttft_answer_ms", "REAL"),
    ("truncated", "INTEGER NOT NULL DEFAULT 0"),
    ("response_text", "TEXT NOT NULL DEFAULT ''"),
];

fn columns(db: &rusqlite::Connection, table: &str) -> Result<Vec<Value>> {
    let mut statement = sql(db.prepare(
        r#"SELECT name,type,"notnull",dflt_value,pk,hidden FROM pragma_table_xinfo(?1)"#,
    ))?;
    let mut rows = sql(statement.query([table]))?;
    let mut out = Vec::new();
    while let Some(row) = sql(rows.next())? {
        if out.len() >= 256 {
            return Err(Error::Limit);
        }
        if sql(row.get::<_, i64>(5))? != 0 {
            return Err(Error::Store);
        }
        out.push(json!([
            sql(row.get::<_, String>(0))?,
            sql(row.get::<_, String>(1))?,
            sql(row.get::<_, i64>(2))?,
            sql(row.get::<_, Option<String>>(3))?,
            sql(row.get::<_, i64>(4))?
        ]));
    }
    Ok(out)
}

fn validate(store: &Store) -> Result<()> {
    let expected: Value = serde_json::from_str(EXPECTED).map_err(|_| Error::Store)?;
    for (table, definition) in expected.as_object().ok_or(Error::Store)? {
        store.check()?;
        let create: Option<String> = sql(store
            .db
            .query_row(
                "SELECT sql FROM sqlite_schema WHERE type='table' AND name=?1",
                [table],
                |r| r.get(0),
            )
            .optional())?;
        let create = create.ok_or(Error::Store)?;
        if create.len() > 8192 {
            return Err(Error::Limit);
        }
        // These constraints are absent from the source schema and could change inserts or reads.
        if [
            b"CHECK".as_slice(),
            b"REFERENCES",
            b"CONFLICT",
            b"COLLATE",
            b"GENERATED",
            b"WITHOUT",
            b"STRICT",
        ]
        .iter()
        .any(|word| sql_keyword(&create, word))
        {
            return Err(Error::Store);
        }
        if super::autoincrement_keyword(&create)
            != definition
                .get("autoincrement")
                .and_then(Value::as_bool)
                .ok_or(Error::Store)?
        {
            return Err(Error::Store);
        }
        let actual = columns(&store.db, table)?;
        let wanted = definition
            .get("columns")
            .and_then(Value::as_array)
            .ok_or(Error::Store)?;
        if actual.len() != wanted.len()
            || wanted.iter().any(|column| {
                actual.iter().find(|found| found.get(0) == column.get(0)) != Some(column)
            })
        {
            return Err(Error::Store);
        }
        let triggers: i64 = sql(store.db.query_row(
            "SELECT count(*) FROM sqlite_schema WHERE type='trigger' AND tbl_name=?1",
            [table],
            |r| r.get(0),
        ))?;
        if triggers != 0 {
            return Err(Error::Store);
        }
        let indexes = definition
            .get("indexes")
            .and_then(Value::as_object)
            .ok_or(Error::Store)?;
        let mut query = sql(store
            .db
            .prepare(r#"SELECT name,"unique",origin,partial FROM pragma_index_list(?1)"#))?;
        let mut rows = sql(query.query([table]))?;
        let mut found = 0;
        let mut total = 0;
        while let Some(row) = sql(rows.next())? {
            total += 1;
            if total > 256 {
                return Err(Error::Limit);
            }
            let name: String = sql(row.get(0))?;
            let unique: i64 = sql(row.get(1))?;
            let Some(want) = indexes.get(&name) else {
                if unique != 0 {
                    return Err(Error::Store);
                }
                continue; // Preserve additional non-unique indexes, never drop or rebuild them.
            };
            found += 1;
            if want.get("unique") != Some(&json!(unique))
                || want.get("origin") != Some(&json!(sql(row.get::<_, String>(2))?))
                || want.get("partial") != Some(&json!(sql(row.get::<_, i64>(3))?))
            {
                return Err(Error::Store);
            }
            let mut info = sql(store
                .db
                .prepare("SELECT name,desc,coll,key FROM pragma_index_xinfo(?1)"))?;
            let mut info_rows = sql(info.query([&name]))?;
            let mut parts = Vec::new();
            while let Some(part) = sql(info_rows.next())? {
                if parts.len() >= 256 {
                    return Err(Error::Limit);
                }
                parts.push(json!([
                    sql(part.get::<_, Option<String>>(0))?,
                    sql(part.get::<_, i64>(1))?,
                    sql(part.get::<_, String>(2))?,
                    sql(part.get::<_, i64>(3))?
                ]));
            }
            if want.get("columns") != Some(&json!(parts)) {
                return Err(Error::Store);
            }
        }
        if found != indexes.len() {
            return Err(Error::Store);
        }
    }
    store.check()
}

pub(super) fn initialize(store: &Store) -> Result<()> {
    initialize_before_commit(store, || Ok(()))
}
fn initialize_before_commit(
    store: &Store,
    before_commit: impl FnOnce() -> Result<()>,
) -> Result<()> {
    store.check()?;
    // Same connection, progress handler, deadline/cancel flag and lifetime writer lock.
    // Drop rolls back CREATEs and ALTERs on every error, including cancellation before commit.
    let transaction = sql(store.db.unchecked_transaction())?;
    sql(transaction.execute_batch(SQL))?;
    let existing = columns(&transaction, "bench_result")?;
    for (name, declaration) in ADDED {
        if !existing
            .iter()
            .any(|column| column.get(0).and_then(Value::as_str) == Some(name))
        {
            store.check()?;
            sql(transaction.execute_batch(&format!(
                "ALTER TABLE bench_result ADD COLUMN {name} {declaration}"
            )))?;
        }
    }
    validate(store)?;
    before_commit()?;
    store.check()?; // Recheck after validation and immediately before committing any schema work.
    sql(transaction.commit())?;
    store.check()
}

#[cfg(test)]
mod tests;
