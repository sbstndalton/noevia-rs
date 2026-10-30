//! A research report's two files in the project (core projects.cjs `researchReportTarget` and the
//! create-only half of `writeProjectTextFile`, sbstndalton/noevia#2218): new files only, through the upload's
//! own path ([`crate::upload::organized_locked`]), with what a report adds to it.
//!
//! - **The target** is a nonsecret digest of the exact account and destination. The journal keeps it with
//!   the report intent, so a later explicit retry cannot silently move to another storage connection, project
//!   folder or local fallback after a disconnect. It is core's digest, byte for byte, so a pending intent
//!   written by either process is judged the same by the other.
//! - **Create-only is proved first.** Nextcloud enforces `If-None-Match: *`; generic WebDAV may ignore it and
//!   S3 writes are not supported, so those are refused before any PUT. A configured Nextcloud kind is not
//!   evidence either: a private random file in this project's Text folder is written twice, and the report
//!   PUT starts only after the second write was refused and the bytes stayed. No user file is touched.
//! - **The outcome.** [`Failure::definitely_not_written`] is set only for a refusal before any write could
//!   have landed. Everything else keeps the journal's intent: the report is uncertain until reconciled.
use crate::extract::{Engine, Limits};
use crate::lifecycle::Catalogue;
use crate::source_storage::{BinaryRead, Condition, SourceStorage, Storage};
use crate::uploads_ingest as uploads;
use crate::{project_documents, store, upload, Body, Ctx};
use js_json::{wtf16, JValue};
use rand_core::{OsRng, RngCore};
use std::time::Duration;

/// Why nothing, or perhaps something, was written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub status: u16,
    pub message: String,
    pub definitely_not_written: bool,
}

fn refused(status: u16, message: impl Into<String>) -> Failure {
    Failure {
        status,
        message: message.into(),
        definitely_not_written: true,
    }
}

/// What the writer needs.
pub struct Env<'a> {
    pub ctx: Ctx<'a>,
    pub storage: &'a Storage<'a>,
    pub engine: &'a dyn Engine,
    pub limits: &'a Limits,
    pub catalogue: Option<&'a Catalogue>,
    pub project_root: &'a str,
    pub lock_wait: Duration,
}

fn sha_of(items: &[JValue]) -> String {
    uploads::sha_hex(
        wtf16::stringify(&JValue::Arr(items.to_vec()))
            .unwrap_or_default()
            .as_bytes(),
    )
}

/// project-edit-target.cjs `storageAccount`.
pub fn storage_account(
    kind: &str,
    base_url: &str,
    bucket: &str,
    region: &str,
    username: &str,
) -> String {
    let s = |t: &str| JValue::Str(t.to_owned());
    sha_of(&[
        s("noevia-storage-account"),
        s(kind),
        s(base_url.trim_end_matches('/')),
        s(bucket),
        s(region),
        s(username),
    ])
}

fn target_key(
    user: &str,
    project: &str,
    name: &str,
    folder: &str,
    remote: Option<&dyn SourceStorage>,
) -> String {
    let s = |t: &str| JValue::Str(t.to_owned());
    let destination = uploads::destination_for(folder, name, remote.is_some(), None, "");
    sha_of(&[
        s("research-report-target-v1"),
        s(user),
        s(project),
        s(if remote.is_some() { "remote" } else { "local" }),
        remote.map_or(JValue::Null, |r| JValue::Str(r.account())),
        JValue::Str(destination),
    ])
}

fn the_project(ctx: Ctx<'_>, id: &str) -> Result<(std::path::PathBuf, JValue), Failure> {
    match crate::originals::project(ctx, id) {
        Ok((dir, Some(p))) => Ok((dir, p)),
        _ => Err(refused(
            409,
            "The research project changed before its report write.",
        )),
    }
}

fn remote_of<'a>(storage: &'a Storage<'a>) -> Result<Option<&'a dyn SourceStorage>, Failure> {
    match storage {
        Storage::None => Ok(None),
        Storage::Remote(r) => Ok(Some(*r)),
        Storage::Refused(status, message) => Err(refused(*status, *message)),
    }
}

/// core `researchReportTarget(project, name)`: makes the project's folder when it has none, as a first
/// upload does, then the digest.
pub fn target(env: &Env<'_>, id: &str, name: &str) -> Result<String, Failure> {
    let lock = project_documents::source_lock(env.ctx.user_id, id);
    let Some((_held, ctx)) = project_documents::lock_restamped(&lock, env.lock_wait, env.ctx)
    else {
        return Err(refused(
            429,
            "Another upload or sync of this project is still running; try again shortly.",
        ));
    };
    let (dir, project) = the_project(ctx, id)?;
    let remote = remote_of(env.storage)?;
    let mut folder = project
        .get("projectFolder")
        .as_str()
        .unwrap_or("")
        .to_owned();
    if let (Some(storage), true) = (remote, folder.is_empty()) {
        let made = crate::source_storage::allocate_folder(
            storage,
            env.project_root,
            id,
            project.get("reservedFolder").as_str(),
        )
        .map_err(|_| {
            refused(
                503,
                "The research report folder is unavailable; no report file was written.",
            )
        })?;
        let held = made.clone();
        let saved = store::update(&dir, ctx.switch, |projects| {
            let Some(p) = store::find_mut(projects, id) else {
                return (Err(()), false);
            };
            store::set(p, "projectFolder", JValue::Str(held.clone()));
            let mut sources: Vec<JValue> = match p.get("sourceFolders") {
                JValue::Arr(a) => a.clone(),
                _ => Vec::new(),
            };
            if !sources.iter().any(|x| x.as_str() == Some(held.as_str())) {
                sources.push(JValue::Str(held.clone()));
            }
            store::set(p, "sourceFolders", JValue::Arr(sources));
            (Ok(()), true)
        });
        if !matches!(saved, Ok(Ok(()))) {
            return Err(refused(
                409,
                "The research project changed before its report write.",
            ));
        }
        folder = made;
    }
    Ok(target_key(ctx.user_id, id, name, &folder, remote))
}

fn random_token() -> String {
    let mut b = [0u8; 16];
    OsRng.fill_bytes(&mut b);
    uploads::sha_hex(&b)
}

/// core `verifyResearchCreateOnly`.
fn verify_create_only(storage: &dyn SourceStorage, folder: &str) -> Result<(), Failure> {
    let probe = format!(
        "{folder}/Text/noevia-research-create-probe-{}.tmp",
        random_token()
    );
    let first = random_token().into_bytes();
    let second = random_token().into_bytes();
    let refusal = |extra: &str| {
        refused(503, format!("Storage did not verify create-only report writes; no report file was written.{extra}"))
    };
    if storage.create_folder(&format!("{folder}/Text")).is_err() {
        return Err(refusal(""));
    }
    let receipt = match storage.write(&probe, &first, &Condition::IfNoneMatch) {
        Ok(r) if r.created => r,
        Ok(r) => {
            let (cleaned, _) = storage.discard(&probe, &r.etag);
            return Err(refusal(&leftover(cleaned, &probe)));
        }
        Err(_) => return Err(refusal(&leftover(false, &probe))),
    };
    let second_refused = match storage.write(&probe, &second, &Condition::IfNoneMatch) {
        Err(e) => e.path_taken(),
        Ok(_) => false,
    };
    let mut held = Vec::new();
    let mut no_check = |_: Option<u64>| Ok(());
    let kept = storage
        .read_binary(
            &probe,
            BinaryRead {
                cap: 64,
                timeout: Duration::from_secs(30),
                before_body: &mut no_check,
            },
            &mut |chunk: &[u8]| {
                held.extend_from_slice(chunk);
                Ok(())
            },
        )
        .is_ok()
        && held == first;
    // Only our exact bytes, under the exact version this process wrote, are removed.
    let (cleaned, _) = if kept && !receipt.etag.is_empty() {
        storage.discard(&probe, &receipt.etag)
    } else {
        (false, "")
    };
    if second_refused && kept && cleaned {
        return Ok(());
    }
    Err(refusal(&leftover(cleaned, &probe)))
}

fn leftover(cleaned: bool, probe: &str) -> String {
    if cleaned {
        String::new()
    } else {
        format!(" A private probe may remain at \"{probe}\"; review it before removing it.")
    }
}

fn reply_error(reply: &crate::Reply) -> (String, Option<String>) {
    let Body::Json(text) = &reply.body else {
        return (String::new(), None);
    };
    let parsed = wtf16::parse(text).unwrap_or(JValue::Null);
    (
        parsed.get("error").as_str().unwrap_or("").to_owned(),
        parsed.get("code").as_str().map(str::to_owned),
    )
}

/// core `writeProjectTextFile(project, name, text, { createOnly: true, expectedTarget })`.
pub fn write(
    env: &Env<'_>,
    id: &str,
    name: &str,
    text: &str,
    expected_target: &str,
) -> Result<(), Failure> {
    let bytes = text.as_bytes();
    uploads::validate(
        name,
        bytes.len() as u64,
        bytes.get(..upload_sniff::SNIFF_BYTES).unwrap_or(bytes),
    )
    .map_err(|_| {
        refused(
            400,
            format!("\"{name}\" cannot be saved as a project file; no report file was written."),
        )
    })?;
    let lock = project_documents::source_lock(env.ctx.user_id, id);
    let Some((_held, ctx)) = project_documents::lock_restamped(&lock, env.lock_wait, env.ctx)
    else {
        return Err(refused(
            429,
            "Another upload or sync of this project is still running; try again shortly.",
        ));
    };
    let (dir, project) = the_project(ctx, id)
        .map_err(|_| refused(409, "the project changed while writing; nothing was saved"))?;
    let listed = match project.get("files") {
        JValue::Arr(files) => files.iter().any(|f| {
            f.get("name")
                .as_str()
                .is_some_and(|n| n.rsplit('/').next() == Some(name))
        }),
        _ => false,
    };
    if listed {
        return Err(refused(
            409,
            format!("\"{name}\" already exists in this project; no report file was overwritten"),
        ));
    }
    let remote = remote_of(env.storage)?;
    let folder = project.get("projectFolder").as_str().unwrap_or("");
    if target_key(ctx.user_id, id, name, folder, remote) != expected_target {
        return Err(refused(
            409,
            "The research report target changed before the write; no report file was written.",
        ));
    }
    if let Some(storage) = remote {
        if !storage.nextcloud() {
            return Err(refused(
                409,
                "Research report auto-save needs local or Nextcloud storage; no file was written.",
            ));
        }
        if folder.is_empty() {
            return Err(refused(
                503,
                "The research report folder is unavailable; no report file was written.",
            ));
        }
        verify_create_only(storage, folder)?;
    }
    let body = JValue::obj([
        ("name", JValue::Str(name.to_owned())),
        ("organized", JValue::Bool(true)),
    ]);
    let upload_env = upload::Env {
        ctx,
        storage: env.storage,
        engine: env.engine,
        limits: env.limits,
        catalogue: env.catalogue,
        project_root: env.project_root,
        progress: &|_| {},
        lock_wait: env.lock_wait,
    };
    let reply = upload::organized_locked(&upload_env, &dir, id, &body, name, bytes, "", None);
    upload::prune(&dir, ctx, id);
    if reply.status < 400 {
        return Ok(());
    }
    let (message, code) = reply_error(&reply);
    if code.as_deref() == Some("sourceExists") {
        return Err(refused(409, format!("\"{name}\" already exists in storage; the report was not written. Sync this project's Sources and review the file.")));
    }
    Err(Failure {
        status: reply.status,
        message,
        definitely_not_written: false,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn the_digests_are_core_s() {
        // sha256 of the exact JSON.stringify text core hashes, computed by core's own expression in
        // tests/fixtures (crates/research-core record.cjs) and repeated here on a fixed input.
        assert_eq!(
            storage_account("nextcloud", "https://nc.test/", "", "", "ann"),
            uploads::sha_hex(
                br#"["noevia-storage-account","nextcloud","https://nc.test","","","ann"]"#
            )
        );
        assert_eq!(
            target_key("u1", "p1", "Research x.md", "", None),
            uploads::sha_hex(
                br#"["research-report-target-v1","u1","p1","local",null,"Research x.md"]"#
            )
        );
    }
}
