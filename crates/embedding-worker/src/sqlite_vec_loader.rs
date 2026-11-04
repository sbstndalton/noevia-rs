//! The sole reviewed Rust FFI exception. Statically linked sqlite-vec only;
//! no path-based extension loading, reset, unregister or runtime selection.
use rusqlite::ffi;
use std::sync::OnceLock;
static REGISTERED: OnceLock<Result<(), String>> = OnceLock::new();
fn checked(rc: i32) -> Result<(), String> {
    if rc == ffi::SQLITE_OK {
        Ok(())
    } else {
        Err("sqlite-vec registration failed".into())
    }
}
pub fn register() -> Result<(), String> {
    REGISTERED
        .get_or_init(|| {
            // SAFETY: sqlite-vec's pinned sqlite3_vec_init is the SQLite extension
            // entry point (sqlite3*, char**, sqlite3_api_routines*) -> int. Its crate
            // exposes an erased C function, so restore that ABI once. The statically
            // linked code has process lifetime; rusqlite and sqlite-vec share the same
            // SQLite ABI. No untrusted symbol or library path participates.
            let init = unsafe {
                std::mem::transmute::<
                    unsafe extern "C" fn(),
                    unsafe extern "C" fn(
                        *mut ffi::sqlite3,
                        *mut *mut std::ffi::c_char,
                        *const ffi::sqlite3_api_routines,
                    ) -> std::ffi::c_int,
                >(sqlite_vec::sqlite3_vec_init)
            };
            // SAFETY: the function has SQLite's auto-extension signature and process
            // lifetime above. SQLite serializes registration; OnceLock additionally
            // ensures this module registers it once, before opening vec connections.
            checked(unsafe { ffi::sqlite3_auto_extension(Some(init)) })
        })
        .clone()
}
#[cfg(test)]
mod tests {
    #[test]
    fn registration_failure_is_sticky_error_value() {
        assert!(super::checked(rusqlite::ffi::SQLITE_ERROR).is_err());
    }
}
