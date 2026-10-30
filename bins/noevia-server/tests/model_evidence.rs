//! Real HTTP through the native front for `GET /api/models/evidence` and `POST /api/models/evidence/import`
//! (sbstndalton/noevia#2210). tests/fixtures/model-maint/evidence-cases.json holds scenarios first run through
//! Core's own routes, manager and evidence store (record-evidence.cjs, Core b6113b57, before the routes moved)
//! over synthetic models, a scripted router and a scripted model-card source. The front replays each
//! operation (a record is queued in the inbox exactly as Node's producers do) and must answer every request
//! with the same status and body, and leave the same evidence log, with record ids and clock times normalised.
//! The model-card source is the QA source (`NOEVIA_QA_MODEL_CARD_SOURCE`), so nothing reaches the Internet.
//! Then: the front is the store's only writer, and what Node's old log and the front's log share.
//! Synthetic data only; no engine is touched.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
mod maint_support;
use maint_support::*;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const CASES: &str = include_str!("fixtures/model-maint/evidence-cases.json");

/// `"at":123` and `"retrievedAt":123` to `...:0`; `"id":"ev_<uuid>"` to `"id":"ev_X"`.
fn normalise(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    loop {
        let next = ["\"at\":", "\"retrievedAt\":", "\"id\":\"ev_"]
            .iter()
            .filter_map(|k| rest.find(k).map(|i| (i, *k)))
            .min_by_key(|(i, _)| *i);
        let Some((i, key)) = next else {
            out.push_str(rest);
            return out;
        };
        out.push_str(&rest[..i + key.len()]);
        rest = &rest[i + key.len()..];
        if key.ends_with("ev_") {
            out.push('X');
            rest = rest.trim_start_matches(|c: char| c.is_ascii_hexdigit() || c == '-');
        } else {
            out.push('0');
            rest = rest.trim_start_matches(|c: char| c.is_ascii_digit());
        }
    }
}

/// The text around the first place `a` differs from `b` (all of it when they are equal), so a failure
/// shows the difference and not two whole answers.
fn around_first_difference(a: &str, b: &str) -> String {
    let at = a
        .bytes()
        .zip(b.bytes())
        .position(|(x, y)| x != y)
        .unwrap_or(a.len().min(b.len()));
    if a == b {
        return "same".into();
    }
    let from = a.floor_char_boundary(at.saturating_sub(120));
    let to = a.ceil_char_boundary((at + 160).min(a.len()));
    format!("...{}...", &a[from..to])
}

fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.' {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

fn set_mtime(path: &Path, secs: u64) {
    std::fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(std::time::UNIX_EPOCH + Duration::from_secs(secs))
        .unwrap();
}

#[tokio::test]
async fn every_recorded_core_answer_and_evidence_log_is_the_fronts() {
    let doc: Value = serde_json::from_str(CASES).unwrap();
    let port = doc["port"].as_u64().unwrap() as u16;
    let mtime = doc["mtime"].as_u64().unwrap();
    let mut answers_checked = 0;
    // serde_json sorts object keys; a record keeps the key order Node wrote it in, so the records are
    // read again through js_json, which keeps it.
    let ordered = js_json::parse(CASES).unwrap();
    let ordered_cases = match ordered.get("cases") {
        js_json::JValue::Arr(items) => items.clone(),
        _ => panic!("cases"),
    };
    for (case_index, case) in doc["cases"].as_array().unwrap().iter().enumerate() {
        let name = case["name"].as_str().unwrap();
        let model = case["model"].as_str().unwrap().to_owned();
        let d = tempfile::tempdir().unwrap();
        let _db = seed_database(d.path());
        let models = d.path().join("models");
        std::fs::create_dir_all(models.join("fx")).unwrap();
        let file = models.join("fx/fx.gguf");
        std::fs::write(&file, vec![7u8; 2048]).unwrap();
        set_mtime(&file, mtime);
        if case["mmproj"] == true {
            std::fs::write(models.join("fx/mm.gguf"), vec![3u8; 1024]).unwrap();
            set_mtime(&models.join("fx/mm.gguf"), mtime);
        }
        if case["draft"] == true {
            std::fs::write(models.join("fx/draft.gguf"), vec![9u8; 512]).unwrap();
            set_mtime(&models.join("fx/draft.gguf"), mtime);
        }
        let ini = d.path().join("models.ini");
        let ini_text = match case["iniPad"].as_u64() {
            Some(pad) => format!(
                "[fx]\nmodel = /models/fx/fx.gguf\n; {}\n",
                "x".repeat(pad as usize)
            ),
            None => case["ini"].as_str().unwrap().to_owned(),
        };
        std::fs::write(&ini, ini_text).unwrap();

        // The scripted router, on the port the recording used (the address is part of every identity).
        let listing = case["listing"].as_str().unwrap_or("").to_owned();
        let row = json!({"id": model, "status": {"value": "unloaded", "preset": "p", "args": if case["mmproj"] == true {json!(["--model", "/models/fx/fx.gguf", "--mmproj", "/models/fx/mm.gguf"])} else {json!(["--model", "/models/fx/fx.gguf"])}}});
        let router_handler: Handler = Arc::new(move |req: &Req| match req.target.as_str() {
            "/models" if listing == "down" => Answer::Drop,
            "/models" => Answer::Reply(
                200,
                json!({"data": if listing == "empty" { json!([]) } else { json!([row]) }})
                    .to_string(),
            ),
            "/props" => Answer::Reply(200, r#"{"build_info":"b-synthetic"}"#.into()),
            _ => Answer::Reply(404, "{}".into()),
        });
        // The previous scenario's router is being stopped; its port is free again in a moment.
        let mut bound = None;
        for _ in 0..100 {
            match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
                Ok(l) => {
                    bound = Some(l);
                    break;
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
            }
        }
        let listener =
            bound.unwrap_or_else(|| panic!("port {port} is needed for the recorded addresses"));
        let (_, router_seen, router_task) = engine_on(listener, router_handler, port).await;
        let _router_stop = Stop(router_task);
        // The scripted model-card source.
        let table: Arc<Mutex<HashMap<String, (u64, Value)>>> = Arc::new(Mutex::new(HashMap::new()));
        let set_table = |t: &Value| {
            let mut g = table.lock().unwrap();
            for (path, v) in t.as_object().unwrap() {
                g.insert(
                    path.clone(),
                    (v["status"].as_u64().unwrap(), v["body"].clone()),
                );
            }
        };
        set_table(&case["hf"]);
        let t2 = Arc::clone(&table);
        let hf_handler: Handler =
            Arc::new(move |req: &Req| match t2.lock().unwrap().get(&req.target) {
                Some((status, Value::String(s))) => Answer::Reply(*status as u16, s.clone()),
                Some((status, body)) => Answer::Reply(*status as u16, body.to_string()),
                None => Answer::Reply(404, r#"{"error":"not found"}"#.into()),
            });
        let (hf_port, _) = engine(hf_handler).await;

        let base = format!("http://127.0.0.1:{port}/v1");
        let sfx = suffix(&base);
        let evidence_dir = d.path().join(format!("evidence-{sfx}"));
        let inbox = d.path().join(format!("evidence-inbox-{sfx}"));
        let env: Vec<(String, String)> = vec![
            (
                "MODEL_MANAGER_KIND".into(),
                case["kind"].as_str().unwrap().into(),
            ),
            ("MODEL_MANAGER_BASE_URL".into(), base),
            ("LLAMACPP_PRESET_PATH".into(), ini.display().to_string()),
            ("LLAMACPP_MODELS_PATH".into(), models.display().to_string()),
            (
                "NOEVIA_QA_MODEL_CARD_SOURCE".into(),
                format!("http://127.0.0.1:{hf_port}"),
            ),
        ];
        let (addr, stop) = start(d.path(), &env).await;
        let cookie = |role: &str| if role == "admin" { ADMIN } else { MEMBER };
        let mut got: Vec<(u16, String)> = Vec::new();
        let mut queued = 0u32;
        for (op_index, op) in case["ops"].as_array().unwrap().iter().enumerate() {
            match op["op"].as_str().unwrap() {
                "get" => {
                    let (s, h, b) = call(
                        addr,
                        "GET",
                        &format!("/api/models/evidence?model={}", encode(&model)),
                        Some(cookie(case["role"].as_str().unwrap())),
                        b"",
                    )
                    .await;
                    assert!(h.contains("cache-control: no-store"), "{name}");
                    got.push((s, b));
                }
                "raw" => {
                    let role = op["role"]
                        .as_str()
                        .unwrap_or(case["role"].as_str().unwrap());
                    let path = format!(
                        "{}{}",
                        op["path"].as_str().unwrap_or("/api/models/evidence"),
                        op["query"].as_str().unwrap_or("")
                    );
                    let (s, _, b) = call(
                        addr,
                        op["method"].as_str().unwrap(),
                        &path,
                        Some(cookie(role)),
                        op["body"].as_str().unwrap_or("").as_bytes(),
                    )
                    .await;
                    got.push((s, b));
                }
                "import" => {
                    let (s, _, b) = call(
                        addr,
                        "POST",
                        "/api/models/evidence/import",
                        Some(ADMIN),
                        op["body"].to_string().as_bytes(),
                    )
                    .await;
                    got.push((s, b));
                }
                "record" => {
                    // How Node's producers hand a record over: a whole file in the inbox, then the front takes it.
                    queued += 1;
                    std::fs::create_dir_all(&inbox).unwrap();
                    let record = match ordered_cases[case_index].get("ops") {
                        js_json::JValue::Arr(ops) => ops[op_index].get("record").clone(),
                        _ => panic!("ops"),
                    };
                    let mut fields = vec![
                        ("kind", js_json::JValue::Str("record".into())),
                        ("model", js_json::JValue::Str(model.clone())),
                    ];
                    // A non-reported record carries the identity it was measured on; here, the live one.
                    if record.get("result").as_str() != Some("reported") {
                        let (_, _, answer) = call(
                            addr,
                            "GET",
                            &format!("/api/models/evidence?model={}", encode(&model)),
                            Some(ADMIN),
                            b"",
                        )
                        .await;
                        let answer: Value = serde_json::from_str(&answer).unwrap();
                        if let Some(hash) = answer["identityHash"].as_str() {
                            fields.push(("identityHash", js_json::JValue::Str(hash.into())));
                        }
                    }
                    fields.push(("record", record));
                    let entry = js_json::stringify(&js_json::JValue::obj(fields)).unwrap();
                    let name_ = format!(
                        "{:015}-{queued:08x}",
                        1_700_000_000_000u64 + u64::from(queued)
                    );
                    std::fs::write(inbox.join(format!("{name_}.tmp")), entry).unwrap();
                    std::fs::rename(
                        inbox.join(format!("{name_}.tmp")),
                        inbox.join(format!("{name_}.json")),
                    )
                    .unwrap();
                    let started = std::time::Instant::now();
                    while inbox.join(format!("{name_}.json")).exists() {
                        assert!(
                            started.elapsed() < Duration::from_secs(10),
                            "{name}: the front did not take the record"
                        );
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                }
                "ini" => std::fs::write(&ini, op["text"].as_str().unwrap()).unwrap(),
                "file" => {
                    std::fs::write(
                        &file,
                        vec![
                            op["fill"].as_u64().unwrap() as u8;
                            op["size"].as_u64().unwrap() as usize
                        ],
                    )
                    .unwrap();
                    set_mtime(&file, mtime);
                }
                "rmfile" => std::fs::remove_file(&file).unwrap(),
                "hf" => set_table(&op["set"]),
                other => panic!("{other}"),
            }
        }
        let _ = stop.send(());
        let expect = &case["expect"];
        let want = expect["answers"].as_array().unwrap();
        assert_eq!(got.len(), want.len(), "{name}");
        for (i, ((status, body), w)) in got.iter().zip(want).enumerate() {
            // The recorded source URLs are Hugging Face's; the front under test reads the QA source.
            let (got_body, want_body) = (
                normalise(&body.replace(
                    &format!("http://127.0.0.1:{hf_port}"),
                    "https://huggingface.co",
                )),
                normalise(w["body"].as_str().unwrap()),
            );
            assert_eq!(
                (*status, around_first_difference(&got_body, &want_body)),
                (
                    w["status"].as_u64().unwrap() as u16,
                    around_first_difference(&want_body, &got_body)
                ),
                "{name}: answer {i}"
            );
            answers_checked += 1;
        }
        let log = std::fs::read_to_string(evidence_dir.join("evidence.jsonl")).ok();
        assert_eq!(
            log.as_deref().map(|l| normalise(&l.replace(
                &format!("http://127.0.0.1:{hf_port}"),
                "https://huggingface.co"
            ))),
            expect["log"].as_str().map(normalise),
            "{name}: the evidence log"
        );
        // The scripted router saw only the reads the work needs.
        assert!(
            router_seen
                .lock()
                .unwrap()
                .iter()
                .all(|r| r.method == "GET" && (r.target == "/models" || r.target == "/props")),
            "{name}"
        );
    }
    assert!(answers_checked >= 60, "{answers_checked} answers compared");
}

#[tokio::test]
async fn the_front_is_the_only_writer_and_a_bad_inbox_entry_is_dropped_without_harm() {
    let d = tempfile::tempdir().unwrap();
    let _db = seed_database(d.path());
    let models = d.path().join("models/fx");
    std::fs::create_dir_all(&models).unwrap();
    std::fs::write(models.join("fx.gguf"), vec![7u8; 2048]).unwrap();
    let ini = d.path().join("models.ini");
    std::fs::write(&ini, "[fx]\nmodel = /models/fx/fx.gguf\nctx-size = 8192\n").unwrap();
    let router: Handler = Arc::new(|req: &Req| {
        match req.target.as_str() {
        "/models" => Answer::Reply(200, r#"{"data":[{"id":"fx","status":{"value":"unloaded","preset":"p","args":["--model","/models/fx/fx.gguf"]}}]}"#.into()),
        "/props" => Answer::Reply(200, r#"{"build_info":"b"}"#.into()),
        _ => Answer::Reply(404, "{}".into()),
    }
    });
    let (rport, _) = engine(router).await;
    let base = format!("http://127.0.0.1:{rport}/v1");
    let sfx = suffix(&base);
    let inbox = d.path().join(format!("evidence-inbox-{sfx}"));
    std::fs::create_dir_all(&inbox).unwrap();
    // Unusable entries of every kind, then a good one: all are taken, only the good one is stored.
    let credential = json!({"kind": "record", "model": "fx", "record": {"category": "vision", "result": "passed", "limitations": ["Bearer abcdefgh12345678"]}});
    let bad: Vec<(&str, String)> = vec![
        ("001-notjson.json", "not json".into()),
        ("002-nokind.json", json!({"model": "fx"}).to_string()),
        ("003-nomodel.json", json!({"kind": "record", "record": {}}).to_string()),
        ("004-invalid.json", json!({"kind": "record", "model": "fx", "record": {"category": "vision", "result": "maybe"}}).to_string()),
        ("005-credential.json", credential.to_string()),
        ("006-unknown.json", json!({"kind": "frobnicate", "model": "fx"}).to_string()),
        ("007-model.json", json!({"kind": "record", "model": "x".repeat(201), "record": {"category": "vision", "result": "passed"}}).to_string()),
        ("008-huge.json", format!("{{\"kind\":\"record\",\"model\":\"fx\",\"pad\":\"{}\"}}", "x".repeat(70_000))),
        ("009-partial.tmp", "{".into()),
        ("010-good.json", json!({"kind": "record", "model": "fx", "record": {"category": "vision", "result": "reported", "value": null, "limitations": []}}).to_string()),
    ];
    for (n, t) in &bad {
        std::fs::write(inbox.join(n), t).unwrap();
    }
    std::os::unix::fs::symlink(d.path().join("models.ini"), inbox.join("011-link.json")).unwrap();
    let (addr, stop) = start(
        d.path(),
        &[
            ("MODEL_MANAGER_KIND".into(), "llamacpp".into()),
            ("MODEL_MANAGER_BASE_URL".into(), base),
            ("LLAMACPP_PRESET_PATH".into(), ini.display().to_string()),
            (
                "LLAMACPP_MODELS_PATH".into(),
                d.path().join("models").display().to_string(),
            ),
        ],
    )
    .await;
    let started = std::time::Instant::now();
    loop {
        let left: Vec<String> = std::fs::read_dir(&inbox)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        if left == ["009-partial.tmp"] {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "left in the inbox: {left:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let log =
        std::fs::read_to_string(d.path().join(format!("evidence-{sfx}/evidence.jsonl"))).unwrap();
    assert_eq!(log.lines().count(), 1, "{log}");
    assert!(log.contains("\"category\":\"vision\""));
    assert!(
        !log.contains("Bearer"),
        "a credential-shaped record is never stored"
    );
    // The log is the format Core reads: one JSON object per line, id/at/limitations first.
    let first: Value = serde_json::from_str(log.lines().next().unwrap()).unwrap();
    assert!(first["id"].as_str().unwrap().starts_with("ev_"));
    assert_eq!(first["identity"]["backend"], "llamacpp");
    assert_eq!(first["identityHash"].as_str().unwrap().len(), 64);
    assert!(log.starts_with("{\"id\":\"ev_"));
    // And the read answers from it.
    let (s, _, b) = call(
        addr,
        "GET",
        "/api/models/evidence?model=fx",
        Some(MEMBER),
        b"",
    )
    .await;
    let body: Value = serde_json::from_str(&b).unwrap();
    assert_eq!(s, 200);
    assert_eq!(body["categories"][1]["category"], "vision");
    assert_eq!(body["categories"][1]["state"], "verified");
    let _ = stop.send(());
}

#[tokio::test]
async fn signed_out_other_methods_and_the_recheck_are_not_the_fronts() {
    let d = tempfile::tempdir().unwrap();
    let _db = seed_database(d.path());
    let (addr, stop) = start(
        d.path(),
        &[
            ("MODEL_MANAGER_KIND".into(), "llamacpp".into()),
            (
                "MODEL_MANAGER_BASE_URL".into(),
                "http://127.0.0.1:9/v1".into(),
            ),
        ],
    )
    .await;
    let (s, ..) = call(addr, "GET", "/api/models/evidence?model=fx", None, b"").await;
    assert_eq!(s, 401);
    let (s, ..) = call(addr, "POST", "/api/models/evidence/import", None, b"{}").await;
    assert_eq!(s, 401);
    // Node's: the other methods of both paths, and the recheck (an image probe that may load the model).
    for (m, p) in [
        ("POST", "/api/models/evidence"),
        ("GET", "/api/models/evidence/import"),
        ("POST", "/api/models/evidence/recheck"),
        ("GET", "/api/models/evidence/recheck"),
    ] {
        let (s, ..) = call(addr, m, p, Some(ADMIN), b"{}").await;
        assert_eq!(s, 502, "{m} {p}");
    }
    let _ = stop.send(());
}
