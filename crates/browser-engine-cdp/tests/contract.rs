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
struct Decline(std::sync::Arc<std::sync::Mutex<Vec<Value>>>);
#[async_trait::async_trait]
impl Approval for Decline {
    async fn decide(&self, card: Value) -> anyhow::Result<bool> {
        let approve = card.pointer("/element/text").and_then(Value::as_str)
            == Some("Delete after navigation");
        self.0.lock().unwrap().push(card);
        if approve {
            tokio::time::sleep(Duration::from_millis(700)).await;
        }
        Ok(approve)
    }
}
fn normalize(v: &Value) -> Value {
    let mut v = v.clone();
    if v.get("screenshot").is_some() {
        v["screenshot"] = json!("PNG");
    }
    if let Some(e) = v.get_mut("evidence") {
        if e.get("screenshot").is_some() {
            e["screenshot"] = json!("PNG");
        }
    }
    v
}
#[tokio::test]
async fn exact_contract() {
    if std::env::var_os("BROWSER_EVIDENCE").is_none() {
        eprintln!("External qualification not provisioned");
        return;
    }
    let root = PathBuf::from(std::env::var("BROWSER_EVIDENCE").unwrap())
        .join(format!("contract-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let mut child = Command::new(std::env::var("BROWSER_NODE").unwrap_or("node".into()))
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/contract-oracle.cjs"
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
    let config = Launch {
        executable: std::env::var("BROWSER_EXECUTABLE").unwrap().into(),
        profile_dir: root.join("profile"),
        downloads_dir: root.join("downloads"),
        allowed_domains: if std::env::var("BROWSER_CONTRACT_MODE").is_ok() {
            vec!["fixture.test".into(), "frame.test".into()]
        } else {
            vec!["fixture.test".into()]
        },
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
                "--unsafely-treat-insecure-origin-as-secure={},{}",
                oracle["base"].as_str().unwrap(),
                oracle["base"]
                    .as_str()
                    .unwrap()
                    .replace("fixture.test", "frame.test")
            ),
        ],
        timeout: Duration::from_millis(1200),
    };
    let audit = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
    let captured = audit.clone();
    let mut engine = CdpEngine::launch_with_log(
        config,
        "contract".into(),
        std::sync::Arc::new(move |mut e| {
            e.as_object_mut().unwrap().remove("session");
            captured.lock().unwrap().push(e)
        }),
    )
    .await
    .unwrap();
    engine
        .grant_fixture_loopback(oracle["base"].as_str().unwrap())
        .await
        .unwrap();
    let cards = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let decline = Decline(cards.clone());
    let mut rows = vec![];
    let mut failures = vec![];
    for row in oracle["results"].as_array().unwrap() {
        let native = engine
            .act(row["action"].clone(), &decline)
            .await
            .unwrap_or_else(|e| json!({"error":e.to_string()}));
        let pass = normalize(&native) == normalize(&row["result"]);
        if !pass {
            failures.push(row["name"].clone());
        }
        rows.push(json!({"name":row["name"],"pass":pass,"native":native,"oracle":row["result"]}));
        std::fs::write(root.join("progress.json"),serde_json::to_vec_pretty(&json!({"rows":rows,"eventError":engine.event_error().await,"trace":engine.policy_trace().await})).unwrap()).unwrap();
    }
    let targets = engine.fixture_targets().await.unwrap();
    let state = engine.state().await;
    assert!(engine.close().await);
    assert!(engine.close().await);
    let closed = engine
        .act(json!({"type":"wait","ms":1}), &decline)
        .await
        .unwrap();
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
    std::fs::write(root.join("contract.json"),serde_json::to_vec_pretty(&json!({"rows":rows,"targets":targets,"oracleTargets":oracle["targets"],"nativeCards":*cards.lock().unwrap(),"oracleCards":oracle["cards"],"nativeAudit":*audit.lock().unwrap(),"oracleAudit":oracle["audit"],"state":state,"oracleState":oracle["state"],"closed":closed,"oracleClosed":oracle["closed"]})).unwrap()).unwrap();
    assert_eq!(
        cards
            .lock()
            .unwrap()
            .iter()
            .map(normalize)
            .collect::<Vec<_>>(),
        oracle["cards"]
            .as_array()
            .unwrap()
            .iter()
            .map(normalize)
            .collect::<Vec<_>>()
    );
    if std::env::var("BROWSER_CONTRACT_MODE").as_deref() == Ok("locator") {
        for inventory in [&targets, &oracle["targets"]] {
            assert!(
                inventory
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|v| v["type"] == "iframe"
                        && v["url"].as_str().unwrap_or("").contains("frame.test")),
                "Cross-origin iframe must have its own renderer target"
            );
        }
    }
    assert_eq!(state, oracle["state"]);
    let mut oracle_audit = oracle["audit"].clone();
    // Owner-approved exception: Node's raw action type can disclose a declared secret.
    for entry in oracle_audit.as_array_mut().unwrap() {
        if let Some(kind) = entry.get_mut("type") {
            if let Some(text) = kind.as_str() {
                *kind = json!(text.replace("synthetic-secret", "{{secret:fixture}}"));
            }
        }
    }
    let native_audit = json!(audit.lock().unwrap().clone());
    assert!(!native_audit.to_string().contains("synthetic-secret"));
    assert_eq!(native_audit, oracle_audit);
    assert_eq!(closed, oracle["closed"]);
    assert!(failures.is_empty(), "Exact errors differ: {failures:?}");
}
