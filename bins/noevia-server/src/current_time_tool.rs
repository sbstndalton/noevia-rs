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
//! The server zone is the web container's `TZ` (Node's ICU reads the same variable): only an IANA
//! zone ID spelled as tzdb spells it ([`current_time::server_zone_from_tz`]). With `TZ` unset ICU
//! reads the system's zone file; calls without a `timezone` argument then go to core.
//!
//! Core keeps counting the call: chat_native.rs asks core's native op `tool-used`, as for
//! read_project_file.
use chat_turn::host::ToolRun;
use js_json::JValue;
use std::path::Path;
use std::sync::OnceLock;

/// The tool this module answers.
pub const NAME: &str = "get_current_time";

/// `NOEVIA_RUST_CURRENT_TIME=1`, read once.
pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("NOEVIA_RUST_CURRENT_TIME").as_deref() == Ok("1"))
}

/// The process's `TZ` as the server zone, read once (see the module documentation).
pub fn server_zone() -> Option<&'static str> {
    static ZONE: OnceLock<Option<String>> = OnceLock::new();
    ZONE.get_or_init(|| {
        let tz = std::env::var("TZ").ok();
        current_time::server_zone_from_tz(tz.as_deref()).map(str::to_string)
    })
    .as_deref()
}

/// The tool's text and failure flag when the front answers this call at `now_ms` on a server whose
/// zone is `server_zone`; `None` leaves it to core. `active(user)` is core's account check
/// ([`crate::project_file_read::account_active`]).
pub fn answer(
    data_dir: &Path,
    user: &str,
    run: &ToolRun,
    server_zone: Option<&str>,
    now_ms: f64,
    active: impl FnOnce(&str) -> bool,
) -> Option<(String, bool)> {
    let (args, _, _) = crate::project_file_read::bridge_scope(data_dir, user, run, NAME, active)?;
    time_text(&args, server_zone, now_ms)
}

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

    #[test]
    fn answers_verified_zones_and_leaves_the_rest_to_core() {
        let data = data_dir("zones");
        let ny = Some("America/New_York");
        // The texts are the leaf's (replayed against Node there); here: who answers.
        let (text, failed) = answer(
            &data,
            USER,
            &run(r#"{"timezone":"Europe/Berlin"}"#),
            ny,
            NOW,
            yes,
        )
        .unwrap();
        assert!(!failed);
        assert!(text.starts_with("Current time: Saturday, 14 November 2026 at 07:05:12 CET (Europe/Berlin) | ISO: 2026-11-14T06:05:12.000Z | "), "{text}");
        let (text, _) = answer(&data, USER, &run("{}"), ny, NOW, yes).unwrap();
        assert!(
            text.starts_with(
                "Current time: Sat Nov 14 2026 01:05:12 GMT-0500 (Eastern Standard Time) | "
            ),
            "{text}"
        );
        let (text, _) = answer(&data, USER, &run(""), ny, NOW, yes).unwrap();
        assert!(text.contains("Eastern Standard Time"));
        // A non-string or empty timezone is the server zone's, as in Node.
        let (text, _) = answer(&data, USER, &run(r#"{"timezone":5}"#), ny, NOW, yes).unwrap();
        assert!(text.contains("Eastern Standard Time"));
        let (text, _) = answer(
            &data,
            USER,
            &run(r#"{"timezone":"+05:30"}"#),
            None,
            NOW,
            yes,
        )
        .unwrap();
        assert!(text.contains("(+05:30)"), "{text}");
        // Core's: the server zone unknown, an unknown zone (core's error text), a zone whose tz data
        // differs, outside the recorded span, and every call the bridge answers otherwise.
        assert_eq!(answer(&data, USER, &run("{}"), None, NOW, yes), None);
        assert_eq!(
            answer(
                &data,
                USER,
                &run(r#"{"timezone":"Mars/Olympus"}"#),
                ny,
                NOW,
                yes
            ),
            None
        );
        assert_eq!(
            answer(
                &data,
                USER,
                &run(r#"{"timezone":"America/Vancouver"}"#),
                ny,
                NOW,
                yes
            ),
            None
        );
        assert_eq!(
            answer(&data, USER, &run("{}"), Some("Africa/Casablanca"), NOW, yes),
            None
        );
        assert_eq!(
            answer(&data, USER, &run("{}"), ny, 1_900_000_000_000.0, yes),
            None
        );
        assert_eq!(answer(&data, USER, &run("[]"), ny, NOW, yes), None);
        assert_eq!(answer(&data, USER, &run("{"), ny, NOW, yes), None);
        assert_eq!(answer(&data, USER, &run("{}"), ny, NOW, |_| false), None);
        let mut other = run("{}");
        other.name = "read_project_file".into();
        assert_eq!(answer(&data, USER, &other, ny, NOW, yes), None);
        let mut off = run("{}");
        off.allowed = vec!["read_project_file".into()];
        assert_eq!(answer(&data, USER, &off, ny, NOW, yes), None);
        let mut routed = run("{}");
        routed.routes = vec![(NAME.into(), "qa-mcp".into())];
        assert_eq!(answer(&data, USER, &routed, ny, NOW, yes), None);
        let mut missing = run("{}");
        missing.project_id = Some("qa-gone".into());
        assert_eq!(answer(&data, USER, &missing, ny, NOW, yes), None);
        let mut free = run("{}");
        free.project_id = None;
        assert!(answer(&data, USER, &free, ny, NOW, yes).is_some());
        let _ = std::fs::remove_dir_all(&data);
    }
}
