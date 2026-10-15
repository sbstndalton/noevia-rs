//! Linux-required owned main-thread DNS/HTTP/TLS child (the same body as
//! `noevia-server --storage-request-child`).
fn main() -> std::process::ExitCode {
    server_storage_reads::transport::child_main()
}
