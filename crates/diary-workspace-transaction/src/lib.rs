//! Source-only transactional workspace engine. Production activation and its
//! schema/backup migration are deliberately outside this crate.
#![forbid(unsafe_code)]
mod contract;
mod json;
pub use json::{Json, Number as JsonNumber};
mod metadata;
mod mutations;
mod reads;
mod schema;
use diary_workspace_rules::{normalize_uuid, ProtectionConfig};
use rusqlite::{
    params, Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{io::Read, path::Path, time::Duration};

pub const REQUEST_MAGIC: &[u8; 4] = b"DWT1";
pub const RESPONSE_MAGIC: &[u8; 4] = b"DWR1";
pub const END_MAGIC: &[u8; 4] = b"END1";
pub const MAX_REQUEST: usize = 2 * 1024 * 1024;
pub const MAX_RESPONSE: usize = 1024 * 1024;

/// Presence is part of mutation identity, even where execution has the same
/// default for an omitted condition and an explicit JSON null.
#[derive(Debug, Default, Clone, PartialEq, Serialize)]
#[serde(tag = "presence", content = "value", rename_all = "camelCase")]
pub enum Condition<T> {
    #[default]
    Absent,
    Null,
    Value(T),
}
impl<'de, T: Deserialize<'de>> Deserialize<'de> for Condition<T> {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        Ok(match Option::<T>::deserialize(de)? {
            Some(value) => Self::Value(value),
            None => Self::Null,
        })
    }
}
impl<T> Condition<T> {
    pub fn is_present(&self) -> bool {
        !matches!(self, Self::Absent)
    }
    pub fn value(&self) -> Option<&T> {
        match self {
            Self::Value(value) => Some(value),
            _ => None,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Request {
    pub version: u8,
    pub db_path: String,
    pub expected_tenant: String,
    #[serde(default)]
    pub operation_namespace: Option<String>,
    pub op: String,
    #[serde(default)]
    pub operation_id: Option<String>,
    #[serde(default)]
    pub capsule_id: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub destination: Option<String>,
    #[serde(default)]
    pub source_version: Condition<String>,
    #[serde(default)]
    pub destination_version: Condition<String>,
    #[serde(default)]
    pub overwrite: Condition<bool>,
    #[serde(default)]
    pub now: Option<f64>,
    #[serde(default)]
    pub debounce: Option<f64>,
    #[serde(default)]
    pub config: Option<ProtectionConfigDto>,
    #[serde(default)]
    pub limit: Option<u32>,
    #[serde(default)]
    pub cursor: Option<String>,
}
#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProtectionConfigDto {
    pub remote_root: String,
    pub entries_prefix: String,
    pub monthly_prefix: String,
    pub month_file_template: String,
    pub index_file: String,
    pub month_names: Vec<String>,
}
impl From<ProtectionConfigDto> for ProtectionConfig {
    fn from(c: ProtectionConfigDto) -> Self {
        Self {
            remote_root: c.remote_root,
            entries_prefix: c.entries_prefix,
            monthly_prefix: c.monthly_prefix,
            month_file_template: c.month_file_template,
            index_file: c.index_file,
            month_names: c.month_names,
        }
    }
}
/// A caller can inspect the result but cannot change a sealed receipt payload.
#[derive(Debug, Clone, PartialEq)]
pub struct Reply {
    status: u16,
    body: Json,
    wire_json: Option<Vec<u8>>,
}
impl Reply {
    pub fn status(&self) -> u16 {
        self.status
    }
    pub fn body(&self) -> &Json {
        &self.body
    }
    fn seal(mut self) -> Result<Self, Fault> {
        self.wire_json = Some(encode_reply(&self)?);
        Ok(self)
    }
}
struct ReceiptMetadata {
    fingerprint: Option<String>,
    op: Option<String>,
    size: Option<i64>,
    storage_type: String,
    created: Option<f64>,
}

fn receipt_metadata(tx: &Transaction<'_>, id: &str) -> Result<Option<ReceiptMetadata>, Fault> {
    // Both predicates use the existing BINARY primary-key index. A lost NULL
    // identity is unscopable; an exact BLOB encoding of this ID is ambiguous.
    // Neither may be mistaken for a new operation or repaired on a retry.
    let ambiguous:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM workspace_operation_receipts WHERE id IS NULL) OR EXISTS(SELECT 1 FROM workspace_operation_receipts WHERE typeof(id)='blob' AND id=CAST(? AS BLOB))",[id],|row|row.get(0))?;
    if ambiguous {
        return Err(Fault::Schema);
    }
    Ok(tx.query_row(concat!(
            "SELECT CASE WHEN typeof(fingerprint)='text' AND length(CAST(fingerprint AS BLOB))=64 THEN fingerprint END,",
            "CASE WHEN typeof(op)='text' AND length(CAST(op AS BLOB))<=16 THEN op END,",
            "length(response),typeof(response),CASE WHEN typeof(created)='real' THEN created END ",
            "FROM workspace_operation_receipts WHERE id=?"), [id], |row| Ok(ReceiptMetadata {
        fingerprint: row.get(0)?, op: row.get(1)?, size: row.get(2)?, storage_type: row.get(3)?, created: row.get(4)?,
    })).optional()?)
}

struct BoundedOutput {
    bytes: Vec<u8>,
    exceeded: bool,
}
impl std::io::Write for BoundedOutput {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        if self.bytes.len().saturating_add(data.len()) > MAX_RESPONSE {
            self.exceeded = true;
            return Err(std::io::Error::other("response capacity exceeded"));
        }
        self.bytes.extend_from_slice(data);
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn encode_reply(reply: &Reply) -> Result<Vec<u8>, Fault> {
    let mut output = BoundedOutput {
        bytes: Vec::new(),
        exceeded: false,
    };
    use std::io::Write;
    let body = reply.body.encode()?;
    let prefix = format!("{{\"status\":{},\"body\":", reply.status);
    for part in [prefix.as_bytes(), body.as_slice(), b"}"] {
        output.write_all(part).map_err(|_| {
            if output.exceeded {
                Fault::Capacity
            } else {
                Fault::Internal
            }
        })?;
    }
    Ok(output.bytes)
}
fn stored_reply(r: &Request, bytes: Vec<u8>, created: f64) -> Result<Reply, Fault> {
    if bytes.is_empty() || bytes.len() > MAX_RESPONSE {
        return Err(Fault::Schema);
    }
    let Json::Object(mut value) = Json::parse(&bytes)? else {
        return Err(Fault::Schema);
    };
    if value.len() != 2 {
        return Err(Fault::Schema);
    }
    let Some(Json::Number(json::Number::Integer(status))) = value.remove("status") else {
        return Err(Fault::Schema);
    };
    let status: u16 = status.parse().map_err(|_| Fault::Schema)?;
    let body = value.remove("body").ok_or(Fault::Schema)?;
    if !(200..=599).contains(&status) || body.as_object().is_none() {
        return Err(Fault::Schema);
    }
    let reply = Reply {
        status,
        body,
        wire_json: None,
    };
    // Receipts contain only canonical payloads emitted by this engine. This
    // rejects duplicate keys, alternate encodings and parsing/rounding changes;
    // it does not prove provenance of arbitrary values in an attacker-owned DB.
    if encode_reply(&reply).map_err(|_| Fault::Schema)? != bytes {
        return Err(Fault::Schema);
    }
    contract::receipt(r, &reply, Some(created))?;
    Ok(Reply {
        wire_json: Some(bytes),
        ..reply
    })
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    Frame,
    Schema,
    Tenant,
    Inactive,
    Invalid,
    Conflict,
    Precondition,
    Required,
    Missing,
    Protected,
    Capacity,
    Database,
    Internal,
}
impl Fault {
    pub fn code(self) -> &'static str {
        match self {
            Self::Frame => "frame",
            Self::Schema => "schema",
            Self::Tenant => "tenant",
            Self::Inactive => "inactive",
            Self::Invalid => "invalid",
            Self::Conflict => "conflict",
            Self::Precondition => "precondition",
            Self::Required => "required",
            Self::Missing => "missing",
            Self::Protected => "protected",
            Self::Capacity => "capacity",
            Self::Database => "database",
            Self::Internal => "internal",
        }
    }
    pub fn status(self) -> u16 {
        match self {
            Self::Invalid | Self::Frame => 400,
            Self::Protected => 403,
            Self::Missing => 404,
            Self::Conflict => 409,
            Self::Precondition => 412,
            Self::Required => 428,
            Self::Schema | Self::Tenant | Self::Inactive | Self::Database | Self::Internal => 503,
            Self::Capacity => 507,
        }
    }
}
impl std::fmt::Display for Fault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "workspace transaction refused ({})", self.code())
    }
}
impl std::error::Error for Fault {}
impl From<rusqlite::Error> for Fault {
    fn from(_: rusqlite::Error) -> Self {
        Self::Database
    }
}

/// Length-prefixed strict request; rejects trailing bytes before touching SQLite.
pub fn read_request(mut input: impl Read) -> Result<Request, Fault> {
    let mut magic = [0; 4];
    input.read_exact(&mut magic).map_err(|_| Fault::Frame)?;
    if &magic != REQUEST_MAGIC {
        return Err(Fault::Frame);
    }
    let mut n = [0; 4];
    input.read_exact(&mut n).map_err(|_| Fault::Frame)?;
    let size = u32::from_le_bytes(n) as usize;
    if size == 0 || size > MAX_REQUEST {
        return Err(Fault::Frame);
    }
    let mut data = vec![0; size];
    input.read_exact(&mut data).map_err(|_| Fault::Frame)?;
    let mut tail = [0; 1];
    if input.read(&mut tail).map_err(|_| Fault::Frame)? != 0 {
        return Err(Fault::Frame);
    }
    let r: Request = serde_json::from_slice(&data).map_err(|_| Fault::Frame)?;
    if r.version != 1 || !Path::new(&r.db_path).is_absolute() || !canonical_uuid(&r.expected_tenant)
    {
        return Err(Fault::Frame);
    }
    Ok(r)
}
pub fn write_reply(mut out: impl std::io::Write, reply: &Reply) -> Result<(), Fault> {
    let encoded;
    let b = if let Some(bytes) = &reply.wire_json {
        bytes.as_slice()
    } else {
        encoded = encode_reply(reply)?;
        &encoded
    };
    out.write_all(RESPONSE_MAGIC)
        .and_then(|_| out.write_all(&(b.len() as u32).to_le_bytes()))
        .and_then(|_| out.write_all(b))
        .and_then(|_| out.write_all(END_MAGIC))
        .and_then(|_| out.flush())
        .map_err(|_| Fault::Internal)
}

fn canonical_uuid(s: &str) -> bool {
    s.len() == 36 && normalize_uuid(s).is_ok_and(|x| x == s)
}
fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn reply(status: u16, body: impl Into<Json>) -> Reply {
    Reply {
        status,
        body: body.into(),
        wire_json: None,
    }
}
fn error(f: Fault) -> Reply {
    reply(f.status(), serde_json::json!({"error":f.code()}))
}

/// Existing source transaction boundary. Caller activation, shared schema and
/// full-backup/lifecycle integration remain separately held.
pub fn execute(r: &Request) -> Result<Reply, Fault> {
    execute_with(r, |tx, r, namespace| match r.op.as_str() {
        "stat" => reads::stat(tx, r),
        "list" => reads::list(tx, r),
        "trashList" => trash_list(tx, r),
        _ => mutate(tx, r, namespace),
    })
}
fn execute_with(
    r: &Request,
    apply: impl FnOnce(&Transaction<'_>, &Request, &str) -> Result<Reply, Fault>,
) -> Result<Reply, Fault> {
    if r.version != 1 || !Path::new(&r.db_path).is_absolute() || !canonical_uuid(&r.expected_tenant)
    {
        return Err(Fault::Frame);
    }
    let mutation = match r.op.as_str() {
        "delete" | "move" | "copy" | "preserve" | "trash" | "restore" => true,
        "stat" | "list" | "trashList" => false,
        _ => return Err(Fault::Invalid),
    };
    if mutation && !r.operation_id.as_deref().is_some_and(canonical_uuid) {
        return Err(Fault::Invalid);
    }
    if mutation {
        contract::request(r)?;
    }
    let mut db = Connection::open_with_flags(
        &r.db_path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    db.busy_timeout(Duration::from_secs(30))?;
    db.pragma_update(None, "synchronous", "FULL")?;
    let tx = db.transaction_with_behavior(if mutation {
        TransactionBehavior::Immediate
    } else {
        TransactionBehavior::Deferred
    })?;
    validate_db(&tx, r)?;
    let namespace = meta(&tx, "workspace_operation_namespace")?.ok_or(Fault::Schema)?;
    if !canonical_uuid(&namespace) {
        return Err(Fault::Schema);
    }
    if mutation && r.operation_namespace.as_deref() != Some(namespace.as_str()) {
        return Err(Fault::Conflict);
    }
    let fingerprint = if mutation {
        Some(fingerprint(r, &namespace)?)
    } else {
        None
    };
    if let Some(id) = r.operation_id.as_deref().filter(|_| mutation) {
        // Inspect bounded metadata before asking SQLite/Rust to materialize a
        // stored payload. BEGIN IMMEDIATE keeps the row stable through replay.
        if let Some(meta) = receipt_metadata(&tx, id)? {
            let ReceiptMetadata {
                fingerprint: Some(old),
                op: Some(op),
                size: Some(size),
                storage_type,
                created: Some(created),
            } = meta
            else {
                return Err(Fault::Schema);
            };
            if old.len() != 64
                || !old
                    .bytes()
                    .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
                || !created.is_finite()
                || storage_type != "blob"
                || size <= 0
                || size > MAX_RESPONSE as i64
                || !matches!(
                    op.as_str(),
                    "delete" | "move" | "copy" | "preserve" | "trash" | "restore"
                )
            {
                return Err(Fault::Schema);
            }
            if Some(old.as_str()) != fingerprint.as_deref() {
                return error(Fault::Conflict).seal();
            }
            if op != r.op {
                return Err(Fault::Schema);
            }
            let response: Vec<u8> = tx.query_row(
                "SELECT response FROM workspace_operation_receipts WHERE id=?",
                [id],
                |row| row.get(0),
            )?;
            let stored = stored_reply(r, response, created)?;
            tx.commit()?;
            return Ok(stored);
        }
    }
    if mutation {
        tx.execute_batch("SAVEPOINT workspace_operation")?;
    }
    let evaluated = apply(&tx, r, &namespace).and_then(Reply::seal);
    let result = match evaluated {
        Ok(value) => {
            if mutation {
                tx.execute_batch("RELEASE workspace_operation")?;
            }
            value
        }
        Err(fault) if terminal(fault) => {
            // A refused operation may already have issued SQL. Undo every
            // operation effect before atomically storing only its terminal reply.
            if mutation {
                tx.execute_batch("ROLLBACK TO workspace_operation; RELEASE workspace_operation")?;
            }
            error(fault).seal()?
        }
        Err(fault) => return Err(fault),
    };
    if mutation {
        contract::receipt(r, &result, r.now).map_err(|_| Fault::Internal)?;
    }
    if let Some(id) = r.operation_id.as_deref().filter(|_| mutation) {
        let created = r.now.filter(|n| n.is_finite()).ok_or(Fault::Invalid)?;
        let bytes = result.wire_json.as_deref().ok_or(Fault::Internal)?;
        tx.execute("INSERT INTO workspace_operation_receipts(id,fingerprint,op,response,created) VALUES(?,?,?,?,?)",
            params![id, fingerprint.as_deref().ok_or(Fault::Internal)?, r.op, bytes, created])?;
    }
    tx.commit()?;
    Ok(result)
}
fn terminal(fault: Fault) -> bool {
    matches!(
        fault,
        Fault::Conflict
            | Fault::Precondition
            | Fault::Required
            | Fault::Missing
            | Fault::Protected
            | Fault::Capacity
    )
}
fn fingerprint(r: &Request, namespace: &str) -> Result<String, Fault> {
    let path = |value: &Option<String>| -> Result<Option<String>, Fault> {
        value
            .as_deref()
            .map(|p| diary_workspace_rules::rel_path(p, false).map_err(|_| Fault::Invalid))
            .transpose()
    };
    let stable = serde_json::json!({"v":1,"tenant":r.expected_tenant,"namespace":namespace,"op":r.op,
        "source":path(&r.source)?,"destination":path(&r.destination)?,"sourceVersion":r.source_version,
        "destinationVersion":r.destination_version,"overwrite":r.overwrite,"capsuleId":r.capsule_id});
    let bytes = serde_json::to_vec(&stable).map_err(|_| Fault::Internal)?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn meta(tx: &Transaction<'_>, key: &str) -> Result<Option<String>, Fault> {
    let bound = match key {
        "tenant" | "workspace_operation_namespace" => Some(36),
        "active" => Some(1),
        _ => None,
    };
    if let Some(bound) = bound {
        let result:Option<Option<String>>=tx.query_row("SELECT CASE WHEN typeof(value)='text' AND length(CAST(value AS BLOB))<=? THEN value END FROM meta WHERE key=?",params![bound,key],|row|row.get(0)).optional()?;
        return match result {
            Some(Some(value)) => Ok(Some(value)),
            Some(None) => Err(Fault::Schema),
            None => Ok(None),
        };
    }
    Ok(tx
        .query_row("SELECT value FROM meta WHERE key=?", [key], |row| {
            row.get(0)
        })
        .optional()?)
}

fn validate_db(tx: &Transaction<'_>, r: &Request) -> Result<(), Fault> {
    schema::validate(tx)?;
    if meta(tx, "tenant")?.as_deref() != Some(r.expected_tenant.as_str()) {
        return Err(Fault::Tenant);
    }
    if meta(tx, "active")?.as_deref() != Some("1") {
        return Err(Fault::Inactive);
    }
    Ok(())
}
#[derive(Clone)]
struct Entry {
    data: Vec<u8>,
    version: String,
}
fn config(r: &Request) -> Result<ProtectionConfig, Fault> {
    r.config.clone().map(Into::into).ok_or(Fault::Invalid)
}
fn protected(c: &ProtectionConfig, p: &str) -> Result<bool, Fault> {
    c.protected(p).map_err(|_| Fault::Invalid)
}
fn need_version(actual: &str, expected: Option<&str>) -> Result<(), Fault> {
    let e = expected.filter(|x| !x.is_empty()).ok_or(Fault::Required)?;
    if e != actual {
        Err(Fault::Precondition)
    } else {
        Ok(())
    }
}
fn mutate(tx: &Transaction<'_>, r: &Request, _namespace: &str) -> Result<Reply, Fault> {
    mutations::apply(tx, r)
}
fn derived_uuid(operation: &str, path: &str) -> String {
    let mut h = Sha256::new();
    h.update(operation.as_bytes());
    h.update([0]);
    h.update(path.as_bytes());
    let digest = h.finalize();
    let mut b: [u8; 16] = [0; 16];
    for (i, v) in b.iter_mut().enumerate() {
        *v = digest.get(i).copied().unwrap_or(0);
    }
    b[6] = (b[6] & 0x0f) | 0x50;
    b[8] = (b[8] & 0x3f) | 0x80;
    let x = b.iter().map(|v| format!("{v:02x}")).collect::<String>();
    format!(
        "{}-{}-{}-{}-{}",
        &x[0..8],
        &x[8..12],
        &x[12..16],
        &x[16..20],
        &x[20..32]
    )
}
fn trash_list(tx: &Transaction<'_>, r: &Request) -> Result<Reply, Fault> {
    mutations::trash_list(tx, r)
}

#[cfg(test)]
#[path = "../tests/common/mod.rs"]
mod test_fixture;
#[cfg(test)]
mod receipt_atomicity {
    use super::*;
    use crate::test_fixture::{config, fixture, CAPSULE, NAMESPACE, OP_ID, TENANT};
    fn request(path: &str) -> Result<Request, Box<dyn std::error::Error>> {
        Ok(serde_json::from_value(
            serde_json::json!({"version":1,"dbPath":path,"expectedTenant":TENANT,
            "operationNamespace":NAMESPACE,"operationId":OP_ID,"capsuleId":CAPSULE,"op":"delete",
            "source":"Notes/one.md","sourceVersion":sha256(b"synthetic note"),"config":config(),"now":2.0,"debounce":0.0}),
        )?)
    }
    fn unchanged(path: &str, receipts: i64) -> Result<(), Box<dyn std::error::Error>> {
        let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let raw: Vec<u8> = db.query_row(
            "SELECT data FROM files WHERE path='Notes/one.md'",
            [],
            |r| r.get(0),
        )?;
        assert_eq!(raw, b"synthetic note");
        assert_eq!(
            db.query_row::<i64, _, _>("SELECT COUNT(*) FROM files", [], |r| r.get(0))?,
            1
        );
        assert_eq!(
            db.query_row::<i64, _, _>(
                "SELECT COUNT(*) FROM directories WHERE path='Notes'",
                [],
                |r| r.get(0)
            )?,
            1
        );
        assert_eq!(
            db.query_row::<i64, _, _>("SELECT COUNT(*) FROM workspace_index_outbox", [], |r| r
                .get(0))?,
            0
        );
        for key in ["generation", "due", "snapshot_time"] {
            assert_eq!(
                db.query_row::<String, _, _>("SELECT value FROM meta WHERE key=?", [key], |r| r
                    .get(0))?,
                "0"
            );
        }
        assert_eq!(
            db.query_row::<i64, _, _>(
                "SELECT COUNT(*) FROM workspace_operation_receipts",
                [],
                |r| r.get(0)
            )?,
            receipts
        );
        Ok(())
    }
    fn framed(reply: &Reply) -> Result<Vec<u8>, Fault> {
        let mut b = Vec::new();
        write_reply(&mut b, reply)?;
        Ok(b)
    }
    #[test]
    fn late_policy_refusal_rolls_back_operation_before_storing_exact_outcome(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let (_dir, path) = fixture()?;
        let req = request(&path)?;
        let refused = execute_with(&req, |tx, r, ns| {
            mutate(tx, r, ns)?;
            Err(Fault::Protected)
        })?;
        assert_eq!(refused.status(), 403);
        unchanged(&path, 1)?;
        let first_bytes = framed(&refused)?;
        let mut retry = request(&path)?;
        retry.config = None;
        retry.now = None;
        retry.debounce = None;
        assert_eq!(framed(&execute(&retry)?)?, first_bytes);
        unchanged(&path, 1)?;
        let mut deliberate_new = request(&path)?;
        deliberate_new.operation_id = Some("41234567-89ab-4cde-8fab-0123456789ab".into());
        assert_eq!(execute(&deliberate_new)?.status(), 200);
        let db = Connection::open(&path)?;
        assert_eq!(
            db.query_row::<String, _, _>(
                "SELECT value FROM meta WHERE key='generation'",
                [],
                |r| r.get(0)
            )?,
            "1"
        );
        assert_eq!(
            db.query_row::<i64, _, _>(
                "SELECT COUNT(*) FROM workspace_operation_receipts",
                [],
                |r| r.get(0)
            )?,
            2
        );
        Ok(())
    }
    #[test]
    fn infrastructure_fault_after_operation_is_uncached_and_rolls_back(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let (_dir, path) = fixture()?;
        let req = request(&path)?;
        assert_eq!(
            execute_with(&req, |tx, r, ns| {
                mutate(tx, r, ns)?;
                Err(Fault::Database)
            })
            .err(),
            Some(Fault::Database)
        );
        unchanged(&path, 0)?;
        assert_eq!(execute(&req)?.status(), 200);
        Ok(())
    }
    #[test]
    fn oversized_reply_rolls_back_operation_before_committing_capacity_refusal(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let (_dir, path) = fixture()?;
        let req = request(&path)?;
        let refused = execute_with(&req, |tx, r, ns| {
            mutate(tx, r, ns)?;
            Ok(reply(
                200,
                serde_json::json!({"large":"x".repeat(MAX_RESPONSE)}),
            ))
        })?;
        assert_eq!(refused.status(), 507);
        unchanged(&path, 1)?;
        assert_eq!(framed(&execute(&req)?)?, framed(&refused)?);
        unchanged(&path, 1)?;
        Ok(())
    }
}

#[cfg(test)]
mod nul_text_preflight {
    use super::*;
    use crate::test_fixture::{fixture, OP_ID};
    #[test]
    fn nul_identity_preflight_refuses_before_returning_large_text(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let (_dir, path) = fixture()?;
        let mut db = Connection::open(path)?;
        let mut selected = Vec::new();
        for (key, prefix, limit) in [
            ("tenant", "01234567-89ab-4cde-8fab-0123456789ab", 36i64),
            ("active", "1", 1),
            (
                "workspace_operation_namespace",
                "11234567-89ab-4cde-8fab-0123456789ab",
                36,
            ),
        ] {
            let value = format!("{prefix}\0{}", "x".repeat(100_000));
            db.execute("UPDATE meta SET value=? WHERE key=?", params![value, key])?;
            let lengths: (i64, i64) = db.query_row(
                "SELECT length(value),length(CAST(value AS BLOB)) FROM meta WHERE key=?",
                [key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            assert!(lengths.0 <= limit);
            assert_eq!(lengths.1, i64::try_from(value.len())?);
            let tx = db.transaction()?;
            if !matches!(meta(&tx, key), Err(Fault::Schema)) {
                selected.push(key);
            }
            tx.rollback()?;
            assert_eq!(
                db.query_row::<i64, _, _>(
                    "SELECT length(CAST(value AS BLOB)) FROM meta WHERE key=?",
                    [key],
                    |r| r.get(0)
                )?,
                lengths.1
            );
            db.execute("UPDATE meta SET value=? WHERE key=?", params![prefix, key])?;
        }
        assert!(
            selected.is_empty(),
            "identity preflight selected oversized fields: {selected:?}"
        );
        Ok(())
    }
    #[test]
    fn nul_receipt_preflight_does_not_return_oversized_text(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let (_dir, path) = fixture()?;
        let mut db = Connection::open(path)?;
        let hash = "0".repeat(64);
        db.execute(
            "INSERT INTO workspace_operation_receipts VALUES(?,?,?,x'00',1.0)",
            params![OP_ID, hash, "delete"],
        )?;
        let mut selected = Vec::new();
        for (field, prefix, limit) in [("fingerprint", hash.as_str(), 64i64), ("op", "delete", 16)]
        {
            let value = format!("{prefix}\0{}", "x".repeat(100_000));
            db.execute(
                &format!("UPDATE workspace_operation_receipts SET {field}=?"),
                [&value],
            )?;
            let lengths:(i64,i64)=db.query_row(&format!("SELECT length({field}),length(CAST({field} AS BLOB)) FROM workspace_operation_receipts"),[],|r|Ok((r.get(0)?,r.get(1)?)))?;
            assert!(lengths.0 <= limit);
            assert_eq!(lengths.1, i64::try_from(value.len())?);
            let tx = db.transaction()?;
            let result = receipt_metadata(&tx, OP_ID)?.ok_or("missing receipt metadata")?;
            if (field == "fingerprint" && result.fingerprint.is_some())
                || (field == "op" && result.op.is_some())
            {
                selected.push(field);
            }
            tx.rollback()?;
            assert_eq!(
                db.query_row::<i64, _, _>(
                    &format!(
                        "SELECT length(CAST({field} AS BLOB)) FROM workspace_operation_receipts"
                    ),
                    [],
                    |r| r.get(0)
                )?,
                lengths.1
            );
            db.execute(
                &format!("UPDATE workspace_operation_receipts SET {field}=?"),
                [prefix],
            )?;
        }
        assert!(
            selected.is_empty(),
            "receipt preflight selected oversized fields: {selected:?}"
        );
        Ok(())
    }
}

#[cfg(test)]
mod fresh_receipt_contract {
    use super::*;
    use crate::test_fixture::{config, fixture, CAPSULE, NAMESPACE, OP_ID, TENANT};
    #[test]
    fn invalid_internal_success_rolls_back_real_operation_and_stores_no_receipt(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let (_dir, path) = fixture()?;
        let r: Request = serde_json::from_value(
            serde_json::json!({"version":1,"dbPath":path,"expectedTenant":TENANT,"operationNamespace":NAMESPACE,"operationId":OP_ID,"capsuleId":CAPSULE,"op":"delete","source":"Notes/one.md","sourceVersion":sha256(b"synthetic note"),"config":config(),"now":2.0,"debounce":0.0}),
        )?;
        let result = execute_with(&r, |tx, r, ns| {
            mutate(tx, r, ns)?;
            Ok(reply(200, serde_json::json!({"synthetic":true})))
        });
        let db = Connection::open(&path)?;
        let generation: String =
            db.query_row("SELECT value FROM meta WHERE key='generation'", [], |row| {
                row.get(0)
            })?;
        let receipts: i64 = db.query_row(
            "SELECT count(*) FROM workspace_operation_receipts",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(
            result.err(),
            Some(Fault::Internal),
            "generation={generation}, receipts={receipts}"
        );
        assert_eq!(generation, "0");
        assert_eq!(receipts, 0);
        assert_eq!(
            db.query_row::<Vec<u8>, _, _>(
                "SELECT data FROM files WHERE path='Notes/one.md'",
                [],
                |row| row.get(0)
            )?,
            b"synthetic note"
        );
        Ok(())
    }
}

#[cfg(test)]
mod admission_refusal_contract {
    use super::*;
    use crate::test_fixture::{config, fixture, CAPSULE, NAMESPACE, OP_ID, TENANT};
    #[test]
    fn admission_impossible_fresh_refusal_rolls_back_real_mutation(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let (_dir, path) = fixture()?;
        let r: Request = serde_json::from_value(
            serde_json::json!({"version":1,"dbPath":path,"expectedTenant":TENANT,"operationNamespace":NAMESPACE,"operationId":OP_ID,"capsuleId":CAPSULE,"op":"trash","source":"Notes/one.md","sourceVersion":sha256(b"synthetic note"),"config":config(),"now":2.0,"debounce":0.0}),
        )?;
        let result = execute_with(&r, |tx, r, ns| {
            mutate(tx, r, ns)?;
            Ok(error(Fault::Missing))
        });
        let db = Connection::open(&path)?;
        let generation: String =
            db.query_row("SELECT value FROM meta WHERE key='generation'", [], |r| {
                r.get(0)
            })?;
        let receipts: i64 = db.query_row(
            "SELECT count(*) FROM workspace_operation_receipts",
            [],
            |r| r.get(0),
        )?;
        assert_eq!(
            result.err(),
            Some(Fault::Internal),
            "generation={generation}, receipts={receipts}"
        );
        assert_eq!(generation, "0");
        assert_eq!(receipts, 0);
        assert_eq!(
            db.query_row::<Vec<u8>, _, _>(
                "SELECT data FROM files WHERE path='Notes/one.md'",
                [],
                |r| r.get(0)
            )?,
            b"synthetic note"
        );
        Ok(())
    }
}
