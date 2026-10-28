//! Original source-text read contract over existing projects.json and content-addressed text.
//! No source pointer is rewritten when its blob disappears or becomes unreadable.
use crate::{chats, js, store, Ctx, Reply};
use js_json::{wtf16, JValue};
use std::io::Read;
const MAX_TEXT: u64 = 64 * 1024 * 1024;

/// Read each path component without following symlinks. IDs use the original source-text safeId.
pub fn text(dir: &std::path::Path, project: &str, file: &JValue) -> Result<String, ()> {
    if let Some(content) = file.get("content").as_str() {
        return Ok(content.to_owned());
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
        return Err(());
    }
    use rustix::fs::{open, openat, Mode, OFlags};
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW;
    let root = open(dir, flags, Mode::empty()).map_err(|_| ())?;
    let sources = openat(&root, "project-sources", flags, Mode::empty()).map_err(|_| ())?;
    let project = openat(&sources, id.as_str(), flags, Mode::empty()).map_err(|_| ())?;
    let fd = openat(
        &project,
        format!("{reference}.txt"),
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(|_| ())?;
    let file = std::fs::File::from(fd);
    let metadata = file.metadata().map_err(|_| ())?;
    if !metadata.is_file() || metadata.len() > MAX_TEXT {
        return Err(());
    }
    let mut bytes = Vec::new();
    file.take(MAX_TEXT + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| ())?;
    if bytes.len() as u64 > MAX_TEXT {
        return Err(());
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

pub fn get(ctx: Ctx<'_>, raw_project: &str, name: Option<&str>) -> Reply {
    let Some(id) = js::decode_uri_component(raw_project) else {
        return Reply::error(400, "Invalid project identifier");
    };
    if id == "cowork-diary-extras" || id.starts_with("cowork-chat-context-") {
        return Reply::error(404, "no such project");
    }
    let dir = match store::user_dir(ctx.data_dir, ctx.user_id) {
        Ok(v) => v,
        Err(_) => return Reply::internal(),
    };
    // Read and apply getProject's existing normalization while holding the shared project lock.
    match store::update(&dir, ctx.switch, |projects| {
        let Some(project) = store::find_mut(projects, &id) else {
            return (Reply::error(404, "no such project"), false);
        };
        let effects = match chats::get_project_effects(project) {
            Ok(v) => v,
            Err(_) => return (Reply::internal(), false),
        };
        let changed = effects.saved.is_some();
        if let Some(saved) = effects.saved {
            *project = saved;
        }
        let reply = (|| {
            let Some(name) = name.filter(|s| !s.is_empty()) else {
                return Reply::error(400, "a source name is required");
            };
            let JValue::Arr(files) = effects.memory.get("files") else {
                return Reply::error(404, "no such source");
            };
            let Some(file) = files.iter().find(|f| f.get("name").as_str() == Some(name)) else {
                return Reply::error(404, "no such source");
            };
            let content =
                match text(&dir, &id, file) {
                    Ok(v) => v,
                    Err(()) => return Reply::json(
                        404,
                        wtf16::stringify(&JValue::obj([
                            (
                                "error",
                                JValue::from(
                                    "The text of this source is missing. Re-upload or refresh it.",
                                ),
                            ),
                            ("code", JValue::from("SOURCE_TEXT_MISSING")),
                            ("name", JValue::from(name)),
                        ]))
                        .unwrap_or_default(),
                    ),
                };
            let chars = wtf16::len(&content);
            Reply::json(
                200,
                wtf16::stringify(&JValue::obj([
                    ("name", JValue::from(name)),
                    ("content", JValue::from(content)),
                    ("chars", JValue::Num(chars as f64)),
                ]))
                .unwrap_or_default(),
            )
        })();
        (reply, changed)
    }) {
        Ok(v) => v,
        Err(_) => Reply::internal(),
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
