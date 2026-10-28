//! Original workspace metadata and bounded transcript search over existing state.
//! Workspace reads run the original hourly retention sweep through native tombstoned deletion.
use crate::{chats, free_chats, retention, store, transcript, Ctx, Reply};
use js_json::{wtf16, JValue};
fn visible(project: &JValue) -> bool {
    project
        .get("id")
        .as_str()
        .is_some_and(|id| id != "cowork-diary-extras" && !id.starts_with("cowork-chat-context-"))
}
fn serial(value: JValue) -> Reply {
    match wtf16::stringify(&value) {
        Some(v) => Reply::json(200, v),
        None => Reply::internal(),
    }
}
fn lists(ctx: Ctx<'_>) -> Result<(Vec<JValue>, Vec<JValue>), Reply> {
    let dir = store::user_dir(ctx.data_dir, ctx.user_id).map_err(|_| Reply::internal())?;
    let projects = free_chats::workspace_projects(&dir)?;
    if projects.iter().any(|p| p.get("id").as_str().is_none()) {
        return Err(Reply::internal());
    }
    let free = free_chats::as_list(
        store::read_json(&dir.join(chats::FREE_CHATS_FILE)).map_err(|_| Reply::internal())?,
    );
    Ok((free, projects.into_iter().filter(visible).collect()))
}
pub fn get(ctx: Ctx<'_>) -> Reply {
    if let Err(reply) = retention::sweep(ctx) {
        return reply;
    }
    let (free, projects) = match lists(ctx) {
        Ok(v) => v,
        Err(v) => return v,
    };
    let mut out = Vec::new();
    for mut project in projects {
        let chats = match chats::sanitize(project.get("chats")) {
            Ok(v) => v,
            Err(_) => return Reply::internal(),
        };
        store::set(&mut project, "chats", JValue::Arr(chats));
        if let JValue::Arr(files) = project.get("files") {
            let mut meta = Vec::new();
            for file in files {
                let JValue::Obj(entries) = file else {
                    meta.push(file.clone());
                    continue;
                };
                let mut value = JValue::Obj(
                    entries
                        .iter()
                        .filter(|(key, _)| key != "content" && key != "textMissing")
                        .cloned()
                        .collect(),
                );
                let valid_ref = file.get("textRef").as_str().is_some_and(|r| {
                    r.len() == 64
                        && r.bytes()
                            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                });
                if !valid_ref {
                    if let Some(content) = file.get("content").as_str() {
                        store::set(&mut value, "chars", JValue::Num(wtf16::len(content) as f64));
                    }
                }
                // A metadata read never loads source blobs. The explicit source endpoint detects
                // missing text; adoption of the original asynchronous missing-text scanner remains open.
                meta.push(value);
            }
            store::set(&mut project, "files", JValue::Arr(meta));
        }
        out.push(project);
    }
    let free = match chats::sanitize(&JValue::Arr(free)) {
        Ok(v) => v,
        Err(_) => return Reply::internal(),
    };
    serial(JValue::obj([
        ("projects", JValue::Arr(out)),
        ("freeChats", JValue::Arr(free)),
    ]))
}
fn units(s: &str) -> Vec<u16> {
    let mut out = Vec::new();
    for c in s.chars() {
        if let Some(u) = wtf16::lone_unit(c) {
            out.push(u)
        } else {
            out.extend(c.encode_utf16(&mut [0; 2]).iter().copied())
        }
    }
    out
}
pub fn search(ctx: Ctx<'_>, query: &str) -> Reply {
    let query = crate::js::trim(query);
    let len = wtf16::len(query);
    if len > 200 {
        return Reply::error(400, "Search for at most 200 characters.");
    }
    if len < 2 {
        return serial(JValue::obj([
            ("chats", JValue::Arr(Vec::new())),
            ("truncated", JValue::Bool(false)),
        ]));
    }
    let needle = prompt_framing::js::to_lower(&units(query));
    let dir = match store::user_dir(ctx.data_dir, ctx.user_id) {
        Ok(v) => v,
        Err(_) => return Reply::internal(),
    };
    let deleted = match chats::read_tombstones(&dir) {
        Ok(v) => v,
        Err(_) => return Reply::internal(),
    };
    let (free, projects) = match lists(ctx) {
        Ok(v) => v,
        Err(v) => return v,
    };
    let mut candidates = free;
    for project in projects {
        match project.get("chats") {
            JValue::Arr(v) => candidates.extend(v.iter().cloned()),
            v if !v.truthy() => (),
            _ => return Reply::internal(),
        }
    }
    let mut found = Vec::new();
    let mut scanned = 0;
    let mut truncated = false;
    for chat in candidates {
        let Some(id) = chat.get("id").as_str() else {
            continue;
        };
        if deleted.contains(id) {
            continue;
        }
        if scanned >= 500 {
            truncated = true;
            break;
        }
        scanned += 1;
        let history = match transcript::read(ctx.data_dir, ctx.user_id, id) {
            Ok(v) => v.history,
            Err(_) => return Reply::internal(),
        };
        let JValue::Arr(history) = history else {
            return Reply::internal();
        };
        if history.iter().any(|m| {
            matches!(m.get("role").as_str(), Some("user" | "assistant"))
                && m.get("content").as_str().is_some_and(|s| {
                    prompt_framing::js::to_lower(&units(s))
                        .windows(needle.len())
                        .any(|w| w == needle)
                })
        }) {
            found.push(JValue::from(id));
            if found.len() >= 100 {
                truncated = true;
                break;
            }
        }
    }
    serial(JValue::obj([
        ("chats", JValue::Arr(found)),
        ("truncated", JValue::Bool(truncated)),
    ]))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn metadata_hides_text_internal_projects_and_search_obeys_roles_and_tombstones(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        let user = "11111111-2222-4333-8444-555555555555";
        let dir = root.path().join("users").join(user);
        std::fs::create_dir_all(&dir)?;
        std::fs::write(
            dir.join("projects.json"),
            r#"{"projects":[{"id":"p","files":[{"name":"n.md","content":"PRIVATE SOURCE"}],"chats":[null,{"id":"c1"}]},{"id":"cowork-diary-extras","files":[]},{"id":"cowork-chat-context-c1","files":[]}] }"#,
        )?;
        std::fs::write(
            dir.join("free-chats.json"),
            r#"[{"id":"gone"},{"id":"tool-only"}]"#,
        )?;
        std::fs::write(dir.join("deleted-chats.json"), r#"["gone"]"#)?;
        for (id, role) in [("c1", "assistant"), ("gone", "user"), ("tool-only", "tool")] {
            std::fs::write(
                dir.join(format!("history-{id}.json")),
                format!(r#"{{"history":[{{"role":"{role}","content":"Synthetic EVIDENCE"}}]}}"#),
            )?;
        }
        let ctx = Ctx {
            data_dir: root.path(),
            user_id: user,
            switch: server_store::RustProjects::owned(),
            now_ms: 1,
        };
        let meta = get(ctx);
        assert_eq!(meta.status, 200);
        let crate::Body::Json(text) = meta.body else {
            return Err("body".into());
        };
        assert!(!text.contains("PRIVATE SOURCE"));
        assert!(!text.contains("cowork-"));
        assert!(text.contains("\"chars\":14"));
        let reply = search(ctx, "evidence");
        assert_eq!(
            reply,
            Reply::json(200, r#"{"chats":["c1"],"truncated":false}"#.into())
        );
        std::fs::write(dir.join(retention::FILE), r#"{"days":30}"#)?;
        assert_eq!(get(ctx).status, 200);
        Ok(())
    }
}
