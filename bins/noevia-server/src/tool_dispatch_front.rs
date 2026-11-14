//! The native turn's tool executor in the front (`crates/tool-dispatch`, core's `toolboxes.cjs`
//! `runToolCall` order), behind `NOEVIA_RUST_TOOL_DISPATCH=1` (off by default: unset, every call goes to
//! core's tool bridge, `/internal/chat/tools/execute`, as before).
//!
//! The front knows the turn's offer on every call (`allowed`, `routes`) but not the account's
//! encyclopedia, Drive and discovered MCP names, nor whether the project is a coworker conversation, so
//! it answers only what [`tool_dispatch::decide`] settles from the offer alone and that runs no executor:
//! core's three refusals before any executor (a name the turn was not offered, arguments that are not
//! JSON, arguments that are not an object). Every other call, including the routed MCP calls the offer
//! settles (their executor still needs core's write check, Nextcloud pass-through and result reducer),
//! goes to core's bridge as before.
//!
//! The call is answered here only when core's bridge would take it as sent and run it as the user
//! ([`project_file_read::bridge_takes`] and [`project_file_read::bridge_scope`]: no grant, no edit
//! target, ids within the caps, the account active and the project found). Core keeps counting it:
//! chat_native.rs asks core's native op `tool-used`, because core's bridge counts a refused call too
//! (noevia#2668, kept for parity).
use crate::project_file_read::{bridge_scope, bridge_takes};
use chat_turn::host::ToolRun;
use std::collections::HashSet;
use std::path::Path;
use std::sync::OnceLock;
use tool_dispatch::{decide, Decision, Knowledge, Route};

/// The switch, read once.
pub const SWITCH: &str = "NOEVIA_RUST_TOOL_DISPATCH";

/// The coworker page tool (core `specialist-page-save.cjs` `NAME`): its applicability is core's.
pub const PAGE_TOOL: &str = "space_page_save";

/// `NOEVIA_RUST_TOOL_DISPATCH=1`, read once.
pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| switch_on(std::env::var(SWITCH).ok().as_deref()))
}

fn switch_on(value: Option<&str>) -> bool {
    value == Some("1")
}

/// What the offer alone settles for this call, before the bridge checks.
pub fn settled(run: &ToolRun) -> Decision {
    let allowed: HashSet<String> = run.allowed.iter().cloned().collect();
    decide(
        &run.name,
        &run.args,
        &Knowledge::offer_only(&allowed, &run.routes, PAGE_TOOL),
    )
}

/// The refusal text (failed) when the front answers this call as core's bridge would; `None` leaves it
/// to core. `active(user)` is core's account check ([`crate::project_file_read::account_active`]).
pub fn answer(
    data_dir: &Path,
    user: &str,
    run: &ToolRun,
    active: impl FnOnce(&str) -> bool,
) -> Option<(String, bool)> {
    if !bridge_takes(run) {
        return None;
    }
    let Decision::Route(Route::Refused(text)) = settled(run) else {
        return None;
    };
    bridge_scope(data_dir, user, run, active)?;
    Some((text, true))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;

    const USER: &str = "0000aaaa-1111-2222-3333-444455556666";

    fn run(name: &str, args: &str) -> ToolRun {
        ToolRun {
            project_id: None,
            name: name.into(),
            args: args.into(),
            allowed: vec![
                "qa_lookup".into(),
                PAGE_TOOL.into(),
                "get_current_time".into(),
            ],
            routes: vec![("qa_lookup".into(), "qa-server".into())],
            chat_key: Some("qa-chat".into()),
            exchange_key: "qa-exchange".into(),
            edit_target: None,
            grant: None,
        }
    }

    fn account(dir: &Path) {
        std::fs::create_dir_all(server_projects::store::user_dir(dir, USER).unwrap()).unwrap();
    }

    #[test]
    fn only_the_value_1_turns_it_on() {
        assert!(switch_on(Some("1")));
        for v in [
            None,
            Some(""),
            Some("0"),
            Some("true"),
            Some("on"),
            Some(" 1"),
        ] {
            assert!(!switch_on(v), "{v:?}");
        }
    }

    #[test]
    fn the_three_refusals_are_answered_with_cores_texts() {
        let dir = tempfile::tempdir().unwrap();
        account(dir.path());
        let yes = |_: &str| true;
        let cases = [
            (
                run("qa_unoffered", "{}"),
                "ERROR: tool \"qa_unoffered\" is not enabled for this project",
            ),
            (
                run("get_current_time", "{nope"),
                "ERROR: tool arguments were not valid JSON: {nope",
            ),
            (
                run("get_current_time", "[1,2]"),
                "ERROR: tool arguments must be a JSON object: [1,2]",
            ),
            (
                run("qa_lookup", "null"),
                "ERROR: tool arguments must be a JSON object: null",
            ),
        ];
        for (r, want) in cases {
            assert_eq!(
                answer(dir.path(), USER, &r, yes),
                Some((want.to_string(), true)),
                "{r:?}"
            );
        }
    }

    #[test]
    fn calls_that_run_an_executor_stay_cores() {
        let dir = tempfile::tempdir().unwrap();
        account(dir.path());
        let yes = |_: &str| true;
        for r in [
            run("qa_lookup", "{\"q\":1}"),
            run(PAGE_TOOL, "{}"),
            run("get_current_time", ""),
            // Arguments this port does not read as JSON.parse does.
            run("get_current_time", "{\"n\":1e400}"),
        ] {
            assert_eq!(answer(dir.path(), USER, &r, yes), None, "{r:?}");
        }
    }

    #[test]
    fn a_call_core_would_not_take_or_not_run_as_the_user_stays_cores() {
        let dir = tempfile::tempdir().unwrap();
        account(dir.path());
        let refused = run("qa_unoffered", "{}");
        // An inactive account: core answers 404.
        assert_eq!(answer(dir.path(), USER, &refused, |_| false), None);
        // A project that is not there: core answers 404.
        let mut r = refused.clone();
        r.project_id = Some("qa-missing-project".into());
        assert_eq!(answer(dir.path(), USER, &r, |_| true), None);
        // An edit target or a grant: core's.
        let mut r = refused.clone();
        r.edit_target = Some("a".repeat(64));
        assert_eq!(answer(dir.path(), USER, &r, |_| true), None);
        // An exchange key over the bridge's cap: core's 400.
        let mut r = refused.clone();
        r.exchange_key = "k".repeat(201);
        assert_eq!(answer(dir.path(), USER, &r, |_| true), None);
        // No account folder: core's.
        let empty = tempfile::tempdir().unwrap();
        assert_eq!(answer(empty.path(), USER, &refused, |_| true), None);
    }

    /// Every call core's executor recorded (crates/tool-dispatch's fixture) with an offer as the bridge
    /// sends one: the front answers exactly core's refusals, with core's text, and nothing else.
    #[test]
    fn the_recorded_refusals_are_answered_and_nothing_else() {
        use js_json::{wtf16, JValue};
        let f = wtf16::parse(include_str!(
            "../../../crates/tool-dispatch/tests/fixtures/tool-dispatch.v1.json"
        ))
        .unwrap();
        let JValue::Arr(cases) = f.get("cases") else {
            panic!("no cases")
        };
        let strs = |v: &JValue| match v {
            JValue::Arr(items) => Some(
                items
                    .iter()
                    .map(|x| x.as_str().unwrap().to_owned())
                    .collect::<Vec<_>>(),
            ),
            _ => None,
        };
        let dir = tempfile::tempdir().unwrap();
        account(dir.path());
        let (mut answered, mut left) = (0usize, 0usize);
        for c in cases {
            let (Some(allowed), JValue::Arr(routes)) = (strs(c.get("allowed")), c.get("routes"))
            else {
                continue;
            };
            let routes = routes
                .iter()
                .map(|p| {
                    let p = strs(p).unwrap();
                    (p[0].clone(), p[1].clone())
                })
                .collect();
            let r = ToolRun {
                name: c.get("name").as_str().unwrap().into(),
                args: c.get("rawArgs").as_str().unwrap().into(),
                allowed,
                routes,
                ..run("", "")
            };
            let text = c.get("text").as_str().unwrap();
            let core_refused = c.get("failed").as_bool() == Some(true)
                && (text.starts_with("ERROR: tool \"") && text.ends_with("\" is not enabled for this project")
                    || text.starts_with("ERROR: tool arguments were not valid JSON: ")
                    || text.starts_with("ERROR: tool arguments must be a JSON object: "));
            match answer(dir.path(), USER, &r, |_| true) {
                Some((got, failed)) => {
                    assert_eq!((got.as_str(), failed), (text, true), "{r:?}");
                    assert_eq!(c.get("notRun").as_bool(), Some(false));
                    answered += 1;
                }
                None => {
                    // Left to core: never one of its refusals, except for arguments this port does not
                    // read as JSON.parse does (crates/tool-dispatch: Route::NotPorted).
                    let not_ported = r.args.contains("1e400")
                        || r.args.contains('\u{10F800}')
                        || r.args.matches('[').count() > 100;
                    assert!(
                        !core_refused || not_ported,
                        "core refused, the front did not answer: {r:?} {text:?}"
                    );
                    left += 1;
                }
            }
        }
        assert!(answered > 300, "answered {answered}");
        assert!(left > 300, "left {left}");
    }
}
