//! Which real engines the OCR tests need are missing here, so a test skips with that exact reason
//! instead of passing vacuously.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The tools the real-engine OCR/PDF tests run, in the reference's order.
pub const TOOLS: [&str; 5] = ["tesseract", "pdftoppm", "pdfinfo", "pdftotext", "gs"];
/// The OCR language data they need.
pub const LANGUAGES: [&str; 2] = ["eng", "deu"];

/// What `missing_engines` asks of the machine (a seam for tests, as the reference patches
/// `shutil.which` and `subprocess.run`).
pub trait Probe {
    /// Whether `tool` is an executable on the search path.
    fn which(&self, tool: &str) -> bool;
    /// The stdout of `tesseract --list-langs`, or `None` when it cannot be started, fails or
    /// takes longer than 30 s.
    fn list_langs(&self) -> Option<Vec<u8>>;
}

/// The real machine: `PATH` lookup and a 30-second `tesseract --list-langs`.
pub struct SystemProbe;

fn executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

impl Probe for SystemProbe {
    fn which(&self, tool: &str) -> bool {
        if tool.contains('/') {
            return executable(Path::new(tool));
        }
        std::env::var_os("PATH").is_some_and(|path| {
            std::env::split_paths(&path).any(|dir| {
                let dir = if dir.as_os_str().is_empty() { Path::new(".").to_path_buf() } else { dir };
                executable(&dir.join(tool))
            })
        })
    }

    fn list_langs(&self) -> Option<Vec<u8>> {
        let mut child = Command::new("tesseract")
            .arg("--list-langs")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .ok()?;
        let stdout = child.stdout.take()?;
        let stderr = child.stderr.take()?;
        let reader = std::thread::spawn(move || {
            let mut out = Vec::new();
            let _ = std::io::Read::read_to_end(&mut { stdout }, &mut out);
            out
        });
        // Drain stderr too so a chatty child can never block on a full pipe.
        let drain = std::thread::spawn(move || {
            let _ = std::io::copy(&mut { stderr }, &mut std::io::sink());
        });
        let deadline = Instant::now() + Duration::from_secs(30);
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
            }
        };
        let out = reader.join().ok();
        let _ = drain.join();
        status.filter(|s| s.success()).and(out)
    }
}

/// Names of the real engines the OCR tests need that are not usable here (empty = all present).
pub fn missing_engines(probe: &dyn Probe) -> Vec<String> {
    let mut missing: Vec<String> =
        TOOLS.iter().filter(|tool| !probe.which(tool)).map(|tool| (*tool).to_owned()).collect();
    if !missing.iter().any(|m| m == "tesseract") {
        let listed = probe.list_langs().unwrap_or_default();
        let listed = String::from_utf8_lossy(&listed);
        let installed: Vec<&str> = listed.split_whitespace().collect();
        missing.extend(
            LANGUAGES
                .iter()
                .filter(|lang| !installed.contains(lang))
                .map(|lang| format!("tesseract language data '{lang}'")),
        );
    }
    missing
}

/// The skip reason for `missing` ("" when nothing is missing).
pub fn skip_reason_from(missing: &[String]) -> String {
    if missing.is_empty() {
        String::new()
    } else {
        format!("real-engine OCR/PDF tests need what is not installed here: {}", missing.join(", "))
    }
}

/// The skip reason on this machine ("" when every engine is present).
pub fn skip_reason(probe: &dyn Probe) -> String {
    skip_reason_from(&missing_engines(probe))
}
