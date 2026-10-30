//! Original source-text read contract over existing projects.json and content-addressed text.
//! No source pointer is rewritten when its blob disappears or becomes unreadable.
use crate::{js, Ctx, Reply};
use js_json::{wtf16, JValue};
use std::io::Read;
const MAX_TEXT: u64 = 64 * 1024 * 1024;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextUnavailable;

/// Read each path component without following symlinks. IDs use the original source-text safeId.
pub fn text(
    dir: &std::path::Path,
    project: &str,
    file: &JValue,
) -> Result<String, TextUnavailable> {
    // An own `content` (inline text) is what Node reads: attachFile leaves such a source alone, and
    // the route answers `typeof file.content === 'string' ? file.content : ''`.
    if let JValue::Obj(items) = file {
        if let Some((_, content)) = items.iter().find(|(k, _)| k == "content") {
            return Ok(content.as_str().unwrap_or_default().to_owned());
        }
    }
    let Some(reference) = file.get("textRef").as_str() else {
        return Ok(String::new());
    };
    if reference.len() != 64
        || !reference
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Ok(String::new());
    }
    let id: String = project
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .collect();
    if id.is_empty() {
        return Err(TextUnavailable);
    }
    use rustix::fs::{open, openat, Mode, OFlags};
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW;
    let root = open(dir, flags, Mode::empty()).map_err(|_| TextUnavailable)?;
    let sources =
        openat(&root, "project-sources", flags, Mode::empty()).map_err(|_| TextUnavailable)?;
    let project =
        openat(&sources, id.as_str(), flags, Mode::empty()).map_err(|_| TextUnavailable)?;
    let fd = openat(
        &project,
        format!("{reference}.txt"),
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(|_| TextUnavailable)?;
    let file = std::fs::File::from(fd);
    let metadata = file.metadata().map_err(|_| TextUnavailable)?;
    if !metadata.is_file() || metadata.len() > MAX_TEXT {
        return Err(TextUnavailable);
    }
    let mut bytes = Vec::new();
    file.take(MAX_TEXT + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| TextUnavailable)?;
    if bytes.len() as u64 > MAX_TEXT {
        return Err(TextUnavailable);
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Whether a source's text file can be opened (source-text.cjs's #1818 scan: `fs.access(file, R_OK)`
/// in `users/<id>/project-sources/<safeId>/<ref>.txt`, here without following a symlink, as the read
/// above). `Some(false)` is a file that is gone or cannot be opened (`ENOENT`, `EACCES`, `ENOTDIR`,
/// `EPERM`, a refused symlink); any other failure is unknown (`None`) and flags nothing.
pub fn text_readable(dir: &std::path::Path, project: &str, reference: &str) -> Option<bool> {
    use rustix::fs::{open, openat, Mode, OFlags};
    use rustix::io::Errno;
    let id: String = project
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .collect();
    if id.is_empty() {
        return Some(false);
    }
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW;
    let gone = |e: Errno| {
        matches!(
            e,
            Errno::NOENT | Errno::ACCESS | Errno::NOTDIR | Errno::PERM | Errno::LOOP
        )
        .then_some(false)
    };
    let root = open(dir, flags, Mode::empty()).map_err(gone);
    let root = match root {
        Ok(fd) => fd,
        Err(v) => return v,
    };
    let sources = match openat(&root, "project-sources", flags, Mode::empty()) {
        Ok(fd) => fd,
        Err(e) => return gone(e),
    };
    let project = match openat(&sources, id.as_str(), flags, Mode::empty()) {
        Ok(fd) => fd,
        Err(e) => return gone(e),
    };
    match openat(
        &project,
        format!("{reference}.txt"),
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    ) {
        Ok(_) => Some(true),
        Err(e) => gone(e),
    }
}

pub fn get(ctx: Ctx<'_>, raw_project: &str, name: Option<&str>) -> Reply {
    // decodeURIComponent's URIError reaches index.cjs's errorResponse: a 500.
    let Some(id) = js::decode_uri_component(raw_project) else {
        return Reply::internal();
    };
    // getProject first (its self-heal and reconcile are saved even for an internal project),
    // then the internal check, as routes/projects.cjs does.
    let (dir, project) = match crate::originals::project(ctx, &id) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let Some(project) = project else {
        return Reply::error(404, "no such project");
    };
    if id == "cowork-diary-extras" || id.starts_with("cowork-chat-context-") {
        return Reply::error(404, "no such project");
    }
    let Some(name) = name.filter(|s| !s.is_empty()) else {
        return Reply::error(400, "a source name is required");
    };
    // `(project.files || []).find((f) => f && f.name === name)`.
    let file = match project.get("files") {
        JValue::Arr(files) => files
            .iter()
            .find(|f| f.truthy() && f.get("name").as_str() == Some(name)),
        v if !v.truthy() => None,
        _ => return Reply::internal(),
    };
    let Some(file) = file else {
        return Reply::error(404, "no such source");
    };
    let Some(file_name) = file.get("name").as_str() else {
        return Reply::internal();
    };
    let content = match text(&dir, &id, file) {
        Ok(v) => v,
        Err(TextUnavailable) => {
            return Reply::json(
                404,
                wtf16::stringify(&JValue::obj([
                    (
                        "error",
                        JValue::from(
                            "The text of this source is missing. Re-upload or refresh it.",
                        ),
                    ),
                    ("code", JValue::from("SOURCE_TEXT_MISSING")),
                    ("name", JValue::from(file_name)),
                ]))
                .unwrap_or_default(),
            )
        }
    };
    let chars = wtf16::len(&content);
    match wtf16::stringify(&JValue::obj([
        ("name", JValue::from(file_name)),
        ("content", JValue::from(content)),
        ("chars", JValue::Num(chars as f64)),
    ])) {
        Some(v) => Reply::json(200, v),
        None => Reply::internal(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn original_source_contract_keeps_pointer_and_refuses_symlink_and_internal(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        let user = "01234567-89ab-4cde-8fab-0123456789ab";
        let dir = root.path().join("users").join(user);
        let source = dir.join("project-sources").join("project1");
        std::fs::create_dir_all(&source)?;
        let reference = "a".repeat(64);
        std::fs::write(
            dir.join("projects.json"),
            format!(
                r#"{{"projects":[{{"id":"project1","files":[{{"name":"source.md","textRef":"{reference}","chars":3}},{{"name":"inline.md","content":"\\ud800"}}]}}]}}"#
            ),
        )?;
        let call = |id, name| {
            get(
                Ctx {
                    data_dir: root.path(),
                    user_id: user,
                    switch: server_store::RustProjects::owned(),
                    now_ms: 1,
                },
                id,
                Some(name),
            )
        };
        assert_eq!(call("project1", "source.md").status, 404);
        assert!(std::fs::read_to_string(dir.join("projects.json"))?.contains(&reference));
        std::fs::write(source.join(format!("{reference}.txt")), "A😀")?;
        let reply = call("project1", "source.md");
        assert_eq!(reply.status, 200);
        let crate::Body::Json(raw) = reply.body else {
            return Err("body".into());
        };
        let value: serde_json::Value = serde_json::from_str(&raw)?;
        assert_eq!(value.get("chars"), Some(&serde_json::json!(3)));
        let target = source.join(format!("{reference}.txt"));
        std::fs::rename(&target, source.join("held.txt"))?;
        std::os::unix::fs::symlink(source.join("held.txt"), target)?;
        assert_eq!(call("project1", "source.md").status, 404);
        assert_eq!(call("cowork-diary-extras", "source.md").status, 404);
        assert_eq!(call("cowork-chat-context-chat1", "source.md").status, 404);
        assert_eq!(call("project1", "unknown").status, 404);
        let other = "11234567-89ab-4cde-8fab-0123456789ab";
        std::fs::create_dir_all(root.path().join("users").join(other))?;
        assert_eq!(
            get(
                Ctx {
                    data_dir: root.path(),
                    user_id: other,
                    switch: server_store::RustProjects::owned(),
                    now_ms: 1
                },
                "project1",
                Some("source.md")
            )
            .status,
            404
        );
        Ok(())
    }
}
