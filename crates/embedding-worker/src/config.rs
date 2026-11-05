//! File publication shared by native request handlers and the background worker.
//! Callers perform asynchronous address/served/coverage/parity checks first, then
//! supply the checked generation. No file lock survives an await. This module
//! does not grant permission to switch or bypass those checks.
use crate::{runtime, Config, Target};
use serde_json::{json, Value};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

#[derive(Debug, PartialEq, Eq)]
pub struct Error {
    pub status: u16,
    pub code: &'static str,
    pub message: &'static str,
}
fn refusal(code: &'static str, message: &'static str) -> Error {
    Error {
        status: 409,
        code,
        message,
    }
}
fn unavailable() -> Error {
    Error {
        status: 503,
        code: "configUnavailable",
        message: "The embedding configuration is unavailable.",
    }
}

pub enum Change {
    SetNext(Target),
    ClearNext,
    Switch,
    Rollback,
    Finish,
}
/// A complete configuration is the proof's identity, including the content hash
/// generation. A saved file rewritten with different bytes invalidates the proof.
fn matches(a: &Config, b: &Config) -> bool {
    a.version == b.version && a.active == b.active && a.next == b.next && a.previous == b.previous
}
pub fn target_json(t: &Target) -> Value {
    json!({"baseUrl": t.base_url, "model": t.model, "profile": t.profile})
}
pub fn state_json(c: &Config) -> Value {
    json!({"active": target_json(&c.active), "next": c.next.as_ref().map(target_json), "previous": c.previous.as_ref().map(target_json)})
}
struct Temporary(PathBuf);
impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}
static SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// The returned targets are candidates for orphan cleanup, never permission to
/// delete a vector slot. Cleanup must recheck all three current identities under
/// the existing config/projects/outbox/index publication fence.
pub struct Published {
    pub config: Config,
    pub retired: Vec<Target>,
}

pub fn publish(
    root: &Path,
    seed: &Config,
    checked: &Config,
    change: Change,
) -> Result<Published, Error> {
    publish_checked(root, seed, checked, change, || Ok(()))
}
/// Synchronous final validation while holding the configuration publication
/// fence. Source outbox/index writers take this same fence, so coverage cannot
/// become stale between the last check and the atomic configuration rename.
pub fn publish_checked(
    root: &Path,
    seed: &Config,
    checked: &Config,
    change: Change,
    validate: impl FnOnce() -> Result<(), Error>,
) -> Result<Published, Error> {
    fs::create_dir_all(root).map_err(|_| unavailable())?;
    let file = root.join("embedding-config.json");
    let _hold = server_projects::lock::acquire(&file).map_err(|_| unavailable())?;
    let mut current = runtime::load_config(root, seed).map_err(|_| {
        refusal(
            "badState",
            "The saved embedding configuration cannot be read.",
        )
    })?;
    if !matches(&current, checked) {
        return Err(refusal(
            "nextChanged",
            "The re-index target changed while the switch was being checked. Nothing was switched.",
        ));
    }
    validate()?;
    let mut retired = Vec::new();
    match change {
        Change::SetNext(target) => {
            // Reuse the background owner's parser: invalid targets can never be
            // persisted even if a caller omitted its input validation.
            let target = runtime::parse_target(&target_json(&target)).map_err(|_| Error {
                status: 400,
                code: "invalidTarget",
                message: "Invalid embedding target.",
            })?;
            if target == current.active {
                return Err(refusal(
                    "alreadyActive",
                    "That model, profile and address are already active.",
                ));
            }
            retired.extend(current.next.replace(target));
        }
        Change::ClearNext => retired.extend(current.next.take()),
        Change::Switch => {
            let next = current
                .next
                .take()
                .ok_or_else(|| refusal("noNext", "No re-index is under way."))?;
            retired.extend(current.previous.take());
            current.previous = Some(std::mem::replace(&mut current.active, next));
        }
        Change::Rollback => {
            let previous = current.previous.take().ok_or_else(|| {
                refusal(
                    "noPrevious",
                    "There is no previous embedding model to return to.",
                )
            })?;
            retired.extend(current.next.take());
            current.previous = Some(std::mem::replace(&mut current.active, previous));
        }
        Change::Finish => retired.push(current.previous.take().ok_or_else(|| {
            refusal(
                "noPrevious",
                "There is no previous embedding model to remove.",
            )
        })?),
    }
    retired.retain(|t| {
        !std::iter::once(&current.active)
            .chain(current.next.iter())
            .chain(current.previous.iter())
            .any(|kept| kept.same_identity(t))
    });
    let mut value = state_json(&current);
    value
        .as_object_mut()
        .ok_or_else(unavailable)?
        .insert("version".into(), json!(1));
    let bytes = serde_json::to_vec_pretty(&value).map_err(|_| unavailable())?;
    let temp_path = root.join(format!(
        "embedding-config.json.{}.{}.tmp",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut output = options.open(&temp_path).map_err(|_| unavailable())?;
    let temp = Temporary(temp_path);
    output.write_all(&bytes).map_err(|_| unavailable())?;
    output.sync_all().map_err(|_| unavailable())?;
    fs::rename(&temp.0, file).map_err(|_| unavailable())?;
    // Derive the exact byte generation the background reader will use.
    let config = runtime::load_config(root, seed).map_err(|_| unavailable())?;
    Ok(Published { config, retired })
}
