//! chat-pipeline `tools_mode` against core tools-mode.cjs (tests/fixtures/record-tools-mode.cjs).
#![allow(clippy::expect_used, clippy::indexing_slicing, clippy::panic)]
use chat_pipeline::tools_mode::{self as tm, Eligibility, Moved, ToolsReady};
use chat_pipeline::Thrown;
use js_json::{wtf16, JValue};
use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::future::Future;
use std::pin::pin;
use std::task::{Context, Poll, Waker};

fn cases() -> JValue {
    wtf16::parse(include_str!("fixtures/tools-mode-cases.json")).expect("fixture")
}

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

fn undef(v: &JValue) -> JValue {
    if v.as_str() == Some("__undef") {
        JValue::Undefined
    } else {
        v.clone()
    }
}

fn boxes_of(all: &JValue, ix: &JValue) -> JValue {
    JValue::Arr(
        arr(ix)
            .iter()
            .map(|k| arr(all)[k.as_num().unwrap_or(0.0) as usize].clone())
            .collect(),
    )
}

/// Drives a future that never pends on real I/O.
fn block<F: Future>(f: F) -> F::Output {
    let mut f = pin!(f);
    let mut cx = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(v) = f.as_mut().poll(&mut cx) {
            return v;
        }
    }
}

fn is_write(n: &str) -> bool {
    n.ends_with("_write") || n.ends_with("_send")
}

#[test]
fn tools_mode_matches_node() {
    let c = cases();
    let all = c.get("BOXES");
    for m in arr(c.get("modes")) {
        let p = undef(m.get("project"));
        assert_eq!(Some(tm::tools_mode_of(&p)), m.get("mode").as_str(), "{m:?}");
        assert_eq!(
            Some(tm::initial_tools_mode(&p)),
            m.get("initial").as_str(),
            "{m:?}"
        );
    }
    for g in arr(c.get("gates")) {
        assert_eq!(
            Some(tm::gate_says_tool_needed(g.get("gate"))),
            g.get("needed").as_bool(),
            "{g:?}"
        );
    }
    for (i, b) in arr(c.get("boxesFor")).iter().enumerate() {
        let m = b.get("message");
        assert_eq!(
            Some(tm::wants_web(m)),
            b.get("wantsWeb").as_bool(),
            "wantsWeb {i}: {m:?}"
        );
        let got: Vec<String> = tm::automatic_boxes_for(m)
            .into_iter()
            .map(str::to_owned)
            .collect();
        assert_eq!(got, strs(b.get("boxes")), "boxesFor {i}: {m:?}");
    }
    for (i, a) in arr(c.get("additions")).iter().enumerate() {
        let connectors: HashSet<String> = strs(a.get("connectors")).into_iter().collect();
        let answers: Vec<bool> = arr(a.get("answers"))
            .iter()
            .map(|x| x.as_bool() == Some(true))
            .collect();
        let at = Cell::new(0usize);
        let ready = |_: &JValue| {
            let k = at.get();
            at.set(k + 1);
            answers.get(k).copied().unwrap_or(false)
        };
        let e = Eligibility {
            connector_boxes: &connectors,
            is_write_tool: &is_write,
            ready: &ready,
            private_data: a.get("privateData").as_bool() == Some(true),
        };
        let got = tm::automatic_additions(
            a.get("message"),
            arr(a.get("base")),
            &boxes_of(all, a.get("boxes")),
            &e,
            a.get("max").as_num().unwrap_or(0.0) as usize,
            &undef(a.get("carried")),
        );
        let want = if a.get("result").as_str() == Some("__threw") {
            Err(Thrown)
        } else {
            Ok(strs(a.get("result")))
        };
        assert_eq!(got, want, "additions {i}: {a:?}");
        assert_eq!(at.get(), answers.len(), "ready calls {i}");
    }
    for (i, h) in arr(c.get("carried")).iter().enumerate() {
        let got = tm::carried_public_boxes(h.get("history"), &boxes_of(all, h.get("boxes")));
        assert_eq!(got, Ok(strs(h.get("result"))), "carried {i}: {h:?}");
    }
    for o in arr(c.get("order")) {
        let got = tm::order_with_additions(
            &strs(o.get("ids")),
            &strs(o.get("added")),
            &strs(o.get("head")),
        );
        assert_eq!(got, strs(o.get("result")), "{o:?}");
    }
    for (i, r) in arr(c.get("ready")).iter().enumerate() {
        let storage = r.get("storage").clone();
        let reads = Cell::new(0);
        let get_storage = |_: &str| {
            reads.set(reads.get() + 1);
            if storage.as_str() == Some("__throw") {
                Err(Thrown)
            } else {
                Ok(storage.clone())
            }
        };
        let throws = r.get("originThrows").as_bool() == Some(true);
        let origin = |u: &JValue| {
            if throws {
                Err(Thrown)
            } else {
                Ok(u.as_str() == Some("https://nc.invalid"))
            }
        };
        let ready = ToolsReady::new(r.get("user"), &get_storage, &origin);
        for a in arr(r.get("asks")) {
            assert_eq!(
                Some(ready.ready(a.get("box"))),
                a.get("ready").as_bool(),
                "ready {i}: {r:?}"
            );
        }
        assert_eq!(
            Some(f64::from(reads.get())),
            r.get("reads").as_num(),
            "storage reads {i}"
        );
    }
    for (i, r) in arr(c.get("route")).iter().enumerate() {
        let capable = strs(r.get("capable"));
        let reject_at = r.get("rejectAt").as_num().unwrap_or(-1.0) as i64;
        let calls: RefCell<Vec<(String, Option<String>)>> = RefCell::new(Vec::new());
        let roles_v = r.get("roles").clone();
        let roles = |role: &str| roles_v.get(role).as_str().map(str::to_owned);
        let hosts = r.get("hosts").clone();
        let host_of = |role: Option<&str>| {
            hosts
                .get(role.unwrap_or("null"))
                .as_str()
                .unwrap_or("")
                .to_owned()
        };
        let allows = |m: String, role: Option<String>| {
            let mut c = calls.borrow_mut();
            let k = c.len() as i64;
            let ok = capable.contains(&format!("{m}@{}", role.as_deref().unwrap_or("null")));
            c.push((m, role));
            async move {
                if k == reject_at {
                    Err(Thrown)
                } else {
                    Ok(ok)
                }
            }
        };
        let routed = r.get("routedRole").as_str();
        let got = block(tm::tool_capable_route(
            r.get("model").as_str().unwrap_or(""),
            routed,
            &roles,
            allows,
            &host_of,
        ));
        let want = if r.get("result").as_str() == Some("__threw") {
            Err(Thrown)
        } else {
            let w = r.get("result");
            Ok(Moved {
                rerouted: w.get("rerouted").as_bool() == Some(true),
                model: w.get("model").as_str().unwrap_or("").to_owned(),
                role: w.get("role").as_str().map(str::to_owned),
            })
        };
        assert_eq!(got, want, "route {i}: {r:?}");
        let want_calls: Vec<(String, Option<String>)> = arr(r.get("calls"))
            .iter()
            .map(|c| {
                (
                    arr(c)[0].as_str().unwrap_or("").to_owned(),
                    arr(c)[1].as_str().map(str::to_owned),
                )
            })
            .collect();
        assert_eq!(*calls.borrow(), want_calls, "route calls {i}");
    }
}

/// Dropping the route at its first and at a fallback probe leaves nothing behind: it holds no state of
/// its own, and a probe it stopped waiting for is never resumed.
#[test]
fn dropping_the_tool_capable_route_at_each_probe_ends_it() {
    for stop_at in 0..3usize {
        let started = Cell::new(0usize);
        let allows = |_: String, _: Option<String>| {
            let k = started.get();
            started.set(k + 1);
            let pend = k == stop_at;
            std::future::poll_fn(move |_| {
                if pend {
                    Poll::Pending
                } else {
                    Poll::Ready(Ok::<bool, Thrown>(false))
                }
            })
        };
        let roles = |r: &str| Some(format!("{r}-m"));
        let host = |_: Option<&str>| String::new();
        {
            let mut f = pin!(tm::tool_capable_route(
                "x-m",
                Some("fast"),
                &roles,
                allows,
                &host
            ));
            let mut cx = Context::from_waker(Waker::noop());
            assert!(
                f.as_mut().poll(&mut cx).is_pending(),
                "pends at probe {stop_at}"
            );
        }
        assert_eq!(started.get(), stop_at + 1, "no probe starts after the drop");
    }
}
