//! CLOSED numeric-loopback controls. All fixture tasks are explicitly joined.
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::{BodyExt, Empty};
use noevia_server::{serve, App};
use std::{sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::oneshot,
    task::{JoinHandle, JoinSet},
};
pub struct Fixture {
    pub address: std::net::SocketAddr,
    stop: oneshot::Sender<()>,
    task: JoinHandle<()>,
}
impl Fixture {
    pub async fn upstream() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (stop, mut stopped) = oneshot::channel();
        let task = tokio::spawn(async move {
            let mut children = JoinSet::new();
            loop {
                tokio::select! {
                    _ = &mut stopped => break,
                    accepted = listener.accept() => {
                        let (mut stream, _) = accepted.unwrap();
                        children.spawn(async move {
                            let mut input = [0; 8192];
                            let n = stream.read(&mut input).await.unwrap();
                            let ready = input.get(..n).unwrap().starts_with(b"GET /api/ready ");
                            let (status, body) = if ready { ("200 OK", "{\"ready\":true,\"version\":\"fixture\"}") } else { ("418 I'm a teapot", "node-sentinel") };
                            let wire = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{body}", body.len());
                            stream.write_all(wire.as_bytes()).await.unwrap();
                        });
                    },
                    _ = children.join_next(), if !children.is_empty() => {}
                }
            }
            children.abort_all();
            while children.join_next().await.is_some() {}
        });
        Self {
            address,
            stop,
            task,
        }
    }
    pub async fn front(app: Arc<App>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (stop, stopped) = oneshot::channel();
        let task = tokio::spawn(serve::serve(
            listener,
            app,
            serve::Limits {
                keep_alive_idle: Duration::from_millis(50),
                max_connections: 16,
                ..Default::default()
            },
            async move {
                let _ = stopped.await;
            },
        ));
        Self {
            address,
            stop,
            task,
        }
    }
    pub async fn finish(self) {
        let _ = self.stop.send(());
        self.task.await.unwrap();
    }
}
pub async fn request(
    address: std::net::SocketAddr,
    method: &str,
    path: &str,
    cookie: Option<&str>,
) -> (StatusCode, axum::http::HeaderMap, String) {
    let headers: Vec<_> = cookie.into_iter().map(|v| ("cookie", v)).collect();
    request_headers(address, method, path, &headers).await
}
pub async fn request_headers(
    address: std::net::SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
) -> (StatusCode, axum::http::HeaderMap, String) {
    let stream = tokio::net::TcpStream::connect(address).await.unwrap();
    let (mut sender, connection) =
        hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(stream))
            .await
            .unwrap();
    let task = tokio::spawn(connection);
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header("host", address.to_string())
        .header("connection", "close");
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let result = sender
        .send_request(builder.body(Empty::<bytes::Bytes>::new()).unwrap())
        .await
        .unwrap();
    let status = result.status();
    let headers = result.headers().clone();
    let body = String::from_utf8(
        result
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();
    drop(sender);
    task.await.unwrap().unwrap();
    (status, headers, body)
}
pub async fn body(response: axum::http::Response<Body>) -> String {
    String::from_utf8(
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap()
}
