//! The front owns auto-tune's state: the public routes (status, settings, resume, cancel, clear) run
//! against the real owner and a synthetic Node actor that answers only the private actor calls and
//! writes through the store channel, exactly as the real actor does. No engine or model work.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
#[path = "mcp_support/mod.rs"]
mod support;
use noevia_server::project_store;
use serde_json::{json, Value};
use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
    time::Duration,
};
use support::*;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

const BASE: &str = "native-autotune-0123456789abcdef.json";
const STATUS: &str = "/api/models/autotune";
const CANCEL: &str = "/api/models/autotune/cancel";
const CLEAR: &str = "/api/models/autotune/clear";
const SETTINGS: &str = "/api/models/autotune/settings";
const RESUME: &str = "/api/models/autotune/resume";

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Ok,
    Refuse,
    Down,
}
struct Actor {
    revision: String,
    system: HashSet<String>,
    unconfigured: HashSet<String>,
    abort: Mode,
    launch: Mode,
    listing_system: bool,
}
type Shared = Arc<Mutex<Actor>>;

fn actor() -> Shared {
    Arc::new(Mutex::new(Actor {
        revision: "r1".into(),
        system: HashSet::new(),
        unconfigured: HashSet::new(),
        abort: Mode::Ok,
        launch: Mode::Ok,
        listing_system: false,
    }))
}
fn install(up: &Upstream, a: &Shared) {
    let a = Arc::clone(a);
    up.set(move |r| {
        let a = a.lock().unwrap();
        let body = j(&r.body);
        match r.path.as_str() {
            "/internal/autotune-actor/outside" => {
                let mut models = serde_json::Map::new();
                for name in body["models"].as_array().into_iter().flatten() {
                    let n = name.as_str().unwrap();
                    models.insert(
                        n.to_owned(),
                        json!({"configured": !a.unconfigured.contains(n), "system": a.system.contains(n)}),
                    );
                }
                reply(
                    "200 OK",
                    &json!({"revision": a.revision, "allowBelowQ5Kv": false, "models": models})
                        .to_string(),
                )
            }
            "/internal/autotune-actor/abort" => match a.abort {
                Mode::Ok => reply("200 OK", r#"{"aborted":true}"#),
                Mode::Refuse => reply("200 OK", r#"{"aborted":false}"#),
                Mode::Down => reply("503 Down", "{}"),
            },
            "/internal/autotune-actor/launch" => match a.launch {
                Mode::Ok => reply("200 OK", r#"{"launched":true}"#),
                Mode::Refuse => reply("200 OK", r#"{"launched":false}"#),
                Mode::Down => reply("503 Down", "{}"),
            },
            "/internal/autotune-actor/invalidate" => reply("200 OK", r#"{"ok":true}"#),
            "/internal/autotune-actor/listing" => {
                let rows: Vec<Value> = body["items"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .enumerate()
                    .map(|(i, _)| json!({"index": i, "system": a.listing_system, "modelListed": true, "baseListed": false}))
                    .collect();
                reply("200 OK", &json!({"models": rows}).to_string())
            }
            "/internal/autotune-actor/identities" => {
                let ids: Vec<Value> = body["selectors"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|s| json!({"index": s["index"], "field": s["field"], "identity": "a".repeat(64)}))
                    .collect();
                reply("200 OK", &json!({"identities": ids}).to_string())
            }
            _ => reply("404 No", "{}"),
        }
    });
}

async fn store_at(addr: std::net::SocketAddr, op: &str, body: &str) -> (u16, Value) {
    let mut body = body.to_owned();
    if op == "attach" && j(&body).get("actor").is_none() {
        static SESSION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let actor = format!(
            "{:032x}",
            SESSION.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let payload: Value = serde_json::from_str(&body).unwrap();
        let mut open = payload.clone();
        open["actor"] = json!(actor);
        open["open"] = json!(true);
        let (_, session) = Box::pin(store_at(addr, op, &open.to_string())).await;
        let mut attach = payload;
        attach["actor"] = json!(actor);
        attach["incarnation"] = session["incarnation"].clone();
        body = attach.to_string();
    }
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(format!("POST /v1/autotune-state/{op} HTTP/1.1\r\nhost: store\r\nconnection: close\r\ncontent-type: application/json\r\ncontent-length: {}\r\n{}: {STORE_TOKEN}\r\n\r\n{body}", body.len(), project_store::TOKEN_HEADER).as_bytes()).await.unwrap();
    let mut out = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(40), s.read_to_end(&mut out)).await;
    let text = String::from_utf8_lossy(&out).into_owned();
    let status = text[9..12].parse().unwrap();
    let body = text.split_once("\r\n\r\n").map_or("", |(_, b)| b);
    (status, serde_json::from_str(body).unwrap_or(Value::Null))
}

struct Rig {
    d: tempfile::TempDir,
    f: Front,
    up: Upstream,
    actor: Shared,
}
async fn rig(state: Option<&str>) -> Rig {
    let d = tempfile::tempdir().unwrap();
    seed(d.path());
    if let Some(s) = state {
        std::fs::write(d.path().join(BASE), s).unwrap();
    }
    let up = upstream().await;
    let a = actor();
    install(&up, &a);
    let f = front_env(
        d.path(),
        true,
        &[
            ("NOEVIA_LEGACY_UPSTREAM", up.base()),
            ("NOEVIA_PROJECT_STORE_TOKEN", STORE_TOKEN.into()),
        ],
    )
    .await;
    Rig { d, f, up, actor: a }
}
impl Rig {
    async fn attach(&self) -> Value {
        let (s, v) = store_at(
            self.f.store,
            "attach",
            &format!(r#"{{"base":"{BASE}","available":true}}"#),
        )
        .await;
        assert_eq!(s, 200, "{v}");
        v
    }
    async fn commit(&self, w: &str, job_rev: u64, job: &Value, history: &Value) -> (u16, Value) {
        store_at(
            self.f.store,
            "commit",
            &json!({"writer": w, "jobRev": job_rev, "job": job, "history": history}).to_string(),
        )
        .await
    }
    fn disk(&self) -> Value {
        j(&std::fs::read_to_string(self.d.path().join(BASE)).unwrap())
    }
    async fn post(&self, path: &str, body: &str) -> (u16, Value) {
        write(self.f.addr, "POST", path, ADMIN, body).await
    }
}
fn running() -> Value {
    json!({"id":"j","model":"m","status":"running","startedAt":1,"promptBudgetSeconds":120,"_revision":"r1","originalText":"private",
        "models":[{"model":"m","status":"running","phases":[{"id":"context","status":"running","steps":[{"status":"running"}]}]}]})
}
fn finished(status: &str) -> Value {
    let mut v = running();
    v["status"] = json!(status);
    v["models"][0]["status"] = json!("interrupted");
    v["models"][0]["phases"][0]["status"] = json!("interrupted");
    v["models"][0]["phases"][0]["steps"][0]["status"] = json!("interrupted");
    v
}
const NODE_STATE: &str = r#"{"history":{"m":[{"at":1,"context":8192}]},"settings":{"m":{"allowQ5Kv":true}},"job":{"id":"j","model":"m","status":"failed","startedAt":1,"_revision":"r1","models":[]}}"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn routes_wait_for_the_actor_and_an_actor_without_a_tuner_is_unavailable() {
    let r = rig(None).await;
    for path in [STATUS, CANCEL, CLEAR, SETTINGS, RESUME] {
        let (s, v) = if path == STATUS {
            get(r.f.addr, path, ADMIN).await
        } else {
            r.post(path, "{}").await
        };
        assert_eq!(s, 503, "{path} {v}");
    }
    let (s, _) = store_at(
        r.f.store,
        "attach",
        &format!(r#"{{"base":"{BASE}","available":false}}"#),
    )
    .await;
    assert_eq!(s, 200);
    assert_eq!(get(r.f.addr, STATUS, ADMIN).await.0, 404);
    assert_eq!(r.post(CANCEL, "").await.0, 404);
    assert_eq!(r.post(CLEAR, "").await.0, 404);
    assert!(r.up.seen().is_empty(), "no actor call without a tuner");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_serves_node_written_state_and_a_restarted_front_refuses_the_old_writer() {
    let r = rig(Some(NODE_STATE)).await;
    let a = r.attach().await;
    let (s, v) = get(r.f.addr, &format!("{STATUS}?model=m"), ADMIN).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["job"]["id"], "j");
    assert_eq!(v["settings"], json!({"allowQ5Kv": true}));
    assert_eq!(v["history"][0]["context"], 8192);
    // Attaching rewrites nothing: the file is Node's bytes until the owner changes something.
    assert_eq!(
        std::fs::read_to_string(r.d.path().join(BASE)).unwrap(),
        NODE_STATE
    );
    // The front restarts over the same directory: a new incarnation is unbound, and the actor that
    // survived is refused until it attaches again, then sees the same state.
    let again = front_env(
        r.d.path(),
        true,
        &[
            ("NOEVIA_LEGACY_UPSTREAM", r.up.base()),
            ("NOEVIA_PROJECT_STORE_TOKEN", STORE_TOKEN.into()),
        ],
    )
    .await;
    assert_eq!(get(again.addr, STATUS, ADMIN).await.0, 503);
    let writer = a["writer"].as_str().unwrap();
    let (s, v) = store_at(
        again.store,
        "commit",
        &json!({"writer": writer, "jobRev": 1, "job": running(), "history": {}}).to_string(),
    )
    .await;
    assert_eq!((s, v["fenced"].as_str()), (409, Some("unbound")));
    assert_eq!(
        std::fs::read_to_string(r.d.path().join(BASE)).unwrap(),
        NODE_STATE
    );
    let (s, _) = store_at(
        again.store,
        "attach",
        &format!(r#"{{"base":"{BASE}","available":true}}"#),
    )
    .await;
    assert_eq!(s, 200);
    let (s, v) = get(again.addr, &format!("{STATUS}?model=m"), ADMIN).await;
    assert_eq!((s, v["job"]["id"].as_str()), (200, Some("j")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_reaches_the_running_actor_and_the_owner_records_the_generation_matched_result() {
    let r = rig(None).await;
    let a = r.attach().await;
    let w = a["writer"].as_str().unwrap().to_owned();
    let (s, c) = r.commit(&w, 1, &running(), &json!({})).await;
    assert_eq!(s, 200);
    let epoch = c["epoch"].as_u64().unwrap();
    let (s, v) = r.post(CANCEL, "{malformed").await;
    assert_eq!(s, 202, "{v}");
    assert_eq!(v["id"], "j");
    assert!(v.get("originalText").is_none() && v["models"][0].get("_x").is_none());
    let abort: Vec<_> =
        r.up.seen()
            .into_iter()
            .filter(|s| s.path.ends_with("/abort"))
            .collect();
    assert_eq!(abort.len(), 1);
    assert_eq!(j(&abort[0].body), json!({"jobId":"j","epoch":epoch}));
    assert_eq!(
        abort[0].header("x-noevia-store-token").as_deref(),
        Some(STORE_TOKEN)
    );
    // Cancelling again while the run is still going is accepted, as before.
    assert_eq!(r.post(CANCEL, "").await.0, 202);
    // The actor ends the run through its fenced commit; afterwards nothing is running.
    let (s, _) = r.commit(&w, 1, &finished("cancelled"), &json!({})).await;
    assert_eq!(s, 200);
    let (s, v) = r.post(CANCEL, "").await;
    assert_eq!(
        (s, v["error"].as_str()),
        (409, Some("No auto-tune is running."))
    );
    assert_eq!(r.disk()["job"]["status"], "cancelled");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_actor_that_does_not_take_the_abort_never_gets_a_false_success() {
    let r = rig(None).await;
    let a = r.attach().await;
    let w = a["writer"].as_str().unwrap().to_owned();
    r.commit(&w, 1, &running(), &json!({})).await;
    r.actor.lock().unwrap().abort = Mode::Down;
    assert_eq!(r.post(CANCEL, "").await.0, 503);
    r.actor.lock().unwrap().abort = Mode::Refuse;
    assert_eq!(
        r.post(CANCEL, "").await.0,
        503,
        "an actor that is not running it: bounded retries, then unavailable"
    );
    let tries = r.up.count("/internal/autotune-actor/abort");
    assert!(tries >= 4, "{tries}");
    // The failed attempts withdrew their requests: the run is still cancellable once the actor is back.
    r.actor.lock().unwrap().abort = Mode::Ok;
    assert_eq!(r.post(CANCEL, "").await.0, 202);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_replaced_run_is_not_cancelled_by_a_request_for_the_old_one() {
    let r = rig(None).await;
    let a = r.attach().await;
    let w = a["writer"].as_str().unwrap().to_owned();
    r.commit(&w, 1, &running(), &json!({})).await;
    // The owner's facts name this run; a new run with the same job id registers before the abort.
    let facts =
        r.f.app
            .config
            .autotune
            .call("cancel/facts", &json!({}))
            .await
            .unwrap();
    let token = facts.1.get("token").as_str().unwrap().to_owned();
    let reg = store_at(
        r.f.store,
        "commit",
        &json!({"writer": w, "jobRev": 1, "register": true, "job": running(), "history": {}})
            .to_string(),
    )
    .await;
    assert_eq!(reg.0, 200);
    let abort =
        r.f.app
            .config
            .autotune
            .call("cancel/abort", &json!({"token": token}))
            .await
            .unwrap();
    assert_eq!(abort.0, 409);
    assert_eq!(r.up.count("/internal/autotune-actor/abort"), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clear_removes_only_a_finished_job_and_concurrent_cancel_and_clear_agree() {
    let r = rig(None).await;
    let a = r.attach().await;
    let w = a["writer"].as_str().unwrap().to_owned();
    r.commit(&w, 1, &running(), &json!({"m": [{"at": 1}]}))
        .await;
    // On a running job exactly one of them can win: cancel does, clear never.
    let (c, d) = tokio::join!(r.post(CANCEL, ""), r.post(CLEAR, ""));
    assert_eq!(c.0, 202, "{}", c.1);
    assert_eq!(d.0, 409, "{}", d.1);
    assert_eq!(r.disk()["job"]["status"], "running");
    r.commit(&w, 1, &finished("cancelled"), &json!({"m": [{"at": 1}]}))
        .await;
    // On a finished job clear wins and cancel finds nothing running.
    let (c, d) = tokio::join!(r.post(CANCEL, ""), r.post(CLEAR, ""));
    assert_eq!(c.0, 409, "{}", c.1);
    assert_eq!(d.0, 200, "{}", d.1);
    let disk = r.disk();
    assert!(disk.get("job").is_none(), "{disk}");
    assert_eq!(disk["history"], json!({"m": [{"at": 1}]}));
    // The actor's next save is refused: it holds the job the owner removed; it receives the owner's state.
    let (s, v) = r.commit(&w, 1, &finished("cancelled"), &json!({})).await;
    assert_eq!((s, v["fenced"].as_str()), (409, Some("job")));
    assert!(v["state"]["state"].get("job").is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn settings_follow_the_owners_state_and_the_actors_facts() {
    let r = rig(None).await;
    let a = r.attach().await;
    let w = a["writer"].as_str().unwrap().to_owned();
    let (s, v) = r.post(SETTINGS, r#"{"model":"m","allowQ5Kv":true}"#).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["settings"], json!({"allowQ5Kv": true}));
    assert_eq!(r.disk()["settings"], json!({"m": {"allowQ5Kv": true}}));
    // The actor's own save keeps the owner's settings.
    let (s, c) = r.commit(&w, 1, &running(), &json!({"m": []})).await;
    assert_eq!(s, 200);
    assert_eq!(c["settings"], json!({"m": {"allowQ5Kv": true}}));
    // An unfinished tune of the model blocks the change; the validation order is Core's.
    let (s, v) = r.post(SETTINGS, r#"{"model":"m","allowQ5Kv":false}"#).await;
    assert_eq!(s, 409, "{v}");
    assert_eq!(r.disk()["settings"], json!({"m": {"allowQ5Kv": true}}));
    r.actor.lock().unwrap().unconfigured.insert("x".into());
    assert_eq!(
        r.post(SETTINGS, r#"{"model":"x","allowQ5Kv":true}"#)
            .await
            .0,
        404
    );
    r.actor.lock().unwrap().system.insert("s".into());
    assert_eq!(
        r.post(SETTINGS, r#"{"model":"s","allowQ5Kv":true}"#)
            .await
            .0,
        400
    );
    assert_eq!(
        r.post(SETTINGS, r#"{"model":"","allowQ5Kv":true}"#).await.0,
        400
    );
    assert_eq!(r.post(SETTINGS, r#"{"model":"m"}"#).await.0, 400);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_settings_changes_neither_lose_nor_invent_a_write() {
    let r = rig(None).await;
    r.attach().await;
    let bodies = [
        r#"{"model":"a","allowQ5Kv":true}"#,
        r#"{"model":"b","allowQ5Kv":true}"#,
        r#"{"model":"c","allowQ5Kv":true}"#,
        r#"{"model":"a","allowQ5Kv":false}"#,
    ];
    let results = futures_join(&r, &bodies).await;
    let disk = r.disk()["settings"].clone();
    for (body, (status, _)) in bodies.iter().zip(&results) {
        assert!(matches!(*status, 200 | 409), "{status}");
        let v = j(body);
        let name = v["model"].as_str().unwrap();
        if v["allowQ5Kv"] == json!(true) && *status == 200 {
            // An acknowledged enable stays unless the racing disable also succeeded.
            let disabled = bodies.iter().zip(&results).any(|(b, (s, _))| {
                *s == 200 && j(b)["model"] == name && j(b)["allowQ5Kv"] == json!(false)
            });
            assert!(disabled || disk.get(name).is_some(), "{name} {disk}");
        }
    }
    for k in disk.as_object().unwrap().keys() {
        assert!(
            bodies.iter().zip(&results).any(|(b, (s, _))| *s == 200
                && j(b)["model"] == k.as_str()
                && j(b)["allowQ5Kv"] == json!(true)),
            "{k} was never acknowledged"
        );
    }
}
async fn futures_join(r: &Rig, bodies: &[&str]) -> Vec<(u16, Value)> {
    let mut tasks = Vec::new();
    for b in bodies {
        let (addr, body) = (r.f.addr, (*b).to_owned());
        tasks.push(tokio::spawn(async move {
            write(addr, "POST", SETTINGS, ADMIN, &body).await
        }));
    }
    let mut out = Vec::new();
    for t in tasks {
        out.push(t.await.unwrap());
    }
    out
}

fn interrupted() -> Value {
    let mut v = finished("interrupted");
    v["models"][0]["phases"][0]["restored"] = json!(true);
    v
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resume_prepares_commits_and_launches_the_actor_under_the_owners_hold() {
    let r = rig(None).await;
    let a = r.attach().await;
    let w = a["writer"].as_str().unwrap().to_owned();
    r.commit(&w, 1, &interrupted(), &json!({"m": [{"at": 1}]}))
        .await;
    let (s, v) = r.post(RESUME, r#"{"confirmPause":true}"#).await;
    assert_eq!(s, 202, "{v}");
    assert_eq!(v["status"], "running");
    assert!(v.get("originalText").is_none());
    let seen = r.up.seen();
    let launch = seen
        .iter()
        .find(|s| s.path.ends_with("/launch"))
        .expect("launch");
    let body = j(&launch.body);
    assert_eq!(body["job"]["status"], "running");
    assert_eq!(body["history"], json!({"m": [{"at": 1}]}));
    assert!(body["epoch"].as_u64().unwrap() > a["epoch"].as_u64().unwrap());
    let order: Vec<&str> = seen
        .iter()
        .map(|s| s.path.rsplit('/').next().unwrap())
        .collect();
    let pos = |name: &str| order.iter().position(|p| *p == name).unwrap();
    assert!(
        pos("listing") < pos("identities") && pos("identities") < pos("launch"),
        "{order:?}"
    );
    assert_eq!(r.disk()["job"]["status"], "running");
    // The job is running now: a second resume finds nothing resumable and the hold is free.
    assert_eq!(r.post(RESUME, r#"{"confirmPause":true}"#).await.0, 409);
    let (s, v) = store_at(
        r.f.store,
        "start",
        &json!({"writer": w, "op": "begin"}).to_string(),
    )
    .await;
    assert_eq!(
        (s, v["busy"].as_bool()),
        (409, Some(true)),
        "a running job refuses a start"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_launch_the_actor_does_not_take_leaves_no_running_job_without_a_runner() {
    let r = rig(None).await;
    let a = r.attach().await;
    let w = a["writer"].as_str().unwrap().to_owned();
    r.commit(&w, 1, &interrupted(), &json!({})).await;
    for mode in [Mode::Down, Mode::Refuse] {
        r.actor.lock().unwrap().launch = mode;
        let (s, _) = r.post(RESUME, r#"{"confirmPause":true}"#).await;
        assert_eq!(s, 503);
        let disk = r.disk();
        assert_eq!(disk["job"]["status"], "interrupted", "fail closed");
        assert!(disk["job"]["error"].is_string());
    }
    // The actor, told about the owner's change on its next save, receives the owner's job.
    let (s, v) = r.commit(&w, 1, &finished("failed"), &json!({})).await;
    assert_eq!((s, v["fenced"].as_str()), (409, Some("job")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resume_of_system_models_only_filters_the_queue_without_a_launch() {
    let r = rig(None).await;
    let a = r.attach().await;
    let w = a["writer"].as_str().unwrap().to_owned();
    r.commit(&w, 1, &interrupted(), &json!({})).await;
    r.actor.lock().unwrap().listing_system = true;
    let (s, v) = r.post(RESUME, r#"{"confirmPause":true}"#).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["status"], "passed");
    assert_eq!(r.up.count("/internal/autotune-actor/launch"), 0);
    assert_eq!(r.up.count("/internal/autotune-actor/invalidate"), 1);
    assert_eq!(
        r.disk()["job"]["skipped"][0]["reason"],
        "System routing model — not tuned"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_resume_whose_caller_goes_away_mid_preparation_releases_the_hold() {
    let r = rig(None).await;
    let a = r.attach().await;
    let w = a["writer"].as_str().unwrap().to_owned();
    r.commit(&w, 1, &interrupted(), &json!({})).await;
    // The actor's listing answer is slow; the caller gives up while the owner holds the preparation.
    let slow = Arc::clone(&r.actor);
    let up = r.up.clone();
    up.set(move |req| {
        if req.path.ends_with("/listing") {
            (
                Duration::from_secs(30),
                reply("200 OK", r#"{"models":[]}"#).1,
            )
        } else {
            let a = slow.lock().unwrap();
            let _ = &a;
            let body = j(&req.body);
            if req.path.ends_with("/outside") {
                let mut models = serde_json::Map::new();
                for n in body["models"].as_array().into_iter().flatten() {
                    models.insert(
                        n.as_str().unwrap().into(),
                        json!({"configured": true, "system": false}),
                    );
                }
                reply(
                    "200 OK",
                    &json!({"revision":"r1","allowBelowQ5Kv":false,"models":models}).to_string(),
                )
            } else {
                reply("200 OK", "{}")
            }
        }
    });
    let addr = r.f.addr;
    let call = tokio::spawn(async move {
        write(addr, "POST", RESUME, ADMIN, r#"{"confirmPause":true}"#).await
    });
    r.up.wait_for("/internal/autotune-actor/listing", 1).await;
    // Held: a start is refused while the preparation is in flight.
    let (s, v) = store_at(
        r.f.store,
        "start",
        &json!({"writer": w, "op": "begin"}).to_string(),
    )
    .await;
    assert_eq!((s, v["busy"].as_bool()), (409, Some(true)));
    call.abort();
    let _ = call.await;
    // The front notices the dropped future (its guard): the hold is free again.
    let mut free = false;
    for _ in 0..100 {
        let (s, _) = store_at(
            r.f.store,
            "start",
            &json!({"writer": w, "op": "begin"}).to_string(),
        )
        .await;
        if s == 200 {
            free = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(free, "the preparation hold must not outlive its caller");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_store_channel_needs_the_secret_and_never_binds_a_path() {
    let r = rig(None).await;
    let mut s = TcpStream::connect(r.f.store).await.unwrap();
    let body = format!(r#"{{"base":"{BASE}","available":true}}"#);
    s.write_all(format!("POST /v1/autotune-state/attach HTTP/1.1\r\nhost: store\r\nconnection: close\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).await.unwrap();
    assert!(out.starts_with("HTTP/1.1 401"), "{out}");
    assert!(r.f.app.config.autotune.owner().availability().is_none());
    for base in ["../x.json", "/etc/passwd", "native-autotune-ZZ.json", "x"] {
        let (s, _) = store_at(
            r.f.store,
            "attach",
            &json!({"base": base, "available": true}).to_string(),
        )
        .await;
        assert_eq!(s, 503, "{base}");
    }
    assert!(r.f.app.config.autotune.owner().availability().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auth_role_and_csrf_precede_every_owner_and_actor_call() {
    let r = rig(Some(NODE_STATE)).await;
    r.attach().await;
    let before = r.up.seen().len();
    for path in [CANCEL, CLEAR, SETTINGS, RESUME] {
        assert_eq!(get(r.f.addr, path, "").await.0, 401, "{path}");
        assert_eq!(
            write(r.f.addr, "POST", path, MEMBER, "{}").await.0,
            403,
            "{path}"
        );
        assert_eq!(
            raw(r.f.addr, "POST", path, ADMIN, "", b"{}").await.0,
            403,
            "{path}"
        );
    }
    assert_eq!(get(r.f.addr, STATUS, MEMBER).await.0, 403);
    assert_eq!(r.up.seen().len(), before);
    assert_eq!(
        std::fs::read_to_string(r.d.path().join(BASE)).unwrap(),
        NODE_STATE
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delayed_retired_actor_cannot_reattach_through_the_store() {
    let r = rig(None).await;
    let a = "a".repeat(32);
    let b = "b".repeat(32);
    let open = |actor: &str| json!({"actor":actor,"open":true}).to_string();
    let attach = |actor: &str, incarnation: u64| {
        json!({"actor":actor,"incarnation":incarnation,"base":BASE,"available":true}).to_string()
    };
    assert_eq!(store_at(r.f.store, "attach", &open(&a)).await.0, 200);
    assert_eq!(store_at(r.f.store, "attach", &attach(&a, 1)).await.0, 200);
    assert_eq!(store_at(r.f.store, "attach", &open(&b)).await.0, 200);
    let current = store_at(r.f.store, "attach", &attach(&b, 2)).await;
    assert_eq!(current.0, 200);
    assert_eq!(store_at(r.f.store, "attach", &attach(&a, 1)).await.0, 409);
    assert_eq!(store_at(r.f.store, "attach", &open(&a)).await.0, 409);
    assert_eq!(store_at(r.f.store, "attach", &attach(&b, 2)).await, current);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropped_resume_caller_still_settles_refused_launch() {
    let r = rig(None).await;
    let a = r.attach().await;
    r.commit(a["writer"].as_str().unwrap(), 1, &interrupted(), &json!({}))
        .await;
    let auto = Arc::clone(&r.f.app.config.autotune);
    let facts = auto.call("resume/facts", &json!({})).await.unwrap();
    let token = facts.1.get("token").as_str().unwrap().to_owned();
    // Keep outside responses valid, but block and then refuse the launch.
    r.up.set(|req| {
        if req.path.ends_with("/outside") {
            reply("200 OK",r#"{"revision":"r1","allowBelowQ5Kv":false,"models":{"m":{"configured":true,"system":false}}}"#)
        } else {
            (Duration::from_millis(200),reply("200 OK",r#"{"launched":false}"#).1)
        }
    });
    let caller = tokio::spawn(async move {
        auto.call(
            "resume/commit",
            &json!({"token":token,"plan":{"kind":"launch","reset":[]}}),
        )
        .await
    });
    r.up.wait_for("/internal/autotune-actor/launch", 1).await;
    caller.abort();
    let _ = caller.await;
    for _ in 0..100 {
        if r.disk()["job"]["status"] == "interrupted" {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("cancelled caller abandoned a persisted launch");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropped_cancel_caller_still_undoes_refused_abort() {
    let r = rig(None).await;
    let a = r.attach().await;
    r.commit(a["writer"].as_str().unwrap(), 1, &running(), &json!({}))
        .await;
    let auto = Arc::clone(&r.f.app.config.autotune);
    let before = auto
        .owner()
        .state_snapshot()
        .unwrap()
        .get("rev")
        .as_num()
        .unwrap();
    let facts = auto.call("cancel/facts", &json!({})).await.unwrap();
    let token = facts.1.get("token").as_str().unwrap().to_owned();
    r.up.set(|_| {
        (
            Duration::from_millis(200),
            reply("200 OK", r#"{"aborted":false}"#).1,
        )
    });
    let worker = Arc::clone(&auto);
    let caller =
        tokio::spawn(async move { worker.call("cancel/abort", &json!({"token":token})).await });
    r.up.wait_for("/internal/autotune-actor/abort", 1).await;
    caller.abort();
    let _ = caller.await;
    for _ in 0..100 {
        if auto
            .owner()
            .state_snapshot()
            .unwrap()
            .get("rev")
            .as_num()
            .unwrap()
            >= before + 2.0
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("cancelled caller abandoned its abort request");
}
