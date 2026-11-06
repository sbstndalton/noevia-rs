//! chat-pipeline `machines` against core chat-machines.cjs (#1840/#1849/#1851/#1854/#1880;
//! tests/fixtures/record-machines.cjs).
#![allow(clippy::expect_used, clippy::indexing_slicing, clippy::panic)]
use chat_pipeline::machines::{self as m, ConnectFailure, Facts, Health, MachinesIo, Ready, Target};
use js_json::{wtf16, JValue};
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
        Poll::Pending => panic!("the oracle's machines never wait"),
    }
}

fn st(v: &JValue) -> String {
    v.as_str().unwrap_or("").to_owned()
}

fn target_of(v: &JValue) -> Option<Target> {
    matches!(v, JValue::Obj(_)).then(|| Target {
        role: st(v.get("role")),
        model: st(v.get("model")),
        port: v.get("port").as_num().unwrap_or(0.0) as u16,
        base_url: st(v.get("baseUrl")),
        machine_id: st(v.get("machine").get("id")),
        machine_name: st(v.get("machine").get("name")),
    })
}

fn target_json(t: &Option<Target>, src: &JValue, role: &str) -> JValue {
    // Targets are passed through unchanged: compare by identity of the recorded row.
    match t {
        None => JValue::Null,
        Some(t) => {
            assert_eq!(t.role, role);
            src.get(role).clone()
        }
    }
}

struct Io<'a>(&'a JValue);

impl MachinesIo for Io<'_> {
    fn has_machines(&self) -> bool {
        self.0.get("hasMachines") == &JValue::Bool(true)
    }
    fn chat_target(&self, role: &str) -> Option<Target> {
        if self.0.get("throws") == &JValue::Bool(true) {
            return None;
        }
        target_of(self.0.get("targets").get(role))
    }
    fn has_health(&self) -> bool {
        self.0.get("hasHealth") == &JValue::Bool(true)
    }
    async fn probe(&self, _: &str) -> Result<Health, ()> {
        let h = self.0.get("health");
        if h.as_str() == Some("throw") {
            return Err(());
        }
        Ok(Health { state: h.get("state").as_str().map(str::to_owned), reason: h.get("reason").as_str().map(str::to_owned) })
    }
    async fn facts(&self, _: &Target) -> Facts {
        let models = self.0.get("models");
        let props = self.0.get("props");
        m::load_facts(
            None,
            matches!(models, JValue::Obj(_)).then(|| m::model_ids(models)),
            matches!(props, JValue::Obj(_)).then(|| m::props_facts_of(props)),
        )
    }
}

fn ready_json(r: &Ready) -> JValue {
    match r {
        Ready::Ok => JValue::obj([("ok", JValue::Bool(true))]),
        Ready::Not { state, code, error } => JValue::obj([
            ("ok", JValue::Bool(false)),
            ("state", JValue::Str(state.clone())),
            ("code", JValue::Str((*code).to_owned())),
            ("error", JValue::Str(error.clone())),
        ]),
    }
}

fn failure(e: &JValue) -> ConnectFailure {
    if e.get("code").as_str() == Some("EPUBLICADDR") {
        ConnectFailure::PublicAddress
    } else if e.get("message").as_str().is_some_and(|s| s.to_lowercase().contains("redirect")) {
        ConnectFailure::Redirect
    } else if e.get("name").as_str() == Some("TimeoutError") {
        ConnectFailure::Timeout
    } else {
        ConnectFailure::Other
    }
}

#[test]
fn parsers_match_node() {
    let c = wtf16::parse(include_str!("fixtures/machines-cases.json")).expect("fixture");
    for u in arr(c.get("units").get("models")) {
        let got: Vec<JValue> = m::model_ids(&arr(u)[0]).into_iter().map(JValue::Str).collect();
        assert_eq!(JValue::Arr(got), arr(u)[1], "modelIds {u:?}");
    }
    for u in arr(c.get("units").get("props")) {
        let f = m::props_facts_of(&arr(u)[0]);
        let w = &arr(u)[1];
        assert_eq!(f.context_tokens, w.get("contextTokens").as_num(), "ctx {u:?}");
        assert_eq!(f.thinking, w.get("thinking").as_bool(), "thinking {u:?}");
        assert_eq!(f.vision, w.get("vision").as_bool(), "vision {u:?}");
    }
}

#[test]
fn machines_match_node() {
    let c = wtf16::parse(include_str!("fixtures/machines-cases.json")).expect("fixture");
    for (i, k) in arr(c.get("cases")).iter().enumerate() {
        let io = Io(k);
        let o = k.get("out");
        let ask = st(k.get("ask"));
        let t = m::target(&io, &ask);
        assert_eq!(target_json(&t, k.get("targets"), &ask), *o.get("target"), "target {i}");
        let remote: Vec<String> = m::remote_roles(&io).into_iter().map(|(r, _)| r).collect();
        let want: Vec<String> = match o.get("remote") {
            JValue::Obj(items) => items.iter().map(|(r, _)| r.clone()).collect(),
            _ => Vec::new(),
        };
        assert_eq!(remote, want, "remote {i}");
        assert_eq!(m::effective_roles(&io, k.get("roles")), *o.get("effective"), "effective {i}");
        assert_eq!(m::local_roles(&io, k.get("roles")), *o.get("local"), "local {i}");
        assert_eq!(ready_json(&block(m::ready(&io, t.as_ref()))), *o.get("ready"), "ready {i}: {k:?}");
        if let Some(t) = &t {
            assert_eq!(Some(block(m::context_tokens(&io, t))), o.get("context").as_num(), "context {i}");
            assert_eq!(block(m::measured_provider(&io, t)), *o.get("provider"), "provider {i}");
            assert_eq!(JValue::Str(m::connect_error(t, failure(k.get("err")))), *o.get("connect"), "connect {i}");
        }
        let reads = std::cell::Cell::new(0);
        let manual = block(m::manual_target(&io, k.get("manual"), || {
            reads.set(reads.get() + 1);
            let cat = k.get("catalogue").clone();
            async move { if cat.as_str() == Some("throw") { Err(()) } else { Ok(cat) } }
        }));
        let want = o.get("manual");
        assert_eq!(manual.as_ref().map(|t| t.machine_id.clone()), want.get("machine").get("id").as_str().map(str::to_owned), "manual {i}: {k:?}");
        assert_eq!(manual.map(|t| t.model), want.get("model").as_str().map(str::to_owned), "manual model {i}");
        assert_eq!(Some(f64::from(reads.get())), o.get("catalogueReads").as_num(), "catalogue reads {i}");
    }
}

/// Cancellation: `ready` dropped at the probe await settles nothing, and a turn guard is released on drop.
#[test]
fn ready_dropped_at_the_probe_and_turn_guards_release() {
    struct Hang;
    impl MachinesIo for Hang {
        fn has_machines(&self) -> bool {
            true
        }
        fn chat_target(&self, _: &str) -> Option<Target> {
            None
        }
        fn has_health(&self) -> bool {
            true
        }
        async fn probe(&self, _: &str) -> Result<Health, ()> {
            std::future::pending().await
        }
        async fn facts(&self, _: &Target) -> Facts {
            panic!("facts read after the probe was dropped")
        }
    }
    let t = Target { role: "smart".into(), model: "m".into(), port: 1, base_url: "http://x".into(), machine_id: "a".into(), machine_name: "A".into() };
    {
        let mut f = pin!(m::ready(&Hang, Some(&t)));
        assert!(f.as_mut().poll(&mut Context::from_waker(Waker::noop())).is_pending());
    }
    let turns = m::Turns::default();
    let g1 = turns.begin("a");
    let g2 = turns.begin("a");
    assert!(turns.busy("a") && !turns.busy("b"));
    drop(g1);
    assert!(turns.busy("a"));
    {
        let fut = async move {
            let _g = g2;
            std::future::pending::<()>().await;
        };
        let mut f = Box::pin(fut);
        assert!(f.as_mut().poll(&mut Context::from_waker(Waker::noop())).is_pending());
    }
    assert!(!turns.busy("a"), "a turn cancelled at an await releases its machine");
}
