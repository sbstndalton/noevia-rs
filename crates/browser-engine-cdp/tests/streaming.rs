#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
use browser_engine_cdp::{Approval, BrowserEngine, CdpEngine, Launch, Secret};
use serde_json::{json, Value};
use std::{collections::BTreeMap, path::PathBuf, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::Command,
    time::timeout,
};
struct Approve;
#[async_trait::async_trait]
impl Approval for Approve {
    async fn decide(&self, _: Value) -> anyhow::Result<bool> {
        Ok(true)
    }
}
#[tokio::test]
#[ignore = "requires pinned external oracle and synthetic Chromium"]
async fn streaming_differential() {
    let root = PathBuf::from(std::env::var("BROWSER_EVIDENCE").unwrap())
        .join(format!("stream-{}", std::process::id()));
    std::fs::create_dir(&root).unwrap();
    let mut child = Command::new("node")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/stream-oracle.cjs"
        ))
        .env("BROWSER_EVIDENCE", &root)
        .stdout(Stdio::piped())
        .stdin(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut line = String::new();
    timeout(
        Duration::from_secs(30),
        BufReader::new(child.stdout.take().unwrap()).read_line(&mut line),
    )
    .await
    .unwrap()
    .unwrap();
    let oracle: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(oracle["rows"][0]["result"]["evidence"]["text"], "streamed");
    assert_eq!(oracle["rows"][1]["result"]["evidence"]["text"], "large");
    let mut engine = CdpEngine::launch(Launch {
        executable: std::env::var("BROWSER_EXECUTABLE").unwrap().into(),
        profile_dir: root.join("profile"),
        downloads_dir: root.join("downloads"),
        allowed_domains: vec!["fixture.test".into()],
        upload_files: vec![],
        secrets: BTreeMap::from([(
            "fixture".into(),
            Secret {
                value: "synthetic-secret".into(),
                domains: vec!["fixture.test".into()],
            },
        )]),
        proxy: None,
        proxy_credentials: None,
        synthetic_direct: true,
        args: vec![
            "--host-resolver-rules=MAP *.test 127.0.0.1".into(),
            "--no-proxy-server".into(),
            format!(
                "--unsafely-treat-insecure-origin-as-secure={}",
                oracle["base"].as_str().unwrap()
            ),
        ],
        timeout: Duration::from_secs(4),
    })
    .await
    .unwrap();
    engine
        .grant_fixture_loopback(oracle["base"].as_str().unwrap())
        .await
        .unwrap();
    let trace_probe = oracle["base"]
        .as_str()
        .unwrap()
        .replace("fixture.test", "synthetic-secret.fixture.test");
    engine
        .act(
            json!({"type":"navigate","url":format!("{trace_probe}/trace")}),
            &Approve,
        )
        .await
        .unwrap();
    let mut rows = vec![];
    for row in oracle["rows"].as_array().unwrap() {
        let nav=engine.act(json!({"type":"navigate","url":format!("{}/{}",oracle["base"].as_str().unwrap(),row["endpoint"].as_str().unwrap())}),&Approve).await;
        engine
            .act(json!({"type":"wait","ms":200}), &Approve)
            .await
            .unwrap();
        let got = engine
            .act(json!({"type":"extract","selector":"#result"}), &Approve)
            .await
            .unwrap_or_else(|_| json!({"status":"error"}));
        rows.push(json!({"endpoint":row["endpoint"],"navigation":nav.ok(),"native":got,"oracle":row["result"],"pass":got==row["result"]}));
    }
    let trace_redacted = !serde_json::to_string(&engine.policy_trace().await)
        .unwrap()
        .contains("synthetic-secret");
    let closed = engine.close().await;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"close\n")
        .await
        .unwrap();
    timeout(Duration::from_secs(5), child.wait())
        .await
        .unwrap()
        .unwrap();
    std::fs::write(
        root.join("streaming.json"),
        serde_json::to_vec_pretty(
            &json!({"rows":rows,"closed":closed,"traceRedacted":trace_redacted}),
        )
        .unwrap(),
    )
    .unwrap();
    assert!(closed);
    assert!(trace_redacted, "Diagnostic trace exposed a declared secret");
    assert!(
        rows.iter().all(|r| r["pass"] == true),
        "Streaming differential: {rows:?}"
    );
}
