#![allow(clippy::unwrap_used, clippy::expect_used)]

use code_sandbox_supervisor::{serve, Config};
use std::path::PathBuf;
use std::time::Duration;
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;

struct Harness {
    _tmp: TempDir,
    work: PathBuf,
    port: u16,
    stop: watch::Sender<bool>,
    server: tokio::task::JoinHandle<std::io::Result<()>>,
}
impl Harness {
    async fn new(script: &str, cap: usize, wall: Duration) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("workspaces");
        let work = root.join("task");
        std::fs::create_dir_all(&work).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let config = Config {
            command: "/bin/sh".into(),
            args: vec!["-c".into(), script.into()],
            root,
            max_connections: cap,
            max_wall: wall,
            grace: Duration::from_millis(30),
            fallback_home: Some("/sandbox-home".into()),
        };
        let (stop, rx) = watch::channel(false);
        let server = tokio::spawn(serve(listener, config, rx));
        Self {
            _tmp: tmp,
            work,
            port,
            stop,
            server,
        }
    }
    async fn connect(&self) -> TcpStream {
        TcpStream::connect(("127.0.0.1", self.port)).await.unwrap()
    }
    fn start(&self, env: &str) -> String {
        format!(
            r#"{{"noevia":"start","cwd":"{}","env":{env}}}"#,
            self.work.display()
        ) + "\n"
    }
    async fn close(self) {
        self.stop.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(3), self.server)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
}
async fn read_all(mut socket: TcpStream) -> String {
    let mut bytes = Vec::new();
    tokio::time::timeout(Duration::from_secs(3), socket.read_to_end(&mut bytes))
        .await
        .unwrap()
        .unwrap();
    String::from_utf8(bytes).unwrap()
}

#[tokio::test]
async fn forwards_start_remainder_and_later_acp_bytes() {
    let h = Harness::new("cat", 2, Duration::from_secs(2)).await;
    let mut socket = h.connect().await;
    socket
        .write_all((h.start("{}") + "first\n").as_bytes())
        .await
        .unwrap();
    let mut first = [0_u8; 6];
    socket.read_exact(&mut first).await.unwrap();
    assert_eq!(&first, b"first\n");
    socket.write_all(b"second\n").await.unwrap();
    let mut second = [0_u8; 7];
    socket.read_exact(&mut second).await.unwrap();
    assert_eq!(&second, b"second\n");
    drop(socket);
    h.close().await;
}

#[tokio::test]
async fn rejects_bad_start_escape_and_oversize() {
    let h = Harness::new("printf AGENT_STARTED", 2, Duration::from_secs(2)).await;
    for payload in [
        "not json\n".to_owned(),
        r#"{"method":"initialize"}"#.to_owned() + "\n",
        r#"{"noevia":"start","cwd":"/etc"}"#.to_owned() + "\n",
        "x".repeat(70 * 1024),
    ] {
        let mut socket = h.connect().await;
        socket.write_all(payload.as_bytes()).await.unwrap();
        let response = read_all(socket).await;
        assert!(response.contains("\"code\":-32000"), "{response}");
        assert!(!response.contains("AGENT_STARTED"));
    }
    h.close().await;
}

#[tokio::test]
async fn filters_environment_and_falls_back_home() {
    let h = Harness::new(
        "printf '%s|%s|%s' \"$HOME\" \"$PATH\" \"$(printenv LD_PRELOAD || printf unset)\"",
        1,
        Duration::from_secs(2),
    )
    .await;
    let mut socket = h.connect().await;
    socket
        .write_all(
            h.start(r#"{"PATH":"/bin","LD_PRELOAD":"/evil","HOME":5}"#)
                .as_bytes(),
        )
        .await
        .unwrap();
    assert_eq!(read_all(socket).await, "/sandbox-home|/bin|unset");
    h.close().await;
}

#[tokio::test]
async fn cap_releases_after_disconnect() {
    let h = Harness::new("cat", 1, Duration::from_secs(2)).await;
    let mut first = h.connect().await;
    first.write_all(h.start("{}").as_bytes()).await.unwrap();
    first.write_all(b"ready\n").await.unwrap();
    let mut echoed = [0_u8; 6];
    first.read_exact(&mut echoed).await.unwrap();
    assert_eq!(&echoed, b"ready\n");
    let response = read_all(h.connect().await).await;
    assert!(response.contains("already running 1 agents"));
    drop(first);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut third = h.connect().await;
    third.write_all(h.start("{}").as_bytes()).await.unwrap();
    third.write_all(b"again\n").await.unwrap();
    third.read_exact(&mut echoed).await.unwrap();
    assert_eq!(&echoed, b"again\n");
    drop(third);
    h.close().await;
}

#[tokio::test]
async fn timeout_stops_descendant_and_returns_error() {
    let marker = tempfile::tempdir().unwrap();
    let path = marker.path().join("survived");
    let script = format!("(sleep 0.5; touch '{}') & wait", path.display());
    let h = Harness::new(&script, 1, Duration::from_millis(40)).await;
    let mut socket = h.connect().await;
    socket.write_all(h.start("{}").as_bytes()).await.unwrap();
    let response = read_all(socket).await;
    assert!(response.contains("time limit"), "{response}");
    tokio::time::sleep(Duration::from_millis(650)).await;
    assert!(!path.exists(), "descendant survived group stop");
    h.close().await;
}

#[tokio::test]
async fn shutdown_stops_active_connection() {
    let h = Harness::new("cat", 1, Duration::from_secs(5)).await;
    let mut socket = h.connect().await;
    socket.write_all(h.start("{}").as_bytes()).await.unwrap();
    socket.write_all(b"ready\n").await.unwrap();
    let mut echoed = [0_u8; 6];
    socket.read_exact(&mut echoed).await.unwrap();
    assert_eq!(&echoed, b"ready\n");
    h.close().await;
    let mut rest = Vec::new();
    tokio::time::timeout(Duration::from_secs(2), socket.read_to_end(&mut rest))
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn leader_exit_terminates_its_background_process_group() {
    let marker = tempfile::tempdir().unwrap();
    let path = marker.path().join("survived");
    let script = format!("(sleep 0.5; touch '{}') & exit 0", path.display());
    let h = Harness::new(&script, 1, Duration::from_secs(2)).await;
    let mut socket = h.connect().await;
    socket.write_all(h.start("{}").as_bytes()).await.unwrap();
    assert_eq!(read_all(socket).await, "");
    tokio::time::sleep(Duration::from_millis(650)).await;
    assert!(!path.exists(), "descendant survived leader exit");
    h.close().await;
}

#[tokio::test]
async fn disconnect_terminates_its_background_process_group() {
    let marker = tempfile::tempdir().unwrap();
    let path = marker.path().join("survived");
    let script = format!("(sleep 0.5; touch '{}') & cat", path.display());
    let h = Harness::new(&script, 1, Duration::from_secs(2)).await;
    let mut socket = h.connect().await;
    socket.write_all(h.start("{}").as_bytes()).await.unwrap();
    socket.write_all(b"ready\n").await.unwrap();
    let mut echoed = [0_u8; 6];
    socket.read_exact(&mut echoed).await.unwrap();
    assert_eq!(&echoed, b"ready\n");
    drop(socket);
    tokio::time::sleep(Duration::from_millis(650)).await;
    assert!(!path.exists(), "descendant survived disconnect");
    h.close().await;
}
