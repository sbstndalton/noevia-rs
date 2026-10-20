#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
//! The actual Python differential and the ingest qualification: every pass of
//! `tests/fixtures/telemetry-ingest-python.v1.json` (the services app's own `telemetry.ingest`,
//! `stats_for` and `config_history`, recorded by `tools/gen-telemetry-ingest-oracle.py`) is
//! replayed against a store made from the app's own schema, the same log bytes through a stand-in
//! Docker daemon and the same clock. After every pass the telemetry tables and keys, and the
//! section's measurements as the route renders them, must be Python's. The passes cover a first
//! read, the interval, repeated reads, a restart, port reuse with the spawn scrolled out, a gap, an
//! interrupted write and its recovery, a parse that raises, a multiplexed log with a truncated
//! frame and invalid UTF-8, and unreadable stored keys. Every name is synthetic.
use super::*;
use crate::backend_stats::{Config, DockerApi, Raw};
use crate::Job;
use serde_json::Value as V;
use std::collections::HashMap;
use std::sync::Mutex;

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

fn fixture() -> V {
    serde_json::from_str(include_str!(
        "../../../tests/fixtures/telemetry-ingest-python.v1.json"
    ))
    .unwrap()
}

/// One pass's Docker: each name's log bytes (`None`: the read fails) and whether it has a TTY.
type PassLogs = HashMap<String, (Option<Vec<u8>>, bool)>;

/// Log bytes standing for an answer the transport refused past its cap.
const PAST_CAP: &[u8] = b"synthetic answer past the cap";

/// The logs of one pass, and how many log reads were made.
#[derive(Default)]
struct Daemon(
    Mutex<PassLogs>,
    std::sync::atomic::AtomicUsize,
    Mutex<Option<std::path::PathBuf>>,
);

fn id_of(name: &str) -> String {
    format!(
        "{:0>64}",
        name.bytes().map(|b| format!("{b:02x}")).collect::<String>()
    )
    .chars()
    .rev()
    .take(64)
    .collect()
}

impl DockerApi for Daemon {
    fn get<'a>(&'a self, path: &'a str) -> Job<'a, Raw> {
        let logs = self.0.lock().unwrap().clone();
        Box::pin(async move {
            let ok = |body: Vec<u8>| Raw {
                status: 200,
                reason: "OK".into(),
                body,
            };
            let missing = Raw {
                status: 404,
                reason: "Not Found".into(),
                body: br#"{"message":"No such container"}"#.to_vec(),
            };
            if path == "/version" {
                return Ok(ok(br#"{"ApiVersion":"1.47"}"#.to_vec()));
            }
            if path.starts_with("/containers/json") {
                return Ok(ok(b"[]".to_vec()));
            }
            let (p, query) = path.split_once('?').unwrap_or((path, ""));
            let parts: Vec<&str> = p.split('/').collect();
            match parts.as_slice() {
                ["", "containers", name, "json"] => match logs.get(*name) {
                    Some((_, tty)) => Ok(ok(serde_json::json!({
                        "Id": id_of(name), "Name": format!("/{name}"), "Image": "sha256:00",
                        "Config": {"Tty": tty},
                    })
                    .to_string()
                    .into_bytes())),
                    None => Ok(missing),
                },
                ["", "containers", id, "logs"] => {
                    assert_eq!(query, crate::docker::TELEMETRY_LOGS_QUERY);
                    let previous = self.1.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    if previous > 0 {
                        if let Some(path) = self.2.lock().unwrap().as_ref() {
                            assert!(
                                rows(path) > 0,
                                "previous tail must be stored before reading the next"
                            );
                        }
                    }
                    let found = logs.iter().find(|(n, _)| id_of(n) == *id);
                    match found {
                        Some((_, (Some(bytes), _))) if bytes == PAST_CAP => {
                            Err(crate::docker::EXCEEDED)
                        }
                        Some((_, (Some(bytes), _))) => Ok(ok(bytes.clone())),
                        _ => Ok(Raw {
                            status: 500,
                            reason: "Internal Server Error".into(),
                            body: br#"{"message":"synthetic docker failure"}"#.to_vec(),
                        }),
                    }
                }
                _ => Ok(missing),
            }
        })
    }
}

fn store(dir: &std::path::Path, schema: &V) -> std::path::PathBuf {
    let path = dir.join("model_loader.db");
    let db = rusqlite::Connection::open(&path).unwrap();
    let _: String = db
        .query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))
        .unwrap();
    for sql in schema.as_array().unwrap() {
        db.execute_batch(sql.as_str().unwrap()).unwrap();
    }
    path
}

fn cell(v: rusqlite::types::ValueRef<'_>) -> V {
    use rusqlite::types::ValueRef;
    match v {
        ValueRef::Null => V::Null,
        ValueRef::Integer(i) => V::from(i),
        ValueRef::Real(r) => V::String(gguf::py_float_repr(r)),
        ValueRef::Text(t) => V::String(String::from_utf8(t.to_vec()).unwrap()),
        ValueRef::Blob(_) => panic!("blob"),
    }
}

fn dump(path: &std::path::Path) -> V {
    let db = rusqlite::Connection::open(path).unwrap();
    let rows = |sql: &str| -> V {
        let mut q = db.prepare(sql).unwrap();
        let n = q.column_count();
        let out: Vec<V> = q
            .query_map([], |r| {
                Ok(V::Array(
                    (0..n).map(|i| cell(r.get_ref(i).unwrap())).collect(),
                ))
            })
            .unwrap()
            .map(Result::unwrap)
            .collect();
        V::Array(out)
    };
    serde_json::json!({
        "req_timing": rows("SELECT id, ts, backend, instance, task, model_path, alias, spec_type, prompt_tokens, prompt_tps, gen_tokens, gen_tps, draft_acc, draft_len FROM req_timing ORDER BY id"),
        "server_config": rows("SELECT backend, instance, alias, model_path, argv_json, first_seen FROM server_config ORDER BY backend, instance"),
        "kv": rows("SELECT key, value FROM kv WHERE key LIKE 'telemetry%' ORDER BY key"),
    })
}

fn world(
    names: &[String],
    daemon: Arc<Daemon>,
    dir: &std::path::Path,
    clock: Arc<Mutex<f64>>,
) -> Telemetry {
    let stats = BackendStats::new(
        Config {
            configured: names.to_vec(),
            gpu_vram: Vec::new(),
            default_port: Some(8080),
            drm_root: dir.join("drm"),
            meminfo: dir.join("meminfo"),
            ram_reserve_gb: Some(32.0),
        },
        daemon,
    )
    .unwrap();
    Telemetry::with_clock(
        dir,
        Arc::new(stats),
        Arc::new(move || *clock.lock().unwrap()),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_pass_is_pythons() {
    let fx = fixture();
    // docker-py's own call.
    assert_eq!(
        fx["asked"],
        serde_json::json!([
            r#"[["stderr", true], ["stdout", true], ["tail", 20000], ["timestamps", true]]"#
        ])
    );
    let dir = tempfile::tempdir().unwrap();
    let path = store(dir.path(), &fx["schema"]);
    let names: Vec<String> = fx["names"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n.as_str().unwrap().to_owned())
        .collect();
    let daemon = Arc::new(Daemon::default());
    let clock = Arc::new(Mutex::new(0.0));
    let telemetry = world(&names, daemon.clone(), dir.path(), clock.clone());
    let passes = fx["passes"].as_array().unwrap();
    assert!(passes.len() >= 12);
    for p in passes {
        let id = p["id"].as_str().unwrap();
        {
            let mut logs = daemon.0.lock().unwrap();
            logs.clear();
            for (name, data) in p["logs"].as_object().unwrap() {
                let tty = p["tty"].get(name).and_then(V::as_bool).unwrap_or(true);
                let bytes = data.as_str().filter(|d| *d != "raise").map(unhex);
                logs.insert(name.clone(), (bytes, tty));
            }
        }
        {
            let db = rusqlite::Connection::open(&path).unwrap();
            for (k, v) in p["kv"].as_object().unwrap() {
                db.execute(
                    "INSERT INTO kv(key, value) VALUES(?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                    rusqlite::params![k, v.as_str().unwrap()],
                )
                .unwrap();
            }
            if p["trigger"].as_bool().unwrap() {
                db.execute_batch(fx["trigger"].as_str().unwrap()).unwrap();
            }
        }
        *clock.lock().unwrap() = p["now"].as_str().unwrap().parse().unwrap();
        let ingest = telemetry.ingest().await;
        let want = p["ingest"].as_str().unwrap();
        match want {
            "ok" => assert_eq!(ingest, Ok(()), "{id}"),
            _ => assert_eq!(ingest, Err(Stop::Raised), "{id}: Python raised {want}"),
        }
        rusqlite::Connection::open(&path)
            .unwrap()
            .execute_batch("DROP TRIGGER IF EXISTS synthetic_interrupt")
            .unwrap();
        assert_eq!(dump(&path), p["store"], "{id}: store");
        let shown = telemetry.read("syn-a", "/models/syn/a.gguf").await.unwrap();
        assert_eq!(
            shown.measured,
            p["measured"].as_str().unwrap(),
            "{id}: measured"
        );
        assert_eq!(
            shown.history,
            p["history"].as_str().unwrap(),
            "{id}: history"
        );
    }
}

/// The route's view: an ingest that raises is the empty measurements (Python's `except`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_raising_ingest_is_the_empty_measurements() {
    let fx = fixture();
    let dir = tempfile::tempdir().unwrap();
    let path = store(dir.path(), &fx["schema"]);
    let daemon = Arc::new(Daemon::default());
    let raising = fx["passes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == "raises")
        .unwrap();
    let bytes = unhex(raising["logs"]["syn-a-backend"].as_str().unwrap());
    daemon
        .0
        .lock()
        .unwrap()
        .insert("syn-a-backend".into(), (Some(bytes), true));
    let clock = Arc::new(Mutex::new(1.0e9));
    let telemetry = world(&["syn-a-backend".into()], daemon, dir.path(), clock);
    let shown = telemetry
        .measure("syn-a", "/models/syn/a.gguf")
        .await
        .unwrap();
    assert_eq!(shown.measured, empty().measured);
    assert_eq!(shown.history, "[]");
    // Nothing of the raising pass was stored, and the interval key was not set.
    assert_eq!(dump(&path)["kv"], V::Array(Vec::new()));
}

/// The first pass of the oracle's log of `syn-a-backend` (a spawn and six requests).
fn first_log(fx: &V) -> Vec<u8> {
    unhex(fx["passes"][0]["logs"]["syn-a-backend"].as_str().unwrap())
}

fn kv_keys(path: &std::path::Path) -> Vec<String> {
    dump(path)["kv"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r[0].as_str().unwrap().to_owned())
        .collect()
}

fn rows(path: &std::path::Path) -> usize {
    dump(path)["req_timing"].as_array().unwrap().len()
}

/// A bound skips the backend as a failed read does (nothing of it written, its keys kept, the
/// other backends ingested), and it is not read again until the backoff is over.
async fn skipped_then_backed_off(bad: Vec<u8>, pre: &[(&str, &str)]) {
    let fx = fixture();
    let dir = tempfile::tempdir().unwrap();
    let path = store(dir.path(), &fx["schema"]);
    {
        let db = rusqlite::Connection::open(&path).unwrap();
        for (k, v) in pre {
            db.execute(
                "INSERT INTO kv(key, value) VALUES(?, ?)",
                rusqlite::params![k, v],
            )
            .unwrap();
        }
    }
    let daemon = Arc::new(Daemon::default());
    daemon
        .0
        .lock()
        .unwrap()
        .insert("syn-x-backend".into(), (Some(bad), true));
    daemon
        .0
        .lock()
        .unwrap()
        .insert("syn-a-backend".into(), (Some(first_log(&fx)), true));
    let clock = Arc::new(Mutex::new(1.0e9));
    let telemetry = world(
        &["syn-x-backend".into(), "syn-a-backend".into()],
        daemon.clone(),
        dir.path(),
        clock.clone(),
    );
    let reads = || daemon.1.load(std::sync::atomic::Ordering::SeqCst);
    let mut want_keys: Vec<String> = pre.iter().map(|(k, _)| (*k).to_owned()).collect();
    want_keys.extend([
        "telemetry_continuous_since:syn-a-backend".to_owned(),
        "telemetry_last_ingest".to_owned(),
        "telemetry_log_watermark:syn-a-backend".to_owned(),
    ]);
    want_keys.sort();
    let pre_values = dump(&path)["kv"].clone();

    assert_eq!(telemetry.ingest().await, Ok(()));
    assert_eq!(reads(), 2);
    assert_eq!(kv_keys(&path), want_keys);
    let syn_x: Vec<V> = dump(&path)["kv"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r[0].as_str().unwrap().ends_with(":syn-x-backend"))
        .cloned()
        .collect();
    assert_eq!(
        V::Array(syn_x),
        pre_values,
        "the skipped backend's keys stay"
    );
    let stored = rows(&path);
    assert!(stored > 0);
    assert!(dump(&path)["req_timing"]
        .as_array()
        .unwrap()
        .iter()
        .all(|r| r[2] == "syn-a-backend"));
    // The route answers measurements, not the 503.
    assert!(telemetry.read("syn-a", "/models/syn/a.gguf").await.is_ok());

    // Past the interval, within the backoff: only the other backend is read.
    *clock.lock().unwrap() += 30.0;
    assert_eq!(telemetry.ingest().await, Ok(()));
    assert_eq!(reads(), 3);
    // Past the backoff: read again (and skipped again, logged only the first time).
    *clock.lock().unwrap() += BACKOFF_S;
    assert_eq!(telemetry.ingest().await, Ok(()));
    assert_eq!(reads(), 5);
    assert_eq!(rows(&path), stored);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_log_past_64_mib_is_skipped_and_backed_off() {
    skipped_then_backed_off(PAST_CAP.to_vec(), &[]).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tail_of_too_many_lines_is_skipped_and_backed_off() {
    skipped_then_backed_off("x\n".repeat(MAX_LINES + 1).into_bytes(), &[]).await;
}

/// A gap-free tail (its first line at the watermark) whose requests name more stored-instance
/// lookups than the bound.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tail_of_too_many_ports_is_skipped_and_backed_off() {
    let mut log = String::new();
    for p in 0..=MAX_PORTS {
        log.push_str(&format!(
            "2026-10-18T10:00:03.123456789Z [{}] x slot print_timing: id  0 | task 1 |        \
             eval time =  777.37 ms /  111 tokens (7.07 ms per token,  141.5 tokens per second)\n",
            20000 + p
        ));
    }
    let first = bt::log_span(&log).unwrap().0;
    let mark = gguf::py_float_repr(first);
    skipped_then_backed_off(
        log.into_bytes(),
        &[
            ("telemetry_continuous_since:syn-x-backend", "1.0"),
            ("telemetry_log_watermark:syn-x-backend", &mark),
        ],
    )
    .await;
    // One port fewer is ingested.
    let fx = fixture();
    let dir = tempfile::tempdir().unwrap();
    let path = store(dir.path(), &fx["schema"]);
    let mut log = String::new();
    for p in 0..MAX_PORTS {
        log.push_str(&format!(
            "2026-10-18T10:00:03.123456789Z [{}] x slot print_timing: id  0 | task 1 |        \
             eval time =  777.37 ms /  111 tokens (7.07 ms per token,  141.5 tokens per second)\n",
            20000 + p
        ));
    }
    let daemon = Arc::new(Daemon::default());
    daemon
        .0
        .lock()
        .unwrap()
        .insert("syn-x-backend".into(), (Some(log.into_bytes()), true));
    {
        let db = rusqlite::Connection::open(&path).unwrap();
        db.execute_batch(&format!(
            "INSERT INTO kv(key, value) VALUES('telemetry_continuous_since:syn-x-backend', '1.0');
             INSERT INTO kv(key, value) VALUES('telemetry_log_watermark:syn-x-backend', '{mark}');"
        ))
        .unwrap();
    }
    let telemetry = world(
        &["syn-x-backend".into()],
        daemon,
        dir.path(),
        Arc::new(Mutex::new(1.0e9)),
    );
    assert_eq!(telemetry.ingest().await, Ok(()));
    assert_eq!(rows(&path), MAX_PORTS);
}

/// A stored argv that is not an object of strings is not followed: the native 503.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shapes_not_followed_refuse() {
    let fx = fixture();
    let dir = tempfile::tempdir().unwrap();
    let path = store(dir.path(), &fx["schema"]);
    let telemetry = world(
        &["syn-a-backend".into()],
        Arc::new(Daemon::default()),
        dir.path(),
        Arc::new(Mutex::new(1.0e9)),
    );
    // A stored argv Python would carry on with as a list.
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute(
        "INSERT INTO req_timing(ts, backend, instance, task, alias, gen_tokens, gen_tps) VALUES(5.0, 'b', 'i', 1, 'syn-a', 20, 1.0)",
        [],
    )
    .unwrap();
    db.execute(
        "INSERT INTO server_config(backend, instance, argv_json) VALUES('b', 'i', '[1]')",
        [],
    )
    .unwrap();
    assert_eq!(telemetry.read("syn-a", "").await.err(), Some(Stop::Refused));
}

#[test]
fn stored_keys_read_as_python_reads_them() {
    use store::py_float;
    assert_eq!(py_float("").unwrap(), 0.0);
    assert_eq!(py_float(" 12.5\n").unwrap(), 12.5);
    assert_eq!(py_float("x").unwrap(), 0.0);
    assert!(py_float(" nan ").unwrap().is_nan());
    assert_eq!(py_float("-Infinity").unwrap(), f64::NEG_INFINITY);
    assert!(py_float("1_000").is_err());
    assert!(py_float("\u{0661}").is_err());
}

#[test]
fn argv_is_written_as_json_dumps_sort_keys_writes_it() {
    let mut argv = bt::Dict::new();
    argv.insert("--z".into(), "1".into());
    argv.insert(
        "--a".into(),
        "\u{e9} \"q\" \u{2603} \u{1f600} \u{7f}\n".into(),
    );
    assert_eq!(
        store::dumps_sorted(&argv),
        r#"{"--a": "\u00e9 \"q\" \u2603 \ud83d\ude00 \u007f\n", "--z": "1"}"#
    );
    assert_eq!(store::dumps_sorted(&bt::Dict::new()), "{}");
}

/// The route with native telemetry never makes the private runtime call, and answers as the
/// child-backed route answers when the child carries the same measurements and list.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_route_no_longer_asks_the_child() {
    use crate::section_autoconfig::tests::{first_case, run, shown, stand_in};
    let fx = fixture();
    let fit = crate::backend_stats::fit::tests::fixture();
    let fit_case = fit["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == "baselines")
        .unwrap();
    let data = tempfile::tempdir().unwrap();
    store(data.path(), &fx["schema"]);
    let stats = Arc::new(crate::backend_stats::fit::tests::stats_for(
        fit_case,
        data.path(),
    ));
    let (base, stand) = stand_in().await;
    let client = reqwest::Client::new();
    let section = crate::section_autoconfig::tests::fixture();
    let path = section["cases"][0]["results"][0]["path"]
        .as_str()
        .unwrap()
        .to_owned();

    let models = tempfile::tempdir().unwrap();
    let (native, _) = first_case(models.path());
    native.set_stats(stats.clone());
    native.set_telemetry(Arc::new(Telemetry::with_clock(
        data.path(),
        stats.clone(),
        Arc::new(|| 1.0e9),
    )));
    stand.lock().unwrap().guard = true;
    stand.lock().unwrap().reply = Some((500, "child asked".into()));
    let got = shown(run(&native, &path, &base, &client).await);
    assert_eq!(got.0, 200, "{}", got.1);
    assert!(
        stand.lock().unwrap().seen.is_empty(),
        "the runtime call was made"
    );

    let list =
        crate::section_autoconfig::inventory::backend_list(&stats.descriptors().await.unwrap())
            .unwrap();
    let mut text = String::new();
    crate::section_autoconfig::render::write(&mut text, &list);
    let e = empty();
    let runtime = format!(
        "{{\"measured\":{},\"history\":{},\"promptTps\":\"0.0\",\"backends\":{}}}",
        e.measured,
        e.history,
        crate::model_updates::json_string(&text)
    );
    stand.lock().unwrap().reply = Some((200, runtime));
    let models = tempfile::tempdir().unwrap();
    let (child_backed, _) = first_case(models.path());
    let want = shown(run(&child_backed, &path, &base, &client).await);
    assert_eq!(got, want);
}

/// A guarded-but-unadvertised child cannot safely be used as an ingest fallback.
/// Refuse without writing or calling its private runtime.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_guarded_child_without_the_advertisement_refuses_without_fallback() {
    use crate::section_autoconfig::tests::{first_case, run, shown, stand_in};
    let fx = fixture();
    let fit = crate::backend_stats::fit::tests::fixture();
    let fit_case = fit["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == "baselines")
        .unwrap();
    let data = tempfile::tempdir().unwrap();
    let db = store(data.path(), &fx["schema"]);
    let stats = Arc::new(crate::backend_stats::fit::tests::stats_for(
        fit_case,
        data.path(),
    ));
    let (base, stand) = stand_in().await;
    let client = reqwest::Client::new();
    let section = crate::section_autoconfig::tests::fixture();
    let path = section["cases"][0]["results"][0]["path"]
        .as_str()
        .unwrap()
        .to_owned();
    let runtime = section["cases"][0]["results"][0]["runtime"].clone();
    stand.lock().unwrap().reply =
        Some((200, runtime["body"].as_str().unwrap_or_default().to_owned()));

    let models = tempfile::tempdir().unwrap();
    let (native, _) = first_case(models.path());
    native.set_stats(stats.clone());
    native.set_telemetry(Arc::new(Telemetry::with_clock(
        data.path(),
        stats.clone(),
        Arc::new(|| 1.0e9),
    )));
    // A services#98 child has already disabled ingest under confirmation=1 but
    // answers 404 here. Its private runtime can still return old/empty measurements;
    // never call it as a substitute for an ownership handshake.
    let got = shown(run(&native, &path, &base, &client).await);
    assert_eq!(got.0, 503);
    assert!(stand.lock().unwrap().seen.is_empty());
    assert_eq!(dump(&db)["kv"], V::Array(Vec::new()));
    stand.lock().unwrap().guard = true;
    let again = shown(run(&native, &path, &base, &client).await);
    assert_eq!(again, got);
    assert!(stand.lock().unwrap().seen.is_empty());
    assert_eq!(dump(&db)["kv"], V::Array(Vec::new()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_tail_is_stored_before_reading_the_next_backend() {
    let fx = fixture();
    let dir = tempfile::tempdir().unwrap();
    let path = store(dir.path(), &fx["schema"]);
    let daemon = Arc::new(Daemon::default());
    *daemon.2.lock().unwrap() = Some(path.clone());
    let names: Vec<String> = vec!["syn-first".into(), "syn-second".into()];
    for name in &names {
        daemon
            .0
            .lock()
            .unwrap()
            .insert(name.clone(), (Some(first_log(&fx)), true));
    }
    let telemetry = world(
        &names,
        daemon.clone(),
        dir.path(),
        Arc::new(Mutex::new(1.0e9)),
    );
    assert_eq!(telemetry.ingest().await, Ok(()));
    assert_eq!(daemon.1.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert!(dump(&path)["req_timing"]
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r[2] == "syn-second"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn guard_requires_the_exact_bounded_advertisement() {
    use axum::{routing::get, Router};
    for (body, expected) in [
        (GUARD_BODY.to_vec(), Some(true)),
        (b"{}".to_vec(), Some(false)),
        (vec![b'x'; GUARD_BODY.len() + 1], None),
    ] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().route(
                    GUARD_PATH,
                    get(move || {
                        let body = body.clone();
                        async move { body }
                    }),
                ),
            )
            .await
            .unwrap();
        });
        let dir = tempfile::tempdir().unwrap();
        let stats = Arc::new(crate::backend_stats::fit::tests::stats_for(
            &crate::backend_stats::fit::tests::fixture()["cases"][0],
            dir.path(),
        ));
        let telemetry = Telemetry::new(dir.path(), stats);
        let client = reqwest::Client::new();
        let child = crate::section_autoconfig::Child {
            client: &client,
            base: &base,
            token: "",
        };
        assert_eq!(telemetry.owner(&child).await, expected);
        assert!(!dir.path().join("model_loader.db").exists());
        server.abort();
    }
}

#[test]
fn unverified_owner_message_names_no_retired_selector() {
    let line = super::unverified_owner_message(reqwest::StatusCode::NOT_FOUND);
    assert!(!line.contains("MODEL_MANAGER_"), "{line}");
    assert!(line.contains("telemetry guard"), "{line}");
    assert!(line.contains("answered 404 Not Found"), "{line}");
}
