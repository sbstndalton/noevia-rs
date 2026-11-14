//! Replays `fixtures/tool-dispatch.v1.json` through [`tool_dispatch::execute`], with fake [`Executors`]
//! that answer as the recording's fakes do (see `gen-fixtures.cjs`); the calls this port does not read
//! are answered with core's recorded outcome, as the host hands them to core.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
use js_json::{wtf16, JValue};
use std::collections::HashSet;
use std::future::Future;
use std::pin::pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Waker};
use tool_dispatch::*;

/// Every fake answers at once, so one poll finishes the call.
fn now<F: Future>(f: F) -> F::Output {
    let mut f = pin!(f);
    match f.as_mut().poll(&mut Context::from_waker(Waker::noop())) {
        Poll::Ready(v) => v,
        Poll::Pending => panic!("a fake executor waited"),
    }
}

struct Fakes {
    name: String,
    fence_closed: bool,
    recorded: Outcome,
    handed_to_core: AtomicUsize,
}

impl Fakes {
    fn sends(&self, marker: String) -> Ran {
        if self.name.contains("write") && self.fence_closed {
            Ran::FenceRefused
        } else {
            Ran::text(marker)
        }
    }
}

impl Executors for Fakes {
    async fn specialist(&self, args: &JValue) -> Ran {
        Ran::text(format!("SPEC {}", args_json(args)))
    }
    async fn mcp(&self, name: &str, server: Option<&str>, args: &JValue) -> Ran {
        let marker = format!("MCP {} {}", server.unwrap_or("-"), args_json(args));
        if name.starts_with("mcp_fail") {
            Ran::text(format!("ERROR from tool: {marker}"))
        } else {
            self.sends(marker)
        }
    }
    async fn kiwix(&self, name: &str, args: &JValue) -> Ran {
        self.sends(format!("KIWIX {name} {}", args_json(args)))
    }
    async fn drive(&self, name: &str, args: &JValue) -> Ran {
        if name.starts_with("drive_fail") {
            Ran::text(format!("ERROR: DRIVE {name}"))
        } else {
            self.sends(format!("DRIVE {name} {}", args_json(args)))
        }
    }
    async fn current_time(&self, timezone: Option<&str>) -> Ran {
        match timezone {
            Some(tz) if tz.starts_with("Bad") => Ran::failed(unknown_zone(tz)),
            Some(tz) => Ran::text(format!("Current time: ZONED ({tz}) | ISO: ISO | FACTS")),
            None => Ran::text("Current time: SERVER | ISO: ISO | FACTS"),
        }
    }
    async fn read_project_file(&self, args: &JValue) -> Ran {
        let name = JValue::Arr(vec![args.get("name").clone()]);
        Ran::failed(format!("ERROR: RPF {}", args_json(&name)))
    }
    async fn not_ported(&self, _name: &str, _raw_args: &str) -> Outcome {
        self.handed_to_core.fetch_add(1, Ordering::Relaxed);
        self.recorded.clone()
    }
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

#[test]
fn execute_ends_every_recorded_call_as_core_does() {
    let f = wtf16::parse(include_str!("fixtures/tool-dispatch.v1.json")).unwrap();
    let JValue::Arr(cases) = f.get("cases") else {
        panic!("no cases")
    };
    let (mut core_answered, mut counted) = (0usize, 0usize);
    for c in cases {
        let name = c.get("name").as_str().unwrap();
        let raw = c.get("rawArgs").as_str().unwrap();
        let allowed: Option<HashSet<String>> =
            strs(c.get("allowed")).map(|v| v.into_iter().collect());
        let routes: Option<Vec<(String, String)>> = match c.get("routes") {
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
        };
        let kiwix = strs(c.get("kiwix")).unwrap_or_default();
        let drive = strs(c.get("drive")).unwrap_or_default();
        let mcp: HashSet<String> = strs(c.get("mcp")).unwrap_or_default().into_iter().collect();
        let coworker = c.get("project").as_str() == Some("coworker");
        let offer = Offer {
            allowed: allowed.as_ref(),
            routes: routes.as_deref(),
            specialist: (c.get("specialist").as_bool() == Some(true) && coworker)
                .then_some("page_tool"),
            kiwix: &kiwix,
            drive: &drive,
            mcp: Some(&mcp),
        };
        let want = Outcome {
            text: c.get("text").as_str().unwrap().to_owned(),
            failed: c.get("failed").as_bool().unwrap(),
            not_run: c.get("notRun").as_bool().unwrap(),
        };
        let fakes = Fakes {
            name: name.to_owned(),
            fence_closed: c.get("fence").as_bool() == Some(true),
            recorded: want.clone(),
            handed_to_core: AtomicUsize::new(0),
        };
        let got = now(execute(&fakes, name, raw, &offer));
        assert_eq!(got, want, "name {name:?} args {raw:?}");
        core_answered += fakes.handed_to_core.load(Ordering::Relaxed);
        counted += usize::from(counted_by_core(&got));
    }
    assert!(core_answered < 200, "handed to core {core_answered}");
    // The fence refused some writes, which core does not count; everything else it counts.
    assert!(counted < cases.len() && counted > cases.len() / 2);
}

#[test]
fn a_closed_fence_is_not_run_and_not_counted() {
    let fakes = Fakes {
        name: "mcp_write".into(),
        fence_closed: true,
        recorded: refused(String::new()),
        handed_to_core: AtomicUsize::new(0),
    };
    let mcp: HashSet<String> = ["mcp_write".to_owned()].into_iter().collect();
    let offer = Offer {
        mcp: Some(&mcp),
        ..Offer::default()
    };
    let got = now(execute(&fakes, "mcp_write", "{}", &offer));
    assert_eq!(got, fence_refused());
    assert!(got.failed && got.not_run && !counted_by_core(&got));
    assert_eq!(got.text, format!("ERROR: {FENCE_MESSAGE}"));
}
