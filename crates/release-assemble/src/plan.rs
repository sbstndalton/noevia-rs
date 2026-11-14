//! Component resolution: `resolve_component` and the noevia-rs block of assemble-release.sh.
//!
//! The order is the shell's: web, core (then core's `code-sandbox/` when core is `self`), services,
//! then noevia-rs, and each component is fetched before the next one is read, so the first refusal
//! (a bad pin or a failed download) is the same one the shell stops at. The tree work itself is
//! behind [`Source`], so the decisions are testable without git or the network.

use crate::lock::{lower_hex, Lock};
use std::path::{Path, PathBuf};

/// One component of the release tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Component {
    Web,
    Core,
    Services,
}

impl Component {
    pub const ALL: [Component; 3] = [Component::Web, Component::Core, Component::Services];

    /// The lock key stem (`NOEVIA_WEB` → `NOEVIA_WEB_REF`, `NOEVIA_WEB_SHA256`).
    pub fn key(self) -> &'static str {
        match self {
            Component::Web => "NOEVIA_WEB",
            Component::Core => "NOEVIA_CORE",
            Component::Services => "NOEVIA_SERVICES",
        }
    }

    /// The repo fetched at a pinned ref.
    pub fn repo(self) -> &'static str {
        match self {
            Component::Web => "noevia-web",
            Component::Core => "noevia-core",
            Component::Services => "noevia-services",
        }
    }

    /// The directory under the release tree.
    pub fn dir(self) -> &'static str {
        match self {
            Component::Web => "web",
            Component::Core => "core",
            Component::Services => "services",
        }
    }

    /// The `self` export: the prefix inside the noevia commit and the paths under it (tools/repo-split's map).
    pub fn self_export(self) -> (&'static str, &'static [&'static str]) {
        match self {
            Component::Web => (
                "apps/web",
                &[
                    "src",
                    "public",
                    "index.html",
                    "vite.config.ts",
                    "tsconfig.json",
                    "package.json",
                    "package-lock.json",
                    "scripts",
                    "contracts",
                    "tests/client",
                    "qa",
                    "tests/hermetic-network.cjs",
                    ".gitignore",
                    ".dockerignore",
                ],
            ),
            Component::Core => (
                "apps/web",
                &[
                    "server",
                    "contracts",
                    "tests/server",
                    "tests/fixtures",
                    "tests/hermetic-network.cjs",
                ],
            ),
            Component::Services => (
                "services",
                &["diary", "docling", "laya", "model-manager", "ocr"],
            ),
        }
    }
}

/// noevia-core's own `services/code-sandbox` export when core is `self`.
pub const CORE_SANDBOX_EXPORT: (&str, &[&str]) = ("services", &["code-sandbox"]);

/// How one component is obtained.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resolution {
    /// Exported from the noevia commit (`self`).
    SelfExport,
    /// Fetched from codeload at a 40-hex ref and checked against a 64-hex sha256.
    Pinned { reference: String, sha256: String },
}

/// What the tree work needs from the outside world. The shell's are `git archive | tar -x` and
/// `curl … && sha256 check && tar -xzf --strip-components=1`.
pub trait Source {
    /// `export_self DEST PREFIX PATHS…`: Err is the full refusal (already worded like `die`).
    fn export_self(&mut self, dest: &Path, prefix: &str, paths: &[&str]) -> Result<(), String>;
    /// `fetch_remote REPO REF SUM DEST`.
    fn fetch(
        &mut self,
        repo: &str,
        reference: &str,
        sha256: &str,
        dest: &Path,
    ) -> Result<(), String>;
    /// `[ -f PATH ]`.
    fn is_file(&self, path: &Path) -> bool;
}

/// The resolved shas, as written to `release-refs`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolved {
    pub web: String,
    pub core: String,
    pub services: String,
    /// Empty when the lock pins no noevia-rs tarball (locks from before M1).
    pub rs: String,
}

fn shown(v: &[u8]) -> String {
    String::from_utf8_lossy(v).into_owned()
}

/// Pure decision of `resolve_component` for one component (no tree work).
pub fn resolve(lock: &Lock, component: Component, sha: &str) -> Result<Resolution, String> {
    let key = component.key();
    let mut reference = lock.value(&format!("{key}_REF"));
    let sum = lock.value(&format!("{key}_SHA256"));
    // Locks from before #952 have no NOEVIA_SERVICES_REF: services were always exported from noevia.
    if reference.is_empty()
        && component == Component::Services
        && !lock.has_key("NOEVIA_SERVICES_REF")
    {
        reference = b"self".to_vec();
    }
    if reference == b"self" {
        Ok(Resolution::SelfExport)
    } else if lower_hex(&reference, 40) {
        if !lower_hex(&sum, 64) {
            return Err(format!(
                "{key}_REF is pinned but {key}_SHA256 is missing or not 64 lowercase hex"
            ));
        }
        Ok(Resolution::Pinned {
            reference: shown(&reference),
            sha256: shown(&sum),
        })
    } else {
        Err(format!(
            "{key}_REF in versions.lock at {sha} must be 'self' or a 40-char lowercase sha, got: '{}'",
            shown(&reference)
        ))
    }
}

/// The noevia-rs pin: None when the lock has no `NOEVIA_RS_SHA256` (an empty `rs/`).
pub fn resolve_rs(lock: &Lock, sha: &str) -> Result<Option<(String, String)>, String> {
    let reference = lock.value("NOEVIA_RS_REF");
    let sum = lock.value("NOEVIA_RS_SHA256");
    if sum.is_empty() {
        return Ok(None);
    }
    if !lower_hex(&reference, 40) {
        return Err(format!(
            "NOEVIA_RS_REF in versions.lock at {sha} must be a 40-char lowercase sha, got: '{}'",
            shown(&reference)
        ));
    }
    if !lower_hex(&sum, 64) {
        return Err(format!(
            "NOEVIA_RS_SHA256 in versions.lock at {sha} is not 64 lowercase hex"
        ));
    }
    Ok(Some((shown(&reference), shown(&sum))))
}

/// The component steps of the assembly, in the shell's order, into `tree/{web,core,services,rs}`.
/// `sha` is the noevia sha as given, `full_sha` what it resolved to (the `self` components' sha).
pub fn assemble_components(
    lock: &Lock,
    sha: &str,
    full_sha: &str,
    tree: &Path,
    source: &mut dyn Source,
) -> Result<Resolved, String> {
    let mut shas: Vec<String> = Vec::with_capacity(3);
    for component in Component::ALL {
        let dest: PathBuf = tree.join(component.dir());
        let got = match resolve(lock, component, sha)? {
            Resolution::SelfExport => {
                let (prefix, paths) = component.self_export();
                source.export_self(&dest, prefix, paths)?;
                full_sha.to_string()
            }
            Resolution::Pinned { reference, sha256 } => {
                source.fetch(component.repo(), &reference, &sha256, &dest)?;
                reference
            }
        };
        // noevia-core also owns services/code-sandbox, at code-sandbox/ (tools/repo-split's map). The
        // shell re-reads the raw lock value here: only a literal `self` triggers it.
        if component == Component::Core && lock.value("NOEVIA_CORE_REF") == b"self" {
            source.export_self(&dest, CORE_SANDBOX_EXPORT.0, CORE_SANDBOX_EXPORT.1)?;
        }
        shas.push(got);
    }
    let rs = match resolve_rs(lock, sha)? {
        None => String::new(),
        Some((reference, sum)) => {
            let dest = tree.join("rs");
            source.fetch("noevia-rs", &reference, &sum, &dest)?;
            if !source.is_file(&dest.join("Cargo.toml")) {
                return Err(format!("noevia-rs at {reference} has no Cargo.toml"));
            }
            reference
        }
    };
    let mut it = shas.into_iter();
    Ok(Resolved {
        web: it.next().unwrap_or_default(),
        core: it.next().unwrap_or_default(),
        services: it.next().unwrap_or_default(),
        rs,
    })
}
