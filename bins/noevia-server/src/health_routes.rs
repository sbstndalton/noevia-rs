//! `GET /api/health` (sbstndalton/noevia#2190 close-out, capability `rust-health`), ported from core
//! routes/health.cjs: whether the default provider answers, whether the Diary sidecar answers (null
//! when the add-on is off for the account), and whether project-file retrieval can run.
//!
//! Authentication is exactly Node's: the route is mounted after the session gate, so a signed-in
//! person (a session or a device token) gets the answer and anyone else the gate's 401/403. It is
//! not `GET /api/ready` (health.rs), which is unauthenticated and says only whether the process is
//! up. Only GET is the front's; the other methods stay Node's (a Node that no longer has the route
//! answers its own 404).
//!
//! The probes the front can make itself, it makes, with core's bounds (5 s each, the body counts
//! inside the bound, redirects are failures, a 2xx is "up", no caching: core cached only the
//! retrieval probe, and that stays Node's):
//! - the default provider: the env-derived row (`INFERENCE_BASE_URL`, else `LEMONADE_BASE_URL`, else
//!   core's default address; `INFERENCE_API_KEY`/`LEMONADE_API_KEY` as a bearer unless empty or the
//!   `local` sentinel), `GET <base without a trailing slash or /v1>/v1/models`. `inferenceUp` and
//!   `lemonadeUp` are the same answer;
//! - the Diary sidecar, through server-diary's client and the person's tenant, only while the add-on
//!   is on for the account.
//!
//! `ragAvailable` and `retrieval` are Node-live (native index dependencies, the embedding probe and
//! its cache, the administrator's embedding switch). Node publishes them as `rag-status.json`
//! (snapshot.rs), and only while someone asks: this route touches the demand marker, and when Node
//! was idle it waits up to two seconds for a fresh file. An unusable or stale file is
//! `ragAvailable: false, retrieval: "unavailable"`, never an old value. A `retrieval` that is not one
//! of core's four words reads the same way. With Core down the answer is still a 200 (Node's route
//! died with it: a 502 from the proxy before), `retrieval: "unavailable"`.
//!
//! Each probe runs in a task of its own that the handler awaits, bounded by its own deadline: a
//! client that goes away does not leave a half-read connection, and nothing outlives the 5 s bound.

use crate::{identity, reply, snapshot, App};
use axum::body::Body;
use axum::http::{Method, Request, Response, StatusCode};
use axum::response::IntoResponse;
use std::path::Path;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

pub const PATH: &str = "/api/health";
/// Where Node publishes retrieval's state (core `rust-health.cjs`), in the data directory.
pub const SNAPSHOT_FILE: &str = "rag-status.json";
pub const SNAPSHOT_KIND: &str = "rag-status";

/// core: `fetchJson(url, ..., 5000)` for both probes.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
/// core http.cjs `DEFAULT_MAX_RESPONSE_BYTES`: a longer reply is `ok: false`.
const REPLY_CAP: usize = 8 * 1024 * 1024;
/// core rag.cjs `retrievalStatus`'s words.
const WORDS: [&str; 4] = ["available", "degraded", "off", "unavailable"];

/// The probe tasks that are running; a debug build counts them so a test can see them end.
#[cfg(debug_assertions)]
pub static PROBES_RUNNING: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

struct Counted;
impl Counted {
    fn new() -> Self {
        #[cfg(debug_assertions)]
        PROBES_RUNNING.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Counted
    }
}
impl Drop for Counted {
    fn drop(&mut self) {
        #[cfg(debug_assertions)]
        PROBES_RUNNING.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

fn client() -> Option<&'static reqwest::Client> {
    static CLIENT: OnceLock<Option<reqwest::Client>> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .ok()
        })
        .as_ref()
}

/// The default provider's models URL and bearer, as core's `getProvider(DEFAULT_PROVIDER_ID)` and
/// `providerHeaders` give them (provider_routes.rs holds the same environment rules for the listing).
pub fn default_provider_probe(get: &impl Fn(&str) -> Option<String>) -> (String, Option<String>) {
    let set = |k: &str| get(k).filter(|v| !v.is_empty());
    let base = set("INFERENCE_BASE_URL")
        .or_else(|| set("LEMONADE_BASE_URL"))
        .unwrap_or_else(|| "http://host.docker.internal:11434".to_owned());
    let key = set("INFERENCE_API_KEY").or_else(|| set("LEMONADE_API_KEY"));
    // `.replace(/\/+$/, '').replace(/\/v1$/, '')`.
    let trimmed = base.trim_end_matches('/');
    let root = trimmed.strip_suffix("/v1").unwrap_or(trimmed);
    // `if (provider.apiKey && provider.apiKey !== 'local')`.
    let bearer = key.filter(|k| k != "local");
    (format!("{root}/v1/models"), bearer)
}

/// `fetchJson(url, { headers }, 5000)` and its `.ok`.
async fn provider_up(url: String, bearer: Option<String>) -> bool {
    let _counted = Counted::new();
    let Some(client) = client() else {
        return false;
    };
    let mut request = client.get(url).header("Content-Type", "application/json");
    if let Some(key) = bearer {
        request = request.header("Authorization", format!("Bearer {key}"));
    }
    let probe = async {
        let mut response = request.send().await.ok()?;
        let ok = response.status().is_success();
        let mut seen = 0usize;
        while let Some(chunk) = response.chunk().await.ok()? {
            seen += chunk.len();
            if seen > REPLY_CAP {
                return Some(false);
            }
        }
        Some(ok)
    };
    matches!(
        tokio::time::timeout(PROBE_TIMEOUT, probe).await,
        Ok(Some(true))
    )
}

/// `(ragAvailable, retrieval)` from the published snapshot, `None` when it is unusable.
pub fn retrieval_checked(data_dir: &Path, now_ms: i64) -> Option<(bool, &'static str)> {
    let snap = snapshot::read(&data_dir.join(SNAPSHOT_FILE), SNAPSHOT_KIND, now_ms).ok()?;
    let rag = snap.body.get("ragAvailable").and_then(|v| v.as_bool());
    let word = snap.body.get("retrieval").and_then(|v| v.as_str());
    match (rag, word.and_then(|w| WORDS.iter().find(|k| **k == w))) {
        (Some(rag), Some(word)) => Some((rag, word)),
        _ => None,
    }
}

/// `(ragAvailable, retrieval)` from the published snapshot; the unusable case is
/// `(false, "unavailable")`.
pub fn retrieval_from(data_dir: &Path, now_ms: i64) -> (bool, &'static str) {
    retrieval_checked(data_dir, now_ms).unwrap_or((false, "unavailable"))
}

fn body(inference: bool, diary: Option<bool>, rag: bool, retrieval: &str) -> String {
    let diary = match diary {
        Some(true) => "true",
        Some(false) => "false",
        None => "null",
    };
    format!(
        "{{\"inferenceUp\":{inference},\"lemonadeUp\":{inference},\"diaryUp\":{diary},\"ragAvailable\":{rag},\"retrieval\":\"{retrieval}\"}}"
    )
}

pub async fn serve(
    app: &Arc<App>,
    conn: crate::serve::Conn,
    req: Request<Body>,
    path: String,
) -> Response<Body> {
    // Node matched the exact pathname; the table's trailing-slash tolerance is not its route.
    if path != PATH || req.method() != Method::GET {
        return app.proxy.forward(conn.peer, req).await;
    }
    let (parts, _body) = req.into_parts();
    let Some(data_dir) = app.config.data_dir.clone() else {
        return identity::Rejection::Unavailable.into_response();
    };
    let creds = identity::creds_of(&parts);
    let layer = Arc::clone(&app.identity);
    let now = identity::now_ms();
    let gate_path = path.clone();
    let gate_layer = Arc::clone(&layer);
    let id = match tokio::task::spawn_blocking(move || {
        gate_layer.gate_blocking(&creds, &gate_path, now)
    })
    .await
    {
        Ok(Ok(id)) => id,
        Ok(Err(rejection)) => return rejection.into_response(),
        Err(_) => return identity::Rejection::Unavailable.into_response(),
    };
    let diary_on = id.user.diary_enabled;
    // A signed-in request asks Node to publish (the demand marker); when Node was idle the read waits
    // up to two seconds for it, while the probes below run.
    let dir = data_dir.clone();
    let snap = app
        .demand
        .read(data_dir.clone(), move |now| retrieval_checked(&dir, now));
    let (url, bearer) = default_provider_probe(&|k| std::env::var(k).ok());
    let inference = tokio::spawn(provider_up(url, bearer));
    let diary = if diary_on {
        let sidecar = app.diary.clone();
        let task = tokio::spawn(async move {
            let _counted = Counted::new();
            let Some(diary) = sidecar else {
                return false;
            };
            let Ok(Ok(store)) = tokio::task::spawn_blocking(move || layer.store_blocking()).await
            else {
                return false;
            };
            // The health probe asks the sidecar only: no storage-login latch, no exchange set.
            let ctx = server_diary::Ctx {
                identity: id,
                store,
                data_dir,
                latch: None,
                start_id: None,
            };
            diary.health(&ctx).await
        });
        Some(task)
    } else {
        None
    };
    let inference = inference.await.unwrap_or(false);
    let diary = match diary {
        Some(task) => Some(task.await.unwrap_or(false)),
        None => None,
    };
    let (rag, retrieval) = snap.await.unwrap_or((false, "unavailable"));
    reply::json_text(StatusCode::OK, body(inference, diary, rag, retrieval), true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn probe(env: &[(&str, &str)]) -> (String, Option<String>) {
        let map: HashMap<_, _> = env.iter().copied().collect();
        default_provider_probe(&|k| map.get(k).map(|v| (*v).to_owned()))
    }

    #[test]
    fn the_provider_url_and_key_follow_core() {
        assert_eq!(
            probe(&[]),
            ("http://host.docker.internal:11434/v1/models".into(), None)
        );
        assert_eq!(
            probe(&[
                ("LEMONADE_BASE_URL", "http://a:1/v1/"),
                ("LEMONADE_API_KEY", "k")
            ]),
            ("http://a:1/v1/models".into(), Some("k".into()))
        );
        // INFERENCE_* wins; a key of `local` is no bearer; trailing slashes go before `/v1`.
        assert_eq!(
            probe(&[
                ("INFERENCE_BASE_URL", "http://b:2///"),
                ("LEMONADE_BASE_URL", "http://a:1"),
                ("INFERENCE_API_KEY", "local"),
                ("LEMONADE_API_KEY", "k"),
            ]),
            ("http://b:2/v1/models".into(), None)
        );
        // An empty value counts as unset (JS `||`).
        assert_eq!(
            probe(&[
                ("INFERENCE_BASE_URL", ""),
                ("LEMONADE_BASE_URL", "http://a:1/v1")
            ])
            .0,
            "http://a:1/v1/models"
        );
        // `/v1` is stripped once and only at the end.
        assert_eq!(
            probe(&[("INFERENCE_BASE_URL", "http://a/v1/v1")]).0,
            "http://a/v1/v1/models"
        );
        assert_eq!(
            probe(&[("INFERENCE_BASE_URL", "http://a/v1x")]).0,
            "http://a/v1x/v1/models"
        );
    }

    #[test]
    fn the_body_has_cores_keys_in_cores_order() {
        assert_eq!(
            body(true, None, false, "unavailable"),
            r#"{"inferenceUp":true,"lemonadeUp":true,"diaryUp":null,"ragAvailable":false,"retrieval":"unavailable"}"#
        );
        assert_eq!(
            body(false, Some(true), true, "degraded"),
            r#"{"inferenceUp":false,"lemonadeUp":false,"diaryUp":true,"ragAvailable":true,"retrieval":"degraded"}"#
        );
    }
}
