//! The model folder sync worker end to end against synthetic services: the real `Maint` (the exclusion is a
//! lease on Node's maintenance gate, as in production without an admin token), the real model-manager
//! client, the real settled-file check over a synthetic models folder, and fakes for the engine router,
//! Node's lease route and the model management service. No real model, account or credential.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
mod maint_support;
use maint_support::*;
use noevia_server::model_folder_sync::{live::Live, Env, Worker};
use noevia_server::{config::Config, App};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const SECRET: &str = "synthetic-store-token-0123456789abcdef";
const LEASE: &str = "/internal/model-maintenance/lease";
const ID: &str = "0123456789abcdef0123456789abcdef";
const GRANT: &str =
    r#"{"lease":"0123456789abcdef0123456789abcdef","ttlMs":180000,"renewMs":100,"hardMs":3600000}"#;

type Order = Arc<Mutex<Vec<String>>>;

struct Rig {
    dir: tempfile::TempDir,
    order: Order,
    app: Arc<App>,
    ini: std::path::PathBuf,
}

fn gguf(path: &Path, age: Duration) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut bytes = b"GGUF".to_vec();
    bytes.extend_from_slice(&[0u8; 60]);
    std::fs::write(path, bytes).unwrap();
    let f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    f.set_modified(std::time::SystemTime::now() - age).unwrap();
}

async fn rig(
    stems: &'static [&'static str],
    node_renew: fn(usize) -> Answer,
    loader_delay: fn(&str) -> Duration,
) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let models = dir.path().join("models");
    std::fs::create_dir_all(&models).unwrap();
    let ini = dir.path().join("models.ini");
    std::fs::write(&ini, "[A]\nmodel = /models/a.gguf\n").unwrap();
    let order: Order = Arc::default();
    let renews = Arc::new(AtomicUsize::new(0));
    // The engine router (no gate: the router's own 404 for /gate/).
    let o = Arc::clone(&order);
    let (router, _) = engine(Arc::new(move |req: &Req| {
        if req.target.starts_with("/gate/v1/") {
            return Answer::Reply(404, r#"{"error":"not found"}"#.into());
        }
        o.lock()
            .unwrap()
            .push(format!("router {} {}", req.method, req.target));
        match (req.method.as_str(), req.target.as_str()) {
            ("GET", "/models") => Answer::Reply(200, r#"{"data":[]}"#.into()),
            ("GET", "/models?reload=1") => Answer::Reply(200, r#"{"success":true}"#.into()),
            _ => Answer::Reply(404, "{}".into()),
        }
    }))
    .await;
    // Node's maintenance gate: the lease route.
    let o = Arc::clone(&order);
    let (node, _) = engine(Arc::new(move |req: &Req| {
        assert_eq!(
            req.headers.get("x-noevia-store-token").map(String::as_str),
            Some(SECRET)
        );
        o.lock()
            .unwrap()
            .push(format!("node {} {}", req.method, req.target));
        if req.method == "POST" && req.target == LEASE {
            Answer::Reply(201, GRANT.into())
        } else if req.method == "POST" {
            node_renew(renews.fetch_add(1, Ordering::SeqCst))
        } else {
            Answer::Reply(204, String::new())
        }
    }))
    .await;
    // The model management service: the scan and the safe-defaults registration (it writes models.ini).
    let o = Arc::clone(&order);
    let ini2 = ini.clone();
    let (loader, _) = engine(Arc::new(move |req: &Req| {
        o.lock()
            .unwrap()
            .push(format!("loader {} {}", req.method, req.target));
        if req.method == "GET" && req.target == "/api/v1/models" {
            let have = std::fs::read_to_string(&ini2).unwrap();
            let list: Vec<&str> = stems
                .iter()
                .copied()
                .filter(|s| !have.contains(&format!("[{s}]")))
                .collect();
            return Answer::Reply(
                200,
                serde_json::json!({"models": [], "unregistered": list}).to_string(),
            );
        }
        if let Some(stem) = req
            .target
            .strip_prefix("/api/v1/sections/")
            .and_then(|r| r.strip_suffix("/safe-defaults"))
        {
            let mut text = std::fs::read_to_string(&ini2).unwrap();
            text.push_str(&format!("[{stem}]\nmodel = /models/{stem}.gguf\n"));
            std::fs::write(&ini2, &text).unwrap();
            let body = serde_json::json!({"ok": true, "revision": hex(&text)}).to_string();
            return Answer::After(loader_delay(stem), 200, body);
        }
        Answer::Reply(404, "{}".into())
    }))
    .await;
    let env = [
        ("MODEL_MANAGER_KIND", "llamacpp".to_owned()),
        (
            "MODEL_MANAGER_BASE_URL",
            format!("http://127.0.0.1:{router}/v1"),
        ),
        ("LLAMACPP_PRESET_PATH", ini.display().to_string()),
        ("LLAMACPP_MODELS_PATH", models.display().to_string()),
        ("MODEL_LOADER_URL", format!("http://127.0.0.1:{loader}")),
        ("NOEVIA_LEGACY_UPSTREAM", format!("http://127.0.0.1:{node}")),
        ("NOEVIA_PROJECT_STORE_TOKEN", SECRET.to_owned()),
        ("NOEVIA_PROJECT_STORE_PORT", String::new()),
        ("UI_DATA_DIR", dir.path().display().to_string()),
    ];
    let get = move |k: &str| env.iter().find(|(n, _)| *n == k).map(|(_, v)| v.clone());
    let app = App::new(Config::from_lookup(get).unwrap());
    Rig {
        dir,
        order,
        app,
        ini,
    }
}

fn live(r: &Rig) -> Live {
    let env = [(
        "LLAMACPP_MODELS_PATH",
        r.dir.path().join("models").display().to_string(),
    )];
    Live::from_lookup(
        &r.app.config.maint,
        &r.app.config.model_manager_scan,
        &|k| env.iter().find(|(n, _)| *n == k).map(|(_, v)| v.clone()),
    )
    .expect("a configured front")
    .with_timing(Duration::from_secs(60), Duration::from_millis(10))
}

fn worker(r: &Rig) -> (Worker<Live>, Arc<Mutex<Vec<String>>>) {
    let logs: Arc<Mutex<Vec<String>>> = Arc::default();
    let sink = Arc::clone(&logs);
    let w = Worker::new(
        live(r),
        Some(r.dir.path().join("model-folder-sync.json")),
        Arc::new(move |m: &str| sink.lock().unwrap().push(m.to_owned())),
    );
    (w, logs)
}

fn ok(_: usize) -> Answer {
    Answer::Reply(200, "{}".into())
}
fn instant(_: &str) -> Duration {
    Duration::ZERO
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_new_file_is_registered_and_reloaded_inside_one_lease() {
    let r = rig(&["fresh"], ok, instant).await;
    gguf(
        &r.dir.path().join("models/fresh.gguf"),
        Duration::from_secs(3600),
    );
    let (w, logs) = worker(&r);
    let out = w.run().await;
    assert_eq!(
        out.added,
        vec!["fresh"],
        "{:?} {:?}",
        out,
        logs.lock().unwrap()
    );
    let seen = r.order.lock().unwrap().clone();
    let at = |what: &str| {
        seen.iter()
            .position(|e| e == what)
            .unwrap_or_else(|| panic!("{what} in {seen:?}"))
    };
    let lease = at(&format!("node POST {LEASE}"));
    let register = at("loader POST /api/v1/sections/fresh/safe-defaults");
    let reload = at("router GET /models?reload=1");
    let release = at(&format!("node DELETE {LEASE}/{ID}"));
    assert!(
        lease < register && register < reload && reload < release,
        "{seen:?}"
    );
    assert_eq!(
        seen.iter().filter(|e| e.starts_with("loader GET")).count(),
        2,
        "scan, then again under the lease"
    );
    assert!(std::fs::read_to_string(&r.ini).unwrap().contains("[fresh]"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_busy_node_gate_defers_without_touching_anything() {
    let r = rig(&["fresh"], ok, instant).await;
    // Replace Node's answer: the lease is refused (a chat or a tune is running).
    let dir = r.dir.path().to_path_buf();
    let (busy_node, _) = engine(Arc::new(|_: &Req| {
        Answer::Reply(409, r#"{"error":"busy"}"#.into())
    }))
    .await;
    let env = [
        ("MODEL_MANAGER_KIND", "llamacpp".to_owned()),
        ("MODEL_MANAGER_BASE_URL", "http://127.0.0.1:9/v1".to_owned()),
        ("LLAMACPP_PRESET_PATH", r.ini.display().to_string()),
        (
            "LLAMACPP_MODELS_PATH",
            dir.join("models").display().to_string(),
        ),
        (
            "MODEL_LOADER_URL",
            format!("http://127.0.0.1:{}", {
                let o = Arc::clone(&r.order);
                engine(Arc::new(move |req: &Req| {
                    o.lock()
                        .unwrap()
                        .push(format!("loader {} {}", req.method, req.target));
                    Answer::Reply(200, r#"{"models":[],"unregistered":["fresh"]}"#.into())
                }))
                .await
                .0
            }),
        ),
        (
            "NOEVIA_LEGACY_UPSTREAM",
            format!("http://127.0.0.1:{busy_node}"),
        ),
        ("NOEVIA_PROJECT_STORE_TOKEN", SECRET.to_owned()),
        ("NOEVIA_PROJECT_STORE_PORT", String::new()),
        ("UI_DATA_DIR", dir.display().to_string()),
    ];
    let get = move |k: &str| env.iter().find(|(n, _)| *n == k).map(|(_, v)| v.clone());
    let app = App::new(Config::from_lookup(get).unwrap());
    gguf(&dir.join("models/fresh.gguf"), Duration::from_secs(3600));
    let env2 = [(
        "LLAMACPP_MODELS_PATH",
        dir.join("models").display().to_string(),
    )];
    let live = Live::from_lookup(&app.config.maint, &app.config.model_manager_scan, &|k| {
        env2.iter().find(|(n, _)| *n == k).map(|(_, v)| v.clone())
    })
    .unwrap();
    let w = Worker::new(live, None, Arc::new(|_| {}));
    let out = w.run().await;
    assert!(out.deferred && out.added.is_empty(), "{out:?}");
    assert!(w.skipped().is_empty(), "busy is never a refusal");
    let seen = r.order.lock().unwrap().clone();
    assert!(
        seen.iter().all(|e| e == "loader GET /api/v1/models"),
        "only the scan happened: {seen:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lease_node_stops_confirming_ends_the_batch_with_no_reload() {
    // The first registration takes 700 ms; Node answers the first renewal and then forgets the lease.
    fn forgets(n: usize) -> Answer {
        if n == 0 {
            Answer::Reply(200, "{}".into())
        } else {
            Answer::Reply(404, "{}".into())
        }
    }
    fn slow(stem: &str) -> Duration {
        if stem == "first" {
            Duration::from_millis(700)
        } else {
            Duration::ZERO
        }
    }
    let r = rig(&["first", "second"], forgets, slow).await;
    gguf(
        &r.dir.path().join("models/first.gguf"),
        Duration::from_secs(3600),
    );
    gguf(
        &r.dir.path().join("models/second.gguf"),
        Duration::from_secs(3600),
    );
    let (w, logs) = worker(&r);
    let out = w.run().await;
    let seen = r.order.lock().unwrap().clone();
    assert!(out.lost && out.deferred, "{out:?} {seen:?}");
    assert!(
        !seen.iter().any(|e| e.contains("sections/second")),
        "{seen:?}"
    );
    assert!(
        !seen.iter().any(|e| e == "router GET /models?reload=1"),
        "no reload after the lease was lost: {seen:?}"
    );
    assert_eq!(
        seen.iter()
            .filter(|e| **e == format!("node POST {LEASE}"))
            .count(),
        1,
        "never retaken: {seen:?}"
    );
    assert!(
        logs.lock()
            .unwrap()
            .iter()
            .any(|l| l.contains("next reload") || l.contains("exclusion")),
        "{:?}",
        logs.lock().unwrap()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn partial_and_unsafe_files_are_never_registered() {
    let r = rig(
        &["young", "downloading", "linked", "plain", "good"],
        ok,
        instant,
    )
    .await;
    let m = r.dir.path().join("models");
    gguf(&m.join("young.gguf"), Duration::from_secs(1));
    gguf(&m.join("downloading.gguf"), Duration::from_secs(3600));
    std::fs::write(m.join("downloading.gguf.download"), b"partial").unwrap();
    // A link to a file outside the models folder, and a plain file with no header.
    let outside = r.dir.path().join("outside.gguf");
    gguf(&outside, Duration::from_secs(3600));
    std::os::unix::fs::symlink(&outside, m.join("linked.gguf")).unwrap();
    std::fs::write(m.join("plain.gguf"), vec![0u8; 64]).unwrap();
    let f = std::fs::OpenOptions::new()
        .write(true)
        .open(m.join("plain.gguf"))
        .unwrap();
    f.set_modified(std::time::SystemTime::now() - Duration::from_secs(3600))
        .unwrap();
    gguf(&m.join("good.gguf"), Duration::from_secs(3600));
    let (w, _) = worker(&r);
    let out = w.run().await;
    assert_eq!(out.added, vec!["good"], "{out:?}");
    assert_eq!(out.unsettled.len(), 4, "{out:?}");
    let seen = r.order.lock().unwrap().clone();
    for bad in ["young", "downloading", "linked", "plain"] {
        assert!(
            !seen.iter().any(|e| e.contains(&format!("sections/{bad}/"))),
            "{bad} {seen:?}"
        );
    }
    assert!(
        w.skipped().is_empty(),
        "an unsettled file is looked at again, never remembered"
    );
}

#[tokio::test]
async fn the_hold_is_given_back_when_the_run_is_dropped() {
    let r = rig(&["fresh"], ok, |_| Duration::from_secs(30)).await;
    gguf(
        &r.dir.path().join("models/fresh.gguf"),
        Duration::from_secs(3600),
    );
    let (w, _) = worker(&r);
    let w = Arc::new(w);
    let task = {
        let w = Arc::clone(&w);
        tokio::spawn(async move { w.run().await })
    };
    // In the registration call, under the lease.
    let started = std::time::Instant::now();
    while !r
        .order
        .lock()
        .unwrap()
        .iter()
        .any(|e| e.contains("safe-defaults"))
    {
        assert!(started.elapsed() < Duration::from_secs(10));
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    task.abort();
    let _ = task.await;
    let started = std::time::Instant::now();
    while !r
        .order
        .lock()
        .unwrap()
        .iter()
        .any(|e| e.starts_with("node DELETE"))
    {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the lease was not given back: {:?}",
            r.order.lock().unwrap()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // And the worker is usable again.
    assert!(!live(&r).busy());
}
