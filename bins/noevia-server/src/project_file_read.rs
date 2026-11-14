//! Track R13: the chat's built-in `read_project_file` tool, answered by the front for a native turn
//! instead of core's tool bridge (`/internal/chat/tools/execute`), behind `NOEVIA_RUST_READ_FILE=1`
//! (off by default: unset, every call goes to core as before).
//!
//! The call is answered here only when core's bridge would run exactly the tool branch
//! `server_projects::file_read` ports: the tool name, offered (`allowed`), not routed to an MCP
//! server, no chat-wide grant and no edit target, its arguments a JSON object, ids within the
//! bridge's caps, and the project found as stored now (`getProject`, with its self-heal saved as core
//! saves it). Everything else, and every call `file_read` leaves to core
//! ([`server_projects::file_read::Outcome::Core`]), goes to core's bridge as before.
//!
//! Core keeps counting the call: chat_native.rs asks core's native op `tool-used` (core's
//! `recordToolUse`, the usage store's single writer), as core's bridge counts a call it runs; the
//! release assembler pairs that op with core's NATIVE_OPS.
use chat_turn::host::ToolRun;
use js_json::{wtf16, JValue};
use server_projects::file_read::{read_project_file, Outcome};
use std::path::Path;
use std::sync::OnceLock;

/// The tool this module answers.
pub const NAME: &str = "read_project_file";
/// The bridge's caps on a project id, an exchange key and a chat key (rust-chat-native.cjs `text`).
const ID_CAP: usize = 200;

/// `NOEVIA_RUST_READ_FILE=1`, read once.
pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("NOEVIA_RUST_READ_FILE").as_deref() == Ok("1"))
}

/// `typeof v === 'string' && v.length <= cap` for a non-empty id.
fn id_ok(v: &str) -> bool {
    !v.is_empty() && wtf16::len(v) <= ID_CAP
}

/// The tool's text and failure flag when the front answers this call; `None` leaves it to core.
pub fn answer(data_dir: &Path, user: &str, run: &ToolRun) -> Option<(String, bool)> {
    if run.name != NAME
        || !run.allowed.iter().any(|a| a == NAME)
        || run.routes.iter().any(|(tool, _)| tool == NAME)
        || run.grant.is_some()
        || run.edit_target.is_some()
        || !id_ok(&run.exchange_key)
        || run.chat_key.as_deref().is_some_and(|k| !id_ok(k))
        || run.project_id.as_deref().is_some_and(|p| !id_ok(p))
    {
        return None;
    }
    // runToolCall: `rawArgs ? JSON.parse(rawArgs) : {}`, an object or core's own error.
    let args = if run.args.is_empty() {
        JValue::Obj(Vec::new())
    } else {
        wtf16::parse(&run.args).ok()?
    };
    if !args.is_object() {
        return None;
    }
    let (dir, project) = match run.project_id.as_deref() {
        Some(id) => {
            let ctx = server_projects::Ctx {
                data_dir,
                user_id: user,
                switch: server_store::RustProjects::owned(),
                now_ms: crate::identity::now_ms(),
            };
            match server_projects::originals::project(ctx, id) {
                // No such project: core answers it (404).
                Ok((dir, Some(p))) => (dir, Some(p)),
                _ => return None,
            }
        }
        None => (server_projects::store::user_dir(data_dir, user).ok()?, None),
    };
    // core's asUser: an account that is gone is core's answer.
    if !dir.is_dir() {
        return None;
    }
    match read_project_file(&dir, project.as_ref(), &args) {
        Outcome::Done(r) => Some((r.text, r.failed)),
        Outcome::Core => None,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    const USER: &str = "0000aaaa-1111-2222-3333-444455556666";

    fn run(args: &str) -> ToolRun {
        ToolRun {
            project_id: Some("qa-proj".into()),
            name: NAME.into(),
            args: args.into(),
            allowed: vec![NAME.into(), "get_current_time".into()],
            routes: Vec::new(),
            chat_key: Some("qa-chat".into()),
            exchange_key: "qa-exchange".into(),
            edit_target: None,
            grant: None,
        }
    }

    fn data_dir(tag: &str) -> std::path::PathBuf {
        let data = std::env::temp_dir().join(format!("qa-r13-front-{tag}-{}", std::process::id()));
        let dir = data.join("users").join(USER);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("projects.json"),
            r#"{"projects":[{"id":"qa-proj","name":"QA","files":[{"name":"QA-notes.md","content":"QA notes text"}]}]}"#,
        )
        .unwrap();
        data
    }

    #[test]
    fn answers_a_plain_read_and_leaves_the_rest_to_core() {
        let data = data_dir("plain");
        let (text, failed) = answer(&data, USER, &run(r#"{"name":"QA-notes.md"}"#)).unwrap();
        assert_eq!(text, "File \"QA-notes.md\" (13 chars):\n\nQA notes text");
        assert!(!failed);
        let (text, failed) = answer(&data, USER, &run(r#"{"name":"nope.md"}"#)).unwrap();
        assert!(failed && text.starts_with("ERROR: no project file named \"nope.md\""));
        // Core's answers: another tool, not offered, routed to MCP, a grant, bad JSON, not an object,
        // an unknown project, a missing account.
        let mut other = run("{}");
        other.name = "get_current_time".into();
        assert_eq!(answer(&data, USER, &other), None);
        let mut off = run("{}");
        off.allowed = vec!["get_current_time".into()];
        assert_eq!(answer(&data, USER, &off), None);
        let mut routed = run("{}");
        routed.routes = vec![(NAME.into(), "qa-server".into())];
        assert_eq!(answer(&data, USER, &routed), None);
        let mut granted = run("{}");
        granted.grant = Some(chat_turn::host::GrantRun {
            handle: "a".repeat(32),
            chat: "qa-chat".into(),
            scope: "qa".into(),
            binding: JValue::Null,
        });
        assert_eq!(answer(&data, USER, &granted), None);
        assert_eq!(answer(&data, USER, &run("{")), None);
        assert_eq!(answer(&data, USER, &run("[1]")), None);
        let mut gone = run("{}");
        gone.project_id = Some("qa-other".into());
        assert_eq!(answer(&data, USER, &gone), None);
        assert_eq!(
            answer(&data, "0000aaaa-1111-2222-3333-444455550000", &run("{}")),
            None
        );
        let mut long = run("{}");
        long.exchange_key = "k".repeat(201);
        assert_eq!(answer(&data, USER, &long), None);
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn a_call_without_a_project_lists_no_files() {
        let data = data_dir("none");
        let mut r = run(r#"{"name":"QA-notes.md"}"#);
        r.project_id = None;
        let (text, failed) = answer(&data, USER, &r).unwrap();
        assert_eq!(
            text,
            "ERROR: no project file named \"QA-notes.md\". Available: (none attached)"
        );
        assert!(failed);
        let _ = std::fs::remove_dir_all(&data);
    }
}
