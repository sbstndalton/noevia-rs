//! `noevia-web-supervisor --image-entrypoint`: one image command for both supervisors, so a release
//! can move the web image's CMD once and the owner picks the supervisor by switch (inert: no image
//! runs this yet; the image's CMD is still `bash /app/web-supervisor.sh`).
//!
//! Switch `NOEVIA_SUPERVISOR_IMPL`:
//! - unset, empty or `sh`: exec `bash <script>` in this process (same PID, same environment,
//!   same arguments as the image's `CMD ["bash", "/app/web-supervisor.sh"]`), so nothing changes;
//! - `rust`: the native supervisor (`crate::run`), exactly as the binary without arguments;
//! - anything else: refused, exit 2, nothing started (no fallback either way).
//!
//! `NOEVIA_SUPERVISOR_SCRIPT` names the script (default `/app/web-supervisor.sh`; tests point it
//! at a stub).

use std::ffi::CString;

pub const SWITCH: &str = "NOEVIA_SUPERVISOR_IMPL";
pub const SCRIPT_VAR: &str = "NOEVIA_SUPERVISOR_SCRIPT";
pub const DEFAULT_SCRIPT: &str = "/app/web-supervisor.sh";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Choice {
    /// exec `bash <path>`.
    Script(String),
    /// The native supervisor.
    Native,
    /// Refuse with this message, exit 2.
    Refuse(String),
}

/// Picks the supervisor from the two variables (`None` = unset).
pub fn choose(implementation: Option<&str>, script: Option<&str>) -> Choice {
    match implementation.unwrap_or_default() {
        "" | "sh" => Choice::Script(match script {
            Some(path) if !path.is_empty() => path.to_string(),
            _ => DEFAULT_SCRIPT.to_string(),
        }),
        "rust" => Choice::Native,
        other => Choice::Refuse(format!(
            "web-supervisor: {SWITCH} must be unset, sh or rust (got {other:?}); nothing started"
        )),
    }
}

fn var(name: &str) -> Result<Option<String>, String> {
    match std::env::var_os(name) {
        None => Ok(None),
        Some(value) => value
            .into_string()
            .map(Some)
            .map_err(|_| format!("web-supervisor: {name} is not valid UTF-8; nothing started")),
    }
}

/// Replaces this process with `bash <path>`; returns only when the exec failed (127 when bash is
/// missing, 126 otherwise, as a shell reports a command it cannot run).
fn exec_script(path: &str) -> i32 {
    let (Ok(bash), Ok(script)) = (CString::new("bash"), CString::new(path)) else {
        eprintln!("web-supervisor: {SCRIPT_VAR} contains a NUL byte; nothing started");
        return 2;
    };
    match nix::unistd::execvp(&bash, &[bash.clone(), script]) {
        Ok(never) => match never {},
        Err(e) => {
            eprintln!("web-supervisor: cannot exec bash {path}: {e}");
            if e == nix::errno::Errno::ENOENT {
                127
            } else {
                126
            }
        }
    }
}

/// `--image-entrypoint`.
pub fn run() -> i32 {
    let implementation = match var(SWITCH) {
        Ok(v) => v,
        Err(msg) => {
            eprintln!("{msg}");
            return 2;
        }
    };
    let script = match var(SCRIPT_VAR) {
        Ok(v) => v,
        Err(msg) => {
            eprintln!("{msg}");
            return 2;
        }
    };
    match choose(implementation.as_deref(), script.as_deref()) {
        Choice::Script(path) => exec_script(&path),
        Choice::Native => crate::run(),
        Choice::Refuse(msg) => {
            eprintln!("{msg}");
            2
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_script_is_the_default() {
        let script = Choice::Script(DEFAULT_SCRIPT.to_string());
        assert_eq!(choose(None, None), script);
        assert_eq!(choose(Some(""), None), script);
        assert_eq!(choose(Some("sh"), None), script);
        assert_eq!(choose(Some("sh"), Some("")), script);
        assert_eq!(choose(None, Some("/x/y.sh")), Choice::Script("/x/y.sh".to_string()));
    }

    #[test]
    fn only_rust_selects_the_native_supervisor() {
        assert_eq!(choose(Some("rust"), None), Choice::Native);
        assert_eq!(choose(Some("rust"), Some("/x/y.sh")), Choice::Native);
        for other in ["RUST", "Rust", " rust", "rust ", "1", "node", "native", "bash", "SH"] {
            match choose(Some(other), None) {
                Choice::Refuse(msg) => assert!(msg.contains(SWITCH), "{msg}"),
                c => unreachable!("{other:?} -> {c:?}"),
            }
        }
    }
}
