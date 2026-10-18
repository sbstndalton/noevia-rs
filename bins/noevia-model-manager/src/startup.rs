//! #2044: explicit child startup readiness, independent of its refused public health route.
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
pub(crate) const PATH: &str = "/api/v1/internal/startup-ready";
const BODY: &[u8] = br#"{"startup_ready":1}"#;
const WAIT: Duration = Duration::from_secs(10);

/// The child never respawns within one parent lifetime. Cache only the exact advertisement,
/// after actual startup; incompatible children and unknown successful bodies fail closed.
pub(crate) async fn ready(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    started: &AtomicBool,
) -> bool {
    if started.load(Ordering::Acquire) {
        return true;
    }
    let wait = async {
        loop {
            let mut request = client
                .get(format!("{base}{PATH}"))
                .timeout(Duration::from_secs(1));
            if !token.is_empty() {
                request = request.header("x-model-loader-token", token);
            }
            match request.send().await {
                Ok(mut answer) if answer.status() == reqwest::StatusCode::OK => {
                    let mut body = Vec::new();
                    loop {
                        match answer.chunk().await {
                            Ok(Some(chunk))
                                if body.len().saturating_add(chunk.len()) <= BODY.len() =>
                            {
                                body.extend_from_slice(&chunk)
                            }
                            Ok(None) => break,
                            _ => return false,
                        }
                    }
                    if body != BODY {
                        return false;
                    }
                    started.store(true, Ordering::Release);
                    return true;
                }
                Ok(answer) if answer.status() == reqwest::StatusCode::SERVICE_UNAVAILABLE => {}
                Ok(_) => return false,
                Err(_) => {}
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    };
    tokio::time::timeout(WAIT, wait).await.unwrap_or(false)
}

#[cfg(test)]
mod tests;
