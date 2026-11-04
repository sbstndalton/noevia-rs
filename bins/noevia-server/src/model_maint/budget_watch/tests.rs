//! Core `inference-budget.test.cjs`'s watchdog cases, ported over a synthetic [`Io`] (no engine, no sysfs), plus
//! the timer: serial sampling, a restart and the stop at shutdown.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
use super::*;
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Default)]
struct Fake {
    budget: Mutex<Option<f64>>,
    gpu: Mutex<Option<Gpu>>,
    engine: Mutex<Option<Engine>>,
    loaded: Mutex<Option<Vec<Value>>>,
    held: Mutex<Option<bool>>,
    /// model -> the unload's answer (`Some(true)` when absent).
    answers: Mutex<Vec<(Value, Option<bool>)>>,
    unloaded: Mutex<Vec<Value>>,
    quarantined: Mutex<Vec<(Value, f64)>>,
    log: Mutex<Vec<String>>,
    now: Mutex<f64>,
    reads: AtomicUsize,
    /// When set, every GPU read waits for it (a slow reading).
    gate: Mutex<Option<Arc<tokio::sync::Notify>>>,
}

impl Fake {
    fn new(budget: f64, gpu: Option<Gpu>, engine: Option<Engine>, loaded: &[&str]) -> Fake {
        let f = Fake::default();
        *f.budget.lock().unwrap() = Some(budget);
        *f.gpu.lock().unwrap() = gpu;
        *f.engine.lock().unwrap() = engine;
        *f.loaded.lock().unwrap() = Some(loaded.iter().map(|m| json!(m)).collect());
        *f.held.lock().unwrap() = Some(false);
        f
    }
    fn unloaded(&self) -> Vec<Value> {
        self.unloaded.lock().unwrap().clone()
    }
}

impl Io for Fake {
    async fn budget(&self) -> Option<f64> {
        *self.budget.lock().unwrap()
    }
    async fn read_gpu(&self) -> Option<Gpu> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        let gate = self.gate.lock().unwrap().clone();
        if let Some(g) = gate {
            g.notified().await;
        }
        *self.gpu.lock().unwrap()
    }
    async fn read_engine(&self) -> Option<Engine> {
        *self.engine.lock().unwrap()
    }
    async fn list_loaded(&self) -> Option<Vec<Value>> {
        self.loaded.lock().unwrap().clone()
    }
    async fn baseline_allowed(&self) -> bool {
        *self.held.lock().unwrap() == Some(false)
    }
    async fn unload(&self, model: &Value) -> Option<bool> {
        self.unloaded.lock().unwrap().push(model.clone());
        self.answers
            .lock()
            .unwrap()
            .iter()
            .find(|(m, _)| m == model)
            .map(|(_, a)| *a)
            .unwrap_or(Some(true))
    }
    async fn quarantine(&self, model: &Value, budget_gib: f64) {
        self.quarantined
            .lock()
            .unwrap()
            .push((model.clone(), budget_gib));
    }
    fn log(&self, line: &str) {
        self.log.lock().unwrap().push(line.to_owned());
    }
    fn now_ms(&self) -> f64 {
        *self.now.lock().unwrap()
    }
}

fn settings(env: &[(&str, &str)]) -> Settings {
    let env: Vec<(String, String)> = env
        .iter()
        .map(|(k, v)| ((*k).into(), (*v).into()))
        .collect();
    Settings::from_lookup(&|k: &str| env.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone()))
}

fn gpu(gtt: f64, vram: f64) -> Option<Gpu> {
    Some(Gpu {
        gtt_gib: gtt,
        vram_gib: vram,
    })
}

fn engine(container: f64) -> Option<Engine> {
    Some(Engine {
        container_gib: container,
        gtt_gib: 0.0,
        vram_gib: 0.0,
    })
}

fn watch(fake: Fake) -> Watch<Fake> {
    Watch::new(settings(&[]), fake)
}

async fn state(w: &Watch<Fake>) -> &'static str {
    w.tick().await.unwrap().state()
}

fn sample(s: &Status) -> &Sample {
    match s {
        Status::Ok(x) | Status::Over { sample: x, .. } | Status::Unloaded { sample: x, .. } => x,
        other => panic!("no sample in {other:?}"),
    }
}

#[tokio::test]
async fn over_by_more_than_ten_percent_for_two_readings_unloads_and_logs_numbers_only() {
    // 12 + 0.5 + 1.5 = 14 GiB > 13.2.
    let w = watch(Fake::new(12.0, gpu(12.0, 0.5), engine(1.5), &["chat"]));
    assert_eq!(state(&w).await, "over");
    assert!(w.io().unloaded().is_empty());
    let r = w.tick().await.unwrap();
    assert_eq!(r.state(), "unloaded");
    assert_eq!(w.io().unloaded(), vec![json!("chat")]);
    let log = w.io().log.lock().unwrap().clone();
    assert_eq!(log.len(), 1);
    assert_eq!(
        log[0],
        r#"[inference-budget] {"event":"unload","usedGib":14,"gttGib":12,"vramGib":0.5,"baselineGib":0,"containerGib":1.5,"budgetGib":12,"limitGib":13.2,"models":["chat"],"failed":[]}"#
    );
}

#[tokio::test]
async fn within_the_overshoot_or_a_single_spike_leaves_the_model_alone() {
    // 13.1 <= 13.2.
    let a = watch(Fake::new(12.0, gpu(12.6, 0.5), engine(0.0), &["chat"]));
    for _ in 0..5 {
        assert_eq!(state(&a).await, "ok");
    }
    assert!(a.io().unloaded().is_empty());
    let b = watch(Fake::new(12.0, gpu(20.0, 0.0), None, &["chat"]));
    b.tick().await;
    *b.io().gpu.lock().unwrap() = gpu(4.0, 0.0);
    assert_eq!(state(&b).await, "ok");
    *b.io().gpu.lock().unwrap() = gpu(20.0, 0.0);
    b.tick().await;
    assert!(
        b.io().unloaded().is_empty(),
        "strikes reset after a reading under the limit"
    );
}

#[tokio::test]
async fn after_an_unload_the_cooldown_is_waited_out() {
    let w = watch(Fake::new(12.0, gpu(20.0, 0.0), None, &["chat"]));
    w.tick().await;
    w.tick().await;
    assert_eq!(w.io().unloaded().len(), 1);
    assert!(matches!(
        w.tick().await,
        Some(Status::Over { strikes: 1, .. })
    ));
    // Strikes go on counting through the cooldown, as Node's do.
    assert!(matches!(
        w.tick().await,
        Some(Status::Over { strikes: 2, .. })
    ));
    assert!(matches!(
        w.tick().await,
        Some(Status::Over { strikes: 3, .. })
    ));
    assert_eq!(w.io().unloaded().len(), 1);
    *w.io().now.lock().unwrap() = 61000.0;
    assert_eq!(state(&w).await, "unloaded");
    assert_eq!(w.io().unloaded().len(), 2);
}

#[test]
fn overshoot_interval_and_switch_come_from_the_environment() {
    let d = settings(&[]);
    assert!(d.enabled);
    assert_eq!(d.every, Duration::from_millis(15000));
    assert_eq!(d.overshoot, 0.1);
    assert_eq!((d.strikes_needed, d.cooldown_ms), (2, 60000.0));
    assert_eq!(
        settings(&[("INFERENCE_BUDGET_WATCH_INTERVAL_MS", "30000")]).every,
        Duration::from_millis(30000)
    );
    assert_eq!(
        settings(&[("INFERENCE_BUDGET_WATCH_INTERVAL_MS", "2147483648")]).every,
        Duration::from_millis(1)
    );
    // max(2000, ...); an empty value is Number('') = 0; a bad one is the default.
    assert_eq!(
        settings(&[("INFERENCE_BUDGET_WATCH_INTERVAL_MS", "500")]).every,
        Duration::from_millis(2000)
    );
    assert_eq!(
        settings(&[("INFERENCE_BUDGET_WATCH_INTERVAL_MS", "")]).every,
        Duration::from_millis(2000)
    );
    assert_eq!(
        settings(&[("INFERENCE_BUDGET_WATCH_INTERVAL_MS", "-5")]).every,
        Duration::from_millis(15000)
    );
    assert_eq!(
        settings(&[("INFERENCE_BUDGET_WATCH_INTERVAL_MS", "0x1000")]).every,
        Duration::from_millis(4096)
    );
    assert_eq!(
        settings(&[("INFERENCE_BUDGET_OVERSHOOT_PCT", "0")]).overshoot,
        0.0
    );
    assert_eq!(
        settings(&[("INFERENCE_BUDGET_OVERSHOOT_PCT", "250")]).overshoot,
        1.0
    );
    assert_eq!(
        settings(&[("INFERENCE_BUDGET_OVERSHOOT_PCT", "abc")]).overshoot,
        0.1
    );
    for v in ["off", "OFF", " no ", "0", "false", "\u{feff}off"] {
        assert!(!settings(&[("INFERENCE_BUDGET_WATCH", v)]).enabled, "{v:?}");
    }
    for v in ["", "on", "1", "nope", "\u{85}off"] {
        assert!(settings(&[("INFERENCE_BUDGET_WATCH", v)]).enabled, "{v:?}");
    }
}

#[tokio::test]
async fn a_zero_overshoot_unloads_just_over_the_budget() {
    let w = Watch::new(
        settings(&[("INFERENCE_BUDGET_OVERSHOOT_PCT", "0")]),
        Fake::new(12.0, gpu(12.5, 0.0), None, &["chat"]),
    );
    w.tick().await;
    w.tick().await;
    assert_eq!(w.io().unloaded(), vec![json!("chat")]);
}

#[tokio::test]
async fn nothing_measurable_does_nothing_and_resets_the_strikes() {
    let w = watch(Fake::new(12.0, gpu(20.0, 0.0), None, &["chat"]));
    assert_eq!(state(&w).await, "over");
    *w.io().gpu.lock().unwrap() = None;
    assert_eq!(w.tick().await, Some(Status::Unavailable));
    *w.io().gpu.lock().unwrap() = gpu(20.0, 0.0);
    assert!(matches!(
        w.tick().await,
        Some(Status::Over { strikes: 1, .. })
    ));
    assert!(w.io().unloaded().is_empty());
}

#[tokio::test]
async fn no_budget_reads_nothing() {
    for b in [Some(0.0), Some(-1.0), Some(f64::NAN), None] {
        let f = Fake::new(12.0, gpu(20.0, 0.0), None, &["chat"]);
        *f.budget.lock().unwrap() = b;
        let w = watch(f);
        assert_eq!(w.tick().await, Some(Status::NoBudget));
        assert_eq!(w.io().reads.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn an_idle_baseline_with_no_model_loaded_is_not_charged() {
    let f = Fake::new(12.0, gpu(1.5, 0.5), engine(0.0), &[]);
    let w = watch(f);
    assert_eq!(
        sample(&w.tick().await.unwrap()).baseline_gib,
        2.0,
        "startup with nothing loaded"
    );
    *w.io().loaded.lock().unwrap() = Some(vec![json!("chat")]);
    *w.io().gpu.lock().unwrap() = gpu(14.0, 0.5);
    let r = w.tick().await.unwrap();
    assert_eq!(r.state(), "ok");
    assert_eq!(
        sample(&r).used_gib,
        12.5,
        "12.5 <= 13.2 once the 2 GiB baseline is taken off"
    );
    *w.io().gpu.lock().unwrap() = gpu(15.0, 0.5);
    w.tick().await;
    assert_eq!(state(&w).await, "unloaded");
    assert_eq!(w.io().unloaded(), vec![json!("chat")]);
}

#[tokio::test]
async fn without_an_idle_reading_the_baseline_is_zero() {
    let w = watch(Fake::new(12.0, gpu(13.0, 0.5), None, &["chat"]));
    let r = w.tick().await.unwrap();
    assert_eq!(
        (sample(&r).baseline_gib, sample(&r).used_gib, r.state()),
        (0.0, 13.5, "over")
    );
}

#[tokio::test]
async fn nothing_loaded_means_nothing_to_unload() {
    let w = watch(Fake::new(12.0, gpu(30.0, 0.0), None, &[]));
    for _ in 0..3 {
        assert_eq!(state(&w).await, "ok");
    }
    assert!(w.io().unloaded().is_empty());
}

#[tokio::test]
async fn every_model_unloaded_goes_to_the_quarantine_with_the_budget_and_failures_do_not() {
    let f = Fake::new(12.0, gpu(20.0, 0.0), None, &["chat", "vision", "lost"]);
    f.answers
        .lock()
        .unwrap()
        .push((json!("vision"), Some(false)));
    // A request that did not settle counts as unloaded (`r?.ok !== false`).
    f.answers.lock().unwrap().push((json!("lost"), None));
    let w = watch(f);
    w.tick().await;
    let r = w.tick().await.unwrap();
    assert!(
        matches!(&r, Status::Unloaded { models, .. } if models == &vec![json!("chat"), json!("lost")])
    );
    assert_eq!(
        *w.io().quarantined.lock().unwrap(),
        vec![(json!("chat"), 12.0), (json!("lost"), 12.0)]
    );
    let log = w.io().log.lock().unwrap()[0].clone();
    assert!(
        log.ends_with(r#""models":["chat","lost"],"failed":["vision"]}"#),
        "{log}"
    );
}

#[tokio::test]
async fn an_unknown_listing_is_not_idle_and_keeps_the_strikes() {
    // #874: a 13 GiB model is resident; the engine answers /models with an error for one reading.
    let f = Fake::new(12.0, gpu(13.0, 0.5), None, &["chat"]);
    let w = watch(f);
    assert!(matches!(
        w.tick().await,
        Some(Status::Over { strikes: 1, .. })
    ));
    *w.io().loaded.lock().unwrap() = None;
    *w.io().gpu.lock().unwrap() = gpu(0.1, 0.0);
    assert_eq!(w.tick().await, Some(Status::ModelsUnknown { strikes: 1 }));
    *w.io().loaded.lock().unwrap() = Some(vec![json!("chat")]);
    *w.io().gpu.lock().unwrap() = gpu(13.0, 0.5);
    let r = w.tick().await.unwrap();
    assert_eq!(
        sample(&r).baseline_gib,
        0.0,
        "the unknown reading was not taken as the idle baseline"
    );
    assert_eq!(
        r.state(),
        "unloaded",
        "strikes carried over the unknown tick"
    );
}

#[tokio::test]
async fn no_idle_baseline_while_the_maintenance_gate_is_held_or_unknown() {
    let f = Fake::new(12.0, gpu(11.0, 0.0), None, &[]);
    *f.held.lock().unwrap() = Some(true);
    let w = watch(f);
    assert_eq!(
        sample(&w.tick().await.unwrap()).baseline_gib,
        0.0,
        "the sweep's 11 GiB is not the idle baseline"
    );
    *w.io().held.lock().unwrap() = Some(false);
    *w.io().gpu.lock().unwrap() = gpu(1.0, 0.0);
    assert_eq!(
        sample(&w.tick().await.unwrap()).baseline_gib,
        1.0,
        "a real idle reading after the sweep is"
    );
    let f = Fake::new(12.0, gpu(1.0, 0.0), None, &[]);
    *f.held.lock().unwrap() = None;
    assert_eq!(
        sample(&watch(f).tick().await.unwrap()).baseline_gib,
        0.0,
        "an unanswered state is held"
    );
}

#[test]
fn round2_is_math_round_over_hundredths() {
    assert_eq!(round2(13.199999999), 13.2);
    assert_eq!(round2(0.125), 0.13);
    assert_eq!(round2(-0.125), -0.12, "ties toward +Infinity");
    assert_eq!(round2(14.0), 14.0);
    assert!(round2(f64::NAN).is_nan());
}

#[tokio::test]
async fn gpu_memory_comes_from_the_configured_counters_or_every_drm_card() {
    let d = tempfile::tempdir().unwrap();
    for n in ["card0", "card1", "card1-DP-1", "renderD129", "card"] {
        std::fs::create_dir_all(d.path().join(n).join("device")).unwrap();
    }
    let dev = d.path().join("card1/device");
    std::fs::write(dev.join("mem_info_gtt_used"), format!("{}\n", 12u64 << 30)).unwrap();
    std::fs::write(dev.join("mem_info_vram_used"), format!("{}", 1u64 << 29)).unwrap();
    // card1-DP-1 has a counter too but is no card.
    std::fs::write(d.path().join("card1-DP-1/device/mem_info_gtt_used"), "1").unwrap();
    let none = |_: &str| None;
    let auto = gpu_memory_files(&none, d.path());
    assert_eq!(
        auto,
        GpuFiles {
            gtt: vec![dev.join("mem_info_gtt_used")],
            vram: vec![dev.join("mem_info_vram_used")]
        }
    );
    assert_eq!(read_gpu_memory(&auto).await, gpu(12.0, 0.5));
    let pinned_env = |k: &str| {
        (k == "INFERENCE_GTT_USED_PATH")
            .then(|| " /host-sys/card7/mem_info_gtt_used ,, ".to_owned())
    };
    let pinned = gpu_memory_files(&pinned_env, Path::new("/nonexistent"));
    assert_eq!(
        pinned,
        GpuFiles {
            gtt: vec![PathBuf::from("/host-sys/card7/mem_info_gtt_used")],
            vram: vec![]
        }
    );
    assert_eq!(
        read_gpu_memory(&pinned).await,
        None,
        "an unreadable counter is no reading"
    );
    assert_eq!(
        gpu_memory_files(&none, Path::new("/nonexistent")),
        GpuFiles::default()
    );
    // Bad counter text is skipped; a negative one too.
    std::fs::write(dev.join("mem_info_vram_used"), "-1").unwrap();
    assert_eq!(read_gpu_memory(&auto).await, gpu(12.0, 0.0));
}

#[test]
fn engine_memory_counts_only_the_main_engine_container_as_anonymous_memory() {
    let mut body = json!({ "backends": [
        { "name": "cowork-llama-1", "stats": { "ok": true, "container": { "mem_used_gb": 7.5, "mem_anon_gb": 3.5 }, "gpu": { "shared_used_gb": 9, "vram_used_gb": 0.4 } } },
        { "name": "cowork-embed-1", "stats": { "ok": true, "container": { "mem_used_gb": 0.9, "mem_anon_gb": 0.8 }, "gpu": null } },
        { "name": "cowork-rerank-1", "stats": { "ok": true, "container": { "mem_used_gb": 1.2, "mem_anon_gb": 1.1 }, "gpu": null } },
        { "name": "cowork-llama-2", "stats": { "ok": false } },
        null,
    ]});
    let e = |b: &Value, c: &[String]| engine_from_backends(true, Some(b), c);
    assert_eq!(
        e(&body, &[]),
        Some(Engine {
            container_gib: 3.5,
            gtt_gib: 9.0,
            vram_gib: 0.4
        }),
        "anon, not usage"
    );
    assert_eq!(
        e(&body, &["cowork-embed-1".to_owned()]),
        Some(Engine {
            container_gib: 0.8,
            gtt_gib: 0.0,
            vram_gib: 0.0
        }),
        "configurable by name"
    );
    // An older model manager without mem_anon_gb: usage less the page cache; strings are Number()ed.
    body["backends"][0]["stats"]["container"] = json!({ "mem_used_gb": "4.2" });
    assert_eq!(e(&body, &[]).unwrap().container_gib, 4.2);
    assert_eq!(engine_from_backends(false, Some(&body), &[]), None);
    assert_eq!(e(&json!({ "backends": {} }), &[]), None);
    assert_eq!(e(&json!("text"), &[]), None);
    assert!(is_engine_container("llama", &[]));
    assert!(is_engine_container("cowork_llama_1", &[]));
    assert!(is_engine_container("x-llama", &[]));
    assert!(!is_engine_container("cowork-embed-1", &[]));
    assert!(!is_engine_container("llama-rerank", &[]));
    assert!(!is_engine_container("ollama", &[]));
    assert!(!is_engine_container("cowork-llama-", &[]));
    assert!(is_engine_container("é-llama-12", &[]));
}

#[test]
fn a_failed_listing_is_unknown_and_loaded_or_loading_rows_are_the_models() {
    assert_eq!(
        loaded_from_listing(false, Some(&json!({ "error": "down" }))),
        None
    );
    assert_eq!(loaded_from_listing(true, Some(&json!({}))), None);
    assert_eq!(loaded_from_listing(true, None), None);
    assert_eq!(
        loaded_from_listing(true, Some(&json!({ "data": [null] }))),
        None
    );
    let body = json!({ "data": [
        { "id": "a", "status": { "value": "loaded" } },
        { "id": "b", "status": { "value": "unloaded" } },
        { "id": "c", "status": { "value": "loading" } },
        { "id": "d" },
        { "id": "e", "status": { "value": ["loaded"] } },
        "f",
    ]});
    assert_eq!(
        loaded_from_listing(true, Some(&body)),
        Some(vec![json!("a"), json!("c")])
    );
}

#[tokio::test]
async fn sampling_is_serial_a_tick_asked_for_while_one_runs_answers_the_last() {
    let f = Fake::new(12.0, gpu(20.0, 0.0), None, &["chat"]);
    let w = Arc::new(watch(f));
    assert_eq!(state(&w).await, "over");
    let slow = Arc::new(tokio::sync::Notify::new());
    *w.io().gate.lock().unwrap() = Some(Arc::clone(&slow));
    let running = {
        let w = Arc::clone(&w);
        tokio::spawn(async move { w.tick().await })
    };
    while w.io().reads.load(Ordering::SeqCst) < 2 {
        tokio::task::yield_now().await;
    }
    // The second tick neither reads nor unloads: it answers the previous state.
    assert!(matches!(
        w.tick().await,
        Some(Status::Over { strikes: 1, .. })
    ));
    assert_eq!(w.io().reads.load(Ordering::SeqCst), 2);
    assert!(w.io().unloaded().is_empty());
    slow.notify_one();
    assert_eq!(running.await.unwrap().unwrap().state(), "unloaded");
    assert_eq!(w.io().unloaded().len(), 1);
}

#[tokio::test]
async fn a_restart_starts_from_no_strikes_no_baseline_and_no_cooldown() {
    let f = Fake::new(12.0, gpu(1.0, 0.0), None, &[]);
    let w = watch(f);
    w.tick().await;
    *w.io().loaded.lock().unwrap() = Some(vec![json!("chat")]);
    *w.io().gpu.lock().unwrap() = gpu(20.0, 0.0);
    w.tick().await;
    w.tick().await;
    assert_eq!(w.io().unloaded().len(), 1);
    // A new process: the same readings, none of the memory.
    let after = watch(Fake::new(12.0, gpu(20.0, 0.0), None, &["chat"]));
    let r = after.tick().await.unwrap();
    assert!(matches!(r, Status::Over { strikes: 1, .. }));
    assert_eq!(sample(&r).baseline_gib, 0.0);
    assert_eq!(state(&after).await, "unloaded", "no cooldown carried over");
}

#[tokio::test(start_paused = true)]
async fn the_timer_ticks_every_interval_after_the_first_and_stops_at_shutdown() {
    let w = Arc::new(Watch::new(
        settings(&[("INFERENCE_BUDGET_WATCH_INTERVAL_MS", "2000")]),
        Fake::new(12.0, gpu(1.0, 0.0), None, &["chat"]),
    ));
    let (stop, rx) = tokio::sync::watch::channel(false);
    let task = tokio::spawn(Arc::clone(&w).run(rx));
    tokio::time::sleep(Duration::from_millis(1999)).await;
    assert_eq!(
        w.io().reads.load(Ordering::SeqCst),
        0,
        "no reading at start (setInterval)"
    );
    tokio::time::sleep(Duration::from_millis(2)).await;
    assert_eq!(w.io().reads.load(Ordering::SeqCst), 1);
    tokio::time::sleep(Duration::from_millis(4000)).await;
    assert_eq!(w.io().reads.load(Ordering::SeqCst), 3);
    stop.send(true).unwrap();
    task.await.unwrap();
    tokio::time::sleep(Duration::from_millis(10_000)).await;
    assert_eq!(
        w.io().reads.load(Ordering::SeqCst),
        3,
        "nothing after shutdown"
    );
}

#[tokio::test(start_paused = true)]
async fn a_long_reading_skips_the_instants_it_covered() {
    let w = Arc::new(Watch::new(
        settings(&[("INFERENCE_BUDGET_WATCH_INTERVAL_MS", "2000")]),
        Fake::new(12.0, gpu(1.0, 0.0), None, &["chat"]),
    ));
    let slow = Arc::new(tokio::sync::Notify::new());
    *w.io().gate.lock().unwrap() = Some(Arc::clone(&slow));
    let (_stop, rx) = tokio::sync::watch::channel(false);
    let task = tokio::spawn(Arc::clone(&w).run(rx));
    tokio::time::sleep(Duration::from_millis(2001)).await;
    assert_eq!(w.io().reads.load(Ordering::SeqCst), 1);
    // The reading takes 5 s: the 4 s and 6 s instants are covered and skipped.
    tokio::time::sleep(Duration::from_millis(5000)).await;
    *w.io().gate.lock().unwrap() = None;
    slow.notify_one();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(w.io().reads.load(Ordering::SeqCst), 1);
    tokio::time::sleep(Duration::from_millis(1000)).await;
    assert_eq!(
        w.io().reads.load(Ordering::SeqCst),
        2,
        "next at the 8 s instant"
    );
    task.abort();
}

#[tokio::test(start_paused = true)]
async fn shutdown_during_a_reading_drops_it_before_any_unload() {
    let f = Fake::new(12.0, gpu(20.0, 0.0), None, &["chat"]);
    let w = Arc::new(Watch::new(
        settings(&[("INFERENCE_BUDGET_WATCH_INTERVAL_MS", "2000")]),
        f,
    ));
    assert_eq!(state(&w).await, "over");
    let slow = Arc::new(tokio::sync::Notify::new());
    *w.io().gate.lock().unwrap() = Some(Arc::clone(&slow));
    let (stop, rx) = tokio::sync::watch::channel(false);
    let task = tokio::spawn(Arc::clone(&w).run(rx));
    tokio::time::sleep(Duration::from_millis(2001)).await;
    assert_eq!(
        w.io().reads.load(Ordering::SeqCst),
        2,
        "the timer's reading is in progress"
    );
    stop.send(true).unwrap();
    tokio::time::timeout(Duration::from_millis(100), task)
        .await
        .expect("stops at once")
        .unwrap();
    slow.notify_waiters();
    tokio::time::sleep(Duration::from_millis(10_000)).await;
    assert!(
        w.io().unloaded().is_empty(),
        "the dropped reading unloaded nothing"
    );
    // And the dropped tick released the running flag: a later one (a new start) runs.
    *w.io().gate.lock().unwrap() = None;
    assert_eq!(state(&w).await, "unloaded");
}

#[tokio::test]
async fn a_watch_turned_off_never_starts() {
    let w = Arc::new(Watch::new(
        settings(&[("INFERENCE_BUDGET_WATCH", "off")]),
        Fake::default(),
    ));
    let (_stop, rx) = tokio::sync::watch::channel(false);
    tokio::time::timeout(Duration::from_millis(100), Arc::clone(&w).run(rx))
        .await
        .expect("returns at once");
    let w = Arc::new(watch(Fake::default()));
    let (_stop, rx) = tokio::sync::watch::channel(true);
    tokio::time::timeout(Duration::from_millis(100), w.run(rx))
        .await
        .expect("already stopping");
}

#[test]
fn status_objects_have_nodes_keys_in_order() {
    let s = Sample {
        used_gib: 14.0,
        gtt_gib: 12.0,
        vram_gib: 0.5,
        baseline_gib: 0.0,
        container_gib: 1.5,
        budget_gib: 12.0,
        limit_gib: 13.2,
    };
    let j = |st: Status| js_json::stringify(&st.to_json()).unwrap();
    assert_eq!(j(Status::NoBudget), r#"{"state":"no-budget"}"#);
    assert_eq!(j(Status::Unavailable), r#"{"state":"unavailable"}"#);
    assert_eq!(
        j(Status::ModelsUnknown { strikes: 3 }),
        r#"{"state":"models-unknown","strikes":3}"#
    );
    assert_eq!(
        j(Status::Over {
            strikes: 1,
            sample: s.clone()
        }),
        r#"{"state":"over","strikes":1,"usedGib":14,"gttGib":12,"vramGib":0.5,"baselineGib":0,"containerGib":1.5,"budgetGib":12,"limitGib":13.2}"#
    );
    assert_eq!(
        j(Status::Unloaded {
            models: vec![json!("a")],
            sample: s
        }),
        r#"{"state":"unloaded","models":["a"],"usedGib":14,"gttGib":12,"vramGib":0.5,"baselineGib":0,"containerGib":1.5,"budgetGib":12,"limitGib":13.2}"#
    );
}
