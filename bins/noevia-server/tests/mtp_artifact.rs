//! Real HTTP through the native front for `GET /api/models/mtp-artifact` (model_maint/mtp_artifact.rs).
//! tests/fixtures/mtp-artifact-node.json holds Core's own answers (mtp-artifact-node.cjs: routes/models.cjs,
//! llamacpp-variants.cjs, model-manager.cjs, http.cjs fetchJson and mtp-artifact.cjs at Core cf5c567f,
//! before the route moved) over synthetic GGUF headers and a scripted model source. The front replays
//! every scenario against the same scripted source (the QA source, `NOEVIA_QA_MODEL_CARD_SOURCE`, and
//! the other engine's base) and must give the same status and body, key order included; `checkedAt` is
//! normalised. Then what the fixture cannot show: sign-in, the cache, coalescing and the running limit.
//! Synthetic data only; nothing reaches the Internet and no engine is touched.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
mod maint_support;
use base64::Engine as _;
use maint_support::*;
use noevia_server::model_maint::mtp_artifact;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const ORACLE: &str = include_str!("fixtures/mtp-artifact-node.json");

/// What the scripted source answers for one target.
#[derive(Clone)]
struct Source {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    fail: bool,
    delay: Duration,
}

fn source_of(spec: &Value) -> Source {
    let body = if let Some(j) = spec.get("json") {
        j.to_string().into_bytes()
    } else if let Some(b) = spec.get("base64").and_then(Value::as_str) {
        base64::engine::general_purpose::STANDARD.decode(b).unwrap()
    } else {
        spec.get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .as_bytes()
            .to_vec()
    };
    let headers = spec
        .get("headers")
        .and_then(Value::as_object)
        .map(|h| {
            h.iter()
                .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_owned()))
                .collect()
        })
        .unwrap_or_default();
    Source {
        status: spec.get("status").and_then(Value::as_u64).unwrap_or(200) as u16,
        headers,
        body,
        fail: spec.get("fail").and_then(Value::as_bool).unwrap_or(false),
        delay: Duration::ZERO,
    }
}

type Sources = Arc<Mutex<HashMap<String, Source>>>;
type Seen = Arc<Mutex<Vec<(String, HashMap<String, String>)>>>;

/// The scripted source: a target it does not know, or one marked `fail`, closes the connection
/// unanswered (a failed fetch, as the oracle's fetch throws).
async fn source_server(sources: Sources) -> (u16, Seen) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = listener.accept().await else {
                return;
            };
            let sources = Arc::clone(&sources);
            let log = Arc::clone(&log);
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    match s.read(&mut chunk).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    }
                }
                let head = String::from_utf8_lossy(&buf).to_string();
                let mut lines = head.lines();
                let target = lines
                    .next()
                    .unwrap_or("")
                    .split(' ')
                    .nth(1)
                    .unwrap_or("")
                    .to_owned();
                let headers: HashMap<String, String> = lines
                    .filter_map(|l| l.split_once(':'))
                    .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_owned()))
                    .collect();
                log.lock().unwrap().push((target.clone(), headers));
                let found = sources.lock().unwrap().get(&target).cloned();
                let Some(src) = found.filter(|s| !s.fail) else {
                    return;
                };
                tokio::time::sleep(src.delay).await;
                let mut reply = format!(
                    "HTTP/1.1 {} X\r\nContent-Length: {}\r\nConnection: close\r\n",
                    src.status,
                    src.body.len()
                );
                for (k, v) in &src.headers {
                    reply.push_str(&format!("{k}: {v}\r\n"));
                }
                reply.push_str("\r\n");
                let _ = s.write_all(reply.as_bytes()).await;
                let _ = s.write_all(&src.body).await;
            });
        }
    });
    (port, seen)
}

fn env_for(kind: &str, port: u16) -> Vec<(String, String)> {
    let source = format!("http://127.0.0.1:{port}");
    let mut env = vec![
        ("MODEL_MANAGER_KIND".into(), kind.into()),
        ("NOEVIA_QA_MODEL_CARD_SOURCE".into(), source.clone()),
    ];
    match kind {
        "llamacpp" => env.push(("MODEL_MANAGER_BASE_URL".into(), "http://127.0.0.1:9".into())),
        "lemonade" => {
            env.push(("LEMONADE_BASE_URL".into(), format!("{source}/")));
            env.push(("MODEL_MANAGER_API_KEY".into(), "synthetic-key".into()));
        }
        _ => {}
    }
    env
}

/// `"checkedAt":"<iso>"` to `"checkedAt":"X"`.
fn normalise(text: &str) -> String {
    let key = "\"checkedAt\":\"";
    match text.find(key) {
        None => text.to_owned(),
        Some(i) => {
            let rest = &text[i + key.len()..];
            let end = rest.find('"').unwrap();
            format!("{}X{}", &text[..i + key.len()], &rest[end..])
        }
    }
}

/// The oracle's answer for `name` in `section` (`body` for an HTTP scenario) as `JSON.stringify`
/// writes it: read with the order-keeping parser, so key order is compared too.
fn js_text(section: &str, name: &Value, body: bool) -> String {
    let all = js_json::parse(ORACLE).unwrap();
    let js_json::JValue::Arr(items) = all.get(section) else {
        panic!("{section}")
    };
    let item = items
        .iter()
        .find(|c| c.get("name").as_str() == name.as_str())
        .unwrap();
    let answer = item.get("answer");
    js_json::stringify(if body { answer.get("body") } else { answer }).unwrap()
}

/// A scenario's requests: its sources and Node's answer, one per step (the source changes between
/// steps, the front's state does not).
fn steps_of(s: &Value) -> Vec<(&serde_json::Map<String, Value>, &Value)> {
    match (s.get("steps"), s.get("answers")) {
        (Some(Value::Array(steps)), Some(Value::Array(answers))) => steps
            .iter()
            .zip(answers)
            .map(|(st, a)| (st.as_object().unwrap(), a))
            .collect(),
        _ => vec![(s["sources"].as_object().unwrap(), &s["answer"])],
    }
}

/// Node's exact `JSON.stringify` of the answer (`checkedAt` normalised).
fn node_text(answer: &Value) -> &str {
    answer["bodyText"].as_str().unwrap()
}

#[test]
fn inspect_matches_node_on_every_synthetic_header() {
    let oracle: Value = serde_json::from_str(ORACLE).unwrap();
    assert_eq!(
        oracle["core"], "cf5c567ffa890a9556376357c234e8697fabfb44",
        "oracle Core"
    );
    let cases = oracle["inspect"].as_array().unwrap();
    assert!(cases.len() >= 20);
    for case in cases {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(case["gguf"].as_str().unwrap())
            .unwrap();
        let got = match mtp_artifact::inspect(&bytes) {
            Ok(i) => format!(
                "{{\"ok\":{}}}",
                js_json::stringify(&i.inspect_json()).unwrap()
            ),
            Err(e) => {
                js_json::stringify(&js_json::JValue::obj([("error", js_json::JValue::Str(e))]))
                    .unwrap()
            }
        };
        assert_eq!(
            got,
            js_text("inspect", &case["name"], false),
            "{}",
            case["name"]
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_scenario_answers_as_node() {
    let oracle: Value = serde_json::from_str(ORACLE).unwrap();
    let scenarios = oracle["http"].as_array().unwrap();
    assert!(scenarios.len() >= 30);
    let mut by_kind: HashMap<String, Vec<&Value>> = HashMap::new();
    for s in scenarios {
        by_kind
            .entry(s["kind"].as_str().unwrap().to_owned())
            .or_default()
            .push(s);
    }
    let mut checked = 0;
    let mut mismatches: Vec<String> = Vec::new();
    for (kind, list) in by_kind {
        // The front refuses to start with the other engine kind (its statistics stay Node's), so
        // those scenarios cannot reach it here; every_scenario_answers_as_node_in_process replays them.
        if kind == "lemonade" {
            checked += list.len();
            continue;
        }
        let sources: Sources = Arc::new(Mutex::new(HashMap::new()));
        for s in &list {
            for (target, spec) in s["sources"].as_object().unwrap() {
                sources
                    .lock()
                    .unwrap()
                    .insert(target.clone(), source_of(spec));
            }
        }
        let (port, seen) = source_server(Arc::clone(&sources)).await;
        let d = tempfile::tempdir().unwrap();
        let _db = seed_database(d.path());
        let env = env_for(&kind, port);
        let map: HashMap<String, String> = env.iter().cloned().collect();
        if let Err(e) = noevia_server::config::Config::from_lookup(|k| {
            map.get(k).cloned().or_else(|| match k {
                "NOEVIA_LEGACY_UPSTREAM" => Some("http://127.0.0.1:9".into()),
                "UI_DATA_DIR" => Some(d.path().display().to_string()),
                _ => None,
            })
        }) {
            panic!("{kind}: {e}");
        }
        let (addr, stop) = start(d.path(), &env).await;
        for s in list {
            let cookie = if s["role"] == "admin" { ADMIN } else { MEMBER };
            let target = format!("/api/models/mtp-artifact{}", s["query"].as_str().unwrap());
            for (step, (step_sources, answer)) in steps_of(s).into_iter().enumerate() {
                for (t, spec) in step_sources {
                    sources.lock().unwrap().insert(t.clone(), source_of(spec));
                }
                let (status, headers, body) = call(addr, "GET", &target, Some(cookie), b"").await;
                if answer["status"].as_u64() != Some(u64::from(status))
                    || normalise(&body) != node_text(answer)
                {
                    mismatches.push(format!(
                        "{kind} {} step {step}: {status} {}",
                        s["name"],
                        normalise(&body)
                    ));
                }
                assert!(headers.contains("content-type: application/json"));
                assert!(headers.contains("cache-control: no-store"));
            }
            checked += 1;
        }
        let seen = seen.lock().unwrap();
        for (t, h) in seen.iter().filter(|(t, _)| t.contains("/resolve/")) {
            assert_eq!(h.get("range").unwrap(), "bytes=0-16777215", "{t}");
            assert!(h.get("authorization").is_none(), "no key to the source");
        }
        let _ = stop.send(());
    }
    assert!(
        mismatches.is_empty(),
        "{} of {} scenarios differ from Node:\n{}",
        mismatches.len(),
        scenarios.len(),
        mismatches.join("\n")
    );
    assert_eq!(checked, scenarios.len());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cancelled_inspection_cannot_overwrite_a_newer_answer_or_exceed_two_executions() {
    const REV2: &str = "cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd";
    let sources: Sources = Arc::new(Mutex::new(HashMap::new()));
    add_repo(&sources, "synthetic/race", Duration::ZERO);
    for r in ["synthetic/c", "synthetic/d"] {
        add_repo(&sources, r, Duration::from_millis(400));
    }
    let (port, seen) = source_server(Arc::clone(&sources)).await;
    let maint = maint_for("llamacpp", port);
    let uri = uri_for("synthetic/race");
    // A reaches publication and is held there, inside a poll an abort cannot stop.
    let (reached, release) = maint.mtp_pause_before_publish("synthetic/race");
    let a = {
        let (maint, uri) = (Arc::clone(&maint), uri.clone());
        tokio::spawn(async move { mtp_artifact::serve(&maint, &uri).await })
    };
    tokio::task::spawn_blocking(move || reached.recv_timeout(Duration::from_secs(10)))
        .await
        .unwrap()
        .expect("A reached publication");
    // A's only caller leaves: the generation is cancelled, its task still executes.
    a.abort();
    let _ = a.await;
    assert_eq!(maint.mtp_running(), 0, "A is no longer joinable");
    assert_eq!(maint.mtp_executing(), 1, "A still holds its permit");
    // The source moves on; B for the same key runs on the one free permit and publishes.
    {
        let mut s = sources.lock().unwrap();
        s.get_mut("/api/models/synthetic/race").unwrap().body =
            format!("{{\"sha\":\"{REV2}\"}}").into_bytes();
        let file = s[&format!("/synthetic/race/resolve/{REV}/m-Q8_0.gguf")]
            .body
            .clone();
        s.insert(
            format!("/synthetic/race/resolve/{REV2}/m-Q8_0.gguf"),
            Source {
                status: 200,
                headers: Vec::new(),
                body: file,
                fail: false,
                delay: Duration::ZERO,
            },
        );
    }
    let b = body_of(mtp_artifact::serve(&maint, &uri).await).await;
    assert!(b.contains(REV2), "B answered the newer revision: {b}");
    // Two further checks are admitted while A holds a permit; they share the other one.
    let (uc, ud) = (uri_for("synthetic/c"), uri_for("synthetic/d"));
    let (c, d) = tokio::join!(
        mtp_artifact::serve(&maint, &uc),
        mtp_artifact::serve(&maint, &ud)
    );
    for res in [c, d] {
        let body = body_of(res).await;
        assert!(body.starts_with("{\"status\":\"present\""), "{body}");
    }
    assert!(
        maint.mtp_peak_executing() <= 2,
        "peak {}",
        maint.mtp_peak_executing()
    );
    // A is released and finishes; its stale answer is dropped.
    release.send(()).unwrap();
    for _ in 0..100 {
        if maint.mtp_executing() == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(maint.mtp_executing(), 0);
    let metas = requests_to(&seen, "/api/models/synthetic/race");
    let again = body_of(mtp_artifact::serve(&maint, &uri).await).await;
    assert_eq!(again, b, "the cache still holds B's answer");
    assert!(!again.contains(&format!("\"{REV}\"")), "{again}");
    assert_eq!(
        requests_to(&seen, "/api/models/synthetic/race"),
        metas + 1,
        "only the list was read again: the cache answered"
    );
    assert!(
        maint.mtp_peak_executing() <= 2,
        "peak {}",
        maint.mtp_peak_executing()
    );
}

fn present_file() -> Vec<u8> {
    let oracle: Value = serde_json::from_str(ORACLE).unwrap();
    let case = oracle["inspect"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "mtp tensor")
        .unwrap();
    base64::engine::general_purpose::STANDARD
        .decode(case["gguf"].as_str().unwrap())
        .unwrap()
}

const REV: &str = "abababababababababababababababababababab";

fn add_repo(sources: &Sources, repo: &str, delay: Duration) {
    let mut s = sources.lock().unwrap();
    let ok = |body: Vec<u8>, delay| Source {
        status: 200,
        headers: Vec::new(),
        body,
        fail: false,
        delay,
    };
    s.insert(
        format!("/api/models/{repo}?blobs=true"),
        ok(
            br#"{"siblings":[{"rfilename":"m-Q8_0.gguf","size":1}]}"#.to_vec(),
            Duration::ZERO,
        ),
    );
    s.insert(
        format!("/api/models/{repo}"),
        ok(format!("{{\"sha\":\"{REV}\"}}").into_bytes(), delay),
    );
    s.insert(
        format!("/{repo}/resolve/{REV}/m-Q8_0.gguf"),
        ok(present_file(), Duration::ZERO),
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn signed_out_is_refused_before_any_source_read() {
    let sources: Sources = Arc::new(Mutex::new(HashMap::new()));
    add_repo(&sources, "synthetic/out", Duration::ZERO);
    let (port, seen) = source_server(sources).await;
    let d = tempfile::tempdir().unwrap();
    let _db = seed_database(d.path());
    let (addr, stop) = start(d.path(), &env_for("llamacpp", port)).await;
    let q = "/api/models/mtp-artifact?repo=synthetic%2Fout&variant=Q8_0";
    let (status, _, _) = call(addr, "GET", q, None, b"").await;
    assert_eq!(status, 401);
    let (status, _, _) = call(addr, "GET", q, Some("cowork_session=nope"), b"").await;
    assert_eq!(status, 401);
    assert!(seen.lock().unwrap().is_empty());
    // A member is enough (only writes under /api/models/ are administrator-only).
    let (status, _, body) = call(addr, "GET", q, Some(MEMBER), b"").await;
    assert_eq!(status, 200, "{body}");
    assert!(body.starts_with("{\"status\":\"present\""), "{body}");
    // Other methods stay Node's (the dead legacy upstream answers 502).
    let (status, _, _) = call(addr, "POST", q, Some(ADMIN), b"{}").await;
    assert_eq!(status, 502);
    let _ = stop.send(());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn answers_are_cached_and_identical_checks_coalesce() {
    let sources: Sources = Arc::new(Mutex::new(HashMap::new()));
    add_repo(&sources, "synthetic/slow", Duration::from_millis(600));
    let (port, seen) = source_server(Arc::clone(&sources)).await;
    let d = tempfile::tempdir().unwrap();
    let _db = seed_database(d.path());
    let (addr, stop) = start(d.path(), &env_for("llamacpp", port)).await;
    let q = "/api/models/mtp-artifact?repo=synthetic%2Fslow&variant=Q8_0";
    let (a, b) = tokio::join!(
        call(addr, "GET", q, Some(MEMBER), b""),
        call(addr, "GET", q, Some(ADMIN), b"")
    );
    assert_eq!((a.0, b.0), (200, 200));
    assert_eq!(a.2, b.2, "one check, one answer");
    let metas = |seen: &Seen| {
        seen.lock()
            .unwrap()
            .iter()
            .filter(|(t, _)| t == "/api/models/synthetic/slow")
            .count()
    };
    assert_eq!(metas(&seen), 1, "coalesced");
    let (status, _, again) = call(addr, "GET", q, Some(MEMBER), b"").await;
    assert_eq!(status, 200);
    assert_eq!(again, a.2, "cached, checkedAt included");
    assert_eq!(metas(&seen), 1, "the cache answered");
    let _ = stop.send(());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn at_most_two_checks_run_at_once() {
    let sources: Sources = Arc::new(Mutex::new(HashMap::new()));
    for r in ["synthetic/one", "synthetic/two", "synthetic/three"] {
        add_repo(&sources, r, Duration::from_millis(1500));
    }
    let (port, _seen) = source_server(Arc::clone(&sources)).await;
    let d = tempfile::tempdir().unwrap();
    let _db = seed_database(d.path());
    let (addr, stop) = start(d.path(), &env_for("llamacpp", port)).await;
    let q = |r: &str| format!("/api/models/mtp-artifact?repo=synthetic%2F{r}&variant=Q8_0");
    let (q1, q2, q3) = (q("one"), q("two"), q("three"));
    let first = tokio::spawn(async move { call(addr, "GET", &q1, Some(MEMBER), b"").await });
    let second = tokio::spawn(async move { call(addr, "GET", &q2, Some(MEMBER), b"").await });
    tokio::time::sleep(Duration::from_millis(500)).await;
    let (status, _, body) = call(addr, "GET", &q3, Some(MEMBER), b"").await;
    assert_eq!(status, 200);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["status"], "unknown");
    assert_eq!(
        v["reason"],
        "Two artifact checks are already running. Try again shortly."
    );
    assert_eq!(v["files"], serde_json::json!(["m-Q8_0.gguf"]));
    for h in [first, second] {
        let (status, _, body) = h.await.unwrap();
        assert_eq!(status, 200);
        assert!(body.starts_with("{\"status\":\"present\""), "{body}");
    }
    // Once they are done, the third runs (and is not served the refusal from a cache).
    let (_, _, body) = call(addr, "GET", &q("three"), Some(MEMBER), b"").await;
    assert!(body.starts_with("{\"status\":\"present\""), "{body}");
    let _ = stop.send(());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_range_ignoring_source_is_read_only_to_the_limit() {
    let sources: Sources = Arc::new(Mutex::new(HashMap::new()));
    add_repo(&sources, "synthetic/big", Duration::ZERO);
    {
        let mut s = sources.lock().unwrap();
        let mut big = present_file();
        big.resize(mtp_artifact::LIMIT + 4 * 1024 * 1024, 0);
        s.get_mut(&format!("/synthetic/big/resolve/{REV}/m-Q8_0.gguf"))
            .unwrap()
            .body = big;
    }
    let (port, _seen) = source_server(sources).await;
    let d = tempfile::tempdir().unwrap();
    let _db = seed_database(d.path());
    let (addr, stop) = start(d.path(), &env_for("llamacpp", port)).await;
    let (status, _, body) = call(
        addr,
        "GET",
        "/api/models/mtp-artifact?repo=synthetic%2Fbig&variant=Q8_0",
        Some(MEMBER),
        b"",
    )
    .await;
    assert_eq!(status, 200);
    assert!(body.starts_with("{\"status\":\"present\""), "{body}");
    let _ = stop.send(());
}

const JSON_ORACLE: &str = include_str!("fixtures/json-parse-errors-node.json");

fn units_of(v: &Value) -> Vec<u16> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|u| u.as_u64().unwrap() as u16)
        .collect()
}

#[test]
fn json_parse_errors_are_node_22_messages_unit_for_unit() {
    use noevia_server::model_maint::v8_json::parse_error;
    let oracle: Value = serde_json::from_str(JSON_ORACLE).unwrap();
    assert!(oracle["node"].as_str().unwrap().starts_with("v22."));
    let cases = oracle["cases"].as_array().unwrap();
    assert!(cases.len() >= 110);
    for case in cases {
        let ok = case.get("ok").is_some();
        let input = if ok {
            units_of(&case["units"])
        } else {
            units_of(&case["inputUnits"])
        };
        let got = parse_error(&input);
        if ok {
            assert_eq!(got, None, "{:?} parses", case["input"]);
        } else {
            assert_eq!(
                got.map(|m| String::from_utf16_lossy(&m)),
                Some(String::from_utf16_lossy(&units_of(&case["units"]))),
                "{:?}",
                case["input"]
            );
            assert_eq!(
                got_units(&input),
                units_of(&case["units"]),
                "{:?}",
                case["input"]
            );
        }
    }
}

fn got_units(input: &[u16]) -> Vec<u16> {
    noevia_server::model_maint::v8_json::parse_error(input).unwrap()
}

/// The front's handler called in-process for every scenario and every engine kind, the other engine
/// kind included (the front refuses to start with it, so the HTTP replay cannot reach it there).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_scenario_answers_as_node_in_process() {
    use http_body_util::BodyExt;
    use noevia_server::model_maint::Maint;
    let oracle: Value = serde_json::from_str(ORACLE).unwrap();
    let scenarios = oracle["http"].as_array().unwrap();
    let mut by_kind: HashMap<String, Vec<&Value>> = HashMap::new();
    for s in scenarios {
        by_kind
            .entry(s["kind"].as_str().unwrap().to_owned())
            .or_default()
            .push(s);
    }
    assert_eq!(by_kind.len(), 3, "llamacpp, lemonade and none");
    let mut checked = 0;
    let mut mismatches: Vec<String> = Vec::new();
    for (kind, list) in by_kind {
        let sources: Sources = Arc::new(Mutex::new(HashMap::new()));
        for s in &list {
            for (target, spec) in s["sources"].as_object().unwrap() {
                sources
                    .lock()
                    .unwrap()
                    .insert(target.clone(), source_of(spec));
            }
        }
        let (port, seen) = source_server(Arc::clone(&sources)).await;
        let env: HashMap<String, String> = env_for(&kind, port).into_iter().collect();
        let get = |k: &str| env.get(k).cloned();
        let engine = Arc::new(server_stats::Engine::from_lookup(&get).unwrap());
        let sizing = Arc::new(noevia_server::model_sizing::Sizing::from_lookup(
            engine.kind(),
            Arc::clone(&engine),
            &get,
        ));
        let maint = Arc::new(Maint::from_lookup(engine.kind(), &engine, sizing, &get).unwrap());
        for s in list {
            let uri: axum::http::Uri =
                format!("/api/models/mtp-artifact{}", s["query"].as_str().unwrap())
                    .parse()
                    .unwrap();
            for (step, (step_sources, answer)) in steps_of(s).into_iter().enumerate() {
                for (t, spec) in step_sources {
                    sources.lock().unwrap().insert(t.clone(), source_of(spec));
                }
                let res = mtp_artifact::serve(&maint, &uri).await;
                let status = res.status().as_u16();
                let body =
                    String::from_utf8(res.into_body().collect().await.unwrap().to_bytes().to_vec())
                        .unwrap();
                if answer["status"].as_u64() != Some(u64::from(status))
                    || normalise(&body) != node_text(answer)
                {
                    mismatches.push(format!(
                        "{kind} {} step {step}: {status} {}",
                        s["name"],
                        normalise(&body)
                    ));
                }
            }
            checked += 1;
        }
        let seen = seen.lock().unwrap();
        if kind == "lemonade" {
            let (_, h) = seen
                .iter()
                .find(|(t, _)| t.starts_with("/api/v1/pull/variants"))
                .unwrap();
            assert_eq!(h.get("authorization").unwrap(), "Bearer synthetic-key");
        }
        for (t, h) in seen.iter().filter(|(t, _)| t.contains("/resolve/")) {
            assert_eq!(h.get("range").unwrap(), "bytes=0-16777215", "{t}");
            assert!(h.get("authorization").is_none(), "no key to the source");
        }
    }
    assert!(
        mismatches.is_empty(),
        "{} of {} scenarios differ from Node:\n{}",
        mismatches.len(),
        scenarios.len(),
        mismatches.join("\n")
    );
    assert_eq!(checked, scenarios.len());
}

fn maint_for(kind: &str, port: u16) -> Arc<noevia_server::model_maint::Maint> {
    let env: HashMap<String, String> = env_for(kind, port).into_iter().collect();
    let get = |k: &str| env.get(k).cloned();
    let engine = Arc::new(server_stats::Engine::from_lookup(&get).unwrap());
    let sizing = Arc::new(noevia_server::model_sizing::Sizing::from_lookup(
        engine.kind(),
        Arc::clone(&engine),
        &get,
    ));
    Arc::new(
        noevia_server::model_maint::Maint::from_lookup(engine.kind(), &engine, sizing, &get)
            .unwrap(),
    )
}

fn uri_for(repo: &str) -> axum::http::Uri {
    format!(
        "/api/models/mtp-artifact?repo={}&variant=Q8_0",
        repo.replace('/', "%2F")
    )
    .parse()
    .unwrap()
}

async fn body_of(res: axum::http::Response<axum::body::Body>) -> String {
    use http_body_util::BodyExt;
    String::from_utf8(res.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap()
}

fn requests_to(seen: &Seen, part: &str) -> usize {
    seen.lock()
        .unwrap()
        .iter()
        .filter(|(t, _)| t.contains(part))
        .count()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_last_waiter_leaving_cancels_the_inspection_and_frees_its_slot() {
    let sources: Sources = Arc::new(Mutex::new(HashMap::new()));
    add_repo(&sources, "synthetic/left", Duration::from_millis(800));
    add_repo(&sources, "synthetic/next-a", Duration::from_millis(300));
    add_repo(&sources, "synthetic/next-b", Duration::from_millis(300));
    let (port, seen) = source_server(Arc::clone(&sources)).await;
    let maint = maint_for("llamacpp", port);
    let left = uri_for("synthetic/left");
    let gone = tokio::time::timeout(
        Duration::from_millis(200),
        mtp_artifact::serve(&maint, &left),
    )
    .await;
    assert!(
        gone.is_err(),
        "the caller left while the revision was pending"
    );
    assert_eq!(
        maint.mtp_running(),
        0,
        "a newcomer starts a new generation at once"
    );
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert_eq!(
        maint.mtp_executing(),
        0,
        "the aborted task released its permit"
    );
    assert_eq!(
        requests_to(&seen, "/api/models/synthetic/left"),
        2,
        "list and revision only"
    );
    assert_eq!(
        requests_to(&seen, "/synthetic/left/resolve/"),
        0,
        "no file read after the abort"
    );
    // Both slots are free: two other checks run side by side.
    let (ua, ub) = (uri_for("synthetic/next-a"), uri_for("synthetic/next-b"));
    let (a, b) = tokio::join!(
        mtp_artifact::serve(&maint, &ua),
        mtp_artifact::serve(&maint, &ub)
    );
    for res in [a, b] {
        let body = body_of(res).await;
        assert!(body.starts_with("{\"status\":\"present\""), "{body}");
    }
    assert_eq!(maint.mtp_running(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_of_two_waiters_leaving_keeps_the_inspection_for_the_other() {
    let sources: Sources = Arc::new(Mutex::new(HashMap::new()));
    add_repo(&sources, "synthetic/shared", Duration::from_millis(700));
    let (port, seen) = source_server(Arc::clone(&sources)).await;
    let maint = maint_for("llamacpp", port);
    let uri = uri_for("synthetic/shared");
    let (left, stayed) = tokio::join!(
        tokio::time::timeout(
            Duration::from_millis(300),
            mtp_artifact::serve(&maint, &uri)
        ),
        async {
            // Joins after the first has started the inspection.
            tokio::time::sleep(Duration::from_millis(100)).await;
            mtp_artifact::serve(&maint, &uri).await
        }
    );
    assert!(left.is_err(), "the first caller left");
    let body = body_of(stayed).await;
    assert!(body.starts_with("{\"status\":\"present\""), "{body}");
    assert_eq!(
        requests_to(&seen, "/api/models/synthetic/shared?"),
        2,
        "each caller lists"
    );
    assert_eq!(
        seen.lock()
            .unwrap()
            .iter()
            .filter(|(t, _)| t == "/api/models/synthetic/shared")
            .count(),
        1,
        "one coalesced revision read"
    );
    assert_eq!(
        requests_to(&seen, "/synthetic/shared/resolve/"),
        1,
        "one file read"
    );
    assert_eq!(maint.mtp_running(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_variant_list_read_is_cancelled_with_its_caller() {
    let sources: Sources = Arc::new(Mutex::new(HashMap::new()));
    add_repo(&sources, "synthetic/slow-list", Duration::ZERO);
    sources
        .lock()
        .unwrap()
        .get_mut("/api/models/synthetic/slow-list?blobs=true")
        .unwrap()
        .delay = Duration::from_millis(700);
    let (port, seen) = source_server(Arc::clone(&sources)).await;
    let maint = maint_for("llamacpp", port);
    let gone = tokio::time::timeout(
        Duration::from_millis(200),
        mtp_artifact::serve(&maint, &uri_for("synthetic/slow-list")),
    )
    .await;
    assert!(gone.is_err());
    tokio::time::sleep(Duration::from_millis(1000)).await;
    assert_eq!(
        seen.lock()
            .unwrap()
            .iter()
            .filter(|(t, _)| t == "/api/models/synthetic/slow-list")
            .count(),
        0,
        "no revision read after the caller left during the list"
    );
    assert_eq!(maint.mtp_running(), 0);
}
