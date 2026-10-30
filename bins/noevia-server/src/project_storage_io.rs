//! The project storage writes on the owned child transport (sbstndalton/noevia#2191 round 3):
//! server-projects' [`ProjectStorage`] over server-storage-reads' [`writes`].
//!
//! The connection is the signed-in user's own row (owner_secret, as the storage browse reads it);
//! the endpoint policy is the browse's (`endpointApproved`: an admin any http(s) origin, a member
//! only MEMBER_OUTBOUND_ORIGINS), checked before any outbound work; every attempt runs in a fresh
//! owned child (no redirects, proxies or cookies; deadlines per method). Writes have their own
//! admission (four at once, one per account) so they never take a browse slot. Errors reach the
//! log as the storage client words them (status, path), never with a credential or endpoint.
use crate::App;
use server_projects::project_storage::{ProjectStorage, Remote};
use server_storage_reads::{
    admission::Permit,
    owner_secret,
    protocol::{self, Connection, Kind},
    transport::{ChildTransport, CHILD_ARG},
    writes::{self, Condition},
};
use std::{
    path::PathBuf,
    sync::{atomic::AtomicBool, Arc},
    time::{Duration, Instant},
};

/// How long a write waits for an admission slot.
const SLOT_WAIT: Duration = Duration::from_secs(10);

pub struct RemoteStorage {
    projected: owner_secret::ProjectedConnection,
    program: PathBuf,
    cancelled: AtomicBool,
    _permit: Permit,
}

/// What the user's storage is for a project write.
pub enum Opened {
    /// No browsable storage, or no connection could be read (core's getStorage throwing): the
    /// storage steps do nothing.
    None,
    S3,
    Dav(Box<RemoteStorage>),
    /// Storage is there but cannot be used now (no admission slot, no child program, endpoint
    /// policy): a create keeps its files in noevia; a sweep job stays queued.
    Unavailable,
}

impl Opened {
    pub fn remote(&self) -> Remote<'_> {
        match self {
            Opened::Dav(r) => Remote::Dav(r.as_ref()),
            Opened::S3 => Remote::S3,
            Opened::None | Opened::Unavailable => Remote::None,
        }
    }
}

/// The signed-in user's storage, opened for writes. Blocking (runs on the blocking pool).
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
    match connection.kind {
        Kind::Local => return Opened::None,
        Kind::S3 => return Opened::S3,
        Kind::WebDav | Kind::Nextcloud => {}
    }
    let admin = projected.effective_role() == server_auth::Role::Admin;
    let members = std::env::var("MEMBER_OUTBOUND_ORIGINS").unwrap_or_default();
    if !protocol::endpoint_approved(&connection.base_url, admin, &members) {
        eprintln!("noevia-server: project storage write skipped: the storage endpoint is not approved for this account");
        return Opened::Unavailable;
    }
    let Some(program) = app.storage_child.clone() else {
        return Opened::Unavailable;
    };
    let end = Instant::now() + SLOT_WAIT;
    let permit = loop {
        if let Ok(p) = app.storage_writes.try_acquire(authn.user.id.as_str()) {
            break p;
        }
        if Instant::now() >= end {
            eprintln!("noevia-server: project storage write skipped: no storage slot");
            return Opened::Unavailable;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    Opened::Dav(Box::new(RemoteStorage {
        projected,
        program,
        cancelled: AtomicBool::new(false),
        _permit: permit,
    }))
}

impl RemoteStorage {
    fn connection(&self) -> &Connection {
        self.projected.connection()
    }
    fn transport(&self) -> ChildTransport<'_> {
        ChildTransport::new(self.program.clone(), &[CHILD_ARG], &self.cancelled)
    }
}

impl ProjectStorage for RemoteStorage {
    fn create_folder(&self, path: &str) -> Result<bool, String> {
        writes::create_folder(self.connection(), path, &mut self.transport())
            .map(|f| f.existed)
            .map_err(|e| e.message)
    }
    fn write_new(&self, path: &str, bytes: &[u8]) -> Result<(), String> {
        let mut t = self.transport();
        let written = writes::write_file(
            self.connection(),
            path,
            bytes,
            &Condition::IfNoneMatch,
            &mut t,
        )
        .map_err(|e| e.message)?;
        // uploads.cjs ingest: nothing after the write can fail for a text file, so the #1664
        // cleanup (discardWritten) is never needed here; it stays available in `writes`.
        let _ = written;
        Ok(())
    }
    fn remove_empty_folder(&self, path: &str) -> Result<(bool, &'static str), String> {
        writes::remove_empty_folder(self.connection(), path, &mut self.transport())
            .map_err(|e| e.message)
    }
}
