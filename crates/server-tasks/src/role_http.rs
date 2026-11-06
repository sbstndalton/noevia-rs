//! Primitive model HTTP transport. Native role actors retain all pinning, retry, review
//! and correction decisions; remote diagnostics and headers never enter task state.
use crate::role_stream::{Body, Error, Response, Wait};
use js_json::{wtf16, JValue};
use tokio::sync::watch;
struct Incoming {
    response: reqwest::Response,
    cancel: watch::Receiver<bool>,
}
async fn stopped(cancel: &mut watch::Receiver<bool>) {
    if !*cancel.borrow() {
        let _ = cancel.changed().await;
    }
}
impl Body for Incoming {
    fn next(&mut self) -> Wait<'_, Result<Option<Vec<u8>>, Error>> {
        Box::pin(async move {
            tokio::select! {biased;_=stopped(&mut self.cancel)=>Err(Error::Aborted),next=self.response.chunk()=>next.map(|v|v.map(|v|v.to_vec())).map_err(|_|Error::Transport)}
        })
    }
}
/// The front chooses a local or on-this-network machine client using its existing routing
/// policy. This function follows no redirect and never copies an error response body/header.
pub async fn send(
    client: &reqwest::Client,
    url: &str,
    key: Option<&str>,
    payload: &JValue,
    mut cancel: watch::Receiver<bool>,
) -> Result<Response, Error> {
    if *cancel.borrow() {
        return Err(Error::Aborted);
    }
    let body = wtf16::stringify(payload).ok_or(Error::Invalid)?;
    let mut request = client
        .post(url)
        .header("content-type", "application/json")
        .body(body);
    if let Some(key) = key.filter(|k| !k.is_empty() && *k != "local") {
        let mut value = reqwest::header::HeaderValue::from_str(&format!("Bearer {key}"))
            .map_err(|_| Error::Transport)?;
        value.set_sensitive(true);
        request = request.header(reqwest::header::AUTHORIZATION, value);
    }
    let response = tokio::select! {biased;_=stopped(&mut cancel)=>return Err(Error::Aborted),response=request.send()=>response.map_err(|_|Error::Transport)?};
    let status = response.status().as_u16();
    // Errors still expose their status for the existing schema fallback rule, never their text.
    if !(200..300).contains(&status) {
        return Ok(Response { status, body: None });
    }
    Ok(Response {
        status,
        body: Some(Box::new(Incoming { response, cancel })),
    })
}
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };
    #[tokio::test]
    async fn streamed_body_and_reflected_errors_are_separate() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            for status in [200, 401] {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut input = [0; 4096];
                let _ = socket.read(&mut input).await.unwrap();
                let content = if status == 200 {
                    "data: synthetic\n\n"
                } else {
                    "reflected synthetic-token"
                };
                let head=format!("HTTP/1.1 {status} X\r\nx-reflected: synthetic-token\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",content.len());
                socket.write_all(head.as_bytes()).await.unwrap();
                socket.write_all(content.as_bytes()).await.unwrap();
            }
        });
        let client = model_stream::client().unwrap();
        let (_stop, cancel) = watch::channel(false);
        let payload = JValue::Obj(vec![]);
        let mut response = send(
            &client,
            &base,
            Some("synthetic-token"),
            &payload,
            cancel.clone(),
        )
        .await
        .unwrap();
        let mut text = vec![];
        let body = response.body.as_mut().unwrap();
        while let Some(chunk) = body.next().await.unwrap() {
            text.extend(chunk);
        }
        assert_eq!(text, b"data: synthetic\n\n");
        let response = send(&client, &base, Some("synthetic-token"), &payload, cancel)
            .await
            .unwrap();
        assert_eq!(response.status, 401);
        assert!(response.body.is_none());
        server.await.unwrap();
    }
    #[tokio::test]
    async fn cancelling_during_headers_drops_the_actual_request() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (entered, wait) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut input = [0; 4096];
            let _ = socket.read(&mut input).await.unwrap();
            entered.send(()).unwrap();
            tokio::time::timeout(Duration::from_secs(2), socket.read(&mut input))
                .await
                .unwrap()
                .unwrap()
        });
        let (stop, cancel) = watch::channel(false);
        let client = model_stream::client().unwrap();
        let request =
            tokio::spawn(async move { send(&client, &base, None, &JValue::Null, cancel).await });
        wait.await.unwrap();
        stop.send_replace(true);
        assert!(matches!(request.await.unwrap(), Err(Error::Aborted)));
        assert_eq!(server.await.unwrap(), 0);
    }
}
