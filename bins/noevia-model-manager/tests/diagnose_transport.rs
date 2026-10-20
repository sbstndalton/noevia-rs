#![cfg(feature = "native-diagnose")]
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
#[path = "support/diagnose.rs"]
mod support;
use noevia_model_manager::diagnose_docker::{Context, DiagnosisRead, Error};
use std::time::{Duration, Instant};
const START: &str = "2026-10-11T12:34:56.987654321Z";
#[test]
fn native_cutoffs_match_bound_actual_sdk_utc_wire_and_refuse_narrowed_dates() {
    use sha2::Digest;
    use std::io::Read;
    let path = std::env::var_os("NOEVIA_DIAGNOSE_SDK_ORACLE")
        .unwrap_or_else(|| "/tmp/s1-diagnose-sdk-wire-final-gate.json".into());
    // The oracle comes from the actual Docker SDK (the diagnose CI job generates it). On GitHub
    // Actions its absence fails the test; anywhere else the test says it skipped and why.
    if !std::path::Path::new(&path).exists() && std::env::var_os("GITHUB_ACTIONS").is_none() {
        eprintln!("skipped: SDK wire oracle {path:?} is missing; set NOEVIA_DIAGNOSE_SDK_ORACLE");
        return;
    }
    let file = std::fs::File::open(path).unwrap();
    assert!(file.metadata().unwrap().len() <= 1_048_576);
    let mut bytes = Vec::new();
    file.take(1_048_577).read_to_end(&mut bytes).unwrap();
    assert!(bytes.len() <= 1_048_576);
    let fixture: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        fixture.get("oracle_ref").unwrap(),
        "d890ab476d1f1a8b6de799d6428e45b6af42a473"
    );
    assert_eq!(
        fixture.get("api_sha256").unwrap(),
        "800edae73e7f7764845510ddf4e108bc2ea0a639f721e06e825a4a4644600dd6"
    );
    let generator = fixture.get("generator_sha256").unwrap().as_str().unwrap();
    let current = format!(
        "{:x}",
        sha2::Sha256::digest(include_bytes!(
            "../src/bin/generate-diagnose-transport-oracle.rs"
        ))
    );
    // The local historical gate is explicitly bound to its preserved c4 source;
    // CI's newly generated gate must match the current generator source instead.
    assert!(
        generator == current
            || (std::env::var_os("NOEVIA_DIAGNOSE_SDK_ORACLE").is_none()
                && generator == "c4e92ed40ffd3aac17a491237d46030c7bee9212d40fba320e393b152584898e")
    );
    for run in fixture.get("runs").unwrap().as_array().unwrap() {
        let utc = run.get("zone").unwrap() == "UTC";
        for case in run
            .get("result")
            .unwrap()
            .get("cases")
            .unwrap()
            .as_array()
            .unwrap()
        {
            let id = case.get("id").unwrap().as_str().unwrap();
            let started = case.get("started").unwrap().as_str().unwrap();
            let native = noevia_model_manager::diagnose_docker::started_seconds(started);
            if matches!(
                id,
                "missing" | "offset" | "malformed" | "year-one" | "zero-date"
            ) {
                assert!(native.is_err());
                continue;
            }
            let native = native.unwrap();
            let wire = case
                .get("wire")
                .unwrap()
                .as_array()
                .unwrap()
                .first()
                .unwrap();
            let since = wire
                .get("params")
                .unwrap()
                .get("since")
                .unwrap()
                .as_i64()
                .unwrap();
            if utc {
                assert_eq!(native, since);
            } else {
                assert!(since > native);
            }
            assert_eq!(wire.get("params").unwrap().get("tail").unwrap(), 40000);
        }
    }
}
#[tokio::test]
async fn exact_current_run_wire_and_split_utf8() {
    let mut logs = support::frame(1, b"prefix\xe2");
    logs.extend(support::frame(2, b"\x82\xac\n"));
    let inspect = support::metadata("synthetic-id", START, false);
    let (_root, docker, daemon) = support::wire(vec![
        support::response(200, &inspect),
        support::response(200, &logs),
        support::response(200, &inspect),
    ])
    .await;
    let context = Context::new(Instant::now() + Duration::from_secs(2));
    let raw = docker
        .current_logs("synthetic-backend", &context)
        .await
        .unwrap();
    context.cleanup().await;
    let requests = daemon.await.unwrap();
    assert_eq!(requests[1].lines().next(),Some("GET /containers/synthetic-id/logs?stdout=true&stderr=true&timestamps=false&follow=false&tail=40000&since=1791722096 HTTP/1.1"));
    assert_eq!(
        noevia_model_manager::diagnose_docker::lossy(&raw).unwrap(),
        "prefix€\n"
    );
    assert!(requests[2].starts_with("GET /containers/synthetic-id/json "));
}
#[tokio::test]
async fn restart_identity_and_tty_races_discard() {
    for after in [
        support::metadata("synthetic-id", "2026-10-11T12:35:56Z", true),
        support::metadata("replacement-id", START, true),
        support::metadata("synthetic-id", START, false),
    ] {
        let before = support::metadata("synthetic-id", START, true);
        let (_root, docker, daemon) = support::wire(vec![
            support::response(200, &before),
            support::response(200, b"old fatal"),
            support::response(200, &after),
        ])
        .await;
        let context = Context::new(Instant::now() + Duration::from_secs(2));
        let result = docker.current_logs("synthetic-backend", &context).await;
        context.cleanup().await;
        daemon.await.unwrap();
        assert_eq!(result, Err(Error::Ordinary));
    }
}
#[tokio::test]
async fn absent_cutoff_never_reads_unfiltered_history() {
    for started in [
        "",
        "invalid",
        "0000-01-01T00:00:00Z",
        "2026-10-11T12:34:56+02:00",
    ] {
        let (_root, docker, daemon) = support::wire(vec![support::response(
            200,
            &support::metadata("synthetic-id", started, true),
        )])
        .await;
        let context = Context::new(Instant::now() + Duration::from_secs(2));
        let result = docker.current_logs("synthetic-backend", &context).await;
        context.cleanup().await;
        assert_eq!(daemon.await.unwrap().len(), 1);
        assert_eq!(result, Err(Error::Ordinary));
    }
}
#[tokio::test]
async fn all_stopped_first_tag_and_per_candidate_errors() {
    let listing = br#"[{"Id":"stopped"},{"Id":"broken"},{"Id":"second-tag"},{"Id":"excluded"}]"#;
    let meta = |name: &str| {
        serde_json::to_vec(&serde_json::json!({"Id":name,"Name":format!("/{name}"),"Image":format!("sha256:{name}")})).unwrap()
    };
    let (_root, docker, daemon) = support::wire(vec![
        support::response(200, listing),
        support::response(200, &meta("stopped")),
        support::response(200, br#"{"RepoTags":["LLAMA.CPP:server"]}"#),
        support::response(404, b""),
        support::response(200, &meta("second-tag")),
        support::response(200, br#"{"RepoTags":["other","llama.cpp:server"]}"#),
        support::response(200, &meta("model-loader")),
    ])
    .await;
    let context = Context::new(Instant::now() + Duration::from_secs(2));
    let inventory = docker.inventory(&context).await.unwrap();
    context.cleanup().await;
    let requests = daemon.await.unwrap();
    assert_eq!(
        requests[0].lines().next(),
        Some("GET /containers/json?all=true&limit=1025&size=false HTTP/1.1")
    );
    assert_eq!(
        inventory
            .iter()
            .map(|container| container.name.as_str())
            .collect::<Vec<_>>(),
        vec!["stopped"]
    );
}
#[tokio::test]
async fn metadata_and_http_protocol_refuse_whole_acquisition() {
    for response in [
        b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n".to_vec(),
        b"HTTP/1.1 302 Found\r\nContent-Length: 0\r\n\r\n".to_vec(),
        b"HTTP/1.1 200 OK\r\nContent-Length: 1048577\r\n\r\n".to_vec(),
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n[]\r\n0\r\nx-synthetic: trailer\r\n\r\n".to_vec(),
        support::response(200,b"[{}]"),
    ] {
        let (_root,docker,daemon)=support::wire(vec![response]).await;
        let context=Context::new(Instant::now()+Duration::from_secs(2));
        let result=docker.inventory(&context).await;context.cleanup().await;daemon.await.unwrap();
        assert!(matches!(result,Err(Error::Refused)));
    }
}
#[test]
fn framing_and_decoded_caps_are_closed() {
    use noevia_model_manager::diagnose_docker::{decode_frames, lossy};
    for raw in [
        vec![0; 7],
        vec![0; 8],
        vec![3, 0, 0, 0, 0, 0, 0, 0],
        vec![1, 1, 0, 0, 0, 0, 0, 0],
        vec![1, 0, 0, 0, 0, 0, 0, 4, 1],
        vec![1, 0, 0, 0, 255, 255, 255, 255],
    ] {
        assert_eq!(decode_frames(&raw, false), Err(Error::Refused));
    }
    let raw = vec![255; noevia_model_manager::diagnose::LOG_CAP / 3 + 1];
    assert_eq!(lossy(&raw), Err(Error::Refused));
    assert_eq!(decode_frames(b"raw tty", true).unwrap(), b"raw tty");
}
proptest::proptest! {
    #![proptest_config(proptest::test_runner::Config::with_cases(64))]
    #[test]
    fn bounded_frames_and_utf8_are_total(raw in proptest::collection::vec(proptest::prelude::any::<u8>(),0..4096),tty in proptest::prelude::any::<bool>()) {
        if let Ok(decoded)=noevia_model_manager::diagnose_docker::decode_frames(&raw,tty) { proptest::prop_assert!(decoded.len()<=raw.len()); }
        let decoded=noevia_model_manager::diagnose_docker::lossy(&raw).unwrap();
        let expected=String::from_utf8_lossy(&raw);
        proptest::prop_assert_eq!(decoded,expected.as_ref());
    }
}
