//! chat-pipeline `toolbox` against core chat.cjs's tool selection and toolboxes.cjs (tests/fixtures/record-select.cjs).
#![allow(clippy::expect_used, clippy::indexing_slicing, clippy::panic)]
use chat_pipeline::replay::map_history;
use chat_pipeline::toolbox::{self as tb, Request, Router, Routing, Turn};
use chat_pipeline::tools_mode::tools_mode_of;
use js_json::{wtf16, JValue};
use sha2::{Digest, Sha256};
use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::future::Future;
use std::pin::pin;
use std::task::{Context, Poll, Waker};

fn arr(v: &JValue) -> &[JValue] {
    match v {
        JValue::Arr(a) => a,
        _ => &[],
    }
}

fn strs(v: &JValue) -> Vec<String> {
    arr(v)
        .iter()
        .filter_map(|x| x.as_str().map(str::to_owned))
        .collect()
}

fn block<F: Future>(f: F) -> F::Output {
    let mut f = pin!(f);
    let mut cx = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(v) = f.as_mut().poll(&mut cx) {
            return v;
        }
    }
}

/// The router's recorded answer, for the ids it was recorded with.
struct Recorded {
    call: JValue,
    seen: RefCell<Option<Vec<String>>>,
}

impl Router for Recorded {
    async fn select(&self, ids: &[String], _message: &JValue) -> Routing {
        *self.seen.borrow_mut() = Some(ids.to_vec());
        let a = self.call.get("answer");
        Routing {
            ids: strs(a.get("ids")),
            routed: a.get("routed") == &JValue::Bool(true),
            narrowed: a.get("narrowed").truthy(),
        }
    }
}

fn sha(v: &JValue) -> String {
    let text = wtf16::stringify(v).expect("tools");
    // JSON.stringify's text, hashed as UTF-8 (the fixture has no lone surrogates).
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[test]
fn units_match_node() {
    let c = wtf16::parse(include_str!("fixtures/select-cases.json")).expect("fixture");
    for u in arr(c.get("units")) {
        if let Some(m) = u.get("model").as_str() {
            assert_eq!(
                Some(tb::tool_cap_for(m) as f64),
                u.get("cap").as_num(),
                "cap {m}"
            );
            assert_eq!(
                Some(tb::tool_token_budget_for(m, None)),
                u.get("budget").as_num(),
                "budget {m}"
            );
            assert_eq!(
                Some(tb::tool_token_budget_for(m, Some(777.0))),
                u.get("measured777").as_num(),
                "measured {m}"
            );
        } else {
            assert_eq!(
                Some(tb::estimate_tool_tokens(arr(u.get("tools")))),
                u.get("est").as_num(),
                "{u:?}"
            );
        }
    }
}

#[test]
fn tool_selection_matches_node() {
    let c = wtf16::parse(include_str!("fixtures/select-cases.json")).expect("fixture");
    let all = arr(c.get("ALL"));
    let defaults = vec!["core".to_owned()];
    let connectors: HashSet<String> = ["gdrive".to_owned()].into_iter().collect();
    let mut checked = (0, 0);
    for (i, k) in arr(c.get("cases")).iter().enumerate() {
        let out = k.get("out");
        let ids = strs(out.get("boxes"));
        let boxes: Vec<JValue> = ids
            .iter()
            .map(|id| {
                all.iter()
                    .find(|b| b.get("id").as_str() == Some(id))
                    .expect("box")
                    .clone()
            })
            .collect();
        let project = match k.get("project") {
            JValue::Undefined => JValue::Null,
            p => p.clone(),
        };
        let connected = strs(k.get("connected"));
        let oauth: HashSet<String> = strs(k.get("oauth")).into_iter().collect();
        let not_signed = strs(k.get("notSignedIn"));
        let not_ready = strs(k.get("notReady"));
        let blocked = strs(k.get("blockedNames"));
        let writes = strs(out.get("writes"));
        let account_ready = |id: &str| !not_signed.iter().any(|n| n == id);
        let is_write = |n: &str| writes.iter().any(|w| w == n);
        let ready = |b: &JValue| {
            !b.get("id")
                .as_str()
                .is_some_and(|id| not_ready.iter().any(|n| n == id))
        };
        let policy = |n: &JValue| n.as_str().is_some_and(|n| blocked.iter().any(|b| b == n));
        let history = JValue::Arr(map_history(k.get("history"), 20));
        let provider = k.get("providerId").as_str().unwrap_or("default");
        let t = Turn {
            request: Request {
                project: &project,
                defaults: &defaults,
                connectors: &connectors,
                connected: &connected,
                offered: &ids,
                turn_toolboxes: k.get("turnToolboxes"),
                diary_enabled: k.get("diaryEnabled") == &JValue::Bool(true),
                oauth: &oauth,
                account_ready: &account_ready,
            },
            boxes: &boxes,
            message: k.get("message"),
            history: &history,
            automatic_tools: tools_mode_of(&project) == "auto",
            compact_only: false,
            automatic_local: provider == "default",
            external_strict: out.get("externalStrict") == &JValue::Bool(true),
            cloud_ungranted: false,
            project_has_files: false,
            is_write_tool: &is_write,
            ready: &ready,
            policy_blocked: &policy,
        };
        let router = Recorded {
            call: out.get("routerCall").clone(),
            seen: RefCell::new(None),
        };
        let s = block(tb::select(&t, &router)).expect("select");
        assert_eq!(
            router.seen.borrow().clone(),
            Some(strs(out.get("routerCall").get("ids"))),
            "router ids {i}: {k:?}"
        );
        assert_eq!(
            &tb::tools_scope_event(&t, &s),
            out.get("scope"),
            "scope {i}: {k:?}"
        );
        let up = out.get("upstream");
        if up.is_object() {
            let measured = k.get("measured").as_num();
            let model = k.get("model").as_str().unwrap_or("");
            let r = tb::resolve_for(&t, &s, model, measured).expect("resolve");
            let active = JValue::Arr(tb::active_tools(&s, &r));
            let names: Vec<String> = arr(&active)
                .iter()
                .map(|x| {
                    x.get("function")
                        .get("name")
                        .as_str()
                        .unwrap_or("")
                        .to_owned()
                })
                .collect();
            assert_eq!(
                names,
                strs(up.get("names")),
                "tools {i}: {k:?} {:?}",
                r.dropped
            );
            if Some(sha(&active).as_str()) != up.get("toolsSha").as_str() {
                let _ = std::fs::write(
                    std::env::temp_dir().join("select-diff.txt"),
                    format!("{i}\n{}\n", wtf16::stringify(&active).unwrap_or_default()),
                );
            }
            assert_eq!(
                Some(sha(&active).as_str()),
                up.get("toolsSha").as_str(),
                "tool JSON {i}"
            );
            checked.0 += 1;
        }
        checked.1 += 1;
    }
    assert!(checked.0 > 1000, "{checked:?}");
}

/// A router that never answers leaves the turn at its await; dropping it there starts nothing else.
#[test]
fn dropping_the_selection_at_the_router_await_ends_it() {
    struct Stuck<'a>(&'a Cell<u32>);
    impl Router for Stuck<'_> {
        async fn select(&self, _ids: &[String], _m: &JValue) -> Routing {
            self.0.set(self.0.get() + 1);
            std::future::pending::<()>().await;
            unreachable!()
        }
    }
    let calls = Cell::new(0);
    let none: HashSet<String> = HashSet::new();
    let defaults = vec!["core".to_owned()];
    let msg = JValue::Str("search the web for cats".into());
    let core = JValue::obj([
        ("id", JValue::Str("core".into())),
        ("tools", JValue::Arr(vec![])),
    ]);
    let boxes = vec![core];
    let ids = vec!["core".to_owned()];
    let resolved = Cell::new(0u32);
    let t = Turn {
        request: Request {
            project: &JValue::Null,
            defaults: &defaults,
            connectors: &none,
            connected: &[],
            offered: &ids,
            turn_toolboxes: &JValue::Undefined,
            diary_enabled: false,
            oauth: &none,
            account_ready: &|_| true,
        },
        boxes: &boxes,
        message: &msg,
        history: &JValue::Arr(vec![]),
        automatic_tools: true,
        compact_only: false,
        automatic_local: true,
        external_strict: false,
        cloud_ungranted: false,
        project_has_files: false,
        is_write_tool: &|_| false,
        ready: &|_| true,
        policy_blocked: &|_| {
            resolved.set(resolved.get() + 1);
            false
        },
    };
    {
        let router = Stuck(&calls);
        let mut f = pin!(tb::select(&t, &router));
        let mut cx = Context::from_waker(Waker::noop());
        assert!(f.as_mut().poll(&mut cx).is_pending());
        assert!(f.as_mut().poll(&mut cx).is_pending());
    }
    assert_eq!(calls.get(), 1, "the router is asked once");
    assert_eq!(
        resolved.get(),
        0,
        "nothing is resolved after a dropped selection"
    );
}
