//! GET /api/ready (core routes/health.cjs createReadyRoutes, noevia#297): unauthenticated,
//! `{ready, version}`, no tenant or upstream detail.
//!
//! While Node still answers most routes, the front is ready only once Node is: `ready` is true
//! when the upstream's own /api/ready says so (probed per call, 2 s bound). `version` follows
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

async fn probe(upstream: &Upstream) -> Option<(bool, Option<String>)> {
    let fut = async {
        let stream = tokio::net::TcpStream::connect((upstream.ip, upstream.port))
            .await
            .ok()?;
        let (mut send, conn) =
            hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(stream))
                .await
                .ok()?;
        let task = tokio::spawn(conn);
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
        task.abort();
        out
    };
    tokio::time::timeout(PROBE_TIMEOUT, fut)
        .await
        .ok()
        .flatten()
}

pub async fn ready(upstream: &Upstream, local: Option<&str>) -> Response<Body> {
    let (ready, upstream_version) = probe(upstream).await.unwrap_or((false, None));
    let version = local
        .map(str::to_string)
        .or(upstream_version)
        .unwrap_or_else(|| "unknown".into());
    reply::json(
        StatusCode::OK,
        &json!({ "ready": ready, "version": version }),
        true,
    )
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
