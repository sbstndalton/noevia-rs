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
        tokio::time::sleep(Duration::from_millis(200)).await;
        Ok(true)
    }
}
fn normalized(v: &Value) -> Value {
    let mut v = v.clone();
    if let Some(e) = v.get_mut("evidence") {
        if e.get("screenshot").is_some() {
            e["screenshot"] = json!("PNG");
        }
    }
    // Driver diagnostics differ; failure class and page effects are compared separately.
    v.as_object_mut().unwrap().remove("reason");
    v
}
#[tokio::test]
#[ignore = "requires pinned external oracle and headless executable; see ADOPTION.md"]
async fn synthetic_differential() {
    let root = PathBuf::from(std::env::var("BROWSER_EVIDENCE").expect("evidence directory"));
    let mut child = Command::new(std::env::var("BROWSER_NODE").unwrap_or("node".into()))
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/oracle.cjs"))
        .stdout(Stdio::piped())
        .stdin(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut reader = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    timeout(Duration::from_secs(90), reader.read_line(&mut line))
        .await
        .unwrap()
        .unwrap();
    let oracle: Value = serde_json::from_str(&line).expect("oracle output");
    let config = Launch {
        executable: std::env::var("BROWSER_EXECUTABLE").unwrap().into(),
        profile_dir: root.join(format!("native-profile-{}", std::process::id())),
        downloads_dir: root.join("native-downloads"),
        allowed_domains: vec!["fixture.test".into()],
        upload_files: vec![root.join("upload.txt")],
        secrets: BTreeMap::from([(
            "fixture".into(),
            Secret {
                value: "synthetic-secret".into(),
                domains: vec!["fixture.test".into()],
            },
        )]),
        proxy: None,
        synthetic_direct: true,
        args: vec![
            "--host-resolver-rules=MAP *.test 127.0.0.1".into(),
            "--no-proxy-server".into(),
        ],
        timeout: Duration::from_millis(400),
    };
    std::fs::write(
        root.join("oracle.json"),
        serde_json::to_vec_pretty(&oracle).unwrap(),
    )
    .unwrap();
    let mut engine = match CdpEngine::launch(config).await {
        Ok(engine) => engine,
        Err(error) => {
            let rows: Vec<_> = oracle["results"]
                .as_array()
                .unwrap()
                .iter()
                .map(
                    |row| json!({"name":row["name"],"status":"blocked","reason":error.to_string()}),
                )
                .collect();
            std::fs::write(
                root.join("differential.json"),
                serde_json::to_vec_pretty(&json!({"gate":"blocked","actions":rows})).unwrap(),
            )
            .unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(b"close\n")
                .await
                .unwrap();
            timeout(Duration::from_secs(10), child.wait())
                .await
                .unwrap()
                .unwrap();
            panic!("Native launch blocked: {error}");
        }
    };
    let mut rows = vec![];
    let mut failures = vec![];
    for row in oracle["results"].as_array().unwrap() {
        let got = engine
            .act(row["action"].clone(), &Approve)
            .await
            .unwrap_or_else(|e| json!({"error":e.to_string()}));
        let mut downloads = vec![];
        if let Ok(mut entries) = tokio::fs::read_dir(root.join("native-downloads")).await {
            while let Some(e) = entries.next_entry().await.unwrap() {
                let name = e.file_name().to_string_lossy().to_string();
                if name.starts_with("1-") {
                    downloads.push(json!({"name":"fixture.txt","file":name,"bytes":tokio::fs::read_to_string(e.path()).await.unwrap()}));
                }
            }
        }
        let pass =
            normalized(&got) == normalized(&row["result"]) && json!(downloads) == row["downloads"];
        if !pass {
            failures.push(row["name"].clone());
        }
        rows.push(json!({"name":row["name"],"pass":pass,"native":got,"oracle":row["result"]}));
    }
    let closed = engine.close().await;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"close\n")
        .await
        .unwrap();
    timeout(Duration::from_secs(10), child.wait())
        .await
        .unwrap()
        .unwrap();
    std::fs::write(
        root.join("differential.json"),
        serde_json::to_vec_pretty(
            &json!({"actions":rows,"nativeClosed":closed,"oracleClosed":oracle["closed"]}),
        )
        .unwrap(),
    )
    .unwrap();
    assert!(closed);
    assert!(failures.is_empty(), "differential failures: {failures:?}");
}
