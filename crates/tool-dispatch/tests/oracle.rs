//! Replays `fixtures/tool-dispatch.v1.json`, recorded from core's own `server/toolboxes.cjs`
//! `executeToolCall` by `gen-fixtures.cjs` (see its header for the executor markers). Each case runs
//! [`tool_dispatch::route`], plays the executor it names with the same marker, and compares text,
//! `failed` and `notRun` with core's.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
use js_json::{wtf16, JValue};
use std::collections::HashSet;
use tool_dispatch::*;

fn fixture() -> JValue {
    wtf16::parse(include_str!("fixtures/tool-dispatch.v1.json")).unwrap()
}

fn strs(v: &JValue) -> Option<Vec<String>> {
    match v {
        JValue::Arr(items) => Some(
            items
                .iter()
                .map(|x| x.as_str().unwrap().to_owned())
                .collect(),
        ),
        _ => None,
    }
}

fn pairs(v: &JValue) -> Option<Vec<(String, String)>> {
    match v {
        JValue::Arr(items) => Some(
            items
                .iter()
                .map(|p| {
                    let p = strs(p).unwrap();
                    (p[0].clone(), p[1].clone())
                })
                .collect(),
        ),
        _ => None,
    }
}

/// The executor a route names, played as the recording's fakes play it.
fn play(name: &str, route: Route, fence_closed: bool) -> Option<Outcome> {
    let sends = |marker: String| {
        if name.contains("write") && fence_closed {
            fence_refused()
        } else {
            finish(marker, false)
        }
    };
    Some(match route {
        Route::Refused(text) => refused(text),
        Route::Specialist(args) => finish(format!("SPEC {}", args_json(&args)), false),
        Route::Mcp { server, args } => {
            let marker = format!(
                "MCP {} {}",
                server.as_deref().unwrap_or("-"),
                args_json(&args)
            );
            if name.starts_with("mcp_fail") {
                finish(format!("ERROR from tool: {marker}"), false)
            } else {
                sends(marker)
            }
        }
        Route::Kiwix(args) => sends(format!("KIWIX {name} {}", args_json(&args))),
        Route::Drive(args) => {
            if name.starts_with("drive_fail") {
                finish(format!("ERROR: DRIVE {name}"), false)
            } else {
                sends(format!("DRIVE {name} {}", args_json(&args)))
            }
        }
        Route::CurrentTime { timezone } => match timezone {
            Some(tz) if tz.starts_with("Bad") => finish(unknown_zone(&tz), true),
            Some(tz) => finish(
                format!("Current time: ZONED ({tz}) | ISO: ISO | FACTS"),
                false,
            ),
            None => finish("Current time: SERVER | ISO: ISO | FACTS".into(), false),
        },
        Route::ReadProjectFile(args) => {
            let name = JValue::Arr(vec![args.get("name").clone()]);
            finish(format!("ERROR: RPF {}", args_json(&name)), true)
        }
        Route::NotPorted => return None,
    })
}

#[test]
fn the_constants_are_cores() {
    let f = fixture();
    assert_eq!(f.get("version").as_num(), Some(1.0));
    assert_eq!(f.get("source").get("path").as_str(), Some("server/toolboxes.cjs"));
    assert_eq!(f.get("source").get("sha256").as_str().unwrap().len(), 64);
    let pin = include_str!("fixtures/core-ref").trim();
    assert_eq!(f.get("source").get("coreRef").as_str(), Some(pin));
    assert_eq!(f.get("fenceMessage").as_str(), Some(FENCE_MESSAGE));
    #[allow(clippy::cast_precision_loss)]
    let cap = TOOL_RESULT_CAP as f64;
    assert_eq!(f.get("resultCap").as_num(), Some(cap));
}

#[test]
fn every_recorded_call_routes_and_ends_as_in_core() {
    let f = fixture();
    let JValue::Arr(cases) = f.get("cases") else {
        panic!("no cases")
    };
    assert!(cases.len() > 3000);
    let (mut checked, mut not_ported) = (0usize, 0usize);
    for c in cases {
        let name = c.get("name").as_str().unwrap();
        let raw = c.get("rawArgs").as_str().unwrap();
        let allowed: Option<HashSet<String>> = strs(c.get("allowed")).map(|v| v.into_iter().collect());
        let routes = pairs(c.get("routes"));
        let kiwix = strs(c.get("kiwix")).unwrap_or_default();
        let drive = strs(c.get("drive")).unwrap_or_default();
        let mcp: HashSet<String> = strs(c.get("mcp")).unwrap_or_default().into_iter().collect();
        let coworker = c.get("project").as_str() == Some("coworker");
        let offer = Offer {
            allowed: allowed.as_ref(),
            routes: routes.as_deref(),
            specialist: (c.get("specialist").as_bool() == Some(true) && coworker).then_some("page_tool"),
            kiwix: &kiwix,
            drive: &drive,
            mcp: Some(&mcp),
        };
        let fence = c.get("fence").as_bool() == Some(true);
        let Some(got) = play(name, route(name, raw, &offer), fence) else {
            // Stricter than the JS, and only for these arguments (see the crate docs).
            assert!(
                raw.contains("1e400") || raw.contains('\u{10F800}') || raw.matches('[').count() > 100,
                "not ported: {raw:?}"
            );
            not_ported += 1;
            continue;
        };
        let want = Outcome {
            text: c.get("text").as_str().unwrap().to_owned(),
            failed: c.get("failed").as_bool().unwrap(),
            not_run: c.get("notRun").as_bool().unwrap(),
        };
        assert_eq!(got, want, "name {name:?} args {raw:?} case {c:?}");
        checked += 1;
    }
    assert!(checked > 3000, "checked {checked}");
    assert!(not_ported < 200, "not ported {not_ported}");
}
