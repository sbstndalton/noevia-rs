//! The end of the assembly: from a filled `tree/` to `noevia-release-<sha>.tar.gz`.
//!
//! Shell steps, in order (assemble-release.sh after the pairing guards):
//! 1. every required entry exists (`assembled tree lacks <need>`);
//! 2. `release-refs` (five `KEY=sha` lines; the noevia sha as given, not the full one);
//! 3. `find "$tree" -type d -path '*/server/ui-data' -prune -exec rm -rf {} +`;
//! 4. `.dockerignore`;
//! 5. `tar -czf <out>.partial -C "$tree" .dockerignore release-refs noevia web core services rs`, then rename.
//!
//! The Rust tarball differs from GNU tar's in metadata only (fixed mtime, uid/gid 0, normalized modes,
//! byte-ordered directories, pax instead of GNU long names): [`manifest`] lists what an extraction
//! gives (type, executable bit, size, content sha256 or link target), and two tarballs with the same
//! manifest build the same image context.

use crate::plan::Resolved;
use crate::tar::{self, Kind, Writer};
use std::fs;
use std::io::{self, Write};
use std::path::Path;

/// The entries the shell requires before it writes the tarball.
pub const REQUIRED: [&str; 13] = [
    "web/package.json",
    "web/package-lock.json",
    "web/src",
    "web/index.html",
    "core/server/package.json",
    "core/server/index.cjs",
    "core/contracts",
    "core/code-sandbox/Dockerfile",
    "services/diary/Dockerfile",
    "services/docling/Dockerfile",
    "services/laya/Dockerfile",
    "services/model-manager/Dockerfile",
    "services/ocr/Dockerfile",
];

/// The tarball's top-level members, in the shell's order.
pub const TOP: [&str; 7] = [
    ".dockerignore",
    "release-refs",
    "noevia",
    "web",
    "core",
    "services",
    "rs",
];

/// The `.dockerignore` the shell writes.
pub const DOCKERIGNORE: &str =
    "noevia\n!noevia/build/web-supervisor.sh\nservices\n**/server/ui-data\n";

/// `[ -e "$tree/$need" ] || die "assembled tree lacks $need"` (`-e` follows symlinks).
pub fn check_required(tree: &Path) -> Result<(), String> {
    for need in REQUIRED {
        if fs::metadata(tree.join(need)).is_err() {
            return Err(format!("assembled tree lacks {need}"));
        }
    }
    Ok(())
}

/// The `release-refs` text.
pub fn release_refs(sha: &str, r: &Resolved) -> String {
    format!(
        "NOEVIA_SHA={sha}\nNOEVIA_WEB_SHA={}\nNOEVIA_CORE_SHA={}\nNOEVIA_SERVICES_SHA={}\nNOEVIA_RS_SHA={}\n",
        r.web, r.core, r.services, r.rs
    )
}

/// `find TREE -type d -path '*/server/ui-data' -prune -exec rm -rf {} +`: removes every real directory
/// (never a symlink to one) whose path ends in `/server/ui-data`, without descending into it. Returns
/// how many were removed.
#[cfg(unix)]
pub fn prune_ui_data(tree: &Path) -> io::Result<usize> {
    use std::os::unix::ffi::OsStrExt;
    let mut removed = 0;
    let mut stack = vec![tree.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let meta = fs::symlink_metadata(&dir)?;
        if !meta.file_type().is_dir() {
            continue;
        }
        if dir.as_os_str().as_bytes().ends_with(b"/server/ui-data") {
            fs::remove_dir_all(&dir)?;
            removed += 1;
            continue;
        }
        for child in fs::read_dir(&dir)? {
            let child = child?;
            if child.file_type()?.is_dir() {
                stack.push(child.path());
            }
        }
    }
    Ok(removed)
}

/// Steps 1 to 5 over a tree whose components are in place. Writes `<out_dir>/noevia-release-<sha>.tar.gz`
/// through `<out>.partial` (removed on failure) and returns its path.
#[cfg(unix)]
pub fn finish(
    tree: &Path,
    sha: &str,
    resolved: &Resolved,
    out_dir: &Path,
    mtime: u64,
) -> Result<std::path::PathBuf, String> {
    check_required(tree)?;
    let io_err = |what: &str, e: io::Error| format!("{what}: {e}");
    fs::write(tree.join("release-refs"), release_refs(sha, resolved))
        .map_err(|e| io_err("release-refs", e))?;
    prune_ui_data(tree).map_err(|e| io_err("ui-data prune", e))?;
    fs::write(tree.join(".dockerignore"), DOCKERIGNORE).map_err(|e| io_err(".dockerignore", e))?;
    let out = out_dir.join(format!("noevia-release-{sha}.tar.gz"));
    let partial = out_dir.join(format!("noevia-release-{sha}.tar.gz.partial"));
    let written = pack(tree, &TOP, &partial, mtime).and_then(|()| fs::rename(&partial, &out));
    if let Err(e) = written {
        let _ = fs::remove_file(&partial);
        return Err(io_err("tar", e));
    }
    Ok(out)
}

/// `tar -czf OUT -C ROOT MEMBERS…`, deterministic (see the module docs).
#[cfg(unix)]
pub fn pack(root: &Path, members: &[&str], out: &Path, mtime: u64) -> io::Result<()> {
    let file = fs::File::create(out)?;
    let gz = flate2::GzBuilder::new()
        .mtime(0)
        .write(io::BufWriter::new(file), flate2::Compression::default());
    let mut w = Writer::new(gz, mtime);
    for m in members {
        w.add_path(root, m.as_bytes())?;
    }
    let gz = w.finish()?;
    let mut buf = gz.finish()?;
    buf.flush()
}

/// One line per member, sorted: `<d|f|x|l> <size> <sha256|-> <name>[ -> <target>]`, where `x` is an
/// executable file. A name listed twice keeps its last member (what an extraction leaves).
pub fn manifest(entries: &[tar::Entry]) -> Vec<String> {
    let mut by_name: std::collections::BTreeMap<Vec<u8>, String> =
        std::collections::BTreeMap::new();
    for e in entries {
        let mut name: Vec<u8> = e.name.clone();
        while name.starts_with(b"./") {
            name.drain(..2);
        }
        if name.is_empty() || name == b"." {
            continue;
        }
        let shown = String::from_utf8_lossy(&name).into_owned();
        let line = match &e.kind {
            Kind::Dir => format!("d 0 - {shown}"),
            Kind::File { executable, data } => format!(
                "{} {} {} {shown}",
                if *executable { "x" } else { "f" },
                data.len(),
                crate::digest::sha256_hex(data)
            ),
            Kind::Symlink { target } => {
                format!("l 0 - {shown} -> {}", String::from_utf8_lossy(target))
            }
        };
        by_name.insert(name, line);
    }
    by_name.into_values().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refs_text() {
        let r = Resolved {
            web: "w".into(),
            core: "c".into(),
            services: "s".into(),
            rs: String::new(),
        };
        assert_eq!(
            release_refs("abc1234", &r),
            "NOEVIA_SHA=abc1234\nNOEVIA_WEB_SHA=w\nNOEVIA_CORE_SHA=c\nNOEVIA_SERVICES_SHA=s\nNOEVIA_RS_SHA=\n"
        );
    }
}
