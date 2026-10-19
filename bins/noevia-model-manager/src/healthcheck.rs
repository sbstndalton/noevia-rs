use std::time::Duration;

const HEALTHCHECK_URL: &str = "http://127.0.0.1:8090/api/v1/health";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(3);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(1);

pub fn is_healthcheck_command(args: impl IntoIterator<Item = String>) -> bool {
    let mut args = args.into_iter();
    matches!((args.next(), args.next()), (Some(flag), None) if flag == "--healthcheck")
}

pub async fn check() -> Result<(), &'static str> {
    let client = client(REQUEST_TIMEOUT)?;
    check_url(&client, HEALTHCHECK_URL).await
}

fn client(timeout: Duration) -> Result<reqwest::Client, &'static str> {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(timeout.min(CONNECT_TIMEOUT))
        .timeout(timeout)
        .build()
        .map_err(|_| "could not create health check client")
}

async fn check_url(client: &reqwest::Client, url: &str) -> Result<(), &'static str> {
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|_| "health check request failed")?;
    if response.status() == reqwest::StatusCode::OK {
        Ok(())
    } else {
        Err("health check returned a non-200 status")
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        task::JoinHandle,
    };

    async fn response_server(response: &'static [u8]) -> (String, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral test listener");
        let address = listener.local_addr().expect("read test listener address");
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept test request");
            let mut request = [0_u8; 1024];
            let _ = socket.read(&mut request).await;
            socket
                .write_all(response)
                .await
                .expect("write test response");
        });
        (format!("http://{address}/api/v1/health"), task)
    }

    #[test]
    fn healthcheck_is_a_standalone_cli_command() {
        assert!(is_healthcheck_command(["--healthcheck".to_owned()]));
        assert!(!is_healthcheck_command(Vec::<String>::new()));
        assert!(!is_healthcheck_command([
            "--healthcheck".to_owned(),
            "--features".to_owned()
        ]));
    }

    #[tokio::test]
    async fn probes_the_public_health_route_without_the_child_token() {
        use std::sync::Arc;

        assert_eq!(HEALTHCHECK_URL, "http://127.0.0.1:8090/api/v1/health");
        let mut state = crate::test_support::test_state("http://127.0.0.1:9");
        state.token = "required-synthetic-token".to_owned();
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral router listener");
        let address = listener.local_addr().expect("read router listener address");
        let server =
            tokio::spawn(
                async move { axum::serve(listener, crate::router(Arc::new(state))).await },
            );
        let client = client(REQUEST_TIMEOUT).unwrap();
        let route = format!("http://{address}/api/v1/health");
        assert_eq!(check_url(&client, &route).await, Ok(()));

        let wrong_route = client
            .get(format!("http://{address}/health"))
            .send()
            .await
            .expect("request legacy health path");
        assert_eq!(wrong_route.status(), reqwest::StatusCode::UNAUTHORIZED);
        server.abort();
    }

    #[tokio::test]
    async fn accepts_only_http_200() {
        let (url, server) = response_server(
            b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\nConnection: close\r\n\r\nsecret body",
        )
        .await;
        assert_eq!(
            check_url(&client(REQUEST_TIMEOUT).unwrap(), &url).await,
            Ok(())
        );
        server.await.expect("test server finished");

        let (url, server) = response_server(
            b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert_eq!(
            check_url(&client(REQUEST_TIMEOUT).unwrap(), &url).await,
            Err("health check returned a non-200 status")
        );
        server.await.expect("test server finished");
    }

    #[tokio::test]
    async fn rejects_redirect_without_following_it() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral test listener");
        let address = listener.local_addr().expect("read test listener address");
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept first request");
            let mut request = [0_u8; 1024];
            let _ = socket.read(&mut request).await;
            socket
                .write_all(b"HTTP/1.1 302 Found\r\nLocation: /second\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .await
                .expect("write redirect");
            drop(socket);
            tokio::time::timeout(Duration::from_millis(150), listener.accept())
                .await
                .is_err()
        });
        assert_eq!(
            check_url(
                &client(REQUEST_TIMEOUT).unwrap(),
                &format!("http://{address}/api/v1/health")
            )
            .await,
            Err("health check returned a non-200 status")
        );
        assert!(task.await.expect("redirect server finished"));
    }

    #[tokio::test]
    async fn bounds_a_server_that_never_sends_response_headers() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral test listener");
        let address = listener.local_addr().expect("read test listener address");
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept hanging request");
            let mut request = [0_u8; 1024];
            let _ = socket.read(&mut request).await;
            tokio::time::sleep(Duration::from_secs(1)).await;
        });
        let timeout = Duration::from_millis(150);
        let result = tokio::time::timeout(
            Duration::from_millis(300),
            check_url(
                &client(timeout).unwrap(),
                &format!("http://{address}/api/v1/health"),
            ),
        )
        .await;
        assert!(matches!(result, Ok(Err("health check request failed"))));
        task.abort();
    }
}
