//! Complete verifier entry point. Non-Linux and absent isolation always refuse.
#![forbid(unsafe_code)]
#[cfg(any(target_os = "linux", test))]
mod budget;
#[cfg(any(target_os = "linux", test))]
mod engine;
#[cfg(any(target_os = "linux", test))]
mod lifecycle;
#[cfg(target_os = "linux")]
mod runtime;
#[cfg(any(target_os = "linux", test))]
mod session;
fn main() -> std::process::ExitCode {
    if std::env::args_os().skip(1).collect::<Vec<_>>()
        == [std::ffi::OsString::from("--capabilities")]
    {
        println!(
            "{}",
            serde_json::json!({"probe":"source-capability","capabilities":["noevia-code-verify/1"],"nativeVerifier":true,"oneShot":true,"readiness":"requires-isolated-namespace-init","requestMaxBytes":code_verify_native::MAX_REQUEST_BYTES})
        );
        return std::process::ExitCode::SUCCESS;
    }
    #[cfg(target_os = "linux")]
    {
        if runtime::entry().is_ok() {
            return std::process::ExitCode::SUCCESS;
        }
    }
    // Never echo caller input, paths, commands or a privileged helper plan.
    eprintln!("code-verify-native: isolated verifier unavailable");
    std::process::ExitCode::FAILURE
}
