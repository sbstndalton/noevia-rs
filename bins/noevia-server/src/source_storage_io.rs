//! The user's storage for a source sync and an organized upload (sbstndalton/noevia#2191 round 8):
//! server-projects' [`SourceStorage`] over server-storage-reads' list and text read ([`protocol`]), its
//! streamed binary read ([`stream`]) and its writes ([`writes`]).
//!
//! Like the round-3 writes ([`crate::project_storage_io`]) the connection is the signed-in user's own row,
//! the endpoint policy is the browse's (an admin any http(s) origin, a member only MEMBER_OUTBOUND_ORIGINS)
//! and every attempt runs in a fresh owned child (no redirects, proxies or cookies; the child's address
//! space, wire and write caps unchanged). S3 is read here (listing, text, ranged GETs) and refused where
//! core refuses it (every write).
//!
//! Admission is per operation, not per session: a sync that runs for an hour holds no slot between its
//! requests, so a project create or a file delete of the same account is not refused for its whole length.
//! Each operation waits up to [`SLOT_WAIT`] for one of the four slots (one per account).
use crate::App;
use server_projects::source_storage::{
    BinaryRead, Condition, Entry, SourceStorage, Storage, StorageError, TextRead, Written,
};
use server_storage_reads::{
    admission::Permit,
    owner_secret,
    protocol::{self, Connection, Kind, Operation},
    stream::{self, ReadOptions},
    transport::{ChildTransport, CHILD_ARG},
    writes::{self, Condition as WriteCondition},
    Failure,
};
use std::{
    path::PathBuf,
    sync::{atomic::AtomicBool, Arc},
    time::{Duration, Instant},
};

const BUSY: &str = "Storage is busy with other requests; try again shortly.";
/// How long one operation waits for an admission slot.
const SLOT_WAIT: Duration = Duration::from_secs(30);

pub struct Session {
    projected: owner_secret::ProjectedConnection,
    program: PathBuf,
    app: Arc<App>,
    user: String,
    cancelled: AtomicBool,
}

/// What the account's storage is for a source operation.
pub enum Opened {
    None,
    Refused(u16, &'static str),
    Remote(Box<Session>),
}

impl Opened {
    pub fn storage(&self) -> Storage<'_> {
        match self {
            Opened::None => Storage::None,
            Opened::Refused(status, message) => Storage::Refused(*status, message),
            Opened::Remote(s) => Storage::Remote(s.as_ref()),
        }
    }
}

/// The signed-in user's storage, opened for reads and writes. Blocking (the blocking pool).
pub fn open(app: &Arc<App>, authn: &server_auth::Identity) -> Opened {
    let Some(data_dir) = app.config.data_dir.as_ref() else {
        return Opened::None;
    };
    let previous = std::env::var("SECRETS_KEY_PREVIOUS")
        .ok()
        .map(zeroize::Zeroizing::new);
    let keys =
        owner_secret::load_existing_keys(data_dir, previous.as_deref().map(String::as_str)).ok();
    let Ok(store) = app.identity.store_blocking() else {
        return Opened::None;
    };
    let Ok(projected) = owner_secret::project_connection_with(&store, authn, keys.as_ref()) else {
        return Opened::None;
    };
    drop(store);
    let connection = projected.connection();
    if connection.kind == Kind::Local {
        return Opened::None;
    }
    let admin = projected.effective_role() == server_auth::Role::Admin;
    let members = std::env::var("MEMBER_OUTBOUND_ORIGINS").unwrap_or_default();
    if !protocol::endpoint_approved(&connection.base_url, admin, &members) {
        eprintln!("noevia-server: a source operation skipped storage: the endpoint is not approved for this account");
        return Opened::Refused(400, protocol::ENDPOINT_REFUSAL);
    }
    let Some(program) = app.storage_child.clone() else {
        return Opened::Refused(503, BUSY);
    };
    Opened::Remote(Box::new(Session {
        projected,
        program,
        app: Arc::clone(app),
        user: authn.user.id.as_str().to_owned(),
        cancelled: AtomicBool::new(false),
    }))
}

fn convert(e: writes::StorageError) -> StorageError {
    let mut out = StorageError::new(e.status, e.message);
    if let Some(code) = e.code {
        out = out.with_code(code);
    }
    if let Some(up) = e.upstream {
        out = out.with_upstream(up);
    }
    out
}

fn back(e: &StorageError) -> writes::StorageError {
    let mut out = writes::StorageError {
        status: e.status,
        message: e.message.clone(),
        code: None,
        upstream: e.upstream,
    };
    // The write error's code is a static string; the two a hook can carry are mapped.
    out.code = match e.code.as_deref() {
        Some("timeout") => Some("timeout"),
        Some("unreachable") => Some("unreachable"),
        Some("changed") => Some("changed"),
        Some("unknown") => Some("unknown"),
        Some("large-document-busy") => Some("large-document-busy"),
        _ => None,
    };
    out
}

/// A read engine failure as a storage error: the words core's helpers give (storage-client.cjs
/// `transportError`, `upstreamError`).
fn failure(f: Failure) -> StorageError {
    let mut e = StorageError::new(f.status, f.message.clone());
    match f.message.as_str() {
        "the storage server did not answer in time" => e = e.with_code("timeout"),
        "the storage server could not be reached" => e = e.with_code("unreachable"),
        m => {
            if let Some(n) = m
                .strip_prefix("storage returned ")
                .and_then(|s| s.parse::<u16>().ok())
            {
                e = e.with_upstream(n);
            }
        }
    }
    e
}

impl Session {
    fn connection(&self) -> &Connection {
        self.projected.connection()
    }
    fn permit(&self) -> Result<Permit, StorageError> {
        let end = Instant::now() + SLOT_WAIT;
        loop {
            if let Ok(p) = self.app.storage_writes.try_acquire(self.user.as_str()) {
                return Ok(p);
            }
            if Instant::now() >= end {
                eprintln!("noevia-server: a source operation found no storage slot");
                return Err(StorageError::new(503, BUSY));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    fn transport(&self) -> ChildTransport<'_> {
        ChildTransport::new(self.program.clone(), &[CHILD_ARG], &self.cancelled)
    }
}

fn kind_name(k: Kind) -> &'static str {
    match k {
        Kind::WebDav => "webdav",
        Kind::Nextcloud => "nextcloud",
        Kind::S3 => "s3",
        Kind::Local => "local",
    }
}

impl SourceStorage for Session {
    fn s3(&self) -> bool {
        self.connection().kind == Kind::S3
    }

    fn fingerprint(&self) -> String {
        let c = self.connection();
        server_projects::storage_latch::fingerprint(
            kind_name(c.kind),
            &c.base_url,
            &c.bucket,
            &c.username,
            &String::from_utf8_lossy(&c.secret),
        )
    }

    fn list(&self, dir: &str) -> Result<Vec<Entry>, StorageError> {
        let _slot = self.permit()?;
        let units: Vec<u16> = dir.encode_utf16().collect();
        let mut t = self.transport();
        let text = protocol::execute(self.connection(), &units, Operation::List, &mut t)
            .map_err(failure)?;
        let rows: Vec<serde_json::Value> = serde_json::from_str(&text)
            .map_err(|_| StorageError::new(502, "storage listing could not be read"))?;
        Ok(rows
            .into_iter()
            .map(|r| {
                let text = |k: &str| r.get(k).and_then(|v| v.as_str()).unwrap_or("").to_owned();
                Entry {
                    name: text("name"),
                    path: text("path"),
                    is_dir: r.get("isDir").and_then(|v| v.as_bool()).unwrap_or(false),
                    size: r.get("size").and_then(|v| v.as_f64()),
                    ext: text("ext"),
                }
            })
            .collect())
    }

    fn read_text(&self, path: &str) -> Result<TextRead, StorageError> {
        let _slot = self.permit()?;
        let units: Vec<u16> = path.encode_utf16().collect();
        let mut t = self.transport();
        let text = protocol::execute(self.connection(), &units, Operation::Read, &mut t)
            .map_err(failure)?;
        let v: serde_json::Value = serde_json::from_str(&text)
            .map_err(|_| StorageError::new(502, "storage file could not be read"))?;
        Ok(TextRead {
            content: v
                .get("content")
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_owned(),
            truncated: v
                .get("truncated")
                .and_then(|t| t.as_bool())
                .unwrap_or(false),
        })
    }

    fn read_binary(
        &self,
        path: &str,
        read: BinaryRead<'_>,
        sink: &mut dyn FnMut(&[u8]) -> Result<(), StorageError>,
    ) -> Result<u64, StorageError> {
        let _slot = self.permit()?;
        let mut t = self.transport();
        let BinaryRead {
            cap,
            timeout,
            before_body,
        } = read;
        let mut hook = |d: Option<u64>| before_body(d).map_err(|e| back(&e));
        let mut take = |b: &[u8]| sink(b).map_err(|e| back(&e));
        stream::read_binary(
            self.connection(),
            path,
            ReadOptions {
                cap: cap.min(server_storage_reads::FILE_CAP),
                timeout,
                before_body: &mut hook,
            },
            &mut take,
            &mut t,
        )
        .map_err(convert)
    }

    fn create_folder(&self, path: &str) -> Result<bool, StorageError> {
        let _slot = self.permit()?;
        writes::create_folder(self.connection(), path, &mut self.transport())
            .map(|f| f.existed)
            .map_err(convert)
    }

    fn write(
        &self,
        path: &str,
        bytes: &[u8],
        condition: &Condition,
    ) -> Result<Written, StorageError> {
        let _slot = self.permit()?;
        let condition = match condition {
            Condition::None => WriteCondition::None,
            Condition::IfMatch(v) => WriteCondition::IfMatch(v.clone()),
            Condition::IfNoneMatch => WriteCondition::IfNoneMatch,
        };
        writes::write_file(
            self.connection(),
            path,
            bytes,
            &condition,
            &mut self.transport(),
        )
        .map(|w| Written {
            etag: w.etag,
            created: w.created,
        })
        .map_err(convert)
    }

    fn discard(&self, path: &str, etag: &str) -> (bool, &'static str) {
        let Ok(_slot) = self.permit() else {
            return (false, "error");
        };
        writes::discard_written(self.connection(), path, etag, &mut self.transport())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_storage_cap_is_the_front_s() {
        assert_eq!(
            server_projects::SOURCE_FILE_CAP,
            server_storage_reads::FILE_CAP
        );
    }

    #[test]
    fn read_failures_carry_the_codes_core_gives_them() {
        let timeout = failure(Failure::new(
            504,
            "the storage server did not answer in time",
        ));
        assert_eq!(timeout.failure().map(|f| f.kind), Some("timeout"));
        let down = failure(Failure::new(502, "the storage server could not be reached"));
        assert_eq!(down.failure().map(|f| f.kind), Some("unreachable"));
        let login = failure(Failure::new(502, "storage returned 401"));
        assert_eq!(login.failure().map(|f| f.kind), Some("login"));
        assert_eq!(login.upstream, Some(401));
        assert!(failure(Failure::new(502, "storage returned 404"))
            .failure()
            .is_none());
    }
}
