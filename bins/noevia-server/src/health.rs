//! GET /api/ready (core routes/health.cjs createReadyRoutes, noevia#297): unauthenticated,
//! `{ready, version}`, no tenant or upstream detail.
//!
//! While Node still answers most routes, the front is ready only once Node is: `ready` is true
//! when the upstream's own /api/ready says so (probed per call, 2 s bound); until then the
//! answer is a 503 with `ready: false`. `version` follows
//! version-resolve.cjs: dist/version.json, then STAMP_VERSION, then what Node reports (its
//! package.json fallbacks), then "unknown". GET /api/health stays Node's: it is authenticated and
//! probes the provider, the Diary sidecar and retrieval for the signed-in account.

use crate::config::Upstream;
use crate::reply;
use axum::body::Body;
use axum::http::{Request, Response, StatusCode};
use http_body_util::{BodyExt, Empty, Limited};
use serde_json::{json, Value};
use std::path::Path;
use std::time::Duration;
use std::{
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    task::{Context, Poll, Waker},
};

const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const PROBE_BODY_CAP: usize = 16 * 1024;
const PROBES: usize = 8;
struct ProbeSlot {
    alive: AtomicBool,
    cancelled: AtomicBool,
    joined: AtomicBool,
    publication_closed: AtomicBool,
    request_waiting: AtomicBool,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}
struct OwnedProbes {
    slots: Mutex<Vec<Arc<ProbeSlot>>>,
    stopped: AtomicBool,
}
static PROBE_OWNER: OwnedProbes = OwnedProbes::new();
struct ProbeGuard(Arc<ProbeSlot>);
impl ProbeGuard {
    fn cancel(&self) {
        cancel_slot(&self.0);
    }
    fn finish(&self) {
        self.cancel();
        // Called only after the operation future returned or is synchronously
        // being dropped: it cannot publish another connection handle.
        self.0.publication_closed.store(true, Ordering::Release);
    }
}
fn cancel_slot(value: &ProbeSlot) {
    value.cancelled.store(true, Ordering::Release);
    value.alive.store(false, Ordering::Release);
    if let Ok(slot) = value.task.try_lock() {
        if let Some(task) = slot.as_ref() {
            task.abort();
        }
    }
}
impl Drop for ProbeGuard {
    fn drop(&mut self) {
        self.0.request_waiting.store(false, Ordering::Release);
        self.finish();
    }
}
fn poll_join(slot: &ProbeSlot, cx: &mut Context<'_>, request: bool) -> Poll<()> {
    let Ok(mut task) = slot.task.try_lock() else {
        cx.waker().wake_by_ref();
        return Poll::Pending;
    };
    // Check while holding the task slot: maintenance must never replace the
    // active request's JoinHandle waker. Cancellation Drop revokes that owner.
    if !request && slot.request_waiting.load(Ordering::Acquire) {
        return Poll::Pending;
    }
    if let Some(handle) = task.as_mut() {
        if slot.cancelled.load(Ordering::Acquire) {
            handle.abort();
        }
        if Pin::new(handle).poll(cx).is_pending() {
            return Poll::Pending;
        }
        task.take();
    }
    if !slot.publication_closed.load(Ordering::Acquire) {
        return Poll::Pending;
    }
    slot.joined.store(true, Ordering::Release);
    Poll::Ready(())
}
impl OwnedProbes {
    const fn new() -> Self {
        Self {
            slots: Mutex::new(Vec::new()),
            stopped: AtomicBool::new(false),
        }
    }
    /// Retained bounded slots; Ready from the actual JoinHandle proves task completion.
    /// Cancellation/abort/is_finished alone never release a slot.
    fn reap(&self) {
        let slots = match self.slots.try_lock() {
            Ok(s) => s.clone(),
            Err(_) => return,
        };
        let waker = Waker::noop();
        let mut cx = Context::from_waker(waker);
        for slot in slots.iter().filter(|s| !s.alive.load(Ordering::Acquire)) {
            let _ = poll_join(slot, &mut cx, false);
        }
        if let Ok(mut slots) = self.slots.try_lock() {
            slots.retain(|s| s.alive.load(Ordering::Acquire) || !s.joined.load(Ordering::Acquire));
        }
    }
    fn reserve(&self) -> Option<ProbeGuard> {
        if self.stopped.load(Ordering::Acquire) {
            return None;
        }
        self.reap();
        let mut slots = self.slots.try_lock().ok()?;
        if slots.len() >= PROBES || self.stopped.load(Ordering::Acquire) {
            return None;
        }
        let slot = Arc::new(ProbeSlot {
            alive: AtomicBool::new(true),
            cancelled: AtomicBool::new(false),
            joined: AtomicBool::new(false),
            publication_closed: AtomicBool::new(false),
            request_waiting: AtomicBool::new(false),
            task: Mutex::new(None),
        });
        slots.push(slot.clone());
        Some(ProbeGuard(slot))
    }
    /// Synchronous main retains readiness tasks after the HTTP drain too.
    fn shutdown(&self) {
        self.stopped.store(true, Ordering::Release);
        loop {
            let slots = match self.slots.try_lock() {
                Ok(s) => s.clone(),
                Err(_) => {
                    std::thread::park_timeout(Duration::from_millis(10));
                    continue;
                }
            };
            if slots.is_empty() {
                return;
            }
            for slot in slots {
                cancel_slot(&slot);
            }
            self.reap();
            std::thread::park_timeout(Duration::from_millis(10));
        }
    }
}
/// Retained bounded slots; Ready from the actual JoinHandle proves task completion.
pub fn reap_owned_probes() {
    PROBE_OWNER.reap();
}
fn reserve_probe() -> Option<ProbeGuard> {
    PROBE_OWNER.reserve()
}
/// Synchronous main retains readiness tasks after the HTTP drain too.
pub fn shutdown_owned_probes() {
    PROBE_OWNER.shutdown();
}

/// version-resolve.cjs readVersionFrom(dist/version.json), else STAMP_VERSION.
pub fn local_version(dist: &Path, stamp: Option<&str>) -> Option<String> {
    let from_file = std::fs::read(dist.join("version.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|v| v.get("version").and_then(Value::as_str).map(str::to_string))
        .filter(|v| !v.is_empty());
    from_file.or_else(|| stamp.map(str::to_string))
}

async fn probe(upstream: &Upstream) -> Option<(bool, Option<String>)> {
    let guard = reserve_probe()?;
    let fut = async {
        let stream = tokio::net::TcpStream::connect((upstream.ip, upstream.port))
            .await
            .ok()?;
        let (mut send, conn) =
            hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(stream))
                .await
                .ok()?;
        // The retained slot already owns capacity before connect/handshake.
        // Publish the handle under its slot before any subsequent await.
        {
            let mut task = guard.0.task.try_lock().ok()?;
            if guard.0.cancelled.load(Ordering::Acquire) {
                return None;
            }
            *task = Some(tokio::spawn(async move {
                let _ = tokio::time::timeout(PROBE_TIMEOUT, conn).await;
            }));
        }
        let req = Request::get("/api/ready")
            .header("host", upstream.authority())
            .header("accept", "application/json")
            .body(Empty::<bytes::Bytes>::new())
            .ok()?;
        let res = send.send_request(req).await.ok();
        let out = match res {
            Some(res) if res.status() == StatusCode::OK => {
                let body = Limited::new(res.into_body(), PROBE_BODY_CAP)
                    .collect()
                    .await
                    .ok()?
                    .to_bytes();
                let v: Value = serde_json::from_slice(&body).ok()?;
                Some((
                    v.get("ready").and_then(Value::as_bool).unwrap_or(false),
                    v.get("version").and_then(Value::as_str).map(str::to_string),
                ))
            }
            _ => None,
        };
        out
    };
    let result = tokio::time::timeout(PROBE_TIMEOUT, fut)
        .await
        .ok()
        .flatten();
    guard.0.request_waiting.store(true, Ordering::Release);
    guard.finish();
    std::future::poll_fn(|cx| poll_join(&guard.0, cx, true)).await;
    reap_owned_probes();
    result
}

pub async fn ready(upstream: &Upstream, local: Option<&str>) -> Response<Body> {
    ready_with_native(upstream, local, true).await
}

pub async fn ready_with_native(
    upstream: &Upstream,
    local: Option<&str>,
    native_ready: bool,
) -> Response<Body> {
    let (ready, upstream_version) = probe(upstream).await.unwrap_or((false, None));
    let ready = ready && native_ready;
    let version = local
        .map(str::to_string)
        .or(upstream_version)
        .unwrap_or_else(|| "unknown".into());
    // Node only ever answers this once it is listening, so a client never saw `ready: false` with
    // a 200 from it; a front whose Node is not up yet answers 503 (the web client's
    // checkApiCompatibility and the replayer read !ok as "not yet").
    let status = if ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    reply::json(status, &json!({ "ready": ready, "version": version }), true)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn version_order() {
        let dir = std::env::temp_dir().join(format!("noevia-server-ver-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(local_version(&dir, None), None);
        assert_eq!(local_version(&dir, Some("s1")).as_deref(), Some("s1"));
        std::fs::write(dir.join("version.json"), r#"{"version":""}"#).unwrap();
        assert_eq!(local_version(&dir, Some("s1")).as_deref(), Some("s1"));
        std::fs::write(dir.join("version.json"), r#"{"version":"abc","web":"w"}"#).unwrap();
        assert_eq!(local_version(&dir, Some("s1")).as_deref(), Some("abc"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod m6_owned_controls {
    use super::*;
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn m6_health_capacity_join_and_shutdown_before_publication() {
        let owner = Arc::new(OwnedProbes::new());
        let fresh = !owner.stopped.load(Ordering::Acquire)
            && owner.slots.lock().is_ok_and(|s| s.is_empty());
        let mut guards = Vec::new();
        let mut published = true;
        for _ in 0..PROBES {
            let Some(guard) = owner.reserve() else { break };
            if let Ok(mut task) = guard.0.task.lock() {
                *task = Some(tokio::spawn(std::future::pending()));
            } else {
                published = false;
            }
            guards.push(guard);
        }
        let eight = guards.len() == PROBES && published;
        let references: Vec<_> = guards.iter().map(|g| g.0.clone()).collect();
        let distinct = references.iter().enumerate().all(|(i, slot)| {
            references
                .iter()
                .skip(i + 1)
                .all(|other| !Arc::ptr_eq(slot, other))
        });
        // Retain any unexpectedly admitted ninth operation until cleanup.
        let extra = owner.reserve();
        let extra_refused = extra.is_none();
        drop(extra);
        drop(guards);
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while !owner.slots.lock().is_ok_and(|s| s.is_empty())
            && std::time::Instant::now() < deadline
        {
            owner.reap();
            tokio::task::yield_now().await;
        }
        let actual_join = references
            .iter()
            .all(|s| s.joined.load(Ordering::Acquire) && s.task.lock().is_ok_and(|t| t.is_none()));
        let reaped = owner.slots.lock().is_ok_and(|s| s.is_empty());
        // Hold a reserved operation before handle publication. The shutdown
        // controller must retain it until its operation seals publication.
        let pending = owner.reserve();
        let pending_reserved = pending.is_some();
        let slot = pending.as_ref().map(|g| g.0.clone());
        let shutdown_owner = owner.clone();
        let shutdown = std::thread::spawn(move || shutdown_owner.shutdown());
        let mut prematurely_returned = false;
        let mut retained = false;
        let mut no_false_join = false;
        let mut cancelled = false;
        if let Some(slot) = slot.as_ref() {
            let deadline = std::time::Instant::now() + Duration::from_secs(1);
            while !slot.cancelled.load(Ordering::Acquire) && std::time::Instant::now() < deadline {
                tokio::task::yield_now().await;
            }
            cancelled = slot.cancelled.load(Ordering::Acquire);
            tokio::time::sleep(Duration::from_millis(20)).await;
            prematurely_returned = shutdown.is_finished();
            retained = owner
                .slots
                .lock()
                .is_ok_and(|s| s.iter().any(|s| Arc::ptr_eq(s, slot)));
            no_false_join = !slot.joined.load(Ordering::Acquire);
        }
        // Every guard is released before waiting, including failed preconditions.
        drop(pending);
        while !shutdown.is_finished() {
            tokio::task::yield_now().await;
        }
        let shutdown_joined = shutdown.join().is_ok();
        let sealed = slot.as_ref().is_some_and(|s| {
            s.publication_closed.load(Ordering::Acquire) && s.joined.load(Ordering::Acquire)
        });
        let empty = owner.slots.lock().is_ok_and(|s| s.is_empty());
        assert!(
            fresh && eight && distinct,
            "M6_HEALTH_ASSERT/fresh-eight-precondition"
        );
        assert!(extra_refused, "M6_HEALTH_ASSERT/admission-eight");
        assert!(actual_join, "M6_HEALTH_ASSERT/actual-join");
        assert!(reaped, "M6_HEALTH_ASSERT/reaped");
        assert!(
            pending_reserved && cancelled,
            "M6_HEALTH_ASSERT/pending-precondition"
        );
        assert!(
            !prematurely_returned,
            "M6_HEALTH_ASSERT/prepublication-shutdown"
        );
        assert!(
            retained && no_false_join,
            "M6_HEALTH_ASSERT/prepublication-retained"
        );
        assert!(sealed, "M6_HEALTH_ASSERT/publication-sealed");
        assert!(shutdown_joined && empty, "M6_HEALTH_ASSERT/shutdown-empty");
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod m6_join_waker_controls {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::task::Wake;
    struct Count(AtomicUsize);
    impl Wake for Count {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::AcqRel);
        }
        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, Ordering::AcqRel);
        }
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn m6_health_active_waiter_keeps_join_waker() {
        let owner = Arc::new(OwnedProbes::new());
        let fresh = !owner.stopped.load(Ordering::Acquire)
            && owner.slots.lock().is_ok_and(|s| s.is_empty());
        let guard = owner.reserve();
        let reserved = guard.is_some();
        let mut published = false;
        let mut waiting = false;
        let mut maintenance_deferred = false;
        let mut active_notified = false;
        let mut no_replacement = false;
        if let Some(guard) = guard.as_ref() {
            let (release, blocked) = tokio::sync::oneshot::channel::<()>();
            if let Ok(mut task) = guard.0.task.lock() {
                *task = Some(tokio::spawn(async {
                    let _ = blocked.await;
                }));
                published = true;
            }
            guard.0.alive.store(false, Ordering::Release);
            guard.0.publication_closed.store(true, Ordering::Release);
            guard.0.request_waiting.store(true, Ordering::Release);
            let active = Arc::new(Count(AtomicUsize::new(0)));
            let upkeep = Arc::new(Count(AtomicUsize::new(0)));
            let active_waker = Waker::from(active.clone());
            let upkeep_waker = Waker::from(upkeep.clone());
            let mut active_cx = Context::from_waker(&active_waker);
            let mut upkeep_cx = Context::from_waker(&upkeep_waker);
            waiting = poll_join(&guard.0, &mut active_cx, true).is_pending();
            maintenance_deferred = poll_join(&guard.0, &mut upkeep_cx, false).is_pending();
            // Always release the actual driver before any assertion can unwind.
            let _ = release.send(());
            let deadline = std::time::Instant::now() + Duration::from_secs(1);
            while active.0.load(Ordering::Acquire) == 0 && std::time::Instant::now() < deadline {
                tokio::task::yield_now().await;
            }
            active_notified = active.0.load(Ordering::Acquire) > 0;
            no_replacement = upkeep.0.load(Ordering::Acquire) == 0;
            std::future::poll_fn(|cx| poll_join(&guard.0, cx, true)).await;
        }
        drop(guard);
        owner.reap();
        // Failed reservation/publication also reaches actual owner teardown.
        let shutdown_owner = owner.clone();
        let shutdown = std::thread::spawn(move || shutdown_owner.shutdown());
        while !shutdown.is_finished() {
            tokio::task::yield_now().await;
        }
        let shutdown_joined = shutdown.join().is_ok();
        let empty = owner.slots.lock().is_ok_and(|s| s.is_empty());
        assert!(
            fresh && reserved && published,
            "M6_HEALTH_ASSERT/fresh-waker-precondition"
        );
        assert!(
            waiting && maintenance_deferred,
            "M6_HEALTH_ASSERT/active-waiter-pending"
        );
        assert!(
            active_notified && no_replacement,
            "M6_HEALTH_ASSERT/active-waker-preserved"
        );
        assert!(
            shutdown_joined && empty,
            "M6_HEALTH_ASSERT/active-waker-joined"
        );
    }
}
