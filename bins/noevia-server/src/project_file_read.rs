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
/// The bridge's caps (rust-chat-native.cjs `text`, `names`, the routes check and `readJson`): a project
/// id, an exchange key, a tool name or route string; a chat key; the allowed and routes lists; the args.
const ID_CAP: usize = 200;
const CHAT_ID_CAP: usize = 400;
const LIST_CAP: usize = 2000;
/// BODY_CAP: the bridge reads at most 16 MiB of request body (http.cjs readBody) and `args` at most
/// 16 Mi UTF-16 units; a call whose body would come near that is core's (its 400).
const BODY_BYTES: usize = 16 * 1024 * 1024;
/// Room for the rest of the body (keys, user, project, chat and exchange keys).
const BODY_SLACK: usize = 64 * 1024;

/// The bytes of a JSON string as the front's bridge body writes it (escapes included).
fn json_len(v: &str) -> usize {
    wtf16::stringify(&JValue::Str(v.to_string())).map_or(usize::MAX, |q| q.len())
}

/// Whether the execute bridge's request body for this call would stay within core's read cap.
fn body_fits(run: &ToolRun) -> bool {
    let mut total = BODY_SLACK.saturating_add(json_len(&run.args));
    for a in &run.allowed {
        total = total.saturating_add(json_len(a) + 1);
    }
    for (a, b) in &run.routes {
        total = total.saturating_add(json_len(a) + json_len(b) + 3);
    }
    total <= BODY_BYTES && wtf16::len(&run.args) <= BODY_BYTES
}

/// `NOEVIA_RUST_READ_FILE=1`, read once.
pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("NOEVIA_RUST_READ_FILE").as_deref() == Ok("1"))
}

/// `typeof v === 'string' && v.length <= cap` for a non-empty value.
fn within(v: &str, cap: usize) -> bool {
    !v.is_empty() && wtf16::len(v) <= cap
}

/// core's asUser for the bridge: the account's row exists and is not disabled (rust-chat-native.cjs
/// `createAsUser`; a revoked or removed account has no row). `false` when it cannot be read.
pub fn account_active(data_dir: &Path, user: &str) -> bool {
    let Ok(store) = server_store::Store::open(data_dir) else {
        return false;
    };
    store
        .read(|r| {
            r.row(
                "SELECT id FROM users WHERE id=?1 AND (disabled_at IS NULL OR disabled_at=0)",
                [user],
                |row| row.get::<_, String>(0),
            )
        })
        .ok()
        .flatten()
        .is_some()
}

/// The tool's text and failure flag when the front answers this call; `None` leaves it to core.
/// `active(user)` is core's account check ([`account_active`]).
pub fn answer(
    data_dir: &Path,
    user: &str,
    run: &ToolRun,
    active: impl FnOnce(&str) -> bool,
) -> Option<(String, bool)> {
    let listed = |v: &str| wtf16::len(v) <= ID_CAP;
    if run.name != NAME
        || !run.allowed.iter().any(|a| a == NAME)
        || run.allowed.len() > LIST_CAP
        || !run.allowed.iter().all(|a| listed(a))
        || run.routes.len() > LIST_CAP
        || !run.routes.iter().all(|(a, b)| listed(a) && listed(b))
        || run.routes.iter().any(|(tool, _)| tool == NAME)
        || run.grant.is_some()
        || run.edit_target.is_some()
        || !body_fits(run)
        || !within(&run.exchange_key, ID_CAP)
        || run
            .chat_key
            .as_deref()
            .is_some_and(|k| !within(k, CHAT_ID_CAP))
        || run
            .project_id
            .as_deref()
            .is_some_and(|p| !within(p, ID_CAP))
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
    if !active(user) {
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
    // core's asUser: an account that is gone, disabled or revoked is core's answer (404).
    if !dir.is_dir() {
        return None;
    }
    match read_project_file(&dir, project.as_ref(), &args) {
        Outcome::Done(r) => Some((r.text, r.failed)),
        Outcome::Core => None,
    }
}

/// `NOEVIA_RUST_INAPP_READS=1`, read once: the in-app MCP server's read-only project tools
/// (`project_list_files`, `project_read_file`) answered by the front in its MCP call path
/// (mcp_call.rs) instead of core's loopback server. Off by default.
pub fn inapp_enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("NOEVIA_RUST_INAPP_READS").as_deref() == Ok("1"))
}

/// core's in-app server's JSON-RPC `result` for `name` called as `uid` in project `pid` (the
/// capability token's claims), or `None` to leave the call to core's server: another tool, an account
/// core's `runAs` would refuse or that is disabled, a project read that fails, or an answer the port
/// does not word as core does ([`server_projects::file_read::inapp_result`]).
pub fn inapp_answer(
    data_dir: &Path,
    uid: &str,
    pid: Option<&str>,
    name: &str,
    arguments: &JValue,
    active: impl FnOnce(&str) -> bool,
) -> Option<JValue> {
    if !matches!(name, "project_list_files" | "project_read_file") || uid.is_empty() || !active(uid)
    {
        return None;
    }
    let (dir, project) = match pid {
        Some(id) => {
            let ctx = server_projects::Ctx {
                data_dir,
                user_id: uid,
                switch: server_store::RustProjects::owned(),
                now_ms: crate::identity::now_ms(),
            };
            server_projects::originals::project(ctx, id).ok()?
        }
        None => (server_projects::store::user_dir(data_dir, uid).ok()?, None),
    };
    if !dir.is_dir() {
        return None;
    }
    server_projects::file_read::inapp_result(&dir, project.as_ref(), name, arguments)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    const USER: &str = "0000aaaa-1111-2222-3333-444455556666";

    fn yes(_: &str) -> bool {
        true
    }

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
        let (text, failed) = answer(&data, USER, &run(r#"{"name":"QA-notes.md"}"#), yes).unwrap();
        assert_eq!(text, "File \"QA-notes.md\" (13 chars):\n\nQA notes text");
        assert!(!failed);
        let (text, failed) = answer(&data, USER, &run(r#"{"name":"nope.md"}"#), yes).unwrap();
        assert!(failed && text.starts_with("ERROR: no project file named \"nope.md\""));
        // Core's answers: another tool, not offered, routed to MCP, a grant, bad JSON, not an object,
        // an unknown project, a missing account.
        let mut other = run("{}");
        other.name = "get_current_time".into();
        assert_eq!(answer(&data, USER, &other, yes), None);
        let mut off = run("{}");
        off.allowed = vec!["get_current_time".into()];
        assert_eq!(answer(&data, USER, &off, yes), None);
        let mut routed = run("{}");
        routed.routes = vec![(NAME.into(), "qa-server".into())];
        assert_eq!(answer(&data, USER, &routed, yes), None);
        let mut granted = run("{}");
        granted.grant = Some(chat_turn::host::GrantRun {
            handle: "a".repeat(32),
            chat: "qa-chat".into(),
            scope: "qa".into(),
            binding: JValue::Null,
        });
        assert_eq!(answer(&data, USER, &granted, yes), None);
        assert_eq!(answer(&data, USER, &run("{"), yes), None);
        assert_eq!(answer(&data, USER, &run("[1]"), yes), None);
        let mut gone = run("{}");
        gone.project_id = Some("qa-other".into());
        assert_eq!(answer(&data, USER, &gone, yes), None);
        assert_eq!(
            answer(
                &data,
                "0000aaaa-1111-2222-3333-444455550000",
                &run("{}"),
                yes
            ),
            None
        );
        let mut long = run("{}");
        long.exchange_key = "k".repeat(201);
        assert_eq!(answer(&data, USER, &long, yes), None);
        // A disabled or removed account, an approved edit target, the bridge's other caps.
        assert_eq!(
            answer(&data, USER, &run(r#"{"name":"QA-notes.md"}"#), |_| false),
            None
        );
        let mut edit = run(r#"{"name":"QA-notes.md"}"#);
        edit.edit_target = Some("a".repeat(64));
        assert_eq!(answer(&data, USER, &edit, yes), None);
        let mut chat = run(r#"{"name":"QA-notes.md"}"#);
        chat.chat_key = Some("c".repeat(400));
        assert!(answer(&data, USER, &chat, yes).is_some());
        chat.chat_key = Some("c".repeat(401));
        assert_eq!(answer(&data, USER, &chat, yes), None);
        let mut names = run(r#"{"name":"QA-notes.md"}"#);
        names.allowed.push("t".repeat(201));
        assert_eq!(answer(&data, USER, &names, yes), None);
        let mut many = run(r#"{"name":"QA-notes.md"}"#);
        many.routes = (0..2001).map(|i| (format!("t{i}"), "s".into())).collect();
        assert_eq!(answer(&data, USER, &many, yes), None);
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn the_inapp_reads_answer_as_cores_server_and_leave_the_rest() {
        let data = data_dir("inapp");
        let text = |v: &JValue| match v.get("content") {
            JValue::Arr(items) => items
                .first()
                .and_then(|c| c.get("text").as_str())
                .map(str::to_string),
            _ => None,
        };
        let list = inapp_answer(
            &data,
            USER,
            Some("qa-proj"),
            "project_list_files",
            &JValue::Null,
            yes,
        )
        .unwrap();
        assert_eq!(
            text(&list).unwrap(),
            "Pass a name exactly as shown (or just its file name, when unique) to project_read_file.\nQA-notes.md — text, 13 chars"
        );
        assert!(list.get("isError").is_nullish());
        let read = inapp_answer(
            &data,
            USER,
            Some("qa-proj"),
            "project_read_file",
            &js_json::wtf16::parse(r#"{"name":"QA-notes.md"}"#).unwrap(),
            yes,
        )
        .unwrap();
        assert_eq!(
            text(&read).unwrap(),
            "File \"QA-notes.md\" (13 chars):\n\nQA notes text"
        );
        let none =
            inapp_answer(&data, USER, None, "project_read_file", &JValue::Null, yes).unwrap();
        assert_eq!(
            text(&none).unwrap(),
            "ERROR: this chat is not in a project, so there are no project files"
        );
        assert_eq!(none.get("isError"), &JValue::Bool(true));
        let gone = inapp_answer(
            &data,
            USER,
            Some("qa-other"),
            "project_list_files",
            &JValue::Null,
            yes,
        )
        .unwrap();
        assert_eq!(gone.get("isError"), &JValue::Bool(true));
        // Core's: another tool, a disabled account, no user.
        assert_eq!(
            inapp_answer(
                &data,
                USER,
                Some("qa-proj"),
                "project_search",
                &JValue::Null,
                yes
            ),
            None
        );
        assert_eq!(
            inapp_answer(
                &data,
                USER,
                Some("qa-proj"),
                "project_list_files",
                &JValue::Null,
                |_| false
            ),
            None
        );
        assert_eq!(
            inapp_answer(
                &data,
                "",
                Some("qa-proj"),
                "project_list_files",
                &JValue::Null,
                yes
            ),
            None
        );
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn a_call_without_a_project_lists_no_files() {
        let data = data_dir("none");
        let mut r = run(r#"{"name":"QA-notes.md"}"#);
        r.project_id = None;
        let (text, failed) = answer(&data, USER, &r, yes).unwrap();
        assert_eq!(
            text,
            "ERROR: no project file named \"QA-notes.md\". Available: (none attached)"
        );
        assert!(failed);
        let _ = std::fs::remove_dir_all(&data);
    }
}
