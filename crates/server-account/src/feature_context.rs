//! Shared feature-runtime process fence and typed producer-input provenance.
//! The private/public transport and session controller must share THIS instance.
//! No HTTP owner or device admission is activated by constructing this component.
use rand_core::RngCore;
use server_store::{
    Reader, RustFeatureSettings, StoreError, WriteTx, Writer, FEATURE_RUNTIME_INCARNATION_KEY,
};
use sha2::{Digest, Sha256};
use std::sync::Mutex;
mod lifecycle;
pub mod startup;
pub mod wire;
pub use lifecycle::{
    Admission, BootFacts, BrowserFact, Cancellation, ChoiceReason, Control, Controller,
    Registration, Reply, Snapshot,
};

pub const SOURCE_LIMIT: i64 = 1024 * 1024;
fn unavailable() -> StoreError {
    StoreError::NotReady("feature runtime is unavailable".into())
}
fn valid_incarnation(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        && value.bytes().any(|b| b != b'0')
}
const EPOCH_READ: &str = "SELECT typeof(value), CASE WHEN typeof(value)='text' AND length(CAST(value AS BLOB))=32 THEN value ELSE NULL END FROM settings WHERE key=?1";
fn stored_incarnation(reader: &Reader<'_>) -> Result<Option<String>, StoreError> {
    let row = reader.row(EPOCH_READ, [FEATURE_RUNTIME_INCARNATION_KEY], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
    })?;
    match row {
        None => Ok(None),
        Some((kind, Some(value))) if kind == "text" && valid_incarnation(&value) => Ok(Some(value)),
        Some(_) => Err(unavailable()),
    }
}
struct State {
    initialized: bool,
    retired: bool,
    /// Outer None: no successful observation; inner None: observed absent metadata.
    first_observation: Option<Option<String>>,
}
pub struct FeatureRuntime {
    incarnation: String,
    operations: lifecycle::Gate,
    state: Mutex<State>,
}
impl std::fmt::Debug for FeatureRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FeatureRuntime { .. }")
    }
}
impl FeatureRuntime {
    pub fn new() -> Result<Self, StoreError> {
        let mut bytes = [0_u8; 16];
        rand_core::OsRng
            .try_fill_bytes(&mut bytes)
            .map_err(|_| unavailable())?;
        if bytes.iter().all(|b| *b == 0) {
            return Err(unavailable());
        }
        let incarnation = bytes.iter().map(|b| format!("{b:02x}")).collect();
        Ok(Self::with_incarnation(incarnation))
    }
    fn with_incarnation(incarnation: String) -> Self {
        Self {
            incarnation,
            operations: lifecycle::Gate::default(),
            state: Mutex::new(State {
                initialized: false,
                retired: false,
                first_observation: None,
            }),
        }
    }
    /// Private-protocol identity; never log this value or use container start as its replacement.
    pub fn incarnation(&self) -> &str {
        &self.incarnation
    }
    /// Controller permit MUST precede Writer; no inner auth callback acquires it.
    fn operation_permit(&self) -> Result<lifecycle::Permit<'_>, StoreError> {
        self.operations.acquire()
    }
    /// Existing schema only: Writer itself opens without CREATE. This publishes no session Ready.
    pub fn initialize(&self, writer: &Writer) -> Result<(), StoreError> {
        let _operation = self.operation_permit()?;
        self.initialize_under_permit(writer)
    }
    fn initialize_under_permit(&self, writer: &Writer) -> Result<(), StoreError> {
        writer.write(|tx| {
            let observed = stored_incarnation(&tx.reader())?;
            let mut state = self.state.lock().map_err(|_| StoreError::Poisoned)?;
            if state.retired {
                return Err(unavailable());
            }
            if state.initialized {
                if observed.as_deref() != Some(self.incarnation.as_str()) {
                    state.retired = true;
                    return Err(unavailable());
                }
                return Ok(());
            }
            if let Some(expected) = &state.first_observation {
                if &observed != expected && observed.as_deref() != Some(self.incarnation.as_str()) {
                    state.retired = true;
                    return Err(unavailable());
                }
            } else {
                state.first_observation = Some(observed.clone());
            }
            tx.set_feature_runtime_incarnation(RustFeatureSettings::owned(), &self.incarnation)?;
            Ok(())
        })?;
        // Writer's commit happens AFTER its callback. Never mark initialized from that callback.
        writer.read(|reader| {
            let observed = stored_incarnation(reader)?;
            let mut state = self.state.lock().map_err(|_| StoreError::Poisoned)?;
            if state.retired || observed.as_deref() != Some(self.incarnation.as_str()) {
                state.retired = true;
                return Err(unavailable());
            }
            Ok(())
        })?;
        let mut state = self.state.lock().map_err(|_| StoreError::Poisoned)?;
        if state.retired {
            return Err(unavailable());
        }
        state.initialized = true;
        Ok(())
    }
    /// Call INSIDE the same Writer/read transaction as the protected operation.
    /// This is only the durable process fence; live-session/device/credential checks remain required.
    pub fn check_incarnation(&self, reader: &Reader<'_>) -> Result<(), StoreError> {
        // A pre-initialization read snapshot may still see the old row. Refuse that read,
        // but only a current IMMEDIATE writer observation may permanently retire this instance.
        self.check_epoch(reader, false)
    }
    pub fn check_write_incarnation(&self, tx: &WriteTx<'_>) -> Result<(), StoreError> {
        self.check_epoch(&tx.reader(), true)
    }
    fn check_epoch(&self, reader: &Reader<'_>, retire_on_mismatch: bool) -> Result<(), StoreError> {
        let observed = stored_incarnation(reader)?;
        let mut state = self.state.lock().map_err(|_| StoreError::Poisoned)?;
        if state.retired || !state.initialized {
            return Err(unavailable());
        }
        if observed.as_deref() != Some(self.incarnation.as_str()) {
            if retire_on_mismatch {
                state.retired = true;
            }
            return Err(unavailable());
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContextSource {
    Decision,
    InferenceMachines,
}
impl ContextSource {
    pub const fn key(self) -> &'static str {
        match self {
            Self::Decision => "decision:configuration",
            Self::InferenceMachines => "inference:machines",
        }
    }
}
/// SHA256 of exact ASCII key NUL SQLite typeof NUL uppercase SQLite hex(CAST(value AS BLOB)).
/// Missing rows use kind=missing and empty bytes; SQL NULL uses kind=null and empty bytes.
/// CASE limits the SQL result BEFORE raw/hex payload allocation on the Rust side.
pub fn source_fingerprint(
    reader: &Reader<'_>,
    source: ContextSource,
) -> Result<String, StoreError> {
    let row = reader.row(
        "SELECT typeof(value), length(CAST(value AS BLOB)), CASE WHEN value IS NULL OR length(CAST(value AS BLOB))<=1048576 THEN hex(CAST(value AS BLOB)) ELSE NULL END FROM settings WHERE key=?1",
        [source.key()],
        |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<i64>>(1)?, r.get::<_, Option<String>>(2)?)),
    )?;
    let (kind, bytes) = match row {
        None => ("missing".to_string(), String::new()),
        Some((kind, size, Some(bytes))) if size.is_none_or(|n| (0..=SOURCE_LIMIT).contains(&n)) => {
            (kind, bytes)
        }
        Some(_) => return Err(unavailable()),
    };
    let mut digest = Sha256::new();
    digest.update(source.key().as_bytes());
    digest.update([0]);
    digest.update(kind.as_bytes());
    digest.update([0]);
    digest.update(bytes.as_bytes());
    Ok(format!("{:x}", digest.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use server_store::{db_path, rusqlite::Connection, RustAuth};
    type Error = Box<dyn std::error::Error>;
    fn fixture() -> Result<(tempfile::TempDir, Connection, Writer), Error> {
        let dir = tempfile::tempdir()?;
        let node = Connection::open(db_path(dir.path()))?;
        node.execute_batch(include_str!(
            "../../server-store/tests/fixtures/node-schema.sql"
        ))?;
        let writer = Writer::open(dir.path(), RustAuth::owned())?;
        Ok((dir, node, writer))
    }
    fn epoch(byte: u8) -> FeatureRuntime {
        FeatureRuntime::with_incarnation(format!("{byte:02x}").repeat(16))
    }
    #[test]
    fn epoch_projection_bounds_actual_text_bytes_before_materialization() -> Result<(), Error> {
        let (_dir, node, writer) = fixture()?;
        let valid = "ab".repeat(16);
        let nul_suffix = format!("{valid}\0{}", "z".repeat(2 * 1024 * 1024));
        for (raw, accepted) in [
            (valid, true),
            (format!("{}é", "a".repeat(31)), false),
            ("a".repeat(33), false),
            (nul_suffix, false),
        ] {
            node.execute(
                "INSERT OR REPLACE INTO settings(key,value) VALUES(?1,?2)",
                (FEATURE_RUNTIME_INCARNATION_KEY, &raw),
            )?;
            // Inspect the SAME production CASE: malformed text is suppressed inside SQLite,
            // before conversion to a Rust String, including SQLite length(TEXT)'s NUL case.
            let projected = writer.read(|reader| {
                reader.row(EPOCH_READ, [FEATURE_RUNTIME_INCARNATION_KEY], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
                })
            })?;
            assert_eq!(projected.as_ref().map(|row| row.0.as_str()), Some("text"));
            assert_eq!(projected.and_then(|row| row.1).is_some(), accepted);
            let decoded = writer.read(stored_incarnation);
            assert_eq!(decoded.is_ok(), accepted);
            assert_eq!(
                node.query_row(
                    "SELECT length(CAST(value AS BLOB)) FROM settings WHERE key=?1",
                    [FEATURE_RUNTIME_INCARNATION_KEY],
                    |r| r.get::<_, i64>(0),
                )?,
                i64::try_from(raw.len())?
            ); // refusal never repairs the input
        }
        Ok(())
    }
    #[test]
    fn actual_db_fences_old_process_and_debug_is_redacted() -> Result<(), Error> {
        let (_dir, node, writer) = fixture()?;
        let a = epoch(1);
        let b = epoch(2);
        assert!(writer.read(|r| a.check_incarnation(r)).is_err());
        a.initialize(&writer)?;
        writer.read(|r| a.check_incarnation(r))?;
        b.initialize(&writer)?;
        assert!(writer
            .write(|tx| {
                a.check_write_incarnation(tx)?;
                tx.set_feature_enabled(
                    RustFeatureSettings::owned(),
                    server_features::Feature::PlannerReview,
                    true,
                )?;
                Ok(())
            })
            .is_err());
        assert!(a.initialize(&writer).is_err());
        writer.write(|tx| {
            b.check_write_incarnation(tx)?;
            tx.set_feature_enabled(
                RustFeatureSettings::owned(),
                server_features::Feature::PlannerReview,
                false,
            )?;
            Ok(())
        })?;
        let actual: String = node.query_row(
            "SELECT value FROM settings WHERE key=?1",
            [FEATURE_RUNTIME_INCARNATION_KEY],
            |r| r.get(0),
        )?;
        assert_eq!(actual, b.incarnation());
        assert!(!format!("{a:?}").contains(a.incarnation()));
        Ok(())
    }
    #[test]
    fn stale_read_snapshot_cannot_retire_a_newly_initialized_runtime() -> Result<(), Error> {
        let (dir, _node, writer) = fixture()?;
        let identity_writer = Writer::open(dir.path(), RustAuth::owned())?;
        let runtime = epoch(6);
        identity_writer.read(|reader| {
            let _: Option<i64> =
                reader.row("SELECT count(*) FROM settings", [], |row| row.get(0))?;
            // Different real Writer/connection, as in IdentityLayer and AccountLayer. WAL permits this commit.
            runtime.initialize(&writer)?;
            assert!(runtime.check_incarnation(reader).is_err());
            Ok(())
        })?;
        writer.read(|reader| runtime.check_incarnation(reader))?;
        Ok(())
    }
    #[test]
    fn failed_install_never_admits_or_replaces_a_later_epoch() -> Result<(), Error> {
        let (_dir, node, writer) = fixture()?;
        node.execute_batch("CREATE TRIGGER abort_epoch BEFORE INSERT ON settings WHEN new.key='native:feature-runtime:incarnation' BEGIN SELECT RAISE(ABORT,'synthetic'); END;")?;
        let a = epoch(3);
        let b = epoch(4);
        assert!(a.initialize(&writer).is_err());
        assert!(writer.read(|r| a.check_incarnation(r)).is_err());
        node.execute_batch("DROP TRIGGER abort_epoch")?;
        b.initialize(&writer)?;
        assert!(a.initialize(&writer).is_err());
        writer.read(|r| b.check_incarnation(r))?;
        Ok(())
    }
    #[test]
    fn malformed_epoch_refuses_without_repair() -> Result<(), Error> {
        let (_dir, node, writer) = fixture()?;
        node.execute(
            "INSERT INTO settings VALUES(?1,?2)",
            (FEATURE_RUNTIME_INCARNATION_KEY, vec![1_u8; 32]),
        )?;
        let runtime = epoch(5);
        assert!(runtime.initialize(&writer).is_err());
        let kind: String = node.query_row(
            "SELECT typeof(value) FROM settings WHERE key=?1",
            [FEATURE_RUNTIME_INCARNATION_KEY],
            |r| r.get(0),
        )?;
        assert_eq!(kind, "blob");
        Ok(())
    }
    #[test]
    fn random_process_incarnations_are_distinct_and_valid() -> Result<(), Error> {
        let a = FeatureRuntime::new()?;
        let b = FeatureRuntime::new()?;
        assert!(valid_incarnation(a.incarnation()));
        assert!(valid_incarnation(b.incarnation()));
        assert_ne!(a.incarnation(), b.incarnation());
        Ok(())
    }
    #[test]
    fn typed_input_changes_and_oversize_refuse_before_hex_result() -> Result<(), Error> {
        let (dir, node, _writer) = fixture()?;
        node.execute_batch(
            "DROP TABLE settings; CREATE TABLE settings(key TEXT PRIMARY KEY,value);",
        )?;
        let writer = Writer::open(dir.path(), RustAuth::owned())?;
        let mut fingerprints =
            vec![writer.read(|r| source_fingerprint(r, ContextSource::Decision))?];
        for value in [
            server_store::rusqlite::types::Value::Null,
            String::new().into(),
            String::from("A").into(),
            vec![65_u8].into(),
            1_i64.into(),
            1.5_f64.into(),
        ] {
            node.execute(
                "INSERT OR REPLACE INTO settings VALUES(?1,?2)",
                (ContextSource::Decision.key(), value),
            )?;
            fingerprints.push(writer.read(|r| source_fingerprint(r, ContextSource::Decision))?);
        }
        let unique: std::collections::BTreeSet<_> = fingerprints.iter().collect();
        assert_eq!(unique.len(), fingerprints.len());
        node.execute(
            "UPDATE settings SET value=?1 WHERE key=?2",
            (
                vec![0_u8; usize::try_from(SOURCE_LIMIT)? + 1],
                ContextSource::Decision.key(),
            ),
        )?;
        assert!(writer
            .read(|r| source_fingerprint(r, ContextSource::Decision))
            .is_err());
        Ok(())
    }
}
