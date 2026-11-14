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

/// Whether core's execute bridge would take this call's body as it is and run it with no grant and no edit
/// target (its 400s, a grant's redeem and an approved edit stay core's).
pub(crate) fn bridge_takes(run: &ToolRun) -> bool {
    let listed = |v: &str| wtf16::len(v) <= ID_CAP;
    run.allowed.len() <= LIST_CAP
        && run.allowed.iter().all(|a| listed(a))
        && run.routes.len() <= LIST_CAP
        && run.routes.iter().all(|(a, b)| listed(a) && listed(b))
        && run.grant.is_none()
        && run.edit_target.is_none()
        && body_fits(run)
        && within(&run.exchange_key, ID_CAP)
        && !run
            .chat_key
            .as_deref()
            .is_some_and(|k| !within(k, CHAT_ID_CAP))
        && !run
            .project_id
            .as_deref()
            .is_some_and(|p| !within(p, ID_CAP))
}

/// core's bridge scope for the call: the account active (`asUser`, [`account_active`]), and the call's
/// project found as stored now (`getProject`, with its self-heal saved as core saves it). `None` = core
/// answers it (its 404).
pub(crate) fn bridge_scope(
    data_dir: &Path,
    user: &str,
    run: &ToolRun,
    active: impl FnOnce(&str) -> bool,
) -> Option<(std::path::PathBuf, Option<JValue>)> {
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
    Some((dir, project))
}

/// The tool's text and failure flag when the front answers this call; `None` leaves it to core.
/// `active(user)` is core's account check ([`account_active`]).
pub fn answer(
    data_dir: &Path,
    user: &str,
    run: &ToolRun,
    active: impl FnOnce(&str) -> bool,
) -> Option<(String, bool)> {
    if run.name != NAME
        || !run.allowed.iter().any(|a| a == NAME)
        || run.routes.iter().any(|(tool, _)| tool == NAME)
        || !bridge_takes(run)
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
    let (dir, project) = bridge_scope(data_dir, user, run, active)?;
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
