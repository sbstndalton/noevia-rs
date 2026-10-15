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

const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const PROBE_BODY_CAP: usize = 16 * 1024;

/// version-resolve.cjs readVersionFrom(dist/version.json), else STAMP_VERSION.
pub fn local_version(dist: &Path, stamp: Option<&str>) -> Option<String> {
    let from_file = std::fs::read(dist.join("version.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|v| v.get("version").and_then(Value::as_str).map(str::to_string))
        .filter(|v| !v.is_empty());
    from_file.or_else(|| stamp.map(str::to_string))
}

type Connection = hyper::client::conn::http1::Connection<
    hyper_util::rt::TokioIo<tokio::net::TcpStream>,
    Empty<bytes::Bytes>,
>;

#[cfg(not(test))]
fn spawn_connection(conn: Connection) -> tokio::task::JoinHandle<Result<(), hyper::Error>> {
    tokio::spawn(conn)
}

/// Test builds count each connection task from spawn until its future is dropped; see `live`.
#[cfg(test)]
fn spawn_connection(conn: Connection) -> tokio::task::JoinHandle<Result<(), hyper::Error>> {
    let live = live::enter();
    tokio::spawn(async move {
        let _live = live;
        conn.await
    })
}

/// Owns the probe's Hyper connection task. `finish` aborts it and observes its join. A probe
/// future dropped before that (client gone, handler cancelled) aborts it in `Drop`, which is a
/// request to stop, not an observed join.
#[derive(Default)]
struct ConnectionTask(Option<tokio::task::JoinHandle<Result<(), hyper::Error>>>);

impl ConnectionTask {
    async fn finish(mut self) {
        if let Some(task) = self.0.take() {
            task.abort();
            let _ = task.await;
        }
    }
}

impl Drop for ConnectionTask {
    fn drop(&mut self) {
        if let Some(task) = &self.0 {
            task.abort();
        }
    }
}

#[cfg(test)]
mod live {
    use std::sync::atomic::{AtomicUsize, Ordering};

    pub static COUNT: AtomicUsize = AtomicUsize::new(0);

    /// Counted from spawn until the task's future is dropped, whether it finished or was aborted.
    pub struct Guard;

    pub fn enter() -> Guard {
        COUNT.fetch_add(1, Ordering::SeqCst);
        Guard
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            COUNT.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

/// The timed exchange. Its connection task is stored in `connection`, not in this future, so a
/// timeout or an early `?` return leaves the handle with the caller to abort and join.
async fn exchange(
    upstream: &Upstream,
    connection: &mut ConnectionTask,
) -> Option<(bool, Option<String>)> {
    let stream = tokio::net::TcpStream::connect((upstream.ip, upstream.port))
        .await
        .ok()?;
    let (mut send, conn) =
        hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(stream))
            .await
            .ok()?;
    connection.0 = Some(spawn_connection(conn));
    let req = Request::get("/api/ready")
        .header("host", upstream.authority())
        .header("accept", "application/json")
        .body(Empty::<bytes::Bytes>::new())
        .ok()?;
    let res = send.send_request(req).await.ok()?;
    if res.status() != StatusCode::OK {
        return None;
    }
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

async fn probe(upstream: &Upstream) -> Option<(bool, Option<String>)> {
    let mut connection = ConnectionTask::default();
    let outcome = tokio::time::timeout(PROBE_TIMEOUT, exchange(upstream, &mut connection)).await;
    // Every outcome (answer, refusal, body or JSON error, timeout) reaches this join before the
    // probe returns, so no connection task outlives its probe.
    connection.finish().await;
    outcome.ok().flatten()
}

pub async fn ready(upstream: &Upstream, local: Option<&str>) -> Response<Body> {
    let (ready, upstream_version) = probe(upstream).await.unwrap_or((false, None));
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
    use std::io::{Read, Write};

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

    /// Serialises the probe controls: the live-task count is process-wide.
    static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// A loopback-only stand-in for Node's /api/ready: reads one request, writes `response`, then
    /// keeps the socket open until the probe closes it or the window ends.
    fn serve_once(response: Vec<u8>) -> Upstream {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut peer, _) = listener.accept().unwrap();
            peer.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut request = Vec::new();
            let mut chunk = [0u8; 512];
            while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                match peer.read(&mut chunk) {
                    Ok(n) if n > 0 => request.extend_from_slice(chunk.get(..n).unwrap()),
                    _ => break,
                }
            }
            peer.write_all(&response).unwrap();
            let mut rest = [0u8; 64];
            let _ = peer.read(&mut rest);
        });
        Upstream {
            ip: std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            port,
        }
    }

    #[tokio::test]
    async fn ready_answer_leaves_no_connection_task() {
        let _serial = SERIAL.lock().await;
        let body = br#"{"ready":true,"version":"v1"}"#;
        let mut response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
            body.len()
        )
        .into_bytes();
        response.extend_from_slice(body);
        let upstream = serve_once(response);
        assert_eq!(probe(&upstream).await, Some((true, Some("v1".to_string()))));
        assert_eq!(live::COUNT.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn invalid_json_body_leaves_no_connection_task() {
        let _serial = SERIAL.lock().await;
        // The keep-alive connection stays open after the body, so only the probe can end it.
        let upstream = serve_once(b"HTTP/1.1 200 OK\r\nContent-Length: 8\r\n\r\nnot json".to_vec());
        assert_eq!(probe(&upstream).await, None);
        // Before the fix, the `?` on JSON parse returned without observing the connection task.
        assert_eq!(live::COUNT.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn stalled_body_leaves_no_connection_task_at_the_timeout() {
        let _serial = SERIAL.lock().await;
        // Headers promise 100 bytes; only two arrive, so the body collect waits until the
        // two-second probe budget drops the exchange.
        let upstream = serve_once(
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 100\r\n\r\n{\""
                .to_vec(),
        );
        let started = std::time::Instant::now();
        assert_eq!(probe(&upstream).await, None);
        assert!(started.elapsed() < Duration::from_secs(4));
        // Before the fix, the timed-out future dropped its JoinHandle, leaving the task running.
        assert_eq!(live::COUNT.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn not_ready_status_leaves_no_connection_task() {
        let _serial = SERIAL.lock().await;
        let upstream =
            serve_once(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n".to_vec());
        assert_eq!(probe(&upstream).await, None);
        assert_eq!(live::COUNT.load(std::sync::atomic::Ordering::SeqCst), 0);
    }
}
