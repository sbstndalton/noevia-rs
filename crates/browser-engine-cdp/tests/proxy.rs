#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
use browser_engine_cdp::{Approval, BrowserEngine, CdpEngine, Launch, ProxyCredentials};
use serde_json::{json, Value};
use std::{collections::BTreeMap, path::PathBuf, process::Stdio, time::Duration};
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
async fn tls_and_socks_negatives() {
    if std::env::var_os("BROWSER_EVIDENCE").is_none() {
        eprintln!("External qualification not provisioned");
        return;
    }
    let root = PathBuf::from(std::env::var("BROWSER_EVIDENCE").unwrap())
        .join(format!("proxy-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let mut child = Command::new(std::env::var("BROWSER_NODE").unwrap_or("node".into()))
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/proxy-oracle.cjs"
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
    timeout(Duration::from_secs(90), reader.read_line(&mut line))
        .await
        .unwrap()
        .unwrap();
    let oracle: Value = serde_json::from_str(&line).unwrap();
    std::fs::write(
        root.join("oracle.json"),
        serde_json::to_vec_pretty(&oracle).unwrap(),
    )
    .unwrap();
    let mut results = Vec::new();
    let mut failures = Vec::new();
    for row in oracle["rows"].as_array().unwrap() {
        let name = row["name"].as_str().unwrap();
        let config = Launch {
            executable: std::env::var("BROWSER_EXECUTABLE").unwrap().into(),
            profile_dir: root.join(format!("profile-{name}")),
            downloads_dir: root.join(format!("downloads-{name}")),
            allowed_domains: vec!["fixture.test".into()],
            upload_files: vec![],
            secrets: BTreeMap::new(),
            proxy: row["server"].as_str().map(str::to_owned),
            proxy_credentials: row["username"].as_str().map(|user| ProxyCredentials {
                username: user.into(),
                password: row["password"].as_str().unwrap().into(),
            }),
            synthetic_direct: true,
            synthetic_proxy_ca: row["proxyCa"].as_str().map(PathBuf::from),
            args: row["args"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap().to_owned())
                .collect(),
            timeout: Duration::from_millis(1500),
        };
        let (native, expected) = match CdpEngine::launch(config).await {
            Err(error) => (
                json!({"error":error.to_string()}),
                json!({"error":row["error"]}),
            ),
            Ok(mut engine) => {
                let result = engine
                    .act(row["action"].clone(), &Decline)
                    .await
                    .unwrap_or_else(|e| json!({"error":e.to_string()}));
                let result = if result["status"] == "done" && row.get("path").is_none() {
                    json!({"navigation":result,"extract":engine.act(json!({"type":"extract","selector":"#text"}),&Decline).await.unwrap_or_else(|e|json!({"error":e.to_string()}))})
                } else {
                    result
                };
                let closed = engine.close().await;
                assert!(closed);
                (
                    result,
                    if row.get("error").is_some() {
                        json!({"error":row["error"]})
                    } else {
                        row["result"].clone()
                    },
                )
            }
        };
        assert!(
            !native.to_string().contains("synthetic-alpha")
                && !native.to_string().contains("synthetic-wrong"),
            "Proxy secret appeared in public output"
        );
        let pass = native == expected;
        if !pass {
            failures.push(name.to_owned());
        }
        results.push(json!({"name":name,"pass":pass,"native":native,"oracle":expected}));
        std::fs::write(
            root.join("proxy.json"),
            serde_json::to_vec_pretty(&json!({"rows":results})).unwrap(),
        )
        .unwrap();
    }
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"stats\n")
        .await
        .unwrap();
    let mut stats = String::new();
    timeout(Duration::from_secs(3), reader.read_line(&mut stats))
        .await
        .unwrap()
        .unwrap();
    let stats: Value = serde_json::from_str(&stats).unwrap();
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
        root.join("proxy.json"),
        serde_json::to_vec_pretty(&json!({"rows":results,"stats":stats})).unwrap(),
    )
    .unwrap();
    assert_eq!(stats["originCredentials"], 0);
    assert!(
        failures.is_empty(),
        "Proxy differential failures: {failures:?}"
    );
}
