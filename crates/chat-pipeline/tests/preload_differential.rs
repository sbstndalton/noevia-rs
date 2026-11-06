//! chat-pipeline `preload` against core route-preload.cjs (#1863; tests/fixtures/record-preload.cjs).
#![allow(clippy::expect_used, clippy::indexing_slicing, clippy::panic)]
use chat_pipeline::preload::{self as pl, Engine, LoadTask, Orphan, Roles};
use js_json::{wtf16, JValue};
use std::cell::{Cell, RefCell};
use std::future::Future;
use std::pin::pin;
use std::rc::Rc;
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

#[derive(Clone)]
struct Done(bool);

impl LoadTask for Done {
    fn cancel(&self) {}
    async fn settled(&self) -> bool {
        self.0
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
    type Load = Done;
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
    fn begin_load(&self, _: &str) -> Done {
        self.calls.borrow_mut().push(JValue::Str("load".into()));
        Done(self.k.get("load").as_str() == Some("ok"))
    }
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
    fn adopt(&self, _: Orphan<Done>) {
        panic!("the oracle never drops a release");
    }
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
            let settled = block(pre.run());
            assert_eq!(Some(settled), k.get("settled").as_bool(), "settled {i}");
            if k.get("release") == &JValue::Bool(true) {
                let idle = k.get("idle") == &JValue::Bool(true);
                let released = block(pre.release_if_unused(&|| idle));
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

/// A load the test settles by hand; its cancel answers at once but the load stays outstanding.
#[derive(Clone)]
struct Held(Rc<Cell<Option<bool>>>, Rc<RefCell<Vec<&'static str>>>);

impl LoadTask for Held {
    fn cancel(&self) {
        self.1.borrow_mut().push("cancel");
    }
    async fn settled(&self) -> bool {
        std::future::poll_fn(|_| match self.0.get() {
            Some(v) => Poll::Ready(v),
            None => Poll::Pending,
        })
        .await
    }
}

struct Ctl {
    ev: Rc<RefCell<Vec<&'static str>>>,
    outcome: Rc<Cell<Option<bool>>>,
    hold: Cell<Option<&'static str>>,
    adopted: RefCell<Vec<Orphan<Held>>>,
    listed: Cell<u32>,
}

impl Ctl {
    fn new() -> Self {
        Ctl {
            ev: Rc::default(),
            outcome: Rc::default(),
            hold: Cell::new(None),
            adopted: RefCell::default(),
            listed: Cell::new(0),
        }
    }
    async fn at(&self, step: &'static str) {
        std::future::poll_fn(|_| {
            if self.hold.get() == Some(step) {
                Poll::Pending
            } else {
                Poll::Ready(())
            }
        })
        .await;
        self.ev.borrow_mut().push(step);
    }
    fn settle(&self) {
        self.ev.borrow_mut().push("settled");
        self.outcome.set(Some(true));
    }
    fn count(&self, e: &str) -> usize {
        self.ev.borrow().iter().filter(|x| **x == e).count()
    }
    fn index(&self, e: &str) -> Option<usize> {
        self.ev.borrow().iter().position(|x| *x == e)
    }
}

impl Engine for Ctl {
    type Load = Held;
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
        let first = self.listed.get() == 0;
        self.listed.set(self.listed.get() + 1);
        if !first {
            self.at("list").await;
        }
        let st = if first { "unloaded" } else { "loaded" };
        wtf16::parse(&format!(r#"[{{"id":"m-a","status":{{"value":"{st}"}}}}]"#))
            .ok()
            .map(|v| arr(&v).to_vec())
    }
    fn begin_load(&self, _: &str) -> Held {
        self.ev.borrow_mut().push("load");
        Held(Rc::clone(&self.outcome), Rc::clone(&self.ev))
    }
    async fn unload(&self, _: &str) -> Option<bool> {
        self.at("unload").await;
        Some(true)
    }
    async fn sleep(&self) {
        self.at("sleep").await;
    }
    fn past_wait(&self) -> bool {
        false
    }
    fn log(&self, _: String) {}
    fn adopt(&self, orphan: Orphan<Held>) {
        self.adopted.borrow_mut().push(orphan);
    }
}

fn poll<F: Future + ?Sized>(f: std::pin::Pin<&mut F>) -> Poll<F::Output> {
    f.poll(&mut Context::from_waker(Waker::noop()))
}

/// Two `run` waiters and a release at once share the one load; nothing is unloaded before it settles.
#[test]
fn concurrent_runs_and_release_share_one_load() {
    let e = Ctl::new();
    let pre = block(pl::start(&e, "m-a")).expect("started");
    let mut a = pin!(pre.run());
    let mut b = pin!(pre.run());
    let mut r = pin!(pre.release_if_unused(&|| true));
    assert!(poll(a.as_mut()).is_pending());
    assert!(poll(b.as_mut()).is_pending());
    assert!(
        poll(r.as_mut()).is_pending(),
        "the cancel answered but the load is outstanding"
    );
    assert!(poll(r.as_mut()).is_pending());
    assert_eq!(e.count("load"), 1);
    assert_eq!(e.count("unload"), 0);
    assert_eq!(e.count("list"), 0, "nothing read before settlement");
    e.settle();
    assert_eq!(poll(r.as_mut()), Poll::Ready(true));
    assert_eq!(poll(a.as_mut()), Poll::Ready(true));
    assert_eq!(poll(b.as_mut()), Poll::Ready(true));
    assert_eq!(e.count("load"), 1);
    assert_eq!(e.count("unload"), 1);
    assert!(e.index("settled") < e.index("unload"));
    assert!(e.adopted.borrow().is_empty());
}

/// A release dropped at each of its awaits hands its cleanup to the settle owner, which finishes it after the
/// one load settles: no second load, no unload before settlement.
#[test]
fn a_release_dropped_at_any_await_is_finished_by_the_settle_owner() {
    for step in ["settled", "sleep", "list", "unload"] {
        let e = Ctl::new();
        let pre = block(pl::start(&e, "m-a")).expect("started");
        if step != "settled" {
            e.settle();
            e.hold.set(Some(step));
        }
        {
            let mut r = pin!(pre.release_if_unused(&|| true));
            assert!(poll(r.as_mut()).is_pending(), "{step}");
        }
        assert_eq!(e.adopted.borrow().len(), 1, "{step}: cleanup adopted");
        assert!(
            !block(pre.release_if_unused(&|| true)),
            "{step}: retry defers to the owner"
        );
        let orphan = e.adopted.borrow_mut().pop().expect("orphan");
        let mut o = pin!(pl::settle_orphan(&e, orphan, &|| true));
        if step == "settled" {
            assert!(poll(o.as_mut()).is_pending());
            assert_eq!(
                e.count("unload"),
                0,
                "nothing unloaded before the load settles"
            );
            e.settle();
        }
        e.hold.set(None);
        assert_eq!(poll(o.as_mut()), Poll::Ready(true), "{step}");
        assert_eq!(e.count("load"), 1, "{step}: one load");
        assert_eq!(e.count("unload"), 1, "{step}: cleaned up");
        assert!(e.index("settled") < e.index("unload"), "{step}");
    }
}

/// A pre-load the turn used is never unloaded by an adopted release.
#[test]
fn an_orphan_of_a_used_preload_unloads_nothing() {
    let e = Ctl::new();
    let pre = block(pl::start(&e, "m-a")).expect("started");
    {
        let mut r = pin!(pre.release_if_unused(&|| true));
        assert!(poll(r.as_mut()).is_pending());
    }
    pre.use_it();
    e.settle();
    let orphan = e.adopted.borrow_mut().pop().expect("orphan");
    assert!(!block(pl::settle_orphan(&e, orphan, &|| true)));
    assert_eq!(e.count("unload"), 0);
}
