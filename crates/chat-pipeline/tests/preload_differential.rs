//! chat-pipeline `preload` against core route-preload.cjs (#1863; tests/fixtures/record-preload.cjs).
#![allow(clippy::expect_used, clippy::indexing_slicing, clippy::panic)]
use chat_pipeline::preload::{self as pl, Engine, Roles};
use js_json::{wtf16, JValue};
use std::cell::{Cell, RefCell};
use std::future::Future;
use std::pin::pin;
use std::task::{Context, Poll, Waker};

fn arr(v: &JValue) -> &[JValue] {
    match v {
        JValue::Arr(a) => a,
        _ => &[],
    }
}

fn block<F: Future>(f: F) -> F::Output {
    let mut f = pin!(f);
    let mut cx = Context::from_waker(Waker::noop());
    match f.as_mut().poll(&mut cx) {
        Poll::Ready(v) => v,
        Poll::Pending => panic!("the oracle's engine never waits"),
    }
}

fn s(v: &JValue) -> Option<String> {
    v.as_str().map(str::to_owned)
}

#[test]
fn preload_targets_match_node() {
    let c = wtf16::parse(include_str!("fixtures/preload-cases.json")).expect("fixture");
    for (i, t) in arr(c.get("targets")).iter().enumerate() {
        let r = t.get("roles");
        let roles = matches!(r, JValue::Obj(_)).then(|| Roles {
            fast: s(r.get("fast")),
            smart: s(r.get("smart")),
            code: s(r.get("code")),
            creative: s(r.get("creative")),
        });
        let remote: Vec<String> = arr(t.get("remote")).iter().filter_map(s).collect();
        let asked = Cell::new(0);
        let likely = t.get("likely") == &JValue::Bool(true);
        let got = pl::preload_target(
            roles.as_ref(),
            &|role| remote.iter().any(|x| x == role),
            t.get("hasImages") == &JValue::Bool(true),
            t.get("contextHigh") == &JValue::Bool(true),
            &|| {
                asked.set(asked.get() + 1);
                likely
            },
        );
        assert_eq!(got, s(t.get("target")), "target {i}: {t:?}");
        assert_eq!(
            Some(f64::from(asked.get())),
            t.get("asked").as_num(),
            "likelyCode asked {i}"
        );
    }
}

struct Fake<'a> {
    k: &'a JValue,
    calls: RefCell<Vec<JValue>>,
    lists: RefCell<Vec<JValue>>,
    busy_n: Cell<i64>,
    aborted: Cell<bool>,
}

impl Engine for Fake<'_> {
    fn can_preload(&self) -> bool {
        self.k.get("kind").as_str() == Some("llamacpp")
    }
    fn busy(&self) -> bool {
        let n = self.busy_n.get();
        self.busy_n.set(n + 1);
        if Some(n as f64) == self.k.get("abortedAt").as_num() {
            self.aborted.set(true);
        }
        Some(n as f64) == self.k.get("busyAt").as_num()
    }
    fn aborted(&self) -> bool {
        self.aborted.get()
    }
    async fn list_models(&self) -> Option<Vec<JValue>> {
        let l = self.lists.borrow_mut().remove(0);
        self.calls
            .borrow_mut()
            .push(JValue::obj([("list", l.clone())]));
        match (l.get("ok"), l.get("body").get("data")) {
            (JValue::Bool(true), JValue::Arr(rows)) => Some(rows.clone()),
            _ => None,
        }
    }
    async fn load(&self, _: &str) -> bool {
        self.calls.borrow_mut().push(JValue::Str("load".into()));
        self.k.get("load").as_str() == Some("ok")
    }
    fn cancel_load(&self) {}
    async fn unload(&self, _: &str) -> Option<bool> {
        match self.k.get("unload").as_str() {
            Some("none") => None,
            u => {
                self.calls.borrow_mut().push(JValue::Str("unload".into()));
                Some(u == Some("ok"))
            }
        }
    }
    async fn sleep(&self) {
        self.calls.borrow_mut().push(JValue::Str("sleep".into()));
    }
    fn past_wait(&self) -> bool {
        false
    }
    fn log(&self, _: String) {}
}

#[test]
fn preload_starts_and_releases_match_node() {
    let c = wtf16::parse(include_str!("fixtures/preload-cases.json")).expect("fixture");
    for (i, k) in arr(c.get("starts")).iter().enumerate() {
        let lists: Vec<JValue> = arr(k.get("calls"))
            .iter()
            .filter_map(|c| match c.get("list") {
                JValue::Undefined => None,
                l => Some(l.clone()),
            })
            .collect();
        let e = Fake {
            k,
            calls: RefCell::new(Vec::new()),
            lists: RefCell::new(lists),
            busy_n: Cell::new(0),
            aborted: Cell::new(false),
        };
        let pre = block(pl::start(&e, k.get("model").as_str().unwrap_or("")));
        assert_eq!(
            pre.is_some(),
            k.get("started") == &JValue::Bool(true),
            "started {i}: {k:?}"
        );
        if let Some(pre) = pre {
            if k.get("used") == &JValue::Bool(true) {
                pre.use_it();
            }
            let settled = block(pre.run(&e));
            assert_eq!(Some(settled), k.get("settled").as_bool(), "settled {i}");
            if k.get("release") == &JValue::Bool(true) {
                let idle = k.get("idle") == &JValue::Bool(true);
                let released = block(pre.release_if_unused(&e, &|| idle));
                assert_eq!(
                    Some(released),
                    k.get("released").as_bool(),
                    "released {i}: {k:?}"
                );
            }
        }
        assert_eq!(
            &JValue::Arr(e.calls.borrow().clone()),
            k.get("calls"),
            "calls {i}: {k:?}"
        );
    }
}

/// Exclusion-settle: a release dropped while it waits for the cancelled load reads nothing and unloads nothing.
#[test]
fn a_release_dropped_before_the_load_settled_unloads_nothing() {
    struct Pending(RefCell<Vec<&'static str>>);
    impl Engine for Pending {
        fn can_preload(&self) -> bool {
            true
        }
        fn busy(&self) -> bool {
            false
        }
        fn aborted(&self) -> bool {
            false
        }
        async fn list_models(&self) -> Option<Vec<JValue>> {
            self.0.borrow_mut().push("list");
            wtf16::parse(r#"[{"id":"m-a","status":{"value":"unloaded"}}]"#)
                .ok()
                .map(|v| arr(&v).to_vec())
        }
        async fn load(&self, _: &str) -> bool {
            self.0.borrow_mut().push("load");
            std::future::pending::<()>().await;
            true
        }
        fn cancel_load(&self) {
            self.0.borrow_mut().push("cancel");
        }
        async fn unload(&self, _: &str) -> Option<bool> {
            self.0.borrow_mut().push("unload");
            Some(true)
        }
        async fn sleep(&self) {}
        fn past_wait(&self) -> bool {
            false
        }
        fn log(&self, _: String) {}
    }
    let e = Pending(RefCell::new(Vec::new()));
    let pre = block(pl::start(&e, "m-a")).expect("started");
    {
        let mut f = pin!(pre.release_if_unused(&e, &|| true));
        let mut cx = Context::from_waker(Waker::noop());
        assert!(f.as_mut().poll(&mut cx).is_pending());
    }
    assert_eq!(*e.0.borrow(), vec!["list", "cancel", "load"]);
    assert!(pre.pending(), "the load never settled");
}
