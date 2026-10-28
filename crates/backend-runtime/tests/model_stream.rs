use backend_runtime::{
    chat::{Model, OpenAiModel},
    transport::Endpoint,
    Control,
};
use serde_json::json;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
#[tokio::test]
async fn actual_provider_stream_emits_before_completion_and_closes_at_done(
) -> Result<(), Box<dyn std::error::Error>> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let (release, wait) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await?;
        let mut request = Vec::new();
        let mut byte = [0u8; 1];
        while !request.ends_with(b"\r\n\r\n") {
            socket.read_exact(&mut byte).await?;
            request.extend_from_slice(&byte);
            if request.len() > 8192 {
                return Err(std::io::Error::other("header bound"));
            }
        }
        let head = String::from_utf8_lossy(&request);
        let length = head
            .lines()
            .find_map(|l| {
                l.to_ascii_lowercase()
                    .strip_prefix("content-length: ")
                    .and_then(|v| v.parse::<usize>().ok())
            })
            .ok_or_else(|| std::io::Error::other("length"))?;
        let mut body = vec![0; length];
        socket.read_exact(&mut body).await?;
        let body: serde_json::Value = serde_json::from_slice(&body)?;
        assert_eq!(body.get("stream"), Some(&json!(true)));
        assert_eq!(body.get("temperature"), Some(&json!(0.2)));
        assert_eq!(body.get("top_k"), Some(&json!(12)));
        assert_eq!(body.get("model"), Some(&json!("synthetic")));
        socket.write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n").await?;
        let first="data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"early\"},\"finish_reason\":null}]}\n\n";
        socket
            .write_all(format!("{:x}\r\n{}\r\n", first.len(), first).as_bytes())
            .await?;
        socket.flush().await?;
        let _ = wait.await;
        let last="data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
        socket
            .write_all(format!("{:x}\r\n{}\r\n", last.len(), last).as_bytes())
            .await?;
        socket.flush().await?;
        // Deliberately leave HTTP chunk framing open. DONE must close the provider response.
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), socket.read(&mut byte)).await??,
            0
        );
        Ok::<(), std::io::Error>(())
    });
    let origin = format!("http://{address}");
    let model = OpenAiModel::new(
        Endpoint::new(
            &format!("{origin}/v1/chat/completions"),
            std::slice::from_ref(&origin),
            None,
        )?,
        "synthetic".into(),
    )?;
    let (events, mut output) = tokio::sync::mpsc::unbounded_channel();
    let model = tokio::spawn(async move {
        model
            .complete_sampled(
                &[json!({"role":"user","content":"test"})],
                &[],
                &json!({"temperature":0.2,"top_k":12,"model":"untrusted override"}),
                &Control::new(Duration::from_secs(5))?,
                &mut |v| {
                    events
                        .send(v)
                        .map_err(|_| backend_runtime::Fault::Cancelled)
                },
            )
            .await
    });
    let event = tokio::time::timeout(Duration::from_secs(2), output.recv())
        .await?
        .ok_or("event")?;
    assert_eq!(event, json!({"type":"delta","text":"early"}));
    assert!(!model.is_finished());
    release.send(()).map_err(|_| "release")?;
    let result = tokio::time::timeout(Duration::from_secs(2), model).await???;
    assert_eq!(result.get("content"), Some(&json!("early")));
    server.await?.map_err(|e| e.to_string())?;
    Ok(())
}
