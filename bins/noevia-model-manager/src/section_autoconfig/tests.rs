//! Differential tests against the actual services Python (`tools/gen-section-autoconfig-oracle.py`).
//! Every case's models folder, models.ini and roles file are rebuilt here from the fixture (the
//! GGUF headers byte for byte, the rest as sparse zeros), the synthetic root adapter stands in for
//! the Linux-only kernel-contained opener, and a loopback stand-in answers the private runtime
//! call with what the real child answered (`tests/section_autoconfig.rs` runs cases through the
//! router and the real opener on Linux).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
use super::*;
use serde_json::Value as Json;
use std::{io::Write, sync::Mutex};

pub(crate) const TOKEN: &str = "synthetic-section-autoconfig-token";

/// Cases the native route refuses by design where Python answers 200
/// (`docs/native-section-autoconfig.md`): a relative saved projector path, which Python resolves
/// against its own working directory.
pub(crate) const DELIBERATE_REFUSALS: &[&str] = &["saved-relative-projector"];

pub(crate) fn fixture() -> Json {
    serde_json::from_str(include_str!(
        "../../tests/fixtures/section-autoconfig-python.v1.json"
    ))
    .unwrap()
}

fn hex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap())
        .collect()
}

/// Builds one case like the generator; returns the models folder (absent when the case has none).
pub(crate) fn build(root: &Path, case: &Json) -> PathBuf {
    let models = root.join("models");
    let Some(files) = case["files"].as_array() else {
        return models;
    };
    std::fs::create_dir(&models).unwrap();
    for spec in files {
        let path = models.join(spec["name"].as_str().unwrap());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let data = hex(spec["header"].as_str().unwrap_or(""));
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(&data).unwrap();
        if let Some(size) = spec["size"].as_u64() {
            if size > data.len() as u64 {
                file.set_len(size).unwrap();
            }
        }
    }
    if let Some(ini) = case["ini"].as_str() {
        std::fs::write(models.join("models.ini"), ini).unwrap();
    }
    if let Some(roles) = case["roles"].as_str() {
        std::fs::write(models.join("models.roles.json"), roles).unwrap();
    }
    models
}

pub(crate) fn settings(models: &Path, cache_cap: i64) -> Settings {
    Settings {
        models: models.to_str().unwrap().to_owned(),
        ini: models.join("models.ini").to_str().unwrap().to_owned(),
        cache_ram_cap_mib: cache_cap,
    }
}

/// What the stand-in child answers next, and what it was asked (token, body).
#[derive(Default)]
pub(crate) struct Stand {
    pub(crate) reply: Option<(u16, String)>,
    pub(crate) seen: Vec<(Option<String>, String)>,
}

/// A loopback child answering only the private runtime call.
pub(crate) async fn stand_in() -> (String, Arc<Mutex<Stand>>) {
    let shared = Arc::new(Mutex::new(Stand::default()));
    let state = shared.clone();
    let app = axum::Router::new().route(
        RUNTIME_PATH,
        axum::routing::post(move |headers: axum::http::HeaderMap, body: String| {
            let state = state.clone();
            async move {
                let mut stand = state.lock().unwrap();
                let token = headers
                    .get("x-model-loader-token")
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned);
                stand.seen.push((token, body));
                let (status, body) = stand.reply.clone().unwrap_or((404, String::new()));
                (axum::http::StatusCode::from_u16(status).unwrap(), body)
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (base, shared)
}

pub(crate) fn shown(outcome: Outcome) -> (u16, String) {
    match outcome {
        Outcome::Json(status, body) => (status, String::from_utf8(body).unwrap()),
        Outcome::Failed => (500, "Internal Server Error".to_owned()),
        Outcome::Refused => (503, REFUSAL.to_owned()),
        Outcome::Unavailable => (502, "unavailable".to_owned()),
    }
}

pub(crate) async fn run(
    autoconfig: &SectionAutoconfig,
    path: &str,
    base: &str,
    client: &reqwest::Client,
) -> Outcome {
    let (route_path, query) = path.split_once('?').unwrap_or((path, ""));
    let name = route(&axum::http::Method::GET, route_path).unwrap();
    let child = Child {
        client,
        base,
        token: TOKEN,
    };
    autoconfig.get(&name, query, child).await
}

#[test]
fn fixture_provenance_is_the_pinned_services_source() {
    let fixture = fixture();
    let oracle = fixture["oracle"].as_object().unwrap();
    for name in [
        "api.py",
        "helpers.py",
        "ini.py",
        "autoconfig.py",
        "autoconfig_core.py",
        "gguf_meta.py",
        "telemetry.py",
        "main.py",
    ] {
        assert_eq!(oracle[name].as_str().unwrap().len(), 64, "{name}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_case_matches_the_actual_python_route() {
    let fixture = fixture();
    let cases = fixture["cases"].as_array().unwrap();
    assert!(cases.len() >= 30, "{}", cases.len());
    let (base, stand) = stand_in().await;
    let client = reqwest::Client::new();
    let mut checked = 0;
    let mut failures = Vec::new();
    for case in cases {
        let tmp = tempfile::tempdir().unwrap();
        let models = build(tmp.path(), case);
        let autoconfig = SectionAutoconfig::with(
            settings(&models, case["cache_cap"].as_i64().unwrap()),
            Root::synthetic,
        );
        for result in case["results"].as_array().unwrap() {
            let path = result["path"].as_str().unwrap();
            let runtime = &result["runtime"];
            {
                let mut stand = stand.lock().unwrap();
                stand.seen.clear();
                stand.reply = runtime.as_object().map(|r| {
                    (
                        u16::try_from(r["status"].as_u64().unwrap()).unwrap(),
                        r["body"].as_str().unwrap().to_owned(),
                    )
                });
            }
            let got = shown(run(&autoconfig, path, &base, &client).await);
            let want = (
                u16::try_from(result["status"].as_u64().unwrap()).unwrap(),
                result["body"].as_str().unwrap().to_owned(),
            );
            let want = if DELIBERATE_REFUSALS.contains(&case["id"].as_str().unwrap()) {
                assert_eq!(want.0, 200, "case {} {path}", case["id"]);
                (503, REFUSAL.to_owned())
            } else {
                want
            };
            if got != want {
                failures.push(format!(
                    "case {} {path}\n  got  {} {}\n  want {} {}",
                    case["id"], got.0, got.1, want.0, want.1
                ));
            }
            let seen = std::mem::take(&mut stand.lock().unwrap().seen);
            if runtime.is_null() {
                assert!(
                    seen.is_empty(),
                    "case {} {path}: asked the child",
                    case["id"]
                );
            } else {
                assert_eq!(seen.len(), 1, "case {} {path}", case["id"]);
                assert_eq!(seen[0].0.as_deref(), Some(TOKEN));
                let asked: Json = serde_json::from_str(&seen[0].1).unwrap();
                assert_eq!(asked, runtime["request"], "case {} {path}", case["id"]);
            }
            checked += 1;
        }
    }
    assert!(
        failures.is_empty(),
        "{} failures\n{}",
        failures.len(),
        failures.join("\n")
    );
    assert!(checked >= 100, "{checked}");
}

/// The first case's folder and its first request's recorded child answer.
pub(crate) fn first_case(dir: &Path) -> (SectionAutoconfig, String) {
    let fixture = fixture();
    let case = &fixture["cases"][0];
    let models = build(dir, case);
    let body = case["results"][0]["runtime"]["body"]
        .as_str()
        .unwrap()
        .to_owned();
    (
        SectionAutoconfig::with(settings(&models, 1024), Root::synthetic),
        body,
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn child_failures_map_to_the_forwarded_answers() {
    let dir = tempfile::tempdir().unwrap();
    let (autoconfig, good) = first_case(dir.path());
    let (base, stand) = stand_in().await;
    let client = reqwest::Client::new();
    let path = "/api/v1/sections/m/autoconfig";
    let mut wrong_rate: serde_json::Map<String, Json> = serde_json::from_str(&good).unwrap();
    wrong_rate.insert("promptTps".into(), Json::from(900.0));
    let mut no_list: serde_json::Map<String, Json> = serde_json::from_str(&good).unwrap();
    no_list.insert("backends".into(), Json::from("{}"));
    let mut extra: serde_json::Map<String, Json> = serde_json::from_str(&good).unwrap();
    extra.insert("more".into(), Json::Null);
    let cases: Vec<((u16, String), Outcome)> = vec![
        ((500, "Internal Server Error".into()), Outcome::Failed),
        ((404, String::new()), Outcome::Unavailable),
        ((422, "{}".into()), Outcome::Unavailable),
        ((200, "{}".into()), Outcome::Refused),
        ((200, "not json".into()), Outcome::Refused),
        (
            (200, Json::Object(wrong_rate).to_string()),
            Outcome::Refused,
        ),
        ((200, Json::Object(no_list).to_string()), Outcome::Refused),
        ((200, Json::Object(extra).to_string()), Outcome::Refused),
        ((200, " ".repeat(RUNTIME_CAP + 1)), Outcome::Unavailable),
    ];
    for (reply, want) in cases {
        let label = format!("{} {:.40}", reply.0, reply.1);
        stand.lock().unwrap().reply = Some(reply);
        assert_eq!(
            run(&autoconfig, path, &base, &client).await,
            want,
            "{label}"
        );
    }
    // Nothing listens on port 9.
    assert_eq!(
        run(&autoconfig, path, "http://127.0.0.1:9", &client).await,
        Outcome::Unavailable
    );
    // The recorded answer still gives the 200.
    stand.lock().unwrap().reply = Some((200, good));
    assert!(matches!(
        run(&autoconfig, path, &base, &client).await,
        Outcome::Json(200, _)
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unknowable_query_refuses_before_any_read_or_child_call() {
    let dir = tempfile::tempdir().unwrap();
    let (autoconfig, _) = first_case(dir.path());
    let (base, stand) = stand_in().await;
    let client = reqwest::Client::new();
    for query in ["sessions=%201", "verified_ctx=1_0", "budget_gib=1_0"] {
        let path = format!("/api/v1/sections/m/autoconfig?{query}");
        assert_eq!(
            run(&autoconfig, &path, &base, &client).await,
            Outcome::Refused,
            "{query}"
        );
    }
    assert!(stand.lock().unwrap().seen.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_answer_past_the_response_cap_refuses() {
    let dir = tempfile::tempdir().unwrap();
    let (autoconfig, good) = first_case(dir.path());
    let (base, stand) = stand_in().await;
    let client = reqwest::Client::new();
    let mut runtime: serde_json::Map<String, Json> = serde_json::from_str(&good).unwrap();
    runtime.insert(
        "measured".into(),
        serde_json::json!({ "n": "x".repeat(RESPONSE_CAP) }),
    );
    stand.lock().unwrap().reply = Some((200, Json::Object(runtime).to_string()));
    assert_eq!(
        run(&autoconfig, "/api/v1/sections/m/autoconfig", &base, &client).await,
        Outcome::Refused
    );
}

/// Two reads at a time; a third waits for a slot (and refuses once the deadline passes).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reads_wait_for_a_free_slot() {
    assert_eq!(CONCURRENT_READS, 2);
    let dir = tempfile::tempdir().unwrap();
    let (autoconfig, _) = first_case(dir.path());
    let client = reqwest::Client::new();
    let _a = autoconfig.reads.clone().try_acquire_owned().unwrap();
    let _b = autoconfig.reads.clone().try_acquire_owned().unwrap();
    let waiting = tokio::time::timeout(
        Duration::from_millis(300),
        run(
            &autoconfig,
            "/api/v1/sections/m/autoconfig",
            "http://127.0.0.1:9",
            &client,
        ),
    )
    .await;
    assert!(waiting.is_err());
}

/// noevia#2006: a request dropped while the file part is read (the client went away) keeps its
/// admission permit and its read slot until the read has returned.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dropped_request_keeps_its_permit_and_read_slot_until_the_read_ends() {
    use crate::test_support::{all_permits_back, dropped_in_blocking_work, test_state};
    let dir = tempfile::tempdir().unwrap();
    let (autoconfig, _) = first_case(dir.path());
    let autoconfig = Arc::new(autoconfig);
    let mut state = test_state(crate::LEGACY_BASE);
    state.section_autoconfig = Some(autoconfig.clone());
    let state = Arc::new(state);
    let request = axum::http::Request::get("/api/v1/sections/m/autoconfig")
        .body(axum::body::Body::empty())
        .unwrap();
    let open = dropped_in_blocking_work(&state, request).await;
    assert_eq!(autoconfig.reads.available_permits(), CONCURRENT_READS - 1);
    open.send(()).unwrap();
    all_permits_back(&state).await;
    assert_eq!(autoconfig.reads.available_permits(), CONCURRENT_READS);
}

/// noevia#2093 phase 1: a roles file that names the role (instead of the legacy key) gets the
/// answers the actual Python route recorded for the legacy key, for every path that needs no child.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_role_name_gets_the_recorded_answers_of_the_legacy_key() {
    let fixture = fixture();
    let case = fixture["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == "system-routing-role")
        .unwrap();
    let legacy_roles = case["roles"].as_str().unwrap();
    assert!(legacy_roles.to_lowercase().contains("laya"));
    let mut renamed = case.clone();
    renamed["roles"] = Json::from(
        r#"{"r1": " System-Routing ", "laya-chat": "chat", "plain": "", "r1": "SYSTEM-ROUTING"}"#,
    );
    let tmp = tempfile::tempdir().unwrap();
    let models = build(tmp.path(), &renamed);
    let autoconfig = SectionAutoconfig::with(
        settings(&models, case["cache_cap"].as_i64().unwrap()),
        Root::synthetic,
    );
    let client = reqwest::Client::new();
    let mut checked = 0;
    for result in case["results"].as_array().unwrap() {
        if !result["runtime"].is_null() {
            continue;
        }
        let path = result["path"].as_str().unwrap();
        // Nothing listens on port 9: these paths never ask the child.
        let got = shown(run(&autoconfig, path, "http://127.0.0.1:9", &client).await);
        let want = (
            u16::try_from(result["status"].as_u64().unwrap()).unwrap(),
            result["body"].as_str().unwrap().to_owned(),
        );
        assert_eq!(got, want, "{path}");
        checked += 1;
    }
    // r1 (named only by its role), laya-x, z, plain, LAYA-X, the 422.
    assert_eq!(checked, 6);
}
