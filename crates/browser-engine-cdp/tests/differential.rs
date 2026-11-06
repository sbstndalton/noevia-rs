#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
use browser_engine_cdp::{Approval, BrowserEngine, CdpEngine, Launch, ProxyCredentials, Secret};
use serde_json::{json, Value};
use std::{collections::BTreeMap, path::PathBuf, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::Command,
    time::timeout,
};
struct PendingApproval {
    entered: tokio::sync::Notify,
}
#[async_trait::async_trait]
impl Approval for PendingApproval {
    async fn decide(&self, _: Value) -> anyhow::Result<bool> {
        self.entered.notify_one();
        tokio::time::sleep(Duration::from_secs(300)).await;
        Ok(false)
    }
}
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
    v.as_object_mut().unwrap().remove("downloads");
    v
}
#[tokio::test]
#[ignore = "requires pinned external oracle and headless executable; see ADOPTION.md"]
async fn synthetic_differential() {
    let base_root = PathBuf::from(std::env::var("BROWSER_EVIDENCE").expect("evidence directory"));
    let root = base_root.join(format!("run-{}", std::process::id()));
    std::fs::create_dir(&root).unwrap();
    let mut child = Command::new(std::env::var("BROWSER_NODE").unwrap_or("node".into()))
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/oracle.cjs"))
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
    let oracle: Value = serde_json::from_str(&line).expect("oracle output");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"reset\n")
        .await
        .unwrap();
    let mut reset_ack = String::new();
    timeout(Duration::from_secs(3), reader.read_line(&mut reset_ack))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reset_ack, "reset\n");
    let config = Launch {
        executable: std::env::var("BROWSER_EXECUTABLE").unwrap().into(),
        profile_dir: root.join(format!("native-profile-{}", std::process::id())),
        downloads_dir: root.join(format!("native-downloads-{}", std::process::id())),
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
        timeout: Duration::from_millis(1200),
    };
    let native_downloads = config.downloads_dir.clone();
    std::fs::write(
        root.join("oracle.json"),
        serde_json::to_vec_pretty(&oracle).unwrap(),
    )
    .unwrap();
    let mut engine = match CdpEngine::launch(config.clone()).await {
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
    engine
        .grant_fixture_loopback(oracle["base"].as_str().unwrap())
        .await
        .unwrap();
    let args = engine.launch_arguments().await.unwrap();
    let actual = args["arguments"].as_array().unwrap();
    for flag in oracle["launchFlags"].as_array().unwrap() {
        assert!(actual.contains(flag), "Missing launch flag: {flag}");
    }
    assert!(actual.iter().any(|v| v
        .as_str()
        .unwrap_or("")
        .starts_with("--remote-debugging-port=")));
    assert!(!actual.contains(&json!("--ignore-certificate-errors")));
    let mut rows = vec![];
    let mut failures = vec![];
    for row in oracle["results"].as_array().unwrap() {
        let started = tokio::time::Instant::now();
        let _ = engine.inspect("globalThis.cspViolations=[];addEventListener('securitypolicyviolation',e=>globalThis.cspViolations.push({uri:e.blockedURI,policy:e.originalPolicy}))").await;
        let got = engine
            .act(row["action"].clone(), &Approve)
            .await
            .unwrap_or_else(|e| json!({"error":e.to_string()}));
        let elapsed_ms = started.elapsed().as_millis();
        tokio::time::sleep(Duration::from_millis(40)).await;
        let mut downloads = vec![];
        if let Ok(mut entries) = tokio::fs::read_dir(&native_downloads).await {
            while let Some(e) = entries.next_entry().await.unwrap() {
                let name = e.file_name().to_string_lossy().to_string();
                if name.starts_with("1-") {
                    downloads.push(json!({"name":"fixture.txt","file":name,"bytes":tokio::fs::read_to_string(e.path()).await.unwrap()}));
                }
            }
        }
        let effects = engine
            .inspect(include_str!("effects.js"))
            .await
            .unwrap_or(Value::Null);
        let mut expected = normalized(&row["result"]);
        let security_upgrade = row["name"] == "subnetwork-outcome";
        if security_upgrade {
            let text = expected
                .pointer("/evidence/text")
                .and_then(Value::as_str)
                .unwrap_or("")
                .replace("worker:blocked,allowed", "worker:blocked,blocked");
            expected["evidence"]["text"] = json!(text);
        }
        let pass = normalized(&got) == expected
            && (row["downloadBarrier"] != json!(true) || json!(downloads) == row["downloads"])
            && effects == row["effects"]
            && (row["name"] != "wait-cap" || (4900..8000).contains(&elapsed_ms));
        if !pass {
            failures.push(row["name"].clone());
        }
        rows.push(json!({"name":row["name"],"pass":pass,"native":got,"oracle":row["result"],"elapsedMs":elapsed_ms,"oracleElapsedMs":row["elapsedMs"],"effects":effects,"oracleEffects":row["effects"],"pageUrl":timeout(Duration::from_secs(1),engine.inspect("location.href")).await.ok().and_then(Result::ok),"cspViolations":engine.inspect("globalThis.cspViolations || []").await.ok(),"eventError":engine.event_error().await,"policyTrace":engine.policy_trace().await}));
        std::fs::write(
            root.join("differential.json"),
            serde_json::to_vec_pretty(&json!({"actions":rows,"isolation":"not yet run"})).unwrap(),
        )
        .unwrap();
    }
    let denied_popup = engine
        .act(json!({"type":"click","selector":"#denied-popup"}), &Approve)
        .await
        .unwrap_or_else(|e| json!({"error":e.to_string()}));
    engine
        .act(
            json!({"type":"navigate","url":format!("{}/page",oracle["base"].as_str().unwrap())}),
            &Approve,
        )
        .await
        .unwrap();
    let denied_requests = engine
        .inspect(&format!(
            "fetch({}).then(r=>r.json())",
            json!(format!("{}/stats", oracle["base"].as_str().unwrap()))
        ))
        .await
        .unwrap();
    let network_enforcement_pass =
        denied_requests == json!({"resource":0,"socket":0,"popup":0,"effect":0});
    engine
        .inspect("document.cookie='marker=one';localStorage.setItem('marker','one')")
        .await
        .unwrap();
    let storage = "({cookie:document.cookie,local:localStorage.getItem('marker')})";
    let source = engine.inspect(storage).await.unwrap();
    let mut second_config = config.clone();
    second_config.profile_dir = root.join(format!("second-profile-{}", std::process::id()));
    second_config.downloads_dir = root.join(format!("second-downloads-{}", std::process::id()));
    let mut second = CdpEngine::launch(second_config).await.unwrap();
    second
        .grant_fixture_loopback(oracle["base"].as_str().unwrap())
        .await
        .unwrap();
    second
        .act(
            json!({"type":"navigate","url":format!("{}/page",oracle["base"].as_str().unwrap())}),
            &Approve,
        )
        .await
        .unwrap();
    let fresh = second.inspect(storage).await.unwrap();
    let isolation = json!({"source":source,"fresh":fresh});
    let isolation_pass = isolation == oracle["isolation"];
    let second_closed = second.close().await;
    engine.inspect("document.querySelector('#replace').onclick=()=>fetch('/effect');document.querySelector('#text').textContent='unsafe'").await.unwrap();
    let pending = PendingApproval {
        entered: tokio::sync::Notify::new(),
    };
    {
        let action = engine.act(json!({"type":"click","selector":"#replace"}), &pending);
        tokio::pin!(action);
        timeout(Duration::from_secs(3),async {tokio::select!{_=pending.entered.notified()=>{},result=&mut action=>panic!("Approval unexpectedly completed: {result:?}")}}).await.unwrap();
    }
    let cancelled = engine
        .act(json!({"type":"wait","ms":1}), &Approve)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let automatically_closed = engine.inspect("location.href").await.is_err();
    let cancellation_pass = cancelled["status"] == "blocked" && automatically_closed;
    let closed = engine.close().await;
    let mut proxy_results = Vec::new();
    let mut proxy_closed = true;
    for user in ["alpha", "beta"] {
        let mut proxy_config = config.clone();
        proxy_config.profile_dir = root.join(format!("proxy-profile-{user}"));
        proxy_config.downloads_dir = root.join(format!("proxy-downloads-{user}"));
        proxy_config.args.retain(|a| a != "--no-proxy-server");
        proxy_config.proxy = oracle["proxyServer"].as_str().map(str::to_owned);
        proxy_config.proxy_credentials = Some(ProxyCredentials {
            username: user.into(),
            password: format!("synthetic-{user}"),
        });
        let mut proxy = CdpEngine::launch(proxy_config).await.unwrap();
        proxy
            .grant_fixture_loopback(oracle["base"].as_str().unwrap())
            .await
            .unwrap();
        proxy.act(json!({"type":"navigate","url":format!("{}/proxy-redirect",oracle["base"].as_str().unwrap())}),&Approve).await.unwrap();
        proxy_results.push(
            proxy
                .act(json!({"type":"extract","selector":"#identity"}), &Approve)
                .await
                .unwrap(),
        );
        let flags = proxy.launch_arguments().await.unwrap().to_string();
        assert!(!flags.contains("synthetic-alpha") && !flags.contains("synthetic-beta"));
        proxy_closed &= proxy.close().await;
    }
    let proxy_pass = proxy_closed
        && proxy_results.iter().map(normalized).collect::<Vec<_>>()
            == oracle["proxyResults"]
                .as_array()
                .unwrap()
                .iter()
                .map(normalized)
                .collect::<Vec<_>>();
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
            &json!({"actions":rows,"nativeClosed":closed,"launchFlagsPass":true,"proxyPass":proxy_pass,"proxyResults":proxy_results,"cancellationPass":cancellation_pass,"deniedRequests":denied_requests,"deniedPopup":denied_popup,"networkEnforcementPass":network_enforcement_pass,"oracleClosed":oracle["closed"],"isolation":isolation,"isolationPass":isolation_pass,"secondClosed":second_closed}),
        )
        .unwrap(),
    )
    .unwrap();
    assert!(closed && second_closed);
    assert!(cancellation_pass);
    assert!(proxy_pass, "Proxy authentication/isolation mismatch");
    if !network_enforcement_pass {
        failures.push(json!("denied-request-server-evidence"));
    }
    assert!(isolation_pass, "Isolation mismatch: {isolation}");
    assert!(failures.is_empty(), "differential failures: {failures:?}");
}
