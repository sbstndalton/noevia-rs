//! [`tool_dispatch::decide`] against core's recorded executor: every call of
//! `fixtures/tool-dispatch.v1.json`, with each fact the host may not know (page tool applicability, the
//! encyclopedia, Drive and discovered MCP names) hidden in every combination. A decided route must be
//! the one [`tool_dispatch::route`] takes with every fact (which `oracle.rs` checks against core), and
//! with every fact known nothing is left to core.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
use js_json::{wtf16, JValue};
use std::collections::HashSet;
use tool_dispatch::*;

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

const PAGE_TOOL: &str = "page_tool";

#[test]
fn a_decided_route_is_always_cores_whatever_the_host_does_not_know() {
    let f = wtf16::parse(include_str!("fixtures/tool-dispatch.v1.json")).unwrap();
    let JValue::Arr(cases) = f.get("cases") else {
        panic!("no cases")
    };
    assert!(cases.len() > 3000);
    let (mut decided, mut offer_only_decided, mut with_offer) = (0usize, 0usize, 0usize);
    for c in cases {
        let name = c.get("name").as_str().unwrap();
        let raw = c.get("rawArgs").as_str().unwrap();
        let allowed: Option<HashSet<String>> =
            strs(c.get("allowed")).map(|v| v.into_iter().collect());
        let routes = pairs(c.get("routes"));
        let kiwix = strs(c.get("kiwix")).unwrap_or_default();
        let drive = strs(c.get("drive")).unwrap_or_default();
        let mcp: HashSet<String> = strs(c.get("mcp")).unwrap_or_default().into_iter().collect();
        let applies =
            c.get("specialist").as_bool() == Some(true) && c.get("project").as_str() == Some("coworker");
        let offer = Offer {
            allowed: allowed.as_ref(),
            routes: routes.as_deref(),
            specialist: applies.then_some(PAGE_TOOL),
            kiwix: &kiwix,
            drive: &drive,
            mcp: Some(&mcp),
        };
        let full = route(name, raw, &offer);
        for hidden in 0u8..16 {
            let known = Knowledge {
                allowed: allowed.as_ref(),
                routes: routes.as_deref(),
                specialist_name: PAGE_TOOL,
                specialist_applies: (hidden & 1 == 0).then_some(applies),
                kiwix: (hidden & 2 == 0).then_some(kiwix.as_slice()),
                drive: (hidden & 4 == 0).then_some(drive.as_slice()),
                mcp: (hidden & 8 == 0).then_some(&mcp),
            };
            match decide(name, raw, &known) {
                Decision::Route(r) => {
                    assert_eq!(r, full, "hidden {hidden:#06b} name {name:?} args {raw:?}");
                    decided += 1;
                    if hidden == 15 {
                        offer_only_decided += 1;
                    }
                }
                Decision::Core => assert_ne!(hidden, 0, "all known, left to core: {name:?} {raw:?}"),
            }
        }
        if allowed.is_some() && routes.is_some() {
            with_offer += 1;
        }
    }
    // The offer alone settles a real share (refusals and routed calls): the wiring has work to do.
    assert!(offer_only_decided > 500, "offer only decided {offer_only_decided}");
    assert!(decided > cases.len() * 4, "decided {decided}");
    assert!(with_offer > 100, "cases with a full offer {with_offer}");
}
