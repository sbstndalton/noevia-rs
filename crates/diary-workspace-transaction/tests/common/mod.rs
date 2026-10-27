use rusqlite::{params, Connection};
use serde_json::json;
use sha2::{Digest, Sha256};
use tempfile::tempdir;

pub const TENANT: &str = "01234567-89ab-4cde-8fab-0123456789ab";
pub const NAMESPACE: &str = "11234567-89ab-4cde-8fab-0123456789ab";
pub const OP_ID: &str = "21234567-89ab-4cde-8fab-0123456789ab";
pub const CAPSULE: &str = "31234567-89ab-4cde-8fab-0123456789ab";

pub const RECEIPT_SCHEMA: &str = "CREATE TABLE workspace_operation_receipts(id TEXT PRIMARY KEY,fingerprint TEXT NOT NULL,op TEXT NOT NULL,response BLOB NOT NULL,created REAL NOT NULL);";
pub fn base_schema() -> Result<String, Box<dyn std::error::Error>> {
    let source: serde_json::Value =
        serde_json::from_str(include_str!("../fixtures/managed-schema.json"))?;
    let mut sql = String::new();
    for table in source
        .get("tables")
        .and_then(serde_json::Value::as_array)
        .ok_or("missing tables")?
    {
        sql.push_str(
            table
                .get("sql")
                .and_then(serde_json::Value::as_str)
                .ok_or("missing source SQL")?,
        );
        sql.push_str(";\n");
    }
    sql.push_str(RECEIPT_SCHEMA);
    Ok(sql)
}
pub fn fixture() -> Result<(tempfile::TempDir, String), Box<dyn std::error::Error>> {
    fixture_with_schema(&base_schema()?)
}
pub fn fixture_with_schema(
    schema: &str,
) -> Result<(tempfile::TempDir, String), Box<dyn std::error::Error>> {
    let dir = tempdir()?;
    let path = dir.path().join("tenant.sqlite3");
    let db = Connection::open(&path)?;
    db.execute_batch(schema)?;
    db.execute_batch(
        "         INSERT INTO meta VALUES('tenant','01234567-89ab-4cde-8fab-0123456789ab');
         INSERT INTO meta VALUES('active','1');
         INSERT INTO meta VALUES('workspace_operation_namespace','11234567-89ab-4cde-8fab-0123456789ab');
         INSERT INTO meta VALUES('generation','0');
         INSERT INTO meta VALUES('due','0');
         INSERT INTO meta VALUES('snapshot_time','0');
         INSERT INTO directories VALUES('Notes');",
    )?;
    let data = b"synthetic note";
    let version = format!("{:x}", Sha256::digest(data));
    db.execute(
        "INSERT INTO files(path,data,version,updated) VALUES('Notes/one.md',?,?,0)",
        params![data.as_slice(), version],
    )?;
    drop(db);
    Ok((dir, path.to_string_lossy().into_owned()))
}

pub fn config() -> serde_json::Value {
    json!({
        "remoteRoot":"",
        "entriesPrefix":"Entries",
        "monthlyPrefix":"Monthly",
        "monthFileTemplate":"{year}-{month02}.md",
        "indexFile":"index.md",
        "monthNames":["January","February","March","April","May","June","July","August","September","October","November","December"]
    })
}
