//! chat-pipeline `supervise` against core step-supervision.cjs and vision.cjs (tests/fixtures/record-supervise.cjs).
#![allow(clippy::expect_used, clippy::indexing_slicing, clippy::panic)]
use chat_pipeline::supervise::{self as sv, Probe, ProbeIo, SuperviseIo, VisionProbe};
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
        Poll::Pending => panic!("the oracle's io never waits"),
    }
}

struct Sup<'a> {
    k: &'a JValue,
    aborted: Cell<bool>,
    inputs: RefCell<Vec<JValue>>,
    logs: RefCell<Vec<JValue>>,
    /// Never answers (the cancellation test).
    stuck: bool,
}

impl Sup<'_> {
    fn ms(&self) -> f64 {
        self.k.get("deadline").as_num().unwrap_or(0.0)
    }
}

impl SuperviseIo for Sup<'_> {
    fn enabled(&self) -> bool {
        self.k.get("enabled") == &JValue::Bool(true)
    }
    fn aborted(&self) -> bool {
        self.aborted.get()
    }
    fn deadline_ms(&self) -> JValue {
        self.k.get("deadline").clone()
    }
    async fn decide(&self, input: &JValue) -> Result<Option<JValue>, String> {
        self.inputs.borrow_mut().push(input.clone());
        let behaviour = self.k.get("behaviour").as_str().unwrap_or("");
        if self.stuck {
            std::future::pending::<()>().await;
        }
        if behaviour == "throw" {
            // causeOf(error), as the decision client reports it.
            return Err(arr(self.k.get("logs"))
                .first()
                .and_then(|l| l.get("cause").as_str())
                .unwrap_or("exception")
                .to_owned());
        }
        if self.k.get("abortDuring") == &JValue::Bool(true) {
            self.aborted.set(true);
            std::future::pending::<()>().await;
        }
        // The slow provider answers after 60 ms: only a longer deadline sees it.
        if behaviour == "slow" && self.ms() <= 60.0 {
            std::future::pending::<()>().await;
        }
        let r = self.k.get("result");
        Ok(r.truthy().then(|| r.clone()))
    }
    async fn deadline(&self, _ms: u64) {}
    fn log(&self, entry: JValue) {
        self.logs.borrow_mut().push(entry);
    }
}

#[test]
fn step_supervision_matches_node() {
    let c = wtf16::parse(include_str!("fixtures/supervise-cases.json")).expect("fixture");
    for (i, k) in arr(c.get("supervise")).iter().enumerate() {
        let io = Sup {
            k,
            aborted: Cell::new(k.get("preAborted") == &JValue::Bool(true)),
            inputs: RefCell::new(Vec::new()),
            logs: RefCell::new(Vec::new()),
            stuck: false,
        };
        let msgs = arr(k.get("messages")).to_vec();
        let round = k.get("round").as_num().unwrap_or(0.0) as u32;
        let goal = k.get("goal").as_str();
        let wrote = k.get("wrote") == &JValue::Bool(true);
        let d = if k.get("next") == &JValue::Bool(true) {
            let (messages, pause, d) = block(sv::next_step(Some(&io), round, msgs, goal, wrote));
            let step = k.get("step");
            assert_eq!(
                &JValue::Arr(messages),
                step.get("messages"),
                "step messages {i}"
            );
            assert_eq!(Some(pause), step.get("pause").as_bool(), "pause {i}");
            d.expect("decision")
        } else {
            block(sv::decide(&io, round, &msgs, goal, wrote))
        };
        let want = k.get("decision");
        assert_eq!(
            (Some(d.action), Some(d.source)),
            (want.get("action").as_str(), want.get("source").as_str()),
            "decision {i}: {k:?}"
        );
        assert_eq!(
            &JValue::Arr(io.logs.borrow().clone()),
            k.get("logs"),
            "logs {i}: {k:?}"
        );
        assert_eq!(
            &JValue::Arr(io.inputs.borrow().clone()),
            k.get("inputs"),
            "provider input {i}"
        );
    }
}

/// Dropped while the decision service is asked: nothing is logged and no later decision is taken.
#[test]
fn dropping_supervision_at_the_decision_await_logs_nothing() {
    let k = wtf16::parse(
        r#"{"enabled":true,"deadline":500,"behaviour":"result","result":{"action":"verify"}}"#,
    )
    .expect("case");
    let io = Sup {
        k: &k,
        aborted: Cell::new(false),
        inputs: RefCell::new(Vec::new()),
        logs: RefCell::new(Vec::new()),
        stuck: true,
    };
    struct Never<'a>(&'a Sup<'a>);
    // A deadline that never fires, so the poll stays at the provider's await.
    impl SuperviseIo for Never<'_> {
        fn enabled(&self) -> bool {
            true
        }
        fn aborted(&self) -> bool {
            false
        }
        fn deadline_ms(&self) -> JValue {
            JValue::Num(500.0)
        }
        async fn decide(&self, input: &JValue) -> Result<Option<JValue>, String> {
            self.0.decide(input).await
        }
        async fn deadline(&self, _: u64) {
            std::future::pending::<()>().await;
        }
        fn log(&self, entry: JValue) {
            self.0.log(entry);
        }
    }
    let never = Never(&io);
    let msgs = vec![JValue::obj([
        ("role", JValue::Str("user".into())),
        ("content", JValue::Str("q".into())),
    ])];
    {
        let mut f = pin!(sv::decide(&never, 0, &msgs, None, false));
        let mut cx = Context::from_waker(Waker::noop());
        assert!(f.as_mut().poll(&mut cx).is_pending());
    }
    assert_eq!(io.inputs.borrow().len(), 1, "the service was asked");
    assert!(
        io.logs.borrow().is_empty(),
        "a dropped supervision logged a decision"
    );
}

struct ProbeCase<'a> {
    admit: Option<&'a str>,
    now: Cell<f64>,
    statuses: RefCell<Vec<String>>,
    fetched: RefCell<Vec<JValue>>,
}

impl ProbeIo for ProbeCase<'_> {
    fn now_ms(&self) -> f64 {
        self.now.get()
    }
    async fn admit(&self, _: &str, _: &str) -> Option<Result<(), Option<String>>> {
        match self.admit {
            None => None,
            Some("ok") => Some(Ok(())),
            Some("throw") => Some(Err(Some("Loading is paused.".into()))),
            Some(_) => Some(Err(None)),
        }
    }
    async fn post(&self, url: &str, headers: &JValue, body: &str) -> Result<(u16, String), ()> {
        let mut h = vec![(
            "Content-Type".to_owned(),
            JValue::Str("application/json".into()),
        )];
        if let JValue::Obj(items) = headers {
            for (k, v) in items {
                wtf16::set(&mut h, k, v.clone());
            }
        }
        self.fetched.borrow_mut().push(JValue::obj([
            ("url", JValue::Str(url.into())),
            ("body", JValue::Str(body.into())),
            ("headers", JValue::Obj(h)),
        ]));
        let b = self.statuses.borrow_mut().remove(0);
        match b.as_str() {
            "throw" => Err(()),
            "ok" => Ok((200, "bad".into())),
            "204" => Ok((204, String::new())),
            "mmproj" => Ok((400, "missing mmproj file".into())),
            "Projector" => Ok((400, "No PROJECTOR loaded".into())),
            _ => Ok((400, "bad".into())),
        }
    }
}

#[test]
fn the_vision_probe_matches_node() {
    let c = wtf16::parse(include_str!("fixtures/supervise-cases.json")).expect("fixture");
    for (i, v) in arr(c.get("vision")).iter().enumerate() {
        let probe = VisionProbe::default();
        let io = ProbeCase {
            admit: v.get("admit").as_str(),
            now: Cell::new(0.0),
            statuses: RefCell::new(Vec::new()),
            fetched: RefCell::new(Vec::new()),
        };
        for (k, step) in arr(v.get("steps")).iter().enumerate() {
            io.now.set(step.get("now").as_num().unwrap_or(0.0));
            *io.statuses.borrow_mut() = arr(step.get("status"))
                .iter()
                .map(|s| s.as_str().unwrap_or("").to_owned())
                .collect();
            io.fetched.borrow_mut().clear();
            let a = arr(step.get("args"));
            let got = block(probe.probe(
                &io,
                a[0].as_str().unwrap_or(""),
                &a[1],
                a[2].as_str().unwrap_or(""),
            ));
            let want = step.get("result");
            assert_eq!(
                got,
                Probe {
                    supported: want.get("supported") == &JValue::Bool(true),
                    reason: want.get("reason").as_str().map(str::to_owned)
                },
                "probe {i}/{k}: {v:?}"
            );
            assert_eq!(
                &JValue::Arr(io.fetched.borrow().clone()),
                step.get("fetched"),
                "request {i}/{k}"
            );
        }
    }
}
