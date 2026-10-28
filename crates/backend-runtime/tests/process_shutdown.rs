#![cfg(target_os = "linux")]
#[path = "../../server-auth/tests/common/mod.rs"]
mod auth_fixture;
use serde_json::Value;
use std::{
    process::{Child, Command, Stdio},
    time::Duration,
};
struct OwnedChild(Child);
impl Drop for OwnedChild {
    fn drop(&mut self) {
        if !matches!(self.0.try_wait(), Ok(Some(_))) {
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}
#[tokio::test]
async fn original_front_binary_drains_on_sigterm_without_legacy_process(
) -> Result<(), Box<dyn std::error::Error>> {
    let fixture: Value = serde_json::from_str(include_str!(
        "../../server-auth/tests/fixtures/node-auth.v1.json"
    ))?;
    let scenario = fixture
        .get("scenarios")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .ok_or("scenario")?;
    let built = auth_fixture::build(scenario);
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0")?;
        l.local_addr()?.port()
    };
    let mut child = OwnedChild(
        Command::new(env!("CARGO_BIN_EXE_native-backend-candidate"))
            .arg("--existing-front")
            .arg(built.dir.path().join("owned-journal.db"))
            .env_clear()
            .env("NOEVIA_LEGACY_UPSTREAM", "http://127.0.0.1:9")
            .env("UI_DATA_DIR", built.dir.path())
            .env("UI_PORT", port.to_string())
            .env("NOEVIA_START_ID", "0123456789abcdef0123456789abcdef")
            .env("NOEVIA_WEB_DIST", built.dir.path().join("dist"))
            .env("PUBLIC_ORIGIN", "https://noevia.example.test")
            .env(
                "NATIVE_CANDIDATE_MODEL_ENDPOINT",
                "http://127.0.0.1:9/v1/chat/completions",
            )
            .env("INFERENCE_MODEL", "synthetic-no-calls")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?,
    );
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(1))
        .build()?;
    let ready = async {
        loop {
            if child.0.try_wait()?.is_some() {
                return Err("candidate exited before listening".into());
            }
            if client
                .get(format!("http://127.0.0.1:{port}/healthz"))
                .send()
                .await
                .is_ok()
            {
                return Ok::<_, Box<dyn std::error::Error>>(());
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(10), ready).await??;
    let signal = Command::new("/bin/kill")
        .arg("-TERM")
        .arg(child.0.id().to_string())
        .status()?;
    assert!(signal.success());
    let stopped = async {
        loop {
            if let Some(status) = child.0.try_wait()? {
                return Ok::<_, std::io::Error>(status);
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    };
    let status = tokio::time::timeout(Duration::from_secs(8), stopped).await??;
    assert!(
        status.success(),
        "candidate must handle TERM and exit normally: {status}"
    );
    assert!(tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .is_err());
    Ok(())
}

#[tokio::test]
async fn candidate_reexec_enters_hardened_storage_child_before_app_startup(
) -> Result<(), Box<dyn std::error::Error>> {
    let mut child = OwnedChild(
        Command::new(env!("CARGO_BIN_EXE_native-backend-candidate"))
            .arg(server_storage_reads::transport::CHILD_ARG)
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?,
    );
    let finished = async {
        loop {
            if let Some(status) = child.0.try_wait()? {
                return Ok::<_, std::io::Error>(status);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    let status = tokio::time::timeout(Duration::from_secs(5), finished).await??;
    // EOF is not a request. The real child publishes READY after installing its
    // Linux limits, then refuses the absent frame without making any network call.
    assert!(!status.success());
    use std::io::Read;
    let stdout = child.0.stdout.take().ok_or("stdout")?;
    let mut bytes = Vec::new();
    stdout.take(17).read_to_end(&mut bytes)?;
    assert_eq!(bytes, server_storage_reads::isolated::READY);
    Ok(())
}
