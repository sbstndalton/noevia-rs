//! Source-only mutation gate for a future Diary storage adapter.
//!
//! The transport type is deliberately unconstrained here. These methods cannot
//! send a request, and no DAV Backend implementation or listener uses them.

use server_dav::Fault;

/// Holds a future transport without granting mutation access in stage 1.
pub struct StorageOps<T> {
    _transport: T,
}

impl<T> StorageOps<T> {
    pub fn new(transport: T) -> Self {
        Self {
            _transport: transport,
        }
    }

    /// PUT remains unavailable until a guarded, revocation-fenced write exists.
    pub fn put(&self) -> Result<(), Fault> {
        Err(Fault::Unavailable)
    }

    /// MKCOL remains unavailable until a guarded, revocation-fenced write exists.
    pub fn mkcol(&self) -> Result<(), Fault> {
        Err(Fault::Unavailable)
    }

    /// DELETE remains unavailable until guarded Diary operations are qualified.
    pub fn delete(&self) -> Result<(), Fault> {
        Err(Fault::Unavailable)
    }

    /// MOVE remains unavailable until guarded Diary operations are qualified.
    pub fn move_resource(&self) -> Result<(), Fault> {
        Err(Fault::Unavailable)
    }

    /// COPY remains unavailable until guarded Diary operations are qualified.
    pub fn copy(&self) -> Result<(), Fault> {
        Err(Fault::Unavailable)
    }

    /// Trash preservation remains unavailable until it can be fenced with PUT.
    pub fn preserve(&self) -> Result<(), Fault> {
        Err(Fault::Unavailable)
    }
}
