//! sbstndalton/noevia#2191 group "rust-project-reads": the stored bytes of a project source,
//! answered as core routes/projects.cjs does.
//!
//! - `GET /api/projects/{id}/documents/original?name=` (`docRead`, the `original` half): a PDF
//!   source's stored original, `<user dir>/project-documents/<sha256(id)>/<document.byteHash>.pdf`
//!   (document-sources.cjs `readOriginal`), as `application/pdf`. Any failure to read it (no valid
//!   byteHash, a missing or unreadable file) is Node's 422; no document is a 404.
//! - `GET /api/projects/{id}/uploads/original?name=` (`originalUpload`): an upload's stored bytes,
//!   `<user dir>/project-uploads/<sha256(id)>/<attachment.id>` (uploads.cjs `original`), as
//!   `application/octet-stream`; no attachment, no valid attachment id or a missing file is a 404
//!   (#786), any other read failure Node's 500.
//!
//! Both look the project up as `getProject` does ([`project`]): the context self-heal and the
//! instruction-skills reconcile are saved under the projects.json lock when they change it, exactly
//! as for the chat list. Neither file is ever written here; Node's ingest writes them (content
//! addressed, renamed into place) and Rust only reads.
//!
//! Hardened beyond Node: every path component below the user directory is opened with
//! `O_NOFOLLOW` (a symlink is refused, never followed out of the tenant directory), only a regular
//! file is served, and the file names come only from the validated hex hashes, never from the
//! request. Tenant scope is the gate's user id ([`crate::Ctx`]).

use crate::{chats, js, store, Body, Ctx, Reply};
use js_json::JValue;
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};

/// `/^[a-f0-9]{64}$/`.
pub(crate) fn hex64(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn sha256_hex(s: &str) -> String {
    let digest = Sha256::digest(s.as_bytes());
    let mut out = String::with_capacity(64);
    for b in digest {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// `getProject(id)` for a read route: the project as Node holds it afterwards, its effects saved
/// under the lock when they changed it (chats::get's shape). `Ok(None)` is no such project.
pub(crate) fn project(ctx: Ctx<'_>, id: &str) -> Result<(PathBuf, Option<JValue>), Reply> {
    let dir = store::user_dir(ctx.data_dir, ctx.user_id).map_err(|e| chats::store_failure(&e))?;
    let projects = store::read(&dir).map_err(|e| chats::store_failure(&e))?;
    let found = match chats::locate(&projects, id) {
        Err(()) => return Err(Reply::internal()),
        Ok(None) => return Ok((dir, None)),
        Ok(Some(i)) => projects.get(i).ok_or_else(Reply::internal)?,
    };
    let effects = chats::get_project_effects(found).map_err(|_| Reply::internal())?;
    if effects.saved.is_none() {
        return Ok((dir, Some(effects.memory)));
    }
    let saved = store::update(&dir, ctx.switch, |projects| {
        match chats::locate(projects, id) {
            Err(()) => (Err(Reply::internal()), false),
            Ok(None) => (Ok(None), false),
            Ok(Some(i)) => match projects.get_mut(i) {
                None => (Err(Reply::internal()), false),
                Some(p) => match chats::get_project_effects(p) {
                    Err(_) => (Err(Reply::internal()), false),
                    Ok(chats::Effects { memory, saved }) => {
                        let changed = saved.is_some();
                        if let Some(v) = saved {
                            *p = v;
                        }
                        (Ok(Some(memory)), changed)
                    }
                },
            },
        }
    });
    match saved {
        Ok(Ok(p)) => Ok((dir, p)),
        Ok(Err(reply)) => Err(reply),
        Err(e) => Err(chats::store_failure(&e)),
    }
}

/// Why a stored file could not be served.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReadError {
    /// ENOENT (a missing directory or file), or a symlink where a component should be.
    Missing,
    /// Anything else: a symlink or non-regular file, permissions, an I/O error.
    Other,
}

/// Reads `<dir>/<parts...>` opening each component with `O_NOFOLLOW`, so nothing below `dir`
/// can lead out of it; only a regular file is read.
pub(crate) fn read_below(dir: &Path, parts: &[&str]) -> Result<Vec<u8>, ReadError> {
    use rustix::fs::{open, openat, Mode, OFlags};
    let map = |e: rustix::io::Errno| {
        // A missing file, or a symlink or non-directory in its place: the original is not there
        // (a symlink is never followed out of the tenant directory).
        if e == rustix::io::Errno::NOENT
            || e == rustix::io::Errno::LOOP
            || e == rustix::io::Errno::NOTDIR
        {
            ReadError::Missing
        } else {
            ReadError::Other
        }
    };
    let (last, dirs) = parts.split_last().ok_or(ReadError::Other)?;
    let dflags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW;
    let mut fd = open(dir, dflags, Mode::empty()).map_err(map)?;
    for part in dirs {
        fd = openat(&fd, *part, dflags, Mode::empty()).map_err(map)?;
    }
    let file = openat(
        &fd,
        *last,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .map_err(map)?;
    let mut file = std::fs::File::from(file);
    let meta = file.metadata().map_err(|_| ReadError::Other)?;
    if !meta.is_file() {
        return Err(ReadError::Other);
    }
    let mut bytes = Vec::with_capacity(usize::try_from(meta.len()).unwrap_or(0));
    file.read_to_end(&mut bytes).map_err(|_| ReadError::Other)?;
    Ok(bytes)
}

/// JavaScript `encodeURIComponent`; `None` where it throws (a lone surrogate).
pub(crate) fn encode_uri_component(s: &str) -> Option<String> {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if js_json::wtf16::lone_unit(c).is_some() {
            return None;
        }
        let mut buf = [0u8; 4];
        for b in c.encode_utf8(&mut buf).bytes() {
            if b.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&b) {
                out.push(char::from(b));
            } else {
                out.push_str(&format!("%{b:02X}"));
            }
        }
    }
    Some(out)
}

/// `list.find(f => f.name === name && extra(f))`: `Err` where JS throws (a nullish entry
/// reached before the match), `None` for a `null` name (it matches no string).
pub(crate) fn find_named<'a>(
    list: &'a [JValue],
    name: Option<&str>,
    extra: impl Fn(&JValue) -> bool,
) -> Result<Option<&'a JValue>, ()> {
    for f in list {
        if f.is_nullish() {
            return Err(());
        }
        if name.is_some() && f.get("name").as_str() == name && extra(f) {
            return Ok(Some(f));
        }
    }
    Ok(None)
}

/// `name.split('/').pop()`.
fn base_name(name: &str) -> &str {
    name.rsplit('/').next().unwrap_or(name)
}

/// The files list as `project.files` reads in JS: `Err` where `.find` would throw (a truthy
/// non-array), `None` for a nullish one.
fn files(project: &JValue) -> Result<Option<&[JValue]>, ()> {
    match project.get("files") {
        JValue::Arr(v) => Ok(Some(v)),
        v if v.is_nullish() => Ok(None),
        _ => Err(()),
    }
}

fn bytes_reply(content_type: &str, name: &str, cache_first: bool, bytes: Vec<u8>) -> Reply {
    let Some(encoded) = encode_uri_component(base_name(name)) else {
        return Reply::internal();
    };
    let disposition = format!("attachment; filename*=UTF-8''{encoded}");
    let length = bytes.len().to_string();
    // Node's writeHead order for each route.
    let headers = if cache_first {
        vec![
            ("content-type", content_type.to_string()),
            ("content-disposition", disposition),
            ("cache-control", "private, no-store".to_string()),
            ("content-length", length),
        ]
    } else {
        vec![
            ("content-type", content_type.to_string()),
            ("content-disposition", disposition),
            ("content-length", length),
            ("cache-control", "private, no-store".to_string()),
        ]
    };
    Reply {
        status: 200,
        body: Body::Bytes { headers, bytes },
    }
}

/// `GET /api/projects/{id}/documents/original`.
pub fn document(ctx: Ctx<'_>, raw_project: &str, name: Option<&str>) -> Reply {
    // decodeURIComponent's URIError reaches index.cjs's errorResponse: a 500.
    let Some(id) = js::decode_uri_component(raw_project) else {
        return Reply::internal();
    };
    let (dir, project) = match project(ctx, &id) {
        Ok(v) => v,
        Err(r) => return r,
    };
    // `project?.files?.find(f => f.name === name)`: `null` (no name) matches nothing.
    let file = match project.as_ref().map(files) {
        None | Some(Ok(None)) => None,
        Some(Err(())) => return Reply::internal(),
        Some(Ok(Some(list))) => match find_named(list, name, |_| true) {
            Ok(v) => v,
            Err(()) => return Reply::internal(),
        },
    };
    let Some(file) = file.filter(|f| f.get("document").truthy()) else {
        return Reply::error(404, "no such document");
    };
    let unavailable = || {
        Reply::error(
            422,
            "Saved document data is unavailable; upload or refresh again.",
        )
    };
    let Some(hash) = file
        .get("document")
        .get("byteHash")
        .as_str()
        .filter(|h| hex64(h))
    else {
        return unavailable();
    };
    let Some(file_name) = file.get("name").as_str() else {
        return Reply::internal();
    };
    match read_below(
        &dir,
        &[
            "project-documents",
            &sha256_hex(&id),
            &format!("{hash}.pdf"),
        ],
    ) {
        Ok(bytes) => bytes_reply("application/pdf", file_name, false, bytes),
        Err(_) => unavailable(),
    }
}

/// `GET /api/projects/{id}/uploads/original`.
pub fn upload(ctx: Ctx<'_>, raw_project: &str, name: Option<&str>) -> Reply {
    let Some(id) = js::decode_uri_component(raw_project) else {
        return Reply::internal();
    };
    let (dir, project) = match project(ctx, &id) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let missing = || Reply::error(404, "No such original");
    let Some(project) = project else {
        return missing();
    };
    // `project?.files.find(...)`: a project without a files list throws (a 500).
    let list = match files(&project) {
        Ok(Some(list)) => list,
        Ok(None) | Err(()) => return Reply::internal(),
    };
    let file = match find_named(list, name, |f| f.get("attachment").truthy()) {
        Ok(v) => v,
        Err(()) => return Reply::internal(),
    };
    let Some(file) = file else {
        return missing();
    };
    let Some(attachment) = file
        .get("attachment")
        .get("id")
        .as_str()
        .filter(|h| hex64(h))
    else {
        return missing();
    };
    let Some(file_name) = file.get("name").as_str() else {
        return Reply::internal();
    };
    match read_below(&dir, &["project-uploads", &sha256_hex(&id), attachment]) {
        Ok(bytes) => bytes_reply("application/octet-stream", file_name, true, bytes),
        Err(ReadError::Missing) => missing(),
        Err(ReadError::Other) => Reply::internal(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_matches_javascript() {
        assert_eq!(
            encode_uri_component("a b/é(x)!~*'.pdf").as_deref(),
            Some("a%20b%2F%C3%A9(x)!~*'.pdf")
        );
        assert_eq!(base_name("Proj/Documents/r.pdf"), "r.pdf");
        assert_eq!(base_name("r.pdf"), "r.pdf");
        assert!(hex64(&"a".repeat(64)));
        assert!(!hex64(&"A".repeat(64)));
        assert!(!hex64("../x"));
    }
}
