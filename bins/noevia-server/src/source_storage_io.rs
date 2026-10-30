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
//! Each operation waits up to [`SLOT_WAIT`] for one of the four slots (one per account). A streamed read of
//! a large file takes the slot for each ranged request (one chunk, a few seconds), not for the whole read, so
//! the account's other storage operations are not refused for its length ([`Gated`]).
use crate::App;
use server_projects::source_storage::{
    BinaryRead, Condition, Entry, SourceStorage, Storage, StorageError, TextRead, Written,
};
use server_storage_reads::{
    admission::Permit,
    owner_secret,
    protocol::{
        self, Chunk, Connection, Kind, Operation, Request, Response, Transport, TransportFailure,
    },
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
/// The saved storage login cannot be read (the credential store or its key). Not "no storage": a sync or an
/// upload that quietly went on as if the account had none would keep the person's files only in noevia, or
/// drop a folder's sources, without saying why.
const UNREADABLE: &str = "The saved storage connection could not be read; try again shortly.";
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
        eprintln!("noevia-server: a source operation could not open the account store");
        return Opened::Refused(503, UNREADABLE);
    };
    // No connection row is `Ok` (local storage); an error is a row that could not be read or opened.
    let projected = match owner_secret::project_connection_with(&store, authn, keys.as_ref()) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("noevia-server: a source operation could not read the saved storage connection: {e}");
            return Opened::Refused(503, UNREADABLE);
        }
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

/// A transport that takes an admission slot (`acquire`) for each chunk request only, not for a whole
/// streamed read: a read of 96 MiB is a dozen requests over minutes, and a slot held across them would
/// refuse every other storage write of the account (a project create, a file delete) for that long. A
/// chunk that finds no slot ends the read with the busy answer (`refused`).
struct Gated<T, G, A: Fn() -> Result<G, StorageError>> {
    acquire: A,
    inner: T,
    refused: Option<StorageError>,
}

impl<T: Transport, G, A: Fn() -> Result<G, StorageError>> Transport for Gated<T, G, A> {
    fn elapsed(&self) -> Duration {
        self.inner.elapsed()
    }
    fn amz_date(&self) -> String {
        self.inner.amz_date()
    }
    fn attempt(&mut self, request: Request) -> Result<Response, TransportFailure> {
        self.inner.attempt(request)
    }
    fn attempt_chunk(&mut self, request: Request) -> Result<Chunk, TransportFailure> {
        let _slot = match (self.acquire)() {
            Ok(slot) => slot,
            Err(e) => {
                self.refused = Some(e);
                return Err(TransportFailure::Other);
            }
        };
        self.inner.attempt_chunk(request)
    }
    fn delay(&mut self, delay: Duration) -> Result<(), TransportFailure> {
        self.inner.delay(delay)
    }
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
    fn nextcloud(&self) -> bool {
        self.connection().kind == Kind::Nextcloud
    }

    fn account(&self) -> String {
        let c = self.connection();
        server_projects::research_report::storage_account(
            kind_name(c.kind),
            &c.base_url,
            &c.bucket,
            &c.region,
            &c.username,
        )
    }

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
        let mut t = Gated {
            acquire: || self.permit(),
            inner: self.transport(),
            refused: None,
        };
        let BinaryRead {
            cap,
            timeout,
            before_body,
        } = read;
        let mut hook = |d: Option<u64>| before_body(d).map_err(|e| back(&e));
        let mut take = |b: &[u8]| sink(b).map_err(|e| back(&e));
        let result = stream::read_binary(
            self.connection(),
            path,
            ReadOptions {
                cap: cap.min(server_storage_reads::FILE_CAP),
                timeout,
                before_body: &mut hook,
            },
            &mut take,
            &mut t,
        );
        match (result, t.refused.take()) {
            // No slot for a chunk: the busy answer, not the transport failure it caused.
            (Err(_), Some(busy)) => Err(busy),
            (result, _) => result.map_err(convert),
        }
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
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
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

    /// A streamed read takes the account's admission slot for each chunk request, not across the whole
    /// read: between two chunks the slot is free for the account's other storage operations.
    #[test]
    fn a_streamed_read_holds_its_slot_per_chunk_not_across_the_read() {
        use std::cell::Cell;
        use std::rc::Rc;
        use zeroize::Zeroizing;
        struct Slot(Rc<Cell<usize>>);
        impl Drop for Slot {
            fn drop(&mut self) {
                self.0.set(self.0.get() - 1);
            }
        }
        struct File {
            held: Rc<Cell<usize>>,
            seen_held: Vec<usize>,
            now: Duration,
        }
        impl Transport for File {
            fn elapsed(&self) -> Duration {
                self.now
            }
            fn amz_date(&self) -> String {
                "20260101T000000Z".into()
            }
            fn attempt(&mut self, _: Request) -> Result<Response, TransportFailure> {
                Err(TransportFailure::Other)
            }
            fn delay(&mut self, _: Duration) -> Result<(), TransportFailure> {
                Ok(())
            }
            fn attempt_chunk(&mut self, r: Request) -> Result<Chunk, TransportFailure> {
                self.seen_held.push(self.held.get());
                let h = r
                    .headers
                    .iter()
                    .find(|(k, _)| k == "Range")
                    .unwrap()
                    .1
                    .as_str()
                    .to_owned();
                let (a, b) = h.strip_prefix("bytes=").unwrap().split_once('-').unwrap();
                let (a, b): (u64, u64) = (a.parse().unwrap(), b.parse().unwrap());
                let total = 20 * 1024 * 1024u64;
                let end = b.min(total - 1);
                Ok(Chunk {
                    status: 206,
                    total: Some(total),
                    first: Some(a),
                    last: Some(end),
                    bytes: vec![0u8; (end - a + 1) as usize],
                    capped: false,
                    etag: None,
                })
            }
        }
        let held = Rc::new(Cell::new(0usize));
        let (acquire_held, sink_held) = (Rc::clone(&held), Rc::clone(&held));
        let mut t = Gated {
            acquire: move || {
                acquire_held.set(acquire_held.get() + 1);
                Ok(Slot(Rc::clone(&acquire_held)))
            },
            inner: File {
                held: Rc::clone(&held),
                seen_held: Vec::new(),
                now: Duration::ZERO,
            },
            refused: None,
        };
        let c = Connection {
            kind: Kind::WebDav,
            base_url: "http://storage.example/dav".into(),
            username: "u".into(),
            secret: Zeroizing::new(b"pw".to_vec()),
            corpus_root: String::new(),
            bucket: String::new(),
            region: String::new(),
        };
        let mut between = Vec::new();
        let mut hook = |_: Option<u64>| Ok(());
        let mut sink = |_: &[u8]| {
            between.push(sink_held.get());
            Ok(())
        };
        let n = stream::read_binary(
            &c,
            "big.pdf",
            ReadOptions {
                cap: server_storage_reads::FILE_CAP,
                timeout: Duration::from_secs(60),
                before_body: &mut hook,
            },
            &mut sink,
            &mut t,
        );
        assert_eq!(n.ok(), Some(20 * 1024 * 1024));
        assert_eq!(
            t.inner.seen_held,
            vec![1, 1, 1],
            "one slot while a chunk is fetched"
        );
        assert_eq!(between, vec![0, 0, 0], "none between chunks");
        assert_eq!(held.get(), 0);
        // A chunk that finds no slot is the busy answer.
        let mut t = Gated {
            acquire: || -> Result<(), StorageError> { Err(StorageError::new(503, BUSY)) },
            inner: File {
                held: Rc::new(Cell::new(0)),
                seen_held: Vec::new(),
                now: Duration::ZERO,
            },
            refused: None,
        };
        let mut hook = |_: Option<u64>| Ok(());
        let r = stream::read_binary(
            &c,
            "big.pdf",
            ReadOptions {
                cap: server_storage_reads::FILE_CAP,
                timeout: Duration::from_secs(60),
                before_body: &mut hook,
            },
            &mut |_: &[u8]| Ok(()),
            &mut t,
        );
        assert!(r.is_err());
        assert_eq!(t.refused.map(|e| e.status), Some(503));
    }
}
