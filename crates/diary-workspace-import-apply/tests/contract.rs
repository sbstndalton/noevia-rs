#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
use diary_workspace_import_apply::{
    apply_with_clock, read_request, Error, Request, MAGIC, MAX_METADATA,
};
use rusqlite::{params, Connection};
use sha2::{Digest, Sha256};
const TENANT: &str = "00000000-0000-0000-0000-000000000001";
fn database() -> (tempfile::TempDir, Request) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("managed-diary.db");
    let db = Connection::open(&path).unwrap();
    db.execute_batch(include_str!("managed-schema.sql"))
        .unwrap();
    for (key, value) in [("tenant", TENANT), ("active", "1"), ("generation", "1")] {
        db.execute("INSERT INTO meta VALUES (?,?)", [key, value])
            .unwrap();
    }
    let body = include_bytes!("workspace.zip");
    let destination = "Imports/Copy".to_string();
    let mut sha = Sha256::new();
    sha.update(destination.as_bytes());
    sha.update([0]);
    sha.update(body);
    (
        dir,
        Request {
            version: 1,
            db_path: path.to_string_lossy().into(),
            expected_tenant: TENANT.into(),
            destination,
            reviewed_fingerprint: format!("{:x}", sha.finalize()),
            debounce: 3.0,
        },
    )
}
#[test]
fn timestamps_and_due_match_actual_python_text_independently() {
    let rows: Vec<(String, String, String, String)> =
        serde_json::from_str(include_str!("clock-cases.json")).unwrap();
    for (now, debounce, expected_now, expected_due) in rows {
        let (_dir, mut request) = database();
        request.debounce = f64::from_bits(u64::from_str_radix(&debounce, 16).unwrap());
        let now = f64::from_bits(u64::from_str_radix(&now, 16).unwrap());
        apply_with_clock(&request, include_bytes!("workspace.zip"), || Ok(now)).unwrap();
        let db = Connection::open(&request.db_path).unwrap();
        for (key, expected) in [("snapshot_time", expected_now), ("due", expected_due)] {
            let (actual, kind): (String, String) = db
                .query_row(
                    "SELECT value,typeof(value) FROM meta WHERE key=?",
                    [key],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            assert_eq!(actual, expected);
            assert_eq!(kind, "text");
        }
        let (actual, kind): (f64, String) = db
            .query_row("SELECT updated,typeof(updated) FROM files", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(actual, now);
        assert_eq!(kind, "real");
    }
}
#[test]
fn receipt_avoids_clock_and_later_conflicts() {
    let (_dir, request) = database();
    assert!(
        !apply_with_clock(&request, include_bytes!("workspace.zip"), || Ok(
            1700000000.0
        ))
        .unwrap()
        .already_applied
    );
    assert!(
        apply_with_clock(&request, include_bytes!("workspace.zip"), || Err(
            Error::Clock
        ))
        .unwrap()
        .already_applied
    );
}
#[test]
fn frame_requires_version_and_eof_before_database() {
    let (_dir, request) = database();
    let raw = serde_json::json!({"version":request.version,"dbPath":request.db_path,"expectedTenant":request.expected_tenant,
 "destination":request.destination,"reviewedFingerprint":request.reviewed_fingerprint,"debounce":request.debounce});
    let raw = serde_json::to_vec(&raw).unwrap();
    let body = include_bytes!("workspace.zip");
    let mut bytes = MAGIC.to_vec();
    bytes.extend((raw.len() as u32).to_le_bytes());
    bytes.extend(raw);
    bytes.extend((body.len() as u64).to_le_bytes());
    bytes.extend(body);
    assert!(read_request(bytes.as_slice()).is_ok());
    bytes.push(0);
    assert!(matches!(read_request(bytes.as_slice()), Err(Error::Frame)));
    let mut header = MAGIC.to_vec();
    header.extend(((MAX_METADATA + 1) as u32).to_le_bytes());
    assert!(matches!(read_request(header.as_slice()), Err(Error::Frame)));
}
#[test]
fn schema_and_clock_failures_rollback_without_repair() {
    let (_dir, request) = database();
    assert_eq!(
        apply_with_clock(&request, include_bytes!("workspace.zip"), || Err(
            Error::Clock
        )),
        Err(Error::Clock)
    );
    let db = Connection::open(&request.db_path).unwrap();
    assert_eq!(
        db.query_row("SELECT count(*) FROM files", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
    db.execute(
        "INSERT INTO files VALUES (?,?,?,?)",
        params!["Imports/Copy", b"x", "digest", 1.0],
    )
    .unwrap();
    assert_eq!(
        apply_with_clock(&request, include_bytes!("workspace.zip"), || Ok(1.0)),
        Err(Error::Conflict)
    );
}
