//! The in-app MCP project write tools in the front (track R14 step 6): `project_create_file`,
//! `project_append_file` and `project_replace_text` as core's in-app server answers them
//! (edit_target_lookup::inapp, recorded against core's own handler), their write made by
//! server_projects::text_write as the account the call's token names (text_write_op.rs `write_as`).
//! Behind `NOEVIA_RUST_INAPP_WRITES=1` (read once; unset = core's in-app server, as today).
//!
//! [`answer`] gives `None` to leave the call to core's server: another tool, a write the turn did
//! not approve (core's server refuses it itself), a workspace or argument shape the ports leave to
//! core, or a front that cannot run the write now. It is called from the front's MCP call path for
//! the in-app server only (mcp_call.rs, beside track R13's in-app reads), with the token's account,
//! project and edit pin.
//!
//! An account that is gone answers `ERROR: that account no longer exists`, as core's `runAs` does.
//! Deliberately stricter than core (track R14 D7): a **disabled** account (or one with a role core
//! does not know) is refused the same way, where core's `runAs` (`listUsers`, every row) still
//! writes as it. getProject's own save (the #352 heal, a changed skills reconcile) is not made
//! here: it changes nothing a write tool reads (workspace_view).
//!
//! [`answer`] is blocking. Every `None` comes before the write starts; once [`answer`] has begun
//! the write it always answers. A caller that runs it on the blocking pool must therefore map a
//! failure of that task (a panic) to an error result, never to `None` (core would write again).
//! Unlike core's in-app listener (120 s per request), the write has no deadline of its own beyond
//! the project's source lock wait and the storage client's timeouts.
use std::sync::{Arc, OnceLock};

use edit_target_lookup::inapp::{self, Prepared};
use js_json::JValue;
use server_projects::text_write::Edit;
use server_projects::workspace_view;

use crate::App;

pub const SWITCH: &str = "NOEVIA_RUST_INAPP_WRITES";

/// Whether the switch is on (`1`), read once.
pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var(SWITCH).as_deref() == Ok("1"))
}

/// The write's options for text_write (an option core's caller left out is `undefined`).
fn edit_of(w: &inapp::Write) -> Edit {
    match &w.options {
        None => Edit::default(),
        Some(o) => Edit {
            expect_name: JValue::Str(o.expect_name.clone()),
            expect_content: JValue::Str(o.expect_content.clone()),
            expect_attachment: o.expect_attachment.clone(),
            adopt_to: o.adopt_to.clone().map_or(JValue::Undefined, JValue::Str),
            adopt_account: o
                .adopt_account
                .clone()
                .map_or(JValue::Undefined, JValue::Str),
        },
    }
}

/// One in-app write tool call (blocking): the server's result, or `None` (core's server answers).
pub fn answer(
    app: &Arc<App>,
    uid: &str,
    pid: Option<&str>,
    name: &str,
    arguments: &JValue,
    target: Option<&str>,
    write: bool,
) -> Option<JValue> {
    if !enabled() || !write || !inapp::TOOLS.contains(&name) {
        return None;
    }
    // Without the storage client a write could not reach the account's storage; without the
    // extraction client it cannot run at all.
    if cfg!(not(feature = "rust-storage-browse")) || app.extraction.is_none() {
        return None;
    }
    let data_dir = app.config.data_dir.clone()?;
    if server_projects::store::user_dir(&data_dir, uid).is_err() {
        return None;
    }
    let role = match crate::text_write_op::account_role(app, uid) {
        Ok(Some(role)) => role,
        Ok(None) => return Some(inapp::error_result("that account no longer exists")),
        Err(()) => return None,
    };
    let default = std::env::var("DEFAULT_PROVIDER_ID")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "default".to_owned());
    let view = workspace_view::load(&data_dir, uid, &default).ok()?;
    let project = match pid {
        None => None,
        Some(id) => view.project(id).ok()?.map(|p| p.project),
    };
    let account = crate::edit_target_native::storage_account(app, uid).ok()?;
    let pid_text = project
        .as_ref()
        .and_then(|p| p.get("id").as_str())
        .unwrap_or_default()
        .to_owned();
    let unreadable = std::cell::Cell::new(false);
    let content = |file: &JValue| match view.text(&pid_text, file) {
        Some(t) => t,
        None => {
            unreadable.set(true);
            String::new()
        }
    };
    let prepared = inapp::prepare(
        name,
        arguments,
        project.as_ref(),
        account.as_deref(),
        target,
        &content,
    );
    if unreadable.get() {
        return None;
    }
    match prepared {
        Prepared::Unsupported => None,
        Prepared::Answer(result) => Some(result),
        Prepared::Write(w) => {
            let id = pid?;
            let written = crate::text_write_op::write_as(
                app,
                &data_dir,
                server_store::RustProjects::owned(),
                uid,
                role,
                id,
                &w.name,
                &w.text,
                &edit_of(&w),
            )?;
            Some(inapp::finish(&w.success, written.map(|_| ())))
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn a_tools_options_reach_the_write_as_core_passes_them() {
        let create = inapp::Write {
            name: "a.md".into(),
            text: "x".into(),
            options: None,
            success: String::new(),
        };
        let e = edit_of(&create);
        assert!(
            matches!(e.expect_name, JValue::Undefined) && matches!(e.adopt_to, JValue::Undefined)
        );
        let edit = inapp::Write {
            name: "a.md".into(),
            text: "x".into(),
            options: Some(inapp::WriteOptions {
                expect_name: "a.md".into(),
                expect_content: String::new(),
                expect_attachment: JValue::Null,
                adopt_to: Some("F/Text/a.md".into()),
                adopt_account: Some("acct".into()),
            }),
            success: String::new(),
        };
        let e = edit_of(&edit);
        assert_eq!(e.expect_content, JValue::Str(String::new()));
        assert_eq!(e.expect_attachment, JValue::Null);
        assert_eq!(e.adopt_to, JValue::Str("F/Text/a.md".into()));
        assert_eq!(e.adopt_account, JValue::Str("acct".into()));
    }
}
