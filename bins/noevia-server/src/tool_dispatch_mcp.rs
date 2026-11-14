//! A native turn's routed MCP read, run by the front itself (council p6, behind
//! `NOEVIA_RUST_TOOL_DISPATCH=1` and the front's own MCP executor `NOEVIA_RUST_MCP_CALLS=1`; with either
//! off every call goes to core's tool bridge as before).
//!
//! Today such a call goes front → core's bridge (`/internal/chat/tools/execute`, `executeToolCall`) →
//! core `mcp-wiring.cjs` `executeMcpToolCall` → back to this front's `/v1/mcp/call` (mcp_call.rs), and
//! core reduces the answer. Here the front takes the same steps without the two loopback hops:
//!
//! 1. core's bridge would take the call as sent and run it as the user (tool_dispatch_front.rs: no grant,
//!    no edit target, ids and name within the caps, the account active, the project found);
//! 2. `runToolCall` settles it from the offer alone as a routed MCP call (`routes`, #865):
//!    [`tool_dispatch::decide`] answers `Route::Mcp { server: Some(id) }`;
//! 3. the tool is not one the turn treats as a write (the prepared frame's `writes`, `onceTools`,
//!    `automaticOnlyWrites`, `editTools`): a read runs under no approval fence, so nothing about a
//!    revoked grant or an approved edit target can be skipped here;
//!    3b. and discovery's snapshot does not now hold a server saying the tool writes
//!    (`readOnlyHint: false`): core's call-time `isWriteTool` believes such a server even when the frame
//!    was prepared before it said so, and its gates (`declinedWrites`, the write target, the taint check,
//!    the internal token's `w`) read that call-time answer, so such a call is left to core;
//! 4. `routeMcpTool(name, serverId)` and `MCP_SERVER_BY_ID`: the server's offer of that name is in
//!    discovery's snapshot and the server is configured. Core reads its own copies (`mcpState`, the
//!    generation it last applied, and `MCP_SERVERS` as of its last directory sync), which can lag this
//!    front's, so a call either of them does not find here is left to core and core answers it (its
//!    "unknown tool on MCP server" or "no longer configured" text): nothing is answered here that core
//!    might have run;
//! 5. the server is not one whose call needs what core holds: the Nextcloud pass-through (core holds the
//!    storage credential, `mcpAuthHeaders`) or the in-app server (its capability token carries core's
//!    call-time `isWriteTool`); those stay core's;
//! 6. [`crate::mcp_call::execute`] with core's native payload (`user` and `workspaceUser` the bridge's
//!    `asUser` account, `project`, `write: false`, no pass-through), then core's
//!    `reduceToolResult(text, { maxChars: resultCap })` when the answer asks for it, then
//!    `executeToolCall`'s failure rule ([`tool_dispatch::finish`]). Core still counts the call (its bridge
//!    does, `recordToolUse`): chat_native.rs asks core's `tool-used` op.
//!
//! Named differences: a server or offer core's lagging copy no longer holds but this front's does is run
//! here, where core would answer "unknown tool" or "no longer configured" (core asks the front for a newer
//! generation at most every few seconds); a call the turn's client leaves during answers core's stopped
//! text at once, as core's native path does, and its session is still closed by its own task.
use crate::mcp_call::Request;
use crate::mcp_discovery_actor::{ServerSpec, Snapshot};
use crate::project_file_read::bridge_takes;
use crate::tool_dispatch_front::{name_taken, settled};
use chat_turn::host::ToolRun;
use std::collections::HashSet;
use tool_dispatch::{finish, Decision, Outcome, Route};

/// Core's `resultCap` for MCP results (toolboxes.cjs `TOOL_RESULT_CAP`, given to mcp-wiring.cjs).
pub const RESULT_CAP: usize = 8000;

/// Server modes whose call stays core's (step 5 of the module docs).
const CORE_AUTH: [&str; 2] = ["nextcloud", "internal"];

/// What the front knows when it plans one call.
pub struct Facts<'a> {
    /// Whether discovery's snapshot holds `server`'s offer of `name` (core `mcpState.offers`).
    pub offered: &'a dyn Fn(&str, &str) -> bool,
    /// The configured servers, the operator's then the directory's (core `MCP_SERVERS`).
    pub servers: &'a [ServerSpec],
    /// The tools the turn treats as writes (step 3).
    pub writes: &'a HashSet<String>,
    /// Whether discovery now has a server saying `name` writes (`readOnlyHint: false`), step 3b.
    pub server_says_write: &'a dyn Fn(&str) -> bool,
}

/// One call's plan.
#[derive(Debug, Clone, PartialEq)]
pub enum Plan {
    /// Core's bridge answers it, as before.
    Core,
    /// Run here through [`crate::mcp_call::execute`].
    Call(Request),
}

/// The plan for `run`, made for `user` (see the module docs for the order). Nothing here waits or reads
/// a store: the account and project checks ([`crate::project_file_read::bridge_scope`]) come after.
pub fn plan(user: &str, run: &ToolRun, facts: &Facts<'_>) -> Plan {
    if user.is_empty() || !bridge_takes(run) || !name_taken(&run.name) {
        return Plan::Core;
    }
    let Decision::Route(Route::Mcp {
        server: Some(server),
        args,
    }) = settled(run)
    else {
        return Plan::Core;
    };
    if facts.writes.contains(&run.name) || (facts.server_says_write)(&run.name) {
        return Plan::Core;
    }
    if !(facts.offered)(&server, &run.name) {
        return Plan::Core;
    }
    // `MCP_SERVER_BY_ID`: of two servers with one id the later wins.
    let Some(spec) = facts.servers.iter().rev().find(|s| s.id == server) else {
        return Plan::Core;
    };
    if CORE_AUTH.contains(&spec.auth.as_str()) {
        return Plan::Core;
    }
    Plan::Call(Request {
        user: user.to_string(),
        workspace_user: Some(user.to_string()),
        server,
        name: run.name.clone(),
        arguments: args,
        project: run.project_id.clone(),
        edit_target: None,
        write: false,
        nextcloud: None,
    })
}

/// Whether `snapshot` holds `server`'s offer of `name`.
pub fn snapshot_offers(snapshot: &Snapshot, server: &str, name: &str) -> bool {
    snapshot
        .offers
        .iter()
        .any(|e| e.server == server && e.name == name)
}

/// Whether any offer of `name` in `snapshot` says it writes (`readOnlyHint: false`), step 3b. Core's
/// call-time `isWriteTool` reads only the first server's entry (`mcpState.tools`, noevia#2742); any
/// offer is stricter and can only leave more calls to core.
pub fn snapshot_says_write(snapshot: &Snapshot, name: &str) -> bool {
    snapshot
        .offers
        .iter()
        .any(|e| e.name == name && e.read_only == Some(false))
}

/// The tools the prepared turn treats as writes (step 3 of the module docs).
pub fn writes_of(p: &chat_turn::prepared::Prepared) -> HashSet<String> {
    p.writes
        .iter()
        .chain(&p.once_tools)
        .chain(&p.automatic_only_writes)
        .chain(&p.edit_tools)
        .cloned()
        .collect()
}

/// How the call ends, from what the executor answered: core's native path reduces a successful result
/// (`reduce`) and `executeToolCall` marks it failed by its prefix.
pub fn outcome(answer: crate::mcp_call::Answer) -> Outcome {
    outcome_with(answer, |text, cap| {
        chat_turn::reduce::reduce_tool_result(text, cap).text
    })
}

/// [`outcome`] with `reduce(text, maxChars)` standing for core's `reduceToolResult(...).text` (the MCP
/// call oracle records core's executor with a marker in its place).
pub fn outcome_with(
    answer: crate::mcp_call::Answer,
    reduce: impl FnOnce(&str, usize) -> String,
) -> Outcome {
    let text = if answer.reduce {
        reduce(&answer.text, RESULT_CAP)
    } else {
        answer.text
    };
    finish(text, false)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;
    use crate::mcp_call::Answer;
    use js_json::JValue;

    const USER: &str = "0000aaaa-1111-2222-3333-444455556666";

    fn spec(id: &str, auth: &str) -> ServerSpec {
        ServerSpec {
            id: id.into(),
            url: format!("https://{id}.example.test/mcp"),
            auth: auth.into(),
            directory: auth == "directory",
            title: None,
            added_by: None,
            token_env: None,
        }
    }

    fn servers() -> Vec<ServerSpec> {
        vec![
            spec("qa-open", "none"),
            spec("qa-bearer", "bearer"),
            spec("qa-nc", "nextcloud"),
            spec("qa-inapp", "internal"),
            spec("qa-dir", "directory"),
            spec("qa-own", "personal"),
            spec("qa-signin", "oauth"),
        ]
    }

    fn run(name: &str, server: &str, args: &str) -> ToolRun {
        ToolRun {
            project_id: Some("qa-project".into()),
            name: name.into(),
            args: args.into(),
            allowed: vec![name.into(), "qa_other".into()],
            routes: vec![(name.into(), server.into())],
            chat_key: Some("qa-chat".into()),
            exchange_key: "qa-exchange".into(),
            edit_target: None,
            grant: None,
        }
    }

    fn with<T>(writes: &[&str], f: impl FnOnce(&Facts<'_>) -> T) -> T {
        let offered = |server: &str, name: &str| server != "qa-gone-offer" && name != "qa_unlisted";
        let servers = servers();
        let writes = writes.iter().map(|w| w.to_string()).collect();
        let says = |name: &str| name == "qa_now_writes";
        f(&Facts {
            offered: &offered,
            servers: &servers,
            writes: &writes,
            server_says_write: &says,
        })
    }

    #[test]
    fn a_routed_read_on_a_server_the_front_calls_runs_here_with_cores_payload() {
        for server in ["qa-open", "qa-bearer", "qa-dir", "qa-own", "qa-signin"] {
            let p = with(&[], |f| plan(USER, &run("qa_lookup", server, r#"{"q":"qa"}"#), f));
            let Plan::Call(r) = p.clone() else {
                panic!("{server}: {p:?}")
            };
            assert_eq!(r.server, server);
            assert_eq!(r.name, "qa_lookup");
            assert_eq!((r.user.as_str(), r.workspace_user.as_deref()), (USER, Some(USER)));
            assert_eq!(r.project.as_deref(), Some("qa-project"));
            assert_eq!(r.edit_target, None);
            assert!(!r.write);
            assert_eq!(r.nextcloud, None);
            assert_eq!(r.arguments, JValue::obj([("q", JValue::Str("qa".into()))]));
        }
        // Empty arguments are `{}`, as runToolCall reads them.
        let Plan::Call(r) = with(&[], |f| plan(USER, &run("qa_lookup", "qa-open", ""), f)) else {
            panic!()
        };
        assert_eq!(r.arguments, JValue::Obj(vec![]));
    }

    #[test]
    fn a_server_core_holds_the_credential_or_the_write_flag_of_stays_cores() {
        for server in ["qa-nc", "qa-inapp"] {
            assert_eq!(
                with(&[], |f| plan(USER, &run("qa_lookup", server, "{}"), f)),
                Plan::Core,
                "{server}"
            );
        }
    }

    #[test]
    fn a_write_of_any_kind_stays_cores() {
        assert_eq!(
            with(&["qa_lookup"], |f| plan(USER, &run("qa_lookup", "qa-open", "{}"), f)),
            Plan::Core
        );
    }

    #[test]
    fn a_tool_a_server_now_says_writes_stays_cores_though_the_frame_called_it_a_read() {
        assert_eq!(
            with(&[], |f| plan(USER, &run("qa_now_writes", "qa-open", "{}"), f)),
            Plan::Core
        );
    }

    #[test]
    fn snapshot_says_write_reads_every_offer_of_the_name() {
        let entry = |server: &str, name: &str, ro: Option<bool>| crate::mcp_discovery_actor::Entry {
            name: name.into(),
            server: server.into(),
            tool: JValue::Null,
            read_only: ro,
        };
        let snap = Snapshot {
            offers: vec![
                entry("qa-a", "shared", Some(true)),
                entry("qa-b", "shared", Some(false)),
                entry("qa-a", "silent", None),
                entry("qa-a", "read", Some(true)),
            ],
            ..Snapshot::default()
        };
        // Core reads only qa-a's entry for "shared" (a read); the second server's "writes" counts here.
        assert!(snapshot_says_write(&snap, "shared"));
        // No hint is no claim: the manifest decides (the frame's writes).
        assert!(!snapshot_says_write(&snap, "silent"));
        assert!(!snapshot_says_write(&snap, "read"));
        assert!(!snapshot_says_write(&snap, "absent"));
    }

    #[test]
    fn an_offer_or_server_this_front_does_not_hold_is_left_to_core() {
        // Core answers these from its own copies ('ERROR: unknown tool "search_files" on MCP server
        // "gone"', mcp-tool-routing-865.test.cjs; "no longer configured"), which may still hold them.
        for r in [
            run("qa_unlisted", "qa-open", "{}"),
            run("qa_lookup", "qa-gone-offer", "{}"),
            run("qa_lookup", "qa-removed", "{}"),
        ] {
            assert_eq!(with(&[], |f| plan(USER, &r, f)), Plan::Core, "{r:?}");
        }
    }

    #[test]
    fn anything_but_a_routed_mcp_call_the_bridge_takes_stays_cores() {
        let cases = [
            // Not offered, not an object: tool_dispatch_front's refusals, not this path.
            {
                let mut r = run("qa_lookup", "qa-open", "{}");
                r.allowed = vec!["qa_other".into()];
                r
            },
            run("qa_lookup", "qa-open", "[1]"),
            run("qa_lookup", "qa-open", "{nope"),
            // No route (the executor would pick the server among the discovered ones).
            {
                let mut r = run("qa_lookup", "qa-open", "{}");
                r.routes = vec![];
                r
            },
            // A route with no server.
            run("qa_lookup", "", "{}"),
            // A grant or an edit target.
            {
                let mut r = run("qa_lookup", "qa-open", "{}");
                r.edit_target = Some("a".repeat(64));
                r
            },
            // A name core's bridge answers 400 for.
            run(&"q".repeat(201), "qa-open", "{}"),
            // Arguments this port does not read as JSON.parse does.
            run("qa_lookup", "qa-open", r#"{"n":1e400}"#),
        ];
        for r in cases {
            assert_eq!(with(&[], |f| plan(USER, &r, f)), Plan::Core, "{r:?}");
        }
        // No account.
        assert_eq!(
            with(&[], |f| plan("", &run("qa_lookup", "qa-open", "{}"), f)),
            Plan::Core
        );
    }

    #[test]
    fn the_answer_is_reduced_when_it_asks_and_failed_by_its_prefix() {
        let long = format!("[{}]", vec![r#"{"id":1,"path":"/qa"}"#; 600].join(","));
        let o = outcome(Answer {
            text: long.clone(),
            reduce: true,
        });
        assert_eq!(o.text, chat_turn::reduce::reduce_tool_result(&long, 8000).text);
        assert!(o.text.starts_with("[600 records, tab-separated"));
        assert!(!o.failed);
        // An error text is not reduced and is failed.
        let o = outcome(Answer {
            text: format!("ERROR calling qa_lookup: {}", "x".repeat(9000)),
            reduce: false,
        });
        assert_eq!(js_json::wtf16::len(&o.text), 25 + 9000);
        assert!(o.failed);
        // A text that merely starts with "Error" is not failed.
        let o = outcome(Answer {
            text: "Error budget: fine".into(),
            reduce: true,
        });
        assert_eq!((o.text.as_str(), o.failed), ("Error budget: fine", false));
    }

    #[test]
    fn snapshot_offers_is_by_server_and_name() {
        let entry = |server: &str, name: &str| crate::mcp_discovery_actor::Entry {
            name: name.into(),
            server: server.into(),
            tool: JValue::Null,
            read_only: Some(true),
        };
        let snap = Snapshot {
            offers: vec![entry("qa-a", "one"), entry("qa-b", "two")],
            ..Snapshot::default()
        };
        assert!(snapshot_offers(&snap, "qa-a", "one"));
        assert!(!snapshot_offers(&snap, "qa-a", "two"));
        assert!(!snapshot_offers(&snap, "qa-c", "one"));
    }
}
