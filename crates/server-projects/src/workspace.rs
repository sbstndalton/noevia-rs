//! Original workspace metadata and bounded transcript search over existing state.
//! Workspace reads run the original hourly retention sweep through native tombstoned deletion.
use crate::{chats, free_chats, retention, store, transcript, workspace_scan, Ctx, Reply};
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
/// core index.cjs `DEFAULT_PROVIDER_ID`.
fn default_provider_id() -> String {
    std::env::var("DEFAULT_PROVIDER_ID")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "default".to_owned())
}

/// project-modes.cjs `migrate(project)`: the list names Chat first and keeps the other recorded
/// values; unchanged (and not rewritten) when it already does.
fn migrate_modes(project: &mut JValue) {
    if !matches!(project, JValue::Obj(_)) {
        return;
    }
    let before: Option<Vec<JValue>> = match project.get("modes") {
        JValue::Arr(items) => Some(items.clone()),
        _ => None,
    };
    let has = |mode: &str| {
        before
            .as_ref()
            .is_some_and(|b| b.iter().any(|m| m.as_str() == Some(mode)))
    };
    let mut next = vec![JValue::from("chat")];
    for mode in ["cowork", "code"] {
        if has(mode) {
            next.push(JValue::from(mode));
        }
    }
    if before.as_ref().is_some_and(|b| *b == next) {
        return;
    }
    store::set(project, "modes", JValue::Arr(next));
}

/// What core's workspace load (workspace.cjs) makes of a stored project before any route sees it:
/// a null or primitive entry of `files` dropped (#1236), the retired `lemonade` provider id read as
/// the default's, and the `modes` migration (#1576).
fn loaded(mut project: JValue, default_provider: &str) -> JValue {
    if let JValue::Arr(files) = project.get("files") {
        let kept: Vec<JValue> = files
            .iter()
            .filter(|f| matches!(f, JValue::Obj(_) | JValue::Arr(_)))
            .cloned()
            .collect();
        store::set(&mut project, "files", JValue::Arr(kept));
    }
    if project.get("provider").as_str() == Some("lemonade") {
        store::set(&mut project, "provider", JValue::from(default_provider));
    }
    migrate_modes(&mut project);
    project
}

fn valid_ref(file: &JValue) -> Option<&str> {
    file.get("textRef").as_str().filter(|r| {
        r.len() == 64
            && r.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

/// source-text.cjs `meta(file)`: a source as the client gets it in a list, its text never; `chars`
/// for an inline text, and `textMissing: true` when its text file is known to be gone (#1815/#1818).
fn source_meta(dir: &std::path::Path, project: &str, file: &JValue) -> JValue {
    let mut value = match file {
        JValue::Obj(entries) => JValue::Obj(
            entries
                .iter()
                .filter(|(key, _)| key != "content" && key != "textMissing")
                .cloned()
                .collect(),
        ),
        // `Object.keys(array)`: its indexes.
        JValue::Arr(items) => JValue::Obj(
            items
                .iter()
                .enumerate()
                .map(|(i, v)| (i.to_string(), v.clone()))
                .filter(|(key, _)| key != "content" && key != "textMissing")
                .collect(),
        ),
        other => return other.clone(),
    };
    let reference = valid_ref(file);
    if reference.is_none() {
        if let Some(content) = file.get("content").as_str() {
            store::set(&mut value, "chars", JValue::Num(wtf16::len(content) as f64));
        }
    }
    // An external source is one whose text is in its file: a valid pointer and no inline text.
    let inline = matches!(file, JValue::Obj(items) if items.iter().any(|(k, _)| k == "content"));
    if let (Some(reference), false) = (reference, inline) {
        if crate::workspace_scan::known_missing(dir, project, reference) {
            store::set(&mut value, "textMissing", JValue::Bool(true));
        }
    }
    value
}

/// The attachments project of a chat deleted before #788 is removed once the deletion is proven: the
/// chat's id is tombstoned (a tombstoned id never comes back: mergeChats and moveChat refuse it, and ids
/// are random) and no list holds it. A context whose chat is merely absent from every list stays: a new
/// chat creates its context when it opens, before its first message saves the meta. Idempotent, and at
/// most `limit` per call (each one a projects.json rewrite) so a large backlog drains over a few loads
/// instead of stalling the first request that runs it. projects.cjs `removeOrphanChatContexts`; the
/// removal is the project delete without its chats (`purgeChats: false`). Returns how many went.
pub fn reap_orphan_contexts(ctx: Ctx<'_>, limit: usize) -> usize {
    const PREFIX: &str = "cowork-chat-context-";
    let Ok(dir) = store::user_dir(ctx.data_dir, ctx.user_id) else {
        return 0;
    };
    let Ok(projects) = free_chats::workspace_projects(&dir) else {
        return 0;
    };
    let contexts: Vec<&str> = projects
        .iter()
        .filter_map(|p| p.get("id").as_str())
        .filter(|id| id.starts_with(PREFIX))
        .collect();
    if contexts.is_empty() {
        return 0;
    }
    let Ok(tombstones) = chats::read_tombstones(&dir) else {
        return 0;
    };
    let mut listed: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let free = store::read_json(&dir.join(chats::FREE_CHATS_FILE))
        .ok()
        .flatten()
        .map(|v| free_chats::as_list(Some(v)))
        .unwrap_or_default();
    for chat in &free {
        if let Some(id) = chat.get("id").as_str() {
            listed.insert(id);
        }
    }
    let mut removed = 0;
    for context in contexts {
        if removed >= limit {
            break;
        }
        let chat_id = &context[PREFIX.len()..];
        if !tombstones.contains(chat_id) {
            continue;
        }
        let in_a_project = projects.iter().any(|p| match p.get("chats") {
            JValue::Arr(chats) => chats.iter().any(|c| c.get("id").as_str() == Some(chat_id)),
            _ => false,
        });
        if in_a_project || listed.contains(chat_id) {
            continue;
        }
        if crate::context::chat_project_id(chat_id).as_deref() != Some(context) {
            continue;
        }
        match crate::lifecycle::remove(ctx, &dir, context, false) {
            Ok(true) => removed += 1,
            Ok(false) => {}
            Err(r) => eprintln!(
                "noevia-server: could not remove the attachments of a deleted chat ({})",
                r.status
            ),
        }
    }
    removed
}

pub fn get(ctx: Ctx<'_>) -> Reply {
    // The sweep and the cleanup are the list's chance to run them, never its failure: core logged a
    // sweep that failed and went on to answer.
    if let Err(r) = retention::sweep(ctx) {
        eprintln!(
            "noevia-server: the delete-old-chats sweep failed ({})",
            r.status
        );
    }
    reap_orphan_contexts(ctx, 10);
    let (free, projects) = match lists(ctx) {
        Ok(v) => v,
        Err(v) => return v,
    };
    let dir = match store::user_dir(ctx.data_dir, ctx.user_id) {
        Ok(d) => d,
        Err(_) => return Reply::internal(),
    };
    let default_provider = default_provider_id();
    let mut out = Vec::new();
    for project in projects {
        let mut project = loaded(project, &default_provider);
        let chats = match chats::sanitize(project.get("chats")) {
            Ok(v) => v,
            Err(_) => return Reply::internal(),
        };
        store::set(&mut project, "chats", JValue::Arr(chats));
        if let JValue::Arr(files) = project.get("files") {
            let id = project.get("id").as_str().unwrap_or_default().to_owned();
            let meta: Vec<JValue> = files.iter().map(|f| source_meta(&dir, &id, f)).collect();
            store::set(&mut project, "files", JValue::Arr(meta));
        }
        out.push(project);
    }
    workspace_scan::kick(&dir);
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
