#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
use embedding_worker::{
    service::{self, Loader, Service},
    *,
};
use serde_json::{json, Value};
use std::{
    collections::{BTreeSet, VecDeque},
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex,
    },
};
fn target(model: &str) -> Target {
    Target {
        profile: "nomic-v1".into(),
        model: model.into(),
        base_url: None,
    }
}
struct Stub {
    config: Mutex<Config>,
    now: AtomicU64,
    served: Mutex<VecDeque<Result<Vec<String>, String>>>,
    batches: Mutex<VecDeque<Result<Option<Batch>, String>>>,
    calls: Mutex<Vec<(Index, Target, BTreeSet<i64>)>>,
    drops: Mutex<Vec<(Index, Target)>>,
    sleeps: Mutex<Vec<u64>>,
    control: Mutex<Option<Control>>,
    bump: bool,
}
impl Stub {
    fn new() -> Self {
        Self {
            config: Mutex::new(Config {
                version: 0,
                active: target("old"),
                next: Some(target("new")),
                previous: None,
            }),
            now: AtomicU64::new(0),
            served: Mutex::new(VecDeque::new()),
            batches: Mutex::new(VecDeque::new()),
            calls: Mutex::new(vec![]),
            drops: Mutex::new(vec![]),
            sleeps: Mutex::new(vec![]),
            control: Mutex::new(None),
            bump: false,
        }
    }
    fn batch(&self, embedded: usize, refused: Vec<i64>, remaining: usize) {
        self.batches.lock().unwrap().push_back(Ok(Some(Batch {
            embedded,
            attempted: 4,
            refused,
            remaining,
        })));
    }
}
impl Host for Stub {
    fn config(&self) -> Config {
        self.config.lock().unwrap().clone()
    }
    fn now_ms(&self) -> u64 {
        self.now.load(Ordering::Relaxed)
    }
    fn indexes(&self) -> Vec<Index> {
        vec![
            Index {
                user: Some("synthetic-a".into()),
                project: "project".into(),
            },
            Index {
                user: Some("synthetic-b".into()),
                project: "project".into(),
            },
        ]
    }
    fn served<'a>(&'a self, _: &'a Target) -> Task<'a, Result<Vec<String>, String>> {
        Box::pin(async {
            self.served
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Ok(vec!["new".into()]))
        })
    }
    fn fill<'a>(
        &'a self,
        index: &'a Index,
        t: &'a Target,
        limit: usize,
        exclude: &'a BTreeSet<i64>,
        version: u64,
        control: &'a Control,
    ) -> Task<'a, Result<Option<Batch>, String>> {
        Box::pin(async move {
            assert_eq!(limit, 4);
            assert_eq!(version, self.config().version);
            assert!(!control.stopped());
            self.calls
                .lock()
                .unwrap()
                .push((index.clone(), t.clone(), exclude.clone()));
            self.now.fetch_add(100, Ordering::Relaxed);
            if self.bump {
                self.config.lock().unwrap().version += 1;
            }
            if let Some(control) = self.control.lock().unwrap().take() {
                control.kick();
            }
            self.batches
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Ok(Some(Batch::default())))
        })
    }
    fn drop_slot(&self, index: &Index, t: &Target) -> Result<(), String> {
        self.drops.lock().unwrap().push((index.clone(), t.clone()));
        Ok(())
    }
    fn sleep<'a>(&'a self, ms: u64, _: &'a Control) -> Task<'a, ()> {
        Box::pin(async move {
            self.sleeps.lock().unwrap().push(ms);
            self.now.fetch_add(ms, Ordering::Relaxed);
        })
    }
}
#[tokio::test]
async fn no_work_until_kick_and_tenants_remain_separate() {
    let h = Stub::new();
    let mut w = Worker::new(Options::default());
    w.tick(&h).await;
    assert!(h.calls.lock().unwrap().is_empty());
    w.control().kick();
    w.tick(&h).await;
    let calls = h.calls.lock().unwrap();
    assert_eq!(calls.len(), 4);
    assert_eq!(
        calls
            .iter()
            .map(|(_, t, _)| t.model.as_str())
            .collect::<Vec<_>>(),
        vec!["old", "new", "old", "new"]
    );
    assert_ne!(calls[0].0.user, calls[2].0.user);
    assert!(w.retry_at_ms.is_none());
}
#[tokio::test]
async fn refused_chunks_are_excluded_and_pacing_tracks_busy_time() {
    let h = Stub::new();
    h.config.lock().unwrap().next = None;
    h.batch(2, vec![7], 4);
    h.batch(0, vec![], 3);
    let mut w = Worker::new(Options::default());
    w.control().kick();
    w.tick(&h).await;
    assert_eq!(*h.sleeps.lock().unwrap(), vec![100]);
    assert!(h.calls.lock().unwrap()[1].2.contains(&7));
    assert_eq!(w.last_pass.as_ref().unwrap().failed, 1);
    assert!(w.retry_at_ms.is_none()); // no next: no automatic retry, like Core
}
#[tokio::test]
async fn unavailable_and_error_do_not_stop_other_indexes_and_schedule_retry() {
    let h = Stub::new();
    h.batches
        .lock()
        .unwrap()
        .extend([Ok(None), Err("synthetic outage".into())]);
    let mut w = Worker::new(Options::default());
    w.control().kick();
    w.tick(&h).await;
    assert_eq!(w.last_pass.as_ref().unwrap().failed, 2);
    assert_eq!(h.calls.lock().unwrap().len(), 4);
    let retry = w.retry_at_ms.unwrap();
    h.now.store(retry - 1, Ordering::Relaxed);
    w.tick(&h).await;
    assert_eq!(h.calls.lock().unwrap().len(), 4);
    h.now.store(retry, Ordering::Relaxed);
    w.tick(&h).await;
    assert_eq!(h.calls.lock().unwrap().len(), 8);
}
#[tokio::test]
async fn held_next_keeps_active_work_and_retries_then_recovers() {
    let h = Stub::new();
    h.served.lock().unwrap().push_back(Err("offline".into()));
    let mut w = Worker::new(Options::default());
    w.control().kick();
    w.tick(&h).await;
    assert_eq!(h.calls.lock().unwrap().len(), 2);
    assert_eq!(w.held, Some((target("new"), Held::EndpointUnverified)));
    h.now.store(w.retry_at_ms.unwrap(), Ordering::Relaxed);
    w.tick(&h).await;
    assert!(w.held.is_none());
    assert!(w.verified(&target("new")));
}
#[tokio::test]
async fn model_drift_drops_partial_slot_after_transient_failure() {
    let h = Stub::new();
    let mut w = Worker::new(Options::default());
    w.control().kick();
    w.tick(&h).await;
    h.now.store(300_000, Ordering::Relaxed);
    h.served.lock().unwrap().push_back(Err("offline".into()));
    // Successful initial pass had no writes. Refresh, write next, then expire.
    h.served.lock().unwrap().clear();
    h.batch(0, vec![], 0);
    h.batch(1, vec![], 0);
    w.control().kick();
    w.tick(&h).await;
    h.now.fetch_add(300_000, Ordering::Relaxed);
    h.served.lock().unwrap().push_back(Err("offline".into()));
    w.control().kick();
    w.tick(&h).await;
    assert!(h.drops.lock().unwrap().is_empty());
    h.served
        .lock()
        .unwrap()
        .push_back(Ok(vec!["wrong-model".into()]));
    w.control().kick();
    w.tick(&h).await;
    assert_eq!(h.drops.lock().unwrap().len(), 2);
    assert_eq!(w.held, Some((target("new"), Held::ModelNotServed)));
}
#[tokio::test]
async fn active_or_previous_identity_is_never_dropped() {
    let h = Stub::new();
    h.config.lock().unwrap().previous = Some(target("new"));
    let mut w = Worker::new(Options::default());
    w.control().kick();
    w.tick(&h).await;
    h.now.store(300_000, Ordering::Relaxed);
    h.served.lock().unwrap().push_back(Ok(vec![]));
    w.control().kick();
    w.tick(&h).await;
    assert!(h.drops.lock().unwrap().is_empty());
}
#[tokio::test]
async fn recheck_between_batches_holds_next_before_more_vectors() {
    let h = Stub::new();
    h.batch(0, vec![], 0);
    h.batch(1, vec![], 5);
    h.served
        .lock()
        .unwrap()
        .extend([Ok(vec!["new".into()]), Ok(vec!["wrong".into()])]);
    let mut w = Worker::new(Options {
        min_pause_ms: 1000,
        recheck_ms: 1000,
        ..Options::default()
    });
    w.control().kick();
    w.tick(&h).await;
    assert_eq!(w.last_pass.as_ref().unwrap().embedded, 1);
    assert_eq!(h.drops.lock().unwrap().len(), 2);
    assert_eq!(
        h.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, t, _)| t.model == "new")
            .count(),
        1
    );
}
#[tokio::test]
async fn changed_version_aborts_and_stop_prevents_retries() {
    let mut h = Stub::new();
    h.bump = true;
    h.batch(1, vec![], 2);
    let mut w = Worker::new(Options::default());
    w.control().kick();
    w.tick(&h).await;
    assert!(w.last_pass.as_ref().unwrap().aborted);
    assert_eq!(h.calls.lock().unwrap().len(), 1);
    w.control().stop();
    w.control().kick();
    w.tick(&h).await;
    assert!(w.retry_at_ms.is_none());
    assert_eq!(h.calls.lock().unwrap().len(), 1);
}
#[tokio::test]
async fn kicks_during_pass_coalesce_into_one_more_pass() {
    let h = Stub::new();
    let mut w = Worker::new(Options::default());
    *h.control.lock().unwrap() = Some(w.control());
    w.control().kick();
    w.tick(&h).await;
    assert_eq!(h.calls.lock().unwrap().len(), 8);
}
#[test]
fn bounds_and_model_matching() {
    let o = Options::new(f64::NAN, 99.0, -1.0, 0.0, f64::INFINITY);
    assert_eq!(o.duty, 0.5);
    assert_eq!(o.batch, 16);
    assert_eq!(o.min_pause_ms, 0);
    assert_eq!(o.retry_ms, 1000);
    assert_eq!(o.recheck_ms, 300_000);
    assert!(serves_model(&["NOMIC:latest".into()], " nomic "));
    assert!(!serves_model(&["/models/nomic.gguf".into()], "nomic"));
}
struct FakeLoader(Mutex<VecDeque<Result<(u16, Value), String>>>);
impl Loader for FakeLoader {
    fn get(&self) -> Task<'_, Result<(u16, Value), String>> {
        Box::pin(async { self.0.lock().unwrap().pop_front().unwrap() })
    }
}
#[tokio::test]
async fn service_refresh_is_sticky_stale_and_read_only() {
    let loader = FakeLoader(Mutex::new(VecDeque::from([
        Ok((404, json!({}))),
        Ok((
            200,
            json!({"managed":true,"enabled":true,"models":[{"model":"m","file":"dir/a.GGUF"}],"previous":[{"model":"p","file":"b.gguf"}],"running":{"loaded":["extra"]}}),
        )),
        Err("offline".into()),
        Ok((200, json!({"managed":false}))),
        Ok((200, json!({"managed":true,"enabled":false}))),
    ])));
    let mut s = Service::default();
    s.tick(&loader, || 0).await;
    assert!(!s.managed());
    s.tick(&loader, || 10_000).await;
    assert!(s.managed());
    assert!(s.enabled());
    assert_eq!(s.file_of("p"), Some("b.gguf"));
    assert_eq!(
        s.protected_names(),
        ["m", "p", "dir/a.GGUF", "a", "b.gguf", "b", "extra"]
            .into_iter()
            .map(String::from)
            .collect()
    );
    s.tick(&loader, || 10_001).await; // not due: no HTTP request
    s.tick(&loader, || 20_000).await;
    assert_eq!(s.status(25_000).unwrap()["stale"], false);
    assert_eq!(s.status(25_001).unwrap()["stale"], true);
    assert_eq!(
        s.status(30_000).unwrap()["lastError"],
        "embedding HTTP unavailable"
    );
    s.tick(&loader, || 30_000).await;
    assert!(s.enabled());
    assert_eq!(s.status(30_000).unwrap()["lastError"], "HTTP 200");
    s.tick(&loader, || 40_000).await;
    assert!(!s.enabled());
    assert!(s.status(40_000).unwrap().get("lastError").is_none());
}
#[test]
fn served_listing_and_address_normalization() {
    let ids = service::served_ids(
        &json!({"data":[{"id":"loaded","status":{"value":"loaded"},"aliases":["alias"]},{"id":"sleep","status":{"value":"sleeping"}},{"id":"unloaded","status":{"value":"unloaded"}},{"id":"legacy"}],"models":[{"name":"ollama:latest"}]}),
    );
    assert_eq!(
        ids,
        vec!["alias", "legacy", "loaded", "ollama:latest", "sleep"]
    );
    assert!(service::same_address(
        Some(" http://embed/v1/// "),
        Some("http://embed/")
    ));
    assert!(service::same_address(None, Some("")));
}
