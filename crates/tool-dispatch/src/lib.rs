//! noevia-core's `server/toolboxes.cjs` tool executor (`executeToolCall` and `runToolCall`) in Rust:
//! which executor one chat tool call goes to, the argument checks with core's texts, and whether the
//! call's result counts as failed. Nothing here runs a tool, touches the network or reads a store; the
//! host (a native chat turn) runs the executor [`route`] names and gives its text to [`finish`].
//!
//! The order is core's, check by check (`runToolCall`):
//!
//! 1. a name the turn was not offered (`allowed`, a `Set`) is refused: `ERROR: tool "<name>" is not
//!    enabled for this project`;
//! 2. the arguments: `rawArgs ? JSON.parse(rawArgs) : {}` (empty text means no arguments), refused as
//!    `ERROR: tool arguments were not valid JSON: <first 200 UTF-16 units>`; a value that is not a plain
//!    object (`null`, a number, a string, an array) as `ERROR: tool arguments must be a JSON object: …`;
//! 3. the coworker page tool, when the project is a coworker conversation and no MCP route claims the name;
//! 4. a name the offer routed to an MCP server (`routes`, #865) goes to that server, whatever claims it;
//! 5. the offline-encyclopedia tools, then the Drive tools (by name);
//! 6. the two built-ins: `get_current_time` (the `timezone` argument when it is a non-empty string) and
//!    `read_project_file`;
//! 7. a name a discovered MCP box offers, with no route: the MCP executor picks the server (and refuses a
//!    name more than one server offers);
//! 8. otherwise `ERROR: unknown tool "<name>"`.
//!
//! `executeToolCall` then marks the call failed when a check above refused it, when the built-in flagged
//! its own error, or when the text starts with `ERROR: ` or `ERROR from tool: ` (case-sensitive: a
//! result that merely begins with "Error" is not a failure). A write the approval fence refused at its
//! outbound boundary is `ERROR: <fence message>`, failed and not run (so not counted).
//!
//! # Stricter than the JS
//!
//! `JSON.parse` takes numbers beyond the `f64` range (as `Infinity`) and text holding a character of
//! U+10F800..=U+10FFFF; [`js_json::wtf16::parse`] does not (it uses that range for lone surrogates).
//! For such arguments [`route`] answers [`Route::NotPorted`]: the host hands the call to core as before
//! instead of guessing. Lone surrogates themselves are kept, as the JS keeps them.

#![forbid(unsafe_code)]

use js_json::wtf16;
use js_json::JValue;
use std::collections::HashSet;

mod execute;
pub use execute::{counted_by_core, execute, Executors, Ran};

/// `TOOL_RESULT_CAP`: the characters of a file a built-in read returns at most.
pub const TOOL_RESULT_CAP: usize = 8000;
/// The built-in clock.
pub const CURRENT_TIME: &str = "get_current_time";
/// The built-in project file read.
pub const READ_PROJECT_FILE: &str = "read_project_file";
/// `tool-write-fence.cjs`: the message of a write the permission fence refused at its boundary.
pub const FENCE_MESSAGE: &str =
    "The permission changed before the write was sent; tool was not run. Ask again.";
/// The UTF-16 units of the raw arguments a refusal quotes.
const ARGS_QUOTE: usize = 200;
/// The UTF-16 units of an unknown time zone the clock's refusal quotes.
const ZONE_QUOTE: usize = 64;

/// What the host knows about the turn and the account when one call arrives.
#[derive(Debug, Clone, Copy, Default)]
pub struct Offer<'a> {
    /// The tool names the turn was offered (`allowed`, a `Set`); `None` = no check (core's callers that
    /// pass no set). The native bridge always passes one.
    pub allowed: Option<&'a HashSet<String>>,
    /// The offer's routes: tool name to the MCP server whose box won the name (`routes`, a `Map`).
    /// Later pairs replace earlier ones, as `new Map(pairs)` does.
    pub routes: Option<&'a [(String, String)]>,
    /// The coworker page tool's name, when the project is a coworker conversation
    /// (`specialistTools.appliesTo(project)`); `None` when it does not apply or is not configured.
    pub specialist: Option<&'a str>,
    /// The offline-encyclopedia tool names (`kiwixTools.names`); empty when not configured.
    pub kiwix: &'a [String],
    /// The Drive tool names (`driveTools.names`); empty when not configured.
    pub drive: &'a [String],
    /// The names the discovered MCP boxes offer (`mcpTools()`, any server).
    pub mcp: Option<&'a HashSet<String>>,
}

/// Where one call goes. Every executor gets the parsed arguments, a plain object.
#[derive(Debug, Clone, PartialEq)]
pub enum Route {
    /// Refused before any executor: the call's text, failed (and counted, as core counts it).
    Refused(String),
    /// The coworker page tool (`specialistTools.execute(project, args, {user, chatKey})`).
    Specialist(JValue),
    /// An MCP server: `Some(id)` the routed one, `None` = the executor picks among the discovered servers.
    Mcp {
        server: Option<String>,
        args: JValue,
    },
    /// The offline-encyclopedia executor (`kiwixTools.execute(name, args)`).
    Kiwix(JValue),
    /// The Drive executor (`driveTools.execute(user, name, args, {chatKey, exchangeKey})`).
    Drive(JValue),
    /// The clock: the zone asked for, or `None` for the server's own (`Date#toString`).
    CurrentTime { timezone: Option<String> },
    /// The project file read, with its arguments (`name`, `startPage`, `endPage`, `offset`).
    ReadProjectFile(JValue),
    /// The arguments are JSON this port does not read the way `JSON.parse` does (see the crate docs):
    /// hand the call to core unchanged.
    NotPorted,
}

/// How one call ended, as core's bridge answers it (`{text, failed, notRun?}`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub text: String,
    pub failed: bool,
    /// Refused at its outbound write by the approval fence: not run, so not counted.
    pub not_run: bool,
}

/// `runToolCall`'s choice of executor for `name` with `raw_args` (see the crate docs for the order).
pub fn route(name: &str, raw_args: &str, offer: &Offer<'_>) -> Route {
    if let Some(allowed) = offer.allowed {
        if !allowed.contains(name) {
            return Route::Refused(format!(
                "ERROR: tool \"{name}\" is not enabled for this project"
            ));
        }
    }
    let args = if raw_args.is_empty() {
        JValue::Obj(Vec::new())
    } else {
        match wtf16::parse(raw_args) {
            Ok(v) => v,
            Err(wtf16::Refusal::Invalid) => {
                return Route::Refused(format!(
                    "ERROR: tool arguments were not valid JSON: {}",
                    wtf16::slice(raw_args, ARGS_QUOTE)
                ))
            }
            Err(wtf16::Refusal::Unsupported) => return Route::NotPorted,
        }
    };
    if !matches!(args, JValue::Obj(_)) {
        return Route::Refused(format!(
            "ERROR: tool arguments must be a JSON object: {}",
            wtf16::slice(raw_args, ARGS_QUOTE)
        ));
    }
    let routed = route_of(offer.routes, name);
    if offer.specialist == Some(name) && routed.is_none() {
        return Route::Specialist(args);
    }
    if let Some(Some(server)) = routed {
        return Route::Mcp {
            server: Some(server.to_owned()),
            args,
        };
    }
    if offer.kiwix.iter().any(|n| n == name) {
        return Route::Kiwix(args);
    }
    if offer.drive.iter().any(|n| n == name) {
        return Route::Drive(args);
    }
    if name == CURRENT_TIME {
        let timezone = match args.get("timezone") {
            JValue::Str(s) if !s.is_empty() => Some(s.clone()),
            _ => None,
        };
        return Route::CurrentTime { timezone };
    }
    if name == READ_PROJECT_FILE {
        return Route::ReadProjectFile(args);
    }
    if offer.mcp.is_some_and(|m| m.contains(name)) {
        return Route::Mcp { server: None, args };
    }
    Route::Refused(format!("ERROR: unknown tool \"{name}\""))
}

/// The route for `name` in `routes`, as `routes.get(name)` then `typeof route === 'string' && route`:
/// `None` = the map does not hold the name, `Some(None)` = it holds an empty server id (no route, but the
/// name still counts as held for the coworker check), `Some(Some(id))` = routed.
fn route_of<'a>(routes: Option<&'a [(String, String)]>, name: &str) -> Option<Option<&'a str>> {
    let (_, server) = routes?.iter().rev().find(|(n, _)| n == name)?;
    Some(if server.is_empty() {
        None
    } else {
        Some(server.as_str())
    })
}

/// `DELEGATED_FAILURE` (`/^ERROR(?: from tool)?: /`): the prefixes delegated executors fail with.
pub fn delegated_failure(text: &str) -> bool {
    text.starts_with("ERROR: ") || text.starts_with("ERROR from tool: ")
}

/// `executeToolCall`'s outcome for an executor's `text`: `flagged` is true when the executor (or a
/// refusal of [`route`]) marked the call failed itself.
pub fn finish(text: String, flagged: bool) -> Outcome {
    let failed = flagged || delegated_failure(&text);
    Outcome {
        text,
        failed,
        not_run: false,
    }
}

/// The outcome of a [`Route::Refused`] call.
pub fn refused(text: String) -> Outcome {
    finish(text, true)
}

/// The outcome of a write the approval fence refused at its outbound boundary (`tool-write-fence.cjs`
/// `send`): `ERROR: <message>`, failed, not run.
pub fn fence_refused() -> Outcome {
    Outcome {
        text: format!("ERROR: {FENCE_MESSAGE}"),
        failed: true,
        not_run: true,
    }
}

/// The clock's refusal of a zone it does not know (flagged failed by the built-in).
pub fn unknown_zone(timezone: &str) -> String {
    format!(
        "ERROR: unknown IANA timezone \"{}\"",
        wtf16::slice(timezone, ZONE_QUOTE)
    )
}

/// `JSON.stringify(args)` for an executor that sends the arguments on (the MCP call body, the marker of
/// the recorded oracle): key order and lone surrogates as the JS writes them.
pub fn args_json(args: &JValue) -> String {
    wtf16::stringify(args).unwrap_or_else(|| "{}".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(names: &[&str]) -> HashSet<String> {
        names.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn refuses_names_not_offered_before_reading_arguments() {
        let allowed = set(&["get_current_time"]);
        let offer = Offer {
            allowed: Some(&allowed),
            ..Offer::default()
        };
        assert_eq!(
            route("nc_x", "not json", &offer),
            Route::Refused("ERROR: tool \"nc_x\" is not enabled for this project".into())
        );
    }

    #[test]
    fn empty_arguments_are_an_empty_object() {
        assert_eq!(
            route(CURRENT_TIME, "", &Offer::default()),
            Route::CurrentTime { timezone: None }
        );
    }

    #[test]
    fn non_objects_are_refused_with_the_raw_text() {
        for raw in ["null", "42", "\"x\"", "[]", "true"] {
            assert_eq!(
                route(CURRENT_TIME, raw, &Offer::default()),
                Route::Refused(format!(
                    "ERROR: tool arguments must be a JSON object: {raw}"
                ))
            );
        }
    }

    #[test]
    fn reserved_characters_are_not_guessed() {
        // JSON.parse keeps U+10F800 as a character; this port reserves that range for lone surrogates.
        assert_eq!(
            route(CURRENT_TIME, "{\"q\":\"\u{10F800}\"}", &Offer::default()),
            Route::NotPorted
        );
        // Lone surrogate escapes are kept, as the JS keeps them.
        assert_eq!(
            route(
                CURRENT_TIME,
                "{\"timezone\":\"\\ud83d\"}",
                &Offer::default()
            ),
            Route::CurrentTime {
                timezone: js_json::wtf16::lone_char(0xD83D).map(String::from)
            }
        );
    }

    #[test]
    fn out_of_range_numbers_are_not_guessed() {
        assert_eq!(
            route(CURRENT_TIME, "{\"a\":1e400}", &Offer::default()),
            Route::NotPorted
        );
    }

    #[test]
    fn a_route_wins_over_every_local_executor() {
        let routes = vec![("get_current_time".to_owned(), "srv".to_owned())];
        let kiwix = vec!["get_current_time".to_owned()];
        let offer = Offer {
            routes: Some(&routes),
            specialist: Some("get_current_time"),
            kiwix: &kiwix,
            ..Offer::default()
        };
        assert_eq!(
            route(CURRENT_TIME, "{}", &offer),
            Route::Mcp {
                server: Some("srv".into()),
                args: JValue::Obj(Vec::new())
            }
        );
    }

    #[test]
    fn an_empty_route_blocks_the_coworker_tool_but_routes_nowhere() {
        let routes = vec![("page".to_owned(), String::new())];
        let mcp = set(&["page"]);
        let offer = Offer {
            routes: Some(&routes),
            specialist: Some("page"),
            mcp: Some(&mcp),
            ..Offer::default()
        };
        assert_eq!(
            route("page", "{}", &offer),
            Route::Mcp {
                server: None,
                args: JValue::Obj(Vec::new())
            }
        );
    }

    #[test]
    fn failure_prefixes_are_exact() {
        assert!(delegated_failure("ERROR: x"));
        assert!(delegated_failure("ERROR from tool: x"));
        assert!(!delegated_failure("Error: x"));
        assert!(!delegated_failure("ERROR:x"));
        assert!(!delegated_failure("ERROR from tool:x"));
        assert!(!finish("Error log: clean".into(), false).failed);
        assert!(finish("fine".into(), true).failed);
    }
}
