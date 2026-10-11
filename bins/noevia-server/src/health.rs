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
static OWNED: Mutex<Vec<Arc<ProbeSlot>>> = Mutex::new(Vec::new());
static STOPPED: AtomicBool = AtomicBool::new(false);
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
/// Retained bounded slots; Ready from the actual JoinHandle proves task completion.
/// Cancellation/abort/is_finished alone never release a slot.
pub fn reap_owned_probes() {
    let slots = match OWNED.try_lock() {
        Ok(s) => s.clone(),
        Err(_) => return,
    };
    let waker = Waker::noop();
    let mut cx = Context::from_waker(waker);
    for slot in slots.iter().filter(|s| !s.alive.load(Ordering::Acquire)) {
        let _ = poll_join(slot, &mut cx, false);
    }
    if let Ok(mut slots) = OWNED.try_lock() {
        slots.retain(|s| s.alive.load(Ordering::Acquire) || !s.joined.load(Ordering::Acquire));
    }
}
fn reserve_probe() -> Option<ProbeGuard> {
    if STOPPED.load(Ordering::Acquire) {
        return None;
    }
    reap_owned_probes();
    let mut slots = OWNED.try_lock().ok()?;
    if slots.len() >= PROBES || STOPPED.load(Ordering::Acquire) {
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
pub fn shutdown_owned_probes() {
    STOPPED.store(true, Ordering::Release);
    loop {
        let slots = match OWNED.try_lock() {
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
        reap_owned_probes();
        std::thread::park_timeout(Duration::from_millis(10));
    }
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
        let mut guards = Vec::new();
        for _ in 0..PROBES {
            let guard = reserve_probe().unwrap();
            *guard.0.task.lock().unwrap() = Some(tokio::spawn(std::future::pending()));
            guards.push(guard);
        }
        let extra_refused = reserve_probe().is_none();
        let references: Vec<_> = guards.iter().map(|g| g.0.clone()).collect();
        drop(guards);
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while !OWNED.lock().unwrap().is_empty() && std::time::Instant::now() < deadline {
            reap_owned_probes();
            tokio::task::yield_now().await;
        }
        assert!(extra_refused, "M6_HEALTH_ASSERT/admission-eight");
        assert!(
            references
                .iter()
                .all(|s| s.joined.load(Ordering::Acquire) && s.task.lock().unwrap().is_none()),
            "M6_HEALTH_ASSERT/actual-join"
        );
        assert!(OWNED.lock().unwrap().is_empty(), "M6_HEALTH_ASSERT/reaped");
        // Hold a reserved operation before handle publication. The shutdown
        // controller may signal cancellation, but must retain this slot until
        // its actual operation seals publication. Release before assertions.
        let pending = reserve_probe().unwrap();
        let slot = pending.0.clone();
        let shutdown = std::thread::spawn(shutdown_owned_probes);
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while !slot.cancelled.load(Ordering::Acquire) && std::time::Instant::now() < deadline {
            tokio::task::yield_now().await;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        let prematurely_returned = shutdown.is_finished();
        let retained = OWNED.lock().unwrap().iter().any(|s| Arc::ptr_eq(s, &slot));
        let no_false_join = !slot.joined.load(Ordering::Acquire);
        drop(pending);
        while !shutdown.is_finished() {
            tokio::task::yield_now().await;
        }
        shutdown.join().unwrap();
        assert!(
            !prematurely_returned,
            "M6_HEALTH_ASSERT/prepublication-shutdown"
        );
        assert!(
            retained && no_false_join,
            "M6_HEALTH_ASSERT/prepublication-retained"
        );
        assert!(
            slot.publication_closed.load(Ordering::Acquire) && slot.joined.load(Ordering::Acquire),
            "M6_HEALTH_ASSERT/publication-sealed"
        );
        assert!(
            OWNED.lock().unwrap().is_empty(),
            "M6_HEALTH_ASSERT/shutdown-empty"
        );
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
        let guard = reserve_probe().unwrap();
        let (release, blocked) = tokio::sync::oneshot::channel::<()>();
        *guard.0.task.lock().unwrap() = Some(tokio::spawn(async {
            let _ = blocked.await;
        }));
        guard.0.alive.store(false, Ordering::Release);
        guard.0.publication_closed.store(true, Ordering::Release);
        guard.0.request_waiting.store(true, Ordering::Release);
        let active = Arc::new(Count(AtomicUsize::new(0)));
        let upkeep = Arc::new(Count(AtomicUsize::new(0)));
        let active_waker = Waker::from(active.clone());
        let upkeep_waker = Waker::from(upkeep.clone());
        let mut active_cx = Context::from_waker(&active_waker);
        let mut upkeep_cx = Context::from_waker(&upkeep_waker);
        let waiting = poll_join(&guard.0, &mut active_cx, true).is_pending();
        let maintenance_deferred = poll_join(&guard.0, &mut upkeep_cx, false).is_pending();
        // Always release the actual driver before any assertion can unwind.
        let _ = release.send(());
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while active.0.load(Ordering::Acquire) == 0 && std::time::Instant::now() < deadline {
            tokio::task::yield_now().await;
        }
        let active_notified = active.0.load(Ordering::Acquire) > 0;
        let no_replacement = upkeep.0.load(Ordering::Acquire) == 0;
        std::future::poll_fn(|cx| poll_join(&guard.0, cx, true)).await;
        drop(guard);
        reap_owned_probes();
        assert!(
            waiting && maintenance_deferred,
            "M6_HEALTH_ASSERT/active-waiter-pending"
        );
        assert!(
            active_notified && no_replacement,
            "M6_HEALTH_ASSERT/active-waker-preserved"
        );
        assert!(
            OWNED.lock().unwrap().is_empty(),
            "M6_HEALTH_ASSERT/active-waker-joined"
        );
    }
}
