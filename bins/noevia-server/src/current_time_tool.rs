//! Track R5: the chat's built-in `get_current_time` tool, answered by the front for a native turn
//! instead of core's tool bridge (`/internal/chat/tools/execute`), behind `NOEVIA_RUST_CURRENT_TIME=1`
//! (off by default: unset, every call goes to core as before).
//!
//! The call is answered here only when core's bridge would run exactly toolboxes.cjs's
//! `get_current_time` branch ([`crate::project_file_read::bridge_scope`]: offered, not routed to an
//! MCP server, no chat-wide grant or edit target, ids within the bridge's caps, arguments a JSON
//! object, the account and project as core's asUser and getProject find them), and only when the
//! answer is one the recording proves equal to Node's ([`current_time::verified`]): the zone it uses
//! (the `timezone` argument, else the server zone) and the instant. Everything else goes to core's
//! bridge as before, among them an unknown zone (core writes the error), a zone whose tz data differs
//! between the two (sbstndalton/noevia#2653) and every call after the recorded span.
//!
//! The recording proves the answers for its own Node only: the front answers nothing until core's
//! native op `runtime` has reported a Node with the recording's ICU and tz data
//! ([`current_time::runtime_recorded`]); an older core (404) or another Node keeps every call core's.
//! The same answer gives the server zone, the one core's process resolved: calls without a
//! `timezone` argument are answered here only when it is one the recording ran under
//! ([`current_time::server_zone_recorded`]), so the front never guesses core's zone from its own
//! environment.
//!
//! Core keeps counting the call: chat_native.rs asks core's native op `tool-used`, as for
//! read_project_file. If that op does not answer (core down or older), the call is answered and its
//! count is lost with a log line, where core's own path would have failed the call.
use chat_turn::host::ToolRun;
use js_json::JValue;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

/// The tool this module answers.
pub const NAME: &str = "get_current_time";

/// `NOEVIA_RUST_CURRENT_TIME=1`, read once.
pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("NOEVIA_RUST_CURRENT_TIME").as_deref() == Ok("1"))
}

/// What core's op `runtime` reported, as far as this module uses it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Runtime {
    /// Core's Node has the recording's ICU and tz data ([`current_time::runtime_recorded`]).
    pub recorded: bool,
    /// Core's resolved zone, when it is one the recording ran under.
    pub server_zone: Option<String>,
}

/// Core's answer, kept once it is a 200 or an older core's 404 (core and the front start and stop
/// together); a gone account's 404 or no answer is asked again next time.
static RUNTIME: Mutex<Option<Runtime>> = Mutex::new(None);

/// Core's Node, if already known.
pub fn runtime_known() -> Option<Runtime> {
    RUNTIME
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// What an answer of core's op `runtime` (`{node, icu, tz, zone}`) says, if it is one to keep.
pub fn runtime_of(asked: Option<(u16, JValue)>) -> Option<Runtime> {
    let body = match asked {
        Some((200, body)) => body,
        // A core without the op: nothing here until it is replaced (with the front).
        Some((404, body)) if body.get("error").as_str() != Some("no such account") => {
            return Some(Runtime {
                recorded: false,
                server_zone: None,
            })
        }
        _ => return None,
    };
    let field = |k: &str| body.get(k).as_str().unwrap_or("").to_string();
    let zone = field("zone");
    Some(Runtime {
        recorded: current_time::runtime_recorded(&field("node"), &field("icu"), &field("tz")),
        server_zone: current_time::server_zone_recorded(&zone).then_some(zone),
    })
}

/// Keeps an answer of core's op `runtime` ([`runtime_of`]); whether core's Node is the recording's.
pub fn runtime_answered(asked: Option<(u16, JValue)>) -> bool {
    let Some(runtime) = runtime_of(asked) else {
        return false;
    };
    let recorded = runtime.recorded;
    *RUNTIME
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(runtime);
    recorded
}

/// The tool's text and failure flag when the front answers this call at `now_ms`; `None` leaves it
/// to core. Nothing is answered before core's Node is known to be the recording's ([`runtime_known`]).
/// `active(user)` is core's account check ([`crate::project_file_read::account_active`]).
pub fn answer(
    data_dir: &Path,
    user: &str,
    run: &ToolRun,
    now_ms: f64,
    active: impl FnOnce(&str) -> bool,
) -> Option<(String, bool)> {
    answer_with(&runtime_known()?, data_dir, user, run, now_ms, active)
}

/// [`answer`] for a known runtime.
fn answer_with(
    runtime: &Runtime,
    data_dir: &Path,
    user: &str,
    run: &ToolRun,
    now_ms: f64,
    active: impl FnOnce(&str) -> bool,
) -> Option<(String, bool)> {
    // The cheap checks first, in the bridge's order (name, body size, arguments): a call left to
    // core reads no store here.
    if !runtime.recorded || run.name != NAME || js_json::wtf16::len(&run.args) > ARGS_CAP {
        return None;
    }
    let args = if run.args.is_empty() {
        JValue::Obj(Vec::new())
    } else {
        js_json::wtf16::parse(&run.args).ok()?
    };
    let answer = time_text(&args, runtime.server_zone.as_deref(), now_ms)?;
    // Then what core's bridge checks (the same arguments parsed again there).
    crate::project_file_read::bridge_scope(data_dir, user, run, NAME, active)?;
    Some(answer)
}

/// core's bridge reads `args` up to 16 Mi UTF-16 units (bridge_scope checks the whole body).
const ARGS_CAP: usize = 16 * 1024 * 1024;

/// The tool's answer for these arguments, when [`current_time::verified`] holds for the zone it uses.
fn time_text(args: &JValue, server_zone: Option<&str>, now_ms: f64) -> Option<(String, bool)> {
    // toolboxes.cjs: `typeof args.timezone === 'string' && args.timezone ? args.timezone : undefined`.
    let zone = match args.get("timezone").as_str().filter(|z| !z.is_empty()) {
        Some(z) => z,
        None => server_zone?,
    };
    if !current_time::verified(zone, now_ms) {
        return None;
    }
    Some(
        match current_time::run(args, now_ms, server_zone.unwrap_or("")) {
            Ok(text) => (text, false),
            Err(text) => (text, true),
        },
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    const USER: &str = "0000aaaa-1111-2222-3333-444455556666";
    /// 2026-11-14T06:05:12Z, a recorded instant.
    const NOW: f64 = 1_794_636_312_000.0;

    fn yes(_: &str) -> bool {
        true
    }

    fn run(args: &str) -> ToolRun {
        ToolRun {
            project_id: Some("qa-proj".into()),
            name: NAME.into(),
            args: args.into(),
            allowed: vec![NAME.into(), "read_project_file".into()],
            routes: Vec::new(),
            chat_key: Some("qa-chat".into()),
            exchange_key: "qa-exchange".into(),
            edit_target: None,
            grant: None,
        }
    }

    fn data_dir(tag: &str) -> std::path::PathBuf {
        let data = std::env::temp_dir().join(format!("qa-r5-time-{tag}-{}", std::process::id()));
        let dir = data.join("users").join(USER);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("projects.json"),
            r#"{"projects":[{"id":"qa-proj","name":"QA","files":[]}]}"#,
        )
        .unwrap();
        data
    }

    fn rt(server_zone: Option<&str>) -> Runtime {
        Runtime {
            recorded: true,
            server_zone: server_zone.map(str::to_string),
        }
    }

    fn reported(node: &str, icu: &str, tz: &str, zone: &str) -> Option<(u16, JValue)> {
        let body = js_json::parse(&format!(
            r#"{{"node":"{node}","icu":"{icu}","tz":"{tz}","zone":"{zone}"}}"#
        ))
        .unwrap();
        Some((200, body))
    }

    /// Only a 200 is kept; core's Node must be the recording's, its zone a recorded one.
    #[test]
    fn what_core_reports_decides() {
        let (node, icu, tz) = current_time::recorded_runtime();
        assert_eq!(runtime_of(None), None);
        let older = Runtime {
            recorded: false,
            server_zone: None,
        };
        assert_eq!(runtime_of(Some((404, JValue::Null))), Some(older));
        let gone = js_json::parse(r#"{"error":"no such account"}"#).unwrap();
        assert_eq!(runtime_of(Some((404, gone.clone()))), None);
        assert_eq!(runtime_of(Some((500, JValue::Null))), None);
        assert_eq!(
            runtime_of(reported(node, icu, tz, "America/New_York")),
            Some(rt(Some("America/New_York")))
        );
        // ICU's own name for Kolkata is what core reports, and what the recording ran under.
        assert_eq!(
            runtime_of(reported(node, icu, tz, "Asia/Calcutta")),
            Some(rt(Some("Asia/Calcutta")))
        );
        for zone in ["", "Eire", "Europe/Paris", "Asia/Kolkata"] {
            assert_eq!(
                runtime_of(reported(node, icu, tz, zone)),
                Some(rt(None)),
                "{zone}"
            );
        }
        let other = Runtime {
            recorded: false,
            server_zone: Some("UTC".into()),
        };
        assert_eq!(
            runtime_of(reported(node, icu, "2099z", "UTC")),
            Some(other.clone())
        );
        assert_eq!(runtime_of(reported("99.0.0", icu, tz, "UTC")), Some(other));
        assert_eq!(
            runtime_of(Some((200, js_json::parse("{}").unwrap()))),
            Some(Runtime {
                recorded: false,
                server_zone: None
            })
        );
        // The process-wide cache (only this test touches it): nothing answered before a 200.
        let data = data_dir("runtime");
        assert_eq!(answer(&data, USER, &run("{}"), NOW, yes), None);
        assert!(!runtime_answered(Some((404, gone))));
        assert_eq!(runtime_known(), None);
        assert!(!runtime_answered(reported(node, icu, "2099z", "UTC")));
        assert_eq!(answer(&data, USER, &run("{}"), NOW, yes), None);
        assert!(runtime_answered(reported(node, icu, tz, "UTC")));
        assert_eq!(runtime_known(), Some(rt(Some("UTC"))));
        assert!(answer(&data, USER, &run("{}"), NOW, yes).is_some());
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn answers_verified_zones_and_leaves_the_rest_to_core() {
        let data = data_dir("zones");
        let ny = rt(Some("America/New_York"));
        let a = |run: &ToolRun, rt: &Runtime, now: f64| answer_with(rt, &data, USER, run, now, yes);
        // The texts are the leaf's (replayed against Node there); here: who answers.
        let (text, failed) = a(&run(r#"{"timezone":"Europe/Berlin"}"#), &ny, NOW).unwrap();
        assert!(!failed);
        assert!(text.starts_with("Current time: Saturday, 14 November 2026 at 07:05:12 CET (Europe/Berlin) | ISO: 2026-11-14T06:05:12.000Z | "), "{text}");
        let (text, _) = a(&run("{}"), &ny, NOW).unwrap();
        assert!(
            text.starts_with(
                "Current time: Sat Nov 14 2026 01:05:12 GMT-0500 (Eastern Standard Time) | "
            ),
            "{text}"
        );
        let (text, _) = a(&run(""), &ny, NOW).unwrap();
        assert!(text.contains("Eastern Standard Time"));
        // A non-string or empty timezone is the server zone's, as in Node.
        let (text, _) = a(&run(r#"{"timezone":5}"#), &ny, NOW).unwrap();
        assert!(text.contains("Eastern Standard Time"));
        let (text, _) = a(&run(r#"{"timezone":""}"#), &ny, NOW).unwrap();
        assert!(text.contains("Eastern Standard Time"));
        let (text, _) = a(&run(r#"{"timezone":"+05:30"}"#), &rt(None), NOW).unwrap();
        assert!(text.contains("(+05:30)"), "{text}");
        // Core's: another Node, the server zone not a recorded one, an unknown zone (core's error
        // text), a zone whose tz data differs, outside the recorded span, and every call the bridge
        // answers otherwise.
        let other = Runtime {
            recorded: false,
            server_zone: Some("America/New_York".into()),
        };
        assert_eq!(
            a(&run(r#"{"timezone":"Europe/Berlin"}"#), &other, NOW),
            None
        );
        assert_eq!(a(&run("{}"), &rt(None), NOW), None);
        assert_eq!(a(&run(r#"{"timezone":"Mars/Olympus"}"#), &ny, NOW), None);
        assert_eq!(
            a(&run(r#"{"timezone":"America/Vancouver"}"#), &ny, NOW),
            None
        );
        assert_eq!(a(&run("{}"), &ny, 1_900_000_000_000.0), None);
        assert_eq!(a(&run("[]"), &ny, NOW), None);
        assert_eq!(a(&run("{"), &ny, NOW), None);
        assert_eq!(
            answer_with(&ny, &data, USER, &run("{}"), NOW, |_| false),
            None
        );
        let mut other = run("{}");
        other.name = "read_project_file".into();
        assert_eq!(a(&other, &ny, NOW), None);
        let mut off = run("{}");
        off.allowed = vec!["read_project_file".into()];
        assert_eq!(a(&off, &ny, NOW), None);
        let mut routed = run("{}");
        routed.routes = vec![(NAME.into(), "qa-mcp".into())];
        assert_eq!(a(&routed, &ny, NOW), None);
        let mut missing = run("{}");
        missing.project_id = Some("qa-gone".into());
        assert_eq!(a(&missing, &ny, NOW), None);
        let mut free = run("{}");
        free.project_id = None;
        assert!(a(&free, &ny, NOW).is_some());
        let _ = std::fs::remove_dir_all(&data);
    }
}
