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
