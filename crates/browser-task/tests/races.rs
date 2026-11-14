//! What the recording cannot show: actions after a cancel, an engine call that never returns, a close
//! that never confirms, a panicking engine. A small engine with switches stands in for the browser;
//! synthetic pages only.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use browser_task::engine::{
    Browser, CloseHook, Context, DownloadHook, EngineError, Handle, Launch, Page, PageHook, Proxy,
    Result, RouteGate, SocketGate,
};
use browser_task::service::{Options, Service};
use browser_task::Fail;
use js_json::JValue;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const PROJECT: &str = "qa-project";

#[derive(Default)]
struct Knobs {
    /// A click that never returns (until the test ends).
    hang_click: AtomicBool,
    clicking: AtomicBool,
    /// A context close that never returns.
    hang_close: AtomicBool,
    /// describe() panics.
    panic_describe: AtomicBool,
}

struct Engine(Arc<Knobs>);
struct B(Arc<Knobs>);
struct C(Arc<Knobs>, Mutex<Option<PageHook>>);
struct P(Arc<Knobs>, Mutex<String>);
struct H(Arc<Knobs>);

impl Launch for Engine {
    fn launch(&self) -> std::result::Result<Arc<dyn Browser>, Fail> {
        Ok(Arc::new(B(self.0.clone())))
    }
}
impl Browser for B {
    fn new_context(&self, _proxy: Option<&Proxy>) -> Result<Arc<dyn Context>> {
        Ok(Arc::new(C(self.0.clone(), Mutex::new(None))))
    }
    fn close(&self) -> Result<()> {
        Ok(())
    }
}
impl Context for C {
    fn route(&self, _gate: RouteGate) -> Result<()> {
        Ok(())
    }
    fn route_websocket(&self, _gate: SocketGate) -> Result<()> {
        Ok(())
    }
    fn on_page(&self, hook: PageHook) {
        *self.1.lock().unwrap() = Some(hook);
    }
    fn new_page(&self) -> Result<Arc<dyn Page>> {
        let page: Arc<dyn Page> = Arc::new(P(self.0.clone(), Mutex::new("about:blank".into())));
        if let Some(h) = self.1.lock().unwrap().clone() {
            h(page.clone());
        }
        Ok(page)
    }
    fn pages(&self) -> Vec<Arc<dyn Page>> {
        Vec::new()
    }
    fn close(&self) -> Result<()> {
        while self.0.hang_close.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(10));
        }
        Ok(())
    }
}
impl Page for P {
    fn id(&self) -> u64 {
        1
    }
    fn on_download(&self, _hook: DownloadHook) {}
    fn on_close(&self, _hook: CloseHook) {}
    fn is_closed(&self) -> bool {
        false
    }
    fn url(&self) -> String {
        self.1.lock().unwrap().clone()
    }
    fn locate(&self, _selector: &str, _timeout: Duration) -> Result<Option<Arc<dyn Handle>>> {
        Ok(Some(Arc::new(H(self.0.clone()))))
    }
    fn active_element(&self) -> Result<Option<Arc<dyn Handle>>> {
        Ok(Some(Arc::new(H(self.0.clone()))))
    }
    fn locator_text(&self, _s: &str, _t: Duration) -> Result<String> {
        Ok("QA".into())
    }
    fn body_text(&self, _t: Duration) -> Result<String> {
        Ok("QA body".into())
    }
    fn goto(&self, url: &str, _t: Duration) -> Result<()> {
        *self.1.lock().unwrap() = url.to_string();
        Ok(())
    }
    fn go_back(&self, _t: Duration) -> Result<()> {
        Ok(())
    }
    fn keyboard_press(&self, _key: &str) -> Result<()> {
        Ok(())
    }
    fn wheel(&self, _dy: f64) -> Result<()> {
        Ok(())
    }
    fn wait_for_timeout(&self, _ms: f64) -> Result<()> {
        Ok(())
    }
    fn screenshot(&self) -> Result<Vec<u8>> {
        Ok(b"png".to_vec())
    }
    fn wait_for_load_state(&self, _t: Duration) -> Result<()> {
        Ok(())
    }
}
impl Handle for H {
    fn describe(&self) -> Result<JValue> {
        assert!(
            !self.0.panic_describe.load(Ordering::SeqCst),
            "QA engine panic"
        );
        Ok(js_json::parse(r#"{"tag":"button","type":"button","role":"","name":"","text":"Show more","value":"","inForm":false,"formMethod":"","formAction":"","frameOrigin":"https://shop.example.test"}"#).unwrap())
    }
    fn is_connected(&self) -> Result<bool> {
        Ok(true)
    }
    fn is_focused(&self) -> Result<bool> {
        Ok(true)
    }
    fn click(&self, _t: Duration) -> Result<()> {
        self.0.clicking.store(true, Ordering::SeqCst);
        while self.0.hang_click.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(10));
        }
        Ok(())
    }
    fn hover(&self, _t: Duration) -> Result<()> {
        Ok(())
    }
    fn fill(&self, _v: &str, _t: Duration) -> Result<()> {
        Ok(())
    }
    fn select_option(&self, _v: &str, _t: Duration) -> Result<()> {
        Ok(())
    }
    fn press(&self, _k: &str, _t: Duration) -> Result<()> {
        Ok(())
    }
    fn set_input_files(&self, _f: &Path, _t: Duration) -> Result<()> {
        Err(EngineError::new("no files"))
    }
    fn submit(&self) -> Result<()> {
        Ok(())
    }
    fn dispose(&self) -> Result<()> {
        Ok(())
    }
}

struct Rig {
    _dir: tempfile::TempDir,
    base: std::path::PathBuf,
    knobs: Arc<Knobs>,
    service: Arc<Service>,
}

fn rig(close_timeout: Duration) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let knobs = Arc::new(Knobs::default());
    let mut opts = Options::new(Arc::new(Engine(knobs.clone())), None, Arc::new(|_| {}));
    opts.direct = true;
    opts.close_timeout = close_timeout;
    opts.timeout = Duration::from_secs(30);
    Rig {
        _dir: dir,
        base,
        knobs,
        service: Service::new(opts),
    }
}

fn obj(text: &str) -> JValue {
    js_json::parse(text).unwrap()
}

impl Rig {
    fn start(&self) -> String {
        let v = self
            .service
            .start(
                &self.base,
                PROJECT,
                &obj(r#"{"domains":["shop.example.test"]}"#),
            )
            .unwrap();
        v.get("taskId").as_str().unwrap().to_string()
    }
    fn status(&self, task: &str) -> String {
        self.service
            .get(&self.base, PROJECT, task)
            .unwrap()
            .get("status")
            .as_str()
            .unwrap_or("")
            .to_string()
    }
    fn until(&self, task: &str, want: &str) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while self.status(task) != want {
            assert!(
                Instant::now() < deadline,
                "{task} never became {want}: {}",
                self.status(task)
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    fn journal(&self, task: &str) -> String {
        std::fs::read_to_string(self.base.join("jobs").join(format!("{task}.jsonl"))).unwrap()
    }
}

#[test]
fn no_action_is_taken_after_a_cancel() {
    let r = rig(Duration::from_secs(5));
    let t = r.start();
    let nav = obj(r#"{"type":"navigate","url":"https://shop.example.test/"}"#);
    assert_eq!(
        r.service
            .act(&r.base, PROJECT, &t, &nav)
            .unwrap()
            .get("status")
            .as_str(),
        Some("done")
    );
    let view = r.service.cancel(&r.base, PROJECT, &t).unwrap();
    assert_eq!(
        view.get("status").as_str(),
        Some("running"),
        "the view before the cleanup, as core's"
    );
    // Whatever the cleanup's progress, the task takes nothing more.
    let answer = r.service.act(&r.base, PROJECT, &t, &nav);
    assert!(
        matches!(&answer, Err(f) if f.status == Some(409)),
        "{answer:?}"
    );
    let finish = r.service.finish(&r.base, PROJECT, &t, &JValue::Null);
    assert!(
        matches!(&finish, Err(f) if f.status == Some(409)),
        "{finish:?}"
    );
    r.until(&t, "cancelled");
}

#[test]
fn a_cancel_ends_the_task_even_when_an_engine_call_never_returns() {
    let r = rig(Duration::from_millis(300));
    let t = r.start();
    r.knobs.hang_click.store(true, Ordering::SeqCst);
    let (service, base, task) = (r.service.clone(), r.base.clone(), t.clone());
    let acting = std::thread::spawn(move || {
        service.act(
            &base,
            PROJECT,
            &task,
            &obj(r#"{"type":"click","selector":".more"}"#),
        )
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    while !r.knobs.clicking.load(Ordering::SeqCst) {
        assert!(Instant::now() < deadline, "the click never started");
        std::thread::sleep(Duration::from_millis(10));
    }
    r.service.cancel(&r.base, PROJECT, &t).unwrap();
    let answer = acting.join().unwrap();
    assert!(
        matches!(&answer, Err(f) if f.status == Some(499)),
        "{answer:?}"
    );
    r.until(&t, "cancelled");
    let journal = r.journal(&t);
    assert!(
        journal.contains("The action was still pending when cancellation was accepted."),
        "{journal}"
    );
    // A new task can start: the hung one no longer counts as running.
    r.knobs.hang_click.store(false, Ordering::SeqCst);
    let again = r.start();
    assert_ne!(again, t);
}

#[test]
fn a_close_that_never_confirms_fails_the_task_and_says_so() {
    let r = rig(Duration::from_millis(200));
    let t = r.start();
    r.knobs.hang_close.store(true, Ordering::SeqCst);
    r.service
        .finish(&r.base, PROJECT, &t, &obj(r#"{"summary":"QA"}"#))
        .unwrap();
    r.until(&t, "failed");
    let view = r.service.get(&r.base, PROJECT, &t).unwrap();
    assert_eq!(
        view.get("error").as_str(),
        Some("Browser cleanup could not be confirmed.")
    );
    assert!(r
        .journal(&t)
        .contains("Browser executor closure could not be confirmed."));
    r.knobs.hang_close.store(false, Ordering::SeqCst);
}

#[test]
fn a_panicking_engine_answers_the_action_and_leaves_the_task_usable() {
    let r = rig(Duration::from_secs(5));
    let t = r.start();
    r.knobs.panic_describe.store(true, Ordering::SeqCst);
    let click = obj(r#"{"type":"click","selector":".more"}"#);
    let answer = r.service.act(&r.base, PROJECT, &t, &click);
    assert!(
        matches!(&answer, Err(f) if f.status.is_none()),
        "{answer:?}"
    );
    r.knobs.panic_describe.store(false, Ordering::SeqCst);
    let nav = obj(r#"{"type":"navigate","url":"https://shop.example.test/"}"#);
    assert_eq!(
        r.service
            .act(&r.base, PROJECT, &t, &nav)
            .unwrap()
            .get("status")
            .as_str(),
        Some("done")
    );
    r.service
        .finish(&r.base, PROJECT, &t, &JValue::Null)
        .unwrap();
    r.until(&t, "completed");
}

#[test]
fn a_card_cannot_be_answered_after_the_cancel() {
    let r = rig(Duration::from_secs(5));
    let t = r.start();
    // A navigation the policy sends to a card: a submit in a form (here: a press of Enter in a form).
    let (service, base, task) = (r.service.clone(), r.base.clone(), t.clone());
    let acting = std::thread::spawn(move || {
        service.act(
            &base,
            PROJECT,
            &task,
            &obj(r#"{"type":"submit","selector":".more"}"#),
        )
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    let card = loop {
        let card = r.service.approval(&t);
        if !card.is_nullish() {
            break card;
        }
        assert!(Instant::now() < deadline, "no card");
        std::thread::sleep(Duration::from_millis(10));
    };
    let id = card.get("id").as_str().unwrap().to_string();
    r.service.cancel(&r.base, PROJECT, &t).unwrap();
    let late = r.service.decide(&r.base, PROJECT, &t, "approve", &id);
    assert!(
        matches!(&late, Err(f) if f.status == Some(409) && f.message == "That approval is no longer waiting."),
        "{late:?}"
    );
    let answer = acting.join().unwrap();
    let declined = matches!(&answer, Ok(v) if v.get("reason").as_str() == Some("Declined."));
    let cancelled = matches!(&answer, Err(f) if f.status == Some(499));
    assert!(declined || cancelled, "{answer:?}");
    r.until(&t, "cancelled");
}

/// sbstndalton/noevia#2698: at most QUEUE_CAP actions wait behind a running one (each waiter holds a
/// thread here); the next is refused and runs nothing; a finish is still taken; a cancel answers all.
#[test]
fn a_full_queue_refuses_more_actions_but_takes_a_finish() {
    use browser_task::service::QUEUE_CAP;
    let r = rig(Duration::from_millis(300));
    let t = r.start();
    r.knobs.hang_click.store(true, Ordering::SeqCst);
    let click = obj(r#"{"type":"click","selector":".more"}"#);
    let spawn = |r: &Rig| {
        let (service, base, task, click) =
            (r.service.clone(), r.base.clone(), t.clone(), click.clone());
        std::thread::spawn(move || service.act(&base, PROJECT, &task, &click))
    };
    let first = spawn(&r);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !r.knobs.clicking.load(Ordering::SeqCst) {
        assert!(Instant::now() < deadline, "the click never started");
        std::thread::sleep(Duration::from_millis(10));
    }
    let extra = 3;
    let waiting: Vec<_> = (0..QUEUE_CAP + extra).map(|_| spawn(&r)).collect();
    // The refused ones answer at once; the queued ones wait for the cancel.
    let deadline = Instant::now() + Duration::from_secs(10);
    while waiting.iter().filter(|h| h.is_finished()).count() < extra {
        assert!(Instant::now() < deadline, "no action was refused");
        std::thread::sleep(Duration::from_millis(10));
    }
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(waiting.iter().filter(|h| h.is_finished()).count(), extra);
    assert!(r
        .service
        .finish(&r.base, PROJECT, &t, &JValue::Null)
        .is_ok());
    r.service.cancel(&r.base, PROJECT, &t).unwrap();
    // Every waiter answers soon after the cancel (a hang fails the test instead of stalling it).
    let deadline = Instant::now() + Duration::from_secs(20);
    while !(first.is_finished() && waiting.iter().all(std::thread::JoinHandle::is_finished)) {
        assert!(Instant::now() < deadline, "a waiting action was never answered");
        std::thread::sleep(Duration::from_millis(10));
    }
    let mut refused = 0;
    for h in waiting {
        match h.join().unwrap() {
            Err(f) if f.message.starts_with("Too many actions") => {
                assert_eq!(f.status, Some(409));
                refused += 1;
            }
            Err(f) => assert!(matches!(f.status, Some(409 | 499)), "{f:?}"),
            Ok(v) => panic!("a queued action ran after the cancel: {v:?}"),
        }
    }
    assert_eq!(refused, extra);
    assert!(first.join().unwrap().is_err());
    r.until(&t, "cancelled");
}
