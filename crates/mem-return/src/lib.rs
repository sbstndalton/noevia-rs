//! #1833: the Rust front builds tens of MiB per export in a blocking-pool thread's arena and frees
//! it, but glibc keeps much of the freed heap, so a long-running front sits above one export's peak.
//! [`trim`] asks glibc to return it. A no-op on every other target.

/// Returns freed heap to the OS (glibc `malloc_trim(0)`, all arenas). Safe from any thread; cheap
/// enough to call after each single-flight export.
pub fn trim() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    #[allow(unsafe_code)]
    // SAFETY: malloc_trim takes no pointers and may be called from any thread at any time.
    unsafe {
        libc::malloc_trim(0);
    }
}
