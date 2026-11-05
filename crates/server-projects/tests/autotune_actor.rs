#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use js_json::{wtf16, JValue};
use serde_json::{json, Value};
use server_projects::autotune_actor::{Coverage, Exclusion, Fault, RecoveryEngine, Session};
use server_projects::autotune_state::Owner;
use sha2::{Digest, Sha256};
use std::{
    cell::{Cell, RefCell},
    future::Future,
    pin::pin,
    rc::Rc,
    task::{Context, Poll, Waker},
};

fn poll<F: Future>(f: std::pin::Pin<&mut F>) -> Poll<F::Output> {
    let waker = Waker::noop();
    f.poll(&mut Context::from_waker(waker))
}
fn run<F: Future>(f: F) -> F::Output {
    match poll(pin!(f)) {
        Poll::Ready(x) => x,
        Poll::Pending => panic!("unexpected suspension"),
    }
}
fn v(j: &JValue) -> Value {
    serde_json::from_str(&wtf16::stringify(j).unwrap()).unwrap()
}
fn revision(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[derive(Default)]
struct Gate {
    held: Cell<usize>,
    releases: Cell<usize>,
    acquires: Cell<usize>,
    checks: Cell<usize>,
    lost_at: Cell<usize>,
    pending_at: Cell<usize>,
    busy: Cell<bool>,
    down: Cell<bool>,
    lost: Cell<bool>,
}
struct Guard(Rc<Gate>);
struct GateHandle(Rc<Gate>);
impl std::ops::Deref for GateHandle {
    type Target = Gate;
    fn deref(&self) -> &Gate {
        &self.0
    }
}
impl Drop for Guard {
    fn drop(&mut self) {
        self.0.held.set(self.0.held.get() - 1);
        self.0.releases.set(self.0.releases.get() + 1);
    }
}
impl Coverage for Guard {
    async fn confirm(&self) -> Result<(), Fault> {
        let g = &self.0;
        g.checks.set(g.checks.get() + 1);
        if g.pending_at.get() == g.checks.get() {
            std::future::pending::<()>().await;
        }
        if g.lost.get() || g.lost_at.get() == g.checks.get() {
            g.lost.set(true);
            return Err(Fault::Lost);
        }
        Ok(())
    }
}
impl Exclusion for GateHandle {
    type Guard = Guard;
    async fn acquire(&self) -> Result<Guard, Fault> {
        self.acquires.set(self.acquires.get() + 1);
        if self.down.get() {
            return Err(Fault::Unavailable);
        }
        if self.busy.get() {
            return Err(Fault::Busy);
        }
        self.held.set(self.held.get() + 1);
        Ok(Guard(self.0.clone()))
    }
}
struct Engine {
    text: RefCell<String>,
    effects: RefCell<Vec<String>>,
    input: Value,
    gate: Rc<Gate>,
    calls: Cell<usize>,
    pending_at: Cell<usize>,
    lose_at: Cell<usize>,
}
impl Engine {
    async fn step(&self, effect: Option<&str>) {
        assert_eq!(
            self.gate.held.get(),
            1,
            "engine operation outside reservation"
        );
        assert!(
            !self.gate.lost.get(),
            "engine operation after coverage loss"
        );
        self.calls.set(self.calls.get() + 1);
        if let Some(e) = effect {
            self.effects.borrow_mut().push(e.into());
        }
        if self.lose_at.get() == self.calls.get() {
            self.gate.lost.set(true);
        }
        if self.pending_at.get() == self.calls.get() {
            std::future::pending::<()>().await;
        }
    }
}
impl RecoveryEngine for Engine {
    async fn revision(&self) -> Result<String, Fault> {
        // A pending-revision check is read-only, before any reservation.
        if self.gate.held.get() > 0 {
            self.step(None).await;
        }
        Ok(revision(&self.text.borrow()))
    }
    async fn unload(&self, _: &str, _: bool) -> Result<(), Fault> {
        self.step(Some("unload")).await;
        if self.input["unloadFails"] == true {
            Err(Fault::Engine)
        } else {
            Ok(())
        }
    }
    async fn restore(&self, expected: &str, text: &str, _: bool) -> Result<(), Fault> {
        self.step(Some("restore")).await;
        if revision(&self.text.borrow()) != expected {
            return Err(Fault::Engine);
        }
        *self.text.borrow_mut() = text.into();
        Ok(())
    }
    async fn foreign_live(&self, _: &str) -> Result<bool, Fault> {
        self.step(None).await;
        Ok(self.input["foreign"] == true)
    }
    async fn invalidate_baseline(&self) -> Result<bool, Fault> {
        self.step(Some("invalidate")).await;
        Ok(true)
    }
    async fn reload(&self) -> Result<(), Fault> {
        self.step(Some("reload")).await;
        if self.input["reloadFails"] == true {
            Err(Fault::Engine)
        } else {
            Ok(())
        }
    }
}
fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/autotune-recovery-node.json")).unwrap()
}
fn arrangement(input: &Value) -> (tempfile::TempDir, Owner, JValue, Engine, GateHandle) {
    let dir = tempfile::tempdir().unwrap();
    let base = "native-autotune-0123456789abcdef.json";
    std::fs::write(dir.path().join(base), input["state"].to_string()).unwrap();
    let owner = Owner::new(dir.path());
    let snapshot = owner.attach(base, true).unwrap();
    let gate = Rc::new(Gate::default());
    let engine = Engine {
        input: input.clone(),
        gate: gate.clone(),
        text: RefCell::new(
            input["text"]
                .as_str()
                .unwrap_or("[synthetic]\nctx-size = 8192\n")
                .into(),
        ),
        effects: RefCell::new(vec![]),
        calls: Cell::new(0),
        pending_at: Cell::new(0),
        lose_at: Cell::new(0),
    };
    (dir, owner, snapshot, engine, GateHandle(gate))
}
#[test]
fn recorded_node_recovery_matches_native_state_file_and_engine_effects() {
    for record in fixture()["records"].as_array().unwrap() {
        let input = &record["input"];
        let (dir, owner, snapshot, engine, gate) = arrangement(input);
        let mut session = Session::from_snapshot(&owner, &snapshot).unwrap();
        run(session.recover(&engine, &gate, 123456.0)).unwrap();
        let state: Value = serde_json::from_slice(
            &std::fs::read(dir.path().join("native-autotune-0123456789abcdef.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(state, record["expected"]["state"], "{}", input["name"]);
        assert_eq!(
            *engine.text.borrow(),
            record["expected"]["text"].as_str().unwrap(),
            "{}",
            input["name"]
        );
        assert_eq!(
            json!(*engine.effects.borrow()),
            record["expected"]["effects"],
            "{}",
            input["name"]
        );
        assert_eq!(gate.held.get(), 0);
    }
}
#[test]
fn dropping_at_each_engine_await_releases_reservation_and_leaves_recoverable_intent() {
    let f = fixture();
    let input = &f["records"][2]["input"];
    for pause in 1..=6 {
        let (_dir, owner, snapshot, engine, gate) = arrangement(input);
        engine.pending_at.set(pause);
        let mut session = Session::from_snapshot(&owner, &snapshot).unwrap();
        {
            let mut future = pin!(session.recover(&engine, &gate, 123456.0));
            assert!(poll(future.as_mut()).is_pending(), "{pause}");
        }
        assert_eq!(gate.held.get(), 0, "{pause}");
        assert_eq!(gate.releases.get(), 1, "{pause}");
        if pause >= 3 {
            let state = v(owner.state_snapshot().unwrap().get("state"));
            assert!(
                state["job"]["_pendingRevision"].is_string()
                    || state["job"]["_revision"] == revision("[synthetic]\nctx-size = 4096\n")
            );
        }
    }
}
#[test]
fn dropping_at_each_coverage_await_releases_reservation() {
    let f = fixture();
    let input = &f["records"][2]["input"];
    let (_dir, owner, snapshot, engine, gate) = arrangement(input);
    let mut session = Session::from_snapshot(&owner, &snapshot).unwrap();
    run(session.recover(&engine, &gate, 1.0)).unwrap();
    let checks = gate.checks.get();
    for pause in 1..=checks {
        let (_dir, owner, snapshot, engine, gate) = arrangement(input);
        gate.pending_at.set(pause);
        let mut session = Session::from_snapshot(&owner, &snapshot).unwrap();
        {
            let mut future = pin!(session.recover(&engine, &gate, 1.0));
            assert!(poll(future.as_mut()).is_pending());
        }
        assert_eq!(gate.held.get(), 0);
        assert_eq!(gate.releases.get(), 1);
    }
}
#[test]
fn lost_coverage_is_terminal_and_never_reacquired_or_used_for_cleanup() {
    let f = fixture();
    let input = &f["records"][2]["input"];
    for lose in 1..=6 {
        let (_dir, owner, snapshot, engine, gate) = arrangement(input);
        engine.lose_at.set(lose);
        let mut session = Session::from_snapshot(&owner, &snapshot).unwrap();
        assert_eq!(run(session.recover(&engine, &gate, 1.0)), Err(Fault::Lost));
        let calls = engine.calls.get();
        gate.lost.set(false);
        assert_eq!(run(session.recover(&engine, &gate, 1.0)), Err(Fault::Lost));
        assert_eq!(engine.calls.get(), calls);
        assert_eq!(gate.acquires.get(), 1);
        assert_eq!(gate.held.get(), 0);
    }
}
#[test]
fn unavailable_authority_stays_distinct_from_busy() {
    let f = fixture();
    let input = &f["records"][2]["input"];
    for down in [false, true] {
        let (_dir, owner, snapshot, engine, gate) = arrangement(input);
        gate.down.set(down);
        gate.busy.set(!down);
        let mut session = Session::from_snapshot(&owner, &snapshot).unwrap();
        let error = run(session.recover(&engine, &gate, 1.0)).unwrap_err();
        assert_eq!(error.status(), if down { 503 } else { 409 });
        assert!(engine.effects.borrow().is_empty());
        assert_eq!(gate.held.get(), 0);
    }
}
#[test]
fn newer_writer_and_cancelled_epoch_fence_old_session() {
    let f = fixture();
    let input = &f["records"][2]["input"];
    let (_dir, owner, snapshot, engine, gate) = arrangement(input);
    let mut session = Session::from_snapshot(&owner, &snapshot).unwrap();
    let facts = owner.cancel_facts();
    owner
        .cancel_begin(facts.1.get("token").as_str().unwrap())
        .unwrap();
    assert_eq!(run(session.recover(&engine, &gate, 1.0)), Err(Fault::Lost));
    owner
        .attach("native-autotune-0123456789abcdef.json", true)
        .unwrap();
    assert_eq!(run(session.recover(&engine, &gate, 1.0)), Err(Fault::Lost));
    assert_eq!(gate.acquires.get(), 0);
}

#[test]
fn native_publication_cannot_cross_cancellation_even_when_writer_and_revision_match() {
    let f = fixture();
    let (_dir, owner, snapshot, _engine, _gate) = arrangement(&f["records"][2]["input"]);
    let facts = owner.cancel_facts();
    owner
        .cancel_begin(facts.1.get("token").as_str().unwrap())
        .unwrap();
    let before = owner.state_snapshot().unwrap();
    let ask = JValue::obj([
        ("writer", snapshot.get("writer").clone()),
        ("jobRev", snapshot.get("jobRev").clone()),
        ("nativeEpoch", snapshot.get("epoch").clone()),
        ("history", JValue::Obj(vec![])),
        ("job", JValue::Null),
    ]);
    assert_eq!(owner.commit(&ask).0, 409);
    assert_eq!(v(&before), v(&owner.state_snapshot().unwrap()));
}
