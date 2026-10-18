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

#[derive(Default)]
struct Daemon(Mutex<PassLogs>);

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
                    let found = logs.iter().find(|(n, _)| id_of(n) == *id);
                    match found {
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
            legacy_base: "http://127.0.0.1:9".into(),
            token: String::new(),
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

/// A log past the cap, a tail of too many lines, and a stored argv that is not an object of
/// strings are not followed: the native 503, nothing more written.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shapes_not_followed_refuse() {
    let fx = fixture();
    let dir = tempfile::tempdir().unwrap();
    let path = store(dir.path(), &fx["schema"]);
    let daemon = Arc::new(Daemon::default());
    let many = "x\n".repeat(MAX_LINES + 1).into_bytes();
    daemon
        .0
        .lock()
        .unwrap()
        .insert("syn-a-backend".into(), (Some(many), true));
    let clock = Arc::new(Mutex::new(1.0e9));
    let telemetry = world(
        &["syn-a-backend".into()],
        daemon.clone(),
        dir.path(),
        clock.clone(),
    );
    assert_eq!(telemetry.ingest().await, Err(Stop::Refused));
    assert_eq!(dump(&path)["kv"], V::Array(Vec::new()));
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
