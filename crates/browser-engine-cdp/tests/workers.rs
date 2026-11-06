#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
use browser_engine_cdp::{Approval, BrowserEngine, CdpEngine, Launch};
use serde_json::{json, Value};
use std::{collections::BTreeMap, path::Path, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::Command,
    time::timeout,
};
struct Decline;
#[async_trait::async_trait]
impl Approval for Decline {
    async fn decide(&self, _: Value) -> anyhow::Result<bool> {
        Ok(false)
    }
}
#[tokio::test]
async fn shared_worker_and_separate_renderer_frame() {
    let Some(evidence) = std::env::var_os("BROWSER_EVIDENCE") else {
        eprintln!("External qualification not provisioned");
        return;
    };
    let root = Path::new(&evidence).join(format!("workers-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let mut child = Command::new(std::env::var("BROWSER_NODE").unwrap_or("node".into()))
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/workers-oracle.cjs"
        ))
        .env("BROWSER_EVIDENCE", &root)
        .stdout(Stdio::piped())
        .stdin(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut reader = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    timeout(Duration::from_secs(45), reader.read_line(&mut line))
        .await
        .unwrap()
        .unwrap();
    let oracle: Value = serde_json::from_str(&line).unwrap();
    std::fs::write(
        root.join("oracle.json"),
        serde_json::to_vec_pretty(&oracle).unwrap(),
    )
    .unwrap();
    let config = Launch {
        executable: std::env::var("BROWSER_EXECUTABLE").unwrap().into(),
        profile_dir: root.join("profile"),
        downloads_dir: root.join("downloads"),
        allowed_domains: vec!["fixture.test".into(), "frame.test".into()],
        upload_files: vec![],
        secrets: BTreeMap::new(),
        proxy: oracle["proxyServer"].as_str().map(str::to_owned),
        proxy_credentials: None,
        synthetic_direct: true,
        synthetic_proxy_ca: None,
        args: oracle["args"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_owned())
            .collect(),
        timeout: Duration::from_millis(3000),
    };
    let mut engine = CdpEngine::launch(config).await.unwrap();
    engine
        .grant_fixture_loopback(oracle["base"].as_str().unwrap())
        .await
        .unwrap();
    engine
        .grant_fixture_loopback(oracle["frame"].as_str().unwrap())
        .await
        .unwrap();
    let navigation = engine
        .act(json!({"type":"navigate","url":oracle["base"]}), &Decline)
        .await
        .unwrap();
    let outcome=engine.inspect("Promise.race([window.complete,new Promise((_,reject)=>setTimeout(()=>reject(Error('Worker matrix incomplete')),9000))])").await;
    let outcome = outcome.unwrap_or_else(|e| json!({"error":e.to_string()}));
    let targets = engine.fixture_targets().await.unwrap();
    let denials = engine.proxy_denials().await;
    let error = engine.event_error().await;
    let trace = engine.policy_trace().await;
    let closed = engine.close().await;
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"stats\n")
        .await
        .unwrap();
    line.clear();
    timeout(Duration::from_secs(5), reader.read_line(&mut line))
        .await
        .unwrap()
        .unwrap();
    let stats: Value = serde_json::from_str(&line).unwrap();
    std::fs::write(root.join("workers.json"),serde_json::to_vec_pretty(&json!({"oracle":oracle,"native":{"navigation":navigation,"outcome":outcome,"targets":targets,"stats":stats,"denials":denials,"eventError":error,"trace":trace,"closed":closed}})).unwrap()).unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"close\n")
        .await
        .unwrap();
    assert!(timeout(Duration::from_secs(5), child.wait())
        .await
        .unwrap()
        .unwrap()
        .success());
    assert!(closed);
    assert!(error.is_none(), "Policy coverage lost");
    assert_eq!(navigation, oracle["navigation"]);
    assert_eq!(outcome, oracle["outcome"]);
    for inventory in [&targets, &oracle["targets"]] {
        for (kind, host) in [("iframe", "frame.test"), ("shared_worker", "fixture.test")] {
            assert!(
                inventory
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|v| v["type"] == kind && v["url"].as_str().unwrap_or("").contains(host)),
                "Positive {kind} renderer target required"
            );
        }
    }
    for counters in [&stats, &oracle["stats"]] {
        assert_eq!(
            counters["allowedHttp"], 2,
            "Both renderers must fetch successfully"
        );
        assert_eq!(
            counters["allowedSocket"], 2,
            "Both renderers must connect successfully"
        );
        assert_eq!(counters["deniedHttp"], 0);
        assert_eq!(counters["deniedSocket"], 0);
        assert_eq!(
            counters["workerScript"], 0,
            "First-inline registration must be blocked before fetching its script"
        );
    }
    assert_eq!(outcome["frame"]["registration"], "blocked");
    assert_eq!(
        outcome["frame"]["capability"],
        json!({"secure":true,"api":"object"})
    );
}
