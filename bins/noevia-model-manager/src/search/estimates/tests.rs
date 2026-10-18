#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
//! The actual Python differential: every repository case of `tests/fixtures/fit-python.v1.json`
//! (`main._search_repo_runtime` of the services app, recorded by `tools/gen-fit-oracle.py`) is
//! replayed against the same Docker world, backend cache and meminfo. The inventory, the estimate
//! backends and every answer (chips, estimates, reason, native context) must be Python's, byte for
//! byte. Every name and header is synthetic.
use super::*;
use crate::backend_stats::fit::tests::{fixture, stats_for};
use serde_json::Value as V;

/// The child's `JSONResponse({"groups": [...]})` of native groups.
fn render(live: &[Live]) -> String {
    let mut out = String::from("{\"groups\":[");
    for (i, g) in live.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&format!("{{\"fit\":{}", g.fit.get()));
        if let Some(e) = &g.estimates {
            out.push_str(&format!(
                ",\"estimates\":{},\"reason\":{},\"nativeCtx\":{}",
                e.get(),
                json_string(&g.reason),
                g.native_ctx.as_ref().unwrap().get()
            ));
        }
        out.push('}');
    }
    out.push_str("]}");
    out
}

#[tokio::test]
async fn the_repository_estimates_are_pythons() {
    let fx = fixture();
    let cases = fx["repo"].as_array().unwrap();
    assert!(cases.len() >= 5);
    let mut answers = 0;
    for case in cases {
        let id = case["id"].as_str().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let stats = stats_for(case, dir.path());
        let inv = stats.inventory().await.expect(id);
        let gpu: Vec<V> = inv
            .gpu
            .iter()
            .map(|(n, v)| serde_json::json!([n, gguf::py_float_repr(*v)]))
            .collect();
        assert_eq!(
            V::Array(gpu),
            case["inventory"]["gpu"],
            "{id}: fit backends"
        );
        let descriptors = stats.estimate_descriptors(&inv);
        let shown: Vec<V> = descriptors
            .iter()
            .map(|d| {
                let cards: Vec<String> = d
                    .card_vram_gb
                    .iter()
                    .map(|c| match c {
                        Num::Int(n) => n.to_string(),
                        Num::Float(f) => gguf::py_float_repr(*f),
                    })
                    .collect();
                assert_eq!(
                    gguf::py_float_repr(d.host_ram_gb),
                    case["host_ram"].as_str().unwrap()
                );
                serde_json::json!({
                    "name": d.name,
                    "vram": gguf::py_float_repr(d.vram_gb),
                    "gpu_count": d.gpu_count as i64,
                    "cards": format!("[{}]", cards.join(", ")),
                })
            })
            .collect();
        assert_eq!(V::Array(shown), case["backends"], "{id}: estimate backends");
        let cap = case["cache_cap"].as_i64().unwrap();
        for (n, ask) in case["asks"].as_array().unwrap().iter().enumerate() {
            assert_eq!(ask["status"], 200, "{id} #{n}");
            let asks: Vec<Ask> = ask["groups"]
                .as_array()
                .unwrap()
                .iter()
                .map(|g| Ask {
                    bytes: g[0].as_str().unwrap().parse().unwrap(),
                    estimate: g[1].as_bool().unwrap(),
                })
                .collect();
            let mm = ask["mmproj"].as_str().map(|b| b.parse().unwrap());
            let summary = ask["summary"].as_str().map(|s| super::summary(s).unwrap());
            let live = groups(&asks, mm, summary.as_ref(), &inv, &descriptors, cap);
            // Every former caught-exception case is a valid native response now.
            let live = live.unwrap_or_else(|error| panic!("{id} #{n}: {error:?}"));
            assert_eq!(render(&live), ask["body"].as_str().unwrap(), "{id} #{n}");
            answers += 1;
        }
    }
    assert!(answers >= 20);
    // The projector override changes the answer in the tight-budget world.
    let tight = cases
        .iter()
        .find(|c| c["id"] == "projector-budget")
        .unwrap();
    let bodies: Vec<&str> = tight["asks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["body"].as_str().unwrap())
        .collect();
    assert!(bodies[0].contains("\"ctx\":32768,\"ctx_h\":\"32K\",\"gpu_layers\":40"));
    assert!(bodies[1].contains("\"key\":\"fast\",\"label\":\"Fast\",\"ctx\":4096"));
    // A real chat template is past the default string cap, and is still read.
    let long = cases.iter().find(|c| c["id"] == "real-shapes").unwrap()["asks"][0]["summary"]
        .as_str()
        .unwrap();
    assert!(model_files::json::parse(long).is_err());
    assert!(super::summary(long).is_some());
}

#[test]
fn context_json_and_large_projectors_follow_reference_semantics() {
    let parse = |s: &str| super::summary(s).unwrap();
    assert_eq!(
        native_ctx(&parse(r#"{"model":{"context_length":8192}}"#)).unwrap(),
        "8192"
    );
    assert_eq!(
        native_ctx(&parse(r#"{"model":{"context_length":0}}"#)).unwrap(),
        "0"
    );
    assert_eq!(native_ctx(&parse(r#"{"model":{}}"#)).unwrap(), "0");
    assert_eq!(native_ctx(&parse(r#"{"model":null}"#)).unwrap(), "0");
    assert_eq!(native_ctx(&parse(r#"{"arch":"x"}"#)).unwrap(), "0");
    assert_eq!(
        native_ctx(&parse(r#"{"model":{"context_length":"8K"}}"#)).unwrap(),
        "\"8K\""
    );
    assert_eq!(
        native_ctx(&parse(r#"{"model":{"context_length":2.5}}"#)).unwrap(),
        "2.5"
    );
    // `.get` of a list raises in Python; nested context JSON is rendered natively.
    assert!(native_ctx(&parse(r#"{"model":[1]}"#)).is_err());
    assert_eq!(
        native_ctx(&parse(r#"{"model":{"context_length":[1]}}"#)).unwrap(),
        "[1]"
    );
    // No backend: Python's reason, without running the analysis.
    let (e, r) = estimates(
        &parse(r#"{"model":{}}"#),
        1,
        0.0,
        &Value::Arr(Vec::new()),
        1024,
    )
    .unwrap();
    assert_eq!((e.as_str(), r.as_str()), ("[]", NO_GPU));
    // Power-of-two division stays correctly rounded above 2**53 bytes.
    let inv = Inventory {
        gpu: Vec::new(),
        cpu: Vec::new(),
        ram: 0.0,
    };
    let asks = [Ask {
        bytes: 1,
        estimate: false,
    }];
    assert!(groups(&asks, Some(1u128 << 53), None, &inv, &[], 1024).is_ok());
    assert!(groups(&asks, Some((1u128 << 53) - 1), None, &inv, &[], 1024).is_ok());
}

/// The repository read takes the native answer only while the section autoconfig is native (the
/// cache-ram cap is known), and never asks the child then (its address here is closed).
#[tokio::test]
async fn the_repository_read_answers_natively_only_with_the_cap() {
    use crate::search::{live::Source, Group, Search};
    let fx = fixture();
    let case = &fx["repo"][0];
    let ask = &case["asks"][0];
    let dir = tempfile::tempdir().unwrap();
    let groups: Vec<Group> = ask["groups"]
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
        .map(|(i, g)| Group {
            shard_base: format!("synthetic-{i}.gguf"),
            shards: None,
            bytes: g[0].as_str().unwrap().parse().unwrap(),
            quant: None,
            projector: !g[1].as_bool().unwrap(),
            is_gguf: true,
            files: Vec::new(),
        })
        .collect();
    let summary = ask["summary"].as_str().unwrap();
    let mut unreadable = case.clone();
    unreadable["env"]["HOST_RAM_RESERVE_GB"] = V::String("not-a-float".into());
    let cap = Some(case["cache_cap"].as_i64().unwrap());
    for (case, cap, native) in [
        (case, None, false),
        (&unreadable, cap, false),
        (case, cap, true),
    ] {
        let search = Search::new(dir.path(), "http://127.0.0.1:9", "").unwrap();
        search.set_live(Source {
            stats: std::sync::Arc::new(stats_for(case, dir.path())),
            models: dir.path().join("models"),
            ini: dir.path().join("models.ini"),
            trusted: Default::default(),
            estimates_cache_cap_mib: cap,
        });
        let live = search.native_repo_runtime(&groups, Some(summary)).await;
        if native {
            assert_eq!(render(&live.unwrap()), ask["body"].as_str().unwrap());
        } else {
            // No cap (the python section autoconfig), or an unreadable HOST_RAM_RESERVE_GB.
            assert!(live.is_none());
        }
    }
}

#[test]
fn fallback_reasons_log_again_at_powers_of_ten() {
    assert!(fell_back("a synthetic reason for this test"));
    for _ in 2..10 {
        assert!(!fell_back("a synthetic reason for this test"));
    }
    assert!(fell_back("a synthetic reason for this test"));
    assert!(fell_back("another synthetic reason for this test"));
}

#[test]
fn a_summary_past_the_node_cap_is_not_partially_decoded() {
    let many = format!("{{\"model\":[{}0]}}", "0,".repeat(1 << 20));
    assert!(super::summary(&many).is_none());
    assert!(super::summary("{\"model\":[0,0]}").is_some());
}

#[tokio::test]
async fn an_unreadable_summary_skips_docker_and_a_timeout_leaves_estimates_with_the_child() {
    use crate::backend_stats::fit::tests::hung_stats;
    use crate::search::{live::Source, Group, Search};
    use std::sync::{atomic::Ordering, Arc};
    use std::time::{Duration, Instant};
    let dir = tempfile::tempdir().unwrap();
    let (stats, calls, peer) = hung_stats(dir.path());
    let search = Search::new(dir.path(), "http://127.0.0.1:9", "").unwrap();
    search.set_live(Source {
        stats: Arc::new(stats),
        models: dir.path().join("models"),
        ini: dir.path().join("models.ini"),
        trusted: Default::default(),
        estimates_cache_cap_mib: Some(1024),
    });
    let groups = [Group {
        shard_base: "qa.gguf".into(),
        shards: None,
        bytes: 1,
        quant: None,
        projector: false,
        is_gguf: true,
        files: Vec::new(),
    }];
    let before = Instant::now();
    assert!(search
        .native_repo_runtime(&groups, Some("{bad"))
        .await
        .is_none());
    assert!(before.elapsed() < Duration::from_secs(2));
    assert_eq!(
        calls.load(Ordering::Relaxed),
        0,
        "an unreadable summary must not open Docker"
    );
    let before = Instant::now();
    assert!(tokio::time::timeout(
        Duration::from_secs(20),
        search.native_repo_runtime(&groups, Some("{}"))
    )
    .await
    .unwrap()
    .is_none());
    assert!(before.elapsed() >= Duration::from_secs(9));
    assert!(before.elapsed() < Duration::from_secs(20));
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    let retry = Instant::now();
    assert!(tokio::time::timeout(
        Duration::from_secs(20),
        search.native_repo_runtime(&groups, Some("{}"))
    )
    .await
    .unwrap()
    .is_none());
    assert!(retry.elapsed() < Duration::from_secs(2));
    assert_eq!(
        calls.load(Ordering::Relaxed),
        1,
        "cooldown does not read Docker again"
    );
    peer.abort();
}

fn owned_fixture() -> V {
    serde_json::from_str(include_str!(
        "../../../tests/fixtures/repo-runtime-python.v1.json"
    ))
    .unwrap()
}

/// The native fake Docker failure is explicit. Python's source-bound fixture was produced
/// by the real SDK seeing missing/HTTP500/read-timeout Unix daemons, not this fake.
fn native_case(case: &V) -> V {
    let mut case = case.clone();
    if case["docker"]["qa_fault"].is_string() {
        case["docker"] = V::Null;
    }
    case
}

#[tokio::test]
async fn owned_repository_runtime_matches_every_actual_reference_response() {
    let fx = owned_fixture();
    assert_eq!(
        fx["sources"]["helpers.py"],
        "fcf2ccb14e5e068f97bb7a4b7985ab7f4f4855b940eb6d9e3838f239ad97918d"
    );
    assert_eq!(
        fx["sources"]["main.py"],
        "8a336283aaa001a8eca2f3476e2d85e0ae17bc8b72f0bf8fb13b64993339212a"
    );
    let mut good = 0;
    let mut failed = 0;
    for case in fx["repo"].as_array().unwrap() {
        let dir = tempfile::tempdir().unwrap();
        let stats = stats_for(&native_case(case), dir.path());
        let inventory = stats.repository_inventory().await.unwrap();
        let descriptors = stats.estimate_descriptors(&inventory);
        for ask in case["asks"].as_array().unwrap() {
            let groups: Vec<Ask> = ask["groups"]
                .as_array()
                .unwrap()
                .iter()
                .map(|g| Ask {
                    bytes: g[0].as_str().unwrap().parse().unwrap(),
                    estimate: g[1].as_bool().unwrap(),
                })
                .collect();
            let summary = ask["summary"]
                .as_str()
                .map(|text| super::summary(text).unwrap());
            let result = super::groups(
                &groups,
                ask["mmproj"].as_str().map(|text| text.parse().unwrap()),
                summary.as_ref(),
                &inventory,
                &descriptors,
                case["cache_cap"].as_i64().unwrap(),
            );
            match ask["status"].as_u64().unwrap() {
                200 => {
                    let actual = result.unwrap_or_else(|error| panic!("{}: {error:?}", case["id"]));
                    assert_eq!(
                        render(&actual),
                        ask["body"].as_str().unwrap(),
                        "{}",
                        case["id"]
                    );
                    good += 1;
                }
                500 => {
                    assert!(
                        matches!(result, Err(Error::ReferenceFailure)),
                        "{}",
                        case["id"]
                    );
                    failed += 1;
                }
                status => panic!("unqualified reference status {status}"),
            }
        }
    }
    assert_eq!((good, failed), (38, 2));
}

#[test]
fn full_context_json_and_unsupported_resource_limits_are_distinct() {
    let parse = |text| super::summary(text).unwrap();
    assert_eq!(
        native_ctx(&parse(
            r#"{"model":{"context_length":{"qa":[1,true,null]}}}"#
        ))
        .unwrap(),
        r#"{"qa":[1,true,null]}"#
    );
    for text in [
        r#"{"model":{"context_length":NaN}}"#,
        r#"{"model":{"context_length":Infinity}}"#,
        r#"{"model":"qa"}"#,
    ] {
        assert!(matches!(
            native_ctx(&parse(text)),
            Err(Error::ReferenceFailure)
        ));
    }
    // Bounded parser refusal: no partial object/number decoding and no guessed result.
    for text in [
        "{bad".to_owned(),
        format!("{}0{}", "[".repeat(65), "]".repeat(65)),
        format!(r#"{{"qa":"{}"}}"#, "q".repeat(SUMMARY_CAP)),
    ] {
        assert!(super::summary(&text).is_none());
    }
}

#[cfg(feature = "native-routing")]
fn routing_cases(fixture: &V) -> Vec<V> {
    let mut cases = fixture["repo"].as_array().unwrap().clone();
    let shapes = &fixture["docker_shapes"];
    for answer in shapes["answers"].as_array().unwrap() {
        let mut case = shapes["case"].clone();
        let mut ask = shapes["ask"].clone();
        ask["status"] = answer["status"].clone();
        ask["body"] = answer["body"].clone();
        case["id"] = answer["fault"].clone();
        case["qa_fault"] = answer["fault"].clone();
        case["asks"] = serde_json::json!([ask]);
        cases.push(case);
    }
    cases
}

#[cfg(feature = "native-routing")]
#[tokio::test]
async fn strict_real_router_never_calls_companion_for_reference_cases_or_bounded_refusals() {
    use crate::search::{live::Source, Hub, Search, Timeouts};
    use axum::{body::Body, http::Request, Extension};
    use http_body_util::BodyExt;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tower::ServiceExt;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let child_base = format!("http://{}", listener.local_addr().unwrap());
    let count = Arc::new(AtomicUsize::new(0));
    let counted = count.clone();
    let child = tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            counted.fetch_add(1, Ordering::SeqCst);
            let mut bytes = [0; 8192];
            let _ = stream.read(&mut bytes).await;
            let _ = stream
                .write_all(
                    b"HTTP/1.1 500 QA
Content-Length: 0
Connection: close

",
                )
                .await;
        }
    });
    let fx = owned_fixture();
    let mut checked = 0;
    for case in &routing_cases(&fx) {
        for ask in case["asks"].as_array().unwrap() {
            let groups = ask["groups"].as_array().unwrap();
            // The private-contract oracle also includes shapes that cannot be a Hub group.
            if !ask["mmproj"].is_null()
                || groups.len() != 1
                || groups[0][1] != true
                || groups[0][0].as_str().unwrap().parse::<i64>().is_err()
            {
                continue;
            }
            let dir = tempfile::tempdir().unwrap();
            let db = rusqlite::Connection::open(dir.path().join("model_loader.db")).unwrap();
            db.execute("CREATE TABLE kv(key TEXT PRIMARY KEY,value TEXT)", [])
                .unwrap();
            drop(db);
            let size: i64 = groups[0][0].as_str().unwrap().parse().unwrap();
            let listing =
                serde_json::json!([{"type":"file","path":"qa.gguf","size":size}]).to_string();
            let hex: String = listing
                .as_bytes()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
            let answers = Arc::new(Mutex::new(serde_json::json!({
                "/api/models/qa/repo/tree/main?recursive=true&expand=true": {
                    "status":200,"headers":[["content-type","application/json"]],"body":hex
                }
            })));
            let (hub, _, server) = crate::search::tests::hub(answers, String::new()).await;
            let search = Search::with(
                dir.path(),
                Hub {
                    api: format!("{hub}/api"),
                    resolve: hub,
                },
                &child_base,
                "",
                || 1.0,
                Timeouts::default(),
            )
            .unwrap();
            search.set_live(Source {
                stats: Arc::new(if let Some(fault) = case["qa_fault"].as_str() {
                    crate::backend_stats::fit::tests::stats_with_reference_fault(
                        case,
                        fault,
                        dir.path(),
                    )
                } else {
                    stats_for(&native_case(case), dir.path())
                }),
                models: dir.path().join("models"),
                ini: dir.path().join("models.ini"),
                trusted: Default::default(),
                estimates_cache_cap_mib: case["cache_cap"].as_i64(),
            });
            // Header mutations are explicit private-contract cases. Seed the cache to exercise
            // the actual router/HTTP ownership boundary with those exact summary bytes.
            crate::search::lock(&search.headers).put(
                "qa/repo/qa.gguf".into(),
                ask["summary"].as_str().map(Arc::from),
            );
            let shared = Arc::new(search);
            let mut state = crate::test_support::test_state(&child_base);
            state.search = Some(shared.clone());
            let app = crate::router(Arc::new(state)).layer(Extension(crate::routing::Enabled));
            let response = tokio::time::timeout(
                std::time::Duration::from_secs(15),
                app.clone().oneshot(
                    Request::builder()
                        .uri("/api/v1/search/repo?repo=qa/repo")
                        .body(Body::empty())
                        .unwrap(),
                ),
            )
            .await
            .unwrap()
            .unwrap();
            assert_eq!(
                response.status().as_u16(),
                ask["status"].as_u64().unwrap() as u16,
                "{}",
                case["id"]
            );
            if response.status() == 200 {
                let bytes = response.into_body().collect().await.unwrap().to_bytes();
                let actual: V = serde_json::from_slice(&bytes).unwrap();
                let expected: V = serde_json::from_str(ask["body"].as_str().unwrap()).unwrap();
                let actual = &actual["groups"][0];
                let expected = &expected["groups"][0];
                for key in ["fit", "estimates", "nativeCtx"] {
                    assert_eq!(actual[key], expected[key], "{}: {key}", case["id"]);
                }
                assert_eq!(
                    actual["estimatesReason"].as_str().unwrap_or(""),
                    expected["reason"].as_str().unwrap_or(""),
                    "{}",
                    case["id"]
                );
            }
            assert_eq!(
                count.load(Ordering::SeqCst),
                0,
                "strict ownership sent a companion RPC"
            );
            checked += 1;
            // Positive tripwire: strict refuses a cap before Docker; compatibility still asks
            // its configured companion. No silent fallback on either strict result/error.
            crate::search::lock(&shared.headers)
                .put("qa/repo/qa.gguf".into(), Some(Arc::from("{bad")));
            let response = app
                .oneshot(
                    Request::builder()
                        .uri("/api/v1/search/repo?repo=qa/repo")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), 503);
            assert_eq!(count.load(Ordering::SeqCst), 0);
            server.abort();
            let _ = server.await;
        }
    }
    assert!(checked >= 15);
    // Independent compatibility control using an installed source and known unsupported cap.
    let dir = tempfile::tempdir().unwrap();
    let case = &fx["repo"][0];
    let search = Search::new(dir.path(), &child_base, "").unwrap();
    search.set_live(Source {
        stats: Arc::new(stats_for(case, dir.path())),
        models: dir.path().join("models"),
        ini: dir.path().join("models.ini"),
        trusted: Default::default(),
        estimates_cache_cap_mib: Some(1024),
    });
    let groups = [crate::search::Group {
        shard_base: "qa.gguf".into(),
        shards: None,
        bytes: 1,
        quant: None,
        projector: false,
        is_gguf: true,
        files: Vec::new(),
    }];
    assert!(search
        .native_repo_runtime(&groups, Some("{bad"))
        .await
        .is_none());
    assert!(search.repo_runtime(&groups, Some("{bad")).await.is_err());
    assert_eq!(
        count.load(Ordering::SeqCst),
        1,
        "tripwire must detect an actual private RPC"
    );
    child.abort();
    let _ = child.await;
}

#[test]
fn finite_reserve_forms_match_actual_settings_acceptance_without_defaulting() {
    let fx = owned_fixture();
    for (text, reference) in fx["reserve_forms"].as_object().unwrap() {
        let reference = reference.as_str().unwrap();
        let want = reference
            .parse::<f64>()
            .ok()
            .filter(|value| value.is_finite());
        let actual = crate::backend_stats::fit::reserve_gb(Some(text));
        match (actual, want) {
            (Some(actual), Some(want)) => assert_eq!(actual.to_bits(), want.to_bits(), "{text:?}"),
            (None, None) => {}
            _ => panic!("Settings grammar mismatch for {text:?}: {reference:?} vs {actual:?}"),
        }
    }
    assert_eq!(
        crate::backend_stats::fit::reserve_gb(Some(&"1".repeat(4097))),
        None
    );
}
