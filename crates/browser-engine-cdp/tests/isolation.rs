#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
use browser_engine_cdp::{Approval, BrowserEngine, CdpEngine, Launch};
use chromiumoxide::Connection;
use chromiumoxide_types::{CdpJsonEventMessage, Message};
use futures::StreamExt;
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
// Test-only: address the owned browser, without expanding the production permission API.
async fn grant_geolocation(profile: &Path, origin: &str) -> Connection<CdpJsonEventMessage> {
    let active = tokio::fs::read_to_string(profile.join("DevToolsActivePort"))
        .await
        .unwrap();
    let mut lines = active.lines();
    let port: u16 = lines.next().unwrap().parse().unwrap();
    let endpoint = format!("ws://127.0.0.1:{port}{}", lines.next().unwrap());
    let mut connection = Connection::<CdpJsonEventMessage>::connect(&endpoint)
        .await
        .unwrap();
    let id = connection
        .submit_command(
            "Browser.grantPermissions".into(),
            None,
            json!({"origin":origin,"permissions":["geolocation","localNetworkAccess","localNetwork","loopbackNetwork"]}),
        )
        .unwrap();
    timeout(Duration::from_secs(5), async {
        while let Some(message) = connection.next().await {
            if let Message::Response(response) = message.unwrap() {
                if response.id == id {
                    assert!(response.error.is_none(), "Fixture permission rejected");
                    return;
                }
            }
        }
        panic!("Fixture permission connection closed")
    })
    .await
    .unwrap();
    connection
}
fn config(root: &Path, name: &str, oracle: &Value) -> Launch {
    Launch {
        executable: std::env::var("BROWSER_EXECUTABLE").unwrap().into(),
        profile_dir: root.join(format!("profile-{name}")),
        downloads_dir: root.join(format!("downloads-{name}")),
        allowed_domains: vec!["fixture.test".into()],
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
        timeout: Duration::from_millis(1500),
    }
}
#[tokio::test]
async fn storage_and_permission_isolation() {
    let Some(evidence) = std::env::var_os("BROWSER_EVIDENCE") else {
        eprintln!("External qualification not provisioned");
        return;
    };
    let root = Path::new(&evidence).join(format!("isolation-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let mut child = Command::new(std::env::var("BROWSER_NODE").unwrap_or("node".into()))
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/isolation-oracle.cjs"
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
        Duration::from_secs(45),
        BufReader::new(child.stdout.take().unwrap()).read_line(&mut line),
    )
    .await
    .unwrap()
    .unwrap();
    let oracle: Value = serde_json::from_str(&line).unwrap();
    std::fs::write(
        root.join("oracle.json"),
        serde_json::to_vec_pretty(&oracle).unwrap(),
    )
    .unwrap();
    let base = oracle["base"].as_str().unwrap();
    let first_config = config(&root, "first", &oracle);
    let first_profile = first_config.profile_dir.clone();
    let mut first = CdpEngine::launch(first_config).await.unwrap();
    let permissions = grant_geolocation(&first_profile, base).await;
    let navigation = first
        .act(json!({"type":"navigate","url":base}), &Decline)
        .await
        .unwrap();
    assert_eq!(navigation["status"], "done");
    assert_eq!(
        first
            .inspect(include_str!("storage-seed.js"))
            .await
            .unwrap(),
        true
    );
    let source = first
        .inspect(include_str!("storage-read.js"))
        .await
        .unwrap();
    let mut second = CdpEngine::launch(config(&root, "second", &oracle))
        .await
        .unwrap();
    second.grant_fixture_loopback(base).await.unwrap();
    let navigation = second
        .act(json!({"type":"navigate","url":base}), &Decline)
        .await
        .unwrap();
    assert_eq!(navigation["status"], "done");
    let fresh = second
        .inspect(include_str!("storage-read.js"))
        .await
        .unwrap();
    assert!(first.close().await);
    drop(permissions);
    let after_close = second
        .inspect(include_str!("storage-read.js"))
        .await
        .unwrap();
    assert!(second.close().await);
    let native = json!({"source":source,"fresh":fresh,"afterClose":after_close});
    std::fs::write(
        root.join("isolation.json"),
        serde_json::to_vec_pretty(&json!({"native":native,"oracle":oracle})).unwrap(),
    )
    .unwrap();
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
    for key in ["source", "fresh", "afterClose"] {
        assert_eq!(
            native[key], oracle[key],
            "Storage or permissions differ at {key}"
        );
    }
    // Positive source capabilities prevent an empty storage read from qualifying isolation.
    assert_eq!(source["cached"]["worker-marker"], "worker-one");
    assert_eq!(source["workers"].as_array().unwrap().len(), 1);
    assert_eq!(source["permission"], "granted");
    assert!(fresh["workers"].as_array().unwrap().is_empty());
    assert_eq!(fresh["permission"], "denied");
}
